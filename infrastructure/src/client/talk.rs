use crate::wire::{self, Request, Response, talk::*};
use anyhow::{Context, Result, bail};
use application::talk::{LogFilter, TalkEvent as Event, TalkEventKind as EventKind};
use domain::talk::{
    AgentRef, AgentTalkSummary, Delivery, Message, MessageId, Recipient, Sender, ThreadId,
};
use std::{io::BufReader, os::unix::net::UnixStream, time::Duration};

#[derive(Clone, Default)]
pub struct Caller {
    pub session: Option<String>,
    pub box_sel: Option<String>,
    pub desktop: bool,
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

fn messages(caller: Caller, command: TalkCommand) -> Result<Vec<Message>> {
    match decode(wire::request(&request(caller, command))?)? {
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
    messages(
        caller,
        TalkCommand::Send {
            to,
            run,
            body,
            urgent,
        },
    )
}

pub fn reply(
    caller: Caller,
    id: MessageId,
    body: String,
    recipient_agent: Option<String>,
) -> Result<Message> {
    messages(
        caller,
        TalkCommand::Reply {
            id: id.0,
            body,
            urgent: false,
            recipient_agent,
        },
    )?
    .pop()
    .context("Daemon returned no reply")
}

pub fn summary(caller: Caller) -> Result<Vec<AgentTalkSummary>> {
    match decode(wire::request(&request(caller, TalkCommand::Summary))?)? {
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
    match decode(wire::request(&request(
        caller,
        TalkCommand::Turn { ready, ready_only },
    ))?)? {
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
    let mut socket = wire::connect()?;
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
