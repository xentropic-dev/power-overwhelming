use super::*;
use domain::dag::{AgentState, Provider};
use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};

// The transaction methods take &self to match SQLite. Mutations use a separate backing lock.
#[derive(Clone, Default)]
struct Rows {
    messages: Vec<Message>,
}

#[derive(Default)]
struct Db(Mutex<Rows>, AtomicU64);

impl Db {
    fn writes(&self) -> u64 {
        self.1.load(Ordering::Relaxed)
    }
}

fn agent(id: &str, status: AgentStatus) -> ManagedAgent {
    ManagedAgent {
        id: id.into(),
        box_id: "box".into(),
        worktree: ".".into(),
        provider: Provider::Command,
        model: None,
        parent: Some("coordinator".into()),
        session: Some(format!("{id}-session")),
        run: Some("run".into()),
        task: Some(id.into()),
        attempt: Some(id.into()),
        state: AgentState::Running,
        paused: false,
        status,
        label: id.into(),
        created_at: 0,
    }
}

impl TalkReader for Db {
    fn directory(&self) -> Result<Directory> {
        Ok(Directory {
            agents: vec![
                agent("a", AgentStatus::Working),
                agent("b", AgentStatus::NeedsInput),
            ],
            runs: vec![domain::talk::RunRef {
                id: "run".into(),
                box_id: "box".into(),
                coordinator: Coordinator::External {
                    label: "pane".into(),
                },
                coordinator_session: None,
            }],
        })
    }
    fn message(&self, id: &MessageId) -> Result<Option<Message>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .messages
            .iter()
            .find(|message| &message.id == id)
            .cloned())
    }
    fn inbox(&self, recipient: &Recipient, _: Option<&str>, all: bool) -> Result<Vec<Message>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .messages
            .iter()
            .filter(|message| {
                &message.recipient == recipient && (all || message.delivery.read_at.is_none())
            })
            .cloned()
            .collect())
    }
    fn summary(&self, _: &str) -> Result<Vec<AgentTalkSummary>> {
        unreachable!()
    }
    fn pending_agents(&self, _: u64) -> Result<Vec<String>> {
        Ok(vec!["b".into()])
    }
    fn queued(&self, recipient: &Recipient, _: &str) -> Result<Vec<Queued>> {
        Ok(self
            .inbox(recipient, None, false)?
            .iter()
            .map(Message::queued)
            .collect())
    }
    fn agent(&self, id: &str) -> Result<ManagedAgent> {
        self.directory()?
            .agents
            .into_iter()
            .find(|agent| agent.id == id)
            .context("Unknown agent")
    }
    fn thread(&self, id: &ThreadId) -> Result<Vec<Message>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .messages
            .iter()
            .filter(|message| &message.thread == id)
            .cloned()
            .collect())
    }
    fn log(&self, _: &LogFilter) -> Result<Vec<Message>> {
        unreachable!()
    }
    fn recent_sends(&self, _: &Sender, _: &Recipient, _: u64) -> Result<Vec<u64>> {
        Ok(vec![])
    }
    fn cursor(&self) -> Result<u64> {
        Ok(0)
    }
    fn events(&self, _: u64, _: Option<&str>) -> Result<Vec<TalkEvent>> {
        unreachable!()
    }
}

impl TalkTransaction for Db {
    fn insert(&self, message: &Message) -> Result<()> {
        self.0.lock().unwrap().messages.push(message.clone());
        Ok(())
    }
    fn mark(&self, messages: &[&MessageId], change: DeliveryChange, at: u64) -> Result<()> {
        for stored in &mut self.0.lock().unwrap().messages {
            if messages.contains(&&stored.id) {
                match change {
                    DeliveryChange::Notified => stored.delivery.notified_at = Some(at),
                    DeliveryChange::Reminded => stored.delivery.reminded_at = Some(at),
                    DeliveryChange::Read => stored.delivery.read_at = Some(at),
                }
            }
        }
        Ok(())
    }
}

impl TalkStore for Db {
    fn read<T>(&self, query: impl FnOnce(&dyn TalkReader) -> Result<T>) -> Result<T> {
        query(self)
    }
    fn write<T>(&self, operation: impl FnOnce(&dyn TalkTransaction) -> Result<T>) -> Result<T> {
        self.1.fetch_add(1, Ordering::Relaxed);
        operation(self)
    }
    fn wait_for_change(&self, _: u64, _: Instant) {
        unreachable!()
    }
}

