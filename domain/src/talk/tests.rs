use super::*;
use crate::dag::{AgentState, Provider};

fn agent(id: &str, box_id: &str, task: Option<&str>, status: AgentStatus) -> ManagedAgent {
    ManagedAgent {
        id: id.into(),
        box_id: box_id.into(),
        worktree: ".".into(),
        provider: Provider::Command,
        model: None,
        parent: task.map(|_| "coord".into()),
        session: (status != AgentStatus::Exited).then(|| id.into()),
        run: task.map(|_| "run".into()),
        task: task.map(str::to_owned),
        attempt: None,
        state: if status == AgentStatus::Exited {
            AgentState::Exited
        } else {
            AgentState::Running
        },
        status,
        label: id.into(),
        created_at: 0,
    }
}

fn directory() -> Directory {
    Directory {
        agents: vec![
            agent("coord", "box", None, AgentStatus::Idle),
            agent("worker-a", "box", Some("a"), AgentStatus::Working),
            agent("worker-old", "box", Some("b"), AgentStatus::Exited),
            agent("worker-b", "box", Some("b"), AgentStatus::NeedsInput),
            agent("other", "remote", None, AgentStatus::Idle),
        ],
        runs: vec![RunRef {
            id: "run".into(),
            box_id: "box".into(),
            coordinator: Coordinator::Agent {
                agent: "coord".into(),
            },
        }],
    }
}

#[test]
fn addresses_resolve_exact_aliases_and_current_attempts() {
    let dir = directory();
    let ctx = || SenderContext {
        agent: Some(&dir.agents[1]),
        box_id: None,
        run: None,
    };
    for (input, index) in [
        ("co", 0),
        ("coord", 0),
        ("parent", 0),
        ("coordinator", 0),
        ("task:b", 3),
    ] {
        assert_eq!(
            resolve(ctx(), &Target::One(Address::from(input)), &dir),
            Ok(vec![Recipient::agent(&dir.agents[index])])
        );
    }
    assert_eq!(
        resolve(ctx(), &Target::One(Address::from("user")), &dir),
        Ok(vec![Recipient::User])
    );
    for (input, error) in [
        ("worker", AddressError::Ambiguous("worker".into())),
        ("worker-old", AddressError::Exited("worker-old".into())),
        ("other", AddressError::OtherBox("other".into())),
        ("task:nope", AddressError::Unknown("task:nope".into())),
        ("", AddressError::Unknown("".into())),
    ] {
        assert_eq!(
            resolve(ctx(), &Target::One(Address::from(input)), &dir),
            Err(error)
        );
    }
    let user = || SenderContext {
        agent: None,
        box_id: Some("box"),
        run: Some("run"),
    };
    assert_eq!(
        resolve(user(), &Target::One(Address::from("other")), &dir),
        Ok(vec![Recipient::agent(&dir.agents[4])])
    );
    assert_eq!(
        resolve(user(), &Target::One(Address::Parent), &dir),
        Err(AddressError::Unknown("parent".into()))
    );
    assert!(resolve(user(), &Target::One(Address::Task("a".into())), &dir).is_ok());
}

#[test]
fn broadcast_excludes_the_sender_and_closed_attempts() {
    let dir = directory();
    let ctx = SenderContext {
        agent: Some(&dir.agents[1]),
        box_id: None,
        run: None,
    };
    assert_eq!(
        resolve(ctx, &Target::Run("run".into()), &dir),
        Ok(vec![Recipient::agent(&dir.agents[3])])
    );
}

#[test]
fn external_coordinator_error_names_the_label() {
    let mut dir = directory();
    dir.runs[0].coordinator = Coordinator::External {
        label: "pane A".into(),
    };
    let ctx = SenderContext {
        agent: Some(&dir.agents[1]),
        box_id: None,
        run: None,
    };
    assert_eq!(
        resolve(ctx, &Target::One(Address::Coordinator), &dir),
        Err(AddressError::ExternalCoordinator("pane A".into()))
    );
}

fn message() -> Message {
    Message {
        id: MessageId("id".into()),
        thread: ThreadId("thread".into()),
        box_id: "box".into(),
        sender: Sender::Agent(AgentRef {
            id: "worker-a".into(),
            box_id: "box".into(),
            provider: "Command".into(),
            role: "worker on task a".into(),
            run: Some("run".into()),
        }),
        recipient: Recipient::Agent {
            id: "coord".into(),
            box_id: "box".into(),
        },
        body: "Question\nNext line".into(),
        reply_to: None,
        broadcast: None,
        run: None,
        created_at: 0,
        delivery: Delivery::default(),
        urgent: false,
    }
}

