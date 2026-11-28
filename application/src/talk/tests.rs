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
    grants: Vec<Grant>,
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
                finished: false,
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
    fn grants(&self, agent: Option<&str>) -> Result<Vec<Grant>> {
        Ok(self
            .grant_rows()
            .into_iter()
            .filter(|grant| agent.is_none_or(|agent| grant.agent == agent))
            .collect())
    }
    fn grant_for_request(&self, request: &MessageId) -> Result<Option<Grant>> {
        Ok(self
            .grant_rows()
            .into_iter()
            .find(|grant| grant.request.as_ref() == Some(request)))
    }
    fn decisions(&self, all: bool, now: u64) -> Result<Vec<Message>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .messages
            .iter()
            .filter(|message| {
                message
                    .decision
                    .as_ref()
                    .is_some_and(|decision| all || decision.is_open(now))
            })
            .cloned()
            .collect())
    }
}

impl Db {
    fn grant_rows(&self) -> Vec<Grant> {
        self.0.lock().unwrap().grants.clone()
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
    fn answer(&self, request: &MessageId, at: u64) -> Result<bool> {
        for stored in &mut self.0.lock().unwrap().messages {
            if &stored.id == request
                && let Some(decision) = &mut stored.decision
                && decision.answered_at.is_none()
            {
                decision.answered_at = Some(at);
                return Ok(true);
            }
        }
        Ok(false)
    }
    fn insert_grant(&self, grant: &Grant) -> Result<()> {
        self.0.lock().unwrap().grants.push(grant.clone());
        Ok(())
    }
    fn approve_grant(&self, id: &str, at: u64) -> Result<bool> {
        let mut rows = self.0.lock().unwrap();
        let grant = rows.grants.iter_mut().find(|grant| grant.id == id);
        Ok(match grant {
            Some(grant) if grant.approved_at.is_none() && grant.revoked_at.is_none() => {
                grant.approved_at = Some(at);
                true
            }
            _ => false,
        })
    }
    fn revoke_grant(&self, id: &str, at: u64) -> Result<bool> {
        let mut rows = self.0.lock().unwrap();
        let grant = rows.grants.iter_mut().find(|grant| grant.id == id);
        Ok(match grant {
            Some(grant) if grant.revoked_at.is_none() => {
                grant.revoked_at = Some(at);
                true
            }
            _ => false,
        })
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
        verified: true,
    }
}

/// A caller whose session, or lack of one, the daemon could not verify.
fn unverified(session: Option<&str>) -> Caller {
    Caller {
        verified: false,
        ..caller(session)
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
            scope: None,
            choices: Vec::new(),
            expires_in: None,
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
        scope: None,
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
            choices: Vec::new(),
            scope: None,
            expires_in: None,
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
        scope: None,
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

#[test]
fn only_the_user_answers_a_decision_request_and_only_once() -> Result<()> {
    let store = Arc::new(Db::default());
    let terminal = Arc::new(Terminal::default());
    let talk = talk(&store, &terminal);
    let ask = |target, choices: &[&str]| {
        talk.send(SendMessage {
            caller: caller(Some("a-session")),
            target,
            run: None,
            body: "Ship it?".into(),
            urgent: false,
            scope: None,
            choices: choices.iter().map(|c| (*c).into()).collect(),
            expires_in: Some(60_000),
        })
    };
    assert!(ask(Target::One(Address::Agent("b".into())), &["Approve"]).is_err());
    assert!(ask(Target::One(Address::User), &[]).is_err());
    let request = ask(Target::One(Address::User), &["Approve", "Deny"])?.remove(0);
    assert_eq!(talk.decisions(caller(None), false)?.len(), 1);
    assert!(talk.decisions(caller(Some("b-session")), false).is_err());

    let answer = |session: Option<&str>| {
        talk.reply(Reply {
            caller: Caller {
                desktop: session.is_none(),
                ..caller(session)
            },
            message: request.id.clone(),
            recipient_agent: None,
            body: "Approve".into(),
            urgent: false,
            scope: None,
        })
    };
    let peer = answer(Some("b-session")).unwrap_err();
    assert!(format!("{peer:#}").contains("Only the user"));
    // A shell has the user's authority, but nothing shows the user wrote its answer.
    let shell = talk
        .reply(Reply {
            caller: caller(None),
            message: request.id.clone(),
            recipient_agent: None,
            body: "Approve".into(),
            urgent: false,
            scope: None,
        })
        .unwrap_err();
    assert!(format!("{shell:#}").contains("Only the user"));
    assert_eq!(talk.decisions(caller(None), false)?.len(), 1);
    let reply = answer(None)?;
    assert_eq!(reply.sender, Sender::User);
    assert_eq!(reply.reply_to, Some(request.id.clone()));
    assert!(answer(None).is_err());
    assert!(talk.decisions(caller(None), false)?.is_empty());
    assert_eq!(talk.decisions(caller(None), true)?.len(), 1);
    Ok(())
}

fn hooked(session: &str) -> Caller {
    caller(Some(session))
}

#[test]
fn the_first_turn_start_reserves_the_hook_path_for_urgent_messages() -> Result<()> {
    let store = Arc::new(Db::default());
    let terminal = Arc::new(Terminal::default());
    let talk = talk(&store, &terminal);
    // No Stop ever primed the session. Its first turn start registers it.
    talk.hook_turn_start(&hooked("b-session"))?;
    talk.send(SendMessage {
        caller: caller(None),
        target: Target::One(Address::Agent("b".into())),
        run: None,
        body: "now".into(),
        urgent: true,
        scope: None,
        choices: Vec::new(),
        expires_in: None,
    })?;
    talk.deliver_pending()?;
    assert!(terminal.typed.lock().unwrap().is_empty());
    let claim = talk.hook_turn_end(&hooked("b-session"))?.expect("claim");
    assert_eq!(claim.messages[0].body, "now");
    Ok(())
}

#[test]
fn a_quiet_running_turn_past_busy_stale_keeps_the_hook_path() -> Result<()> {
    let store = Arc::new(Db::default());
    let terminal = Arc::new(Terminal::default());
    let talk = Talk::new(
        store.clone(),
        terminal.clone(),
        terminal.clone(),
        NoticePolicy {
            busy_stale_ms: 1,
            ..NoticePolicy::default()
        },
        || 20_000,
        uuid,
    );
    talk.hook_turn_start(&hooked("b-session"))?;
    send(&talk, caller(None), "later")?;
    // The busy report is stale, but no keystroke interrupted the turn: it still runs.
    talk.deliver_pending()?;
    assert!(terminal.typed.lock().unwrap().is_empty());
    // A user interrupt (keystrokes after the turn start, then quiet) hands back to notices.
    terminal.activity.lock().unwrap().last_input = 1;
    talk.deliver_pending()?;
    assert_eq!(terminal.typed.lock().unwrap().len(), 1);
    Ok(())
}

fn uuid() -> String {
    static ID: AtomicU64 = AtomicU64::new(1_000);
    ID.fetch_add(1, Ordering::Relaxed).to_string()
}

#[test]
fn an_async_wake_stays_unread_until_its_turn_starts() -> Result<()> {
    let store = Arc::new(Db::default());
    let terminal = Arc::new(Terminal::default());
    let talk = talk(&store, &terminal);
    let b = hooked("b-session");
    assert!(talk.hook_turn_end(&b)?.is_none());
    let lease = talk.attach_hook(&b, Instant::now())?.expect("lease");
    let sent = send(&talk, caller(None), "wake")?;
    let claim = match talk.hook_wait(&lease, &b, Instant::now())? {
        HookWait::Claimed(claim) => claim,
        _ => panic!("no claim"),
    };
    // The daemon accepts the acknowledgment, then the waiter dies or its reply is lost.
    talk.hook_confirm(claim)?;
    drop(lease);
    let unread = talk.inbox(hooked("b-session"), false, true)?;
    assert_eq!(unread[0].id, sent.id);
    assert_eq!(unread[0].delivery.read_at, None);
    // Nothing claims it twice, and no notice types it while the wake may still land.
    assert!(talk.hook_turn_end(&b)?.is_none());
    talk.deliver_pending()?;
    assert!(terminal.typed.lock().unwrap().is_empty());
    // The woken turn starts: now the agent has it.
    talk.hook_turn_start(&b)?;
    let all = talk.inbox(b, true, true)?;
    assert_eq!(all[0].delivery.read_at, Some(20_000));
    Ok(())
}

#[test]
fn an_unconsumed_async_wake_returns_to_the_queue() -> Result<()> {
    static NOW: AtomicU64 = AtomicU64::new(20_000);
    let store = Arc::new(Db::default());
    let terminal = Arc::new(Terminal::default());
    let talk = Talk::new(
        store.clone(),
        terminal.clone(),
        terminal.clone(),
        NoticePolicy::default(),
        || NOW.load(Ordering::Relaxed),
        uuid,
    );
    let b = hooked("b-session");
    talk.hook_turn_end(&b)?;
    let lease = talk.attach_hook(&b, Instant::now())?.expect("lease");
    send(&talk, caller(None), "lost")?;
    let HookWait::Claimed(claim) = talk.hook_wait(&lease, &b, Instant::now())? else {
        panic!("no claim");
    };
    talk.hook_confirm(claim)?;
    drop(lease);
    NOW.fetch_add(WAKE_CONSUME_MS, Ordering::Relaxed);
    talk.deliver_pending()?;
    assert_eq!(terminal.typed.lock().unwrap().len(), 1);
    // A later turn start does not mark the returned message read.
    talk.hook_turn_start(&b)?;
    assert_eq!(talk.inbox(b, false, true)?.len(), 1);
    Ok(())
}

#[test]
fn a_late_confirmation_never_changes_a_newer_turn() -> Result<()> {
    let store = Arc::new(Db::default());
    let terminal = Arc::new(Terminal::default());
    let talk = talk(&store, &terminal);
    let b = hooked("b-session");
    talk.hook_turn_start(&b)?;
    send(&talk, caller(None), "held")?;
    let claim = talk.hook_turn_end(&b)?.expect("claim");
    // The claim stays open across the next Busy/Stop pair.
    talk.hook_turn_start(&b)?;
    assert!(talk.hook_turn_end(&b)?.is_none());
    let lease = talk.attach_hook(&b, Instant::now())?.expect("lease");
    talk.hook_confirm(claim)?;
    assert_eq!(terminal.activity.lock().unwrap().hook, HookState::Ready);
    assert!(!lease.stale());
    assert!(talk.hook_attached(&b)?);
    Ok(())
}

fn scoped_send(
    talk: &Talk<Db, Arc<Terminal>, Arc<Terminal>>,
    scope: Option<GrantScope>,
) -> Result<Message> {
    scoped_send_as(talk, caller(Some("a-session")), scope)
}

fn scoped_send_as(
    talk: &Talk<Db, Arc<Terminal>, Arc<Terminal>>,
    caller: Caller,
    scope: Option<GrantScope>,
) -> Result<Message> {
    Ok(talk
        .send(SendMessage {
            caller,
            target: Target::One(Address::Agent("b".into())),
            run: None,
            body: "Cap reviews at 2 rounds".into(),
            urgent: false,
            scope,
            choices: Vec::new(),
            expires_in: None,
        })?
        .remove(0))
}

#[test]
fn only_the_user_grants_and_relays_carry_a_grant_only_inside_its_scope() -> Result<()> {
    let store = Arc::new(Db::default());
    let terminal = Arc::new(Terminal::default());
    let talk = talk(&store, &terminal);
    let request = |caller, scope| GrantRequest {
        caller,
        agent: "a".into(),
        scope,
        duration_ms: 60_000,
    };
    let error = talk
        .grant(request(caller(Some("b-session")), GrantScope::DagPolicy))
        .err()
        .unwrap();
    assert!(error.to_string().contains("Only the user"), "{error:#}");
    assert!(store.grant_rows().is_empty());

    // A grant from the user's shell waits for the desktop.
    let pending = talk.grant(request(caller(None), GrantScope::DagPolicy))?;
    assert_eq!(pending.grant.state(20_000), "pending");
    let ask = pending.request.unwrap();
    assert!(ask.decision.is_some() && ask.recipient == Recipient::User);
    assert_eq!(scoped_send(&talk, Some(GrantScope::DagPolicy))?.grant, None);
    // The shell cannot approve its own grant.
    let approve = |desktop| {
        talk.reply(Reply {
            caller: Caller {
                session: None,
                box_id: Some("box".into()),
                desktop,
                verified: true,
            },
            message: ask.id.clone(),
            recipient_agent: None,
            body: APPROVE_GRANT.into(),
            urgent: false,
            scope: None,
        })
    };
    assert!(approve(false).is_err());
    approve(true)?;
    let relayed = scoped_send(&talk, Some(GrantScope::DagPolicy))?;
    assert_eq!(
        relayed.grant,
        Some(GrantRelay {
            id: pending.grant.id.clone(),
            scope: GrantScope::DagPolicy
        })
    );
    assert!(domain::talk::envelope(&relayed).contains(&format!(
        "Relayed with user grant {}, scope dag-policy",
        pending.grant.id
    )));
    assert_eq!(scoped_send(&talk, Some(GrantScope::DagMerge))?.grant, None);
    assert_eq!(scoped_send(&talk, None)?.grant, None);
    // A request that only claims the granted agent's session is never stamped.
    assert_eq!(
        scoped_send_as(
            &talk,
            unverified(Some("a-session")),
            Some(GrantScope::DagPolicy)
        )?
        .grant,
        None
    );
    // Without verified attribution, a caller without a session is not the user's shell.
    let error = talk
        .grant(request(unverified(None), GrantScope::DagPolicy))
        .err()
        .unwrap();
    assert!(error.to_string().contains("Only the user"), "{error:#}");
    assert!(talk.revoke(unverified(None), &pending.grant.id).is_err());

    // An agent cannot revoke, the user can, at once.
    assert!(
        talk.revoke(caller(Some("a-session")), &pending.grant.id)
            .is_err()
    );
    talk.revoke(caller(None), &pending.grant.id)?;
    assert_eq!(scoped_send(&talk, Some(GrantScope::DagPolicy))?.grant, None);
    // Grant changes are recorded in talk history, for the user and the agent.
    let history: Vec<_> = store
        .0
        .lock()
        .unwrap()
        .messages
        .iter()
        .filter(|m| m.sender == Sender::System && m.decision.is_none())
        .map(|m| m.body.clone())
        .collect();
    assert!(history.iter().any(|body| body.contains("approved")));
    assert!(history.iter().any(|body| body.contains("revoked")));
    Ok(())
}
