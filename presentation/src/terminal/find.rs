//! Find in the terminal: an incremental search over the screen and its scrollback, with
//! matches highlighted under the glyphs.
//!
//! Adapted from herdr-app `src/vt.rs` (`start_search` through `read_search_data`) and the
//! search painting and find bar in `src/terminal.rs` (herdr-app 3e6d564), licensed under
//! Apache-2.0.

use super::TerminalView;
use ghostty_vt::{
    Terminal,
    screen::CellWide,
    search::{Search, Status},
    terminal::PointSpace,
};
use gpui::{Context, Keystroke};
use std::{ops::Range, time::Duration};

/// How long a search that isn't caught up waits before its next pump.
const PUMP_INTERVAL: Duration = Duration::from_millis(16);

impl TerminalView {
    /// Opens the find bar. Keys then edit the query until it closes.
    pub fn open_find(&mut self, cx: &mut Context<Self>) {
        self.finder.open(&self.terminal);
        self.after_find(cx);
    }

    pub fn close_find(&mut self, cx: &mut Context<Self>) {
        self.finder.close();
        cx.notify();
    }

    pub fn finding(&self) -> bool {
        self.finder.is_open()
    }

    /// FindNext (`previous` false) or FindPrev.
    pub fn find_step(&mut self, previous: bool, cx: &mut Context<Self>) {
        self.finder.step(&self.terminal, previous);
        self.after_find(cx);
    }

    /// A key typed into the find bar; whether it edited the query.
    pub fn find_key(&mut self, stroke: &Keystroke, cx: &mut Context<Self>) -> bool {
        let mods = stroke.modifiers;
        if mods.alt || mods.platform {
            return false;
        }
        let Some(query) = edit(
            self.finder.query(),
            &stroke.key,
            stroke.key_char.as_deref(),
            mods.control,
        ) else {
            return false;
        };
        self.finder.set_query(&self.terminal, query);
        self.after_find(cx);
        true
    }

    pub(super) fn find_bar(&self) -> Option<(String, String)> {
        self.finder
            .is_open()
            .then(|| (self.finder.query().to_owned(), self.finder.label()))
    }

    pub(super) fn find_spans(&self) -> &[Span] {
        self.finder.spans()
    }

    /// Takes a new frame, since a step may scroll, and keeps pumping until the search is
    /// caught up.
    fn after_find(&mut self, cx: &mut Context<Self>) {
        self.refresh();
        cx.notify();
        if self.find_pumping || !self.finder.pump(&self.terminal) {
            return;
        }
        self.find_pumping = true;
        cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor().timer(PUMP_INTERVAL).await;
                let more = view.update(cx, |view, cx| {
                    let more = view.finder.pump(&view.terminal);
                    view.refresh();
                    cx.notify();
                    view.find_pumping = more;
                    more
                });
                if !more.unwrap_or(false) {
                    break;
                }
            }
        })
        .detach();
    }
}

/// Ticks one pump may run. Each tick is bounded, so this caps the work per frame on a large
/// scrollback; the view pumps again until the search completes.
const TICKS_PER_PUMP: usize = 64;

/// One row's run of a match, in viewport cells.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Span {
    pub row: usize,
    pub columns: Range<usize>,
    /// Whether this is the match FindNext or FindPrev last moved to.
    pub selected: bool,
}

#[derive(Default)]
pub(super) struct Finder {
    search: Option<Search>,
    query: String,
    spans: Vec<Span>,
    total: usize,
    selected: Option<usize>,
    complete: bool,
    /// A step asked for before indexing finished: `true` for previous. Ghostty's select
    /// catches up synchronously, which would stall the UI on a large scrollback.
    pending: Option<bool>,
}

