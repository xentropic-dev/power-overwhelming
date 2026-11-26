use super::SqliteDagStore;
use anyhow::Result;
use application::talk::{
    DeliveryChange, LogFilter, TalkEvent, TalkEventKind, TalkReader, TalkStore, TalkTransaction,
};
use domain::talk::{AgentRef, Delivery, Message, MessageId, Recipient, Sender, ThreadId};
use rusqlite::{Connection, OptionalExtension, Params, Row, TransactionBehavior, params};
use std::time::Instant;

pub(super) fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS talk_messages (
             id TEXT PRIMARY KEY, box_id TEXT NOT NULL,
             sender_kind TEXT NOT NULL, sender_agent TEXT, sender_provider TEXT,
             sender_role TEXT, sender_run TEXT, recipient_agent TEXT,
             body TEXT NOT NULL, thread TEXT NOT NULL, reply_to TEXT, broadcast TEXT,
             run TEXT, created_at INTEGER NOT NULL, queued_at INTEGER NOT NULL,
             notified_at INTEGER, reminded_at INTEGER, read_at INTEGER,
             urgent INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS talk_unread ON talk_messages(recipient_agent, box_id)
             WHERE read_at IS NULL;
         CREATE INDEX IF NOT EXISTS talk_thread ON talk_messages(thread);
         CREATE INDEX IF NOT EXISTS talk_pair ON talk_messages(sender_agent, recipient_agent, created_at);
         CREATE TABLE IF NOT EXISTS talk_events (
             seq INTEGER PRIMARY KEY AUTOINCREMENT, box_id TEXT NOT NULL,
             message TEXT NOT NULL REFERENCES talk_messages(id), kind TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS talk_events_box ON talk_events(box_id, seq);",
    )?;
    Ok(())
}

impl TalkStore for SqliteDagStore {
    fn read<T>(&self, query: impl FnOnce(&dyn TalkReader) -> Result<T>) -> Result<T> {
        let db = self.0.lock().unwrap();
        query(&TalkDb(&db))
    }

    fn write<T>(&self, operation: impl FnOnce(&dyn TalkTransaction) -> Result<T>) -> Result<T> {
        let mut db = self.0.lock().unwrap();
        let transaction = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let before = transaction.total_changes();
        let result = operation(&TalkDb(&transaction))?;
        let changed = transaction.total_changes() != before;
        transaction.commit()?;
        if changed {
            self.1.notify_all();
        }
        Ok(result)
    }

    fn wait_for_change(&self, cursor: u64, deadline: Instant) {
        let db = self.0.lock().unwrap();
        let _guard = self
            .1
            .wait_timeout_while(
                db,
                deadline.saturating_duration_since(Instant::now()),
                |db| matches!(TalkDb(db).cursor(), Ok(current) if current == cursor),
            )
            .unwrap();
    }
}

struct TalkDb<'a>(&'a Connection);

const COLUMNS: &str =
    "id, box_id, sender_kind, sender_agent, sender_provider, sender_role, sender_run,
     recipient_agent, body, thread, reply_to, broadcast, run, created_at, queued_at,
     notified_at, reminded_at, read_at, urgent";

fn message_from_row(row: &Row<'_>) -> rusqlite::Result<Message> {
    let box_id: String = row.get(1)?;
    let sender = match row.get::<_, String>(2)?.as_str() {
        "agent" => Sender::Agent(AgentRef {
            id: row.get(3)?,
            box_id: box_id.clone(),
            provider: row.get(4)?,
            role: row.get(5)?,
            run: row.get(6)?,
        }),
        "user" => Sender::User,
        "system" => Sender::System,
        kind => return Err(rusqlite::Error::InvalidParameterName(kind.into())),
    };
    let recipient = match row.get::<_, Option<String>>(7)? {
        Some(id) => Recipient::Agent {
            id,
            box_id: box_id.clone(),
        },
        None => Recipient::User,
    };
    Ok(Message {
        id: MessageId(row.get(0)?),
        box_id,
        sender,
        recipient,
        body: row.get(8)?,
        thread: ThreadId(row.get(9)?),
        reply_to: row.get::<_, Option<String>>(10)?.map(MessageId),
        broadcast: row.get(11)?,
        run: row.get(12)?,
        created_at: row.get(13)?,
        delivery: Delivery {
            queued_at: row.get(14)?,
            notified_at: row.get(15)?,
            reminded_at: row.get(16)?,
            read_at: row.get(17)?,
        },
        urgent: row.get(18)?,
    })
}

