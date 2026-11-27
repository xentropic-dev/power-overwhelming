//! URL scanning and wrapped cell mapping adapted from herdr-app `src/links.rs`
//! and `src/vt.rs` (Apache-2.0).

use super::{
    TerminalView,
    frame::{CellPosition, Frame, Row},
};
use crate::keymap::Command;
use domain::{
    artifact::{TargetLocation, artifact_id},
    link::{DAG_URI_PREFIX, dag_run_id},
};
use ghostty_vt::screen::CellWide;
use gpui::{Bounds, Modifiers, Pixels, Point};
use regex::Regex;
use std::{
    ops::Range,
    path::Path,
    sync::{Arc, LazyLock},
};
use url::Url;

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum LinkAction<'a> {
    OpenDag(&'a str),
    OpenBrowser,
    ResolveArtifact,
}

pub(crate) fn link_action(target: &str) -> LinkAction<'_> {
    if let Some(run) = dag_run_id(target) {
        LinkAction::OpenDag(run)
    } else if is_web_url(target)
        && !matches!(
            TargetLocation::parse(target, Path::new("")),
            Ok(TargetLocation::Loopback(_))
        )
    {
        LinkAction::OpenBrowser
    } else {
        LinkAction::ResolveArtifact
    }
}

impl TerminalView {
    /// Numbers up to nine links on screen, newest output first, with a footer built from
    /// the number of hints.
    pub fn show_hints(&mut self, footer: impl FnOnce(usize) -> String) {
        self.hints = hint_positions(&self.frame);
        self.hint_footer = Some(footer(self.hints.len()));
    }

    pub fn clear_hints(&mut self) {
        self.hints.clear();
        self.hint_footer = None;
    }

    /// The link labelled `number`, counting from 1.
    pub fn hint(&self, number: usize) -> Option<String> {
        let (_, link) = self.hints.get(number.checked_sub(1)?)?;
        Some(link.to_string())
    }

    /// Links on screen, top to bottom and without repeats, for keyboard link picking.
    pub fn visible_links(&self) -> Vec<String> {
        let mut links: Vec<String> = Vec::new();
        for link in self.frame.rows.iter().flat_map(|row| {
            row.links
                .iter()
                .filter_map(|span| row.cells[span.start].link.as_deref())
        }) {
            if links.last().map(String::as_str) != Some(link)
                && !links.iter().any(|seen| seen == link)
            {
                links.push(link.to_owned());
            }
        }
        links
    }

    fn link_at(&self, cell: CellPosition) -> Option<&Arc<str>> {
        self.frame
            .rows
            .get(cell.row)?
            .cells
            .get(cell.column)?
            .link
            .as_ref()
    }

    fn link_at_point(&self, point: Point<Pixels>) -> Option<&Arc<str>> {
        if !self.grid.bounds.contains(&point) {
            return None;
        }
        let cell = self.at(point);
        let metrics = self.grid.metrics;
        let origin = metrics.cell_origin(self.grid.bounds.origin, cell.row, cell.column);
        if !Bounds::new(origin, metrics.size).contains(&point) {
            return None;
        }
        self.link_at(cell)
    }

    pub(super) fn clickable_link_at(
        &self,
        point: Point<Pixels>,
        modifiers: Modifiers,
    ) -> Option<&Arc<str>> {
        let link = self.link_at_point(point)?;
        match Command::terminal_link(modifiers, link_action(link) == LinkAction::OpenBrowser) {
            Some(Command::TerminalOpenLink) => Some(link),
            _ => None,
        }
    }

    /// The link under the pointer that a click would open with these modifiers: web links
    /// need Ctrl held, previews and DAG links open on a plain click.
    pub(super) fn hovered_span(
        &self,
        modifiers: Modifiers,
    ) -> Option<(CellPosition, CellPosition)> {
        self.clickable_link_at(self.pointer?, modifiers)?;
        self.pointer_span()
    }

    fn pointer_span(&self) -> Option<(CellPosition, CellPosition)> {
        let point = self.pointer?;
        self.link_at_point(point)?;
        self.frame.link_span(self.at(point))
    }

    pub(crate) fn pointer_over_link(&self) -> bool {
        self.pointer_span().is_some()
    }

    pub(super) fn set_pointer(&mut self, position: Option<Point<Pixels>>) -> bool {
        let previous = self.pointer_span();
        self.pointer = position;
        previous != self.pointer_span()
    }