#[test]
fn trust_limits_peers_and_preserves_message_provenance() {
    let mut msg = message();
    let remote = Recipient::Agent {
        id: "other".into(),
        box_id: "remote".into(),
    };
    assert!(authorize(&msg.sender, &remote).is_err());
    assert!(authorize(&msg.sender, &msg.recipient).is_ok());
    assert!(authorize(&msg.sender, &Recipient::User).is_ok());
    let recent = [1; RATE_LIMIT];
    assert_eq!(rate_limit(&msg.sender, &recent[..19], RATE_WINDOW_MS), None);
    assert_eq!(
        rate_limit(&msg.sender, &recent, RATE_WINDOW_MS),
        Some(RATE_WINDOW_MS + 1)
    );
    assert_eq!(rate_limit(&msg.sender, &recent, RATE_WINDOW_MS + 1), None);

    let framed = envelope(&msg);
    for text in [
        "worker-a",
        "Command",
        "worker on task a",
        "box box",
        "Not from the user.",
        &msg.body,
    ] {
        assert!(framed.contains(text), "{framed:?}");
    }
    let line = notice(&[&msg]);
    assert!(line.contains("Not from the user."));
    assert!(!line.contains("Question"));
    assert_eq!(line.lines().count(), 1);
    for (sender, label) in [
        (Sender::User, "from the user"),
        (Sender::System, "from Power"),
    ] {
        assert!(authorize(&sender, &remote).is_ok());
        assert_eq!(rate_limit(&sender, &recent, RATE_WINDOW_MS), None);
        msg.sender = sender;
        assert!(envelope(&msg).contains(label));
        assert!(notice(&[&msg]).contains(label));
    }
}

#[test]
fn notices_respect_holds_hooks_quiet_and_reminder_limits() {
    use HoldReason::*;
    use NoticeDecision::*;
    let policy = NoticePolicy::default();
    let mut msg = message();
    let idle = Activity::default();
    let busy = Activity {
        hook: HookState::Busy,
        ..idle
    };
    let ready = Activity {
        hook: HookState::Ready,
        ..idle
    };
    let now = REMINDER_AFTER_MS + 20_000;
    assert_eq!(policy.decide(idle, false, false, &msg, now), Notify);
    assert_eq!(policy.decide(idle, false, false, &msg, 9_999), Hold(Busy));
    assert_eq!(
        policy.decide(
            Activity {
                last_output: now - 4_999,
                ..idle
            },
            false,
            false,
            &msg,
            now
        ),
        Hold(Busy)
    );
    assert_eq!(policy.decide(busy, false, false, &msg, now), Hold(Busy));
    assert_eq!(policy.decide(ready, false, false, &msg, 0), Notify);
    assert_eq!(
        policy.decide(idle, true, false, &msg, now),
        Hold(NeedsInput)
    );
    assert_eq!(policy.decide(idle, false, true, &msg, now), Hold(Paused));
    assert_eq!(
        policy.decide(
            Activity {
                restoring: true,
                ..idle
            },
            false,
            false,
            &msg,
            now
        ),
        Hold(Restoring)
    );
    msg.urgent = true;
    assert_eq!(policy.decide(busy, false, false, &msg, now), Hold(Busy));
    msg.sender = Sender::User;
    assert_eq!(policy.decide(busy, true, false, &msg, now), Notify);
    assert_eq!(policy.decide(busy, true, true, &msg, now), Hold(Paused));
    msg.delivery.notified_at = Some(20_000);
    let next_idle = Activity {
        active_since: 20_001,
        ..idle
    };
    assert_eq!(
        policy.decide(next_idle, true, false, &msg, now - 1),
        Hold(ReminderDelay)
    );
    assert_eq!(
        policy.decide(idle, true, false, &msg, now),
        Hold(SameIdlePeriod)
    );
    assert_eq!(policy.decide(next_idle, true, false, &msg, now), Remind);
    msg.delivery.reminded_at = Some(now);
    assert_eq!(
        policy.decide(next_idle, true, false, &msg, now),
        Hold(AlreadyReminded)
    );
    msg.delivery.read_at = Some(now);
    assert_eq!(policy.decide(idle, false, false, &msg, now), Hold(Read));
    msg.recipient = Recipient::User;
    assert_eq!(
        policy.decide(idle, false, false, &msg, now),
        Hold(UserInbox)
    );
}
