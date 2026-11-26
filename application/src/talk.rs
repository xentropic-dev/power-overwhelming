use anyhow::Result;
use domain::talk::{
    Activity, Directory, HookState, Message, MessageId, NoticePolicy, Recipient, Sender, Target,
    ThreadId,
};
use std::{sync::Arc, time::Instant};

pub trait TalkStore: Send + Sync {
    fn read<T>(&self, query: impl FnOnce(&dyn TalkReader) -> Result<T>) -> Result<T>;
    /// An error rolls back all of the operation's changes and events.
    fn write<T>(&self, operation: impl FnOnce(&dyn TalkTransaction) -> Result<T>) -> Result<T>;
    /// A change after the cursor, or a deadline, wakes a waiting reader.
    fn wait_for_change(&self, cursor: u64, deadline: Instant);
}

pub trait TalkReader {
    fn directory(&self) -> Result<Directory>;
    fn message(&self, id: &MessageId) -> Result<Option<Message>>;
    fn inbox(&self, recipient: &Recipient, box_id: Option<&str>, all: bool)
    -> Result<Vec<Message>>;
    fn pending_agents(&self) -> Result<Vec<String>>;
    fn thread(&self, id: &ThreadId) -> Result<Vec<Message>>;
    fn log(&self, filter: &LogFilter) -> Result<Vec<Message>>;
    fn recent_sends(&self, sender: &Sender, recipient: &Recipient, since: u64) -> Result<Vec<u64>>;
    fn cursor(&self) -> Result<u64>;
    fn events(&self, after: u64, box_id: Option<&str>) -> Result<Vec<TalkEvent>>;
}

pub trait TalkTransaction: TalkReader {
    fn insert(&self, message: &Message) -> Result<()>;
    fn mark(&self, messages: &[&Message], change: DeliveryChange, at: u64) -> Result<()>;
}

pub trait SessionActivity: Send + Sync {
    fn activity(&self, session: &str) -> Option<Activity>;
    fn report_turn(&self, session: &str, state: HookState, output_sets_busy: bool) -> Result<()>;
}