    pub(super) fn clear_pointer(&mut self) -> bool {
        let moved_off_link = self.set_pointer(None);
        let released_link = self.pressed_link.take().is_some();
        moved_off_link || released_link
    }

    pub(super) fn pressed_span(&self) -> Option<(CellPosition, CellPosition)> {
        let pressed = self.pressed_link.as_ref()?;
        let (start, end) = self.selection?;
        let link = self.link_at(start)?;
        if start == end && pressed == link {
            self.frame.link_span(start)
        } else {
            None
        }
    }
}

/// Extent of a hovered or pressed link. Spans on either side of a soft wrap can
/// belong to rows captured in different frames.
impl Frame {
    pub(super) fn link_span(&self, at: CellPosition) -> Option<(CellPosition, CellPosition)> {
        let row = self.rows.get(at.row)?;
        let part = row.links.iter().find(|span| span.contains(&at.column))?;
        let uri = row.cells[part.start].link.as_deref()?;
        let mut start = CellPosition {
            row: at.row,
            column: part.start,
        };
        let mut end = CellPosition {
            row: at.row,
            column: part.end - 1,
        };
        while start.row > 0
            && self.rows[start.row].continues_link
            && self.rows[start.row].links.first().map(|span| span.start) == Some(start.column)
        {
            let prior = &self.rows[start.row - 1];
            if !prior.ends_in_link
                || prior.cells[prior.links.last()?.start].link.as_deref() != Some(uri)
            {
                break;
            }
            start = CellPosition {
                row: start.row - 1,
                column: prior.links.last()?.start,
            };
        }
        while self.rows[end.row].ends_in_link
            && self.rows[end.row].links.last().map(|span| span.end - 1) == Some(end.column)
        {
            let Some(next) = self.rows.get(end.row + 1) else {
                break;
            };
            if !next.continues_link
                || next.cells[next.links.first()?.start].link.as_deref() != Some(uri)
            {
                break;
            }
            end = CellPosition {
                row: end.row + 1,
                column: next.links.first()?.end - 1,
            };
        }
        Some((start, end))
    }
}

static MARKDOWN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\[[^\]]+\]\(((?:[^()\r\n]|\([^()\r\n]*\))+)\)|"([^"]+)"|'([^']+)'|`([^`]+)`"#)
        .unwrap()
});
static WEB_SCHEME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bhttps?://").unwrap());
static PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r#"(?i)(?:/|\.{1,2}/)[^<>\[\]()"'`\r\n]*?\.(?:png|jpe?g|webp|mp4|webm|html?)\b|[^\s<>\[\]()"'`,;]+"#,
).unwrap()
});

pub(super) fn candidate(value: &str) -> bool {
    if has_web_scheme(value) {
        is_web_url(value)
    } else {
        preview_candidate(value)
    }
}

fn has_web_scheme(value: &str) -> bool {
    value.split_once("://").is_some_and(|(scheme, _)| {
        scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
    })
}

fn is_web_url(value: &str) -> bool {
    has_web_scheme(value)
        && !value
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
        && Url::parse(value).is_ok()
}

fn preview_candidate(value: &str) -> bool {
    if value.starts_with(DAG_URI_PREFIX) {
        return dag_run_id(value).is_some();
    }
    let value = value.to_ascii_lowercase();
    artifact_id(&value).is_some()
        || value.starts_with("file://")
        || [
            ".png", ".jpg", ".jpeg", ".webp", ".mp4", ".webm", ".html", ".htm",
        ]
        .iter()
        .any(|extension| value.ends_with(extension))
}

fn links(text: &str) -> Vec<(Range<usize>, &str)> {
    let mut result: Vec<_> = MARKDOWN
        .captures_iter(text)
        .filter_map(|captures| {
            let source = (1..=4).find_map(|index| captures.get(index))?;
            let target = source.as_str().trim().trim_matches(['<', '>']);
            candidate(target).then(|| (captures.get(0).unwrap().range(), target))
        })
        .collect();
    let plain_start = result.len();
    if text.contains("://") {
        let mut offset = 0;
        while let Some(scheme) = WEB_SCHEME.find_at(text, offset) {
            let token = web_token(&text[scheme.start()..]);
            offset = scheme.start() + token.len();
            let target = token.trim_end_matches(['.', ',', ':', ';', '!', '?']);
            if is_web_url(target)
                && !result
                    .iter()
                    .any(|(range, _)| range.contains(&scheme.start()))
            {
                result.push((scheme.start()..scheme.start() + target.len(), target));
            }
        }
    }
    for token in PATH.find_iter(text) {
        if result
            .iter()
            .any(|(range, _)| range.start < token.end() && token.start() < range.end)
        {
            continue;
        }
        let target = token.as_str().trim_end_matches(['.', ':']);
        if !has_web_scheme(target) && preview_candidate(target) {
            result.push((token.start()..token.start() + target.len(), target));
        }
    }
    result[plain_start..].sort_unstable_by_key(|(range, _)| range.start);
    result
}

