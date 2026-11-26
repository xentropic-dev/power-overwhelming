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

struct Db(Mutex<Rows>);

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
    fn mark(&self, messages: &[&Message], change: DeliveryChange, at: u64) -> Result<()> {
        for stored in &mut self.0.lock().unwrap().messages {
            if messages.iter().any(|message| message.id == stored.id) {
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
    fn activity(&self, _: &str) -> Option<Activity> {
        Some(*self.activity.lock().unwrap())
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
    }
}

#[test]
fn queued_peer_and_user_messages_follow_delivery_and_read_rules() -> Result<()> {
    let store = Arc::new(Db(Mutex::default()));
    let terminal = Arc::new(Terminal::default());
    let talk = Talk::new(
        store.clone(),
        terminal.clone(),
        terminal.clone(),
        NoticePolicy::default(),
        || 20_000,
        || {
            static ID: AtomicU64 = AtomicU64::new(1);
            ID.fetch_add(1, Ordering::Relaxed).to_string()
        },
    );
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
    assert!(lines[0].contains("from the user"));
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
            None,
            Instant::now()
        )?,
        Some(reply)
    );
    Ok(())
}
