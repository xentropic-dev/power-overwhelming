use crate::{gui::agent_tree::AgentTree, keymap::Shortcut};
use domain::session::Session;
use gpui::{prelude::*, *};
use ignore::{DirEntry, ParallelVisitor, ParallelVisitorBuilder, WalkBuilder, WalkState};
use infrastructure::{client::Run, theme::Scheme};
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{AtomKind, CaseMatching, Normalization, Pattern},
};
use std::{
    cmp::Reverse,
    ops::Range,
    path::{MAIN_SEPARATOR, Path, PathBuf, is_separator},
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

const MAX_COMPLETIONS: usize = 50;
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
    Worktrees(String),
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
    Sessions(String),
    End(String),
    /// Keyboard shortcuts, generated from the keymap.
    Help(Vec<Shortcut>),
    /// Links visible in the active terminal.
    Links,
    /// Shell integration setup, with the lines that explain what it changes. `first_run`
    /// finishes onboarding when it closes; otherwise it returns to the previous view.
    Setup { lines: Vec<String>, first_run: bool },
    ColorSchemes {
        first_run: bool,
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
    Worktree(PathBuf),
    Session(Session, PathBuf),
    /// Finish first-run setup, installing the shell hook when true.
    Setup(bool),
    ColorScheme(&'static Scheme),
    Link(String),
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
    pub focus: FocusHandle,
    query: String,
    cursor: usize,
    select_all: bool,
    edited: bool,
    selected: usize,
    scroll: ScrollHandle,
    theme_scroll: UniformListScrollHandle,
    listing: Option<Listing>,
    walk_request: Option<PathBuf>,
    completions: Vec<Completion>,
    help_matches: Vec<usize>,
    matcher: Matcher,
}

impl Kind {
    fn title(&self) -> &str {
        match self {
            Self::Repository { .. } => "New box",
            Self::Boxes => "Boxes",
            Self::Worktrees(_) => "New tab in a worktree",
            Self::Tabs => "Tabs",
            Self::Runs { .. } => "DAG runs",
            Self::RunLink => "Open DAG run",
            Self::Agents { .. } => "Agent tree",
            Self::StatusActions(title) => title,
            Self::Sessions(_) => "Recover a session",
            Self::End(_) => "End terminal session?",
            Self::Help(_) => "Keyboard shortcuts",
            Self::Links => "Open a link",
            Self::Setup { first_run: true, .. } => "Welcome to Power Overwhelming",
            Self::Setup { first_run: false, .. } => "Shell integration",
            Self::ColorSchemes { first_run: true } => "Choose a color scheme",
            Self::ColorSchemes { first_run: false } => "Color schemes",
        }
    }

    fn placeholder(&self) -> Option<&'static str> {
        match self {
            Self::Repository { .. } => Some("Repository or worktree path"),
            Self::Boxes => Some("Search boxes"),
            Self::Worktrees(_) => Some("Search worktrees"),
            Self::Tabs => Some("Search by tab number or name"),
            Self::Runs { .. } => Some("Search runs"),
            Self::Agents { .. } => Some("Search agents"),
            Self::Sessions(_) => Some("Search sessions"),
            Self::Help(_) => Some("Search shortcuts"),
            Self::Links => Some("Search links"),
            Self::ColorSchemes { .. } => Some("Search color schemes"),
            Self::End(_) | Self::Setup { .. } | Self::StatusActions(_) | Self::RunLink => None,
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
        Self {
            id,
            kind,
            rows,
            message: None,
            waiting: false,
            loading: false,
            accept_pending: false,
            focus,
            query: String::new(),
            cursor: 0,
            select_all: false,
            edited: false,
            selected: 0,
            scroll: ScrollHandle::new(),
            theme_scroll: UniformListScrollHandle::new(),
            listing: None,
            walk_request: None,
            completions: Vec::new(),
            help_matches,
            matcher: Matcher::new(matcher_config),
        }
    }

    pub fn suggest_directory(&mut self, directory: PathBuf) {
        if let Kind::Repository { directory: base } = &mut self.kind {
            *base = Some(directory.clone());
            if !self.edited {
                self.query = directory.to_string_lossy().into_owned();
                self.cursor = self.query.len();
                self.select_all = true;
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
            self.walk_request = Some(root);
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

    /// The directory the caller should walk with [`walk`], if the query moved to a new parent.
    pub fn take_walk_request(&mut self) -> Option<PathBuf> {
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
        self.select_all = false;
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
                .map(|(agent, depth, marker)| Row {
                    label: format!("{}{marker} {}", " ".repeat(depth), agent.label),
                    detail: format!(
                        "{}{} · {}",
                        " ".repeat(depth),
                        agent.provider.name(),
                        agent.worktree.display(),
                    ),
                    choice: Choice::Agent(agent.id.clone()),
                })
                .collect();
        }
        if matches!(self.kind, Kind::Repository { .. }) && !self.query.is_empty() {
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
        let mut rows: Vec<Row> = self
            .rows
            .iter()
            .filter(|row| {
                query.split_whitespace().all(|word| {
                    row.label.to_lowercase().contains(word)
                        || row.detail.to_lowercase().contains(word)
                        || matches!(row.choice, Choice::ColorScheme(scheme) if scheme.aliases.iter().any(|alias| alias.to_lowercase().contains(word)))
                })
            })
            .cloned()
            .collect();
        if matches!(self.kind, Kind::Runs { .. }) {
            // Run IDs are UUIDs, so short queries match them by accident. Prefer title matches.
            rows.sort_by_key(|row| !title_matches(&row.label, &query));
        }
        rows
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

    pub fn set_runs(&mut self, runs: Vec<Run>) {
        let previous = self.selection();
        let Kind::Runs { runs: listed, .. } = &mut self.kind else {
            return;
        };
        self.rows = runs
            .iter()
            .map(|run| Row {
                label: run.title.clone(),
                detail: run.id.clone(),
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

    pub fn set_agent_tree(&mut self, mut new_tree: AgentTree) {
        if let Kind::Agents { tree, .. } = &mut self.kind {
            self.selected = new_tree.search(&self.query, &mut self.matcher);
            *tree = new_tree;
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
        if self.select_all {
            self.query.clear();
            self.cursor = 0;
            self.select_all = false;
        }
        self.query.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.edited = true;
        self.accept_pending = false;
        if !self.loading {
            self.message = None;
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
            _ => self.refresh_completions(),
        }
    }

    /// Whether the picker has a search field. Dialogs without one take plain-letter bindings.
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

    /// Completes the selected directory in the new box picker. Returns false elsewhere.
    pub fn complete_selection(&mut self) -> bool {
        if self.waiting || !matches!(self.kind, Kind::Repository { .. }) {
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
                    self.select_all = true;
                    self.edited = true;
                }
                "v" => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                        self.replace_selection(&text.replace(['\r', '\n', '\0'], ""));
                    }
                }
                "c" if self.select_all => {
                    cx.write_to_clipboard(ClipboardItem::new_string(self.query.clone()));
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
                    "left" if self.select_all => 0,
                    "right" if self.select_all => self.query.len(),
                    "left" => self.before_cursor(),
                    _ => self.after_cursor(),
                };
                self.select_all = false;
                self.edited = true;
            }
            "backspace" | "delete" => {
                if !self.select_all {
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
        let no_matches = visible.is_empty()
            && self.help_matches.is_empty()
            && !self.query.is_empty()
            && self.message.is_none()
            && !self.searching();
        let has_results = !visible.is_empty();
        let completing = matches!(self.kind, Kind::Repository { .. }) && !self.query.is_empty();
        let theme_list = matches!(self.kind, Kind::ColorSchemes { .. }).then(|| {
            let rows = std::mem::take(&mut visible);
            let selected = self.selected;
            let select = select.clone();
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
                            .child(div().flex_1().min_w_0().truncate().child(row.label.clone()))
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
            let highlights = completing
                .then(|| self.completions.get(index))
                .flatten()
                .map(|completion| completion.highlights.clone())
                .unwrap_or_default();
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
                            .child(row.detail),
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
                    .child(div().min_w_0().child(shortcut.description))
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
                        panel.child(if self.select_all && !self.query.is_empty() {
                            field.child(
                                div()
                                    .bg(rgb(colors.raised))
                                    .whitespace_nowrap()
                                    .child(self.query.clone()),
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
                            && (help.is_some() || has_results || !self.query.is_empty()),
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
                    .when(matches!(self.kind, Kind::End(_)), |panel| {
                        panel.child("Enter ends the session and removes its tab. Escape cancels.")
                    })
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
pub fn walk(root: &Path, sender: &async_channel::Sender<Vec<Entry>>) {
    let found = AtomicUsize::new(0);
    WalkBuilder::new(root)
        .hidden(false)
        .follow_links(false)
        .max_depth(Some(WALK_DEPTH))
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

#[cfg(test)]
mod tests {
    use super::{Entry, Rank, Shortcut, path_match, search_shortcuts, walk};
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
                description: "Next tab",
                binding: "Ctrl+A then n/j".into(),
            },
            Shortcut {
                description: "Focus the preview pane",
                binding: "Ctrl+S then v".into(),
            },
            Shortcut {
                description: "Reload page",
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
        walk(root.path(), &sender);
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

fn title_matches(title: &str, query: &str) -> bool {
    let title = title.to_lowercase();
    query.split_whitespace().all(|word| title.contains(word))
}

#[cfg(test)]
mod run_search_tests {
    use super::title_matches;

    #[test]
    fn title_matches_need_every_word_in_the_title() {
        assert!(title_matches("Acceptance 2", "acceptance 2"));
        assert!(!title_matches("Acceptance 1", "acceptance 2"));
    }
}
