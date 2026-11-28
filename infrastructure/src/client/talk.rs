use crate::wire::{self, Request, Response, talk::*};
use anyhow::{Context, Result, bail};
use application::talk::{LogFilter, TalkEvent as Event, TalkEventKind as EventKind};
use domain::talk::{
    AgentRef, AgentTalkSummary, Decision, Delivery, Grant, GrantRelay, GrantScope, Message,
    MessageId, Recipient, Sender, ThreadId,
};
use std::{io::BufReader, os::unix::net::UnixStream, path::PathBuf, time::Duration};

#[derive(Clone, Default)]
pub struct Caller {
    pub session: Option<String>,
    pub box_sel: Option<String>,
    /// Claims the desktop. The daemon acts on it only when it verifies that the calling
    /// process is the desktop, which holds the box layout lock. Otherwise the caller is
    /// the user's shell.
    pub desktop: bool,
    /// The daemon socket, when not the environment's.
    pub socket: Option<PathBuf>,
}

impl Caller {
    /// The desktop, acting for the user in `box_id`.
    pub fn desktop(box_id: String) -> Self {
        Self {
            session: None,
            box_sel: Some(box_id),
            desktop: true,
            socket: None,
        }
    }

    /// The same caller, against the daemon at `socket`.
    pub fn at_socket(self, socket: PathBuf) -> Self {
        Self {
            socket: Some(socket),
            ..self
        }
    }
}

/// The desktop's user inbox for `box_id`: one row per thread, led by the decision requests
/// from every box that are still open at `now`. Closed requests stay in the history.
pub fn desktop_inbox(box_id: String, now: u64) -> Result<Vec<Message>> {
    user_inbox(Caller::desktop(box_id), now)
}

/// The user inbox as `caller` sees it. See `desktop_inbox`.
pub fn user_inbox(caller: Caller, now: u64) -> Result<Vec<Message>> {
    let messages = log(
        caller.clone(),
        LogFilter {
            to_user: true,
            ..LogFilter::default()
        },
    )?;
    let mut threads = std::collections::HashSet::new();
    // Every box's requests, closed ones too, so an answered or expired request from
    // another box stays in the history.
    let rows = decisions(caller, true)?
        .into_iter()
        .chain(messages)
        .filter(|message| threads.insert(message.thread.clone()))
        .collect();
    Ok(domain::talk::open_decisions_first(rows, now))
}

fn request(caller: Caller, command: TalkCommand) -> Request {
    Request::Talk {
        talk: TalkRequest {
            session: caller.session,
            box_sel: caller.box_sel,
            desktop: caller.desktop,
            command,
        },
    }
}

fn decode(response: Response) -> Result<TalkResponse> {
    match response {
        Response::Talk { talk } => Ok(talk),
        Response::Error { message } => bail!("{message}"),
        _ => bail!("Unexpected daemon response"),
    }
}

fn send_request(caller: Caller, command: TalkCommand) -> Result<Response> {
    let socket = caller.socket.clone();
    wire::request_at(socket.as_deref(), &request(caller, command))
}

fn messages(caller: Caller, command: TalkCommand) -> Result<Vec<Message>> {
    match decode(send_request(caller, command)?)? {
        TalkResponse::Messages { messages } => Ok(messages.into_iter().map(Into::into).collect()),
        _ => bail!("Unexpected talk response"),
    }
}

pub fn send(
    caller: Caller,
    to: Option<String>,
    run: Option<String>,
    body: String,
    urgent: bool,
) -> Result<Vec<Message>> {
    send_scoped(caller, to, run, body, urgent, None)
}

/// Sends, relaying the user's decision under the sender's grant in `scope`, if it holds one.
pub fn send_scoped(
    caller: Caller,
    to: Option<String>,
    run: Option<String>,
    body: String,
    urgent: bool,
    scope: Option<GrantScope>,
) -> Result<Vec<Message>> {
    messages(
        caller,
        TalkCommand::Send {
            to,
            run,
            body,
            urgent,
            choices: Vec::new(),
            expires_in_ms: None,
            scope: scope.map(|scope| scope.as_str().into()),
        },
    )
}

/// Posts a decision request to the user inbox.
pub fn ask(
    caller: Caller,
    body: String,
    choices: Vec<String>,
    expires_in_ms: Option<u64>,
) -> Result<Message> {
    messages(
        caller,
        TalkCommand::Send {
            to: Some("user".into()),
            run: None,
            body,
            urgent: false,
            choices,
            expires_in_ms,
            scope: None,
        },
    )?
    .pop()
    .context("Daemon queued no decision request")
}

