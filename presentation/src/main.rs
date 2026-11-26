mod gui;
mod keymap;
mod picker;
mod preview;
mod settings;
mod status_bar;
mod terminal_view;

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
    client::{
        self,
        protocol::{
            DagRequest, DagResponse, Provider, ReportStatus, RunEventKind, TaskSpec, TaskState,
        },
    },
    config, daemon,
    shell::{self, Shell},
    skill_install::{LocalSkillFilesystem, install_targets},
    skills::EmbeddedSkillCatalog,
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
        let dag_command = match command.as_deref().and_then(|arg| arg.to_str()) {
            Some("box" | "dag") => true,
            Some("agent") => !matches!(
                args.next().as_deref().and_then(|arg| arg.to_str()),
                Some("codex" | "claude")
            ),
            _ => false,
        };
        if error.use_stderr() && dag_command {
            let _ = error.print();
            std::process::exit(1);
        }
        error.exit()
    });
    match cli.command {
        Some(Action::Daemon) => daemon::serve(configured_agents()),
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
                SessionAction::Task { label } => client::set_task(
                    std::env::var("POWER_SESSION").context("Set a task from a Power terminal")?,
                    label,
                )?,
                SessionAction::Model { name, .. } => report_model(name)?,
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
            AgentAction::List { box_sel } => {
                client::ensure_daemon()?;
                print_json(json!({"agents": protocol_agents(box_sel)?}))
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
                        worktree: worktree.canonicalize().context("Open worktree")?,
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
    List {
        #[arg(long = "box")]
        box_sel: Option<String>,
    },
    Start {
        #[arg(long = "box")]
        box_sel: Option<String>,
        /// Coding agent profile ID.
        #[arg(long)]
        provider: String,
        #[arg(long)]
        worktree: PathBuf,
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
    List {
        #[arg(long = "box")]
        box_sel: Option<String>,
    },
    Show {
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
    /// Coding agent profile ID, or `command` to run an executable directly.
    #[arg(long)]
    provider: String,
    #[arg(long)]
    worktree: PathBuf,
    #[arg(last = true)]
    args: Vec<String>,
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
        DagResponse::Runs { runs } => {
            let links: serde_json::Map<String, Value> = runs
                .iter()
                .map(|run| (run.id.clone(), json!(domain::link::dag_uri(&run.id))))
                .collect();
            json!({"runs": runs, "links": links})
        }
        DagResponse::Dispatched { attempt, agent } => json!({"attempt": attempt, "agent": agent}),
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

fn dispatch_request(launch: DispatchArgs, retry: bool) -> Result<DagRequest> {
    Ok(DagRequest::Dispatch {
        run: launch.run,
        task: launch.task,
        provider: launch.provider.into(),
        worktree: launch.worktree.canonicalize().context("Open worktree")?,
        retry,
        args: launch.args,
        caller: caller_session(),
    })
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
        DagAction::List { box_sel } => DagRequest::ListRuns {
            box_sel,
            caller: caller_session(),
        },
        DagAction::Show { run } => DagRequest::GetRun { run },
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
            let tasks: Vec<_> = protocol_run(run)?
                .tasks
                .into_iter()
                .filter(|task| task.state == TaskState::Ready)
                .collect();
            return print_json(json!({"tasks": tasks}));
        }
        DagAction::Dispatch { launch, retry } => dispatch_request(launch, retry)?,
        DagAction::Retry(launch) => dispatch_request(launch, true)?,
        DagAction::Report {
            status,
            summary,
            run,
            task,
            attempt,
        } => DagRequest::Report {
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
        },
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
                    TaskReady,
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

const PROTOCOL_TIMEOUT: Duration = Duration::from_secs(10);

fn protocol_boxes() -> Result<Vec<client::protocol::BoxInfo>> {
    match client::dag(DagRequest::ListBoxes, PROTOCOL_TIMEOUT)? {
        DagResponse::Boxes { boxes } => Ok(boxes),
        _ => anyhow::bail!("Unexpected daemon response"),
    }
}

fn protocol_agents(box_sel: Option<String>) -> Result<Vec<client::protocol::ManagedAgent>> {
    let request = DagRequest::ListAgents {
        box_sel,
        caller: caller_session(),
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
