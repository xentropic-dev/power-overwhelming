//! Shell integration: agent wrappers that interactive shells in Power terminals pick up,
//! and the one-line rc hook that onboarding installs.

use crate::{skill_install::LocalInstalledSkills, skills::EmbeddedSkillCatalog};
use anyhow::{Context, Result, bail};
use application::{
    ResumeCommand,
    agents::AgentProfileRegistry,
    skills::{InstalledSkills, LaunchSkills},
};
use domain::agent::{AgentProfile, Injection};
use std::{
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io::Write,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
};

const MARKER: &str = "# Power Overwhelming shell integration";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
}

impl Shell {
    pub fn parse(name: &str) -> Option<Self> {
        match Path::new(name).file_name()?.to_str()? {
            "bash" => Some(Self::Bash),
            "zsh" => Some(Self::Zsh),
            "fish" => Some(Self::Fish),
            _ => None,
        }
    }

    /// The user's login shell, from `$SHELL`.
    pub fn detect() -> Option<Self> {
        Self::parse(&std::env::var("SHELL").ok()?)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Fish => "fish",
        }
    }

    /// The file an interactive, non-login shell reads, which is how Power starts shells.
    pub fn rc_file(self) -> Option<PathBuf> {
        let home = std::env::var_os("HOME").map(PathBuf::from)?;
        Some(match self {
            Self::Bash => home.join(".bashrc"),
            Self::Zsh => std::env::var_os("ZDOTDIR")
                .map(PathBuf::from)
                .unwrap_or(home)
                .join(".zshrc"),
            Self::Fish => std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config"))
                .join("fish/config.fish"),
        })
    }

    /// The hook line onboarding appends. It does nothing outside Power terminals.
    pub fn hook(self) -> &'static str {
        match self {
            Self::Bash => r#"[ -n "$POWER_BIN" ] && eval "$("$POWER_BIN" shell-init bash)""#,
            Self::Zsh => r#"[ -n "$POWER_BIN" ] && eval "$("$POWER_BIN" shell-init zsh)""#,
            Self::Fish => r#"test -n "$POWER_BIN"; and "$POWER_BIN" shell-init fish | source"#,
        }
    }

    /// Defines the agent wrappers and runs any pending resume command.
    pub fn init_script(self, registry: &impl AgentProfileRegistry) -> String {
        let mut script = String::new();
        for profile in registry.profiles() {
            let name = profile.id.as_str();
            script.push_str(&match self {
                Self::Bash | Self::Zsh => {
                    // `function name` keeps a user's alias from expanding in the definition.
                    format!("function {name} {{ \"$POWER_BIN\" exec-agent {name} -- \"$@\"; }}\n")
                }
                Self::Fish => format!(
                    "function {name} --wraps {name}\n    \"$POWER_BIN\" exec-agent {name} -- $argv\nend\n"
                ),
            });
        }
        script.push_str(match self {
            Self::Bash | Self::Zsh => {
                r#"if [ -n "${POWER_RESUME-}" ]; then
    function __power_resume_fallback {
        [ $((SECONDS - __power_resume_started)) -lt 5 ] || return 1
        printf '\033[2m── could not resume, started a new conversation ──\033[0m\n'
        "$@"
    }
    function __power_resume_once {
        local power_resume_command="$POWER_RESUME" __power_resume_started=$SECONDS
        unset -f __power_resume_once
        unset POWER_RESUME && eval "$power_resume_command"
        local power_resume_status=$?
        unset -f __power_resume_fallback
        return $power_resume_status
    }
    __power_resume_once
fi
"#
            }
            Self::Fish => {
                r#"if test -n "$POWER_RESUME"
    function __power_resume_fallback
        test (math (date +%s) - $__power_resume_started) -lt 5; or return 1
        printf '\033[2m── could not resume, started a new conversation ──\033[0m\n'
        eval (string escape -- $argv)
    end
    set -l power_resume_command "$POWER_RESUME"
    set -g __power_resume_started (date +%s)
    set -e POWER_RESUME; and eval "$power_resume_command"
    functions -e __power_resume_fallback
    set -e __power_resume_started
end
"#
            }
        });
        script
    }

    pub fn installed(self) -> bool {
        self.rc_file().is_some_and(|path| has_marker(&path))
    }

    /// Appends the hook to the rc file once and returns the file's path.
    pub fn install(self) -> Result<PathBuf> {
        let path = self
            .rc_file()
            .context("Set HOME to install shell integration")?;
        self.install_at(&path)?;
        Ok(path)
    }

    fn install_at(self, path: &Path) -> Result<()> {
        if has_marker(path) {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let existing = fs::read_to_string(path).unwrap_or_default();
        let separator = if existing.is_empty() || existing.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("Open {}", path.display()))?;
        write!(file, "{separator}\n{MARKER}\n{}\n", self.hook())?;
        Ok(())
    }
}

