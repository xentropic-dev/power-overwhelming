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

pub trait TalkDirectory: Send + Sync {
    fn snapshot(&self) -> Result<Directory>;
}

pub trait SessionActivity: Send + Sync {
    fn activity(&self, session: &str) -> Option<Activity>;
    fn report_turn(&self, session: &str, state: HookState) -> Result<()>;
}

pub trait Notifier: Send + Sync {
    fn notify(&self, session: &str, line: &str) -> Result<()>;
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

pub struct Talk<S, D, A, N> {
    store: Arc<S>,
    directory: D,
    activity: A,
    notifier: N,
    policy: NoticePolicy,
    clock: fn() -> u64,
    new_id: fn() -> String,
}

impl<S: TalkStore, D: TalkDirectory, A: SessionActivity, N: Notifier> Talk<S, D, A, N> {
    pub fn new(
        store: Arc<S>,
        directory: D,
        activity: A,
        notifier: N,
        policy: NoticePolicy,
        clock: fn() -> u64,
        new_id: fn() -> String,
    ) -> Self {
        Self {
            store,
            directory,
            activity,
            notifier,
            policy,
            clock,
            new_id,
        }
    }

    pub fn send(&self, _command: SendMessage) -> Result<Vec<Message>> {
        todo!()
    }
    pub fn reply(&self, _command: Reply) -> Result<Message> {
        todo!()
    }
    pub fn inbox(&self, _caller: Caller, _all: bool, _peek: bool) -> Result<Vec<Message>> {
        todo!()
    }
    pub fn show(&self, _caller: Caller, _message: MessageId) -> Result<Vec<Message>> {
        todo!()
    }
    pub fn wait_for_reply(
        &self,
        _caller: Caller,
        _thread: ThreadId,
        _after: Option<MessageId>,
        _deadline: Instant,
    ) -> Result<Option<Message>> {
        todo!()
    }
    pub fn log(&self, _caller: Caller, _filter: LogFilter) -> Result<Vec<Message>> {
        todo!()
    }
    pub fn report_turn(&self, _caller: Caller, _state: HookState) -> Result<()> {
        todo!()
    }
    pub fn deliver_pending(&self) -> Result<()> {
        todo!()
    }
}
