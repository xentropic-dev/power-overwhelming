use crate::Labels;
use anyhow::Result;
use domain::session::Agent;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Mutex,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRecord {
    pub id: String,
    pub worktree: PathBuf,
    /// Agent the session was launched with.
    pub agent: Agent,
    /// Extra launch args, or the full argv for a shell command session.
    pub args: Vec<OsString>,
    /// Launch environment overrides. A missing value unsets the variable.
    pub env: Vec<(OsString, Option<OsString>)>,
    /// Last observed foreground agent, which can differ from the launch agent.
    pub foreground: Agent,
    /// Agent conversation id to resume, when known.
    pub conversation: Option<String>,
    /// Last observed working directory of the foreground process.
    pub cwd: Option<PathBuf>,
    /// Task and model the foreground agent last reported.
    pub labels: Labels,
}

pub trait SessionStore: Send + Sync {
    /// Inserts or replaces a record, preserving its original position.
    fn save(&self, record: &SessionRecord) -> Result<()>;
    /// Updates the observed foreground state if the record exists.
    fn update_foreground(
        &self,
        id: &str,
        foreground: &Agent,
        conversation: Option<&str>,
        cwd: Option<&Path>,
    ) -> Result<()>;
    /// Updates the reported task and model if the record exists.
    fn update_labels(&self, id: &str, labels: &Labels) -> Result<()>;
    fn remove(&self, id: &str) -> Result<()>;
    /// All records, in creation order.
    fn list(&self) -> Result<Vec<SessionRecord>>;
}

/// In-memory implementation of the session store for tests.
#[derive(Default)]
pub struct MemorySessions(Mutex<Vec<SessionRecord>>);

impl SessionStore for MemorySessions {
    fn save(&self, record: &SessionRecord) -> Result<()> {
        let mut records = self.0.lock().unwrap();
        if let Some(existing) = records.iter_mut().find(|existing| existing.id == record.id) {
            existing.clone_from(record);
        } else {
            records.push(record.clone());
        }
        Ok(())
    }

    fn update_foreground(
        &self,
        id: &str,
        foreground: &Agent,
        conversation: Option<&str>,
        cwd: Option<&Path>,
    ) -> Result<()> {
        let mut records = self.0.lock().unwrap();
        if let Some(record) = records.iter_mut().find(|record| record.id == id) {
            record.foreground = foreground.clone();
            record.conversation = conversation.map(str::to_owned);
            record.cwd = cwd.map(Path::to_path_buf);
        }
        Ok(())
    }

    fn update_labels(&self, id: &str, labels: &Labels) -> Result<()> {
        let mut records = self.0.lock().unwrap();
        if let Some(record) = records.iter_mut().find(|record| record.id == id) {
            record.labels = labels.clone();
        }
        Ok(())
    }

    fn remove(&self, id: &str) -> Result<()> {
        self.0.lock().unwrap().retain(|record| record.id != id);
        Ok(())
    }

    fn list(&self) -> Result<Vec<SessionRecord>> {
        Ok(self.0.lock().unwrap().clone())
    }
}