#[derive(Default)]
struct Terminal {
    activity: Mutex<Activity>,
    typed: Mutex<Vec<String>>,
}

impl SessionActivity for Arc<Terminal> {
    fn activity(&self, session: &str) -> Option<Activity> {
        // Agent sessions and one plain shell session are live.
        ["a-session", "b-session", "shell-session"]
            .contains(&session)
            .then(|| *self.activity.lock().unwrap())
    }
    fn agent(&self, session: &str) -> Option<Agent> {
        match session {
            "a-session" | "b-session" => Some(Agent::Coding("fixture".into())),
            "shell-session" => Some(Agent::Shell),
            "coding-session" => Some(Agent::Coding("codex".into())),
            _ => None,
        }
    }
    fn report_turn(&self, _: &str, hook: HookState, _: bool) -> Result<()> {
        self.activity.lock().unwrap().hook = hook;
        Ok(())
    }
}

impl Notifier for Arc<Terminal> {
    fn notify(&self, _: &str, _: Activity, line: &str) -> Result<bool> {
        self.typed.lock().unwrap().push(line.into());
        Ok(true)
    }
}

fn caller(session: Option<&str>) -> Caller {
    Caller {
        session: session.map(str::to_owned),
        box_id: Some("box".into()),
        desktop: false,
    }
}

fn talk(store: &Arc<Db>, terminal: &Arc<Terminal>) -> Talk<Db, Arc<Terminal>, Arc<Terminal>> {
    Talk::new(
        store.clone(),
        terminal.clone(),
        terminal.clone(),
        NoticePolicy::default(),
        || 20_000,
        || {
            static ID: AtomicU64 = AtomicU64::new(1);
            ID.fetch_add(1, Ordering::Relaxed).to_string()
        },
    )
}

fn send(
    talk: &Talk<Db, Arc<Terminal>, Arc<Terminal>>,
    caller: Caller,
    body: &str,
) -> Result<Message> {
    Ok(talk
        .send(SendMessage {
            caller,
            target: Target::One(Address::Agent("b".into())),
            run: None,
            body: body.into(),
            urgent: false,
        })?
        .remove(0))
}

fn reply(
    talk: &Talk<Db, Arc<Terminal>, Arc<Terminal>>,
    session: &str,
    to: &Message,
    body: &str,
) -> Result<Message> {
    talk.reply(Reply {
        caller: caller(Some(session)),
        message: to.id.clone(),
        recipient_agent: None,
        body: body.into(),
        urgent: false,
    })
}

#[test]
fn queued_peer_and_user_messages_follow_delivery_and_read_rules() -> Result<()> {
    let store = Arc::new(Db::default());
    let terminal = Arc::new(Terminal::default());
    let talk = talk(&store, &terminal);
    let send = |session, body: &str, urgent| {
        talk.send(SendMessage {
            caller: caller(session),
            target: Target::One(Address::Agent("b".into())),
            run: None,
            body: body.into(),
            urgent,
        })
    };
    assert!(send(Some("a-session"), "no", true).is_err());
    let peer = send(Some("a-session"), "question", false)?.remove(0);
    let user = send(None, "answer", false)?.remove(0);
    talk.report_turn(caller(Some("b-session")), HookState::Busy, false)?;
    talk.deliver_pending()?;
    assert!(terminal.typed.lock().unwrap().is_empty());
    talk.report_turn(caller(Some("b-session")), HookState::Ready, false)?;
    talk.deliver_pending()?;
    talk.deliver_pending()?;
    let lines = terminal.typed.lock().unwrap();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("from the user's shell"));
    drop(lines);

    let unread = talk.inbox(caller(Some("b-session")), false, true)?;
    assert_eq!(unread.len(), 2);
    assert_eq!(unread[0].id, peer.id);
    assert_eq!(unread[0].delivery.notified_at, None);
    assert_eq!(unread[1].id, user.id);
    assert_eq!(unread[1].delivery.notified_at, Some(20_000));
    let read = talk.inbox(caller(Some("b-session")), false, false)?;
    assert!(
        read.iter()
            .all(|message| message.delivery.read_at == Some(20_000))
    );
    assert!(
        talk.inbox(caller(Some("b-session")), false, true)?
            .is_empty()
    );

    let reply = talk.reply(Reply {
        caller: caller(Some("b-session")),
        message: peer.id.clone(),
        recipient_agent: None,
        body: "reply".into(),
        urgent: false,
    })?;
    assert_eq!(reply.thread, peer.thread);
    assert_eq!(reply.reply_to, Some(peer.id));
    assert_eq!(
        talk.wait_for_reply(
            caller(Some("a-session")),
            vec![peer.thread],
            Vec::new(),
            Instant::now()
        )?,
        Some(reply)
    );
    Ok(())
}

