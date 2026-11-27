//! Input methods (Pinyin, Japanese, Korean, dead keys, AltGr compose) type into the terminal
//! here. What they are composing shows underlined at the cursor; the finished text goes to the
//! program as it is. Keys the terminal encodes itself never reach this handler, because
//! `key_down` stops their propagation. While Power reads the keyboard (link hints, a pending
//! prefix), input-method text is dropped, as those keys are; see `typing::InputHold`.
//!
//! Adapted from herdr-app `src/terminal.rs` (`impl EntityInputHandler for TerminalView` and the
//! marked-text painting in `paint_grid`; herdr-app 3e6d564), licensed under Apache-2.0.

use super::{CellMetrics, TerminalView, frame::CellPosition};
use gpui::*;
use std::ops::Range;
use unicode_width::UnicodeWidthStr;

impl TerminalView {
    fn marked_len(&self) -> usize {
        self.typing
            .marked()
            .map_or(0, |marked| marked.encode_utf16().count())
    }
}

/// The text before a UTF-16 offset into `text`, clamped to its length.
fn utf16_prefix(text: &str, offset: usize) -> &str {
    let mut units = 0;
    for (index, ch) in text.char_indices() {
        if units >= offset {
            return &text[..index];
        }
        units += ch.len_utf16();
    }
    text
}

impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let units: Vec<u16> = self.typing.marked().unwrap_or("").encode_utf16().collect();
        let range = range.start.min(units.len())..range.end.min(units.len());
        *adjusted = Some(range.clone());
        Some(String::from_utf16_lossy(&units[range]))
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let len = self.marked_len();
        Some(UTF16Selection {
            range: len..len,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.typing.marked().map(|_| 0..self.marked_len())
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if self.typing.unmark() {
            cx.notify();
        }
    }

    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.typing.commit(text) {
            Some(bytes) => self.input(bytes, cx),
            None => cx.notify(),
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.typing.mark(text);
        cx.notify();
    }

    /// Where the candidate window goes: the cursor cell, moved along the composition.
    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let cursor = self.frame.ime_cursor?;
        let metrics = self.grid.metrics;
        let before = utf16_prefix(self.typing.marked().unwrap_or(""), range.start);
        let origin = metrics.cell_origin(self.grid.bounds.origin, cursor.row, cursor.column)
            + point(metrics.size.width * before.width() as f32, px(0.));
        Some(Bounds::new(origin, metrics.size))
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

/// How the composition is drawn.
pub(super) struct Preedit {
    pub text: String,
    pub at: CellPosition,
    pub font: Font,
    pub font_size: Pixels,
    pub foreground: Hsla,
    pub background: Hsla,
}

impl Preedit {
    /// Paints the composition at its cell on the terminal's background, so cells under it
    /// don't show through, and underlines it.
    pub fn paint(
        self,
        grid: Point<Pixels>,
        metrics: CellMetrics,
        window: &mut Window,
        cx: &mut App,
    ) {
        let run = TextRun {
            len: self.text.len(),
            font: self.font,
            color: self.foreground,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line = window
            .text_system()
            .shape_line(self.text.into(), self.font_size, &[run], None);
        let origin = metrics.cell_origin(grid, self.at.row, self.at.column);
        let width = line.width.max(metrics.size.width);
        window.paint_quad(fill(
            Bounds::new(origin, size(width, metrics.size.height)),
            self.background,
        ));
        if let Err(error) = line.paint(origin, metrics.size.height, window, cx) {
            eprintln!("Could not paint the input method's text. {error:#}");
        }
        let underline = Bounds::new(
            origin + point(px(0.), metrics.underline),
            size(width, metrics.underline_thickness.max(px(1.))),
        );
        window.paint_quad(fill(underline, self.foreground));
    }
}

#[cfg(test)]
mod tests {
    use super::utf16_prefix;

    #[test]
    fn utf16_offsets_find_their_prefix() {
        assert_eq!(utf16_prefix("にほん", 0), "");
        assert_eq!(utf16_prefix("にほん", 2), "にほ");
        assert_eq!(utf16_prefix("a😀b", 1), "a");
        assert_eq!(utf16_prefix("a😀b", 3), "a😀");
        assert_eq!(utf16_prefix("ab", 9), "ab");
    }
}