/// Stop before an enclosing delimiter while keeping balanced brackets in the URL itself.
fn web_token(text: &str) -> &str {
    let mut brackets = Vec::new();
    let end = text
        .char_indices()
        .find_map(|(index, c)| {
            if c.is_whitespace() || c.is_control() || r#"<>"'`\"#.contains(c) {
                return Some(index);
            }
            match c {
                '(' => brackets.push(')'),
                '[' => brackets.push(']'),
                '{' => brackets.push('}'),
                ')' | ']' | '}' if brackets.pop() != Some(c) => return Some(index),
                _ => {}
            }
            None
        })
        .unwrap_or(text.len());
    &text[..end]
}

/// Detect text links along one logical line and apply them only to its visible rows.
pub(super) fn mark_links(rows: &mut [Row], prefix: &str, suffix: &str) {
    let mut text = String::from(prefix);
    let mut spots = Vec::new();
    for (row, line) in rows.iter().enumerate() {
        for (column, cell) in line.cells.iter().enumerate() {
            let width = match cell.wide {
                CellWide::SpacerHead | CellWide::SpacerTail => continue,
                CellWide::Wide => 2,
                _ => 1,
            };
            spots.push((text.len(), row, column, width));
            text.push_str(&cell.text);
        }
    }
    text.push_str(suffix);
    for (range, target) in links(&text) {
        let start = spots.partition_point(|(offset, ..)| *offset < range.start);
        let end = spots.partition_point(|(offset, ..)| *offset < range.end);
        let link: Arc<str> = Arc::from(target);
        for &(_, row, column, width) in &spots[start..end] {
            let cells = &mut rows[row].cells;
            let end = (column + width).min(cells.len());
            for cell in &mut cells[column..end] {
                if cell.link.is_none() {
                    cell.link = Some(link.clone());
                    cell.text_link = true;
                }
            }
        }
    }
}

/// The first cell of each distinct link, scanning from the bottom row up, at most nine.
fn hint_positions(frame: &Frame) -> Vec<(CellPosition, Arc<str>)> {
    let mut hints: Vec<(CellPosition, Arc<str>)> = Vec::new();
    for (row, line) in frame.rows.iter().enumerate().rev() {
        for span in &line.links {
            if let Some(link) = &line.cells[span.start].link
                && !hints.iter().any(|(_, seen)| seen == link)
            {
                let column = span.start;
                let position = frame
                    .link_span(CellPosition { row, column })
                    .map_or(CellPosition { row, column }, |(start, _)| start);
                hints.push((position, link.clone()));
                if hints.len() == 9 {
                    return hints;
                }
            }
        }
    }
    hints
}

#[cfg(test)]
mod tests {
    use super::{candidate, hint_positions, links};
    use crate::terminal::frame::Frame;
    use ghostty_vt::{Terminal, render::RenderState, terminal::Options};

    #[test]
    fn detects_agent_paths_and_artifact_tokens() {
        let text = "Video [preview](./out/my clip.mp4) pwr://artifact/a power://artifact/b https://localhost:4000/app";
        let detected: Vec<_> = links(text).into_iter().map(|(_, value)| value).collect();
        assert_eq!(
            detected,
            [
                "./out/my clip.mp4",
                "pwr://artifact/a",
                "power://artifact/b",
                "https://localhost:4000/app"
            ]
        );
        assert_eq!(links("Saved /tmp/my clip.mp4.")[0].1, "/tmp/my clip.mp4");
    }

    #[test]
    fn detects_plain_and_markdown_dag_links() {
        let text = "See pwr://dag/run-123. [Open graph](pwr://dag/Run-456)";
        let detected: Vec<_> = links(text).into_iter().map(|(_, value)| value).collect();
        assert_eq!(detected, ["pwr://dag/Run-456", "pwr://dag/run-123"]);
        assert!(!candidate("pwr://dag/"));
        assert!(!candidate("pwr://dag/another/file"));
        assert!(!candidate("pwr://dag/report.html"));
    }