#[test]
fn held_messages_never_open_a_write_transaction() -> Result<()> {
    let store = Arc::new(Db::default());
    let terminal = Arc::new(Terminal::default());
    let talk = talk(&store, &terminal);
    // Agent b needs input, so a peer message is held however idle the terminal is.
    send(&talk, caller(Some("a-session")), "held")?;
    let before = store.writes();
    // Three seconds of the daemon's 50 ms delivery loop.
    for _ in 0..60 {
        talk.deliver_pending()?;
    }
    assert_eq!(store.writes(), before);
    talk.report_turn(caller(Some("b-session")), HookState::Busy, false)?;
    send(&talk, caller(None), "held while busy")?;
    let before = store.writes();
    for _ in 0..60 {
        talk.deliver_pending()?;
    }
    assert_eq!(store.writes(), before);
    assert!(terminal.typed.lock().unwrap().is_empty());

    // A deliverable batch takes one write that stamps it, and then nothing is pending.
    talk.report_turn(caller(Some("b-session")), HookState::Ready, false)?;
    talk.deliver_pending()?;
    assert_eq!(store.writes(), before + 1);
    assert_eq!(terminal.typed.lock().unwrap().len(), 1);
    for _ in 0..60 {
        talk.deliver_pending()?;
    }
    assert_eq!(store.writes(), before + 1);
    Ok(())
}

#[test]
fn sessions_without_an_agent_need_a_live_session_and_send_as_the_shell() -> Result<()> {
    let store = Arc::new(Db::default());
    let terminal = Arc::new(Terminal::default());
    let talk = talk(&store, &terminal);
    let error = send(&talk, caller(Some("no-such-session")), "spoof").unwrap_err();
    assert!(
        error.to_string().contains("Unknown Power session"),
        "{error}"
    );
    assert!(
        talk.inbox(caller(Some("no-such-session")), false, true)
            .is_err()
    );
    assert!(store.0.lock().unwrap().messages.is_empty());

    assert_eq!(
        send(&talk, caller(Some("shell-session")), "from a tab")?.sender,
        Sender::Shell
    );
    assert_eq!(
        send(&talk, caller(None), "from a script")?.sender,
        Sender::Shell
    );
    let desktop = Caller {
        desktop: true,
        ..caller(None)
    };
    assert_eq!(
        send(&talk, desktop, "from the desktop")?.sender,
        Sender::User
    );
    // The desktop flag never applies to a session.
    let claimed = Caller {
        desktop: true,
        ..caller(Some("shell-session"))
    };
    assert_eq!(send(&talk, claimed, "claimed")?.sender, Sender::Shell);
    // A live coding session is never the user. Without a peer identity it cannot send.
    let error = send(&talk, caller(Some("coding-session")), "as the user?").unwrap_err();
    assert!(
        error.to_string().contains("has no Power agent identity"),
        "{error}"
    );
    assert!(
        talk.inbox(caller(Some("coding-session")), false, true)
            .is_err()
    );
    Ok(())
}

#[test]
fn waits_accept_only_a_reply_to_the_requested_message() -> Result<()> {
    let store = Arc::new(Db::default());
    let terminal = Arc::new(Terminal::default());
    let talk = talk(&store, &terminal);
    let first = send(&talk, caller(Some("a-session")), "first")?;
    let second = reply(&talk, "a-session", &first, "second")?;
    assert_eq!(second.thread, first.thread);
    let to_first = reply(&talk, "b-session", &first, "answer to first")?;
    let wait = |reply_to: &Message| {
        talk.wait_for_reply(
            caller(Some("a-session")),
            vec![first.thread.clone()],
            vec![reply_to.id.clone()],
            Instant::now(),
        )
    };
    assert_eq!(wait(&second)?, None);
    assert_eq!(wait(&first)?, Some(to_first));
    let to_second = reply(&talk, "b-session", &second, "answer to second")?;
    assert_eq!(wait(&second)?, Some(to_second));
    // The requested message must be the caller's own message in the threads.
    assert!(
        talk.wait_for_reply(
            caller(Some("b-session")),
            vec![first.thread.clone()],
            vec![second.id.clone()],
            Instant::now(),
        )
        .is_err()
    );
    Ok(())
}