fn has_marker(path: &Path) -> bool {
    fs::read_to_string(path).is_ok_and(|contents| contents.contains(MARKER))
}

/// Shell function the init script defines for [`resume_command`]'s fallback. It prints
/// a notice and runs its arguments only if the resume exited within 5 seconds.
const RESUME_FALLBACK: &str = "__power_resume_fallback";

/// Builds the `POWER_RESUME` command for Bash, Zsh, or Fish. With a fallback it reads
/// `<resume> || __power_resume_fallback <fresh>`. Claude exits 1 when it finds no
/// conversation, but a resumed agent the user quits later may also exit non-zero, so
/// the fallback also requires the resume to have exited within 5 seconds. Both calls
/// go through the agent's wrapper function, so the fresh one reports its conversation.
pub(crate) fn resume_command(resume: &ResumeCommand) -> OsString {
    let mut command = quote_words(&resume.program, &resume.args);
    if let Some(fallback) = &resume.fallback {
        command.push(" || ");
        command.push(RESUME_FALLBACK);
        command.push(" ");
        command.push(quote_words(&resume.program, fallback));
    }
    command
}

/// Quotes a program and its arguments for evaluation by Bash, Zsh, or Fish.
fn quote_words(program: &str, args: &[OsString]) -> OsString {
    let mut command = Vec::new();
    for word in std::iter::once(OsStr::new(program)).chain(args.iter().map(OsString::as_os_str)) {
        if !command.is_empty() {
            command.push(b' ');
        }
        command.push(b'\'');
        for &byte in word.as_bytes() {
            // Both characters can be escaped outside single quotes in all three shells.
            if matches!(byte, b'\'' | b'\\') {
                command.extend_from_slice(&[b'\'', b'\\', byte, b'\'']);
            } else {
                command.push(byte);
            }
        }
        command.push(b'\'');
    }
    OsString::from_vec(command)
}

/// Arguments that pin a new conversation ID, and the ID to record for a restore.
/// Management subcommands and help or version queries start no conversation, so
/// they get no ID and cannot replace the one a restore would resume.
pub(crate) fn assign_conversation(
    profile: &AgentProfile,
    user: &[OsString],
) -> (Vec<OsString>, Option<String>) {
    let Some(resume) = &profile.resume else {
        return (Vec::new(), None);
    };
    let skipped = user
        .first()
        .and_then(|arg| arg.to_str())
        .is_some_and(|arg| profile.skip_subcommands.iter().any(|skip| skip == arg))
        || user.iter().any(|arg| {
            matches!(
                arg.to_str(),
                Some("-h" | "--help" | "-v" | "-V" | "--version")
            )
        });
    if skipped {
        return (Vec::new(), None);
    }
    resume.launch(user, || uuid::Uuid::new_v4().to_string())
}

