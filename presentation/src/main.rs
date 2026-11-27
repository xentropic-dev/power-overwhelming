mod gui;
mod keymap;
mod picker;
mod preview;
mod settings;
mod status_bar;
mod talk_cli;
mod talk_dialog;
mod terminal;

use anyhow::{Context, Result};
use application::{
    PublishArtifact, StartSession,
    agents::AgentProfileRegistry,
    skill_install::{InstallSkills, SkillAction, select_profiles},
    skills::{GetSkill, ListSkills, SkillOutput, SkillSelection},
};
use clap::{Args, Parser, Subcommand, ValueEnum};
use domain::{agent::display_name, session::Agent};
use infrastructure::{
    agents::AgentProfiles,
    budget::LocalWorkerBudget,
    client::{
        self,
        protocol::{
            BudgetMode, DagRequest, DagResponse, Provider, ReportStatus, RunEventKind, TaskSpec,
            TaskState, WorktreeRequest,
        },
    },
    config, daemon,
    shell::{self, Shell},
    skill_install::{LocalSkillFilesystem, install_targets},
    skills::EmbeddedSkillCatalog,
    workspace::LocalWorktreeRoot,
};
use serde_json::{Value, json};
use std::{ffi::OsString, io::Write as _, path::PathBuf, time::Duration};

const AGENT_HELP_FOOTER: &str = concat!(
    "Are you an AI? Use these resources ONLY IF your task asks you to:\n",
    "  Use Power (artifacts, DAG workers, sessions):\n",
    "    SKIP if a Power skill is already in your context. Otherwise run: power skills list",
);

#[derive(Parser)]
#[command(
    about = "Persistent agent terminals and artifact previews",
    after_help = AGENT_HELP_FOOTER
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Action>,
}