fn recipient_id(recipient: &Recipient) -> Option<&str> {
    match recipient {
        Recipient::Agent { id, .. } => Some(id),
        Recipient::User => None,
    }
}

impl TalkDb<'_> {
    fn messages(&self, suffix: &str, params: impl Params) -> Result<Vec<Message>> {
        Ok(self
            .0
            .prepare(&format!("SELECT {COLUMNS} FROM talk_messages {suffix}"))?
            .query_map(params, message_from_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    fn event(&self, message: &Message, kind: &str) -> Result<()> {
        self.0.execute(
            "INSERT INTO talk_events(box_id, message, kind) VALUES (?1, ?2, ?3)",
            params![message.box_id, message.id.0, kind],
        )?;
        Ok(())
    }
}

impl TalkReader for TalkDb<'_> {
    fn message(&self, id: &MessageId) -> Result<Option<Message>> {
        Ok(self
            .0
            .query_row(
                &format!("SELECT {COLUMNS} FROM talk_messages WHERE id = ?1"),
                [&id.0],
                message_from_row,
            )
            .optional()?)
    }

    fn inbox(
        &self,
        recipient: &Recipient,
        box_id: Option<&str>,
        all: bool,
    ) -> Result<Vec<Message>> {
        let unread = if all { "" } else { " AND read_at IS NULL" };
        self.messages(
            &format!(
                "WHERE recipient_agent IS ?1 AND (?2 IS NULL OR box_id = ?2){unread}
                      ORDER BY rowid"
            ),
            params![recipient_id(recipient), box_id],
        )
    }

    fn pending_agents(&self) -> Result<Vec<String>> {
        Ok(self
            .0
            .prepare(
                "SELECT DISTINCT recipient_agent FROM talk_messages
             WHERE read_at IS NULL AND reminded_at IS NULL AND recipient_agent IS NOT NULL",
            )?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    fn thread(&self, id: &ThreadId) -> Result<Vec<Message>> {
        self.messages("WHERE thread = ?1 ORDER BY rowid", [&id.0])
    }

    fn log(&self, filter: &LogFilter) -> Result<Vec<Message>> {
        let allowed = filter
            .allowed_runs
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let limit = filter
            .limit
            .map(|n| i64::try_from(n).unwrap_or(i64::MAX))
            .unwrap_or(-1);
        self.messages(
            "WHERE (?1 IS NULL OR box_id = ?1)
               AND (?2 IS NULL OR sender_agent = ?2 OR recipient_agent = ?2)
               AND (?3 IS NULL OR run = ?3)
               AND (?4 IS NULL OR run IN (SELECT value FROM json_each(?4)))
               AND (NOT ?5 OR recipient_agent IS NULL)
             ORDER BY rowid DESC LIMIT ?6",
            params![
                filter.box_id,
                filter.agent,
                filter.run,
                allowed,
                filter.to_user,
                limit
            ],
        )
    }

    fn recent_sends(&self, sender: &Sender, recipient: &Recipient, since: u64) -> Result<Vec<u64>> {
        let Sender::Agent(agent) = sender else {
            return Ok(Vec::new());
        };
        Ok(self
            .0
            .prepare(
                "SELECT created_at FROM talk_messages
             WHERE sender_agent = ?1 AND recipient_agent IS ?2 AND created_at >= ?3",
            )?
            .query_map(params![agent.id, recipient_id(recipient), since], |row| {
                row.get(0)
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    fn cursor(&self) -> Result<u64> {
        Ok(self
            .0
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM talk_events", [], |row| {
                row.get(0)
            })?)
    }

    fn events(&self, after: u64, box_id: Option<&str>) -> Result<Vec<TalkEvent>> {
        Ok(self
            .0
            .prepare(
                "SELECT seq, box_id, message, kind FROM talk_events
             WHERE seq > ?1 AND (?2 IS NULL OR box_id = ?2) ORDER BY seq LIMIT 256",
            )?
            .query_map(params![after, box_id], |row| {
                let kind = match row.get::<_, String>(3)?.as_str() {
                    "talk_message" => TalkEventKind::Message,
                    "talk_read" => TalkEventKind::Read,
                    "talk_delivery" => TalkEventKind::Delivery,
                    kind => return Err(rusqlite::Error::InvalidParameterName(kind.into())),
                };
                Ok(TalkEvent {
                    seq: row.get(0)?,
                    box_id: row.get(1)?,
                    message: MessageId(row.get(2)?),
                    kind,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }
}

impl TalkTransaction for TalkDb<'_> {
    fn insert(&self, message: &Message) -> Result<()> {
        let (kind, agent) = match &message.sender {
            Sender::Agent(agent) => ("agent", Some(agent)),
            Sender::User => ("user", None),
            Sender::System => ("system", None),
        };
        self.0.execute(
            &format!(
                "INSERT INTO talk_messages({COLUMNS}) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19
            )"
            ),
            params![
                message.id.0,
                message.box_id,
                kind,
                agent.map(|a| &a.id),
                agent.map(|a| &a.provider),
                agent.map(|a| &a.role),
                agent.and_then(|a| a.run.as_ref()),
                recipient_id(&message.recipient),
                message.body,
                message.thread.0,
                message.reply_to.as_ref().map(|id| &id.0),
                message.broadcast,
                message.run,
                message.created_at,
                message.delivery.queued_at,
                message.delivery.notified_at,
                message.delivery.reminded_at,
                message.delivery.read_at,
                message.urgent
            ],
        )?;
        self.event(message, "talk_message")
    }

    fn mark(&self, messages: &[&Message], change: DeliveryChange, at: u64) -> Result<()> {
        let (column, event) = match change {
            DeliveryChange::Notified => ("notified_at", "talk_delivery"),
            DeliveryChange::Reminded => ("reminded_at", "talk_delivery"),
            DeliveryChange::Read => ("read_at", "talk_read"),
        };
        let mut statement = self.0.prepare(&format!(
            "UPDATE talk_messages SET {column} = ?1 WHERE id = ?2 AND {column} IS NULL"
        ))?;
        for message in messages {
            if statement.execute(params![at, message.id.0])? != 0 {
                self.event(message, event)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_pre_talk_database_and_persists_messages_and_events() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        let db = Connection::open(file.path())?;
        db.execute_batch(
            "CREATE TABLE agents (
                 id TEXT PRIMARY KEY, box_id TEXT NOT NULL, worktree TEXT NOT NULL,
                 provider TEXT NOT NULL, parent TEXT, session TEXT UNIQUE,
                 state TEXT NOT NULL, label TEXT NOT NULL, created_at INTEGER NOT NULL
             );",
        )?;
        let store = SqliteDagStore::initialize(db)?;
        let message = Message {
            id: MessageId("hello".into()),
            box_id: "box".into(),
            sender: Sender::System,
            recipient: Recipient::User,
            body: "hello".into(),
            thread: ThreadId("hello".into()),
            reply_to: None,
            broadcast: None,
            run: None,
            created_at: 1,
            delivery: Delivery {
                queued_at: 1,
                ..Delivery::default()
            },
            urgent: false,
        };
        store.write(|db| db.insert(&message))?;
        assert!(
            store
                .write(|db| -> Result<()> {
                    db.mark(&[&message], DeliveryChange::Read, 5)?;
                    anyhow::bail!("roll back")
                })
                .is_err()
        );
        drop(store);

        let store = SqliteDagStore::initialize(Connection::open(file.path())?)?;
        store.write(|db| {
            assert_eq!(
                db.inbox(&Recipient::User, Some("box"), false)?,
                [message.clone()]
            );
            assert_eq!(db.thread(&message.thread)?, [message.clone()]);
            assert_eq!(db.cursor()?, 1);
            db.mark(&[&message], DeliveryChange::Read, 6)?;
            assert!(db.inbox(&Recipient::User, None, false)?.is_empty());
            assert_eq!(db.message(&message.id)?.unwrap().delivery.read_at, Some(6));
            assert_eq!(db.events(1, Some("box"))?[0].kind, TalkEventKind::Read);
            Ok(())
        })
    }
}
