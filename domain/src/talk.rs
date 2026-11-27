use crate::dag::{AgentStatus, Coordinator, ManagedAgent};
use std::{error::Error, fmt};

pub const MAX_BODY_BYTES: usize = 16 * 1024;
pub const RATE_LIMIT: usize = 20;
pub const RATE_WINDOW_MS: u64 = 10 * 60 * 1000;
pub const REMINDER_AFTER_MS: u64 = 2 * 60 * 1000;

/// Talk IDs are opaque. Timestamps are Unix milliseconds.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct MessageId(pub String);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ThreadId(pub String);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentRef {
    pub id: String,
    pub box_id: String,
    pub provider: String,
    pub role: String,
    pub run: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Sender {
    Agent(AgentRef),
    /// The user, through the desktop.
    User,
    /// A process running as the user outside any agent session, such as a shell or script.
    /// It has the user's authority, but Power cannot confirm that the user wrote it.
    Shell,
    System,
}

impl Sender {
    pub fn has_user_authority(&self) -> bool {
        matches!(self, Self::User | Self::Shell)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Recipient {
    Agent { id: String, box_id: String },
    User,
}

impl Recipient {
    pub fn agent(agent: &ManagedAgent) -> Self {
        Self::Agent {
            id: agent.id.clone(),
            box_id: agent.box_id.clone(),
        }
    }

    pub fn matches_sender(&self, sender: &Sender) -> bool {
        match (self, sender) {
            (Self::User, sender) => sender.has_user_authority(),
            (Self::Agent { id, box_id }, Sender::Agent(agent)) => {
                id == &agent.id && box_id == &agent.box_id
            }
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Delivery {
    pub queued_at: u64,
    pub notified_at: Option<u64>,
    pub reminded_at: Option<u64>,
    pub read_at: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Message {
    pub id: MessageId,
    pub box_id: String,
    pub sender: Sender,
    pub recipient: Recipient,
    pub body: String,
    pub thread: ThreadId,
    pub reply_to: Option<MessageId>,
    pub broadcast: Option<String>,
    pub run: Option<String>,
    pub created_at: u64,
    pub delivery: Delivery,
    pub urgent: bool,
}

/// The delivery metadata of an unread message, without its body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Queued {
    pub id: MessageId,
    pub sender: Sender,
    pub recipient: Recipient,
    pub delivery: Delivery,
    pub urgent: bool,
}

impl Message {
    pub fn queued(&self) -> Queued {
        Queued {
            id: self.id.clone(),
            sender: self.sender.clone(),
            recipient: self.recipient.clone(),
            delivery: self.delivery.clone(),
            urgent: self.urgent,
        }
    }

    pub fn involves_agent(&self, id: &str) -> bool {
        matches!(&self.sender, Sender::Agent(agent) if agent.id == id)
            || matches!(&self.recipient, Recipient::Agent { id: agent, .. } if agent == id)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AgentTalkSummary {
    pub agent: String,
    pub unread: usize,
    pub to_user: usize,
    pub count: usize,
    pub latest: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Address {
    Agent(String),
    Parent,
    Coordinator,
    Task(String),
    User,
}

impl From<&str> for Address {
    fn from(value: &str) -> Self {
        match value {
            "parent" => Self::Parent,
            "coordinator" => Self::Coordinator,
            "user" => Self::User,
            _ => match value.strip_prefix("task:") {
                Some(task) => Self::Task(task.into()),
                None => Self::Agent(value.into()),
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Target {
    One(Address),
    Run(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunRef {
    pub id: String,
    pub box_id: String,
    pub coordinator: Coordinator,
    /// The session the coordinating agent held when it created the run, if any.
    pub coordinator_session: Option<String>,
}

#[derive(Default)]
pub struct Directory {
    /// Agents in creation order. The last attempt of each task is its current one.
    pub agents: Vec<ManagedAgent>,
    pub runs: Vec<RunRef>,
}

pub struct SenderContext<'a> {
    pub agent: Option<&'a ManagedAgent>,
    pub box_id: Option<&'a str>,
    pub run: Option<&'a str>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum AddressError {
    Unknown(String),
    Ambiguous(String),
    Exited(String),
    ExternalCoordinator(String),
    EmptyRun(String),
}

impl fmt::Display for AddressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(target) => write!(f, "Unknown talk target {target:?}"),
            Self::Ambiguous(target) => write!(f, "Talk target {target:?} is ambiguous"),
            Self::Exited(target) => write!(f, "Talk target {target:?} has exited or ended"),
            Self::ExternalCoordinator(label) => {
                write!(
                    f,
                    "Coordinator {label:?} is external and cannot receive Power messages"
                )
            }
            Self::EmptyRun(run) => write!(f, "Run {run:?} has no other active workers"),
        }
    }
}

impl Error for AddressError {}

impl Directory {
    pub fn run(&self, id: &str) -> Result<&RunRef, AddressError> {
        self.runs
            .iter()
            .find(|run| run.id == id)
            .ok_or_else(|| AddressError::Unknown(format!("run {id}")))
    }

    /// Exact IDs take precedence. Prefixes first look in the caller's box.
    pub fn agent(&self, value: &str, box_id: Option<&str>) -> Result<&ManagedAgent, AddressError> {
        if let Some(agent) = self.agents.iter().find(|agent| agent.id == value) {
            return Ok(agent);
        }
        if value.is_empty() {
            return Err(AddressError::Unknown(value.into()));
        }
        let matching = || {
            self.agents
                .iter()
                .filter(|agent| agent.id.starts_with(value))
        };
        let mut local = matching().filter(|agent| box_id == Some(agent.box_id.as_str()));
        let mut global = matching();
        let (first, second) = match local.next() {
            Some(agent) => (Some(agent), local.next()),
            None => (global.next(), global.next()),
        };
        if second.is_some() {
            return Err(AddressError::Ambiguous(value.into()));
        }
        first.ok_or_else(|| AddressError::Unknown(value.into()))
    }
}

pub fn resolve(
    ctx: SenderContext<'_>,
    target: &Target,
    directory: &Directory,
) -> Result<Vec<Recipient>, AddressError> {
    let find_run = |id: Option<&str>, alias: &str| {
        directory.run(id.ok_or_else(|| AddressError::Unknown(alias.into()))?)
    };
    let sender_run = ctx.agent.and_then(|agent| agent.run.as_deref());
    let one = |agent: &ManagedAgent, address: &str| {
        if agent.status == AgentStatus::Exited || agent.session.is_none() {
            return Err(AddressError::Exited(address.into()));
        }
        Ok(vec![Recipient::agent(agent)])
    };
    let exact = |id: &str, address: &str| {
        one(
            directory
                .agents
                .iter()
                .find(|agent| agent.id == id)
                .ok_or_else(|| AddressError::Unknown(address.into()))?,
            address,
        )
    };
    match target {
        Target::One(Address::User) => Ok(vec![Recipient::User]),
        Target::One(Address::Agent(id)) => {
            let box_id = ctx.agent.map(|agent| agent.box_id.as_str()).or(ctx.box_id);
            one(directory.agent(id, box_id)?, id)
        }
        Target::One(Address::Parent) => {
            let parent = ctx
                .agent
                .and_then(|agent| agent.parent.as_deref())
                .ok_or_else(|| AddressError::Unknown("parent".into()))?;
            exact(parent, "parent")
        }
        Target::One(Address::Coordinator) => {
            let run = find_run(sender_run.or(ctx.run), "coordinator")?;
            match &run.coordinator {
                Coordinator::Agent { agent } => exact(agent, "coordinator"),
                Coordinator::External { label } => {
                    Err(AddressError::ExternalCoordinator(label.clone()))
                }
            }
        }
        Target::One(Address::Task(task)) => {
            let address = format!("task:{task}");
            let run = find_run(ctx.run.or(sender_run), &address)?;
            let matches_task = |agent: &ManagedAgent| {
                agent.run.as_ref() == Some(&run.id) && agent.task.as_ref() == Some(task)
            };
            let current = directory
                .agents
                .iter()
                .rev()
                .find(|agent| matches_task(agent))
                .ok_or_else(|| AddressError::Unknown(address.clone()))?;
            one(current, &address)
        }
        Target::Run(id) => {
            let run = find_run(Some(id), id)?;
            let recipients: Vec<_> = directory
                .agents
                .iter()
                .filter(|agent| {
                    agent.run.as_ref() == Some(&run.id)
                        && agent.box_id == run.box_id
                        && matches!(agent.status, AgentStatus::Working | AgentStatus::NeedsInput)
                        && agent.session.is_some()
                        && ctx.agent.is_none_or(|sender| sender.id != agent.id)
                })
                .map(Recipient::agent)
                .collect();
            if recipients.is_empty() {
                return Err(AddressError::EmptyRun(id.clone()));
            }
            Ok(recipients)
        }
    }
}

/// Returns the first timestamp at which sending would be allowed.
pub fn rate_limit(sender: &Sender, recent: &[u64], now: u64) -> Option<u64> {
    if !matches!(sender, Sender::Agent(_)) {
        return None;
    }
    let mut window: Vec<_> = recent
        .iter()
        .copied()
        .filter(|sent| sent.saturating_add(RATE_WINDOW_MS) > now)
        .collect();
    let index = window.len().checked_sub(RATE_LIMIT)?;
    let (_, oldest, _) = window.select_nth_unstable(index);
    Some(oldest.saturating_add(RATE_WINDOW_MS))
}

fn sender_label(sender: &Sender) -> String {
    match sender {
        Sender::Agent(agent) => {
            // Provider IDs and labels are display data. Keep terminal notices on one line.
            format!(
                "{} ({}; {}; box {})",
                agent.id, agent.provider, agent.role, agent.box_id
            )
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect()
        }
        Sender::User => "the user".into(),
        Sender::Shell => "the user's shell".into(),
        Sender::System => "Power".into(),
    }
}

/// Renders terminal control characters (C0 except tab, DEL and C1) as escaped text.
pub fn escape_controls(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() && c != '\t' {
            escaped.extend(c.escape_default());
        } else {
            escaped.push(c);
        }
    }
    escaped
}

/// Prefix of every body line in an envelope. Lines without it come from Power.
pub const QUOTE: &str = "> ";

pub fn envelope(message: &Message) -> String {
    let context = match message.sender {
        Sender::Agent(_) => "Peer message relayed by Power. Not from the user.",
        Sender::User => "Message from the user.",
        Sender::Shell => {
            "Sent with the user's authority from a shell or script outside any agent session. \
             Power cannot confirm that the user wrote it."
        }
        Sender::System => "Message from Power.",
    };
    let mut framed = format!(
        "[power talk] {} · thread {} · from {}. {context} The body follows, each line quoted with {QUOTE:?}.",
        message.id.0,
        message.thread.0,
        sender_label(&message.sender),
    );
    let mut lines = message.body.lines().peekable();
    if lines.peek().is_none() {
        framed.push('\n');
        framed.push_str(QUOTE.trim_end());
    }
    for line in lines {
        framed.push('\n');
        framed.push_str(QUOTE);
        framed.push_str(&escape_controls(line));
    }
    framed
}

pub fn notice(messages: &[&Queued]) -> String {
    let mut senders = Vec::new();
    for message in messages {
        let label = sender_label(&message.sender);
        if !senders.contains(&label) {
            senders.push(label);
        }
    }
    let peer = messages
        .iter()
        .any(|m| matches!(m.sender, Sender::Agent(_)));
    let user = messages.iter().any(|m| m.sender.has_user_authority());
    let framing = match (peer, user) {
        (true, true) => "Includes peer messages not from the user. ",
        (true, false) => "Not from the user. ",
        _ => "",
    };
    let noun = if messages.len() == 1 {
        "message"
    } else {
        "messages"
    };
    let from = if senders.len() <= 3 {
        senders.join(", ")
    } else {
        format!(
            "{} and {} others",
            senders[..3].join(", "),
            senders.len() - 3
        )
    };
    format!(
        "[power talk] {} new {noun} from {from}. {framing}Run: power talk inbox",
        messages.len()
    )
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum HookState {
    #[default]
    Unknown,
    Busy,
    Ready,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Activity {
    pub last_output: u64,
    /// The last keystroke or paste from the user. It never changes the turn state.
    pub last_input: u64,
    pub last_notice: u64,
    /// Session start (including restore) or most recent activity ending an idle period.
    pub active_since: u64,
    /// The turn state the provider last reported. Output and input never change it.
    pub hook: HookState,
    /// When the provider last reported its turn state.
    pub reported_at: u64,
    /// The provider reports only turn ends (Codex `notify`), so output after a ready
    /// report may be a trailing repaint or a new turn. Delivery waits for output quiet.
    pub ready_only: bool,
    /// A known prompt submission (a notice or the initial prompt) started a turn on a
    /// notify-only provider that has not reported its end. Redraws and draft keystrokes
    /// never set it, and only the next turn-end report clears it.
    pub awaiting_turn_end: bool,
    pub restoring: bool,
    pub paused: bool,
    /// Changes with each input, output or hook report, even within one millisecond.
    pub revision: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct NoticePolicy {
    pub idle_after_ms: u64,
    pub user_quiet_ms: u64,
}

impl Default for NoticePolicy {
    fn default() -> Self {
        Self {
            idle_after_ms: 5_000,
            user_quiet_ms: 10_000,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NoticeDecision {
    Notify,
    Remind,
    Hold(HoldReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HoldReason {
    UserInbox,
    Read,
    Restoring,
    Paused,
    NeedsInput,
    Busy,
    AlreadyReminded,
    ReminderDelay,
    SameIdlePeriod,
}

impl NoticePolicy {
    pub fn decide(
        self,
        activity: Activity,
        needs_input: bool,
        paused: bool,
        message: &Queued,
        now: u64,
    ) -> NoticeDecision {
        use HoldReason::*;
        use NoticeDecision::*;
        if message.recipient == Recipient::User {
            return Hold(UserInbox);
        }
        if message.delivery.read_at.is_some() {
            return Hold(Read);
        }
        if activity.restoring {
            return Hold(Restoring);
        }
        if paused {
            return Hold(Paused);
        }
        let from_user = message.sender.has_user_authority();
        if needs_input && !from_user {
            return Hold(NeedsInput);
        }
        if message.delivery.notified_at.is_none() && message.urgent && from_user {
            return Notify;
        }
        let output_quiet = now.saturating_sub(activity.last_output.max(activity.last_notice))
            >= self.idle_after_ms;
        let user_quiet = now.saturating_sub(activity.last_input) >= self.user_quiet_ms;
        // A notify-only provider cannot report the turn a submission started, so quiet output
        // never proves it ended. One notice per turn: hold until the turn-end callback.
        let idle = !activity.awaiting_turn_end
            && match activity.hook {
                HookState::Busy => false,
                // Typing after the turn ended may be a draft or navigation. Wait for the user to
                // pause instead of treating the session as busy until the next report.
                HookState::Ready => {
                    (activity.last_input <= activity.reported_at || user_quiet)
                        && (!activity.ready_only || output_quiet)
                }
                HookState::Unknown => output_quiet && user_quiet,
            };
        if !idle {
            return Hold(Busy);
        }
        match message.delivery.notified_at {
            None => Notify,
            Some(_) if message.delivery.reminded_at.is_some() => Hold(AlreadyReminded),
            Some(at) if now.saturating_sub(at) < REMINDER_AFTER_MS => Hold(ReminderDelay),
            Some(at) if activity.active_since < at => Hold(SameIdlePeriod),
            Some(_) => Remind,
        }
    }
}

#[cfg(test)]
mod tests;
