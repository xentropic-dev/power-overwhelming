use crate::wire::Response;
use anyhow::Result;
use application::TerminalRuntime;
use base64::{Engine, engine::general_purpose::STANDARD};
use domain::session::{Agent, Geometry, Session};
use ghostty_vt::{
    Terminal, snapshot,
    terminal::{Event, Options},
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::Duration,
};

const SKILL: &str = include_str!("../../skills/publish-artifact/SKILL.md");

pub struct PtyRuntime(pub PathBuf);

impl TerminalRuntime for PtyRuntime {
    type Handle = SyncSender<Command>;

    fn start(&self, session: &Session) -> Result<Self::Handle> {
        launch(session, &self.0)
    }

    fn end(&self, handle: Self::Handle) -> Result<()> {
        Ok(handle.send(Command::End)?)
    }
}

pub enum Command {
    Attach(SyncSender<Response>),
    Input(Vec<u8>),
    Resize(Geometry),
    Output(Vec<u8>),
    Exit,
    End,
}

fn launch(session: &Session, socket: &Path) -> Result<SyncSender<Command>> {
    let geometry = Geometry::default();
    let pair = native_pty_system().openpty(pty_size(geometry))?;
    let instructions = SKILL.splitn(3, "---").nth(2).unwrap_or(SKILL).trim();
    let mut process = match session.agent {
        Agent::Shell => {
            CommandBuilder::new(std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into()))
        }
        Agent::Codex => {
            let mut command = CommandBuilder::new("codex");
            command.args([
                "-c",
                &format!(
                    "developer_instructions={}",
                    serde_json::to_string(instructions)?
                ),
            ]);
            command
        }
        Agent::Claude => {
            let mut command = CommandBuilder::new("claude");
            command.args(["--append-system-prompt", instructions]);
            command
        }
    };
    process.cwd(&session.worktree);
    process.env("TERM", "xterm-256color");
    process.env("COLORTERM", "truecolor");
    process.env("POWER_SESSION", &session.id);
    process.env("POWER_SOCKET", socket);
    process.env_remove("TMUX");
    if let Ok(executable) = std::env::current_exe()
        && let Some(dir) = executable.parent()
    {
        let inherited = std::env::var_os("PATH").unwrap_or_default();
        if let Ok(path) = std::env::join_paths(
            std::iter::once(dir.to_owned()).chain(std::env::split_paths(&inherited)),
        ) {
            process.env("PATH", path);
        }
    }

    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;
    let process = Arc::new(Mutex::new(pair.slave.spawn_command(process)?));
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
        let _ = reader_tx.send(Command::Exit);
    });
    let waiter = process.clone();
    thread::spawn(move || {
        while matches!(waiter.lock().unwrap().try_wait(), Ok(None)) {
            thread::sleep(Duration::from_millis(100));
        }
    });
    thread::spawn(move || {
        if let Err(error) = terminal_actor(rx, write_tx, pair.master, geometry) {
            eprintln!("Terminal session failed. {error:#}");
        }
        let mut process = process.lock().unwrap();
        if matches!(process.try_wait(), Ok(None)) {
            let _ = process.kill();
        }
    });
    Ok(tx)
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
) -> Result<()> {
    let mut terminal = Terminal::new(Options {
        cols: geometry.cols,
        rows: geometry.rows,
        ..Options::default()
    })?;
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

    for command in rx {
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
            Command::Exit => {
                exited = true;
                clients.retain(|client| client.try_send(Response::Exit).is_ok());
            }
            Command::End => break,
        }
        for event in terminal.events() {
            if let Event::PtyWrite(bytes) = event {
                let _ = writer.try_send(bytes);
            }
        }
    }
    Ok(())
}
