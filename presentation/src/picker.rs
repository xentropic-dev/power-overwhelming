use crate::{
    gui::agent_tree::AgentTree,
    keymap::{Command, Shortcut},
};
use domain::{session::Session, talk::AgentTalkSummary};
use gpui::{prelude::*, *};
use ignore::{DirEntry, ParallelVisitor, ParallelVisitorBuilder, WalkBuilder, WalkState};
use infrastructure::{
    client::{ManagedAgent, Run, RunStatus},
    theme::Scheme,
};
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{AtomKind, CaseMatching, Normalization, Pattern},
};
use std::{
    cmp::Reverse,
    collections::HashMap,
    ops::Range,
    path::{MAIN_SEPARATOR, Path, PathBuf, is_separator},
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

const MAX_COMPLETIONS: usize = 50;
/// Score added to recently run commands so they rank near the top among good matches.
const RECENT_BONUS: u32 = 24;
/// How deep and how wide the recursive directory search goes below the query's parent.
const WALK_DEPTH: usize = 6;
const WALK_LIMIT: usize = 30_000;
/// Directory names the search lists but never descends into, for checkouts without a
/// `.gitignore` that already excludes them.
const PRUNED: &[&str] = &["node_modules", "target", "__pycache__", "venv"];

pub enum Kind {
    Repository {
        directory: Option<PathBuf>,
    },
    Boxes,
    NewWorktree {
        box_id: String,
        from_session: String,
        /// The box's folder under the worktree root, where a typed name is checked out.
        folder: PathBuf,
        created: bool,
    },
    Tabs,
    Runs {
        box_id: String,
        runs: Vec<Run>,
    },
    RunLink,
    Agents {
        box_id: String,
        tree: AgentTree,
    },
    StatusActions(String),
    TalkDraft {
        title: String,
    },
    Sessions(String),
    End {
        session: String,
        /// Confirmation text, with keys from the current keymap.
        message: String,
    },
    /// Keyboard shortcuts, generated from the keymap.
    Help(Vec<Shortcut>),
    /// Links visible in the active terminal.
    Links,
    /// Shell integration setup, with the lines that explain what it changes. `first_run`
    /// finishes onboarding when it closes; otherwise it returns to the previous view.
    Setup {
        lines: Vec<String>,
        first_run: bool,
    },
    ColorSchemes {
        first_run: bool,
    },
    /// The command palette. Rows are box and tab commands in table order; `recent` lists the
    /// most recently run, newest first.
    Commands {
        recent: Vec<Command>,
    },
}

#[derive(Clone)]
pub enum Choice {
    NewBox,
    Directory(PathBuf),
    Box(String),
    Tab(String),
    Run(String),
    Agent(String),
    CopyText(String),
    CreateWorktree(String),
    Worktree(PathBuf),
    Session(Session, PathBuf),
    /// Finish first-run setup, installing the shell hook when true.
    Setup(bool),
    ColorScheme(&'static Scheme),
    Link(String),
    Command(Command),
}

#[derive(Clone)]
pub struct Row {
    pub label: String,
    pub detail: String,
    pub choice: Choice,
}

struct Completion {
    row: Row,
    /// Query text that Tab inserts, keeping the user's `~` or relative prefix.
    text: String,
    /// Byte ranges of the label that matched the fuzzy query.
    highlights: Vec<Range<usize>>,
}

/// A directory found below the search root.
pub struct Entry {
    /// Path relative to the root, with the platform's separators.
    relative: String,
    path: PathBuf,
    hidden: bool,
    depth: usize,
}

/// How a directory ranks: matches inside its final component come first, then nucleo's score.
type Rank = (bool, u32);

struct Listing {
    root: PathBuf,
    entries: Vec<Entry>,
    done: bool,
}

pub struct Picker {
    pub id: u64,
    pub kind: Kind,
    pub rows: Vec<Row>,
    pub message: Option<String>,
    pub waiting: bool,
    pub loading: bool,
    pub accept_pending: bool,
    /// Whether the run picker and agent tree list finished items too.
    pub show_finished: bool,
    /// The still-working agent whose end key was pressed once, awaiting a second press.
    pub armed_end: Option<String>,
    /// Why the last archive or unarchive in the run picker failed. Kept across refreshes
    /// until the next attempt.
    pub run_notice: Option<String>,
    pub focus: FocusHandle,
    pub agent_talk: HashMap<String, AgentTalkSummary>,
    query: String,
    cursor: usize,
    /// Selected byte range of the query, which typing replaces.
    selection: Option<Range<usize>>,
    edited: bool,
    selected: usize,
    scroll: ScrollHandle,
    theme_scroll: UniformListScrollHandle,
    listing: Option<Listing>,
    /// A directory to walk, and how deep.
    walk_request: Option<(PathBuf, usize)>,
    completions: Vec<Completion>,
    help_matches: Vec<usize>,
    /// Palette rows in display order, each with whether it was run recently.
    command_order: Vec<(usize, bool)>,
    matcher: Matcher,
}

impl Kind {
    fn title(&self) -> &str {
        match self {
            Self::Repository { .. } => "New box",
            Self::Boxes => "Boxes",
            Self::NewWorktree { .. } => "New worktree",
            Self::Tabs => "Tabs",
            Self::Runs { .. } => "DAG runs",
            Self::RunLink => "Open DAG run",
            Self::Agents { .. } => "Agent tree",
            Self::StatusActions(title) | Self::TalkDraft { title } => title,
            Self::Sessions(_) => "Recover a session",
            Self::End { .. } => "End terminal session?",
            Self::Help(_) => "Keyboard shortcuts",
            Self::Links => "Open a link",
            Self::Setup {
                first_run: true, ..
            } => "Welcome to Power Overwhelming",
            Self::Setup {
                first_run: false, ..
            } => "Shell integration",
            Self::ColorSchemes { first_run: true } => "Choose a color scheme",
            Self::ColorSchemes { first_run: false } => "Color schemes",
            Self::Commands { .. } => "Commands",
        }
    }

    fn placeholder(&self) -> Option<&'static str> {
        match self {
            Self::Repository { .. } => Some("Repository or worktree path"),
            Self::Boxes => Some("Search boxes"),
            Self::NewWorktree { created, .. } => (!created).then_some("Worktree path"),
            Self::Tabs => Some("Search by tab number or name"),
            Self::Runs { .. } => Some("Search runs"),
            Self::Agents { .. } => Some("Search agents"),
            Self::TalkDraft { .. } => Some("Write a message"),
            Self::Sessions(_) => Some("Search sessions"),
            Self::Help(_) => Some("Search shortcuts"),
            Self::Links => Some("Search links"),
            Self::ColorSchemes { .. } => Some("Search color schemes"),
            Self::Commands { .. } => Some("Run a command…"),
            Self::End { .. } | Self::Setup { .. } | Self::StatusActions(_) | Self::RunLink => None,
        }
    }
}

impl Picker {
    pub fn new(id: u64, kind: Kind, focus: FocusHandle, rows: Vec<Row>) -> Self {
        let help_matches = match &kind {
            Kind::Help(shortcuts) => (0..shortcuts.len()).collect(),
            _ => Vec::new(),
        };
        let matcher_config = if matches!(kind, Kind::Repository { .. }) {
            Config::DEFAULT.match_paths()
        } else {
            Config::DEFAULT
        };
        let matcher = Matcher::new(matcher_config);
        let command_order = match &kind {
            Kind::Commands { recent } => order_commands(&rows, recent, ""),
            _ => Vec::new(),
        };
        Self {
            id,
            kind,
            rows,
            message: None,
            waiting: false,
            loading: false,
            accept_pending: false,
            show_finished: false,
            armed_end: None,
            run_notice: None,
            focus,
            agent_talk: HashMap::new(),
            query: String::new(),
            cursor: 0,
            selection: None,
            edited: false,
            selected: 0,
            scroll: ScrollHandle::new(),
            theme_scroll: UniformListScrollHandle::new(),
            listing: None,
            walk_request: None,
            completions: Vec::new(),
            help_matches,
            command_order,
            matcher,
        }
    }

    pub fn suggest_directory(&mut self, directory: PathBuf) {
        if let Kind::Repository { directory: base } = &mut self.kind {
            *base = Some(directory.clone());
            if !self.edited {
                self.query = directory.to_string_lossy().into_owned();
                self.cursor = self.query.len();
                self.selection = Some(0..self.query.len());
                self.reset_selection();
                self.refresh_completions();
            }
        }
    }

    /// Matches the text after the last separator against every directory below the text before
    /// it, using nucleo's fzf algorithm and query syntax. Starts a new background walk when the
    /// parent changes.
    fn refresh_completions(&mut self) {
        self.completions.clear();
        if matches!(self.kind, Kind::NewWorktree { created: false, .. }) {
            return self.refresh_worktree_completions();
        }
        let Kind::Repository { directory } = &self.kind else {
            return;
        };
        if self.query.is_empty() {
            return;
        }
        let directory = directory.clone();
        let (parent, fragment) = match self.query.rfind(is_separator) {
            Some(index) => self.query.split_at(index + 1),
            None => ("", self.query.as_str()),
        };
        let (parent, fragment) = (parent.to_owned(), fragment.to_owned());
        let typed = self.path(&self.query, directory.as_ref());
        let root = self.path(&parent, directory.as_ref());
        if self
            .listing
            .as_ref()
            .is_none_or(|listing| listing.root != root)
        {
            self.listing = Some(Listing {
                root: root.clone(),
                entries: Vec::new(),
                done: false,
            });
            self.walk_request = Some((root, WALK_DEPTH));
        }
        let mut completions = Vec::new();
        if typed.is_dir() || fragment.is_empty() {
            completions.push(Completion {
                row: directory_row("Open checkout".into(), &typed, &self.query),
                highlights: Vec::new(),
                text: if self.query.ends_with(is_separator) {
                    self.query.clone()
                } else {
                    format!("{}{}", self.query, separator(&self.query))
                },
            });
        }
        let pattern = Pattern::parse(&fragment, CaseMatching::Smart, Normalization::Smart);
        // With nothing to match, list only the immediate children.
        let children_only = fragment.trim().is_empty();
        let show_hidden = fragment.starts_with('.');
        let mut buffer = Vec::new();
        let mut matches = Vec::new();
        for entry in self.listing.iter().flat_map(|listing| &listing.entries) {
            if (entry.hidden && !show_hidden)
                || entry.path == typed
                || (children_only && entry.depth != 1)
            {
                continue;
            }
            if let Some((rank, ranges)) =
                path_match(&pattern, &mut self.matcher, entry, &mut buffer)
            {
                matches.push((rank, ranges, entry));
            }
        }
        matches.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then(a.2.relative.len().cmp(&b.2.relative.len()))
                .then(a.2.relative.cmp(&b.2.relative))
        });
        let separator = separator(&self.query);
        for (_, highlights, entry) in matches.into_iter().take(MAX_COMPLETIONS) {
            let text = format!("{parent}{}{separator}", entry.relative);
            completions.push(Completion {
                row: directory_row(format!("{}{separator}", entry.relative), &entry.path, &text),
                text,
                highlights,
            });
        }
        if completions.is_empty() && self.listing.as_ref().is_some_and(|listing| listing.done) {
            completions.push(Completion {
                row: directory_row("Open checkout".into(), &typed, &self.query),
                text: self.query.clone(),
                highlights: Vec::new(),
            });
        }
        self.completions = completions;
        let count = self.completions.len();
        if self.selected >= count {
            self.selected = count.saturating_sub(1);
        }
    }

    /// Lists the typed path first, to create it as a worktree or open a shell when it exists,
    /// then the directories beside it that match the text after the last separator.
    fn refresh_worktree_completions(&mut self) {
        let Kind::NewWorktree { folder, .. } = &self.kind else {
            return;
        };
        if self.query.trim().is_empty() {
            return;
        }
        let home = Self::home();
        let target = worktree_target(&self.query, folder, home.as_deref());
        let resolved = target
            .as_ref()
            .ok()
            .map(|(branch, path)| path.clone().unwrap_or_else(|| folder.join(branch)));
        let exists = resolved.as_ref().is_some_and(|path| path.is_dir());
        let mut completions = vec![Completion {
            row: Row {
                label: if exists {
                    "Open shell"
                } else {
                    "Create worktree"
                }
                .into(),
                detail: match (&target, &resolved) {
                    (Err(message), _) => message.clone(),
                    (_, Some(path)) => display_path(path, home.as_deref()),
                    _ => String::new(),
                },
                choice: Choice::CreateWorktree(self.query.clone()),
            },
            text: self.query.clone(),
            highlights: Vec::new(),
        }];
        let (parent, fragment) = match self.query.rfind(is_separator) {
            Some(index) => self.query.split_at(index + 1),
            None => ("", self.query.as_str()),
        };
        let (parent, fragment) = (parent.to_owned(), fragment.to_owned());
        let root = if parent.is_empty() {
            folder.clone()
        } else {
            self.path(&parent, None)
        };
        if root.is_absolute()
            && self
                .listing
                .as_ref()
                .is_none_or(|listing| listing.root != root)
        {
            self.listing = Some(Listing {
                root: root.clone(),
                entries: Vec::new(),
                done: false,
            });
            self.walk_request = Some((root.clone(), 1));
        }
        let pattern = Pattern::parse(&fragment, CaseMatching::Smart, Normalization::Smart);
        let show_hidden = fragment.starts_with('.');
        let mut buffer = Vec::new();
        let mut matches = Vec::new();
        let entries = self
            .listing
            .iter()
            .filter(|listing| listing.root == root)
            .flat_map(|listing| &listing.entries);
        for entry in entries {
            if entry.depth != 1
                || (entry.hidden && !show_hidden)
                || resolved.as_ref() == Some(&entry.path)
            {
                continue;
            }
            if let Some((rank, ranges)) =
                path_match(&pattern, &mut self.matcher, entry, &mut buffer)
            {
                matches.push((rank, ranges, entry));
            }
        }
        matches.sort_by(|a, b| b.0.cmp(&a.0).then(a.2.relative.cmp(&b.2.relative)));
        for (_, highlights, entry) in matches.into_iter().take(MAX_COMPLETIONS) {
            let mut row = directory_row(entry.relative.clone(), &entry.path, "");
            row.choice = Choice::Worktree(entry.path.clone());
            completions.push(Completion {
                row,
                text: format!("{parent}{}", entry.relative),
                highlights,
            });
        }
        self.completions = completions;
        let count = self.completions.len();
        if self.selected >= count {
            self.selected = count.saturating_sub(1);
        }
    }

    /// The directory the caller should walk with [`walk`], and how deep, if the query moved
    /// to a new parent.
    pub fn take_walk_request(&mut self) -> Option<(PathBuf, usize)> {
        self.walk_request.take()
    }

    /// Adds walked directories. Returns false once the walk is stale and should stop.
    pub fn add_entries(&mut self, root: &Path, entries: Vec<Entry>, done: bool) -> bool {
        let Some(listing) = self.listing.as_mut().filter(|listing| listing.root == root) else {
            return false;
        };
        listing.entries.extend(entries);
        listing.done = done;
        self.refresh_completions();
        true
    }

    fn searching(&self) -> bool {
        matches!(self.kind, Kind::Repository { .. })
            && !self.query.is_empty()
            && self.listing.as_ref().is_some_and(|listing| !listing.done)
    }

    /// Replaces the query with the selected completion so the next keystroke filters its children.
    fn complete(&mut self) {
        let Some(text) = self
            .completions
            .get(self.selected)
            .map(|completion| completion.text.clone())
        else {
            return;
        };
        self.query = text;
        self.cursor = self.query.len();
        self.selection = None;
        self.edited = true;
        self.accept_pending = false;
        self.reset_selection();
        self.refresh_completions();
    }

    fn home() -> Option<PathBuf> {
        std::env::home_dir()
    }

    fn path(&self, query: &str, directory: Option<&PathBuf>) -> PathBuf {
        if let Some(rest) = query.strip_prefix('~')
            && (rest.is_empty() || rest.starts_with(is_separator))
            && let Some(home) = Self::home()
        {
            return home.join(rest.trim_start_matches(is_separator));
        }
        let path = PathBuf::from(query);
        if path.is_absolute() {
            return path;
        }
        directory
            .cloned()
            .or_else(Self::home)
            .map_or(path.clone(), |base| base.join(path))
    }

    fn visible_rows(&self) -> Vec<Row> {
        if let Kind::Agents { tree, .. } = &self.kind {
            return tree
                .rows()
                .map(|(agent, depth, marker)| {
                    let indent = " ".repeat(depth);
                    let provider = agent.provider.name();
                    let status = agent.status.label();
                    let detail = match &agent.model {
                        Some(model) => format!("{status} · {provider} · {model}"),
                        None => format!("{status} · {provider}"),
                    };
                    Row {
                        label: format!(
                            "{indent}{marker} {}{}",
                            agent.label,
                            AgentTree::talk_detail(self.agent_talk.get(&agent.id))
                        ),
                        detail: format!("{indent}{detail} · {}", agent.worktree.display()),
                        choice: Choice::Agent(agent.id.clone()),
                    }
                })
                .collect();
        }
        if let Kind::Commands { .. } = &self.kind {
            return self
                .command_order
                .iter()
                .map(|&(index, recent)| {
                    let mut row = self.rows[index].clone();
                    if recent {
                        row.detail = if row.detail.is_empty() {
                            "Recent".to_owned()
                        } else {
                            format!("Recent · {}", row.detail)
                        };
                    }
                    row
                })
                .collect();
        }
        if self.completes_paths() && !self.query.is_empty() {
            return self
                .completions
                .iter()
                .map(|completion| completion.row.clone())
                .collect();
        }
        let query = self.query.to_lowercase();
        if matches!(self.kind, Kind::Tabs)
            && let Ok(index) = query.trim().parse::<usize>()
        {
            return index
                .checked_sub(1)
                .and_then(|index| self.rows.get(index))
                .cloned()
                .into_iter()
                .collect();
        }
        fuzzy_rows(&self.rows, &self.query)
    }

    /// The run under the selection in the run picker.
    pub fn selected_run(&self) -> Option<&Run> {
        let Some(Choice::Run(id)) = self.selection() else {
            return None;
        };
        let Kind::Runs { runs, .. } = &self.kind else {
            return None;
        };
        runs.iter().find(|run| run.id == id)
    }

    pub fn choice(&self, index: usize) -> Option<Choice> {
        self.visible_rows()
            .into_iter()
            .nth(index)
            .map(|row| row.choice)
    }

    pub fn selection(&self) -> Option<Choice> {
        self.choice(self.selected)
    }

    /// Whether the rows are path completions of the query.
    fn completes_paths(&self) -> bool {
        matches!(
            self.kind,
            Kind::Repository { .. } | Kind::NewWorktree { created: false, .. }
        )
    }

    /// Fills the worktree prompt with the path for a suggested name. Only the name is
    /// selected, so typing or one backspace replaces it and keeps the folder.
    pub fn suggest_worktree(&mut self, name: &str, home: Option<&Path>) {
        if let Kind::NewWorktree {
            folder,
            created: false,
            ..
        } = &self.kind
        {
            let mut folder = display_path(folder, home);
            if !folder.ends_with(is_separator) {
                folder.push(MAIN_SEPARATOR);
            }
            self.query = folder + name;
            self.cursor = self.query.len();
            self.selection = Some(self.query.len() - name.len()..self.query.len());
            self.refresh_completions();
        }
    }

    pub fn worktree_created(&mut self, path: PathBuf) {
        if let Kind::NewWorktree { created, .. } = &mut self.kind {
            *created = true;
            self.query.clear();
            self.cursor = 0;
            self.rows = vec![Row {
                label: "Open shell".into(),
                detail: path.display().to_string(),
                choice: Choice::Worktree(path),
            }];
        }
    }

    pub fn set_runs(&mut self, runs: Vec<Run>) {
        let previous = self.selection();
        let Kind::Runs { runs: listed, .. } = &mut self.kind else {
            return;
        };
        self.rows = runs
            .iter()
            .map(|run| Row {
                label: run.title.clone(),
                detail: run_detail(run),
                choice: Choice::Run(run.id.clone()),
            })
            .collect();
        *listed = runs;
        if let Some(Choice::Run(previous)) = previous {
            self.selected = self
                .visible_rows()
                .iter()
                .position(|row| matches!(&row.choice, Choice::Run(id) if id == &previous))
                .unwrap_or_default();
        }
    }

    pub fn set_agents(&mut self, agents: Vec<ManagedAgent>) {
        if let Kind::Agents { tree, .. } = &mut self.kind {
            let previous = tree
                .rows()
                .nth(self.selected)
                .map(|(agent, _, _)| agent.id.clone());
            if !tree.replace(agents) {
                return;
            }
            let best = tree.search(&self.query, &mut self.matcher);
            self.selected = previous
                .and_then(|id| tree.rows().position(|(agent, _, _)| agent.id == id))
                .unwrap_or(best);
            self.scroll.scroll_to_item(self.selected);
        }
    }

    pub fn agent_branch(&mut self, expand: bool) {
        if let Kind::Agents { tree, .. } = &mut self.kind
            && let Some(index) = tree.branch(self.selected, expand)
        {
            self.selected = index;
            self.scroll.scroll_to_item(index);
        }
    }

    fn reset_selection(&mut self) {
        self.selected = 0;
        self.scroll.set_offset(point(px(0.), px(0.)));
        self.theme_scroll
            .scroll_to_item_strict(0, ScrollStrategy::Top);
    }

    fn move_selection(&mut self, up: bool) {
        let count = self.visible_rows().len();
        if count == 0 {
            return;
        }
        let selected = self.selected.min(count - 1);
        self.selected = if up {
            selected.checked_sub(1).unwrap_or(count - 1)
        } else {
            (selected + 1) % count
        };
        if matches!(self.kind, Kind::ColorSchemes { .. }) {
            self.theme_scroll.scroll_to_item(
                self.selected,
                if up {
                    ScrollStrategy::Top
                } else {
                    ScrollStrategy::Bottom
                },
            );
        } else {
            self.scroll.scroll_to_item(self.selected);
        }
    }

    fn before_cursor(&self) -> usize {
        self.query[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index)
    }

    fn after_cursor(&self) -> usize {
        self.query[self.cursor..]
            .chars()
            .next()
            .map_or(self.cursor, |character| self.cursor + character.len_utf8())
    }

    fn replace_selection(&mut self, text: &str) {
        if let Some(range) = self.selection.take() {
            self.cursor = range.start;
            self.query.replace_range(range, "");
        }
        self.query.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.edited = true;
        self.accept_pending = false;
        if !self.loading {
            self.message = None;
            self.run_notice = None;
        }
        self.reset_selection();
        match &mut self.kind {
            Kind::Help(shortcuts) => {
                self.help_matches = search_shortcuts(shortcuts, &self.query, &mut self.matcher);
            }
            Kind::Agents { tree, .. } => {
                self.selected = tree.search(&self.query, &mut self.matcher);
                self.scroll.scroll_to_item(self.selected);
            }
            Kind::Commands { recent } => {
                self.command_order = order_commands(&self.rows, recent, &self.query);
            }
            _ => self.refresh_completions(),
        }
    }

    /// Whether the picker has a search field. Dialogs without one take plain-letter bindings.
    pub fn text(&self) -> &str {
        &self.query
    }

    pub fn has_text_field(&self) -> bool {
        self.kind.placeholder().is_some()
    }

    /// Moves the selection or scrolls shortcut help.
    pub fn step(&mut self, up: bool) {
        if self.waiting {
            return;
        }
        if matches!(self.kind, Kind::Help(_)) {
            let delta = px(if up { 32. } else { -32. });
            let offset =
                (self.scroll.offset().y + delta).clamp(-self.scroll.max_offset().height, px(0.));
            self.scroll.set_offset(point(px(0.), offset));
        } else {
            self.move_selection(up);
        }
    }

    /// Completes the selected directory in the path pickers. Returns false elsewhere.
    pub fn complete_selection(&mut self) -> bool {
        if self.waiting || !self.completes_paths() {
            return false;
        }
        self.complete();
        true
    }

    pub fn edit(&mut self, stroke: &Keystroke, cx: &mut App) {
        if self.waiting || self.kind.placeholder().is_none() {
            return;
        }
        let key = stroke.key.as_str();
        let mods = stroke.modifiers;
        if mods.control && !mods.alt && !mods.platform {
            match key {
                "a" => {
                    self.selection = Some(0..self.query.len());
                    self.edited = true;
                }
                "v" => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                        self.replace_selection(&text.replace(['\r', '\n', '\0'], ""));
                    }
                }
                "c" => {
                    if let Some(range) = self.selection.clone() {
                        cx.write_to_clipboard(ClipboardItem::new_string(
                            self.query[range].to_owned(),
                        ));
                    }
                }
                _ => {}
            }
            return;
        }
        match key {
            "left" | "right" | "home" | "end" => {
                self.cursor = match key {
                    "home" => 0,
                    "end" => self.query.len(),
                    "left" | "right" if self.selection.is_some() => {
                        let range = self.selection.clone().unwrap();
                        if key == "left" {
                            range.start
                        } else {
                            range.end
                        }
                    }
                    "left" => self.before_cursor(),
                    _ => self.after_cursor(),
                };
                self.selection = None;
                self.edited = true;
            }
            "backspace" | "delete" => {
                if self.selection.is_none() {
                    let range = if key == "backspace" {
                        self.before_cursor()..self.cursor
                    } else {
                        self.cursor..self.after_cursor()
                    };
                    self.cursor = range.start;
                    self.query.replace_range(range, "");
                }
                self.replace_selection("");
            }
            _ if !mods.alt && !mods.platform => {
                if let Some(text) = stroke.key_char.as_deref()
                    && !text.chars().any(char::is_control)
                {
                    self.replace_selection(text);
                }
            }
            _ => {}
        }
    }

    pub fn render(
        &self,
        colors: &'static infrastructure::theme::Colors,
        select: impl Fn(usize, &mut Window, &mut App) + Clone + 'static,
    ) -> impl IntoElement {
        let mut visible = self.visible_rows();
        let help = match &self.kind {
            Kind::Help(shortcuts) => Some(shortcuts),
            _ => None,
        };
        let text_prompt = matches!(
            self.kind,
            Kind::NewWorktree { created: false, .. } | Kind::TalkDraft { .. }
        );
        let no_matches = visible.is_empty()
            && !text_prompt
            && self.help_matches.is_empty()
            && !self.query.is_empty()
            && self.message.is_none()
            && !self.searching();
        let has_results = !visible.is_empty();
        let completing = self.completes_paths() && !self.query.is_empty();
        let theme_list = matches!(self.kind, Kind::ColorSchemes { .. }).then(|| {
            let rows = std::mem::take(&mut visible);
            let selected = self.selected;
            let select = select.clone();
            let query = self.query.clone();
            let matched = HighlightStyle {
                color: Some(rgb(colors.accent).into()),
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            };
            let height = px(30. * rows.len().min(10) as f32);
            uniform_list("scheme-options", rows.len(), move |range, _, _| {
                range
                    .map(|index| {
                        let row = &rows[index];
                        let Choice::ColorScheme(scheme) = row.choice else {
                            unreachable!()
                        };
                        let select = select.clone();
                        div()
                            .id(("scheme", index))
                            .h(px(30.))
                            .px_2()
                            .flex()
                            .gap_2()
                            .items_center()
                            .cursor_pointer()
                            .when(index == selected, |row| row.bg(rgb(colors.raised)))
                            .hover(|row| row.bg(rgb(colors.raised)))
                            .child(
                                div().flex_1().min_w_0().truncate().child(
                                    StyledText::new(row.label.clone()).with_highlights(
                                        fuzzy_highlights(&query, &row.label)
                                            .into_iter()
                                            .map(|range| (range, matched)),
                                    ),
                                ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(colors.muted))
                                    .child(row.detail.clone()),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_none()
                                    .gap(px(1.))
                                    .p(px(2.))
                                    .bg(rgb(scheme.colors.background))
                                    .border_1()
                                    .border_color(rgb(colors.border))
                                    .children(
                                        [
                                            scheme.colors.foreground,
                                            scheme.colors.indexed[1],
                                            scheme.colors.indexed[2],
                                            scheme.colors.indexed[4],
                                        ]
                                        .map(|color| div().size(px(9.)).bg(rgb(color))),
                                    ),
                            )
                            .on_click(move |_, window, cx| select(index, window, cx))
                    })
                    .collect()
            })
            .w_full()
            .h(height)
            .track_scroll(self.theme_scroll.clone())
        });
        let rows = visible.into_iter().enumerate().map(|(index, row)| {
            let select = select.clone();
            let highlights = if completing {
                self.completions
                    .get(index)
                    .map(|completion| completion.highlights.clone())
                    .unwrap_or_default()
            } else {
                fuzzy_highlights(&self.query, &row.label)
            };
            let detail_highlights = if completing {
                Vec::new()
            } else {
                fuzzy_highlights(&self.query, &row.detail)
            };
            let matched = HighlightStyle {
                color: Some(rgb(colors.accent).into()),
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            };
            let label = StyledText::new(row.label)
                .with_highlights(highlights.into_iter().map(|range| (range, matched)));
            div()
                .id(("picker-option", index))
                .px_2()
                .py_1()
                .cursor_pointer()
                .when(index == self.selected, |row| row.bg(rgb(colors.raised)))
                .hover(|row| row.bg(rgb(colors.raised)))
                .child(div().overflow_hidden().text_ellipsis().child(label))
                .when(!row.detail.is_empty(), |element| {
                    element.child(
                        div()
                            .text_color(rgb(colors.muted))
                            .text_xs()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(StyledText::new(row.detail).with_highlights(
                                detail_highlights.into_iter().map(|range| (range, matched)),
                            )),
                    )
                })
                .on_click(move |_, window, cx| select(index, window, cx))
        });
        let help_rows = help.into_iter().flat_map(|shortcuts| {
            self.help_matches.iter().map(|&index| {
                let shortcut = &shortcuts[index];
                div()
                    .flex()
                    .items_baseline()
                    .justify_between()
                    .gap_3()
                    .px_2()
                    .py_1()
                    .text_sm()
                    .child(div().min_w_0().child(shortcut.description.clone()))
                    .child(
                        div()
                            .min_w_0()
                            .max_w(relative(0.6))
                            .text_right()
                            .text_color(rgb(colors.muted))
                            .child(shortcut.binding.clone()),
                    )
            })
        });
        let help_notes =
            (help.is_some() && self.query.trim().is_empty()).then(|| {
                div()
                    .mt_2()
                    .px_2()
                    .pt_2()
                    .border_t_1()
                    .border_color(rgb(colors.border))
                    .text_xs()
                    .text_color(rgb(colors.muted))
                    .child(div().child("Press either prefix twice to send it to the terminal."))
                    .child(div().child(
                        "Bindings come from keys in ~/.config/power-overwhelming/config.lua.",
                    ))
            });
        div()
            .absolute()
            .inset_0()
            .occlude()
            .bg(rgba((colors.background << 8) | 0x88))
            .flex()
            .justify_center()
            .items_start()
            .pt(px(if help.is_some() { 24. } else { 76. }))
            .px_4()
            .when(help.is_some(), |overlay| overlay.pb_4())
            .child(
                div()
                    .id("picker")
                    .track_focus(&self.focus)
                    .w_full()
                    .max_w(px(650.))
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .when(help.is_some(), |panel| panel.max_h_full().overflow_hidden())
                    .bg(rgb(colors.surface))
                    .border_1()
                    .border_color(rgb(colors.border))
                    .shadow_lg()
                    .child(
                        div()
                            .flex_none()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(self.kind.title().to_owned()),
                    )
                    .when_some(self.kind.placeholder(), |panel, placeholder| {
                        let field = div()
                            .flex_none()
                            .px_2()
                            .py_1()
                            .flex()
                            .items_center()
                            .overflow_hidden()
                            .border_1()
                            .border_color(rgb(colors.accent))
                            .bg(rgb(colors.background));
                        let selection = self.selection.clone().filter(|range| !range.is_empty());
                        panel.child(if let Some(range) = selection {
                            field
                                .child(
                                    div()
                                        .whitespace_nowrap()
                                        .child(self.query[..range.start].to_owned()),
                                )
                                .child(
                                    div()
                                        .bg(rgb(colors.raised))
                                        .whitespace_nowrap()
                                        .child(self.query[range.clone()].to_owned()),
                                )
                                .child(
                                    div()
                                        .whitespace_nowrap()
                                        .child(self.query[range.end..].to_owned()),
                                )
                        } else {
                            field
                                .child(
                                    div()
                                        .whitespace_nowrap()
                                        .child(self.query[..self.cursor].to_owned()),
                                )
                                .child(
                                    div()
                                        .w(px(1.))
                                        .h(px(16.))
                                        .flex_none()
                                        .bg(rgb(colors.foreground)),
                                )
                                .child(
                                    div()
                                        .whitespace_nowrap()
                                        .child(self.query[self.cursor..].to_owned()),
                                )
                                .when(self.query.is_empty(), |field| {
                                    field.child(
                                        div()
                                            .pl_1()
                                            .text_color(rgb(colors.muted))
                                            .child(placeholder),
                                    )
                                })
                        })
                    })
                    .when_some(self.message.clone(), |panel, message| {
                        panel.child(
                            div()
                                .text_sm()
                                .text_color(rgb(colors.foreground))
                                .child(message),
                        )
                    })
                    .when(self.searching(), |panel| {
                        panel.child(
                            div()
                                .text_sm()
                                .text_color(rgb(colors.muted))
                                .child("Searching…"),
                        )
                    })
                    .when(no_matches, |panel| {
                        panel.child(
                            div()
                                .text_sm()
                                .text_color(rgb(colors.foreground))
                                .child("No matches"),
                        )
                    })
                    .children(theme_list)
                    .when(
                        !matches!(self.kind, Kind::ColorSchemes { .. })
                            && (help.is_some()
                                || has_results
                                || (!self.query.is_empty() && !text_prompt)),
                        |panel| {
                            panel.child(
                                div()
                                    .id("picker-options")
                                    .min_h_0()
                                    .max_h(px(if help.is_some() { 520. } else { 310. }))
                                    .overflow_y_scroll()
                                    .track_scroll(&self.scroll)
                                    .children(rows)
                                    .children(help_rows)
                                    .children(help_notes),
                            )
                        },
                    )
                    .when_some(
                        match &self.kind {
                            Kind::End { message, .. } => Some(message.clone()),
                            _ => None,
                        },
                        |panel, message| panel.child(message),
                    )
                    .when_some(
                        match &self.kind {
                            Kind::Setup { lines, .. } => Some(lines.clone()),
                            _ => None,
                        },
                        |panel, lines| {
                            panel
                                .children(lines.into_iter().map(|line| div().text_sm().child(line)))
                        },
                    ),
            )
    }
}