#[derive(Subcommand)]
enum Action {
    Gui,
    Daemon,
    /// Check that restarting the daemon would not kill DAG workers mid-task. Lists the
    /// open attempts and asks to confirm, or fails when it cannot ask or the daemon does
    /// not answer. Prints the PID of the daemon it cleared, which is the only one to
    /// restart, or nothing when no daemon listens. Run it before anything that restarts
    /// the daemon.
    RestartCheck {
        /// Allow the restart even with workers mid-task or a daemon that does not answer.
        #[arg(long)]
        force: bool,
        /// Only print the PID of the daemon listening on this socket, without checking.
        #[arg(long, conflicts_with = "force")]
        target: bool,
    },
    Session {
        #[command(subcommand)]
        command: SessionAction,
    },
    Artifact {
        #[command(subcommand)]
        command: ArtifactAction,
    },
    /// Install the shell hook that wraps coding agents inside Power terminals.
    Setup {
        /// Print what would change without writing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Print shell code for Power terminals. The installed hook runs this.
    ShellInit {
        #[arg(value_parser = ["bash", "zsh", "fish"])]
        shell: String,
    },
    /// Runs a worker under a subreaper that ends everything it starts. The daemon
    /// uses it without cgroup ownership.
    #[command(hide = true)]
    Reap {
        #[arg(last = true)]
        argv: Vec<OsString>,
    },
    #[command(hide = true)]
    ExecAgent {
        id: String,
        #[arg(last = true)]
        args: Vec<OsString>,
    },
    #[command(hide = true)]
    Codex {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    #[command(hide = true)]
    Claude {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    Box {
        #[command(subcommand)]
        command: BoxAction,
    },
    Agent {
        #[command(subcommand)]
        command: AgentAction,
    },
    #[command(after_help = AGENT_HELP_FOOTER)]
    Dag {
        #[command(subcommand)]
        command: DagAction,
    },
    /// Exchange messages with agents and the user.
    Talk(talk_cli::TalkArgs),
    /// List and prune Power-managed Git worktrees.
    Worktree {
        #[command(subcommand)]
        command: WorktreeAction,
    },
    /// Read and install skills bundled with this Power release.
    #[command(after_help = AGENT_HELP_FOOTER)]
    Skills {
        #[command(subcommand)]
        command: SkillsAction,
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
        /// Profile ID or `shell`.
        agent: String,
        #[arg(default_value = ".")]
        directory: PathBuf,
    },
    /// Set this terminal's active agent task. Pass an empty label to clear it.
    Task {
        label: String,
    },
    /// Report this terminal's active model from a provider integration.
    Model {
        #[arg(required_unless_present = "stdin_json")]
        name: Option<String>,
        /// Read provider hook or status-line JSON from stdin. Skip reports with no model.
        #[arg(long = "json", conflicts_with = "name")]
        stdin_json: bool,
    },
    End {
        id: String,
    },
}

#[derive(Subcommand)]
enum WorktreeAction {
    /// Print managed worktrees as JSON.
    List(WorktreeScopeArgs),
    /// Remove clean, merged, unused worktrees.
    Prune {
        #[command(flatten)]
        scope: WorktreeScopeArgs,
        /// Show what would be removed.
        #[arg(long)]
        dry_run: bool,
        /// Also delete the branch of each removed worktree.
        #[arg(long)]
        delete_branch: bool,
    },
}

#[derive(Args)]
struct WorktreeScopeArgs {
    /// Select a box by ID, shortcut, or repository name. Defaults to the current Power box.
    #[arg(long = "box", value_name = "BOX", conflicts_with = "all")]
    box_sel: Option<String>,
    /// Include managed worktrees from every box.
    #[arg(long)]
    all: bool,
}

#[derive(Subcommand)]
enum ArtifactAction {
    Publish {
        target: String,
        #[arg(long)]
        title: Option<String>,
    },
}

#[derive(Subcommand)]
enum SkillsAction {
    /// List bundled skills and their descriptions.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Print a skill's guide or select its references.
    #[command(visible_alias = "show")]
    Get {
        name: String,
        /// Print one reference by its name or path.
        #[arg(long, value_name = "R", conflicts_with_all = ["references", "full"])]
        reference: Option<String>,
        /// List the skill's reference names.
        #[arg(long, conflicts_with = "full")]
        references: bool,
        /// Print the guide and all its references.
        #[arg(long)]
        full: bool,
        #[arg(long)]
        json: bool,
    },
    /// Install bundled skill stubs and link them for detected agents.
    Install(SkillInstallArgs),
    /// Refresh existing skill stubs and links.
    Update(SkillInstallArgs),
}

#[derive(Args)]
struct SkillInstallArgs {
    /// Select a bundled skill by name. Repeat to select multiple skills. Defaults to all.
    #[arg(long = "skill", value_name = "NAME")]
    names: Vec<String>,
    /// Select agent profile ids to link for. `universal` selects the shared folder alone.
    /// Defaults to every detected agent.
    #[arg(long = "agent", value_name = "ID", value_delimiter = ',')]
    agents: Vec<String>,
    /// Target the current Git worktree.
    #[arg(long)]
    local: bool,
    /// Print the planned actions without writing.
    #[arg(long)]
    dry_run: bool,
    /// Back up and replace conflicting copies.
    #[arg(long)]
    force: bool,
    /// Print location outcomes as JSON.
    #[arg(long)]
    json: bool,
}

fn main() -> Result<()> {
    let cli = Cli::try_parse().unwrap_or_else(|error| {
        let mut args = std::env::args_os().skip(1);
        let command = args.next();
        let command_uses_exit_one = match command.as_deref().and_then(|arg| arg.to_str()) {
            Some("box" | "dag" | "talk") => true,
            Some("agent") => !matches!(
                args.next().as_deref().and_then(|arg| arg.to_str()),
                Some("codex" | "claude")
            ),
            _ => false,
        };
        if error.use_stderr() && command_uses_exit_one {
            let _ = error.print();
            std::process::exit(1);
        }
        error.exit()
    });
    match cli.command {
        Some(Action::Daemon) => daemon::serve(
            configured_agents(),
            LocalWorktreeRoot::new(settings::Settings::defaults()),
            finished_worker_grace(),
            config::load_talk_policy(settings::Settings::defaults()).unwrap_or_else(|error| {
                eprintln!("Could not load talk settings. {error:#}");
                domain::talk::NoticePolicy::default()
            }),
            LocalWorkerBudget::new(settings::Settings::defaults()),
            config::load_stall_after(settings::Settings::defaults()).unwrap_or_else(|error| {
                eprintln!("Could not load dag.stall_after. {error:#}");
                Some(application::dag::STALL_AFTER)
            }),
        ),
        Some(Action::RestartCheck { force, target }) => restart_check(
            force || std::env::var_os("POWER_FORCE_RESTART").is_some_and(|value| value == "1"),
            target,
        ),
        Some(Action::Talk(args)) => talk_cli::run(args),
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
        Some(Action::Skills { command }) => run_skills(command),
        Some(Action::Dag {
            command: DagAction::Guide,
        }) => print_skill("power-dag", SkillSelection::Full, false),
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
                        agent: if agent == "shell" {
                            Agent::Shell
                        } else {
                            Agent::Coding(agent.into())
                        },
                    })?;
                    println!("{}", session.id);
                }
                SessionAction::End { id } => client::end_session(id)?,
                SessionAction::Task { label } => {
                    report_conversation();
                    client::set_task(
                        std::env::var("POWER_SESSION")
                            .context("Set a task from a Power terminal")?,
                        label,
                    )?
                }
                SessionAction::Model { name, .. } => {
                    report_conversation();
                    report_model(name)?
                }
            }
            Ok(())
        }
        Some(Action::Setup { dry_run }) => {
            let shell = Shell::detect().context(
                "Shell integration supports bash, zsh and fish. Set SHELL to one of them.",
            )?;
            let rc = shell
                .rc_file()
                .context("Set HOME to install shell integration")?;
            if shell.installed() {
                println!("Shell integration is already in {}", rc.display());
            } else if dry_run {
                println!("Would append to {}:\n{}", rc.display(), shell.hook());
            } else {
                shell.install()?;
                println!("Added shell integration to {}", rc.display());
            }
            Ok(())
        }
        Some(Action::ShellInit { shell }) => {
            print!(
                "{}",
                Shell::parse(&shell)
                    .unwrap()
                    .init_script(&configured_agents())
            );
            Ok(())
        }
        Some(Action::Box {
            command: BoxAction::List,
        }) => {
            client::ensure_daemon()?;
            print_json(json!({"boxes": protocol_boxes()?}))
        }
        Some(Action::Reap { argv }) => match infrastructure::reaper::run(argv)? {},
        Some(Action::ExecAgent { id, args }) => exec_agent(&id, args),
        Some(Action::Codex { args }) => exec_agent("codex", args),
        Some(Action::Claude { args }) => exec_agent("claude", args),
        Some(Action::Box {
            command:
                BoxAction::Icon {
                    path,
                    reset: _,
                    box_sel,
                },
        }) => {
            // The daemon runs elsewhere, so resolve the path against this terminal's directory.
            let path = path
                .map(|path| {
                    path.canonicalize()
                        .with_context(|| format!("Icon is unavailable at {}", path.display()))
                })
                .transpose()?;
            client::ensure_daemon()?;
            let box_id = client::set_box_icon(box_sel, std::env::var("POWER_SESSION").ok(), path)?;
            print_json(json!({"box_id": box_id}))
        }
        Some(Action::Agent { command }) => match command {
            AgentAction::Codex { args } => exec_agent("codex", args),
            AgentAction::Claude { args } => exec_agent("claude", args),
            AgentAction::List {
                box_sel,
                all,
                all_boxes,
            } => {
                client::ensure_daemon()?;
                let caller = (!all_boxes).then(caller_session).flatten();
                let scoped = box_sel.is_none() && caller.is_some();
                let agents = list_agents(box_sel, caller, all)?;
                if scoped {
                    let others = list_agents(None, None, all)?
                        .len()
                        .saturating_sub(agents.len());
                    if others > 0 {
                        eprintln!(
                            "{others} more in other boxes; pass --all-boxes or --box to see them"
                        );
                    }
                }
                print_json(json!({"agents": agents}))
            }
            AgentAction::Pause { ids, all_except } => {
                client::ensure_daemon()?;
                if all_except {
                    let request = DagRequest::PauseAllExcept {
                        keep: ids,
                        caller: caller_session(),
                    };
                    return dag_reply(request, Duration::from_secs(30));
                }
                set_paused(ids, true)
            }
            AgentAction::Resume { ids } => {
                client::ensure_daemon()?;
                set_paused(ids, false)
            }
            AgentAction::End { id, box_sel } | AgentAction::Stop { id, box_sel } => {
                client::ensure_daemon()?;
                let agent = protocol_agents(box_sel, true)?
                    .into_iter()
                    .find(|agent| agent.id == id)
                    .with_context(|| format!("No agent {id} in this box."))?;
                // The daemon ends only the agent's own session, and refuses an agent
                // that shares a shell session with the shell's other jobs.
                client::stop_agent(agent.id).map(drop)
            }
            AgentAction::Start {
                box_sel,
                provider,
                worktree,
                parent,
                label,
                prompt,
                args,
            } => {
                let allow_main_checkout = worktree.allow_main_checkout;
                let worktree = worktree.request(false, "starting an agent")?;
                client::ensure_daemon()?;
                anyhow::ensure!(
                    provider != "command",
                    "Specify a coding agent profile for --provider"
                );
                let label = label.unwrap_or_else(|| display_name(&provider).into());
                dag_reply(
                    DagRequest::StartAgent {
                        box_sel,
                        caller: caller_session(),
                        provider: Provider::Agent(provider),
                        worktree,
                        allow_main_checkout,
                        parent,
                        label,
                        prompt,
                        args,
                    },
                    Duration::from_secs(10),
                )
            }
        },
        Some(Action::Dag { command }) => {
            client::ensure_daemon()?;
            run_dag(command)
        }
        Some(Action::Worktree { command }) => {
            client::ensure_daemon()?;
            run_worktree(command)
        }
        None | Some(Action::Gui) => {
            gui::run();
            Ok(())
        }
    }
}