/// Decision requests to the user from every box. Unless `all`, only open ones.
pub fn decisions(caller: Caller, all: bool) -> Result<Vec<Message>> {
    messages(caller, TalkCommand::Decisions { all })
}

pub fn reply(
    caller: Caller,
    id: MessageId,
    body: String,
    recipient_agent: Option<String>,
) -> Result<Message> {
    reply_scoped(caller, id, body, recipient_agent, None)
}

pub fn reply_scoped(
    caller: Caller,
    id: MessageId,
    body: String,
    recipient_agent: Option<String>,
    scope: Option<GrantScope>,
) -> Result<Message> {
    messages(
        caller,
        TalkCommand::Reply {
            id: id.0,
            body,
            urgent: false,
            recipient_agent,
            scope: scope.map(|scope| scope.as_str().into()),
        },
    )?
    .pop()
    .context("Daemon returned no reply")
}

pub fn summary(caller: Caller) -> Result<Vec<AgentTalkSummary>> {
    match decode(send_request(caller, TalkCommand::Summary)?)? {
        TalkResponse::Summary { agents } => Ok(agents.into_iter().map(Into::into).collect()),
        _ => bail!("Unexpected talk response"),
    }
}

pub fn inbox(caller: Caller, all: bool, peek: bool) -> Result<Vec<Message>> {
    messages(caller, TalkCommand::Inbox { all, peek })
}

pub fn show(caller: Caller, id: MessageId, read: bool) -> Result<Vec<Message>> {
    messages(caller, TalkCommand::Show { id: id.0, read })
}

pub fn log(caller: Caller, filter: LogFilter) -> Result<Vec<Message>> {
    messages(
        caller,
        TalkCommand::Log {
            agent: filter.agent,
            run: filter.run,
            limit: filter.limit,
            to_user: filter.to_user,
        },
    )
}

pub fn turn(caller: Caller, ready: bool, ready_only: bool) -> Result<()> {
    match decode(send_request(
        caller,
        TalkCommand::Turn { ready, ready_only },
    )?)? {
        TalkResponse::Ok => Ok(()),
        _ => bail!("Unexpected talk response"),
    }
}

pub fn wait(
    caller: Caller,
    threads: Vec<ThreadId>,
    reply_to: Vec<MessageId>,
    seconds: u64,
) -> Result<Option<Message>> {
    let mut socket = connect(
        caller,
        TalkCommand::Wait {
            threads: threads.into_iter().map(|id| id.0).collect(),
            reply_to: reply_to.into_iter().map(|id| id.0).collect(),
            seconds,
        },
        Duration::from_secs(seconds.saturating_add(10)),
    )?;
    match decode(wire::reply(&mut socket)?)? {
        TalkResponse::Reply { message } => Ok(message.map(|message| (*message).into())),
        _ => bail!("Unexpected talk response"),
    }
}

pub fn watch(caller: Caller, mut event: impl FnMut(Option<Event>) -> bool) -> Result<()> {
    let mut socket = connect(caller, TalkCommand::Watch, Duration::from_secs(15))?;
    while let Some(response) = wire::receive(&mut socket)? {
        let next = match decode(response)? {
            TalkResponse::Event { event } => Some(event.into()),
            TalkResponse::Ok => None,
            _ => bail!("Unexpected talk watch response"),
        };
        if !event(next) {
            return Ok(());
        }
    }
    bail!("Talk watch disconnected")
}

fn connect(
    caller: Caller,
    command: TalkCommand,
    timeout: Duration,
) -> Result<BufReader<UnixStream>> {
    let mut socket = wire::connect_at(caller.socket.as_deref())?;
    socket.set_read_timeout(Some(timeout))?;
    socket.set_write_timeout(Some(Duration::from_secs(5)))?;
    wire::send(&mut socket, &request(caller, command))?;
    Ok(BufReader::new(socket))
}

pub fn json(message: Message) -> serde_json::Value {
    serde_json::to_value(TalkMessage::from(message)).expect("talk DTO is serializable")
}

