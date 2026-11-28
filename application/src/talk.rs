use anyhow::Result;
use domain::session::Agent;
use domain::talk::{
    Activity, AgentTalkSummary, Decision, Directory, Grant, GrantRelay, GrantScope, HookState,
    Message, MessageId, NoticePolicy, Queued, Recipient, Sender, Target, ThreadId,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Instant,
};

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
    /// Decision requests to the user from every box, oldest first. Unless `all`, only those
    /// still open at `now`.
    fn decisions(&self, all: bool, now: u64) -> Result<Vec<Message>>;
    fn cursor(&self) -> Result<u64>;
    fn events(&self, after: u64, box_id: Option<&str>) -> Result<Vec<TalkEvent>>;
    /// Every grant, oldest first, optionally only an agent's.
    fn grants(&self, agent: Option<&str>) -> Result<Vec<Grant>>;
    /// The grant a decision request asks the user to approve.
    fn grant_for_request(&self, request: &MessageId) -> Result<Option<Grant>>;
}

pub trait TalkTransaction: TalkReader {
    fn insert(&self, message: &Message) -> Result<()>;
    fn mark(&self, messages: &[&MessageId], change: DeliveryChange, at: u64) -> Result<()>;
    /// Closes an open decision request. Returns false if it was already answered.
    fn answer(&self, request: &MessageId, at: u64) -> Result<bool>;
    fn insert_grant(&self, grant: &Grant) -> Result<()>;
    /// Starts a pending grant. Returns false if it was approved or revoked already.
    fn approve_grant(&self, id: &str, at: u64) -> Result<bool>;
    /// Returns false if it was revoked already.
    fn revoke_grant(&self, id: &str, at: u64) -> Result<bool>;
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
    /// Relays the user's decision in this scope, if the sender holds an active grant for it.
    pub scope: Option<GrantScope>,
    /// Non-empty for a decision request, which goes to the user.
    pub choices: Vec<String>,
    /// How long a decision request stays open, in milliseconds.
    pub expires_in: Option<u64>,
}

pub struct GrantRequest {
    pub caller: Caller,
    pub agent: String,
    pub scope: GrantScope,
    pub duration_ms: u64,
}

/// A new grant, and the decision request that asks the user to approve it when it was
/// requested outside the desktop.
pub struct Granted {
    pub grant: Grant,
    pub request: Option<Message>,
}

pub const APPROVE_GRANT: &str = "Approve";
pub const DENY_GRANT: &str = "Deny";

pub struct Reply {
    pub caller: Caller,
    pub message: MessageId,
    pub recipient_agent: Option<String>,
    pub body: String,
    pub urgent: bool,
    pub scope: Option<GrantScope>,
}

pub struct Talk<S, A, N> {
    store: Arc<S>,
    activity: A,
    notifier: N,
    policy: NoticePolicy,
    clock: fn() -> u64,
    new_id: fn() -> String,
    hooks: Arc<Mutex<Hooks>>,
}

/// Turn-hook state per session. A session appears here once its provider ran a Power turn
/// hook, and from then on the hook, not a terminal notice, delivers while it is healthy.
#[derive(Default)]
struct Hooks {
    next: u64,
    sessions: HashMap<String, HookSession>,
}

#[derive(Default)]
struct HookSession {
    /// Bumps with each turn-state change reported through Talk. Leases and claims bind to
    /// it, so nothing a hook started in one turn can act in a later one.
    turn: u64,
    /// The attached waiter's lease ID and the turn it waits in.
    lease: Option<(u64, u64)>,
    /// A turn end reported by a hook holds terminal notices until then, so the waiter can
    /// attach first.
    grace_until: u64,
    /// Messages a hook has received but not confirmed emitting. They are neither read nor
    /// announced, and go back to the queue if the hook never confirms.
    claim: Option<(u64, Vec<MessageId>)>,
    /// Messages a waiter emitted for an async wake that the provider has not consumed yet.
    emitted: Option<Emitted>,
    /// When a turn end was last reported for the session.
    ended_at: Option<u64>,
}