impl Finder {
    pub fn is_open(&self) -> bool {
        self.search.is_some()
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn spans(&self) -> &[Span] {
        &self.spans
    }

    pub fn open(&mut self, terminal: &Terminal) {
        if self.search.is_none() {
            self.search = Search::new(terminal).ok();
            self.set_query(terminal, self.query.clone());
        }
    }

    pub fn close(&mut self) {
        *self = Self {
            query: std::mem::take(&mut self.query),
            ..Self::default()
        };
    }

    /// A new terminal replaced the one the search was bound to.
    pub fn rebind(&mut self, terminal: &Terminal) {
        if self.search.take().is_some() {
            self.open(terminal);
        }
    }

    pub fn set_query(&mut self, terminal: &Terminal, query: String) {
        self.query = query;
        let Some(search) = self.search.as_mut() else {
            return;
        };
        let needle = Some(self.query.as_str()).filter(|q| !q.is_empty());
        let _ = search.set_needle(terminal, needle);
        self.total = 0;
        self.selected = None;
        self.complete = needle.is_none();
        self.pending = None;
        self.spans.clear();
        self.pump(terminal);
    }

    /// Moves to the next (older) or previous (newer) match, scrolling it into view.
    pub fn step(&mut self, terminal: &Terminal, previous: bool) {
        if self.query.is_empty() {
            return;
        }
        if !self.complete {
            self.pending = Some(previous);
            return;
        }
        let Some(search) = self.search.as_mut() else {
            return;
        };
        let _ = if previous {
            search.select_prev(terminal)
        } else {
            search.select_next(terminal)
        };
        self.read(terminal);
    }

    /// Takes in terminal changes and makes bounded progress. Whether the search still
    /// needs more pumps to finish.
    pub fn pump(&mut self, terminal: &Terminal) -> bool {
        let Some(search) = self.search.as_mut().filter(|_| !self.query.is_empty()) else {
            return false;
        };
        let _ = search.feed(terminal);
        let mut status = Status::Complete;
        for _ in 0..TICKS_PER_PUMP {
            status = match search.tick() {
                Ok(Status::FeedRequired) => {
                    let _ = search.feed(terminal);
                    search.status().unwrap_or(Status::Complete)
                }
                Ok(status) => status,
                Err(_) => Status::Complete,
            };
            if status == Status::Complete {
                break;
            }
        }
        self.complete = status == Status::Complete;
        if self.complete
            && let Some(previous) = self.pending.take()
        {
            self.step(terminal, previous);
        } else {
            self.read(terminal);
        }
        !self.complete
    }

    /// Reads the counts and the viewport's matches. Matches are snapshots valid only until the
    /// terminal changes, so they turn into cell spans at once.
    fn read(&mut self, terminal: &Terminal) {
        let Some(search) = self.search.as_ref() else {
            return;
        };
        self.total = search.total_matches().unwrap_or(0);
        self.selected = search.selected_index().ok().flatten();
        let selected = search.selected_match(terminal).ok().flatten();
        let rows = terminal.rows().map_or(0, usize::from);
        let cols = terminal.cols().map_or(0, usize::from);
        let point = |grid_ref: &ghostty_vt::screen::GridRef<'_>| {
            terminal
                .point_from_grid_ref(grid_ref, PointSpace::Viewport)
                .ok()
                .flatten()
        };
        let mut spans = Vec::new();
        for matched in search.viewport_matches(terminal).unwrap_or_default() {
            let is_selected = selected
                .as_ref()
                .is_some_and(|s| matched.equals(terminal, s).unwrap_or(false));
            let (first, last) = (matched.start(), matched.end());
            let (Some(mut start), Some(mut end)) = (point(&first), point(&last)) else {
                continue;
            };
            let mut last_ref = last;
            if (start.y, start.x) > (end.y, end.x) {
                std::mem::swap(&mut start, &mut end);
                last_ref = first;
            }
            let (start_row, end_row) = (start.y as usize, end.y as usize);
            let mut end_column = usize::from(end.x);
            // A match ends on a wide character's first cell; cover its second too.
            if last_ref
                .cell()
                .and_then(|cell| cell.wide())
                .is_ok_and(|wide| wide == CellWide::Wide)
            {
                end_column += 1;
            }
            for row in start_row..=end_row.min(rows.saturating_sub(1)) {
                let from = if row == start_row {
                    usize::from(start.x)
                } else {
                    0
                };
                let to = if row == end_row { end_column + 1 } else { cols };
                let to = to.min(cols);
                if from < to {
                    spans.push(Span {
                        row,
                        columns: from..to,
                        selected: is_selected,
                    });
                }
            }
        }
        self.spans = spans;
    }

    /// The match count for the find bar, such as `2/5`.
    pub fn label(&self) -> String {
        if self.query.is_empty() {
            return String::new();
        }
        let searching = if self.complete { "" } else { "…" };
        match self.selected {
            Some(index) => format!("{}/{}{searching}", index + 1, self.total),
            None => format!("{}{searching}", self.total),
        }
    }
}

