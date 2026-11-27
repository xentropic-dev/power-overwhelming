use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub struct TalkRequest {
    pub session: Option<String>,
    pub box_sel: Option<String>,
    /// Set by the desktop. Other session-less callers send as the user's shell.
    #[serde(default)]
    pub desktop: bool,
    #[serde(flatten)]
    pub command: TalkCommand,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum TalkCommand {
    Send {
        to: Option<String>,
        run: Option<String>,
        body: String,
        urgent: bool,
    },
    Reply {
        id: String,
        recipient_agent: Option<String>,
        body: String,
        urgent: bool,
    },
    Inbox {
        all: bool,
        peek: bool,
    },
    Show {
        id: String,
        read: bool,
    },
    Wait {
        threads: Vec<String>,
        /// Only a reply to one of these messages ends the wait.
        #[serde(default)]
        reply_to: Vec<String>,
        seconds: u64,
    },
    Log {
        agent: Option<String>,
        run: Option<String>,
        limit: Option<usize>,
        to_user: bool,
    },
    Turn {
        ready: bool,
        /// The provider reports turn ends but not turn starts.
        #[serde(default)]
        ready_only: bool,
    },
    Summary,
    Watch,
    /// Unread messages a turn hook may hand over now. `turn_ended` also reports the
    /// turn's end.
    HookPending {
        turn_ended: bool,
    },
    /// Streams keepalive `Ok` responses until messages a hook may hand over arrive, then
    /// sends them and ends. While it waits, the session gets no terminal notices.
    HookWait,
    /// The hook handed these messages to the agent.
    HookDelivered {
        ids: Vec<String>,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TalkResponse {
    Messages { messages: Vec<TalkMessage> },
    Reply { message: Option<Box<TalkMessage>> },
    Summary { agents: Vec<TalkAgentSummary> },
    Event { event: TalkEvent },
    Ok,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct TalkMessage {
    pub id: String,
    pub box_id: String,
    pub sender: TalkSender,
    pub recipient: TalkRecipient,
    pub body: String,
    pub thread: String,
    pub reply_to: Option<String>,
    pub broadcast: Option<String>,
    pub run: Option<String>,
    pub created_at: u64,
    pub delivery: TalkDelivery,
    pub urgent: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TalkSender {
    Agent {
        id: String,
        box_id: String,
        provider: String,
        role: String,
        run: Option<String>,
    },
    User,
    Shell,
    System,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TalkRecipient {
    Agent { id: String, box_id: String },
    User,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct TalkDelivery {
    pub queued_at: u64,
    pub notified_at: Option<u64>,
    pub reminded_at: Option<u64>,
    pub read_at: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct TalkEvent {
    pub seq: u64,
    pub box_id: String,
    pub message: String,
    pub kind: TalkEventKind,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TalkEventKind {
    #[serde(rename = "talk_message")]
    Message,
    #[serde(rename = "talk_read")]
    Read,
    #[serde(rename = "talk_delivery")]
    Delivery,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct TalkAgentSummary {
    pub agent: String,
    pub unread: usize,
    pub to_user: usize,
    pub count: usize,
    pub latest: String,
}