fn search_shortcuts(shortcuts: &[Shortcut], query: &str, matcher: &mut Matcher) -> Vec<usize> {
    let pattern = Pattern::new(
        query,
        CaseMatching::Ignore,
        Normalization::Smart,
        AtomKind::Fuzzy,
    );
    let mut buffer = Vec::new();
    let mut matches: Vec<_> = shortcuts
        .iter()
        .enumerate()
        .filter_map(|(index, shortcut)| {
            let text = format!("{} {}", shortcut.description, shortcut.binding);
            let score = pattern.score(Utf32Str::new(&text, &mut buffer), matcher)?;
            Some((index, score))
        })
        .collect();
    matches.sort_by_key(|&(_, score)| Reverse(score));
    matches.into_iter().map(|(index, _)| index).collect()
}

/// Orders command palette rows. With an empty query the recent commands come first, newest
/// first, then the rest in table order. Otherwise rows are ranked like every other picker
/// (see [`fuzzy_rank`]), with a small bonus for recent commands among rows matched the same
/// way. Returns row indices, each with whether its command is recent.
fn order_commands(rows: &[Row], recent: &[Command], query: &str) -> Vec<(usize, bool)> {
    let is_recent =
        |row: &Row| matches!(row.choice, Choice::Command(command) if recent.contains(&command));
    if query.trim().is_empty() {
        let first: Vec<usize> = recent
            .iter()
            .filter_map(|command| {
                rows.iter().position(
                    |row| matches!(row.choice, Choice::Command(other) if other == *command),
                )
            })
            .collect();
        let rest = (0..rows.len()).filter(|index| !first.contains(index));
        return first
            .iter()
            .map(|&index| (index, true))
            .chain(rest.map(|index| (index, false)))
            .collect();
    }
    let mut matches = fuzzy_rank(rows, query);
    for (index, (_, score)) in &mut matches {
        if is_recent(&rows[*index]) {
            *score += RECENT_BONUS;
        }
    }
    matches.sort_by_key(|&(index, rank)| (Reverse(rank), index));
    matches
        .into_iter()
        .map(|(index, _)| (index, is_recent(&rows[index])))
        .collect()
}

