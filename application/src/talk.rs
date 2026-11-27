use anyhow::Result;
use domain::session::Agent;
use domain::talk::{
    Activity, AgentTalkSummary, Directory, HookState, Message, MessageId, NoticePolicy, Queued,
    Recipient, Sender, Target, ThreadId,
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
    fn summary(&self, box_id: &str) -> Result<Vec<AgentTalkSummary>>;
    fn pending_agents(&self, remind_before: u64) -> Result<Vec<String>>;
    /// Delivery metadata of the recipient's unread messages, in queue order, without bodies.
    fn queued(&self, recipient: &Recipient, box_id: &str) -> Result<Vec<Queued>>;
    fn agent(&self, id: &str) -> Result<ManagedAgent>;
    fn thread(&self, id: &ThreadId) -> Result<Vec<Message>>;
    fn log(&self, filter: &LogFilter) -> Result<Vec<Message>>;
    fn recent_sends(&self, sender: &Sender, recipient: &Recipient, since: u64) -> Result<Vec<u64>>;
    fn cursor(&self) -> Result<u64>;
    fn events(&self, after: u64, box_id: Option<&str>) -> Result<Vec<TalkEvent>>;
}

pub trait TalkTransaction: TalkReader {
    fn insert(&self, message: &Message) -> Result<()>;
    fn mark(&self, messages: &[&MessageId], change: DeliveryChange, at: u64) -> Result<()>;
}

pub trait SessionActivity: Send + Sync {
    /// None if the session is unknown or no longer running.
    fn activity(&self, session: &str) -> Option<Activity>;
    /// The agent the running session hosts at this moment (see `Application::live_agent`),
    /// resolved for the request, never from a cached foreground. None if the session is
    /// unknown, no longer running or cannot be resolved. Never call it under a store lock.
    fn agent(&self, session: &str) -> Option<Agent>;
    /// `ready_only` marks a provider that reports turn ends but not turn starts.
    fn report_turn(&self, session: &str, state: HookState, ready_only: bool) -> Result<()>;
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
    /// The request comes from the desktop rather than a shell. Only a caller without a
    /// session can be the desktop.
    pub desktop: bool,
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
    pub recipient_agent: Option<String>,
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
        Address, AgentRef, Delivery, MAX_BODY_BYTES, NoticeDecision, RATE_WINDOW_MS,
        REMINDER_AFTER_MS, RunRef, SenderContext, notice, rate_limit, resolve,
    },
};

struct Actor<'a> {
    agent: Option<&'a ManagedAgent>,
    box_id: Option<&'a str>,
    desktop: bool,
    directory: &'a Directory,
}

impl<'a> Actor<'a> {
    /// `live` is what the caller's session hosts, resolved for this request before any
    /// store lock (see `Talk::live`). None means it could not be resolved.
    fn new(caller: &'a Caller, directory: &'a Directory, live: Option<&Agent>) -> Result<Self> {
        let mut agent = None;
        if let Some(session) = caller.session.as_deref() {
            // Authority comes from the live session. A stale or mistyped POWER_SESSION, or a
            // session whose foreground cannot be resolved, never falls back to the user's.
            let Some(live) = live else {
                bail!(
                    "Unknown Power session {session:?}, or Power could not tell what it runs. \
                     Unset POWER_SESSION to send from this shell"
                );
            };
            agent = directory
                .agents
                .iter()
                .find(|agent| agent.session.as_deref() == Some(session) && agent.speaks_for(live));
            if agent.is_none()
                && let Agent::Coding(id) = live
            {
                bail!(
                    "Coding session {session:?} ({}) has no Power agent identity. Talk \
                     treats coding sessions as peers, never as the user",
                    id.as_str()
                );
            }
        }
        Ok(Self {
            agent,
            box_id: agent
                .map(|agent| agent.box_id.as_str())
                .or(caller.box_id.as_deref()),
            desktop: caller.desktop && caller.session.is_none(),
            directory,
        })
    }