/// Arguments Power adds in front of the user's arguments when it starts an agent.
/// Skills the agent already discovers natively are left out of the list.
pub(crate) fn instruction_args(
    profile: &AgentProfile,
    user: &[OsString],
    installed: &dyn InstalledSkills,
) -> Result<Vec<OsString>> {
    if profile.injection == Injection::None
        || user
            .first()
            .and_then(|arg| arg.to_str())
            .is_some_and(|arg| profile.skip_subcommands.iter().any(|skip| skip == arg))
        || user.iter().any(|arg| {
            let arg = arg.to_string_lossy();
            profile
                .skip_if_args_contain
                .iter()
                .any(|skip| arg.contains(skip))
        })
    {
        return Ok(Vec::new());
    }

    let mut instructions = String::from("You are running inside Power Overwhelming.");
    let skills = LaunchSkills::execute(&EmbeddedSkillCatalog, installed, profile)?;
    if !skills.is_empty() {
        instructions.push_str(" Power skills, load with `power skills get <name>` when relevant:");
    }
    instructions.push('\n');
    for skill in skills {
        instructions.push_str(&format!("- {}: {}\n", skill.name, skill.description));
    }
    instructions.push_str(concat!(
        "\nWhen you understand the user's objective, run ",
        "`power session task 'Short task label'` from this terminal. ",
        "Use a short, concrete label and update it when the objective changes.\n\n",
        "When you can verify the active model for this session, run ",
        "`power session model 'Verified model name'` from this terminal at the start of your work ",
        "and again when the model changes. Leave it unset if the model is unknown.\n\n",
        "To give this box a custom icon, run `power box icon <image-path>`.",
    ));
    Ok(match &profile.injection {
        Injection::Flag { flag } => vec![flag.into(), instructions.into()],
        Injection::ConfigValue { flag, key } => vec![
            flag.into(),
            format!("{key}={}", serde_json::to_string(&instructions)?).into(),
        ],
        Injection::None => Vec::new(),
    })
}

/// Replaces this process with the real agent binary, adding Power's instructions.
pub fn exec_agent(profile: &AgentProfile, args: Vec<OsString>) -> Result<()> {
    let program = find_program(&profile.command)
        .with_context(|| format!("{}: command not found", profile.command))?;
    let mut command = std::process::Command::new(&program);
    command.args(instruction_args(
        profile,
        &args,
        &LocalInstalledSkills(&std::env::current_dir()?),
    )?);
    {
        let (assign, conversation) = assign_conversation(profile, &args);
        command.args(assign);
        if let (Some(conversation), Some(session)) =
            (conversation, std::env::var_os("POWER_SESSION"))
        {
            // The agent starts either way. Without the report it restarts fresh.
            let _ = crate::client::set_conversation(
                session.to_string_lossy().into_owned(),
                profile.id.as_str().to_owned(),
                conversation,
            );
        }
    }
    command.args(&args);
    let error = command.exec();
    bail!("Start {}. {error}", program.display())
}

