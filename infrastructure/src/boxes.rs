use crate::persistence::state_directory;
use anyhow::{Context, Result};
use application::dag::{BoxRegistry, RegisteredBox};
use domain::dag::BoxInfo;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::PathBuf,
};
use uuid::Uuid;

#[derive(Default, Deserialize, Serialize)]
pub struct Layout {
    pub boxes: Vec<SavedBox>,
    pub active_box: Option<String>,
    /// Whether the first-run setup has been shown.
    #[serde(default)]
    pub onboarded: bool,
    /// The scheme selected from the desktop picker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_scheme: Option<String>,
    /// Stable names of the most recently run commands, such as `"box.new"`, newest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_commands: Vec<String>,
}

/// How many recently run commands the layout keeps for the command palette.
pub const RECENT_COMMANDS: usize = 3;

impl Layout {
    /// Records a command as the most recently run, keeping the newest [`RECENT_COMMANDS`]
    /// without duplicates. Returns whether the list changed.
    pub fn remember_command(&mut self, name: &str) -> bool {
        if self.recent_commands.first().map(String::as_str) == Some(name) {
            return false;
        }
        self.recent_commands.retain(|recent| recent != name);
        self.recent_commands.insert(0, name.to_owned());
        self.recent_commands.truncate(RECENT_COMMANDS);
        true
    }

    /// Read the last atomically saved layout without taking the desktop's lock.
    pub(crate) fn read() -> Result<Self> {
        Self::read_file(&state_directory()?.join("boxes.json"))
    }

    fn read_file(path: &std::path::Path) -> Result<Self> {
        match File::open(path) {
            Ok(file) => serde_json::from_reader(file).context("Read the saved box layout"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }
}

#[derive(Deserialize, Serialize)]
pub struct SavedBox {
    pub id: String,
    pub repository: PathBuf,
    pub checkout: PathBuf,
    pub shortcut: Option<u8>,
    pub views: Vec<SavedView>,
    pub active_view: Option<String>,
    pub previous_view: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub struct SavedView {
    pub session: String,
    pub worktree: PathBuf,
}

impl SavedBox {
    /// Folder name of the repository, which also names its folder under the worktree root.
    pub fn name(&self) -> String {
        let directory = if self
            .repository
            .file_name()
            .is_some_and(|name| name == ".git")
        {
            self.repository.parent().unwrap_or(&self.repository)
        } else {
            &self.repository
        };
        directory
            .file_name()
            .unwrap_or(directory.as_os_str())
            .to_string_lossy()
            .into_owned()
    }

    pub fn new(repository: PathBuf, checkout: PathBuf, shortcut: u8) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            repository,
            checkout,
            shortcut: Some(shortcut),
            views: Vec::new(),
            active_view: None,
            previous_view: None,
        }
    }

    pub fn select_view(&mut self, session: String) {
        if self.active_view.as_ref() != Some(&session) {
            self.previous_view = self.active_view.replace(session);
        }
    }

    pub fn remove_view(&mut self, index: usize) {
        let removed = self.views.remove(index);
        if self.previous_view.as_ref() == Some(&removed.session) {
            self.previous_view = None;
        }
        if self.active_view.as_ref() == Some(&removed.session) {
            self.active_view = self
                .views
                .get(index.min(self.views.len().saturating_sub(1)))
                .map(|view| view.session.clone());
            if self.previous_view == self.active_view {
                self.previous_view = None;
            }
        }
    }
}

pub(crate) struct BoxesFile;

impl BoxRegistry for BoxesFile {
    fn boxes(&self) -> Result<Vec<RegisteredBox>> {
        Ok(Layout::read()?
            .boxes
            .into_iter()
            .map(|entry| {
                let name = entry.name();
                RegisteredBox {
                    info: BoxInfo {
                        id: entry.id,
                        name,
                        repository: entry.repository,
                        checkout: entry.checkout,
                        shortcut: entry.shortcut,
                        open: entry.shortcut.is_some(),
                    },
                    sessions: entry.views.into_iter().map(|view| view.session).collect(),
                }
            })
            .collect())
    }
}

pub struct Registry {
    file: PathBuf,
    // Hold the lock while the window owns the layout so concurrent launches cannot overwrite it.
    _lock: File,
}

impl Registry {
    pub fn open() -> Result<(Self, Layout)> {
        let directory = state_directory()?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)?;
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(directory.join("boxes.lock"))?;
        FileExt::try_lock_exclusive(&lock)
            .context("Another Power desktop is using the box layout")?;
        let file = directory.join("boxes.json");
        let layout = Layout::read_file(&file)?;
        Ok((Self { file, _lock: lock }, layout))
    }

    pub fn save(&self, layout: &Layout) -> Result<()> {
        let temporary = self.file.with_extension(format!("{}.tmp", Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            serde_json::to_writer(&mut file, layout)?;
            file.flush()?;
            fs::rename(&temporary, &self.file)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.context("Save the box layout")
    }
}

#[cfg(test)]
mod tests {
    use super::Layout;

    #[test]
    fn remember_command_keeps_the_newest_three_without_duplicates() {
        let mut layout = Layout::default();
        for name in ["box.new", "tab.next", "box.runs"] {
            assert!(layout.remember_command(name));
        }
        assert_eq!(layout.recent_commands, ["box.runs", "tab.next", "box.new"]);
        assert!(!layout.remember_command("box.runs"));
        assert!(layout.remember_command("box.new"));
        assert_eq!(layout.recent_commands, ["box.new", "box.runs", "tab.next"]);
        assert!(layout.remember_command("dashboard.open"));
        assert_eq!(
            layout.recent_commands,
            ["dashboard.open", "box.new", "box.runs"]
        );
    }

    #[test]
    fn recent_commands_default_when_missing_from_saved_layouts() {
        let layout: Layout = serde_json::from_str(r#"{"boxes":[],"active_box":null}"#).unwrap();
        assert!(layout.recent_commands.is_empty());
        let mut layout = layout;
        layout.remember_command("box.pick");
        let saved = serde_json::to_string(&layout).unwrap();
        let read: Layout = serde_json::from_str(&saved).unwrap();
        assert_eq!(read.recent_commands, ["box.pick"]);
    }
}