/// An async wake is consumed once the provider starts the turn it wakes, which reports
/// Busy. Until then its messages stay unread. If no turn starts within the grace period,
/// for example because the waiter died before exiting with them, they return to the queue.
struct Emitted {
    ids: Vec<MessageId>,
    turn: u64,
    until: u64,
}

impl HookSession {
    fn waiting(&self) -> bool {
        self.lease.is_some_and(|(_, turn)| turn == self.turn)
    }
}

/// How long a waiter started with the Stop hook waits for that hook's turn-end report.
/// Claude Code runs both Stop hooks at once, so either may reach Power first.
pub const HOOK_ATTACH_MS: u64 = 5_000;

/// Claude Code starts the Stop hook and the waiter together, so a turn end reported this
/// shortly before the waiter started came from its own Stop hook.
const STOP_SLACK_MS: u64 = 1_000;

/// How long an async wake may take to start its turn before its messages return to the
/// queue for a notice.
pub const WAKE_CONSUME_MS: u64 = HOOK_ATTACH_MS;

/// A turn-hook waiter. Dropping it hands delivery back to terminal notices.
pub struct HookLease {
    hooks: Arc<Mutex<Hooks>>,
    session: String,
    id: u64,
    turn: u64,
}

impl HookLease {
    /// A later waiter replaced this one, or the turn it waited in is over.
    pub fn stale(&self) -> bool {
        self.hooks
            .lock()
            .unwrap()
            .sessions
            .get(&self.session)
            .is_none_or(|hook| hook.lease != Some((self.id, self.turn)) || hook.turn != self.turn)
    }
}

impl Drop for HookLease {
    fn drop(&mut self) {
        let mut hooks = self.hooks.lock().unwrap();
        if let Some(hook) = hooks.sessions.get_mut(&self.session)
            && hook.lease.is_some_and(|(id, _)| id == self.id)
        {
            hook.lease = None;
        }
    }
}

/// Messages a hook holds for handover. Until `Talk::hook_confirm`, nothing is marked: a
/// claim dropped unconfirmed returns its messages to the queue, eligible for a notice.
pub struct HookClaim {
    hooks: Arc<Mutex<Hooks>>,
    session: String,
    id: u64,
    /// The turn the claim was made in, and for a waiter its lease.
    turn: u64,
    lease: Option<u64>,
    pub messages: Vec<Message>,
}

impl Drop for HookClaim {
    fn drop(&mut self) {
        let mut hooks = self.hooks.lock().unwrap();
        if let Some(hook) = hooks.sessions.get_mut(&self.session)
            && hook.claim.as_ref().is_some_and(|(id, _)| *id == self.id)
        {
            hook.claim = None;
        }
    }
}

/// What a waiting hook got.
pub enum HookWait {
    Claimed(HookClaim),
    /// Nothing yet. The lease is still current.
    Timeout,
    /// A later waiter or turn replaced the lease.
    Stale,
}

