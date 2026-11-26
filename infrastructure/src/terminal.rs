use crate::{
    theme::Scheme,
    wire::{MetadataField, Response, SessionMetadata},
};
use anyhow::{Context, Result};
use application::{SessionOptions, TerminalRuntime, agents::AgentProfileRegistry};
use base64::{Engine, engine::general_purpose::STANDARD};
use domain::{
    agent::AgentProfile,
    session::{Agent, Geometry, Session},
};
use ghostty_vt::{
    Terminal,
    fmt::{Formatter, FormatterOptions},
    snapshot,
    terminal::{Event, Mode, Options},
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::{
    ffi::OsStr,
    io::{Read, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

pub(crate) type ActiveScheme = Arc<RwLock<&'static Scheme>>;
type ModelChanged = Arc<dyn Fn(&str, &Agent, Option<&str>) -> Result<()> + Send + Sync>;

pub struct PtyRuntime {
    pub socket: PathBuf,
    pub scheme: ActiveScheme,
    pub on_exit: Arc<dyn Fn(&str, &str) + Send + Sync>,
    pub on_model_changed: ModelChanged,
    pub profiles: Arc<dyn AgentProfileRegistry>,
}

struct Lifecycle {
    running: AtomicBool,
    session: String,
    on_exit: Arc<dyn Fn(&str, &str) + Send + Sync>,
    foreground: Mutex<Foreground>,
    on_model_changed: ModelChanged,
}

struct Foreground {
    agent: Agent,
    model: Option<String>,
}

impl Lifecycle {
    fn notify_exit(&self, detail: &str) {
        if self.running.swap(false, Ordering::AcqRel) {
            (self.on_exit)(&self.session, detail);
        }
    }

    fn set_model(&self, agent: &Agent, model: Option<String>) -> Result<()> {
        let previous = std::mem::replace(&mut self.foreground.lock().unwrap().model, model.clone());
        // Registration takes the DAG lock before reading the live model. Release the
        // model lock before writing to the DAG so either operation can finish first.
        if let Err(error) = (self.on_model_changed)(&self.session, agent, model.as_deref()) {
            self.foreground.lock().unwrap().model = previous;
            return Err(error);
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct Handle {
    commands: SyncSender<Command>,
    lifecycle: Arc<Lifecycle>,
}

impl Handle {
    pub fn is_running(&self) -> bool {
        self.lifecycle.running.load(Ordering::Acquire)
    }

    pub fn send(&self, command: Command) -> Result<(), mpsc::SendError<Command>> {
        self.commands.send(command)
    }

    pub(crate) fn refresh_theme(&self) {
        // A full queue already guarantees another actor iteration.
        let _ = self.commands.try_send(Command::RefreshTheme);
    }
}

impl TerminalRuntime for PtyRuntime {
    type Handle = Handle;

    fn start(
        &self,
        session: &Session,
        options: SessionOptions,
        profile: Option<&AgentProfile>,
    ) -> Result<Self::Handle> {
        launch(self, session, options, profile)
    }

    fn is_running(&self, handle: &Self::Handle) -> bool {
        handle.is_running()
    }

    fn end(&self, handle: Self::Handle) -> Result<()> {
        Ok(handle.send(Command::End)?)
    }

    fn agent(&self, handle: &Self::Handle) -> Option<Agent> {
        Some(handle.lifecycle.foreground.lock().unwrap().agent.clone())
    }

    fn model(&self, handle: &Self::Handle, agent: &Agent) -> Option<String> {
        let foreground = handle.lifecycle.foreground.lock().unwrap();
        if foreground.agent == *agent {
            foreground.model.clone()
        } else {
            None
        }
    }
}

pub enum Command {
    RefreshTheme,
    Attach(SyncSender<Response>),
    Input(Vec<u8>),
    Resize(Geometry),
    WorkingDirectory(SyncSender<Option<PathBuf>>),
    SetMetadata {
        pid: u32,
        field: MetadataField,
        value: Option<String>,
        reply: SyncSender<Result<bool>>,
    },
    Output(Vec<u8>),
    /// Recheck which agent owns the foreground of a shell session.
    Poll,
    OutputClosed,
    ProcessExited(String),
    End,
}

fn launch(
    runtime: &PtyRuntime,
    session: &Session,
    options: SessionOptions,
    profile: Option<&AgentProfile>,
) -> Result<Handle> {
    let geometry = Geometry::default();
    let pair = native_pty_system().openpty(pty_size(geometry))?;
    let model = requested_model(&session.agent, &options.args);
    let mut process = if let Some(profile) = profile {
        let mut command = CommandBuilder::new(&profile.command);
        let installed = crate::skill_install::LocalInstalledSkills(&session.worktree);
        command.args(crate::shell::instruction_args(
            profile,
            &options.args,
            &installed,
        )?);
        command.args(&options.args);
        command
    } else if let Some((program, args)) = options.args.split_first() {
        let mut command = CommandBuilder::new(program);
        command.args(args);
        command
    } else {
        CommandBuilder::new(std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into()))
    };
    if let Some(prompt) = options.initial_prompt {
        process.arg(prompt);
    }
    for (name, value) in options.env {
        match value {
            Some(value) => process.env(name, value),
            None => process.env_remove(name),
        }
    }
    process.cwd(&session.worktree);
    process.env("TERM", "xterm-256color");
    process.env("COLORTERM", "truecolor");
    process.env("POWER_SESSION", &session.id);
    process.env("POWER_SOCKET", &runtime.socket);
    process.env_remove("TMUX");
    if let Ok(executable) = std::env::current_exe()
        && let Some(dir) = executable.parent()
    {
        process.env("POWER_BIN", &executable);
        let inherited = std::env::var_os("PATH").unwrap_or_default();
        if let Ok(path) = std::env::join_paths(
            std::iter::once(dir.to_owned()).chain(std::env::split_paths(&inherited)),
        ) {
            process.env("PATH", path);
        }
    }

    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;
    let mut process = pair.slave.spawn_command(process)?;
    let pid = process.process_id();
    let lifecycle = Arc::new(Lifecycle {
        running: AtomicBool::new(true),
        session: session.id.clone(),
        on_exit: runtime.on_exit.clone(),
        foreground: Mutex::new(Foreground {
            agent: session.agent.clone(),
            model,
        }),
        on_model_changed: runtime.on_model_changed.clone(),
    });
    drop(pair.slave);

    let (tx, rx) = mpsc::sync_channel(128);
    let (write_tx, write_rx) = mpsc::sync_channel::<Vec<u8>>(128);
    thread::spawn(move || {
        for bytes in write_rx {
            if writer.write_all(&bytes).is_err() {
                break;
            }
        }
    });

    let reader_tx = tx.clone();
    thread::spawn(move || {
        let mut buffer = [0; 16 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count)
                    if reader_tx
                        .send(Command::Output(buffer[..count].to_vec()))
                        .is_err() =>
                {
                    break;
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let _ = reader_tx.send(Command::OutputClosed);
    });
    let waiter_tx = tx.clone();
    let (stop_tx, stop_rx) = mpsc::channel();
    thread::spawn(move || {
        let result = loop {
            match process.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Err(error) => break Err(error),
                Ok(None) => {}
            }
            match stop_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = process.kill();
                    break process.wait();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        };
        let detail = match result {
            Ok(status) => format!("Worker process exited ({status})"),
            Err(error) => format!("Could not wait for worker process ({error})"),
        };
        let _ = waiter_tx.send(Command::ProcessExited(detail));
    });
    if session.agent == Agent::Shell {
        let poll_tx = tx.clone();
        let lifecycle = lifecycle.clone();
        thread::spawn(move || {
            while lifecycle.running.load(Ordering::Acquire) && poll_tx.send(Command::Poll).is_ok() {
                thread::sleep(Duration::from_millis(500));
            }
        });
    }
    let launched_agent = session.agent.clone();
    let actor_lifecycle = lifecycle.clone();
    let active_scheme = runtime.scheme.clone();
    let profiles = runtime.profiles.clone();
    thread::spawn(move || {
        let result = terminal_actor(
            rx,
            write_tx,
            pair.master,
            geometry,
            pid,
            launched_agent,
            &actor_lifecycle,
            active_scheme,
            profiles.as_ref(),
        );
        if actor_lifecycle.running.load(Ordering::Acquire) {
            let _ = stop_tx.send(());
            let detail = match result {
                Ok(detail) => detail.unwrap_or_else(|| "Terminal session ended".into()),
                Err(error) => {
                    eprintln!("Terminal session failed. {error:#}");
                    format!("Terminal session failed ({error:#})")
                }
            };
            actor_lifecycle.notify_exit(&detail);
        }
    });
    Ok(Handle {
        commands: tx,
        lifecycle,
    })
}

fn pty_size(geometry: Geometry) -> PtySize {
    PtySize {
        rows: geometry.rows,
        cols: geometry.cols,
        pixel_width: geometry.cols.saturating_mul(geometry.cell_width),
        pixel_height: geometry.rows.saturating_mul(geometry.cell_height),
    }
}

fn terminal_actor(
    rx: Receiver<Command>,
    writer: SyncSender<Vec<u8>>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    mut geometry: Geometry,
    shell_pid: Option<u32>,
    launched_agent: Agent,
    lifecycle: &Lifecycle,
    active_scheme: ActiveScheme,
    profiles: &dyn AgentProfileRegistry,
) -> Result<Option<String>> {
    let mut terminal = Terminal::new(Options {
        cols: geometry.cols,
        rows: geometry.rows,
        ..Options::default()
    })?;
    let mut scheme = *active_scheme.read().unwrap();
    scheme.apply_to(&mut terminal)?;
    terminal.set_xtversion(Some("Power Overwhelming".into()));
    terminal.resize(
        geometry.cols,
        geometry.rows,
        geometry.cell_width.into(),
        geometry.cell_height.into(),
    )?;
    let mut clients: Vec<SyncSender<Response>> = Vec::new();
    let mut sequence = 0;
    let mut exited = false;
    let mut run = if launched_agent == Agent::Shell {
        None
    } else {
        shell_pid.map(|pid| (pid, launched_agent.clone()))
    };
    let mut metadata = SessionMetadata {
        model: lifecycle.foreground.lock().unwrap().model.clone(),
        ..SessionMetadata::default()
    };

    let mut output_closed = false;
    let mut process_exit: Option<(String, Instant)> = None;
    loop {
        // Give the reader time to deliver the end of the transcript. A descendant may keep
        // the slave open after the launched process exits, so EOF is not required forever.
        if process_exit
            .as_ref()
            .is_some_and(|(_, deadline)| output_closed || Instant::now() >= *deadline)
        {
            let (detail, _) = process_exit.take().unwrap();
            lifecycle.notify_exit(&with_terminal_tail(&terminal, detail));
            exited = true;
            lifecycle.foreground.lock().unwrap().model = None;
            clear_metadata(&mut metadata, &mut clients);
            clients.retain(|client| client.try_send(Response::Exit).is_ok());
        }
        let command = match &process_exit {
            Some((_, deadline)) => {
                match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(command) => command,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            None => match rx.recv() {
                Ok(command) => command,
                Err(_) => break,
            },
        };
        let next = *active_scheme.read().unwrap();
        if !std::ptr::eq(next, scheme) {
            next.apply_to(&mut terminal)?;
            if !exited
                && next.terminal_mode() != scheme.terminal_mode()
                && terminal.mode(Mode::COLOR_SCHEME_REPORT)?
            {
                let mut report = [0; 32];
                let len = next.terminal_mode().encode_report(&mut report)?;
                let _ = writer.try_send(report[..len].to_vec());
            }
            scheme = next;
        }
        if !exited
            && launched_agent == Agent::Shell
            && matches!(
                command,
                Command::Attach(_) | Command::Poll | Command::SetMetadata { .. }
            )
        {
            let current = foreground_agent(
                master
                    .process_group_leader()
                    .and_then(|pid| u32::try_from(pid).ok()),
                shell_pid,
                profiles,
            );
            if current != run {
                run = current;
                let current = run
                    .as_ref()
                    .map_or(Agent::Shell, |(_, agent)| agent.clone());
                let initial_model = run
                    .as_ref()
                    .and_then(|(pid, agent)| process_model(*pid, agent));
                let previous = std::mem::replace(
                    &mut *lifecycle.foreground.lock().unwrap(),
                    Foreground {
                        agent: current.clone(),
                        model: initial_model.clone(),
                    },
                );
                if previous.agent != current {
                    clients.retain(|client| {
                        client
                            .try_send(Response::Agent {
                                agent: current.clone().into(),
                            })
                            .is_ok()
                    });
                }
                let next = SessionMetadata {
                    model: initial_model,
                    task: None,
                };
                if next != metadata {
                    metadata = next;
                    notify_metadata(&metadata, &mut clients);
                }
            }
        }
        match command {
            Command::Attach(client) => {
                let data = STANDARD.encode(snapshot::encode(&terminal)?);
                if client
                    .try_send(Response::Snapshot {
                        sequence,
                        data,
                        exited,
                    })
                    .is_ok()
                    && client
                        .try_send(Response::Agent {
                            agent: lifecycle.foreground.lock().unwrap().agent.clone().into(),
                        })
                        .is_ok()
                    && (metadata == SessionMetadata::default()
                        || client
                            .try_send(Response::Metadata {
                                metadata: metadata.clone(),
                            })
                            .is_ok())
                {
                    clients.push(client);
                }
            }
            Command::Input(bytes) => {
                if !exited {
                    let _ = writer.try_send(bytes);
                }
            }
            Command::Resize(next) if next != geometry && !exited => {
                if let Err(error) = master.resize(pty_size(next)) {
                    eprintln!("Could not resize terminal. {error}");
                    continue;
                }
                terminal.resize(
                    next.cols,
                    next.rows,
                    next.cell_width.into(),
                    next.cell_height.into(),
                )?;
                geometry = next;
                sequence += 1;
                clients.retain(|client| {
                    client
                        .try_send(Response::Geometry {
                            sequence,
                            geometry: geometry.into(),
                        })
                        .is_ok()
                });
            }
            Command::Resize(_) => {}
            Command::WorkingDirectory(reply) => {
                let directory = if exited {
                    None
                } else {
                    master
                        .process_group_leader()
                        .and_then(|pid| u32::try_from(pid).ok())
                        .and_then(process_directory)
                        .or_else(|| shell_pid.and_then(process_directory))
                };
                let _ = reply.send(directory);
            }
            Command::SetMetadata {
                pid,
                field,
                value,
                reply,
            } => {
                let result = (|| {
                    let Some((_, agent)) = run
                        .as_ref()
                        .filter(|(owner, _)| !exited && descends_from(pid, *owner))
                    else {
                        return Ok(false);
                    };
                    let field = match field {
                        MetadataField::Task => &mut metadata.task,
                        MetadataField::Model => {
                            lifecycle
                                .set_model(agent, value.clone())
                                .context("Could not save agent model")?;
                            &mut metadata.model
                        }
                    };
                    if *field != value {
                        *field = value;
                        notify_metadata(&metadata, &mut clients);
                    }
                    Ok(true)
                })();
                let _ = reply.send(result);
            }
            Command::Output(bytes) => {
                terminal.vt_write(&bytes);
                sequence += 1;
                if !clients.is_empty() {
                    let data = STANDARD.encode(&bytes);
                    clients.retain(|client| {
                        client
                            .try_send(Response::Output {
                                sequence,
                                data: data.clone(),
                            })
                            .is_ok()
                    });
                }
            }
            Command::Poll | Command::RefreshTheme => {}
            Command::OutputClosed => output_closed = true,
            Command::ProcessExited(detail) => {
                process_exit = Some((detail, Instant::now() + Duration::from_millis(150)));
            }
            Command::End => {
                if !exited {
                    return Ok(Some(with_terminal_tail(
                        &terminal,
                        "Terminal session ended".into(),
                    )));
                }
                break;
            }
        }
        for event in terminal.events() {
            if let Event::PtyWrite(bytes) = event {
                let _ = writer.try_send(bytes);
            }
        }
    }
    Ok(None)
}

fn with_terminal_tail(terminal: &Terminal, mut detail: String) -> String {
    if let Ok(mut formatter) = Formatter::new(
        terminal,
        FormatterOptions::new().with_trim(true).with_unwrap(true),
    ) && let Ok(bytes) = formatter.format_alloc()
    {
        let screen = String::from_utf8_lossy(&bytes);
        let lines: Vec<_> = screen
            .lines()
            .rev()
            .filter(|line| !line.trim().is_empty())
            .take(20)
            .collect();
        if !lines.is_empty() {
            detail.push_str("\n\nLast terminal lines:");
            for line in lines.into_iter().rev() {
                detail.push('\n');
                detail.push_str(line);
            }
        }
    }
    detail
}

fn notify_metadata(metadata: &SessionMetadata, clients: &mut Vec<SyncSender<Response>>) {
    clients.retain(|client| {
        client
            .try_send(Response::Metadata {
                metadata: metadata.clone(),
            })
            .is_ok()
    });
}

fn clear_metadata(metadata: &mut SessionMetadata, clients: &mut Vec<SyncSender<Response>>) {
    if std::mem::take(metadata) != SessionMetadata::default() {
        notify_metadata(metadata, clients);
    }
}

fn process_directory(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// Walks from the foreground process group leader up to the shell, looking for an agent.
fn foreground_agent(
    leader: Option<u32>,
    shell: Option<u32>,
    profiles: &dyn AgentProfileRegistry,
) -> Option<(u32, Agent)> {
    let mut pid = leader;
    for _ in 0..32 {
        let Some(current) = pid.filter(|current| Some(*current) != shell && *current > 1) else {
            break;
        };
        if let Some(agent) = process_agent(current, profiles) {
            return Some((current, agent));
        }
        pid = parent_pid(current);
    }
    None
}

fn descends_from(mut pid: u32, owner: u32) -> bool {
    for _ in 0..64 {
        if pid == owner {
            return true;
        }
        let Some(parent) = parent_pid(pid).filter(|parent| *parent > 1 && *parent != pid) else {
            break;
        };
        pid = parent;
    }
    false
}

fn process_agent(pid: u32, profiles: &dyn AgentProfileRegistry) -> Option<Agent> {
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let cmdline = String::from_utf8_lossy(&cmdline);
    let mut args = cmdline.split('\0').filter(|arg| !arg.is_empty());
    let program = args.next()?;
    // Interpreters such as `node /usr/bin/codex` carry the agent in the next argument.
    let program = if matches!(
        program_name(program),
        "node" | "bun" | "deno" | "sh" | "bash" | "dash" | "zsh" | "python" | "python3"
    ) {
        args.find(|arg| !arg.starts_with('-'))?
    } else {
        program
    };
    agent_for_program(program, profiles)
}

fn process_model(pid: u32, agent: &Agent) -> Option<String> {
    use std::os::unix::ffi::OsStrExt;

    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    requested_model(
        agent,
        cmdline
            .split(|&byte| byte == 0)
            .skip(1)
            .map(OsStr::from_bytes),
    )
}

fn requested_model(
    agent: &Agent,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
) -> Option<String> {
    let short_flag = match agent.as_str() {
        "codex" | "grok" => true,
        "claude" => false,
        _ => return None,
    };
    let mut model = None;
    let mut expecting_value = false;
    for arg in args {
        let arg = arg.as_ref();
        if arg == "--" {
            break;
        }
        if std::mem::take(&mut expecting_value) {
            model = arg
                .to_str()
                .filter(|value| !value.starts_with('-'))
                .and_then(model_name);
        }
        if arg == "--model" || (short_flag && arg == "-m") {
            expecting_value = true;
        } else if let Some((flag, value)) = arg.to_str().and_then(|arg| arg.split_once('='))
            && (flag == "--model" || (short_flag && flag == "-m"))
        {
            model = model_name(value);
        }
    }
    if expecting_value { None } else { model }
}

fn model_name(value: &str) -> Option<String> {
    let value = value.trim();
    (valid_metadata(value) && !value.is_empty()).then(|| value.to_owned())
}

pub(crate) fn valid_metadata(value: &str) -> bool {
    value.chars().count() <= 120 && !value.chars().any(char::is_control)
}

fn program_name(arg: &str) -> &str {
    let name = arg.rsplit('/').next().unwrap_or(arg);
    [".js", ".mjs", ".cjs"]
        .iter()
        .find_map(|suffix| name.strip_suffix(suffix))
        .unwrap_or(name)
}

fn agent_for_program(program: &str, profiles: &dyn AgentProfileRegistry) -> Option<Agent> {
    let name = program_name(program);
    let profiles = profiles.profiles();
    profiles
        .iter()
        .find(|profile| profile.command == program)
        .or_else(|| {
            profiles
                .iter()
                .find(|profile| program_name(&profile.command) == name)
        })
        .map(|profile| Agent::Coding(profile.id.clone()))
}

fn parent_pid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name can contain spaces and parentheses, so parse after the last one.
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_agent_programs() {
        let profiles = crate::agents::AgentProfiles::default();
        assert_eq!(
            agent_for_program("/usr/bin/claude", &profiles),
            Some(Agent::Coding("claude".into()))
        );
        assert_eq!(
            agent_for_program("/opt/codex/bin/codex.js", &profiles),
            Some(Agent::Coding("codex".into()))
        );
        assert_eq!(agent_for_program("vim", &profiles), None);
    }

    #[test]
    fn recognizes_a_configured_agent_by_its_executable() {
        let mut profiles = crate::agents::AgentProfiles::default();
        profiles.0.push(AgentProfile {
            id: "custom".into(),
            command: "/opt/agent/bin/assistant.mjs".into(),
            injection: Default::default(),
            skip_subcommands: Vec::new(),
            skip_if_args_contain: Vec::new(),
            skills_dir: None,
        });
        assert_eq!(
            agent_for_program("/opt/agent/bin/assistant.mjs", &profiles),
            Some(Agent::Coding("custom".into()))
        );
    }

    #[test]
    fn reads_parent_of_current_process() {
        let parent = parent_pid(std::process::id()).unwrap();
        assert_eq!(parent, std::os::unix::process::parent_id());
    }
}