impl From<Message> for TalkMessage {
    fn from(message: Message) -> Self {
        let delivery = message.delivery;
        Self {
            id: message.id.0,
            box_id: message.box_id,
            sender: match message.sender {
                Sender::User => TalkSender::User,
                Sender::Shell => TalkSender::Shell,
                Sender::System => TalkSender::System,
                Sender::Agent(agent) => TalkSender::Agent {
                    id: agent.id,
                    box_id: agent.box_id,
                    provider: agent.provider,
                    role: agent.role,
                    run: agent.run,
                },
            },
            recipient: match message.recipient {
                Recipient::User => TalkRecipient::User,
                Recipient::Agent { id, box_id } => TalkRecipient::Agent { id, box_id },
            },
            body: message.body,
            thread: message.thread.0,
            reply_to: message.reply_to.map(|id| id.0),
            broadcast: message.broadcast,
            run: message.run,
            created_at: message.created_at,
            delivery: TalkDelivery {
                queued_at: delivery.queued_at,
                notified_at: delivery.notified_at,
                reminded_at: delivery.reminded_at,
                read_at: delivery.read_at,
            },
            urgent: message.urgent,
            decision: message.decision.map(|d| TalkDecision {
                choices: d.choices,
                expires_at: d.expires_at,
                answered_at: d.answered_at,
            }),
            grant: message.grant.map(|g| TalkGrantRelay {
                id: g.id,
                scope: g.scope.as_str().into(),
            }),
        }
    }
}

impl From<TalkMessage> for Message {
    fn from(message: TalkMessage) -> Self {
        let delivery = message.delivery;
        Self {
            id: MessageId(message.id),
            box_id: message.box_id,
            sender: match message.sender {
                TalkSender::User => Sender::User,
                TalkSender::Shell => Sender::Shell,
                TalkSender::System => Sender::System,
                TalkSender::Agent {
                    id,
                    box_id,
                    provider,
                    role,
                    run,
                } => Sender::Agent(AgentRef {
                    id,
                    box_id,
                    provider,
                    role,
                    run,
                }),
            },
            recipient: match message.recipient {
                TalkRecipient::User => Recipient::User,
                TalkRecipient::Agent { id, box_id } => Recipient::Agent { id, box_id },
            },
            body: message.body,
            thread: ThreadId(message.thread),
            reply_to: message.reply_to.map(MessageId),
            broadcast: message.broadcast,
            run: message.run,
            created_at: message.created_at,
            delivery: Delivery {
                queued_at: delivery.queued_at,
                notified_at: delivery.notified_at,
                reminded_at: delivery.reminded_at,
                read_at: delivery.read_at,
            },
            urgent: message.urgent,
            decision: message.decision.map(|d| Decision {
                choices: d.choices,
                expires_at: d.expires_at,
                answered_at: d.answered_at,
            }),
            // An unknown scope from a newer daemon reads as no grant, never as one.
            grant: message.grant.and_then(|g| {
                Some(GrantRelay {
                    id: g.id,
                    scope: GrantScope::parse(&g.scope).ok()?,
                })
            }),
        }
    }
}

impl From<Event> for TalkEvent {
    fn from(value: Event) -> Self {
        Self {
            seq: value.seq,
            box_id: value.box_id,
            message: value.message.0,
            kind: match value.kind {
                EventKind::Message => TalkEventKind::Message,
                EventKind::Read => TalkEventKind::Read,
                EventKind::Delivery => TalkEventKind::Delivery,
            },
        }
    }
}

impl From<TalkEvent> for Event {
    fn from(value: TalkEvent) -> Self {
        Self {
            seq: value.seq,
            box_id: value.box_id,
            message: MessageId(value.message),
            kind: match value.kind {
                TalkEventKind::Message => EventKind::Message,
                TalkEventKind::Read => EventKind::Read,
                TalkEventKind::Delivery => EventKind::Delivery,
            },
        }
    }
}

impl From<AgentTalkSummary> for TalkAgentSummary {
    fn from(value: AgentTalkSummary) -> Self {
        Self {
            agent: value.agent,
            unread: value.unread,
            to_user: value.to_user,
            count: value.count,
            latest: value.latest,
        }
    }
}

impl From<TalkAgentSummary> for AgentTalkSummary {
    fn from(value: TalkAgentSummary) -> Self {
        Self {
            agent: value.agent,
            unread: value.unread,
            to_user: value.to_user,
            count: value.count,
            latest: value.latest,
        }
    }
}

/// Reports a turn start from the provider's turn-start hook.
pub fn hook_start(caller: Caller) -> Result<()> {
    match decode(send_request(caller, TalkCommand::HookStart)?)? {
        TalkResponse::Ok => Ok(()),
        _ => bail!("Unexpected talk response"),
    }
}