/// The query after a key typed into the find bar, or `None` when the key doesn't edit it.
pub(super) fn edit(
    query: &str,
    key: &str,
    key_char: Option<&str>,
    control: bool,
) -> Option<String> {
    match key {
        "backspace" if control => Some(String::new()),
        "backspace" => {
            let mut query = query.to_owned();
            query.pop();
            Some(query)
        }
        _ if control => None,
        _ => key_char
            .filter(|text| !text.chars().any(char::is_control))
            .map(|text| format!("{query}{text}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ghostty_vt::terminal::{Options, ScrollViewport};

    fn terminal(cols: u16, rows: u16, text: &str) -> Terminal {
        let mut terminal = Terminal::new(Options {
            cols,
            rows,
            max_scrollback: 1000,
        })
        .unwrap();
        terminal.vt_write(text.as_bytes());
        terminal
    }

    fn finish(finder: &mut Finder, terminal: &Terminal) {
        while finder.pump(terminal) {}
    }

    #[test]
    fn counts_and_highlights_matches_in_view() {
        let terminal = terminal(20, 3, "one two\r\nthree TWO\r\n");
        let mut finder = Finder::default();
        finder.open(&terminal);
        finder.set_query(&terminal, "two".into());
        finish(&mut finder, &terminal);
        assert_eq!(finder.label(), "2");
        let spans: Vec<_> = finder
            .spans()
            .iter()
            .map(|s| (s.row, s.columns.clone()))
            .collect();
        assert!(spans.contains(&(0, 4..7)), "{spans:?}");
        assert!(spans.contains(&(1, 6..9)), "{spans:?}");
    }

    #[test]
    fn next_and_previous_move_the_selected_match() {
        let terminal = terminal(20, 3, "ab\r\nab\r\n");
        let mut finder = Finder::default();
        finder.open(&terminal);
        finder.set_query(&terminal, "ab".into());
        finish(&mut finder, &terminal);
        finder.step(&terminal, false);
        assert_eq!(finder.label(), "1/2");
        let selected: Vec<_> = finder.spans().iter().filter(|s| s.selected).collect();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].row, 1, "the newest match comes first");
        finder.step(&terminal, false);
        assert_eq!(finder.label(), "2/2");
        finder.step(&terminal, true);
        assert_eq!(finder.label(), "1/2");
    }

    #[test]
    fn a_match_ending_on_a_wide_character_covers_both_cells() {
        let terminal = terminal(20, 2, "x漢字y");
        let mut finder = Finder::default();
        finder.open(&terminal);
        finder.set_query(&terminal, "x漢字".into());
        finish(&mut finder, &terminal);
        assert_eq!(finder.spans()[0].columns, 0..5);
    }

    #[test]
    fn stepping_scrolls_a_match_in_history_into_view() {
        let mut text = String::from("needle\r\n");
        for line in 0..20 {
            text.push_str(&format!("line {line}\r\n"));
        }
        let mut terminal = terminal(20, 4, &text);
        terminal.scroll_viewport(ScrollViewport::Bottom);
        let mut finder = Finder::default();
        finder.open(&terminal);
        finder.set_query(&terminal, "needle".into());
        finish(&mut finder, &terminal);
        assert!(finder.spans().is_empty());
        finder.step(&terminal, false);
        assert_eq!(finder.spans().len(), 1);
        assert!(finder.spans()[0].selected);
    }

    #[test]
    fn clearing_the_query_clears_matches() {
        let terminal = terminal(20, 2, "abc");
        let mut finder = Finder::default();
        finder.open(&terminal);
        finder.set_query(&terminal, "b".into());
        finish(&mut finder, &terminal);
        assert_eq!(finder.spans().len(), 1);
        finder.set_query(&terminal, String::new());
        assert!(finder.spans().is_empty());
        assert_eq!(finder.label(), "");
    }

    #[test]
    fn typing_edits_the_query() {
        assert_eq!(edit("ab", "c", Some("c"), false).as_deref(), Some("abc"));
        assert_eq!(edit("ab", "backspace", None, false).as_deref(), Some("a"));
        assert_eq!(edit("ab", "backspace", None, true).as_deref(), Some(""));
        assert_eq!(edit("ab", "c", Some("c"), true), None);
        assert_eq!(edit("ab", "left", None, false), None);
    }
}