fn directory_row(label: String, path: &Path, query: &str) -> Row {
    let resolved = path.display().to_string();
    let git = path.join(".git").exists();
    let detail = match (resolved == query.trim_end_matches(is_separator), git) {
        (true, true) => "Git checkout".into(),
        (true, false) => String::new(),
        (false, true) => format!("{resolved} · Git checkout"),
        (false, false) => resolved,
    };
    Row {
        label,
        detail,
        choice: Choice::Directory(path.to_path_buf()),
    }
}

/// The separator the user last typed, so completions keep their style.
fn separator(query: &str) -> char {
    query
        .chars()
        .rev()
        .find(|character| is_separator(*character))
        .unwrap_or(MAIN_SEPARATOR)
}

/// Walks directories below `root` on several threads with ripgrep's walker and sends them in
/// batches. It skips anything `.gitignore` excludes and never follows symlinks. Hidden and
/// build directories are listed but not entered, and hidden ones only directly below the root.
/// Stops early when the receiver is dropped.
pub fn walk(root: &Path, depth: usize, sender: &async_channel::Sender<Vec<Entry>>) {
    let found = AtomicUsize::new(0);
    WalkBuilder::new(root)
        .hidden(false)
        .follow_links(false)
        .max_depth(Some(depth))
        .build_parallel()
        .visit(&mut Visitors {
            root,
            sender,
            found: &found,
        });
}