    /// The run records the agent that created it and that agent's session. After a restart
    /// the same session may register again under a new agent ID and still coordinates it.
    fn coordinates(&self, coordinator: &str, run: &RunRef) -> bool {
        self.agent.is_some_and(|sender| {
            sender.id == coordinator
                || sender
                    .session
                    .as_deref()
                    .is_some_and(|session| run.coordinator_session.as_deref() == Some(session))
        })
    }

    fn coordinated_runs(&self) -> impl Iterator<Item = &str> {
        self.directory
            .runs
            .iter()
            .filter_map(|run| match &run.coordinator {
                Coordinator::Agent { agent } if self.coordinates(agent, run) => {
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
            return if self.desktop {
                Sender::User
            } else {
                Sender::Shell
            };
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

    fn actor<'a>(
        &self,
        caller: &'a Caller,
        directory: &'a Directory,
        live: Option<&Agent>,
    ) -> Result<Actor<'a>> {
        Actor::new(caller, directory, live)
    }

    /// Resolves what the caller's session runs now. Call it before taking a store lock.
    fn live(&self, caller: &Caller) -> Option<Agent> {
        caller
            .session
            .as_deref()
            .and_then(|session| self.activity.agent(session))
    }

    pub fn send(&self, command: SendMessage) -> Result<Vec<Message>> {
        validate_body(&command.body)?;
        let live = self.live(&command.caller);
        self.store.write(|db| {
            let directory = db.directory()?;
            let actor = self.actor(&command.caller, &directory, live.as_ref())?;
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
        let live = self.live(&command.caller);
        self.store.write(|db| {
            let directory = db.directory()?;
            let actor = self.actor(&command.caller, &directory, live.as_ref())?;
            let parent = require_message(db, &command.message)?;
            let sender = actor.sender(parent.run.as_deref());
            let from_me = parent.sender.has_user_authority() && sender.has_user_authority()
                || matches!((&parent.sender, &sender), (Sender::Agent(from), Sender::Agent(me)) if from.id == me.id);
            ensure!(
                actor.agent.is_none() || from_me || parent.recipient.matches_sender(&sender),
                "You are not a participant in message {:?}", command.message.0,
            );
            let recipient = if let Some(id) = &command.recipient_agent {
                ensure!(actor.agent.is_none() && parent.involves_agent(id),
                    "Only the user can direct a reply to a participant in this message");
                let agent = directory.agents.iter().find(|agent| agent.id == *id)
                    .with_context(|| format!("Unknown talk target {id:?}"))?;
                Recipient::agent(agent)
            } else if from_me {
                parent.recipient.clone()
            } else {
                match &parent.sender {
                    Sender::Agent(agent) => Recipient::Agent { id: agent.id.clone(), box_id: agent.box_id.clone() },
                    Sender::User | Sender::Shell => Recipient::User,
                    Sender::System => bail!("Power messages do not accept replies"),
                }
            };
            if let Recipient::Agent { id, .. } = &recipient {
                resolve(SenderContext { agent: actor.agent, box_id: actor.box_id, run: None },
                    &Target::One(Address::Agent(id.clone())), &directory)?;
            }
            let mut message = self.build(db, Outgoing {
                // A reply lives in its recipient's box, which differs across boxes.
                box_id: match &recipient {
                    Recipient::Agent { box_id, .. } => box_id.clone(),
                    Recipient::User => parent.box_id,
                },
                sender, recipient, body: command.body,
                run: parent.run, urgent: command.urgent,
            }, (self.clock)())?;
            message.thread = parent.thread;
            message.reply_to = Some(parent.id);
            db.insert(&message)?;
            Ok(message)
        })
    }

    fn build(&self, db: &dyn TalkReader, outgoing: Outgoing, now: u64) -> Result<Message> {
        ensure!(
            !outgoing.urgent || outgoing.sender.has_user_authority(),
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
        let live = self.live(&caller);
        let read = |db: &dyn TalkReader| {
            let directory = db.directory()?;
            let actor = self.actor(&caller, &directory, live.as_ref())?;
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
        let live = self.live(&caller);
        let show = |db: &dyn TalkReader| {
            let directory = db.directory()?;
            let actor = self.actor(&caller, &directory, live.as_ref())?;
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
        let ids: Vec<_> = unread.iter().map(|message| &message.id).collect();
        db.mark(&ids, DeliveryChange::Read, now)?;
        for message in &mut unread {
            message.delivery.read_at = Some(now);
        }
        Ok(())
    }

    /// With `reply_to`, only a reply to one of those messages ends the wait. Without it,
    /// any reply after the caller's last message in the threads does.
    pub fn wait_for_reply(
        &self,
        caller: Caller,
        threads: Vec<ThreadId>,
        reply_to: Vec<MessageId>,
        deadline: Instant,
    ) -> Result<Option<Message>> {
        ensure!(!threads.is_empty(), "Specify a talk thread");
        loop {
            let live = self.live(&caller);
            let (reply, cursor) = self.store.read(|db| {
                let directory = db.directory()?;
                let actor = self.actor(&caller, &directory, live.as_ref())?;
                let recipient = actor.recipient();
                let sent = |message: &Message| recipient.matches_sender(&message.sender);
                let mut replies = Vec::new();
                let mut found = 0;
                for thread in &threads {
                    let messages = db.thread(thread)?;
                    let from = if reply_to.is_empty() {
                        messages.iter().rposition(sent).with_context(|| {
                            format!("You have no sent message in thread {:?}", thread.0)
                        })?
                    } else {
                        found += messages
                            .iter()
                            .filter(|message| reply_to.contains(&message.id) && sent(message))
                            .count();
                        0
                    };
                    replies.extend(messages.into_iter().skip(from).find(|message| {
                        message.recipient == recipient
                            && !sent(message)
                            && message
                                .reply_to
                                .as_ref()
                                .is_some_and(|id| reply_to.is_empty() || reply_to.contains(id))
                    }));
                }
                ensure!(
                    found == reply_to.len(),
                    "Wait for replies to messages you sent in the given threads"
                );
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
        let live = self.live(&caller);
        self.store.read(|db| {
            let directory = db.directory()?;
            let actor = self.actor(&caller, &directory, live.as_ref())?;
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
                filter.box_id = Some(target.box_id.clone());
                *agent = target.id.clone();
            } else if let Some(run) = &filter.run {
                filter.box_id = Some(directory.run(run)?.box_id.clone());
            } else {
                filter.box_id = scoped_box.map(str::to_owned);
            }
            if actor.agent.is_some() {
                // A coordinator reads its runs' messages in every box they reach.
                filter.box_id = None;
            }
            db.log(&filter)
        })
    }

    pub fn summary(&self, caller: Caller) -> Result<Vec<AgentTalkSummary>> {
        let live = self.live(&caller);
        self.store.read(|db| {
            let directory = db.directory()?;
            let actor = self.actor(&caller, &directory, live.as_ref())?;
            ensure!(
                actor.agent.is_none(),
                "Talk summaries are available to the user"
            );
            db.summary(actor.box_id.context("Specify a box for the talk summary")?)
        })
    }

    pub fn report_turn(&self, caller: Caller, state: HookState, ready_only: bool) -> Result<()> {
        let session = caller
            .session
            .context("POWER_SESSION is required to report a turn")?;
        self.activity.report_turn(&session, state, ready_only)
    }

    pub fn cursor(&self) -> Result<u64> {
        self.store.read(|db| db.cursor())
    }

    pub fn events(&self, caller: &Caller, after: u64, deadline: Instant) -> Result<Vec<TalkEvent>> {
        loop {
            let live = self.live(caller);
            let (events, cursor) = self.store.read(|db| {
                let directory = db.directory()?;
                let actor = self.actor(caller, &directory, live.as_ref())?;
                ensure!(actor.agent.is_none(), "Talk watch is available to the user");
                Ok((db.events(after, actor.box_id)?, db.cursor()?))
            })?;
            if !events.is_empty() || Instant::now() >= deadline {
                return Ok(events);
            }
            self.store.wait_for_change(cursor, deadline);
        }
    }

    /// Holds read only the delivery metadata under a read. Only a deliverable batch takes
    /// a write transaction, which rechecks it before typing the notice and stamping it.
    pub fn deliver_pending(&self) -> Result<()> {
        let remind_before = (self.clock)().saturating_sub(REMINDER_AFTER_MS);
        let deliverable = self.store.read(|db| {
            let mut deliverable = Vec::new();
            for id in db.pending_agents(remind_before)? {
                if let Some(plan) = self.plan(db, &id, None)? {
                    deliverable.push((id, plan.session));
                }
            }
            Ok(deliverable)
        })?;
        let mut first_error = None;
        for (id, session) in deliverable {
            // Resolved outside the store lock, since the terminal may need the store.
            let live = self.activity.agent(&session);
            let result = self.store.write(|db| {
                let Some(plan) = self.plan(db, &id, Some((&session, live.as_ref())))? else {
                    return Ok(());
                };
                let included: Vec<_> = plan.notify.iter().chain(&plan.remind).collect();
                if self
                    .notifier
                    .notify(&plan.session, plan.activity, &notice(&included))?
                {
                    db.mark(&ids(&plan.notify), DeliveryChange::Notified, plan.now)?;
                    db.mark(&ids(&plan.remind), DeliveryChange::Reminded, plan.now)?;
                }
                Ok(())
            });
            if let Err(error) = result {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// With `live`, the plan also requires that the agent's session, as resolved for this
    /// delivery, still runs the agent. A notice for a coding agent that has left a shell's
    /// foreground is never typed into the shell.
    fn plan(
        &self,
        db: &dyn TalkReader,
        id: &str,
        live: Option<(&str, Option<&Agent>)>,
    ) -> Result<Option<Plan>> {
        let agent = db.agent(id)?;
        if agent.status == AgentStatus::Exited {
            return Ok(None);
        }
        let Some(session) = agent.session.clone() else {
            return Ok(None);
        };
        if let Some((resolved, live)) = live
            && (resolved != session || !live.is_some_and(|live| agent.speaks_for(live)))
        {
            return Ok(None);
        }
        let Some(activity) = self.activity.activity(&session) else {
            return Ok(None);
        };
        let needs_input = agent.status == AgentStatus::NeedsInput;
        let now = (self.clock)();
        let mut plan = Plan {
            session,
            activity,
            now,
            notify: Vec::new(),
            remind: Vec::new(),
        };
        for message in db.queued(&Recipient::agent(&agent), &agent.box_id)? {
            match self
                .policy
                .decide(activity, needs_input, activity.paused, &message, now)
            {
                NoticeDecision::Notify => plan.notify.push(message),
                NoticeDecision::Remind => plan.remind.push(message),
                NoticeDecision::Hold(_) => {}
            }
        }
        Ok((!plan.notify.is_empty() || !plan.remind.is_empty()).then_some(plan))
    }
}

struct Plan {
    session: String,
    activity: Activity,
    now: u64,
    notify: Vec<Queued>,
    remind: Vec<Queued>,
}

impl<S: TalkStore, A: SessionActivity, N: Notifier> crate::dag::TaskMessenger for Talk<S, A, N> {
    fn send_to_worker(
        &self,
        caller: Option<String>,
        run: &str,
        agent: &str,
        body: String,
    ) -> Result<String> {
        let messages = self.send(SendMessage {
            caller: Caller {
                session: caller,
                box_id: None,
                desktop: false,
            },
            target: Target::One(Address::Agent(agent.into())),
            run: Some(run.into()),
            body,
            urgent: false,
        })?;
        messages
            .into_iter()
            .next()
            .map(|message| message.id.0)
            .context("Talk queued no message for the worker")
    }
}

fn ids(batch: &[Queued]) -> Vec<&MessageId> {
    batch.iter().map(|message| &message.id).collect()
}

fn require_message(db: &dyn TalkReader, id: &MessageId) -> Result<Message> {
    db.message(id)?
        .with_context(|| format!("Unknown talk message {:?}", id.0))
}

fn validate_body(body: &str) -> Result<()> {
    ensure!(
        body.len() <= MAX_BODY_BYTES,
        "Talk message body exceeds the 16 KiB ({MAX_BODY_BYTES} bytes) limit"
    );
    Ok(())
}

#[cfg(test)]
mod tests;
