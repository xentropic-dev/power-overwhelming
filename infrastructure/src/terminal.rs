use crate::{
    persistence::state_directory,
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
    fmt::{Format, Formatter, FormatterOptions},
    screen::Screen,
    snapshot,
    terminal::{Event, Mode, Options},
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::{
    ffi::{OsStr, OsString},
    fs::{self, File, Permissions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, PermissionsExt},
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
type ExitHandler = Arc<dyn Fn(&str, &str) + Send + Sync>;
type ModelChanged = Arc<dyn Fn(&str, &Agent, Option<&str>) -> Result<()> + Send + Sync>;
/// Saves a session's foreground agent, its conversation and the shell's directory.
type ForegroundChanged =
    Arc<dyn Fn(&str, &Agent, Option<&str>, Option<&Path>) -> Result<()> + Send + Sync>;

/// How often a shell session's working directory is checked for a restore.
const CWD_INTERVAL: Duration = Duration::from_secs(3);
/// How long a shell stays in the foreground before its agent's conversation is
/// forgotten. A reboot kills the agent before the daemon, and this keeps the
/// conversation until the shutdown flush.
const CLEAR_DELAY: Duration = Duration::from_secs(3);
/// An agent restored with resume arguments that exits this soon after launch had
/// nothing to resume, so it is started once more with its fallback arguments.
const RESUME_WINDOW: Duration = Duration::from_secs(5);
/// Written to the terminal when a failed resume starts a new conversation.
const RESUME_NOTICE: &str =
    "\r\n\x1b[2m── could not resume, started a new conversation ──\x1b[0m\r\n";
/// How long the end of a shell's output may trail its exit.
const OUTPUT_DRAIN: Duration = Duration::from_millis(150);
/// How long an agent session's exit waits before it is reported. A reboot signals
/// the agents and the daemon at once, and the wait lets shutdown begin first, so a
/// session that will be restored is not reported as ended.
const AGENT_EXIT_GRACE: Duration = Duration::from_secs(1);

pub struct PtyRuntime {
    pub socket: PathBuf,
    pub scheme: ActiveScheme,
    pub on_exit: ExitHandler,
    pub on_model_changed: ModelChanged,
    pub on_foreground_changed: ForegroundChanged,
    /// Set on daemon shutdown. Exits are then not reported and foreground state
    /// is only saved by an explicit flush, so a reboot cannot erase what a restore needs.
    pub shutting_down: Arc<AtomicBool>,
    pub profiles: Arc<dyn AgentProfileRegistry>,
}

struct Lifecycle {
    running: AtomicBool,
    session: String,
    on_exit: ExitHandler,
    foreground: Mutex<Foreground>,
    on_model_changed: ModelChanged,
    on_foreground_changed: ForegroundChanged,
    shutting_down: Arc<AtomicBool>,
}

struct Foreground {
    agent: Agent,
    model: Option<String>,
}

impl Lifecycle {
    fn notify_exit(&self, detail: &str) {
        if self.running.swap(false, Ordering::AcqRel) && !self.shutting_down.load(Ordering::Acquire)
        {
            (self.on_exit)(&self.session, detail);
        }
    }

    /// Saves foreground state. Only a shutdown flush saves once shutdown has begun.
    fn record_foreground(
        &self,
        agent: &Agent,
        conversation: Option<&str>,
        cwd: Option<&Path>,
        flush: bool,
    ) {
        if !flush && self.shutting_down.load(Ordering::Acquire) {
            return;
        }
        if let Err(error) = (self.on_foreground_changed)(&self.session, agent, conversation, cwd) {
            eprintln!(
                "Could not save foreground state for {}. {error:#}",
                self.session
            );
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
    /// An agent wrapper in this shell session reports the conversation it starts.
    SetConversation {
        pid: u32,
        agent: Agent,
        conversation: String,
        reply: SyncSender<bool>,
    },
    Output(Vec<u8>),
    /// A failed resume was replaced by a fresh agent process with this PID.
    Relaunched(Option<u32>),
    /// Recheck which agent owns the foreground of a shell session.
    Poll,
    OutputClosed,
    ProcessExited(String),
    /// Save pending scrollback and foreground state, then acknowledge after the attempt.
    FlushScrollback(SyncSender<()>),
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
    let cwd = options
        .cwd
        .clone()
        .unwrap_or_else(|| session.worktree.clone());
    let mut conversation = None;
    let mut process = if let Some(profile) = profile {
        let (command, id) = agent_command(profile, &session.worktree, &options.args)?;
        conversation = id;
        command
    } else if let Some((program, args)) = options.args.split_first() {
        let mut command = CommandBuilder::new(program);
        command.args(args);
        command
    } else {
        CommandBuilder::new(std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into()))
    };
    if let Some(prompt) = &options.initial_prompt {
        process.arg(prompt);
    }
    configure(&mut process, runtime, session, &options, &cwd);
    // The fresh start never repeats the initial prompt.
    let fallback = match (profile, &options.fallback_args) {
        (Some(profile), Some(args)) => {
            let (mut command, conversation) = agent_command(profile, &session.worktree, args)?;
            configure(&mut command, runtime, session, &options, &cwd);
            Some((command, conversation))
        }
        _ => None,
    };

    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;
    let process = pair.slave.spawn_command(process)?;
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
        on_foreground_changed: runtime.on_foreground_changed.clone(),
        shutting_down: runtime.shutting_down.clone(),
    });
    // A relaunch needs the slave, which also keeps the reader from seeing EOF between
    // the two processes.
    let relaunch = match fallback {
        Some((command, conversation)) => Some(Relaunch {
            command,
            conversation,
            slave: pair.slave,
            deadline: Instant::now() + RESUME_WINDOW,
        }),
        None => {
            drop(pair.slave);
            None
        }
    };
    if conversation.is_some() {
        lifecycle.record_foreground(&session.agent, conversation.as_deref(), None, false);
    }

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
    let waiter_lifecycle = lifecycle.clone();
    let launched = session.agent.clone();
    thread::spawn(move || {
        let result = wait_process(
            process,
            relaunch,
            &stop_rx,
            &waiter_lifecycle.shutting_down,
            |pid, conversation| {
                eprintln!(
                    "Could not resume {}, so it starts a new conversation.",
                    waiter_lifecycle.session
                );
                let _ = waiter_tx.send(Command::Output(RESUME_NOTICE.as_bytes().to_vec()));
                let _ = waiter_tx.send(Command::Relaunched(pid));
                if conversation.is_some() {
                    waiter_lifecycle.record_foreground(&launched, conversation, None, false);
                }
            },
        );
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
            ActorEnv {
                lifecycle: &actor_lifecycle,
                active_scheme,
                profiles: profiles.as_ref(),
                cwd,
            },
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

/// The agent binary with Power's instructions, any pinned conversation ID, and `args`.
fn agent_command(
    profile: &AgentProfile,
    worktree: &Path,
    args: &[OsString],
) -> Result<(CommandBuilder, Option<String>)> {
    let mut command = CommandBuilder::new(&profile.command);
    let installed = crate::skill_install::LocalInstalledSkills(worktree);
    command.args(crate::shell::instruction_args(profile, args, &installed)?);
    let (assign, conversation) = crate::shell::assign_conversation(profile, args);
    command.args(assign);
    command.args(args);
    Ok((command, conversation))
}

/// Sets the environment and directory every process in a Power terminal starts with.
fn configure(
    process: &mut CommandBuilder,
    runtime: &PtyRuntime,
    session: &Session,
    options: &SessionOptions,
    cwd: &Path,
) {
    for (name, value) in &options.env {
        match value {
            Some(value) => process.env(name, value),
            None => process.env_remove(name),
        }
    }
    if let Some(resume) = &options.resume {
        process.env("POWER_RESUME", crate::shell::resume_command(resume));
    }
    process.cwd(cwd);
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
}

type Child = Box<dyn portable_pty::Child + Send + Sync>;

/// A fresh start for an agent launched with resume arguments.
struct Relaunch {
    command: CommandBuilder,
    /// Conversation the fresh start pins, if its profile can assign one.
    conversation: Option<String>,
    slave: Box<dyn portable_pty::SlavePty + Send>,
    /// An exit after this is a normal exit, not a failed resume.
    deadline: Instant,
}

/// Waits for the session's process, killing it when `stop` fires. If it exits before
/// the relaunch deadline outside shutdown, its fresh start replaces it once, and
/// `relaunched` gets the new PID and conversation.
fn wait_process(
    mut process: Child,
    mut relaunch: Option<Relaunch>,
    stop: &Receiver<()>,
    shutting_down: &AtomicBool,
    relaunched: impl FnOnce(Option<u32>, Option<&str>),
) -> io::Result<portable_pty::ExitStatus> {
    let mut relaunched = Some(relaunched);
    loop {
        match process.try_wait() {
            Ok(Some(status)) => {
                let Some(next) = relaunch
                    .take()
                    .filter(|next| Instant::now() < next.deadline)
                    .filter(|_| !shutting_down.load(Ordering::Acquire))
                else {
                    return Ok(status);
                };
                match next.slave.spawn_command(next.command) {
                    Ok(child) => {
                        process = child;
                        if let Some(relaunched) = relaunched.take() {
                            relaunched(process.process_id(), next.conversation.as_deref());
                        }
                        continue;
                    }
                    Err(error) => {
                        eprintln!("Could not start a new conversation. {error:#}");
                        return Ok(status);
                    }
                }
            }
            Err(error) => return Err(error),
            Ok(None) => {}
        }
        // Past the deadline, release the slave so the reader sees EOF at exit.
        if relaunch
            .as_ref()
            .is_some_and(|next| Instant::now() >= next.deadline)
        {
            relaunch = None;
        }
        match stop.recv_timeout(Duration::from_millis(100)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = process.kill();
                return process.wait();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

fn pty_size(geometry: Geometry) -> PtySize {
    PtySize {
        rows: geometry.rows,
        cols: geometry.cols,
        pixel_width: geometry.cols.saturating_mul(geometry.cell_width),
        pixel_height: geometry.rows.saturating_mul(geometry.cell_height),
    }
}

struct ActorEnv<'a> {
    lifecycle: &'a Lifecycle,
    active_scheme: ActiveScheme,
    profiles: &'a dyn AgentProfileRegistry,
    /// Directory the session started in.
    cwd: PathBuf,
}

fn terminal_actor(
    rx: Receiver<Command>,
    writer: SyncSender<Vec<u8>>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    mut geometry: Geometry,
    mut shell_pid: Option<u32>,
    launched_agent: Agent,
    env: ActorEnv<'_>,
) -> Result<Option<String>> {
    let ActorEnv {
        lifecycle,
        active_scheme,
        profiles,
        cwd,
    } = env;
    let mut terminal = Terminal::new(Options {
        cols: geometry.cols,
        rows: geometry.rows,
        ..Options::default()
    })?;
    // A snapshot taken while saving an alternate screen may cross a PTY read boundary.
    terminal.set_continuation_max_bytes(CONTINUATION_LIMIT)?;
    let mut scheme = *active_scheme.read().unwrap();
    scheme.apply_to(&mut terminal)?;
    terminal.set_xtversion(Some("Power Overwhelming".into()));
    terminal.resize(
        geometry.cols,
        geometry.rows,
        geometry.cell_width.into(),
        geometry.cell_height.into(),
    )?;
    let mut scrollback =
        match state_directory().and_then(|dir| Scrollback::new(&dir, &lifecycle.session)) {
            Ok(mut scrollback) => {
                if let Err(error) = scrollback.restore(&mut terminal) {
                    eprintln!(
                        "Could not restore scrollback for {}. {error:#}",
                        lifecycle.session
                    );
                }
                Some(scrollback)
            }
            Err(error) => {
                eprintln!(
                    "Could not open scrollback for {}. {error:#}",
                    lifecycle.session
                );
                None
            }
        };
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

    let mut restore = RestoreState::new(launched_agent.clone(), cwd);

    let mut output_closed = false;
    let mut process_exit: Option<PendingExit> = None;
    loop {
        // Give the reader time to deliver the end of the transcript. A descendant may keep
        // the slave open after the launched process exits, so EOF is not required forever.
        if process_exit
            .as_ref()
            .is_some_and(|exit| Instant::now() >= exit.due(output_closed))
        {
            let detail = process_exit.take().unwrap().detail;
            lifecycle.notify_exit(&with_terminal_tail(&terminal, detail));
            exited = true;
            lifecycle.foreground.lock().unwrap().model = None;
            clear_metadata(&mut metadata, &mut clients);
            clients.retain(|client| client.try_send(Response::Exit).is_ok());
        }
        let now = Instant::now();
        let save_at = scrollback.as_mut().and_then(|scrollback| {
            if scrollback
                .deadline()
                .is_some_and(|deadline| now >= deadline)
            {
                scrollback.flush(&terminal);
            }
            scrollback.deadline()
        });
        let deadline = save_at
            .into_iter()
            .chain(process_exit.as_ref().map(|exit| exit.due(output_closed)))
            .min();
        let received = match deadline {
            Some(deadline) => rx.recv_timeout(deadline.saturating_duration_since(Instant::now())),
            None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
        };
        let command = match received {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
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
                Command::Attach(_)
                    | Command::Poll
                    | Command::SetMetadata { .. }
                    | Command::SetConversation { .. }
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
                restore.foreground_changed(run.as_ref().map(|(_, agent)| agent), lifecycle);
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
                            if metadata.model != value {
                                lifecycle
                                    .set_model(agent, value.clone())
                                    .context("Could not save agent model")?;
                            }
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
                if let Some(scrollback) = &mut scrollback {
                    scrollback.dirty = true;
                }
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
            Command::Poll => {
                if !exited {
                    restore.poll(run.is_none(), shell_pid, lifecycle);
                }
            }
            Command::SetConversation {
                pid,
                agent,
                conversation,
                reply,
            } => {
                // Only a shell session's own descendants may name its conversation.
                let accepted = !exited
                    && launched_agent == Agent::Shell
                    && shell_pid.is_some_and(|shell| pid != shell && descends_from(pid, shell));
                if accepted {
                    let foreground = run.as_ref().map(|(_, agent)| agent);
                    restore.set_conversation(agent, conversation, foreground, lifecycle);
                }
                let _ = reply.send(accepted);
            }
            Command::RefreshTheme => {}
            Command::OutputClosed => output_closed = true,
            Command::ProcessExited(detail) => {
                process_exit = Some(PendingExit::new(
                    detail,
                    launched_agent == Agent::Shell,
                    Instant::now(),
                ));
            }
            Command::Relaunched(pid) => {
                shell_pid = pid;
                if launched_agent != Agent::Shell {
                    run = pid.map(|pid| (pid, launched_agent.clone()));
                }
            }
            Command::FlushScrollback(reply) => {
                if let Some(scrollback) = &mut scrollback {
                    scrollback.flush(&terminal);
                }
                if !exited {
                    restore.flush(shell_pid, lifecycle);
                }
                let _ = reply.send(());
            }
            Command::End => {
                if let Some(scrollback) = &scrollback
                    && let Err(error) = scrollback.remove()
                {
                    eprintln!(
                        "Could not remove scrollback for {}. {error:#}",
                        lifecycle.session
                    );
                }
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

/// An exit the actor reports once the end of the output has arrived.
struct PendingExit {
    detail: String,
    /// When the exit may be reported if the output has closed.
    earliest: Instant,
    /// When the exit is reported even if a descendant keeps the output open.
    latest: Instant,
}

impl PendingExit {
    fn new(detail: String, shell: bool, now: Instant) -> Self {
        // Shells report at once. An agent session's exit waits out the grace period,
        // after which [`Lifecycle::notify_exit`] drops it if shutdown has begun.
        let (earliest, latest) = if shell {
            (now, now + OUTPUT_DRAIN)
        } else {
            (now + AGENT_EXIT_GRACE, now + AGENT_EXIT_GRACE)
        };
        Self {
            detail,
            earliest,
            latest,
        }
    }

    fn due(&self, output_closed: bool) -> Instant {
        if output_closed {
            self.earliest
        } else {
            self.latest
        }
    }
}

/// What a shell session last saved for a restore.
struct RestoreState {
    shell: bool,
    /// Foreground agent and conversation as saved.
    saved: (Agent, Option<String>),
    /// Conversation reported by the agent wrapper, possibly before the agent runs.
    conversation: Option<(Agent, String)>,
    cwd: Option<PathBuf>,
    cwd_checked: Instant,
    /// When a shell in the foreground makes the saved agent stale.
    clear_at: Option<Instant>,
    /// Whether this actor has saved anything. Until then, the record of a restored
    /// session keeps the agent it is about to resume.
    touched: bool,
}

impl RestoreState {
    fn new(launched: Agent, cwd: PathBuf) -> Self {
        Self {
            shell: launched == Agent::Shell,
            saved: (launched, None),
            conversation: None,
            cwd: Some(cwd),
            cwd_checked: Instant::now(),
            clear_at: None,
            touched: false,
        }
    }

    fn save(&mut self, lifecycle: &Lifecycle, flush: bool) {
        self.touched = true;
        lifecycle.record_foreground(
            &self.saved.0,
            self.saved.1.as_deref(),
            self.cwd.as_deref(),
            flush,
        );
    }

    fn foreground_changed(&mut self, foreground: Option<&Agent>, lifecycle: &Lifecycle) {
        if let Some(agent) = foreground {
            self.clear_at = None;
            let conversation = self
                .conversation
                .as_ref()
                .filter(|(owner, _)| owner == agent)
                .map(|(_, id)| id.clone());
            self.saved = (agent.clone(), conversation);
            self.save(lifecycle, false);
        } else {
            // A conversation reported from now on belongs to the next agent.
            self.conversation = None;
            self.clear_at = Some(Instant::now() + CLEAR_DELAY);
        }
    }

    fn set_conversation(
        &mut self,
        agent: Agent,
        conversation: String,
        foreground: Option<&Agent>,
        lifecycle: &Lifecycle,
    ) {
        if foreground == Some(&agent) {
            self.saved = (agent.clone(), Some(conversation.clone()));
            self.save(lifecycle, false);
        }
        self.conversation = Some((agent, conversation));
    }

    fn poll(&mut self, shell_foreground: bool, shell_pid: Option<u32>, lifecycle: &Lifecycle) {
        let now = Instant::now();
        if self.clear_at.is_some_and(|at| now >= at) {
            self.clear_at = None;
            if shell_foreground {
                self.saved = (Agent::Shell, None);
                self.save(lifecycle, false);
            }
        }
        if now >= self.cwd_checked + CWD_INTERVAL {
            self.cwd_checked = now;
            let cwd = shell_pid.and_then(process_directory);
            if cwd.is_some() && cwd != self.cwd {
                self.cwd = cwd;
                self.save(lifecycle, false);
            }
        }
    }

    fn flush(&mut self, shell_pid: Option<u32>, lifecycle: &Lifecycle) {
        if !self.shell || !self.touched {
            return;
        }
        if let Some(cwd) = shell_pid.and_then(process_directory) {
            self.cwd = Some(cwd);
        }
        self.save(lifecycle, true);
    }
}

const SCROLLBACK_LIMIT: usize = 2 * 1024 * 1024;
const SCROLLBACK_INTERVAL: Duration = Duration::from_secs(30);
const CONTINUATION_LIMIT: usize = 64 * 1024;
const LEAVE_ALT_SCREEN: &[u8] = b"\x1b[?1049l";

struct Scrollback {
    path: PathBuf,
    dirty: bool,
    save_at: Instant,
}

impl Scrollback {
    fn new(state_dir: &Path, session: &str) -> Result<Self> {
        anyhow::ensure!(
            Path::new(session).file_name() == Some(OsStr::new(session)),
            "Invalid scrollback session ID"
        );
        Ok(Self {
            path: state_dir.join("scrollback").join(session),
            dirty: false,
            save_at: Instant::now() + SCROLLBACK_INTERVAL,
        })
    }

    fn restore(&mut self, terminal: &mut Terminal) -> Result<()> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        terminal.vt_write(&bytes);
        terminal.vt_write(b"\x18"); // Cancel a partial VT sequence before writing the separator.
        if terminal.active_screen()? == Screen::Alternate {
            terminal.vt_write(LEAVE_ALT_SCREEN);
        }
        // DECSTR resets modes, margins and the cursor style without clearing the history.
        terminal.vt_write(b"\x1b[!p\x1b]8;;\x1b\\\x1b[0m\r\n\x1b[2m");
        terminal.vt_write("── restored after restart ──".as_bytes());
        terminal.vt_write(b"\x1b[0m\r\n");
        terminal.events().for_each(drop);
        self.dirty = true;
        self.remove()?;
        Ok(())
    }

    fn deadline(&self) -> Option<Instant> {
        self.dirty.then_some(self.save_at)
    }

    fn flush(&mut self, terminal: &Terminal) {
        if !self.dirty {
            return;
        }
        self.save_at = Instant::now() + SCROLLBACK_INTERVAL;
        match format_scrollback(terminal).and_then(|bytes| write_scrollback(&self.path, &bytes)) {
            Ok(()) => self.dirty = false,
            Err(error) => eprintln!(
                "Could not save scrollback at {}. {error:#}",
                self.path.display()
            ),
        }
    }

    fn remove(&self) -> io::Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => File::open(self.path.parent().unwrap())?.sync_all(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

fn format_scrollback(terminal: &Terminal) -> Result<Vec<u8>> {
    // VT formatting emits SGR and CRLF. Preserve physical line boundaries so the
    // saved tail can start at a complete row. Omit the old application's modes.
    let options = || {
        FormatterOptions::new()
            .with_format(Format::Vt)
            .with_trim(true)
    };
    let mut bytes = Vec::new();
    if terminal.active_screen()? == Screen::Alternate {
        // The formatter sees only the active screen. Switch a copy so saving
        // the primary history cannot disturb the live TUI or its VT parser.
        let snapshot = snapshot::encode(terminal).context("Encode terminal snapshot")?;
        let mut copy = snapshot::Decoder::from_slice(&snapshot)
            .decode()
            .context("Decode terminal snapshot")?;
        copy.vt_write(b"\x18");
        copy.vt_write(LEAVE_ALT_SCREEN);
        Formatter::new(&copy, options())?.format_to(&mut bytes)?;
        if !bytes.is_empty() {
            bytes.extend_from_slice(b"\r\n");
        }
    }
    Formatter::new(terminal, options())?.format_to(&mut bytes)?;
    Ok(bytes)
}

fn write_scrollback(path: &Path, bytes: &[u8]) -> Result<()> {
    let directory = path.parent().context("Invalid scrollback path")?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    fs::set_permissions(directory, Permissions::from_mode(0o700))?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary
        .as_file()
        .set_permissions(Permissions::from_mode(0o600))?;
    temporary.write_all(scrollback_tail(bytes))?;
    temporary.as_file().sync_all()?;
    temporary.persist(path)?;
    File::open(directory)?.sync_all()?;
    Ok(())
}

fn scrollback_tail(bytes: &[u8]) -> &[u8] {
    if bytes.len() <= SCROLLBACK_LIMIT {
        return bytes;
    }
    // Skip through the first complete CRLF at or past the cap. This avoids
    // starting the replay inside a UTF-8 character or a VT escape sequence.
    let tail = &bytes[bytes.len() - SCROLLBACK_LIMIT - 1..];
    tail.windows(2)
        .position(|pair| pair == b"\r\n")
        .map_or(&[], |newline| &tail[newline + 2..])
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
    use ghostty_vt::terminal::{Point, PointCoordinate};

    /// Whether each relaunch had a PID, and the conversation it reported.
    type Reports = Vec<(bool, Option<String>)>;

    /// Runs `first` with a fresh start of `fresh` and returns the exit status, the
    /// relaunch reports and the PTY output.
    fn wait_with_relaunch(
        first: &str,
        fresh: &str,
        window: Duration,
        shutting_down: bool,
    ) -> Result<(u32, Reports, String)> {
        let pair = native_pty_system().openpty(pty_size(Geometry::default()))?;
        let shell = |script: &str| {
            let mut command = CommandBuilder::new("/bin/sh");
            command.args(["-c", script]);
            command
        };
        let process = pair.slave.spawn_command(shell(first))?;
        let relaunch = Relaunch {
            command: shell(fresh),
            conversation: Some("conv-1".into()),
            slave: pair.slave,
            deadline: Instant::now() + window,
        };
        let mut reader = pair.master.try_clone_reader()?;
        let output = thread::spawn(move || {
            let mut output = Vec::new();
            let _ = reader.read_to_end(&mut output);
            String::from_utf8_lossy(&output).into_owned()
        });
        let (_stop, stop) = mpsc::channel();
        let mut reports = Vec::new();
        let status = wait_process(
            process,
            Some(relaunch),
            &stop,
            &AtomicBool::new(shutting_down),
            |pid, conversation| reports.push((pid.is_some(), conversation.map(Into::into))),
        )?;
        drop(pair.master);
        Ok((status.exit_code(), reports, output.join().unwrap()))
    }

    #[test]
    fn a_resume_that_exits_at_once_is_relaunched_once_with_its_fresh_start() -> Result<()> {
        let (code, reports, output) = wait_with_relaunch(
            "echo resume; exit 1",
            "echo fresh; exit 3",
            RESUME_WINDOW,
            false,
        )?;
        // The fresh start also exits at once, but there is no second retry.
        assert_eq!(code, 3);
        assert_eq!(reports, [(true, Some("conv-1".into()))]);
        assert!(
            output.contains("resume") && output.contains("fresh"),
            "{output:?}"
        );
        Ok(())
    }

    #[test]
    fn a_resume_that_runs_past_the_window_is_not_relaunched() -> Result<()> {
        let (code, reports, output) = wait_with_relaunch(
            "sleep 0.5; exit 1",
            "echo fresh",
            Duration::from_millis(100),
            false,
        )?;
        assert_eq!(code, 1);
        assert!(reports.is_empty());
        assert!(!output.contains("fresh"), "{output:?}");
        Ok(())
    }

    #[test]
    fn shutdown_never_relaunches_a_failed_resume() -> Result<()> {
        let (code, reports, _) = wait_with_relaunch("exit 1", "echo fresh", RESUME_WINDOW, true)?;
        assert_eq!(code, 1);
        assert!(reports.is_empty());
        Ok(())
    }

    #[test]
    fn agent_exits_wait_for_shutdown_to_begin_before_they_are_reported() {
        let now = Instant::now();
        let shell = PendingExit::new("exit".into(), true, now);
        assert_eq!(shell.due(true), now);
        assert_eq!(shell.due(false), now + OUTPUT_DRAIN);
        let agent = PendingExit::new("exit".into(), false, now);
        assert_eq!(agent.due(true), now + AGENT_EXIT_GRACE);
        assert_eq!(agent.due(false), now + AGENT_EXIT_GRACE);
    }

    #[test]
    fn exits_during_shutdown_are_not_reported() {
        let reported = Arc::new(Mutex::new(Vec::new()));
        let lifecycle = Lifecycle {
            running: AtomicBool::new(true),
            session: "worker".into(),
            on_exit: {
                let reported = reported.clone();
                Arc::new(move |session: &str, _: &str| {
                    reported.lock().unwrap().push(session.to_owned())
                })
            },
            foreground: Mutex::new(Foreground {
                agent: Agent::Coding("claude".into()),
                model: None,
            }),
            on_model_changed: Arc::new(|_, _, _| Ok(())),
            on_foreground_changed: Arc::new(|_, _, _, _| Ok(())),
            shutting_down: Arc::new(AtomicBool::new(false)),
        };
        // A reboot sets shutdown during the agent's exit grace period.
        lifecycle.shutting_down.store(true, Ordering::Release);
        lifecycle.notify_exit("Worker process exited");
        assert!(reported.lock().unwrap().is_empty());
    }

    fn test_terminal() -> Terminal {
        let mut terminal = Terminal::new(Options {
            cols: 40,
            rows: 3,
            ..Options::default()
        })
        .unwrap();
        terminal
            .set_continuation_max_bytes(CONTINUATION_LIMIT)
            .unwrap();
        terminal
    }

    fn terminal_text(terminal: &Terminal) -> String {
        let bytes = Formatter::new(terminal, FormatterOptions::new().with_trim(true))
            .unwrap()
            .format_alloc()
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[test]
    fn scrollback_vt_round_trip_preserves_history_screen_and_style() -> Result<()> {
        let mut original = test_terminal();
        original.vt_write(
            "\x1b[1;31mold red line\x1b[0m\r\nsecond\r\n\r\nthird\r\n  current 界".as_bytes(),
        );
        let mut replayed = test_terminal();
        replayed.vt_write(&format_scrollback(&original)?);

        assert_eq!(terminal_text(&replayed), terminal_text(&original));
        assert_eq!(replayed.scrollback_rows()?, 2);
        let history_start = Point::History(PointCoordinate { x: 0, y: 0 });
        let style = replayed.grid_ref(history_start)?.style()?;
        assert!(!style.is_default());
        assert_eq!(style, original.grid_ref(history_start)?.style()?);
        Ok(())
    }

    #[test]
    fn scrollback_replay_consumes_snapshot_and_allows_a_new_alternate_screen() -> Result<()> {
        let state_dir = tempfile::tempdir()?;
        let mut store = Scrollback::new(state_dir.path(), "test-session")?;
        let mut original = test_terminal();
        original.vt_write(b"first\r\nsecond\r\nthird\r\nold prompt");
        original.vt_write(b"\x1b[?1049h\x1b[?25l\x1b[?2004h\x1b[Hprevious TUI\x1b[");
        store.dirty = true;
        store.flush(&original);
        assert!(!store.dirty);
        assert_eq!(original.active_screen()?, Screen::Alternate);
        assert!(!original.is_vt_ground()?);

        let mut replayed = test_terminal();
        store.restore(&mut replayed)?;
        assert!(store.dirty);
        assert!(!store.path.exists());
        assert_eq!(replayed.active_screen()?, Screen::Primary);
        assert!(replayed.is_cursor_visible()?);
        assert!(!replayed.mode(Mode::BRACKETED_PASTE)?);
        assert!(replayed.cursor_style()?.is_default());
        replayed.vt_write(b"new prompt");
        let restored = terminal_text(&replayed);
        assert_eq!(
            restored,
            "first\nsecond\nthird\nold prompt\nprevious TUI\n── restored after restart ──\nnew prompt"
        );
        assert!(
            replayed
                .grid_ref(Point::Active(PointCoordinate { x: 0, y: 1 }))?
                .style()?
                .faint
        );

        replayed.vt_write(b"\x1b[?1049h\x1b[Hnew TUI");
        assert_eq!(replayed.active_screen()?, Screen::Alternate);
        assert_eq!(terminal_text(&replayed), "new TUI");
        replayed.vt_write(LEAVE_ALT_SCREEN);
        assert_eq!(terminal_text(&replayed), restored);

        let mut unused = test_terminal();
        store.restore(&mut unused)?;
        assert!(terminal_text(&unused).trim().is_empty());
        Ok(())
    }

    #[test]
    fn scrollback_file_keeps_the_newest_complete_rows_within_the_size_cap() -> Result<()> {
        let state_dir = tempfile::tempdir()?;
        let store = Scrollback::new(state_dir.path(), "test-session")?;
        let line = "\x1b[31molder 界 row\x1b[0m\r\n".repeat(SCROLLBACK_LIMIT / 20);
        let bytes = line + "\x1b[1mnewest row\x1b[0m";
        assert!(bytes.len() > SCROLLBACK_LIMIT);
        write_scrollback(&store.path, bytes.as_bytes())?;

        let saved = fs::read_to_string(&store.path)?;
        assert!(saved.len() <= SCROLLBACK_LIMIT);
        assert!(saved.len() > SCROLLBACK_LIMIT - 32);
        assert!(saved.starts_with("\x1b[31molder 界 row\x1b[0m\r\n"));
        assert!(saved.ends_with("\x1b[1mnewest row\x1b[0m"));
        Ok(())
    }

    #[test]
    fn scrollback_replaces_the_file_atomically_with_private_permissions() -> Result<()> {
        let state_dir = tempfile::tempdir()?;
        let store = Scrollback::new(state_dir.path(), "test-session")?;
        let directory = state_dir.path().join("scrollback");
        write_scrollback(&store.path, b"old scrollback")?;
        let mut previous = File::open(&store.path)?;
        write_scrollback(&store.path, b"new scrollback")?;

        let mut old = String::new();
        previous.read_to_string(&mut old)?;
        assert_eq!(old, "old scrollback");
        assert_eq!(fs::read(&store.path)?, b"new scrollback");
        assert_eq!(
            fs::metadata(&directory)?.permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&store.path)?.permissions().mode() & 0o777,
            0o600
        );
        let other_session = directory.join("other-session");
        fs::create_dir(&other_session)?;
        assert!(write_scrollback(&other_session, b"failed write").is_err());
        assert!(other_session.is_dir());
        assert_eq!(fs::read_dir(&directory)?.count(), 2);
        Ok(())
    }

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
            model_callback: domain::agent::ModelCallback::None,
            skills_dir: None,
            resume: None,
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
