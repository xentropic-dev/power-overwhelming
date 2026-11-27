use crate::{
    dag_adapter::DaemonDag,
    dag_store::SqliteDagStore,
    terminal::PtyRuntime,
    wire::{
        self, Response,
        talk::{TalkCommand, TalkRequest, TalkResponse},
    },
};
use anyhow::{Context, Result, bail};
use application::{
    Application,
    dag::BoxSelection,
    talk::{Caller, LogFilter, Notifier, Reply, SendMessage, SessionActivity, Talk},
};
use domain::session::Agent;
use domain::talk::{
    Activity, Address, HookState, Message, MessageId, NoticePolicy, Target, ThreadId,
};
use std::{
    os::unix::net::UnixStream,
    sync::{Arc, mpsc::Receiver},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub(crate) type DaemonTalk = Talk<SqliteDagStore, Terminal, Terminal>;

#[derive(Clone)]
pub(crate) struct Terminal(Arc<Application<PtyRuntime>>);

impl SessionActivity for Terminal {
    fn activity(&self, session: &str) -> Option<Activity> {
        let terminal = self.0.terminal(session).ok()?;
        terminal.is_running().then(|| terminal.activity())
    }

    fn agent(&self, session: &str) -> Option<Agent> {
        self.0.live_agent(session)
    }

    fn report_turn(&self, session: &str, state: HookState, ready_only: bool) -> Result<()> {
        self.0.terminal(session)?.report_turn(state, ready_only);
        Ok(())
    }
}

impl Notifier for Terminal {
    fn notify(&self, session: &str, expected: Activity, line: &str) -> Result<bool> {
        self.0
            .terminal(session)?
            .input(format!("{line}\r").into_bytes(), Some(expected))
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(crate) fn start(
    application: Arc<Application<PtyRuntime>>,
    store: SqliteDagStore,
    policy: NoticePolicy,
    wake: Receiver<()>,
) -> Arc<DaemonTalk> {
    let terminal = Terminal(application);
    let talk = Arc::new(Talk::new(
        Arc::new(store),
        terminal.clone(),
        terminal,
        policy,
        now_ms,
        || uuid::Uuid::new_v4().to_string(),
    ));
    let delivery = talk.clone();
    thread::spawn(move || {
        loop {
            if matches!(
                wake.recv_timeout(Duration::from_millis(50)),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
            ) {
                break;
            }
            if let Err(error) = delivery.deliver_pending() {
                eprintln!("Could not deliver talk notice. {error:#}");
            }
        }
    });
    talk
}

fn caller(
    dag: &DaemonDag,
    session: Option<String>,
    selector: Option<String>,
    desktop: bool,
) -> Result<Caller> {
    let box_id = if selector.is_some() {
        dag.box_for(BoxSelection {
            selector,
            caller: None,
        })?
    } else if session.is_some() {
        // Plain sessions can exist without an attached box. Managed sessions get their
        // authoritative box from the talk store when resolving the caller.
        dag.box_for(BoxSelection {
            selector: None,
            caller: session.clone(),
        })
        .ok()
        .flatten()
    } else {
        None
    };
    Ok(Caller {
        session,
        box_id,
        desktop,
    })
}

pub(crate) fn request(
    talk: &DaemonTalk,
    dag: &DaemonDag,
    request: TalkRequest,
) -> Result<TalkResponse> {
    if let Some(session) = &request.session
        && !matches!(request.command, TalkCommand::Turn { .. })
    {
        // A coding session is a peer, never the user, even without a managed-agent row.
        dag.register_coding_session(session)?;
    }
    let caller = caller(dag, request.session, request.box_sel, request.desktop)?;
    let items = |messages: Vec<Message>| TalkResponse::Messages {
        messages: messages.into_iter().map(Into::into).collect(),
    };
    Ok(match request.command {
        TalkCommand::Send {
            to,
            run,
            body,
            urgent,
        } => {
            let target = match (to, &run) {
                (Some(to), _) => Target::One(Address::from(to.as_str())),
                (None, Some(run)) => Target::Run(run.clone()),
                (None, None) => bail!("Specify a talk target or --run"),
            };
            items(talk.send(SendMessage {
                caller,
                target,
                run,
                body,
                urgent,
            })?)
        }
        TalkCommand::Reply {
            id,
            body,
            urgent,
            recipient_agent,
        } => items(vec![talk.reply(Reply {
            caller,
            message: MessageId(id),
            recipient_agent,
            body,
            urgent,
        })?]),
        TalkCommand::Inbox { all, peek } => items(talk.inbox(caller, all, peek)?),
        TalkCommand::Show { id, read } => items(talk.show(caller, MessageId(id), read)?),
        TalkCommand::Wait {
            threads,
            reply_to,
            seconds,
        } => {
            let deadline = Instant::now()
                .checked_add(Duration::from_secs(seconds))
                .context("Talk timeout is too large")?;
            TalkResponse::Reply {
                message: talk
                    .wait_for_reply(
                        caller,
                        threads.into_iter().map(ThreadId).collect(),
                        reply_to.into_iter().map(MessageId).collect(),
                        deadline,
                    )?
                    .map(|message| Box::new(message.into())),
            }
        }
        TalkCommand::Log {
            agent,
            run,
            limit,
            to_user,
        } => items(talk.log(
            caller,
            LogFilter {
                agent,
                run,
                limit,
                to_user,
                ..LogFilter::default()
            },
        )?),
        TalkCommand::Turn { ready, ready_only } => {
            talk.report_turn(
                caller,
                if ready {
                    HookState::Ready
                } else {
                    HookState::Busy
                },
                ready_only,
            )?;
            TalkResponse::Ok
        }
        TalkCommand::Summary => TalkResponse::Summary {
            agents: talk.summary(caller)?.into_iter().map(Into::into).collect(),
        },
        TalkCommand::HookPending { turn_ended } => items(talk.hook_pending(&caller, turn_ended)?),
        TalkCommand::HookDelivered { ids } => items(
            talk.hook_delivered(&caller, &ids.into_iter().map(MessageId).collect::<Vec<_>>())?,
        ),
        TalkCommand::Watch | TalkCommand::HookWait => {
            unreachable!("Talk watches use the streaming handler")
        }
    })
}

/// Holds the session's hook waiter until messages arrive or the client goes away. A
/// keepalive every few seconds detects a client that died, which hands delivery back to
/// terminal notices.
pub(crate) fn hook_wait(
    talk: &DaemonTalk,
    dag: &DaemonDag,
    request: TalkRequest,
    mut socket: &UnixStream,
) -> Result<()> {
    let caller = caller(dag, request.session, request.box_sel, request.desktop)?;
    let lease = talk.attach_hook(&caller)?;
    let ok = || Response::Talk {
        talk: TalkResponse::Ok,
    };
    wire::send(&mut socket, &ok())?;
    loop {
        let messages = talk.hook_wait(&lease, &caller, Instant::now() + Duration::from_secs(5))?;
        if lease.superseded() {
            return Ok(());
        }
        if !messages.is_empty() {
            return wire::send(
                &mut socket,
                &Response::Talk {
                    talk: TalkResponse::Messages {
                        messages: messages.into_iter().map(Into::into).collect(),
                    },
                },
            );
        }
        wire::send(&mut socket, &ok())?;
    }
}

pub(crate) fn watch(
    talk: &DaemonTalk,
    dag: &DaemonDag,
    request: TalkRequest,
    mut socket: &UnixStream,
) -> Result<()> {
    let caller = caller(dag, request.session, request.box_sel, request.desktop)?;
    let mut cursor = talk.cursor()?;
    wire::send(
        &mut socket,
        &Response::Talk {
            talk: TalkResponse::Ok,
        },
    )?;
    loop {
        let events = talk.events(&caller, cursor, Instant::now() + Duration::from_secs(10))?;
        if events.is_empty() {
            wire::send(
                &mut socket,
                &Response::Talk {
                    talk: TalkResponse::Ok,
                },
            )?;
        }
        for event in events {
            cursor = event.seq;
            wire::send(
                &mut socket,
                &Response::Talk {
                    talk: TalkResponse::Event {
                        event: event.into(),
                    },
                },
            )?;
        }
    }
}