struct Visitors<'a> {
    root: &'a Path,
    sender: &'a async_channel::Sender<Vec<Entry>>,
    found: &'a AtomicUsize,
}

impl<'a> ParallelVisitorBuilder<'a> for Visitors<'a> {
    fn build(&mut self) -> Box<dyn ParallelVisitor + 'a> {
        Box::new(Visitor {
            root: self.root,
            sender: self.sender,
            found: self.found,
            batch: Vec::new(),
            sent: Instant::now(),
        })
    }
}

/// One walker thread's state. Batches are flushed at most twenty times a second per thread.
struct Visitor<'a> {
    root: &'a Path,
    sender: &'a async_channel::Sender<Vec<Entry>>,
    found: &'a AtomicUsize,
    batch: Vec<Entry>,
    sent: Instant,
}

impl Visitor<'_> {
    fn flush(&mut self) -> bool {
        self.sent = Instant::now();
        self.batch.is_empty()
            || self
                .sender
                .send_blocking(std::mem::take(&mut self.batch))
                .is_ok()
    }
}

impl ParallelVisitor for Visitor<'_> {
    fn visit(&mut self, entry: Result<DirEntry, ignore::Error>) -> WalkState {
        let Ok(entry) = entry else {
            return WalkState::Continue;
        };
        let depth = entry.depth();
        let path = entry.path();
        let is_dir = entry
            .file_type()
            .is_some_and(|kind| kind.is_dir() || (kind.is_symlink() && path.is_dir()));
        if depth == 0 || !is_dir {
            return WalkState::Continue;
        }
        let hidden = is_hidden(&entry);
        let Some(relative) = path
            .strip_prefix(self.root)
            .ok()
            .and_then(Path::to_str)
            .filter(|_| depth == 1 || !hidden)
        else {
            return WalkState::Skip;
        };
        self.batch.push(Entry {
            relative: relative.to_owned(),
            path: path.to_path_buf(),
            hidden,
            depth,
        });
        if self.found.fetch_add(1, Ordering::Relaxed) + 1 >= WALK_LIMIT {
            self.flush();
            return WalkState::Quit;
        }
        if self.sent.elapsed() >= Duration::from_millis(50) && !self.flush() {
            return WalkState::Quit;
        }
        let pruned = entry
            .file_name()
            .to_str()
            .is_some_and(|name| PRUNED.contains(&name));
        if hidden || pruned {
            WalkState::Skip
        } else {
            WalkState::Continue
        }
    }
}

