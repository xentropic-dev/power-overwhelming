mod gui;
mod preview;
mod terminal_view;

use anyhow::{Context, Result};
use application::{PublishArtifact, StartSession};
use clap::{Parser, Subcommand, ValueEnum};
use domain::session::Agent;
use infrastructure::{client, daemon};
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "Persistent agent terminals and artifact previews")]
struct Cli {
    #[command(subcommand)]
    command: Option<Action>,
}

#[derive(Subcommand)]
enum Action {
    Gui {
        #[arg(default_value = ".")]
        directory: PathBuf,
    },
    Daemon,
    Session {
        #[command(subcommand)]
        command: SessionAction,
    },
    Artifact {
        #[command(subcommand)]
        command: ArtifactAction,
    },
}

#[derive(Subcommand)]
enum SessionAction {
    List,
    Worktrees {
        #[arg(default_value = ".")]
        directory: PathBuf,
    },
    Start {
        #[arg(value_enum)]
        agent: AgentChoice,
        #[arg(default_value = ".")]
        directory: PathBuf,
    },
    End {
        id: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum AgentChoice {
    Shell,
    Codex,
    Claude,
}

impl From<AgentChoice> for Agent {
    fn from(choice: AgentChoice) -> Self {
        match choice {
            AgentChoice::Shell => Self::Shell,
            AgentChoice::Codex => Self::Codex,
            AgentChoice::Claude => Self::Claude,
        }
    }
}

#[derive(Subcommand)]
enum ArtifactAction {
    Publish {
        target: String,
        #[arg(long)]
        title: Option<String>,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Some(Action::Daemon) => daemon::serve(),
        Some(Action::Artifact {
            command: ArtifactAction::Publish { target, title },
        }) => {
            let session = std::env::var("POWER_SESSION")
                .context("Publish from a Power Overwhelming terminal")?;
            let uri = client::publish_artifact(PublishArtifact {
                session,
                cwd: std::env::current_dir()?,
                target,
                title,
            })?;
            println!("{uri}");
            Ok(())
        }
        Some(Action::Session { command }) => {
            client::ensure_daemon()?;
            match command {
                SessionAction::List => {
                    for session in client::list_sessions()? {
                        println!(
                            "{}\t{}\t{}",
                            session.id,
                            session.agent.name(),
                            session.worktree.display()
                        );
                    }
                }
                SessionAction::Worktrees { directory } => {
                    for path in client::list_worktrees(directory.canonicalize()?)? {
                        println!("{}", path.display());
                    }
                }
                SessionAction::Start { agent, directory } => {
                    let session = client::start_session(StartSession {
                        worktree: directory.canonicalize()?,
                        agent: agent.into(),
                    })?;
                    println!("{}", session.id);
                }
                SessionAction::End { id } => client::end_session(id)?,
            }
            Ok(())
        }
        command => {
            let directory = match command {
                Some(Action::Gui { directory }) => directory,
                _ => std::env::current_dir()?,
            };
            client::ensure_daemon()?;
            gui::run(directory.canonicalize()?);
            Ok(())
        }
    }
}