fn configured_agents() -> AgentProfiles {
    config::load_agent_profiles(settings::Settings::defaults()).unwrap_or_else(|error| {
        eprintln!("Could not load agent profiles. {error:#}");
        AgentProfiles::default()
    })
}

fn finished_worker_grace() -> Option<std::time::Duration> {
    config::load_finished_worker_grace(settings::Settings::defaults()).unwrap_or_else(|error| {
        eprintln!("Could not load dag settings. {error:#}");
        Some(application::dag::FINISHED_WORKER_GRACE)
    })
}

fn exec_agent(id: &str, args: Vec<OsString>) -> Result<()> {
    let profiles = configured_agents();
    shell::exec_agent(profiles.resolve(&id.into())?, args)
}

#[derive(Subcommand)]
enum BoxAction {
    List,
    /// Replace a box's generated icon with an image (PNG, JPEG, WebP, GIF, ...).
    #[command(arg_required_else_help = true)]
    Icon {
        /// Image to use. Large images are scaled down to 64 px; the original is not kept.
        #[arg(required_unless_present = "reset")]
        path: Option<PathBuf>,
        /// Restore the generated icon.
        #[arg(long, conflicts_with = "path")]
        reset: bool,
        /// Box ID, ID prefix, shortcut digit or repo name. Defaults to this terminal's box.
        #[arg(long = "box")]
        box_sel: Option<String>,
    },
}