/// Runs a turn hook against the daemon. `emit` hands the messages to the agent, and only
/// after it succeeds does the hook confirm them. A hook that fails or dies before
/// confirming leaves them queued. Returns whether it emitted any. Once `emit` succeeded the
/// agent gets the messages however the confirmation goes, so a lost reply still returns
/// true: the daemon either recorded the handover or still holds the messages unread.
pub fn hook(
    caller: Caller,
    wait: bool,
    emit: impl FnOnce(&[Message]) -> Result<()>,
) -> Result<bool> {
    let command = if wait {
        TalkCommand::HookWait
    } else {
        TalkCommand::HookStop
    };
    // A waiting hook gets a keepalive every five seconds.
    let mut socket = connect(caller, command, Duration::from_secs(15))?;
    while let Some(response) = wire::receive(&mut socket)? {
        match decode(response)? {
            TalkResponse::Ok if wait => {}
            TalkResponse::Ok => return Ok(false),
            TalkResponse::Messages { messages } => {
                let messages: Vec<Message> = messages.into_iter().map(Into::into).collect();
                emit(&messages)?;
                let recorded = wire::send(socket.get_mut(), &HookEmitted { emitted: true })
                    .and_then(|()| wire::receive(&mut socket))
                    .and_then(|reply| reply.map(decode).transpose());
                if !matches!(recorded, Ok(Some(TalkResponse::Ok))) {
                    eprintln!("Power did not confirm recording the hook's delivery");
                }
                return Ok(true);
            }
            _ => bail!("Unexpected talk hook response"),
        }
    }
    Ok(false)
}

/// Whether the session's waiting hook is attached for its current turn.
pub fn hook_attached(caller: Caller) -> Result<bool> {
    match decode(send_request(caller, TalkCommand::HookStatus)?)? {
        TalkResponse::Hook { attached } => Ok(attached),
        _ => bail!("Unexpected talk response"),
    }
}

impl From<Grant> for TalkGrant {
    fn from(grant: Grant) -> Self {
        Self {
            id: grant.id,
            agent: grant.agent,
            box_id: grant.box_id,
            scope: grant.scope.as_str().into(),
            created_at: grant.created_at,
            duration_ms: grant.duration_ms,
            approved_at: grant.approved_at,
            revoked_at: grant.revoked_at,
            request: grant.request.map(|id| id.0),
        }
    }
}

impl TryFrom<TalkGrant> for Grant {
    type Error = anyhow::Error;

    fn try_from(grant: TalkGrant) -> Result<Self> {
        Ok(Self {
            id: grant.id,
            agent: grant.agent,
            box_id: grant.box_id,
            scope: GrantScope::parse(&grant.scope).map_err(anyhow::Error::msg)?,
            created_at: grant.created_at,
            duration_ms: grant.duration_ms,
            approved_at: grant.approved_at,
            revoked_at: grant.revoked_at,
            request: grant.request.map(MessageId),
        })
    }
}

fn grant_list(caller: Caller, command: TalkCommand) -> Result<Vec<Grant>> {
    match decode(send_request(caller, command)?)? {
        TalkResponse::Grants { grants } => grants.into_iter().map(Grant::try_from).collect(),
        _ => bail!("Unexpected talk response"),
    }
}

/// Grants `agent` authority to relay the user's decisions in `scope`. Returns the grant
/// and, from a shell, the decision request asking the user to approve it in the desktop.
pub fn grant(
    caller: Caller,
    agent: String,
    scope: GrantScope,
    duration_ms: u64,
) -> Result<(Grant, Vec<Message>)> {
    let mut grants = grant_list(
        caller.clone(),
        TalkCommand::Grant {
            agent,
            scope: scope.as_str().into(),
            duration_ms,
        },
    )?;
    let grant = grants.pop().context("Power returned no grant")?;
    let request = match &grant.request {
        Some(id) => show(caller, id.clone(), false)?,
        None => Vec::new(),
    };
    Ok((grant, request))
}

pub fn revoke(caller: Caller, id: String) -> Result<Grant> {
    grant_list(caller, TalkCommand::Revoke { id })?
        .pop()
        .context("Power returned no grant")
}

pub fn grants(caller: Caller) -> Result<Vec<Grant>> {
    grant_list(caller, TalkCommand::Grants)
}

pub fn grant_json(grant: &Grant, now: u64) -> serde_json::Value {
    let mut value =
        serde_json::to_value(TalkGrant::from(grant.clone())).expect("talk DTO is serializable");
    value["state"] = grant.state(now).into();
    value["expires_at"] = grant.expires_at().into();
    value
}
