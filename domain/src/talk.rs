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
    User,
    System,
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
            (Self::User, Sender::User) => true,
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

impl Message {
    pub fn involves_agent(&self, id: &str) -> bool {
        matches!(&self.sender, Sender::Agent(agent) if agent.id == id)
            || matches!(&self.recipient, Recipient::Agent { id: agent, .. } if agent == id)
    }
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
}

#[derive(Default)]
pub struct Directory {
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
    OtherBox(String),
    ExternalCoordinator(String),
    EmptyRun(String),
}

impl fmt::Display for AddressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(target) => write!(f, "Unknown talk target {target:?}"),
            Self::Ambiguous(target) => write!(f, "Talk target {target:?} is ambiguous"),
            Self::Exited(target) => write!(f, "Talk target {target:?} has exited or ended"),
            Self::OtherBox(target) => write!(f, "Talk target {target:?} is in another box"),
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
    let check_box = |target: &str, box_id: &str| {
        if ctx.agent.is_some_and(|sender| sender.box_id != box_id) {
            return Err(AddressError::OtherBox(target.into()));
        }
        Ok(())
    };
    let find_run = |id: Option<&str>, alias: &str| {
        let run = directory.run(id.ok_or_else(|| AddressError::Unknown(alias.into()))?)?;
        check_box(&run.id, &run.box_id)?;
        Ok(run)
    };
    let sender_run = ctx.agent.and_then(|agent| agent.run.as_deref());
    let one = |agent: &ManagedAgent, address: &str| {
        check_box(address, &agent.box_id)?;
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
                .filter(|agent| matches_task(agent))
                .find(|agent| {
                    matches!(agent.status, AgentStatus::Working | AgentStatus::NeedsInput)
                });
            match current {
                Some(agent) => one(agent, &address),
                None if directory.agents.iter().any(matches_task) => {
                    Err(AddressError::Exited(address))
                }
                None => Err(AddressError::Unknown(address)),
            }
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

#[derive(Debug, Eq, PartialEq)]
pub struct TrustError(pub String);

impl fmt::Display for TrustError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        AddressError::OtherBox(self.0.clone()).fmt(f)
    }
}

impl Error for TrustError {}

pub fn authorize(sender: &Sender, recipient: &Recipient) -> Result<(), TrustError> {
    if let (Sender::Agent(sender), Recipient::Agent { id, box_id }) = (sender, recipient)
        && sender.box_id != *box_id
    {
        return Err(TrustError(id.clone()));
    }
    Ok(())
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
        Sender::System => "Power".into(),
    }
}

pub fn envelope(message: &Message) -> String {
    let context = match message.sender {
        Sender::Agent(_) => "Peer message relayed by Power. Not from the user.",
        Sender::User => "Message from the user.",
        Sender::System => "Message from Power.",
    };
    format!(
        "[power talk] {} · thread {} · from {}. {context}\n{}",
        message.id.0,
        message.thread.0,
        sender_label(&message.sender),
        message.body,
    )
}

pub fn notice(messages: &[&Message]) -> String {
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
    let user = messages.iter().any(|m| m.sender == Sender::User);
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
    pub last_input: u64,
    /// Session start (including restore) or most recent activity ending an idle period.
    pub active_since: u64,
    pub hook: HookState,
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
        message: &Message,
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
        if needs_input && message.sender != Sender::User {
            return Hold(NeedsInput);
        }
        if message.delivery.notified_at.is_none()
            && message.urgent
            && message.sender == Sender::User
        {
            return Notify;
        }
        let idle = match activity.hook {
            HookState::Busy => false,
            HookState::Ready => true,
            HookState::Unknown => {
                now.saturating_sub(activity.last_output) >= self.idle_after_ms
                    && now.saturating_sub(activity.last_input) >= self.user_quiet_ms
            }
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