#[derive(Subcommand)]
enum AgentAction {
    /// List the box's live agents.
    List {
        #[arg(long = "box")]
        box_sel: Option<String>,
        /// Include exited and done agents.
        #[arg(long)]
        all: bool,
        /// List agents in every box, not just the current one.
        #[arg(long, conflicts_with = "box_sel")]
        all_boxes: bool,
    },
    /// Pause an agent's whole process tree (SIGSTOP). Only its own processes are touched.
    Pause {
        /// Agents to pause. With --all-except, the agents to leave running.
        #[arg(required_unless_present = "all_except")]
        ids: Vec<String>,
        /// Pause every running agent except the ones named, and the caller's own.
        #[arg(long)]
        all_except: bool,
    },
    /// Resume a paused agent's process tree.
    Resume {
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Stop an agent cleanly: end its terminal session and interrupt its open attempt,
    /// which can then be dispatched again with --retry.
    Stop {
        id: String,
        #[arg(long = "box")]
        box_sel: Option<String>,
    },
    /// End an agent's terminal session, interrupting any attempt it still has open.
    End {
        id: String,
        #[arg(long = "box")]
        box_sel: Option<String>,
    },
    Start {
        #[arg(long = "box")]
        box_sel: Option<String>,
        /// Coding agent profile ID.
        #[arg(long)]
        provider: String,
        #[command(flatten)]
        worktree: WorktreeArgs,
        #[arg(long)]
        parent: Option<String>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        prompt: Option<String>,
        #[arg(last = true)]
        args: Vec<String>,
    },
    #[command(hide = true)]
    Codex {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    #[command(hide = true)]
    Claude {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
}

#[derive(Subcommand)]
enum DagAction {
    /// Print the bundled DAG orchestration guide and all its references.
    Guide,
    Create {
        #[arg(long)]
        title: String,
        #[arg(long = "box")]
        box_sel: Option<String>,
        #[arg(long)]
        coordinator: Option<String>,
    },
    /// List the box's unfinished runs.
    List {
        #[arg(long = "box")]
        box_sel: Option<String>,
        /// Include completed and archived runs.
        #[arg(long)]
        all: bool,
    },
    Show {
        run: String,
    },
    /// Hide a run from default listings, such as an abandoned one.
    Archive {
        run: String,
        /// Archive even while attempts are still open.
        #[arg(long)]
        force: bool,
    },
    /// Return an archived run to default listings.
    Unarchive {
        run: String,
    },
    /// Pause every active worker of a run and stop dispatching from it.
    Pause {
        run: String,
    },
    /// Resume a paused run's workers and let it dispatch again.
    Resume {
        run: String,
    },
    AddTask {
        run: String,
        #[arg(long)]
        id: String,
        #[arg(long)]
        title: String,
        #[arg(long, conflicts_with = "instructions_file")]
        instructions: Option<String>,
        #[arg(long)]
        instructions_file: Option<PathBuf>,
        #[arg(long = "after")]
        deps: Vec<String>,
    },
    Apply {
        run: String,
        #[arg(long)]
        file: PathBuf,
    },
    AddDep {
        run: String,
        task: String,
        dep: String,
    },
    Ready {
        run: String,
    },
    Dispatch {
        #[command(flatten)]
        launch: DispatchArgs,
        #[arg(long)]
        retry: bool,
    },
    Retry(DispatchArgs),
    Report {
        #[arg(value_enum)]
        status: ReportChoice,
        #[arg(long)]
        summary: String,
        #[arg(long)]
        run: Option<String>,
        #[arg(long)]
        task: Option<String>,
        #[arg(long)]
        attempt: Option<String>,
    },
    Events(EventArgs),
    Wait {
        #[command(flatten)]
        events: EventArgs,
        #[arg(long, default_value_t = 600, value_name = "SECS")]
        timeout: u64,
    },
}

#[derive(Args)]
struct DispatchArgs {
    run: String,
    task: String,
    /// Coding agent profile ID, or `command` to run an executable directly. Like any
    /// provider, `command` needs `--allow-main-checkout` to run in the main checkout.
    #[arg(long)]
    provider: String,
    #[command(flatten)]
    worktree: WorktreeArgs,
    /// Fail instead of queueing the task when the worker budget is full.
    #[arg(long, conflicts_with = "over_budget")]
    no_queue: bool,
    /// Start even when the worker budget is full. Starts an already queued task now.
    #[arg(long)]
    over_budget: bool,
    #[arg(last = true)]
    args: Vec<String>,
}

// Where a worker runs. Without `--worktree`, Power creates a worktree on `--branch`, and a
// retry without either reuses the previous managed worktree. A doc comment here would
// replace the help text of every command that flattens these arguments.
#[derive(Args)]
struct WorktreeArgs {
    /// Existing checkout to reuse, or `new` (the default) for a Power-managed worktree.
    #[arg(long, value_name = "PATH|new")]
    worktree: Option<PathBuf>,
    /// Name of the branch for a new worktree.
    #[arg(long, value_name = "NAME", required_if_eq("worktree", "new"))]
    branch: Option<String>,
    /// Starting revision for a new worktree. Required outside Power.
    #[arg(long, value_name = "REV")]
    base: Option<String>,
    /// Allow `--worktree` to be the repository's main checkout, for read-only work.
    #[arg(long, requires = "worktree")]
    allow_main_checkout: bool,
}

impl WorktreeArgs {
    fn request(self, retry: bool, action: &str) -> Result<WorktreeRequest> {
        Ok(match self.worktree {
            Some(path) if path.as_os_str() != "new" => {
                anyhow::ensure!(
                    self.branch.is_none() && self.base.is_none(),
                    "--branch and --base create a new worktree, so they can't be used with an \
                     existing --worktree path"
                );
                WorktreeRequest::Existing(path.canonicalize().context("Open worktree")?)
            }
            worktree => {
                anyhow::ensure!(
                    !self.allow_main_checkout,
                    "--allow-main-checkout requires an existing --worktree path"
                );
                match self.branch {
                    Some(branch) => WorktreeRequest::New {
                        branch,
                        base: self.base,
                    },
                    None if retry && worktree.is_none() && self.base.is_none() => {
                        WorktreeRequest::RetryManaged
                    }
                    None => anyhow::bail!(
                        "Specify --branch <name> for a new worktree when {action}, or \
                         --worktree <PATH> to reuse an existing one"
                    ),
                }
            }
        })
    }
}

#[derive(Args)]
struct EventArgs {
    run: String,
    #[arg(long, default_value_t = 0)]
    since: u64,
    #[arg(long = "kind", value_parser = parse_event_kind)]
    kinds: Vec<RunEventKind>,
}

#[derive(Clone, Copy, ValueEnum)]
enum ReportChoice {
    Success,
    Failed,
    NeedsInput,
    Working,
}

fn caller_session() -> Option<String> {
    std::env::var("POWER_SESSION")
        .ok()
        .filter(|id| !id.is_empty())
}

/// Reports the conversation of an agent that exposes its ID to the commands it runs
/// through its profile's `conversation_env`, so a restore can resume it. Best effort: a
/// failed report leaves the command's own work to succeed or fail.
fn report_conversation() {
    let Some(session) = caller_session() else {
        return;
    };
    for profile in configured_agents().profiles() {
        let Some(conversation) = profile
            .conversation_env
            .as_deref()
            .and_then(|name| std::env::var(name).ok())
            .filter(|conversation| !conversation.is_empty())
        else {
            continue;
        };
        let _ = client::set_conversation(
            session.clone(),
            profile.id.as_str().to_owned(),
            conversation,
        );
    }
}

fn report_model(name: Option<String>) -> Result<()> {
    if let Some(name) = name {
        return client::set_model(
            caller_session().context("Set a model from a Power terminal")?,
            name,
        );
    }

    // Read the payload even outside Power so status-line callers can finish writing it.
    let payload: Value = serde_json::from_reader(std::io::stdin().lock())
        .context("Read provider model report from stdin")?;
    let model = &payload["model"];
    let name = [model, &model["id"], &model["display_name"]]
        .into_iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .find(|name| !name.is_empty());
    if let (Some(session), Some(name)) = (caller_session(), name) {
        client::set_model(session, name.to_owned())?;
    }
    Ok(())
}

fn parse_event_kind(value: &str) -> Result<RunEventKind, serde_json::Error> {
    serde_json::from_value(Value::String(value.replace('-', "_")))
}

fn print_json(value: Value) -> Result<()> {
    writeln!(std::io::stdout().lock(), "{value}")?;
    Ok(())
}

fn run_skills(command: SkillsAction) -> Result<()> {
    match command {
        SkillsAction::List { json } => {
            let skills = ListSkills::execute(&EmbeddedSkillCatalog)?;
            if json {
                let skills: Vec<_> = skills
                    .into_iter()
                    .map(|skill| json!({"name": skill.name, "description": skill.description}))
                    .collect();
                return print_json(json!({"skills": skills}));
            }
            let mut stdout = std::io::stdout().lock();
            for skill in skills {
                writeln!(stdout, "{}\t{}", skill.name, skill.description)?;
            }
            Ok(())
        }
        SkillsAction::Get {
            name,
            reference,
            references,
            full,
            json,
        } => {
            let selection = if let Some(reference) = reference.as_deref() {
                SkillSelection::Reference(reference)
            } else if references {
                SkillSelection::References
            } else if full {
                SkillSelection::Full
            } else {
                SkillSelection::Kernel
            };
            print_skill(&name, selection, json)
        }
        SkillsAction::Install(args) => run_skill_install(args, false),
        SkillsAction::Update(args) => run_skill_install(args, true),
    }
}

fn run_skill_install(args: SkillInstallArgs, update: bool) -> Result<()> {
    let profiles = configured_agents();
    let selected = select_profiles(profiles.profiles(), &args.agents)?;
    let targets = install_targets(args.local, &selected)?;
    let outcomes = InstallSkills {
        names: &args.names,
        update,
        dry_run: args.dry_run,
        force: args.force,
    }
    .execute(
        &LocalSkillFilesystem,
        EmbeddedSkillCatalog.stubs()?,
        &targets,
    )?;
    let failed = outcomes
        .iter()
        .filter(|outcome| outcome.error.is_some())
        .count();
    if args.json {
        let locations: Vec<_> = outcomes
            .iter()
            .map(|outcome| {
                json!({
                    "name": outcome.name,
                    "path": outcome.path.to_string_lossy(),
                    "action": outcome.action.map(|action| action.label()),
                    "reason": outcome.reason.map(|reason| reason.label()),
                    "backup_required": outcome.needs_backup,
                    "backup": outcome.backup.as_ref().map(|path| path.to_string_lossy()),
                    "error": outcome.error,
                })
            })
            .collect();
        print_json(json!({"dry_run": args.dry_run, "locations": locations, "failed": failed}))?;
    } else {
        print_skill_install_outcomes(&outcomes, args.dry_run)?;
    }
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn print_skill_install_outcomes(
    outcomes: &[application::skill_install::SkillOutcome],
    dry_run: bool,
) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    if outcomes.is_empty() {
        writeln!(
            stdout,
            "No installed Power skills found in the selected locations."
        )?;
    }
    for outcome in outcomes {
        let path = outcome.path.display();
        if let Some(error) = &outcome.error {
            eprintln!("failed  {path} ({error})");
            continue;
        }
        let action = outcome.action.expect("successful locations have an action");
        let reason = outcome
            .reason
            .map(|reason| format!(" ({})", reason.label()))
            .unwrap_or_default();
        let backup = match &outcome.backup {
            Some(backup) => format!(" [backup at {}]", backup.display()),
            None if outcome.needs_backup => " [backup before replacement]".into(),
            None => String::new(),
        };
        if action == SkillAction::LeftAlone && !dry_run {
            eprintln!("warning: {}  {path}{reason}", action.label());
        } else {
            writeln!(stdout, "{}  {path}{reason}{backup}", action.label())?;
        }
    }
    Ok(())
}

fn print_skill(name: &str, selection: SkillSelection<'_>, json: bool) -> Result<()> {
    let output = GetSkill { name, selection }.execute(&EmbeddedSkillCatalog)?;
    if json {
        return print_json(match output {
            SkillOutput::Guide { markdown, full } => {
                json!({"name": name, "full": full, "markdown": markdown})
            }
            SkillOutput::Reference {
                name: reference,
                markdown,
            } => json!({"name": name, "reference": reference, "markdown": markdown}),
            SkillOutput::References(references) => json!({"name": name, "references": references}),
        });
    }
    let mut stdout = std::io::stdout().lock();
    match output {
        SkillOutput::Guide { markdown, .. } | SkillOutput::Reference { markdown, .. } => {
            stdout.write_all(markdown.as_bytes())?;
        }
        SkillOutput::References(references) => {
            for reference in references {
                writeln!(stdout, "{reference}")?;
            }
        }
    }
    Ok(())
}

fn dag_reply(request: DagRequest, timeout: Duration) -> Result<()> {
    let with_link = matches!(
        request,
        DagRequest::CreateRun { .. } | DagRequest::GetRun { .. }
    );
    let response = match client::dag(request, timeout)? {
        DagResponse::Agent { agent } => json!({"agent": agent}),
        DagResponse::Run { run } if with_link => {
            json!({"link": domain::link::dag_uri(&run.id), "run": run})
        }
        DagResponse::Run { run } => json!({"run": run}),
        DagResponse::Agents { agents } => json!({"agents": agents}),
        DagResponse::Runs { runs } => {
            let links: serde_json::Map<String, Value> = runs
                .iter()
                .map(|run| (run.id.clone(), json!(domain::link::dag_uri(&run.id))))
                .collect();
            json!({"runs": runs, "links": links})
        }
        DagResponse::Dispatched {
            attempt,
            agent,
            branch,
        } => json!({
            "worktree": attempt.worktree,
            "branch": branch,
            "attempt": attempt,
            "agent": agent,
        }),
        DagResponse::Queued { run, task, reason } => json!({
            "queued": true,
            "run": run,
            "task": task,
            "reason": reason,
        }),
        DagResponse::Reported { applied, reason } => json!({"applied": applied, "reason": reason}),
        DagResponse::Events { events, cursor } => json!({"events": events, "cursor": cursor}),
        _ => anyhow::bail!("Unexpected daemon response"),
    };
    print_json(response)
}

fn report_arg(value: Option<String>, env: &str, flag: &str) -> Result<String> {
    value
        .or_else(|| std::env::var(env).ok().filter(|value| !value.is_empty()))
        .with_context(|| format!("Specify --{flag} or set {env}"))
}

const READY_BUDGET_NOTE: &str = "Dispatch every ready task. Past the worker budget \
     (dag.max_active_workers and dag.min_available_memory in config.lua), Power queues the task \
     and starts it when a worker reports, so don't hold tasks back or retry queued ones.";

fn dispatch_request(launch: DispatchArgs, retry: bool) -> Result<DagRequest> {
    let allow_main_checkout = launch.worktree.allow_main_checkout;
    let worktree = launch.worktree.request(retry, "dispatching a task")?;
    Ok(DagRequest::Dispatch {
        run: launch.run,
        task: launch.task,
        provider: launch.provider.into(),
        worktree,
        allow_main_checkout,
        retry,
        args: launch.args,
        caller: caller_session(),
        budget: if launch.over_budget {
            BudgetMode::Override
        } else if launch.no_queue {
            BudgetMode::Refuse
        } else {
            BudgetMode::Queue
        },
    })
}

fn worktree_scope(args: WorktreeScopeArgs) -> Result<client::WorktreeScope> {
    use client::WorktreeScope;
    if args.all {
        return Ok(WorktreeScope::All);
    }
    if let Some(selector) = args.box_sel {
        return Ok(WorktreeScope::Box { selector });
    }
    let session = caller_session()
        .context("Select a box with --box, pass --all, or run from a Power terminal")?;
    if let Ok(selector) = std::env::var("POWER_BOX")
        && !selector.is_empty()
    {
        return Ok(WorktreeScope::Box { selector });
    }
    Ok(WorktreeScope::View { session })
}

fn run_worktree(command: WorktreeAction) -> Result<()> {
    use client::{KeepReason, PruneVerdict};

    match command {
        WorktreeAction::List(scope) => {
            let request = DagRequest::ListManagedWorktrees {
                scope: worktree_scope(scope)?,
            };
            match client::dag(request, PROTOCOL_TIMEOUT)? {
                DagResponse::ManagedWorktrees { worktrees } => {
                    print_json(json!({"worktrees": worktrees}))
                }
                _ => anyhow::bail!("Unexpected daemon response"),
            }
        }
        WorktreeAction::Prune {
            scope,
            dry_run,
            delete_branch,
        } => {
            let entries = client::prune_worktrees(worktree_scope(scope)?, dry_run, delete_branch)?;
            let mut stdout = std::io::stdout().lock();
            for (worktree, verdict) in entries {
                let path = worktree.path.display();
                let branch = worktree.branch;
                match verdict {
                    PruneVerdict::Remove => {
                        let action = if dry_run { "Would remove" } else { "Removed" };
                        writeln!(stdout, "{action} {path} ({branch})")?;
                    }
                    PruneVerdict::Keep(reason) => {
                        let reason = match reason {
                            KeepReason::Dirty => "uncommitted or untracked changes",
                            KeepReason::Unmerged => "branch has not merged into the default branch",
                            KeepReason::OpenAttempt => {
                                "an attempt there is still active or waiting for input"
                            }
                            KeepReason::InUse => "in use by a live session",
                            KeepReason::NoLongerManaged => "checkout no longer managed by Power",
                        };
                        writeln!(stdout, "Kept {path} ({branch}) — {reason}")?;
                    }
                }
            }
            Ok(())
        }
    }
}

fn run_dag(command: DagAction) -> Result<()> {
    let timeout = Duration::from_secs(10);
    let request = match command {
        DagAction::Guide => unreachable!("The guide is printed without the daemon"),
        DagAction::Create {
            title,
            box_sel,
            coordinator,
        } => DagRequest::CreateRun {
            box_sel,
            caller: caller_session(),
            title,
            coordinator,
        },
        DagAction::List { box_sel, all } => DagRequest::ListRuns {
            box_sel,
            caller: caller_session(),
            include_finished: all,
        },
        DagAction::Show { run } => DagRequest::GetRun { run },
        DagAction::Archive { run, force } => DagRequest::ArchiveRun { run, force },
        DagAction::Unarchive { run } => DagRequest::UnarchiveRun { run },
        DagAction::Pause { run } => DagRequest::PauseRun { run },
        DagAction::Resume { run } => DagRequest::ResumeRun { run },
        DagAction::AddTask {
            run,
            id,
            title,
            instructions,
            instructions_file,
            deps,
        } => {
            let instructions = match instructions_file {
                Some(file) => std::fs::read_to_string(&file)
                    .with_context(|| format!("Read instructions from {}", file.display()))?,
                None => instructions.unwrap_or_default(),
            };
            DagRequest::AddTasks {
                run,
                tasks: vec![TaskSpec {
                    id,
                    title,
                    instructions,
                    deps,
                }],
            }
        }
        DagAction::Apply { run, file } => {
            let bytes = std::fs::read(&file)
                .with_context(|| format!("Read tasks from {}", file.display()))?;
            let value = match serde_json::from_slice(&bytes)? {
                Value::Object(mut envelope) => {
                    envelope.remove("tasks").context("Missing tasks array")?
                }
                value => value,
            };
            DagRequest::AddTasks {
                run,
                tasks: serde_json::from_value(value)?,
            }
        }
        DagAction::AddDep { run, task, dep } => DagRequest::AddDependency { run, task, dep },
        DagAction::Ready { run } => {
            let (tasks, queued): (Vec<_>, Vec<_>) = protocol_run(run)?
                .tasks
                .into_iter()
                .filter(|task| matches!(task.state, TaskState::Ready | TaskState::Queued))
                .partition(|task| task.state == TaskState::Ready);
            return print_json(json!({
                "tasks": tasks,
                "queued": queued.into_iter().map(|task| task.id).collect::<Vec<_>>(),
                "budget": READY_BUDGET_NOTE,
            }));
        }
        DagAction::Dispatch { launch, retry } => dispatch_request(launch, retry)?,
        DagAction::Retry(launch) => dispatch_request(launch, true)?,
        DagAction::Report {
            status,
            summary,
            run,
            task,
            attempt,
        } => {
            report_conversation();
            DagRequest::Report {
                run: report_arg(run, "POWER_RUN", "run")?,
                task: report_arg(task, "POWER_TASK", "task")?,
                attempt: report_arg(attempt, "POWER_ATTEMPT", "attempt")?,
                summary,
                status: match status {
                    ReportChoice::Success => ReportStatus::Success,
                    ReportChoice::Failed => ReportStatus::Failed,
                    ReportChoice::NeedsInput => ReportStatus::NeedsInput,
                    ReportChoice::Working => ReportStatus::Working,
                },
            }
        }
        DagAction::Events(EventArgs { run, since, kinds }) => DagRequest::Events {
            run,
            since,
            kinds,
            wait_ms: 0,
        },
        DagAction::Wait {
            events:
                EventArgs {
                    run,
                    since,
                    mut kinds,
                },
            timeout: seconds,
        } => {
            use RunEventKind::*;
            if kinds.is_empty() {
                kinds = vec![
                    AttemptSucceeded,
                    AttemptFailed,
                    NeedsInput,
                    InputResolved,
                    AttemptInterrupted,
                    AttemptResumed,
                    AttemptStalled,
                    TaskReady,
                    QueuedDispatchFailed,
                ];
            }
            return dag_reply(
                DagRequest::Events {
                    run,
                    since,
                    kinds,
                    wait_ms: seconds
                        .checked_mul(1000)
                        .context("Wait timeout is too large")?,
                },
                Duration::from_secs(seconds) + timeout,
            );
        }
    };
    dag_reply(request, timeout)
}

const PROTOCOL_TIMEOUT: Duration = Duration::from_secs(60);

fn protocol_boxes() -> Result<Vec<client::protocol::BoxInfo>> {
    match client::dag(DagRequest::ListBoxes, PROTOCOL_TIMEOUT)? {
        DagResponse::Boxes { boxes } => Ok(boxes),
        _ => anyhow::bail!("Unexpected daemon response"),
    }
}

/// Pauses or resumes each agent and prints them.
fn set_paused(ids: Vec<String>, paused: bool) -> Result<()> {
    let mut agents = Vec::new();
    for agent in ids {
        match client::dag(
            DagRequest::SetAgentPaused { agent, paused },
            Duration::from_secs(10),
        )? {
            DagResponse::Agent { agent } => agents.push(agent),
            _ => anyhow::bail!("Unexpected daemon response"),
        }
    }
    print_json(json!({"agents": agents}))
}

/// Never starts a daemon: with none running there is nothing to interrupt. On success it
/// prints the PID of the daemon it checked, the one a restart may end, or nothing when
/// none listens. It fails closed: a daemon that cannot be asked for its workers is not
/// cleared for a restart unless forced.
fn restart_check(force: bool, target_only: bool) -> Result<()> {
    use client::protocol::AgentStatus;
    use std::io::IsTerminal as _;
    if target_only {
        if let Some(pid) = client::daemon_pid()? {
            println!("{pid}");
        }
        return Ok(());
    }
    let target = match client::restart_target(RESTART_CHECK_TIMEOUT) {
        Ok(target) => target,
        // Nothing is known about the daemon, not even its PID, so there is nothing to
        // clear for a restart even when forced.
        Err(error) if force => {
            anyhow::bail!("{error:#}\nCould not identify the daemon to restart")
        }
        Err(error) => anyhow::bail!(
            "{error:#}\nRestart cancelled: the daemon may have workers mid-task. Set \
             POWER_FORCE_RESTART=1 (or pass --force) to restart anyway"
        ),
    };
    let Some((pid, agents)) = target else {
        return Ok(());
    };
    let agents = match agents {
        Ok(agents) => agents,
        Err(error) if force => {
            eprintln!("{error:#}\nRestarting anyway (--force).");
            println!("{pid}");
            return Ok(());
        }
        Err(error) => anyhow::bail!(
            "{error:#}\nRestart cancelled: it may have workers mid-task. Wait until it \
             answers, or set POWER_FORCE_RESTART=1 (or pass --force) to restart anyway"
        ),
    };
    let open: Vec<_> = agents
        .iter()
        .filter(|agent| {
            agent.run.is_some()
                && matches!(
                    agent.status,
                    AgentStatus::Working | AgentStatus::NeedsInput | AgentStatus::Paused
                )
        })
        .collect();
    let confirmed = || {
        println!("{pid}");
        Ok(())
    };
    if open.is_empty() {
        return confirmed();
    }
    eprintln!(
        "{} DAG worker{} mid-task on the daemon (pid {pid}). Restarting it kills every \
         command they run, such as builds and tests:",
        open.len(),
        if open.len() == 1 { " is" } else { "s are" }
    );
    for agent in &open {
        eprintln!(
            "  {} {}/{} ({}) {}",
            agent.id,
            agent.run.as_deref().unwrap_or("-"),
            agent.task.as_deref().unwrap_or("-"),
            serde_json::to_value(agent.status)?
                .as_str()
                .unwrap_or_default(),
            agent.label
        );
    }
    if force {
        eprintln!("Restarting anyway (--force).");
        return confirmed();
    }
    if std::io::stdin().is_terminal() {
        eprint!("Restart anyway? [y/N] ");
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if matches!(answer.trim(), "y" | "Y" | "yes") {
            return confirmed();
        }
    }
    anyhow::bail!(
        "Restart cancelled. Wait for these workers to report, or set POWER_FORCE_RESTART=1 \
         (or pass --force) to restart anyway"
    )
}

/// How long `restart-check` waits for the daemon to list its workers.
const RESTART_CHECK_TIMEOUT: Duration = Duration::from_secs(30);

fn protocol_agents(
    box_sel: Option<String>,
    include_finished: bool,
) -> Result<Vec<client::protocol::ManagedAgent>> {
    list_agents(box_sel, caller_session(), include_finished)
}

fn list_agents(
    box_sel: Option<String>,
    caller: Option<String>,
    include_finished: bool,
) -> Result<Vec<client::protocol::ManagedAgent>> {
    let request = DagRequest::ListAgents {
        box_sel,
        caller,
        include_finished,
    };
    match client::dag(request, PROTOCOL_TIMEOUT)? {
        DagResponse::Agents { agents } => Ok(agents),
        _ => anyhow::bail!("Unexpected daemon response"),
    }
}

fn protocol_run(run: String) -> Result<client::protocol::Run> {
    match client::dag(DagRequest::GetRun { run }, PROTOCOL_TIMEOUT)? {
        DagResponse::Run { run } => Ok(run),
        _ => anyhow::bail!("Unexpected daemon response"),
    }
}