use anyhow::{Context, bail, ensure};
use domain::{
    dag::{AgentStatus, Coordinator, ManagedAgent},
    talk::{
        Address, AgentRef, Delivery, MAX_BODY_BYTES, MAX_GRANT_MS, NoticeDecision, RATE_WINDOW_MS,
        REMINDER_AFTER_MS, RunRef, SenderContext, hook_deliverable, notice, rate_limit, resolve,
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
    scope: Option<GrantScope>,
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
            hooks: Arc::default(),
        }
    }

    /// Applies a turn report under the hook lock, so every claim's and every notice
    /// plan's check of the turn, its state and the grace period is atomic with it. Bumps the
    /// session's hook turn when the state changes. `hook` registers the session as hooked
    /// and, for a turn end, starts the grace period in the same step. Returns the messages
    /// of an async wake the turn start consumed, which the caller marks read.
    fn report(
        &self,
        hooks: &mut Hooks,
        session: &str,
        state: HookState,
        ready_only: bool,
        hook: bool,
    ) -> Result<Vec<MessageId>> {
        let before = self
            .activity
            .activity(session)
            .map(|activity| activity.hook);
        self.activity.report_turn(session, state, ready_only)?;
        let entry = if hook {
            Some(hooks.sessions.entry(session.into()).or_default())
        } else {
            hooks.sessions.get_mut(session)
        };
        let Some(entry) = entry else {
            return Ok(Vec::new());
        };
        let mut consumed = Vec::new();
        let now = (self.clock)();
        // An expired wake went back to the queue, so a later turn start does not consume it.
        if entry
            .emitted
            .as_ref()
            .is_some_and(|emitted| now >= emitted.until)
        {
            entry.emitted = None;
        }
        // A turn start after an async wake means the provider ran the woken turn.
        if state == HookState::Busy
            && entry
                .emitted
                .as_ref()
                .is_some_and(|emitted| emitted.turn == entry.turn)
        {
            consumed = entry.emitted.take().map(|e| e.ids).unwrap_or_default();
        }
        if before != Some(state) {
            entry.turn += 1;
        }
        if state == HookState::Ready {
            entry.ended_at = Some(now);
        }
        if hook && state == HookState::Ready {
            entry.grace_until = now.saturating_add(self.policy.hook_grace_ms);
        }
        Ok(consumed)
    }

    /// Marks an async wake's messages read once the provider started the woken turn.
    fn consumed(&self, ids: Vec<MessageId>) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let now = (self.clock)();
        self.store.write(|db| {
            let marked: Vec<_> = ids.iter().collect();
            db.mark(&marked, DeliveryChange::Notified, now)?;
            db.mark(&marked, DeliveryChange::Read, now)
        })
    }

    fn turn_report(&self, session: &str, state: HookState, ready_only: bool) -> Result<()> {
        let consumed = {
            let mut hooks = self.hooks.lock().unwrap();
            self.report(&mut hooks, session, state, ready_only, false)?
        };
        self.consumed(consumed)
    }

    fn hook_session(caller: &Caller) -> Result<&str> {
        caller
            .session
            .as_deref()
            .context("POWER_SESSION is required for hook delivery")
    }

    /// The provider's turn-start hook. It registers the session as hooked from its first
    /// turn, so the hook path delivers even before the first turn end.
    pub fn hook_turn_start(&self, caller: &Caller) -> Result<()> {
        let session = Self::hook_session(caller)?;
        let consumed = {
            let mut hooks = self.hooks.lock().unwrap();
            self.report(&mut hooks, session, HookState::Busy, false, true)?
        };
        self.consumed(consumed)
    }

    /// The Stop hook reports the turn end and claims what it may hand over now. Terminal
    /// notices wait `hook_grace_ms` for the background waiter to attach.
    pub fn hook_turn_end(&self, caller: &Caller) -> Result<Option<HookClaim>> {
        let session = Self::hook_session(caller)?;
        let turn = {
            let mut hooks = self.hooks.lock().unwrap();
            self.report(&mut hooks, session, HookState::Ready, false, true)?;
            hooks.sessions[session].turn
        };
        self.claim(caller, session, turn, None)
    }

    /// Registers the session's waiter for the turn that just ended, replacing any earlier
    /// one. It waits until `deadline` for the turn end, since the Stop hook reporting it
    /// runs alongside. Only a Stop starts a waiter, so if no turn report arrived by then the
    /// Stop hook failed, and the waiter reports the turn end itself. A quiet turn that is
    /// still running starts no waiter and is never taken for over. None if the session is
    /// gone or a turn report arrived without a turn end.
    pub fn attach_hook(&self, caller: &Caller, deadline: Instant) -> Result<Option<HookLease>> {
        let session = Self::hook_session(caller)?;
        let started = (self.clock)();
        let mut arrived = None;
        loop {
            {
                let mut hooks = self.hooks.lock().unwrap();
                let Some(activity) = self.activity.activity(session) else {
                    return Ok(None);
                };
                let entry = hooks.sessions.entry(session.into()).or_default();
                let turn = entry.turn;
                // A turn end reported just before the waiter started came from its Stop
                // hook, so a busy turn now is the next one, not a failed Stop.
                let stopped = entry
                    .ended_at
                    .is_some_and(|at| started.saturating_sub(at) < STOP_SLACK_MS);
                let arrived = *arrived.get_or_insert((turn, activity.reported_at));
                let mut ready = activity.hook == HookState::Ready;
                let expired = Instant::now() >= deadline;
                if !ready && expired && !stopped && arrived == (turn, activity.reported_at) {
                    self.report(&mut hooks, session, HookState::Ready, false, true)?;
                    ready = true;
                }
                if ready {
                    hooks.next += 1;
                    let id = hooks.next;
                    let hook = hooks.sessions.entry(session.into()).or_default();
                    hook.lease = Some((id, hook.turn));
                    return Ok(Some(HookLease {
                        hooks: self.hooks.clone(),
                        session: session.into(),
                        id,
                        turn: hook.turn,
                    }));
                }
                if expired {
                    return Ok(None);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Whether the session's waiter is attached for its current turn.
    pub fn hook_attached(&self, caller: &Caller) -> Result<bool> {
        let session = Self::hook_session(caller)?;
        Ok(self
            .hooks
            .lock()
            .unwrap()
            .sessions
            .get(session)
            .is_some_and(HookSession::waiting))
    }

    /// Claims the messages a hook may hand over now. The store write serializes it with
    /// notices, and under the hook lock it rechecks that the session is still in `turn`,
    /// at a turn end, and, for a waiter, that its lease is current. One atomic step.
    fn claim(
        &self,
        caller: &Caller,
        session: &str,
        turn: u64,
        lease: Option<u64>,
    ) -> Result<Option<HookClaim>> {
        let live = self.live(caller);
        self.store.write(|db| {
            let directory = db.directory()?;
            let actor = self.actor(caller, &directory, live.as_ref())?;
            let Some(agent) = actor.agent else {
                return Ok(None);
            };
            let needs_input = agent.status == AgentStatus::NeedsInput;
            let mut messages = db.inbox(&actor.recipient(), actor.box_id, false)?;
            let mut hooks = self.hooks.lock().unwrap();
            hooks.next += 1;
            let id = hooks.next;
            let Some(hook) = hooks.sessions.get_mut(session) else {
                return Ok(None);
            };
            let current = hook.turn == turn
                && lease.is_none_or(|lease| hook.lease == Some((lease, turn)))
                && hook.claim.is_none();
            let now = (self.clock)();
            let emitted = hook
                .emitted
                .as_ref()
                .filter(|emitted| now < emitted.until)
                .map(|emitted| emitted.ids.clone())
                .unwrap_or_default();
            let Some(activity) = self.activity.activity(session) else {
                return Ok(None);
            };
            if !current || activity.hook != HookState::Ready {
                return Ok(None);
            }
            messages.retain(|message| {
                !emitted.contains(&message.id)
                    && hook_deliverable(&message.queued(), activity, needs_input)
            });
            if messages.is_empty() {
                return Ok(None);
            }
            hook.claim = Some((id, messages.iter().map(|m| m.id.clone()).collect()));
            Ok(Some(HookClaim {
                hooks: self.hooks.clone(),
                session: session.into(),
                id,
                turn,
                lease,
                messages,
            }))
        })
    }

    /// The hook emitted the claimed messages to the agent. A Stop hook's block decision
    /// reaches the agent as the hook exits, so its messages are marked notified and read
    /// at once, and the handover continues the claim's turn. A waiter's messages reach the
    /// agent only when the provider starts the turn its exit wakes, so they stay unread
    /// until that turn start reports Busy (see `Emitted`). Either way the claim acts only
    /// in the turn it was made in: a late confirmation never changes a newer turn.
    pub fn hook_confirm(&self, claim: HookClaim) -> Result<()> {
        let now = (self.clock)();
        if claim.lease.is_some() {
            let mut hooks = self.hooks.lock().unwrap();
            if let Some(hook) = hooks.sessions.get_mut(&claim.session)
                && hook.claim.as_ref().is_some_and(|(id, _)| *id == claim.id)
            {
                hook.claim = None;
                // The wake comes after the waiter exits, so the next turn start consumes it
                // even if a turn ran while the confirmation was late.
                hook.emitted = Some(Emitted {
                    ids: claim.messages.iter().map(|m| m.id.clone()).collect(),
                    turn: hook.turn,
                    until: now.saturating_add(WAKE_CONSUME_MS),
                });
            }
            drop(hooks);
            drop(claim);
            return Ok(());
        }
        self.store.write(|db| {
            let marked: Vec<_> = claim.messages.iter().map(|message| &message.id).collect();
            db.mark(&marked, DeliveryChange::Notified, now)?;
            db.mark(&marked, DeliveryChange::Read, now)
        })?;
        let session = claim.session.clone();
        let turn = claim.turn;
        drop(claim);
        let consumed = {
            let mut hooks = self.hooks.lock().unwrap();
            if hooks
                .sessions
                .get(&session)
                .is_none_or(|hook| hook.turn != turn)
            {
                return Ok(());
            }
            self.report(&mut hooks, &session, HookState::Busy, false, false)?
        };
        self.consumed(consumed)
    }

    /// Waits while the lease is current for messages to claim, until `deadline`.
    pub fn hook_wait(
        &self,
        lease: &HookLease,
        caller: &Caller,
        deadline: Instant,
    ) -> Result<HookWait> {
        loop {
            if lease.stale() {
                return Ok(HookWait::Stale);
            }
            let cursor = self.cursor()?;
            if let Some(claim) = self.claim(caller, &lease.session, lease.turn, Some(lease.id))? {
                return Ok(HookWait::Claimed(claim));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(HookWait::Timeout);
            }
            // Turn state lives outside the store, so recheck it at least every quarter second.
            self.store.wait_for_change(
                cursor,
                deadline.min(now + std::time::Duration::from_millis(250)),
            );
        }
    }

    /// The session's activity, whether a healthy turn hook delivers for it now, and the
    /// messages a hook holds, read together under the hook lock so a turn report and its
    /// grace period are never seen apart. A hooked session gets no terminal notices. Its
    /// turn is over only once a turn end is reported, by the Stop hook or by the waiter it
    /// starts, so quiet output during a running turn never releases it. It stops being
    /// hooked when its waiter is gone past the grace period after a turn end.
    fn hook_hold(&self, session: &str, now: u64) -> Option<(Activity, bool, Vec<MessageId>)> {
        let hooks = self.hooks.lock().unwrap();
        let activity = self.activity.activity(session)?;
        let Some(hook) = hooks.sessions.get(session) else {
            return Some((activity, false, Vec::new()));
        };
        let emitted = hook.emitted.as_ref().filter(|emitted| now < emitted.until);
        // A turn the user interrupted ends without a Stop. Keystrokes after the turn start
        // followed by a stale busy report tell it apart from a quiet turn that still runs.
        let interrupted =
            activity.last_input > activity.reported_at && self.policy.busy_stale(activity, now);
        let in_turn = activity.hook == HookState::Busy && !interrupted;
        let hooked = hook.waiting() || now < hook.grace_until || in_turn || emitted.is_some();
        let claimed = hook
            .claim
            .iter()
            .flat_map(|(_, ids)| ids)
            .chain(emitted.iter().flat_map(|emitted| &emitted.ids))
            .cloned()
            .collect();
        Some((activity, hooked, claimed))
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
        let decision = if command.choices.is_empty() {
            ensure!(
                command.expires_in.is_none(),
                "Only a decision request with choices expires"
            );
            None
        } else {
            ensure!(
                command.target == Target::One(Address::User),
                "Decision requests go to the user"
            );
            let expires_at = command
                .expires_in
                .map(|ms| (self.clock)().saturating_add(ms));
            Some(Decision::new(command.choices.clone(), expires_at).map_err(anyhow::Error::msg)?)
        };
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
                    scope: command.scope,
                };
                let mut message = self.build(db, outgoing, now)?;
                message.broadcast = broadcast.as_ref().map(|(_, id)| id.clone());
                message.decision = decision.clone();
                db.insert(&message)?;
                messages.push(message);
            }
            Ok(messages)
        })
    }

    /// Grants an agent scoped authority to relay the user's decisions. Only the user can.
    /// From the desktop the grant starts at once. From the user's shell it waits for the user
    /// to approve it in the desktop, since Power cannot confirm that the user ran the shell.
    pub fn grant(&self, command: GrantRequest) -> Result<Granted> {
        ensure!(
            command.duration_ms > 0 && command.duration_ms <= MAX_GRANT_MS,
            "A grant lasts between 1 second and 7 days"
        );
        let live = self.live(&command.caller);
        self.store.write(|db| {
            let directory = db.directory()?;
            let actor = self.actor(&command.caller, &directory, live.as_ref())?;
            let sender = actor.sender(None);
            ensure!(
                sender.has_user_authority(),
                "Only the user can grant authority. Agents cannot use power talk grant"
            );
            let agent = directory.agent(&command.agent, actor.box_id)?;
            let now = (self.clock)();
            let mut grant = Grant {
                id: (self.new_id)(),
                agent: agent.id.clone(),
                box_id: agent.box_id.clone(),
                scope: command.scope,
                created_at: now,
                duration_ms: command.duration_ms,
                approved_at: sender.is_user_authored().then_some(now),
                revoked_at: None,
                request: None,
            };
            let mut request = None;
            if !sender.is_user_authored() {
                let mut ask = self.build(
                    db,
                    Outgoing {
                        box_id: agent.box_id.clone(),
                        sender: Sender::System,
                        recipient: Recipient::User,
                        body: format!(
                            "Approve grant {} for agent {} ({}), scope {}, for {}? It was \
                             requested from a shell, so it waits for you here.",
                            grant.id,
                            agent.id,
                            agent.label,
                            grant.scope.as_str(),
                            duration_label(grant.duration_ms)
                        ),
                        run: None,
                        urgent: false,
                        scope: None,
                    },
                    now,
                )?;
                ask.decision = Some(
                    Decision::new(vec![APPROVE_GRANT.into(), DENY_GRANT.into()], None)
                        .map_err(anyhow::Error::msg)?,
                );
                grant.request = Some(ask.id.clone());
                request = Some(ask);
            }
            db.insert_grant(&grant)?;
            if let Some(ask) = &request {
                db.insert(ask)?;
            } else {
                self.record_grant(db, &directory, &grant, "granted", now)?;
            }
            Ok(Granted { grant, request })
        })
    }

    /// Ends a grant at once. Only the user can.
    pub fn revoke(&self, caller: Caller, id: &str) -> Result<Grant> {
        let live = self.live(&caller);
        self.store.write(|db| {
            let directory = db.directory()?;
            let actor = self.actor(&caller, &directory, live.as_ref())?;
            ensure!(actor.agent.is_none(), "Only the user can revoke a grant");
            let grant = db
                .grants(None)?
                .into_iter()
                .find(|grant| grant.id == id)
                .with_context(|| format!("Unknown grant {id:?}"))?;
            let now = (self.clock)();
            ensure!(
                db.revoke_grant(&grant.id, now)?,
                "Grant {id:?} is already revoked"
            );
            let grant = Grant {
                revoked_at: Some(now),
                ..grant
            };
            if let Some(request) = &grant.request {
                // A pending grant's approval request closes with it.
                db.answer(request, now)?;
            }
            self.record_grant(db, &directory, &grant, "revoked", now)?;
            Ok(grant)
        })
    }

    /// Every grant for the user. An agent sees its own.
    pub fn grants(&self, caller: Caller) -> Result<Vec<Grant>> {
        let live = self.live(&caller);
        self.store.read(|db| {
            let directory = db.directory()?;
            let actor = self.actor(&caller, &directory, live.as_ref())?;
            db.grants(actor.agent.map(|agent| agent.id.as_str()))
        })
    }

    /// Records a grant change in talk history: to the user, and to the agent while it runs.
    fn record_grant(
        &self,
        db: &dyn TalkTransaction,
        directory: &Directory,
        grant: &Grant,
        change: &str,
        now: u64,
    ) -> Result<()> {
        let until = match (change, grant.expires_at()) {
            ("granted" | "approved", Some(at)) => format!(
                " It lasts {} and expires at {} (Unix ms) on its own.",
                duration_label(grant.duration_ms),
                at
            ),
            _ => String::new(),
        };
        let body = format!(
            "Grant {} for agent {}, scope {}, {change}.{until}",
            grant.id,
            grant.agent,
            grant.scope.as_str()
        );
        let mut recipients = vec![Recipient::User];
        if let Some(agent) = directory
            .agents
            .iter()
            .find(|agent| agent.id == grant.agent)
            && agent.status != AgentStatus::Exited
            && agent.session.is_some()
        {
            recipients.push(Recipient::agent(agent));
        }
        for recipient in recipients {
            let body = if recipient == Recipient::User {
                body.clone()
            } else {
                format!(
                    "{body} Relay the user's decisions in this scope with \
                     power talk send <to> --scope {} \"<message>\".",
                    grant.scope.as_str()
                )
            };
            let message = self.build(
                db,
                Outgoing {
                    box_id: grant.box_id.clone(),
                    sender: Sender::System,
                    recipient,
                    body,
                    run: None,
                    urgent: false,
                    scope: None,
                },
                now,
            )?;
            db.insert(&message)?;
        }
        Ok(())
    }

    pub fn reply(&self, command: Reply) -> Result<Message> {
        validate_body(&command.body)?;
        let live = self.live(&command.caller);
        self.store.write(|db| {
            let directory = db.directory()?;
            let actor = self.actor(&command.caller, &directory, live.as_ref())?;
            let parent = require_message(db, &command.message)?;
            let sender = actor.sender(parent.run.as_deref());
            let now = (self.clock)();
            let mut grant_request = false;
            // The one choke point for answers: only the user, in the desktop, answers a
            // decision request, once. A shell has the user's authority, but Power cannot
            // confirm that the user wrote its answer.
            if let Some(decision) = &parent.decision {
                ensure!(
                    sender.is_user_authored(),
                    "Only the user can answer decision request {:?}",
                    command.message.0
                );
                ensure!(
                    decision.is_open(now) && db.answer(&parent.id, now)?,
                    "Decision request {:?} is already answered or expired",
                    command.message.0
                );
                db.mark(&[&parent.id], DeliveryChange::Read, now)?;
                if let Some(grant) = db.grant_for_request(&parent.id)? {
                    grant_request = true;
                    if command.body == APPROVE_GRANT {
                        ensure!(db.approve_grant(&grant.id, now)?, "Grant {:?} was revoked", grant.id);
                        let approved = Grant { approved_at: Some(now), ..grant };
                        self.record_grant(db, &directory, &approved, "approved", now)?;
                    } else {
                        db.revoke_grant(&grant.id, now)?;
                        self.record_grant(db, &directory, &grant, "denied", now)?;
                    }
                }
            }
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
                    // The user's answer to a grant approval stays in the user's history.
                    Sender::System if grant_request => Recipient::User,
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
                run: parent.run, urgent: command.urgent, scope: command.scope,
            }, now)?;
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
        // The one choke point for relayed authority: a message carries a grant only if its
        // sender holds an active one in the requested scope when Power stamps it. Otherwise
        // it is a plain peer message.
        let grant = match (&outgoing.sender, outgoing.scope) {
            (Sender::Agent(agent), Some(scope)) => db
                .grants(Some(&agent.id))?
                .into_iter()
                .find(|grant| {
                    grant.scope == scope && grant.box_id == agent.box_id && grant.is_active(now)
                })
                .map(|grant| GrantRelay {
                    id: grant.id,
                    scope,
                }),
            _ => None,
        };
        let id = (self.new_id)();
        Ok(Message {
            grant,
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
            decision: None,
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

    /// Decision requests to the user across every box. Unless `all`, only open ones.
    pub fn decisions(&self, caller: Caller, all: bool) -> Result<Vec<Message>> {
        let live = self.live(&caller);
        self.store.read(|db| {
            let directory = db.directory()?;
            let actor = self.actor(&caller, &directory, live.as_ref())?;
            ensure!(
                actor.agent.is_none(),
                "Decision requests are answered by the user"
            );
            db.decisions(all, (self.clock)())
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
        self.turn_report(&session, state, ready_only)
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
        let needs_input = agent.status == AgentStatus::NeedsInput;
        let now = (self.clock)();
        let Some((activity, hooked, claimed)) = self.hook_hold(&session, now) else {
            return Ok(None);
        };
        let mut plan = Plan {
            session,
            activity,
            now,
            notify: Vec::new(),
            remind: Vec::new(),
        };
        for message in db.queued(&Recipient::agent(&agent), &agent.box_id)? {
            // A healthy turn hook delivers everything, urgent user messages included, at
            // the next turn boundary. Messages a hook holds wait for its confirmation.
            if hooked || claimed.contains(&message.id) {
                continue;
            }
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
            scope: None,
            choices: Vec::new(),
            expires_in: None,
        })?;
        messages
            .into_iter()
            .next()
            .map(|message| message.id.0)
            .context("Talk queued no message for the worker")
    }
}

impl<S: TalkStore, A: SessionActivity, N: Notifier> crate::dag::DecisionPoster for Talk<S, A, N> {
    fn check_decision(&self, body: &str, choices: &[String]) -> Result<()> {
        validate_body(body)?;
        Decision::new(choices.to_vec(), None).map_err(anyhow::Error::msg)?;
        Ok(())
    }

    fn post_decision(
        &self,
        db: &dyn TalkTransaction,
        agent: &str,
        run: &str,
        body: &str,
        choices: Vec<String>,
    ) -> Result<Message> {
        self.check_decision(body, &choices)?;
        let directory = db.directory()?;
        let worker = directory
            .agents
            .iter()
            .find(|candidate| candidate.id == agent)
            .with_context(|| format!("Unknown worker {agent:?}"))?;
        let actor = Actor {
            agent: Some(worker),
            box_id: Some(&worker.box_id),
            desktop: false,
            directory: &directory,
        };
        let now = (self.clock)();
        let mut message = self.build(
            db,
            Outgoing {
                box_id: worker.box_id.clone(),
                sender: actor.sender(Some(run)),
                recipient: Recipient::User,
                body: body.into(),
                run: Some(run.into()),
                urgent: false,
                scope: None,
            },
            now,
        )?;
        message.decision = Some(Decision::new(choices, None).map_err(anyhow::Error::msg)?);
        db.insert(&message)?;
        Ok(message)
    }
}

fn ids(batch: &[Queued]) -> Vec<&MessageId> {
    batch.iter().map(|message| &message.id).collect()
}

fn require_message(db: &dyn TalkReader, id: &MessageId) -> Result<Message> {
    db.message(id)?
        .with_context(|| format!("Unknown talk message {:?}", id.0))
}

pub fn duration_label(ms: u64) -> String {
    let secs = ms / 1000;
    match secs {
        s if s >= 86_400 && s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s >= 3_600 && s % 3_600 == 0 => format!("{}h", s / 3_600),
        s if s >= 60 && s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
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