/// Finds `name` on PATH, skipping the Power binary so a wrapper cannot call itself.
fn find_program(name: &str) -> Option<PathBuf> {
    let current = std::env::current_exe()
        .ok()
        .and_then(|path| path.canonicalize().ok());
    let is_program =
        |path: &Path| is_executable(path) && path.canonicalize().ok().as_ref() != current.as_ref();
    if name.contains('/') {
        let path = PathBuf::from(name);
        return is_program(&path).then_some(path);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|directory| directory.join(name))
        .find(|candidate| is_program(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Whether an agent's binary is on PATH, for onboarding to report.
pub fn agent_available(profile: &AgentProfile) -> bool {
    find_program(&profile.command).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::AgentProfiles;
    use application::skills::ListSkills;
    use std::{
        collections::BTreeSet, io::ErrorKind, os::unix::fs::PermissionsExt, process::Command,
    };

    struct Installed(BTreeSet<String>);

    impl InstalledSkills for Installed {
        fn installed(&self, _: &AgentProfile) -> Result<BTreeSet<String>> {
            Ok(self.0.clone())
        }
    }

    const NONE: Installed = Installed(BTreeSet::new());

    const TASK_LINE: &str = concat!(
        "When you understand the user's objective, run ",
        "`power session task 'Short task label'` from this terminal. ",
        "Use a short, concrete label and update it when the objective changes.",
    );

    const MODEL_LINE: &str = concat!(
        "When you can verify the active model for this session, run ",
        "`power session model 'Verified model name'` from this terminal at the start of your work ",
        "and again when the model changes. Leave it unset if the model is unknown.",
    );

    const ICON_LINE: &str = "To give this box a custom icon, run `power box icon <image-path>`.";

    fn launch_text(installed: &Installed) -> String {
        let profiles = AgentProfiles::default();
        let args = instruction_args(
            profiles.resolve(&"claude".into()).unwrap(),
            &["fix the bug".into()],
            installed,
        )
        .unwrap();
        args[1].to_str().unwrap().to_owned()
    }

    #[test]
    fn launch_instructions_omit_natively_installed_skills() {
        let text = launch_text(&Installed(BTreeSet::from(["power-dag".into()])));
        assert!(text.contains("- power-artifacts: "));
        assert!(!text.contains("- power-dag: "));

        let all = ListSkills::execute(&EmbeddedSkillCatalog).unwrap();
        let text = launch_text(&Installed(
            all.into_iter().map(|skill| skill.name).collect(),
        ));
        assert_eq!(
            text,
            format!(
                "You are running inside Power Overwhelming.\n\n{TASK_LINE}\n\n{MODEL_LINE}\n\n{ICON_LINE}"
            )
        );
    }

    #[test]
    fn skips_instructions_for_management_commands() {
        let profiles = AgentProfiles::default();
        assert!(
            instruction_args(
                profiles.resolve(&"claude".into()).unwrap(),
                &["mcp".into(), "list".into()],
                &NONE
            )
            .unwrap()
            .is_empty()
        );
        assert!(
            instruction_args(
                profiles.resolve(&"codex".into()).unwrap(),
                &["-c".into(), "developer_instructions=\"x\"".into()],
                &NONE
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn respects_no_injection_and_existing_flag_values() -> Result<()> {
        let profiles = AgentProfiles::default();
        let grok = profiles.resolve(&"grok".into())?;
        assert_eq!(instruction_args(grok, &[], &NONE)?[0], "--rules");
        assert!(instruction_args(grok, &["--rules=custom".into()], &NONE)?.is_empty());
        assert!(instruction_args(grok, &["mcp".into(), "list".into()], &NONE)?.is_empty());

        let mut profile = grok.clone();
        profile.injection = Injection::None;
        assert!(instruction_args(&profile, &["prompt".into()], &NONE)?.is_empty());
        Ok(())
    }

    #[test]
    fn launch_instructions_list_every_catalog_skill() {
        let profiles = AgentProfiles::default();
        let claude = instruction_args(
            profiles.resolve(&"claude".into()).unwrap(),
            &["fix the bug".into()],
            &NONE,
        )
        .unwrap();
        assert_eq!(claude[0], "--append-system-prompt");
        let instructions = claude[1].to_str().unwrap();
        let mut lines = instructions.lines();
        assert_eq!(
            lines.next(),
            Some(
                "You are running inside Power Overwhelming. Power skills, load with `power skills get <name>` when relevant:"
            )
        );
        for skill in ListSkills::execute(&EmbeddedSkillCatalog).unwrap() {
            let entry = format!("- {}: {}", skill.name, skill.description);
            assert_eq!(lines.next(), Some(entry.as_str()));
        }
        assert_eq!(lines.next(), Some(""));
        assert_eq!(lines.next(), Some(TASK_LINE));
        assert_eq!(lines.next(), Some(""));
        assert_eq!(lines.next(), Some(MODEL_LINE));
        assert_eq!(lines.next(), Some(""));
        assert_eq!(lines.next(), Some(ICON_LINE));
        assert_eq!(lines.next(), None);

        let codex = instruction_args(
            profiles.resolve(&"codex".into()).unwrap(),
            &["fix the bug".into()],
            &NONE,
        )
        .unwrap();
        assert_eq!(codex[0], "-c");
        let encoded = codex[1]
            .to_str()
            .unwrap()
            .strip_prefix("developer_instructions=")
            .unwrap();
        assert_eq!(
            serde_json::from_str::<String>(encoded).unwrap(),
            instructions
        );
    }

    #[test]
    fn installs_the_hook_once() {
        let home = tempfile::tempdir().unwrap();
        let rc = home.path().join(".bashrc");
        fs::write(&rc, "export A=1").unwrap();
        Shell::Bash.install_at(&rc).unwrap();
        Shell::Bash.install_at(&rc).unwrap();
        let contents = fs::read_to_string(&rc).unwrap();
        assert_eq!(contents.matches(MARKER).count(), 1);
        assert!(contents.starts_with("export A=1\n\n"));
        assert!(has_marker(&rc));
    }

    #[test]
    fn assigns_conversations_only_to_interactive_agent_launches() {
        let profiles = AgentProfiles::default();
        let claude = profiles.resolve(&"claude".into()).unwrap();
        let args = |args: &[&str]| args.iter().map(OsString::from).collect::<Vec<_>>();

        let (assign, conversation) = assign_conversation(claude, &args(&["--model", "opus"]));
        let conversation = conversation.expect("an interactive launch gets an ID");
        assert_eq!(assign, args(&["--session-id", &conversation]));

        for skipped in [&["mcp", "list"][..], &["update"], &["--help"], &["-v"]] {
            assert_eq!(
                assign_conversation(claude, &args(skipped)),
                (Vec::new(), None),
                "{skipped:?}"
            );
        }
        let codex = profiles.resolve(&"codex".into()).unwrap();
        assert_eq!(assign_conversation(codex, &[]), (Vec::new(), None));
    }

    #[test]
    fn quotes_resume_command_words() {
        let args = ["--resume", "two words", "a'b", r"a\b", ""].map(OsString::from);
        assert_eq!(
            quote_words("my agent", &args),
            r"'my agent' '--resume' 'two words' 'a'\''b' 'a'\\'b' ''"
        );
    }

    #[test]
    fn resume_command_falls_back_through_the_guard_function() {
        let mut resume = ResumeCommand {
            program: "claude".into(),
            args: ["--resume", "conv"].map(OsString::from).into(),
            fallback: None,
        };
        assert_eq!(resume_command(&resume), "'claude' '--resume' 'conv'");
        resume.fallback = Some(["--session-id", "conv"].map(OsString::from).into());
        assert_eq!(
            resume_command(&resume),
            "'claude' '--resume' 'conv' || __power_resume_fallback 'claude' '--session-id' 'conv'"
        );
        resume.fallback = Some(Vec::new());
        assert_eq!(
            resume_command(&resume),
            "'claude' '--resume' 'conv' || __power_resume_fallback 'claude'"
        );
    }

    #[test]
    fn init_script_appends_resume_trailer_after_wrappers() {
        let profiles = AgentProfiles::default();
        for (shell, guard, unset) in [
            (
                Shell::Bash,
                r#"if [ -n "${POWER_RESUME-}" ]"#,
                "unset POWER_RESUME",
            ),
            (
                Shell::Zsh,
                r#"if [ -n "${POWER_RESUME-}" ]"#,
                "unset POWER_RESUME",
            ),
            (
                Shell::Fish,
                r#"if test -n "$POWER_RESUME""#,
                "set -e POWER_RESUME",
            ),
        ] {
            let script = shell.init_script(&profiles);
            let (wrappers, trailer) = script.split_once(guard).unwrap();
            assert!(wrappers.contains("function claude"), "{shell:?}");
            let (_, after_unset) = trailer.split_once(unset).unwrap();
            assert!(
                after_unset.contains(r#"eval "$power_resume_command""#),
                "{shell:?}"
            );
        }
    }

    #[test]
    fn shell_hooks_run_resume_once() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let stub = temp.path().join("power stub");
        fs::write(
            &stub,
            r#"#!/bin/sh
if [ "$1" = shell-init ]; then
    exec cat "$POWER_TEST_INIT"
fi
test "${POWER_RESUME+x}" != x || exit 1
printf '<%s>\n' "$@"
"#,
        )?;
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o700))?;

        let profiles = AgentProfiles::default();
        let init = temp.path().join("init");
        let args = ["--resume", "two words", r#"a\'b"c"#, r"a\\b\", ""].map(OsString::from);
        let resume = resume_command(&ResumeCommand {
            program: "claude".into(),
            args: args.into(),
            fallback: Some(vec!["--fresh".into()]),
        });
        let expected =
            "<exec-agent>\n<claude>\n<-->\n<--resume>\n<two words>\n<a\\'b\"c>\n<a\\\\b\\>\n<>\n";
        for (shell, flags, is_unset) in [
            (
                Shell::Bash,
                &["--noprofile", "--norc"][..],
                r#"[ -z "${POWER_RESUME+x}" ]"#,
            ),
            (Shell::Zsh, &["-f"][..], r#"[ -z "${POWER_RESUME+x}" ]"#),
            (Shell::Fish, &["--no-config"][..], "not set -q POWER_RESUME"),
        ] {
            fs::write(&init, shell.init_script(&profiles))?;
            let hook = shell.hook();
            let output = Command::new(shell.name())
                .args(flags)
                .arg("-c")
                .arg(format!("{hook}\n{hook}\n{is_unset}"))
                .env_remove("BASH_ENV")
                .env("POWER_BIN", &stub)
                .env("POWER_TEST_INIT", &init)
                .env("POWER_RESUME", &resume)
                .output();
            let output = match output {
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                output => output?,
            };
            assert!(
                output.status.success(),
                "{shell:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(std::str::from_utf8(&output.stdout)?, expected, "{shell:?}");
        }
        Ok(())
    }

    #[test]
    fn shell_hooks_start_a_fresh_agent_when_the_resume_fails_at_once() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let stub = temp.path().join("power stub");
        // Like claude, the resume exits 1 at once when no conversation exists.
        fs::write(
            &stub,
            r#"#!/bin/sh
if [ "$1" = shell-init ]; then
    exec cat "$POWER_TEST_INIT"
fi
printf '<%s>\n' "$@"
test "$4" != --resume
"#,
        )?;
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o700))?;

        let profiles = AgentProfiles::default();
        let init = temp.path().join("init");
        let resume = resume_command(&ResumeCommand {
            program: "claude".into(),
            args: ["--resume", "conv-1"].map(OsString::from).into(),
            fallback: Some(["--session-id", "conv-1"].map(OsString::from).into()),
        });
        let expected = concat!(
            "<exec-agent>\n<claude>\n<-->\n<--resume>\n<conv-1>\n",
            "\x1b[2m── could not resume, started a new conversation ──\x1b[0m\n",
            "<exec-agent>\n<claude>\n<-->\n<--session-id>\n<conv-1>\n",
        );
        for (shell, flags, is_gone) in [
            (
                Shell::Bash,
                &["--noprofile", "--norc"][..],
                "! type __power_resume_fallback >/dev/null 2>&1",
            ),
            (
                Shell::Zsh,
                &["-f"][..],
                "! type __power_resume_fallback >/dev/null 2>&1",
            ),
            (
                Shell::Fish,
                &["--no-config"][..],
                "not functions -q __power_resume_fallback; and not set -q __power_resume_started",
            ),
        ] {
            fs::write(&init, shell.init_script(&profiles))?;
            let output = Command::new(shell.name())
                .args(flags)
                .arg("-c")
                .arg(format!("{}\n{is_gone}", shell.hook()))
                .env_remove("BASH_ENV")
                .env("POWER_BIN", &stub)
                .env("POWER_TEST_INIT", &init)
                .env("POWER_RESUME", &resume)
                .output();
            let output = match output {
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                output => output?,
            };
            assert!(
                output.status.success(),
                "{shell:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(std::str::from_utf8(&output.stdout)?, expected, "{shell:?}");
        }
        Ok(())
    }
}
