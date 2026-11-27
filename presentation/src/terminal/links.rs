use super::{
    TerminalView,
    frame::{CellPosition, Frame},
};
use domain::{
    artifact::artifact_id,
    link::{DAG_URI_PREFIX, dag_run_id},
};
use gpui::{Pixels, Point};
use regex::Regex;
use std::sync::{Arc, LazyLock};

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

    pub(super) fn link_at_point(&self, point: Point<Pixels>) -> Option<&Arc<str>> {
        if !self.grid.bounds.contains(&point) {
            return None;
        }
        self.link_at(self.at(point))
    }

    pub(super) fn hovered_span(&self) -> Option<(CellPosition, CellPosition)> {
        let point = self.pointer.filter(|p| self.grid.bounds.contains(p))?;
        self.frame.link_span(self.at(point))
    }

    pub(super) fn set_pointer(&mut self, position: Option<Point<Pixels>>) -> bool {
        let previous = self.hovered_span();
        self.pointer = position;
        previous != self.hovered_span()
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

/// Extent of a hovered or pressed link. OSC 8 spans on either side of a soft
/// wrap can belong to rows captured in different frames.
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
        while start.column == 0 && self.rows[start.row].continues_osc && start.row > 0 {
            let prior = &self.rows[start.row - 1];
            if !prior.ends_in_osc || prior.cells.last()?.link.as_deref() != Some(uri) {
                break;
            }
            start = CellPosition {
                row: start.row - 1,
                column: prior.links.last()?.start,
            };
        }
        while end.column + 1 == self.rows[end.row].cells.len() && self.rows[end.row].ends_in_osc {
            let Some(next) = self.rows.get(end.row + 1) else {
                break;
            };
            if !next.continues_osc || next.cells.first()?.link.as_deref() != Some(uri) {
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

static MARKDOWN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\[[^\]]+\]\(([^)]+)\)|"([^"]+)"|'([^']+)'|`([^`]+)`"#).unwrap());
static PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r#"(?i)(?:/|\.{1,2}/)[^<>\[\]()"'`\r\n]*?\.(?:png|jpe?g|webp|mp4|webm|html?)\b|[^\s<>\[\]()"'`,;]+"#,
).unwrap()
});

pub(super) fn candidate(value: &str) -> bool {
    if value.starts_with(DAG_URI_PREFIX) {
        return dag_run_id(value).is_some();
    }
    let value = value.to_ascii_lowercase();
    artifact_id(&value).is_some()
        || value.starts_with("file://")
        || value.starts_with("http://localhost")
        || value.starts_with("https://localhost")
        || value.starts_with("http://127.")
        || value.starts_with("https://127.")
        || value.starts_with("http://[::1]")
        || value.starts_with("https://[::1]")
        || [
            ".png", ".jpg", ".jpeg", ".webp", ".mp4", ".webm", ".html", ".htm",
        ]
        .iter()
        .any(|extension| value.ends_with(extension))
}

pub(super) fn links(text: &str) -> Vec<(std::ops::Range<usize>, String)> {
    let mut result: Vec<_> = MARKDOWN
        .captures_iter(text)
        .filter_map(|captures| {
            let source = (1..=4).find_map(|index| captures.get(index))?;
            let target = source.as_str().trim().trim_matches(['<', '>']);
            candidate(target).then(|| (captures.get(0).unwrap().range(), target.to_owned()))
        })
        .collect();
    for token in PATH.find_iter(text) {
        if result
            .iter()
            .any(|(range, _)| range.contains(&token.start()))
        {
            continue;
        }
        let target = token.as_str().trim_end_matches(['.', ':']);
        if candidate(target) {
            result.push((
                token.start()..token.start() + target.len(),
                target.to_owned(),
            ));
        }
    }
    result
}

/// The first cell of each distinct link, scanning from the bottom row up, at most nine.
fn hint_positions(frame: &Frame) -> Vec<(CellPosition, Arc<str>)> {
    let mut hints: Vec<(CellPosition, Arc<str>)> = Vec::new();
    for (row, line) in frame.rows.iter().enumerate().rev() {
        for span in &line.links {
            if let Some(link) = &line.cells[span.start].link
                && !hints.iter().any(|(_, seen)| seen == link)
            {
                hints.push((
                    CellPosition {
                        row,
                        column: span.start,
                    },
                    link.clone(),
                ));
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
}