    #[test]
    fn detects_http_urls_with_ipv6_unicode_and_balanced_punctuation() {
        let text = "(https://en.example.org/wiki/A_(topic)). HTTP://[2001:db8::1]:8080/q?a=b,c#id; https://例.example/道.";
        let found = links(text);
        let expected = [
            "https://en.example.org/wiki/A_(topic)",
            "HTTP://[2001:db8::1]:8080/q?a=b,c#id",
            "https://例.example/道",
        ];
        for ((range, uri), expected) in found.iter().zip(expected) {
            assert_eq!(*uri, expected);
            assert_eq!(&text[range.clone()], expected);
        }
        assert_eq!(found.len(), expected.len());
        assert!(!candidate("https://[not-an-ip]"));
        assert!(!candidate("https://[not-an-ip]/report.html"));
        assert!(!candidate("https://example.org/one two"));
        assert!(!candidate("javascript:alert(1)"));
        assert_eq!(
            links("[Read this](https://example.org/A_(topic))")[0].1,
            "https://example.org/A_(topic)"
        );
    }

    #[test]
    fn scans_adjacent_parenthesized_web_links() {
        let text = "(https://one.example/a),(https://two.example/b)";
        let detected: Vec<_> = links(text).into_iter().map(|(_, target)| target).collect();
        assert_eq!(detected, ["https://one.example/a", "https://two.example/b"]);
    }

    #[test]
    fn hints_number_the_newest_links_first_without_repeats() {
        let mut terminal = Terminal::new(Options::default()).unwrap();
        terminal.vt_write(
            b"old pwr://artifact/a\r\nnew pwr://artifact/b and pwr://artifact/c\r\nagain pwr://artifact/a\r\n",
        );
        let frame = Frame::default()
            .update(&mut RenderState::new().unwrap(), &terminal)
            .unwrap()
            .unwrap();
        let hints: Vec<_> = hint_positions(&frame)
            .into_iter()
            .map(|(position, link)| (position.row, link.to_string()))
            .collect();
        assert_eq!(
            hints,
            [
                (2, "pwr://artifact/a".to_owned()),
                (1, "pwr://artifact/b".to_owned()),
                (1, "pwr://artifact/c".to_owned()),
            ]
        );
    }

    #[test]
    fn both_plain_and_osc_dag_links_are_clickable_and_numbered() {
        let mut terminal = Terminal::new(Options::default()).unwrap();
        terminal
            .vt_write(b"\x1b]8;;pwr://dag/osc-1\x1b\\Open graph\x1b]8;;\x1b\\ pwr://dag/plain-2.");
        let frame = Frame::default()
            .update(&mut RenderState::new().unwrap(), &terminal)
            .unwrap()
            .unwrap();
        let cells = &frame.rows[0].cells;
        assert_eq!(cells[0].link.as_deref(), Some("pwr://dag/osc-1"));
        assert_eq!(
            cells["Open graph ".len()].link.as_deref(),
            Some("pwr://dag/plain-2")
        );
        let hints: Vec<_> = hint_positions(&frame)
            .into_iter()
            .map(|(_, link)| link)
            .collect();
        assert_eq!(
            hints.iter().map(AsRef::as_ref).collect::<Vec<&str>>(),
            ["pwr://dag/osc-1", "pwr://dag/plain-2"]
        );
    }

    #[test]
    fn wrapped_hint_is_numbered_once_at_the_start_of_its_visible_span() {
        let mut terminal = Terminal::new(Options {
            cols: 16,
            rows: 5,
            ..Options::default()
        })
        .unwrap();
        let uri = "https://example.com/long/path";
        terminal.vt_write(format!("pwr://dag/old\r\nSee {uri}").as_bytes());
        let frame = Frame::default()
            .update(&mut RenderState::new().unwrap(), &terminal)
            .unwrap()
            .unwrap();
        let hints: Vec<_> = hint_positions(&frame)
            .into_iter()
            .map(|(pos, link)| (pos.row, pos.column, link.to_string()))
            .collect();
        assert_eq!(
            hints,
            [(1, 4, uri.to_owned()), (0, 0, "pwr://dag/old".to_owned())]
        );
    }
}
