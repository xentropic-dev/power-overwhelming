use crate::{
    theme::Scheme,
    wire::{MetadataField, Response, SessionMetadata},
};
use anyhow::Result;
use application::{SessionOptions, TerminalRuntime};
use base64::{Engine, engine::general_purpose::STANDARD};
use domain::session::{Agent, Geometry, Session};
use ghostty_vt::{
    Terminal,
    fmt::{Formatter, FormatterOptions},
    snapshot,
    terminal::{Event, Mode, Options},
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

pub(crate) type ActiveScheme = Arc<RwLock<&'static Scheme>>;

pub struct PtyRuntime {
    pub socket: PathBuf,
    pub scheme: ActiveScheme,
    pub on_exit: Arc<dyn Fn(&str, &str) + Send + Sync>,
}

struct Lifecycle {
    running: AtomicBool,
    session: String,
    on_exit: Arc<dyn Fn(&str, &str) + Send + Sync>,
}

impl Lifecycle {
    fn notify_exit(&self, detail: &str) {
        if self.running.swap(false, Ordering::AcqRel) {
            (self.on_exit)(&self.session, detail);
        }
    }
}

#[derive(Clone)]
pub struct Handle {
    commands: SyncSender<Command>,
    agent: Arc<Mutex<Agent>>,
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

    fn start(&self, session: &Session, options: SessionOptions) -> Result<Self::Handle> {
        launch(
            session,
            &self.socket,
            options,
            self.on_exit.clone(),
            self.scheme.clone(),
        )
    }

    fn is_running(&self, handle: &Self::Handle) -> bool {
        handle.is_running()
    }

    fn end(&self, handle: Self::Handle) -> Result<()> {
        Ok(handle.send(Command::End)?)
    }

    fn agent(&self, handle: &Self::Handle) -> Option<Agent> {
        Some(*handle.agent.lock().unwrap())
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
        reply: SyncSender<bool>,
    },
    Output(Vec<u8>),
    /// Recheck which agent owns the foreground of a shell session.
    Poll,
    OutputClosed,
    ProcessExited(String),
    End,
}

fn launch(
    session: &Session,
    socket: &Path,
    options: SessionOptions,
    on_exit: Arc<dyn Fn(&str, &str) + Send + Sync>,
    active_scheme: ActiveScheme,
) -> Result<Handle> {
    let geometry = Geometry::default();
    let pair = native_pty_system().openpty(pty_size(geometry))?;
    let mut process = if let Some((program, args)) = options.argv.split_first() {
        let mut command = CommandBuilder::new(program);
        command.args(crate::shell::instruction_args(session.agent, args)?);
        command.args(args);
        command
    } else {
        match session.agent {
            Agent::Shell => {
                CommandBuilder::new(std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into()))
            }
            agent => {
                let mut command = CommandBuilder::new(agent.command());
                command.args(crate::shell::instruction_args(agent, &[])?);
                command
            }
        }
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
    process.env("POWER_SOCKET", socket);
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
        on_exit,
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
    let agent = Arc::new(Mutex::new(session.agent));
    if session.agent == Agent::Shell {
        let poll_tx = tx.clone();
        let lifecycle = lifecycle.clone();
        thread::spawn(move || {
            while lifecycle.running.load(Ordering::Acquire) && poll_tx.send(Command::Poll).is_ok() {
                thread::sleep(Duration::from_millis(500));
            }
        });
    }
    let actor_agent = agent.clone();
    let launched_agent = session.agent;
    let actor_lifecycle = lifecycle.clone();
    thread::spawn(move || {
        let result = terminal_actor(
            rx,
            write_tx,
            pair.master,
            geometry,
            pid,
            launched_agent,
            actor_agent,
            &actor_lifecycle,
            active_scheme,
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
        agent,
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
    agent: Arc<Mutex<Agent>>,
    lifecycle: &Lifecycle,
    active_scheme: ActiveScheme,
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
        shell_pid.map(|pid| (pid, launched_agent))
    };
    let mut metadata = SessionMetadata::default();

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
            );
            if current != run {
                run = current;
                let current = run.map_or(Agent::Shell, |(_, agent)| agent);
                let mut stored = agent.lock().unwrap();
                if *stored != current {
                    *stored = current;
                    clients.retain(|client| {
                        client
                            .try_send(Response::Agent {
                                agent: current.into(),
                            })
                            .is_ok()
                    });
                }
                clear_metadata(&mut metadata, &mut clients);
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
                            agent: (*agent.lock().unwrap()).into(),
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
                let accepted = !exited && run.is_some_and(|(owner, _)| descends_from(pid, owner));
                if accepted {
                    let field = match field {
                        MetadataField::Task => &mut metadata.task,
                        MetadataField::Model => &mut metadata.model,
                    };
                    if *field != value {
                        *field = value;
                        notify_metadata(&metadata, &mut clients);
                    }
                }
                let _ = reply.send(accepted);
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
fn foreground_agent(leader: Option<u32>, shell: Option<u32>) -> Option<(u32, Agent)> {
    let mut pid = leader;
    for _ in 0..32 {
        let Some(current) = pid.filter(|current| Some(*current) != shell && *current > 1) else {
            break;
        };
        if let Some(agent) = process_agent(current) {
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

fn process_agent(pid: u32) -> Option<Agent> {
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let mut args = cmdline
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).into_owned());
    let program = program_name(&args.next()?);
    // Interpreters such as `node /usr/bin/codex` carry the agent in the next argument.
    let program = if matches!(
        program.as_str(),
        "node" | "bun" | "deno" | "sh" | "bash" | "dash" | "zsh" | "python" | "python3"
    ) {
        program_name(&args.find(|arg| !arg.starts_with('-'))?)
    } else {
        program
    };
    agent_named(&program)
}

fn program_name(arg: &str) -> String {
    let name = arg.rsplit('/').next().unwrap_or(arg);
    let name = [".js", ".mjs", ".cjs"]
        .iter()
        .find_map(|suffix| name.strip_suffix(suffix))
        .unwrap_or(name);
    name.to_owned()
}

fn agent_named(name: &str) -> Option<Agent> {
    match name {
        "claude" => Some(Agent::Claude),
        "codex" => Some(Agent::Codex),
        _ => None,
    }
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
        assert_eq!(
            agent_named(&program_name("/usr/bin/claude")),
            Some(Agent::Claude)
        );
        assert_eq!(
            agent_named(&program_name("/opt/codex/bin/codex.js")),
            Some(Agent::Codex)
        );
        assert_eq!(agent_named(&program_name("vim")), None);
    }

    #[test]
    fn reads_parent_of_current_process() {
        let parent = parent_pid(std::process::id()).unwrap();
        assert_eq!(parent, std::os::unix::process::parent_id());
    }
}