pub trait Notifier: Send + Sync {
    /// Returns false if activity changed before the notice could be typed.
    fn notify(&self, session: &str, expected: Activity, line: &str) -> Result<bool>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryChange {
    Notified,
    Reminded,
    Read,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TalkEventKind {
    Message,
    Read,
    Delivery,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TalkEvent {
    pub seq: u64,
    pub box_id: String,
    pub message: MessageId,
    pub kind: TalkEventKind,
}

#[derive(Default)]
pub struct LogFilter {
    pub box_id: Option<String>,
    pub agent: Option<String>,
    pub run: Option<String>,
    pub allowed_runs: Option<Vec<String>>,
    pub to_user: bool,
    pub limit: Option<usize>,
}

#[derive(Default)]
pub struct Caller {
    pub session: Option<String>,
    pub box_id: Option<String>,
}

pub struct SendMessage {
    pub caller: Caller,
    pub target: Target,
    pub run: Option<String>,
    pub body: String,
    pub urgent: bool,
}

pub struct Reply {
    pub caller: Caller,
    pub message: MessageId,
    pub body: String,
    pub urgent: bool,
}

pub struct Talk<S, A, N> {
    store: Arc<S>,
    activity: A,
    notifier: N,
    policy: NoticePolicy,
    clock: fn() -> u64,
    new_id: fn() -> String,
}

use anyhow::{Context, bail, ensure};
use domain::{
    dag::{AgentStatus, Coordinator, ManagedAgent},
    talk::{
        Address, AgentRef, Delivery, MAX_BODY_BYTES, NoticeDecision, RATE_WINDOW_MS, SenderContext,
        authorize, notice, rate_limit, resolve,
    },
};

struct Actor<'a> {
    agent: Option<&'a ManagedAgent>,
    box_id: Option<&'a str>,
    directory: &'a Directory,
}

impl<'a> Actor<'a> {
    fn new(caller: &'a Caller, directory: &'a Directory) -> Self {
        let agent = caller.session.as_deref().and_then(|session| {
            directory
                .agents
                .iter()
                .find(|agent| agent.session.as_deref() == Some(session))
        });
        Self {
            agent,
            box_id: agent
                .map(|agent| agent.box_id.as_str())
                .or(caller.box_id.as_deref()),
            directory,
        }
    }

    fn coordinated_runs(&self) -> impl Iterator<Item = &str> {
        self.directory
            .runs
            .iter()
            .filter_map(|run| match &run.coordinator {
                Coordinator::Agent { agent }
                    if self.agent.is_some_and(|sender| sender.id == *agent) =>
                {
                    Some(run.id.as_str())
                }
                _ => None,
            })
    }

    fn default_run(&self) -> Option<&str> {
        self.agent
            .and_then(|agent| agent.run.as_deref())
            .or_else(|| {
                let mut runs = self.coordinated_runs();
                let first = runs.next()?;
                runs.next().is_none().then_some(first)
            })
    }

    fn sender(&self, run: Option<&str>) -> Sender {
        let Some(agent) = self.agent else {
            return Sender::User;
        };
        let coordinator_of = run.filter(|id| self.coordinated_runs().any(|r| r == *id));
        let role = if let Some(task) = &agent.task {
            format!("worker on task {task}")
        } else if let Some(run) = coordinator_of {
            format!("coordinator of run {run}")
        } else {
            "agent".into()
        };
        Sender::Agent(AgentRef {
            id: agent.id.clone(),
            box_id: agent.box_id.clone(),
            provider: agent.provider.name().into(),
            role,
            run: agent
                .run
                .clone()
                .or_else(|| coordinator_of.map(str::to_owned)),
        })
    }

    fn recipient(&self) -> Recipient {
        self.agent.map(Recipient::agent).unwrap_or(Recipient::User)
    }

    fn may_view(&self, message: &Message) -> bool {
        self.agent.is_none_or(|agent| {
            message.involves_agent(&agent.id)
                || message
                    .run
                    .as_deref()
                    .is_some_and(|id| self.coordinated_runs().any(|run| run == id))
        })
    }
}

struct Outgoing {
    box_id: String,
    sender: Sender,
    recipient: Recipient,
    body: String,
    run: Option<String>,
    urgent: bool,
}

impl<S: TalkStore, A: SessionActivity, N: Notifier> Talk<S, A, N> {
    pub fn new(
        store: Arc<S>,
        activity: A,
        notifier: N,
        policy: NoticePolicy,
        clock: fn() -> u64,
        new_id: fn() -> String,
    ) -> Self {
        Self {
            store,
            activity,
            notifier,
            policy,
            clock,
            new_id,
        }
    }

    pub fn send(&self, command: SendMessage) -> Result<Vec<Message>> {
        validate_body(&command.body)?;
        self.store.write(|db| {
            let directory = db.directory()?;
            let actor = Actor::new(&command.caller, &directory);
            let run = command.run.as_deref().or_else(|| actor.default_run());
            let recipients = resolve(
                SenderContext {
                    agent: actor.agent,
                    box_id: actor.box_id,
                    run,
                },
                &command.target,
                &directory,
            )?;
            let broadcast = match &command.target {
                Target::Run(id) => Some((id.as_str(), (self.new_id)())),
                _ => None,
            };
            let explicit_run = broadcast
                .as_ref()
                .map(|(run, _)| *run)
                .or(command.run.as_deref());
            let mut messages = Vec::with_capacity(recipients.len());
            let now = (self.clock)();
            for recipient in recipients {
                let (box_id, recipient_run) = match &recipient {
                    Recipient::Agent { id, box_id } => (
                        box_id.as_str(),
                        directory
                            .agents
                            .iter()
                            .find(|agent| agent.id == *id)
                            .and_then(|agent| agent.run.as_deref()),
                    ),
                    Recipient::User => (
                        actor
                            .box_id
                            .context("Specify --box when sending to the user outside Power")?,
                        None,
                    ),
                };
                let run = explicit_run.or(recipient_run).or(run);
                if let Some(run) = run {
                    ensure!(
                        directory.run(run)?.box_id == box_id,
                        "Run {run:?} is in another box"
                    );
                }
                let outgoing = Outgoing {
                    box_id: box_id.into(),
                    sender: actor.sender(run),
                    recipient,
                    body: command.body.clone(),
                    run: run.map(str::to_owned),
                    urgent: command.urgent,
                };
                let mut message = self.build(db, outgoing, now)?;
                message.broadcast = broadcast.as_ref().map(|(_, id)| id.clone());
                db.insert(&message)?;
                messages.push(message);
            }
            Ok(messages)
        })
    }

    pub fn reply(&self, command: Reply) -> Result<Message> {
        validate_body(&command.body)?;
        self.store.write(|db| {
            let directory = db.directory()?;
            let actor = Actor::new(&command.caller, &directory);
            let parent = require_message(db, &command.message)?;
            let sender = actor.sender(parent.run.as_deref());
            let from_me = parent.sender == Sender::User && sender == Sender::User
                || matches!((&parent.sender, &sender), (Sender::Agent(from), Sender::Agent(me)) if from.id == me.id);
            ensure!(
                actor.agent.is_none() || from_me || parent.recipient.matches_sender(&sender),
                "You are not a participant in message {:?}", command.message.0,
            );
            let recipient = if from_me {
                parent.recipient.clone()
            } else {
                match &parent.sender {
                    Sender::Agent(agent) => Recipient::Agent { id: agent.id.clone(), box_id: agent.box_id.clone() },
                    Sender::User => Recipient::User,
                    Sender::System => bail!("Power messages do not accept replies"),
                }
            };
            if let Recipient::Agent { id, .. } = &recipient {
                resolve(SenderContext { agent: actor.agent, box_id: actor.box_id, run: None },
                    &Target::One(Address::Agent(id.clone())), &directory)?;
            }
            let mut message = self.build(db, Outgoing {
                box_id: parent.box_id, sender, recipient, body: command.body,
                run: parent.run, urgent: command.urgent,
            }, (self.clock)())?;
            message.thread = parent.thread;
            message.reply_to = Some(parent.id);
            db.insert(&message)?;
            Ok(message)
        })
    }

    fn build(&self, db: &dyn TalkReader, outgoing: Outgoing, now: u64) -> Result<Message> {
        authorize(&outgoing.sender, &outgoing.recipient)?;
        ensure!(
            !outgoing.urgent || outgoing.sender == Sender::User,
            "--urgent is only available to the user"
        );
        let recent = db.recent_sends(
            &outgoing.sender,
            &outgoing.recipient,
            now.saturating_sub(RATE_WINDOW_MS),
        )?;
        if let Some(retry) = rate_limit(&outgoing.sender, &recent, now) {
            bail!(
                "Talk rate limit reached for this recipient. Retry in {} seconds",
                retry.saturating_sub(now).div_ceil(1000)
            );
        }
        let id = (self.new_id)();
        Ok(Message {
            thread: ThreadId(id.clone()),
            id: MessageId(id),
            box_id: outgoing.box_id,
            sender: outgoing.sender,
            recipient: outgoing.recipient,
            body: outgoing.body,
            run: outgoing.run,
            urgent: outgoing.urgent,
            reply_to: None,
            broadcast: None,
            created_at: now,
            delivery: Delivery {
                queued_at: now,
                ..Delivery::default()
            },
        })
    }

    pub fn inbox(&self, caller: Caller, all: bool, peek: bool) -> Result<Vec<Message>> {
        let read = |db: &dyn TalkReader| {
            let directory = db.directory()?;
            let actor = Actor::new(&caller, &directory);
            db.inbox(&actor.recipient(), actor.box_id, all)
        };
        if peek {
            self.store.read(read)
        } else {
            self.store.write(|db| {
                let mut messages = read(db)?;
                self.mark_read(db, &mut messages, |_| true)?;
                Ok(messages)
            })
        }
    }

    pub fn show(&self, caller: Caller, message: MessageId, read: bool) -> Result<Vec<Message>> {
        let show = |db: &dyn TalkReader| {
            let directory = db.directory()?;
            let actor = Actor::new(&caller, &directory);
            let selected = require_message(db, &message)?;
            ensure!(
                actor.may_view(&selected),
                "You cannot read message {:?}",
                message.0
            );
            let mut messages = db.thread(&selected.thread)?;
            messages.retain(|message| actor.may_view(message));
            Ok((actor.recipient(), messages))
        };
        if read {
            self.store.write(|db| {
                let (recipient, mut messages) = show(db)?;
                self.mark_read(db, &mut messages, |message| message.recipient == recipient)?;
                Ok(messages)
            })
        } else {
            Ok(self.store.read(show)?.1)
        }
    }

    fn mark_read(
        &self,
        db: &dyn TalkTransaction,
        messages: &mut [Message],
        filter: impl Fn(&Message) -> bool,
    ) -> Result<()> {
        let now = (self.clock)();
        let mut unread: Vec<_> = messages
            .iter_mut()
            .filter(|message| message.delivery.read_at.is_none() && filter(message))
            .collect();
        let borrowed: Vec<_> = unread.iter().map(|message| &**message).collect();
        db.mark(&borrowed, DeliveryChange::Read, now)?;
        for message in &mut unread {
            message.delivery.read_at = Some(now);
        }
        Ok(())
    }

    pub fn wait_for_reply(
        &self,
        caller: Caller,
        threads: Vec<ThreadId>,
        after: Option<MessageId>,
        deadline: Instant,
    ) -> Result<Option<Message>> {
        ensure!(!threads.is_empty(), "Specify a talk thread");
        loop {
            let (reply, cursor) = self.store.read(|db| {
                let directory = db.directory()?;
                let actor = Actor::new(&caller, &directory);
                let recipient = actor.recipient();
                let sent = |message: &Message| recipient.matches_sender(&message.sender);
                let mut replies = Vec::new();
                for thread in &threads {
                    let messages = db.thread(thread)?;
                    let from = messages
                        .iter()
                        .position(|message| {
                            sent(message) && after.as_ref().is_none_or(|id| message.id == *id)
                        })
                        .with_context(|| {
                            format!("You have no sent message in thread {:?}", thread.0)
                        })?;
                    replies.extend(messages.into_iter().skip(from + 1).find(|message| {
                        message.reply_to.is_some()
                            && message.recipient == recipient
                            && !sent(message)
                    }));
                }
                Ok((
                    replies.into_iter().min_by_key(|message| message.created_at),
                    db.cursor()?,
                ))
            })?;
            if reply.is_some() || Instant::now() >= deadline {
                return Ok(reply);
            }
            self.store.wait_for_change(cursor, deadline);
        }
    }

    pub fn log(&self, caller: Caller, mut filter: LogFilter) -> Result<Vec<Message>> {
        self.store.read(|db| {
            let directory = db.directory()?;
            let actor = Actor::new(&caller, &directory);
            if actor.agent.is_some() {
                let runs: Vec<_> = actor.coordinated_runs().map(str::to_owned).collect();
                ensure!(
                    !runs.is_empty(),
                    "Talk log is available to coordinators and the user"
                );
                if let Some(run) = &filter.run {
                    ensure!(runs.contains(run), "You do not coordinate run {run:?}");
                }
                filter.allowed_runs = Some(runs);
            }
            let scoped_box = filter.box_id.as_deref().or(actor.box_id);
            if let Some(agent) = &mut filter.agent {
                let target = directory.agent(agent, scoped_box)?;
                if let Some(from) = actor.agent {
                    ensure!(
                        target.box_id == from.box_id,
                        "Talk target {agent:?} is in another box"
                    );
                }
                filter.box_id = Some(target.box_id.clone());
                *agent = target.id.clone();
            } else if let Some(run) = &filter.run {
                filter.box_id = Some(directory.run(run)?.box_id.clone());
            } else {
                filter.box_id = scoped_box.map(str::to_owned);
            }
            db.log(&filter)
        })
    }

    pub fn report_turn(
        &self,
        caller: Caller,
        state: HookState,
        output_sets_busy: bool,
    ) -> Result<()> {
        let session = caller
            .session
            .context("POWER_SESSION is required to report a turn")?;
        self.activity.report_turn(&session, state, output_sets_busy)
    }

    pub fn cursor(&self) -> Result<u64> {
        self.store.read(|db| db.cursor())
    }

    pub fn events(&self, caller: &Caller, after: u64, deadline: Instant) -> Result<Vec<TalkEvent>> {
        loop {
            let (events, cursor) = self.store.read(|db| {
                let directory = db.directory()?;
                let actor = Actor::new(caller, &directory);
                ensure!(actor.agent.is_none(), "Talk watch is available to the user");
                Ok((db.events(after, actor.box_id)?, db.cursor()?))
            })?;
            if !events.is_empty() || Instant::now() >= deadline {
                return Ok(events);
            }
            self.store.wait_for_change(cursor, deadline);
        }
    }

    pub fn deliver_pending(&self) -> Result<()> {
        let agents = self.store.read(|db| db.pending_agents())?;
        let mut first_error = None;
        for id in agents {
            if let Err(error) = self.deliver(&id) {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn deliver(&self, id: &str) -> Result<()> {
        self.store.write(|db| {
            let directory = db.directory()?;
            let Some(agent) = directory
                .agents
                .iter()
                .find(|agent| agent.id == id && agent.status != AgentStatus::Exited)
            else {
                return Ok(());
            };
            let Some(session) = agent.session.as_deref() else {
                return Ok(());
            };
            let Some(activity) = self.activity.activity(session) else {
                return Ok(());
            };
            let messages = db.inbox(&Recipient::agent(agent), Some(&agent.box_id), false)?;
            let needs_input = agent.status == AgentStatus::NeedsInput;
            let now = (self.clock)();
            let mut notify = Vec::new();
            let mut remind = Vec::new();
            let mut included = Vec::new();
            for message in &messages {
                match self
                    .policy
                    .decide(activity, needs_input, activity.paused, message, now)
                {
                    NoticeDecision::Notify => notify.push(message),
                    NoticeDecision::Remind => remind.push(message),
                    NoticeDecision::Hold(_) => continue,
                }
                included.push(message);
            }
            if !included.is_empty()
                && self
                    .notifier
                    .notify(session, activity, &notice(&included))?
            {
                db.mark(&notify, DeliveryChange::Notified, now)?;
                db.mark(&remind, DeliveryChange::Reminded, now)?;
            }
            Ok(())
        })
    }
}

fn require_message(db: &dyn TalkReader, id: &MessageId) -> Result<Message> {
    db.message(id)?
        .with_context(|| format!("Unknown talk message {:?}", id.0))
}

fn validate_body(body: &str) -> Result<()> {
    ensure!(!body.trim().is_empty(), "Talk message body is empty");
    ensure!(
        body.len() <= MAX_BODY_BYTES,
        "Talk message body exceeds the 16 KiB ({MAX_BODY_BYTES} bytes) limit"
    );
    Ok(())
}

#[cfg(test)]
mod tests;