impl Drop for Visitor<'_> {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Dot-prefixed names everywhere, plus the hidden attribute on Windows.
fn is_hidden(entry: &DirEntry) -> bool {
    entry.file_name().to_string_lossy().starts_with('.') || has_hidden_attribute(entry)
}

#[cfg(windows)]
fn has_hidden_attribute(entry: &DirEntry) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    entry
        .metadata()
        .is_ok_and(|metadata| metadata.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0)
}

#[cfg(not(windows))]
fn has_hidden_attribute(_: &DirEntry) -> bool {
    false
}

/// Matches the whole relative path, so negations such as `!test` see every component, then
/// prefers directories whose final component matches on its own. Returns the rank and the
/// matched byte ranges of the relative path.
fn path_match(
    pattern: &Pattern,
    matcher: &mut Matcher,
    entry: &Entry,
    buffer: &mut Vec<char>,
) -> Option<(Rank, Vec<Range<usize>>)> {
    let relative = entry.relative.as_str();
    let mut indices = Vec::new();
    let score = pattern.indices(Utf32Str::new(relative, buffer), matcher, &mut indices)?;
    let base = relative.rfind(is_separator).map_or(0, |index| index + 1);
    let mut base_indices = Vec::new();
    let rank = match pattern.indices(
        Utf32Str::new(&relative[base..], buffer),
        matcher,
        &mut base_indices,
    ) {
        Some(base_score) => {
            let offset = relative[..base].chars().count() as u32;
            indices = base_indices
                .into_iter()
                .map(|index| index + offset)
                .collect();
            (true, base_score)
        }
        None => (false, score),
    };
    Some((rank, byte_ranges(relative, indices)))
}

/// Turns nucleo's character indices into merged byte ranges of `text`.
fn byte_ranges(text: &str, mut indices: Vec<u32>) -> Vec<Range<usize>> {
    indices.sort_unstable();
    indices.dedup();
    let characters: Vec<(usize, char)> = text.char_indices().collect();
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for index in indices {
        let Some(&(start, character)) = characters.get(index as usize) else {
            continue;
        };
        let end = start + character.len_utf8();
        match ranges.last_mut() {
            Some(range) if range.end == start => range.end = end,
            _ => ranges.push(start..end),
        }
    }
    ranges
}

/// Status, task progress and ID, e.g. `Active · 3/10 done · run-id`.
fn run_detail(run: &Run) -> String {
    let status = match run.status() {
        RunStatus::Active => "Active",
        RunStatus::Stalled => "Stalled",
        RunStatus::Completed => "Completed",
        RunStatus::Archived => "Archived",
    };
    format!(
        "{status} · {}/{} done · {}",
        run.completed_tasks(),
        run.tasks.len(),
        run.id
    )
}

/// Shows a path with the home directory as `~`.
fn display_path(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~{MAIN_SEPARATOR}{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// Reads the new worktree prompt as a branch name and, when the path leaves the box's
/// folder, where to check it out. Text after the folder is the branch, so `feature/x`
/// works; a bare name means a branch in the folder.
pub fn worktree_target(
    query: &str,
    folder: &Path,
    home: Option<&Path>,
) -> Result<(String, Option<PathBuf>), String> {
    let query = query.trim();
    let path = match query.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with(is_separator) => match home {
            Some(home) => home.join(rest.trim_start_matches(is_separator)),
            None => PathBuf::from(query),
        },
        _ => PathBuf::from(query),
    };
    if let Ok(branch) = path.strip_prefix(folder) {
        let branch = branch
            .to_string_lossy()
            .trim_matches(is_separator)
            .to_owned();
        if branch.is_empty() {
            return Err("Type a name for the worktree.".into());
        }
        return Ok((branch, None));
    }
    if !path.is_absolute() {
        if query.contains(is_separator) {
            return Err(format!(
                "Type a name, or a full path such as {}{MAIN_SEPARATOR}name.",
                display_path(folder, home)
            ));
        }
        return Ok((query.to_owned(), None));
    }
    match path.file_name() {
        Some(name) => Ok((name.to_string_lossy().into_owned(), Some(path))),
        None => Err("Type a name for the worktree.".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Choice, Entry, Rank, Row, Run, Shortcut, display_path, order_commands, path_match,
        run_detail, search_shortcuts, walk, worktree_target,
    };
    use crate::keymap::{Command, Keymap};
    use infrastructure::client::Coordinator;

    #[test]
    fn worktree_prompt_reads_the_name_after_the_box_folder() {
        let home = PathBuf::from("/home/me");
        let folder = home.join(".power/repo");
        let target = |query| worktree_target(query, &folder, Some(&home));
        assert_eq!(display_path(&folder, Some(&home)), "~/.power/repo");
        assert_eq!(
            target("~/.power/repo/quiet-cedar"),
            Ok(("quiet-cedar".into(), None))
        );
        assert_eq!(
            target("/home/me/.power/repo/feature/x/"),
            Ok(("feature/x".into(), None))
        );
        assert_eq!(target("mine"), Ok(("mine".into(), None)));
        assert_eq!(
            target("~/work/mine"),
            Ok(("mine".into(), Some(home.join("work/mine"))))
        );
        assert!(target("~/.power/repo/").is_err());
        assert!(target("work/mine").is_err());
    }

    #[test]
    fn archived_runs_say_archived_in_the_row_detail() {
        let mut run = Run {
            id: "run-id".into(),
            box_id: "box".into(),
            title: "Abandoned".into(),
            coordinator: Coordinator::External { label: "x".into() },
            created_at: 1,
            archived_at: None,
            paused_at: None,
            tasks: Vec::new(),
        };
        assert_eq!(run_detail(&run), "Active · 0/0 done · run-id");
        run.archived_at = Some(2);
        assert_eq!(run_detail(&run), "Archived · 0/0 done · run-id");
    }

    use nucleo_matcher::{
        Config, Matcher,
        pattern::{CaseMatching, Normalization, Pattern},
    };
    use std::{
        ops::Range,
        path::{MAIN_SEPARATOR, MAIN_SEPARATOR_STR, PathBuf},
    };

    fn entry(relative: &str) -> Entry {
        let relative = relative.replace('/', MAIN_SEPARATOR_STR);
        Entry {
            depth: relative.split(MAIN_SEPARATOR).count(),
            path: PathBuf::from(&relative),
            hidden: relative.starts_with('.'),
            relative,
        }
    }

    fn matches(query: &str, relative: &str) -> Option<(Rank, Vec<Range<usize>>)> {
        let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
        let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
        path_match(&pattern, &mut matcher, &entry(relative), &mut Vec::new())
    }

    #[test]
    fn shortcut_search_matches_command_names_and_bindings() {
        let shortcuts = [
            Shortcut {
                description: "Next tab".into(),
                binding: "Ctrl+A then n/j".into(),
            },
            Shortcut {
                description: "Focus the preview pane".into(),
                binding: "Ctrl+S then v".into(),
            },
            Shortcut {
                description: "Reload page".into(),
                binding: "In the preview pane: !".into(),
            },
        ];
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut search = |query| search_shortcuts(&shortcuts, query, &mut matcher);
        assert_eq!(search(""), [0, 1, 2]);
        assert_eq!(search("NXT TAB ctrl+a"), [0]);
        assert_eq!(search("fcs prvw"), [1]);
        assert_eq!(search("!"), [2]);
        assert!(search("zzz").is_empty());
    }

    fn palette_rows(keymap: &Keymap) -> Vec<Row> {
        keymap
            .palette_entries()
            .into_iter()
            .map(|entry| Row {
                label: entry.description.to_owned(),
                detail: keymap.binding(entry.scope, entry.command),
                choice: Choice::Command(entry.command),
            })
            .collect()
    }

    fn ordered(rows: &[Row], recent: &[Command], query: &str) -> Vec<(Command, bool)> {
        order_commands(rows, recent, query)
            .into_iter()
            .map(|(index, recent)| match rows[index].choice {
                Choice::Command(command) => (command, recent),
                _ => unreachable!(),
            })
            .collect()
    }

    #[test]
    fn palette_lists_recent_commands_first_then_table_order() {
        let keymap = Keymap::default();
        let rows = palette_rows(&keymap);
        let table: Vec<_> = ordered(&rows, &[], "")
            .into_iter()
            .map(|(command, recent)| {
                assert!(!recent);
                command
            })
            .collect();
        assert_eq!(
            table[..3],
            [Command::BoxNew, Command::BoxPick, Command::BoxDetach]
        );
        assert_eq!(table.len(), rows.len());
        let recent = [Command::TabNext, Command::BoxRuns, Command::Reload];
        let order = ordered(&rows, &recent, "");
        assert_eq!(
            order[..4],
            [
                (Command::TabNext, true),
                (Command::BoxRuns, true),
                (Command::Reload, true),
                (Command::BoxNew, false),
            ]
        );
        assert_eq!(order.len(), rows.len());
        assert_eq!(
            order
                .iter()
                .filter(|(command, _)| *command == Command::BoxRuns)
                .count(),
            1
        );
    }

    #[test]
    fn palette_filters_fuzzily_and_boosts_recent_commands() {
        let keymap = Keymap::default();
        let rows = palette_rows(&keymap);
        let order = ordered(&rows, &[], "colsch");
        assert_eq!(order.first(), Some(&(Command::BoxColorScheme, false)));
        assert!(ordered(&rows, &[], "zzzzqx").is_empty());
        // "tab" matches many commands; a recent one moves up among them.
        let plain = ordered(&rows, &[], "new tab");
        assert_eq!(plain.first(), Some(&(Command::TabNew, false)));
        let worktree = plain
            .iter()
            .position(|(command, _)| *command == Command::TabWorktree)
            .unwrap();
        let boosted = ordered(&rows, &[Command::TabWorktree], "new tab");
        let boosted_at = boosted
            .iter()
            .position(|(command, _)| *command == Command::TabWorktree)
            .unwrap();
        assert!(
            boosted_at < worktree || worktree == 0,
            "{plain:?} {boosted:?}"
        );
        assert!(boosted[boosted_at].1);
    }

    #[test]
    fn palette_ranks_like_other_pickers() {
        let keymap = Keymap::default();
        let rows = palette_rows(&keymap);
        // Label matches beat matches that need the binding, even for a recent command.
        let order = ordered(&rows, &[Command::BoxNew], "reload");
        assert_eq!(order.first(), Some(&(Command::Reload, false)));
        // A binding-only query still finds the command.
        let order = ordered(&rows, &[], "ctrl+b then p");
        assert!(
            order
                .iter()
                .all(|(command, _)| *command != Command::Palette)
        );
        assert!(!order.is_empty());
        // fzf syntax: an exact-prefix atom.
        let order = ordered(&rows, &[], "^quit");
        assert_eq!(order, [(Command::Quit, false)]);
    }

    #[test]
    fn path_match_prefers_the_final_component() {
        let rank = |query, relative| matches(query, relative).unwrap().0;
        assert!(rank("pwo", "src/power-overwhelming") > rank("pwo", "Pictures/wallpapers/old"));
        assert!(matches("src pwo", "src/power-overwhelming").is_some());
        assert!(matches("src zzz", "src/power").is_none());
    }

    #[test]
    fn path_match_supports_fzf_syntax_and_smart_case() {
        assert!(matches("!wall pwo", "Pictures/wallpapers/old").is_none());
        assert!(matches("!wall pwo", "src/power-overwhelming").is_some());
        assert!(matches("^src", "src/power").is_some());
        assert!(matches("^power", "src/power").is_none());
        assert!(matches("'over", "src/power-overwhelming").is_some());
        assert!(matches("Pow", "src/power").is_none());
        assert!(matches("pow", "src/Power").is_some());
    }

    #[test]
    fn path_match_reports_byte_ranges() {
        assert_eq!(matches("pow src", "src/power").unwrap().1, vec![0..3, 4..7]);
        assert_eq!(matches("pow", "src/power").unwrap().1, vec![4..7]);
        assert_eq!(matches("éa", "xéya").unwrap().1, vec![1..3, 4..5]);
    }

    #[test]
    fn walk_skips_pruned_hidden_and_gitignored_directories() {
        let root = tempfile::tempdir().unwrap();
        for directory in [
            "a/b/c",
            "a/target/inner",
            ".hidden/inner",
            "z",
            "repo/.git",
            "repo/src",
            "repo/ignored",
        ] {
            std::fs::create_dir_all(root.path().join(directory)).unwrap();
        }
        std::fs::write(root.path().join("a/file"), "").unwrap();
        std::fs::write(root.path().join("repo/.gitignore"), "ignored/\n").unwrap();
        let (sender, receiver) = async_channel::unbounded();
        walk(root.path(), super::WALK_DEPTH, &sender);
        drop(sender);
        let mut found: Vec<String> = std::iter::from_fn(|| receiver.try_recv().ok())
            .flatten()
            .map(|entry| entry.relative.replace(MAIN_SEPARATOR, "/"))
            .collect();
        found.sort();
        assert_eq!(
            found,
            [
                ".hidden", "a", "a/b", "a/b/c", "a/target", "repo", "repo/src", "z"
            ]
        );
    }
}

/// Fuzzy-filters rows with nucleo (fzf syntax, smart case). Rows whose label matches on its own
/// rank first, so run titles beat accidental hits inside UUIDs or details. Ties keep list order.
fn fuzzy_rows(rows: &[Row], query: &str) -> Vec<Row> {
    if query.trim().is_empty() {
        return rows.to_vec();
    }
    let mut ranked = fuzzy_rank(rows, query);
    ranked.sort_by_key(|&(index, rank)| (Reverse(rank), index));
    ranked
        .into_iter()
        .map(|(index, _)| rows[index].clone())
        .collect()
}

/// The rows that the fuzzy `query` matches, in list order, each with its rank: whether the
/// label matches on its own, then the nucleo score. Higher ranks sort first.
fn fuzzy_rank(rows: &[Row], query: &str) -> Vec<(usize, (bool, u32))> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buffer = Vec::new();
    rows.iter()
        .enumerate()
        .filter_map(|(index, row)| {
            let label = pattern.score(Utf32Str::new(row.label.trim(), &mut buffer), &mut matcher);
            let rank = match label {
                Some(score) => (true, score),
                None => {
                    let mut text = format!("{} {}", row.label, row.detail);
                    if let Choice::ColorScheme(scheme) = &row.choice {
                        for alias in scheme.aliases.iter() {
                            text.push(' ');
                            text.push_str(alias);
                        }
                    }
                    (
                        false,
                        pattern.score(Utf32Str::new(&text, &mut buffer), &mut matcher)?,
                    )
                }
            };
            Some((index, rank))
        })
        .collect()
}

/// Byte ranges of `text` that the fuzzy `query` matches, for highlighting. Empty when it doesn't match.
fn fuzzy_highlights(query: &str, text: &str) -> Vec<Range<usize>> {
    let query = query.trim();
    if query.is_empty() || text.is_empty() {
        return Vec::new();
    }
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut indices = Vec::new();
    let mut buffer = Vec::new();
    match pattern.indices(Utf32Str::new(text, &mut buffer), &mut matcher, &mut indices) {
        Some(_) => byte_ranges(text, indices),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod fuzzy_row_tests {
    use super::{Choice, Row, fuzzy_rows};

    fn row(label: &str, detail: &str) -> Row {
        Row {
            label: label.into(),
            detail: detail.into(),
            choice: Choice::NewBox,
        }
    }

    fn labels(rows: &[Row], query: &str) -> Vec<String> {
        fuzzy_rows(rows, query)
            .into_iter()
            .map(|row| row.label)
            .collect()
    }

    #[test]
    fn matches_partial_and_scattered_words() {
        let rows = [row("Acceptance tests", ""), row("Refactor picker", "")];
        assert_eq!(labels(&rows, "accep"), ["Acceptance tests"]);
        assert_eq!(labels(&rows, "rfpk"), ["Refactor picker"]);
        assert_eq!(labels(&rows, "acc tst"), ["Acceptance tests"]);
    }

    #[test]
    fn highlights_cover_the_matched_characters() {
        assert_eq!(
            super::fuzzy_highlights("acc", "Acceptance"),
            [std::ops::Range { start: 0, end: 3 }]
        );
        assert_eq!(super::fuzzy_highlights("rfp", "Refactor picker").len(), 3);
        assert!(super::fuzzy_highlights("zzz", "Acceptance").is_empty());
    }

    #[test]
    fn label_matches_beat_detail_matches() {
        let rows = [row("Other", "acceptance"), row("Acceptance 2", "")];
        assert_eq!(labels(&rows, "acceptance"), ["Acceptance 2", "Other"]);
    }
}
