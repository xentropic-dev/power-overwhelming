//! A terminal row's drawing as runs: backgrounds merged into spans of one color, and text
//! shaped once per run of cells that look the same, each glyph then placed on its own cell.
//!
//! Adapted from herdr-app `src/terminal.rs` (`RowOp`, `row_ops`, `ligature_features` and
//! `needs_ligatures`), licensed under the Apache License, Version 2.0. herdr moves glyphs onto
//! their cells with a patched GPUI (`ShapedLine::with_cell_positions`); this port reads the
//! public `LineLayout` from stock GPUI and keeps each glyph's offset from its cell, to paint
//! with `paint_glyph` and `paint_emoji`.

use super::frame::Cell;
use ghostty_vt::{
    screen::CellWide,
    style::{Style, StyleColor},
};
use gpui::{
    Font, FontFeatures, FontId, GlyphId, Hsla, LineLayout, Pixels, Point, TextRun, Window, point,
};
use std::sync::{Arc, LazyLock};

/// One piece of a row's drawing, in cells from the row's start.
#[derive(Debug, PartialEq)]
pub(super) enum RowOp {
    Background {
        from: usize,
        to: usize,
        color: Hsla,
    },
    /// A run of cells shaped together, starting at cell `column`.
    Text {
        column: usize,
        color: Hsla,
        glyphs: Vec<PlacedGlyph>,
    },
}

/// A shaped glyph, placed from its run's first cell: its own cell's offset plus where the
/// shaper put it inside that cell (combining marks keep their offset from their base).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct PlacedGlyph {
    pub(super) font: FontId,
    pub(super) id: GlyphId,
    pub(super) font_size: Pixels,
    /// From the run's first cell to the glyph's origin on the baseline.
    pub(super) offset: Point<Pixels>,
    pub(super) emoji: bool,
}

impl RowOp {
    /// Paints a text run's glyphs from `origin`, its first cell's top-left corner moved by
    /// `glyph_x` across and the baseline down.
    pub(super) fn paint(&self, origin: Point<Pixels>, window: &mut Window) {
        let Self::Text { color, glyphs, .. } = self else {
            return;
        };
        for glyph in glyphs {
            let at = origin + glyph.offset;
            let _ = if glyph.emoji {
                window.paint_emoji(at, glyph.font, glyph.id, glyph.font_size)
            } else {
                window.paint_glyph(at, glyph.font, glyph.id, glyph.font_size, *color)
            };
        }
    }
}

/// A cell as the row's drawing sees it: its text color after links and selection.
pub(super) struct RunCell<'a> {
    pub(super) cell: &'a Cell,
    pub(super) fg: Hsla,
}

impl RunCell<'_> {
    fn blank(&self) -> bool {
        matches!(self.cell.wide, CellWide::SpacerTail | CellWide::SpacerHead)
            || self.cell.text.trim().is_empty()
    }

    /// Whether `other` can join a run that this cell starts.
    fn joins(&self, other: &Self, ligatures: bool) -> bool {
        other.cell.wide == CellWide::Narrow
            && other.fg == self.fg
            && look(other.cell.style) == look(self.cell.style)
            && (!ligatures || other.cell.bg == self.cell.bg)
    }
}

/// A style without its colors, which cells carry resolved.
fn look(style: Style) -> Style {
    Style {
        fg_color: StyleColor::None,
        bg_color: StyleColor::None,
        ..style
    }
}

/// What a row's text shapes with.
pub(super) struct Shaping<'a> {
    pub(super) font: &'a Font,
    pub(super) font_size: Pixels,
    pub(super) cell_width: Pixels,
    pub(super) ligatures: bool,
    /// The cursor's column on this row. With ligatures on it is shaped on its own, so a plain
    /// glyph under the cursor never covers half of a ligature.
    pub(super) cursor: Option<usize>,
}

/// A row's drawing: backgrounds merged into spans of one color, then text in runs of one
/// style, broken at the cursor, at wide cells (each on its own) and at their spacers.
/// `shape` lays out one run's text, as `WindowTextSystem::layout_line` does.
pub(super) fn row_ops(
    row: &[RunCell],
    shaping: &Shaping,
    mut shape: impl FnMut(&str, Pixels, &[TextRun]) -> Arc<LineLayout>,
) -> Vec<RowOp> {
    let cursor = shaping.cursor.filter(|_| shaping.ligatures);
    let mut ops = Vec::new();
    let mut x = 0;
    while x < row.len() {
        let Some(color) = row[x].cell.bg else {
            x += 1;
            continue;
        };
        let from = x;
        while x < row.len() && row[x].cell.bg == Some(color) {
            x += 1;
        }
        ops.push(RowOp::Background { from, to: x, color });
    }
    let mut x = 0;
    while x < row.len() {
        let start = x;
        let first = &row[start];
        x += 1;
        if first.blank() {
            continue;
        }
        // The byte offset of each cell's text in the run, by cell.
        let mut offsets = vec![0];
        let mut text = first.cell.text.clone();
        let mut end = x;
        if first.cell.wide == CellWide::Narrow && cursor != Some(start) {
            while x < row.len() && cursor != Some(x) && first.joins(&row[x], shaping.ligatures) {
                if row[x].blank() {
                    // Spaces join only when text follows them in the run.
                    x += 1;
                    continue;
                }
                for cell in &row[end..=x] {
                    offsets.push(text.len());
                    text.push_str(&cell.cell.text);
                }
                x += 1;
                end = x;
            }
            x = end;
        }
        let mut font = shaping.font.clone();
        font.features = ligature_features(shaping.ligatures && needs_ligatures(&text, &font));
        let run = TextRun {
            len: text.len(),
            font,
            color: first.fg,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let layout = shape(&text, shaping.font_size, &[run]);
        ops.push(RowOp::Text {
            column: start,
            color: first.fg,
            glyphs: place_glyphs(&layout, &offsets, shaping.cell_width),
        });
    }
    ops
}

/// Moves each glyph cluster onto its cell. `offsets` holds the byte offset where each cell's
/// text starts; a glyph belongs to the last cell starting at or before its index. Glyphs after
/// the first of a cell keep their shaped distance from it.
pub(super) fn place_glyphs(
    layout: &LineLayout,
    offsets: &[usize],
    cell_width: Pixels,
) -> Vec<PlacedGlyph> {
    let mut placed = Vec::new();
    let mut cluster: Option<(usize, Pixels)> = None;
    for run in &layout.runs {
        for glyph in &run.glyphs {
            let cell = offsets
                .partition_point(|offset| *offset <= glyph.index)
                .saturating_sub(1);
            let shift = match cluster {
                Some((current, shift)) if current == cell => shift,
                _ => {
                    let shift = cell_width * cell as f32 - glyph.position.x;
                    cluster = Some((cell, shift));
                    shift
                }
            };
            placed.push(PlacedGlyph {
                font: run.font_id,
                id: glyph.id,
                font_size: layout.font_size,
                offset: point(glyph.position.x + shift, glyph.position.y),
                emoji: glyph.is_emoji,
            });
        }
    }
    placed
}

/// The terminal's font features: contextual alternates and standard ligatures on or off, and
/// discretionary ligatures always off, as Ghostty sets them.
pub(super) fn ligature_features(enabled: bool) -> FontFeatures {
    static FEATURES: LazyLock<[FontFeatures; 2]> = LazyLock::new(|| {
        [0, 1].map(|value| {
            FontFeatures(Arc::new(vec![
                ("calt".into(), value),
                ("liga".into(), value),
                ("dlig".into(), 0),
            ]))
        })
    });
    FEATURES[usize::from(enabled)].clone()
}

/// JetBrains Mono joins only punctuation, so ordinary ASCII output keeps the ligatures-off
/// font even when ligatures are on; any other font may join anything.
fn needs_ligatures(text: &str, font: &Font) -> bool {
    !font.family.contains("JetBrains")
        || !text.is_ascii()
        || text
            .as_bytes()
            .windows(2)
            .any(|pair| pair[0].is_ascii_punctuation() && pair[1].is_ascii_punctuation())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::frame::{Frame, frame};
    use ghostty_vt::{Terminal, render::RenderState, terminal::Options};
    use gpui::{ShapedGlyph, ShapedRun, font, px, rgb};

    fn screen(output: &[u8]) -> Frame {
        let mut terminal = Terminal::new(Options::default()).unwrap();
        terminal.vt_write(output);
        frame(&mut RenderState::new().unwrap(), &terminal, 0xdbe3ee).unwrap()
    }

    fn cells(frame: &Frame, row: usize) -> Vec<RunCell<'_>> {
        frame.rows[row]
            .cells
            .iter()
            .map(|cell| RunCell { cell, fg: cell.fg })
            .collect()
    }

    /// GPUI keeps `GlyphId`'s field private; only its text systems make one.
    fn glyph_id(id: u32) -> GlyphId {
        // SAFETY: `GlyphId` is a `#[repr(C)]` struct of one `u32`.
        unsafe { std::mem::transmute::<u32, GlyphId>(id) }
    }

    fn glyph(index: usize, x: f32) -> ShapedGlyph {
        ShapedGlyph {
            id: glyph_id(index as u32),
            position: point(px(x), px(0.)),
            index,
            is_emoji: false,
        }
    }

    /// A shaper with one 7 px glyph per character, as a stand-in for the text system.
    fn monospace(
        texts: &mut Vec<String>,
    ) -> impl FnMut(&str, Pixels, &[TextRun]) -> Arc<LineLayout> {
        move |text, font_size, _| {
            texts.push(text.to_owned());
            let glyphs = text
                .char_indices()
                .enumerate()
                .map(|(n, (index, _))| glyph(index, 7. * n as f32))
                .collect();
            Arc::new(LineLayout {
                font_size,
                len: text.len(),
                runs: vec![ShapedRun {
                    font_id: FontId(0),
                    glyphs,
                }],
                ..LineLayout::default()
            })
        }
    }

    fn shaping(font: &Font, ligatures: bool, cursor: Option<usize>) -> Shaping<'_> {
        Shaping {
            font,
            font_size: px(13.),
            cell_width: px(8.),
            ligatures,
            cursor,
        }
    }

    #[test]
    fn clusters_move_onto_their_cells_and_keep_their_marks() {
        // "x" with a combining acute (bytes 0..3), then "b": the mark keeps its 0.5 px offset.
        let layout = LineLayout {
            font_size: px(13.),
            runs: vec![ShapedRun {
                font_id: FontId(0),
                glyphs: vec![glyph(0, 0.), glyph(1, 0.5), glyph(3, 7.3)],
            }],
            ..LineLayout::default()
        };
        let placed = place_glyphs(&layout, &[0, 3], px(8.));
        let xs: Vec<_> = placed
            .iter()
            .map(|glyph| f32::from(glyph.offset.x))
            .collect();
        assert_eq!(xs, [0., 0.5, 8.]);
    }

    #[test]
    fn rows_shape_once_per_run_of_matching_cells() {
        let font = font("JetBrains Mono");
        let frame = screen(b"ab \x1b[1mcd\x1b[0m  \xe4\xb8\xadef\x1b[41mgh\x1b[0m");
        let mut texts = Vec::new();
        let ops = row_ops(
            &cells(&frame, 0),
            &shaping(&font, false, None),
            monospace(&mut texts),
        );
        // Spaces join a run only between its text; bold breaks it, and the wide character is
        // shaped alone, its spacer skipped. Backgrounds don't break runs without ligatures.
        assert_eq!(texts, ["ab", "cd", "中", "efgh"]);
        let columns: Vec<_> = ops
            .iter()
            .filter_map(|op| match op {
                RowOp::Text { column, .. } => Some(*column),
                RowOp::Background { .. } => None,
            })
            .collect();
        assert_eq!(columns, [0, 3, 7, 9]);
        let red = frame.rows[0].cells[11].bg.unwrap();
        assert_eq!(
            ops[0],
            RowOp::Background {
                from: 11,
                to: 13,
                color: red
            }
        );
        // Glyphs sit on their cells: 8 px apart, not the shaper's 7.
        let RowOp::Text { glyphs, .. } = &ops[4] else {
            panic!("expected text");
        };
        let xs: Vec<_> = glyphs
            .iter()
            .map(|glyph| f32::from(glyph.offset.x))
            .collect();
        assert_eq!(xs, [0., 8., 16., 24.]);
    }

    #[test]
    fn ligatures_break_runs_at_the_cursor_and_backgrounds() {
        let font = font("JetBrains Mono");
        let frame = screen(b"a=>b\x1b[41mc\x1b[0m");
        let mut texts = Vec::new();
        row_ops(
            &cells(&frame, 0),
            &shaping(&font, true, Some(1)),
            monospace(&mut texts),
        );
        assert_eq!(texts, ["a", "=", ">b", "c"]);
        // Without ligatures the cursor never splits text.
        let mut texts = Vec::new();
        row_ops(
            &cells(&frame, 0),
            &shaping(&font, false, Some(1)),
            monospace(&mut texts),
        );
        assert_eq!(texts, ["a=>bc"]);
    }

    #[test]
    fn link_and_selection_colors_break_runs() {
        let font = font("JetBrains Mono");
        let frame = screen(b"abcd");
        let mut row = cells(&frame, 0);
        row[2].fg = rgb(0xff0000).into();
        let mut texts = Vec::new();
        row_ops(&row, &shaping(&font, false, None), monospace(&mut texts));
        assert_eq!(texts, ["ab", "c", "d"]);
    }

    #[test]
    fn only_jetbrains_punctuation_needs_ligatures() {
        let jetbrains = font("JetBrains Mono");
        assert!(!needs_ligatures("cargo test", &jetbrains));
        assert!(needs_ligatures("a => b", &jetbrains));
        assert!(needs_ligatures("λx", &jetbrains));
        assert!(needs_ligatures("cargo test", &font("Fira Code")));
    }

    /// Port of herdr-app's `ligatures_change_glyphs_without_moving_terminal_cells`, shaped by
    /// cosmic-text as GPUI's Linux text system shapes (`CosmicTextSystemState::layout_line`),
    /// on the bundled JetBrains Mono. Cells are the rounded advance, so glyphs must move.
    #[cfg(target_os = "linux")]
    #[test]
    fn ligatures_change_glyphs_without_moving_terminal_cells() {
        use cosmic_text::{
            Attrs, AttrsList, Family, FeatureTag, FontFeatures as Features, FontSystem,
            ShapeBuffer, ShapeLine, Shaping as Shaper, Wrap, fontdb,
        };

        let mut db = fontdb::Database::new();
        db.load_font_data(include_bytes!("../../assets/fonts/JetBrainsMono-Variable.ttf").to_vec());
        let mut system = FontSystem::new_with_locale_and_db("en-US".into(), db);
        let mut scratch = ShapeBuffer::default();
        let mut shape = |text: &str, font_size: Pixels, runs: &[TextRun]| {
            let mut features = Features::new();
            for (tag, value) in runs[0].font.features.0.iter() {
                let tag: [u8; 4] = tag.as_bytes().try_into().unwrap();
                features.set(FeatureTag::new(&tag), *value);
            }
            let attrs = Attrs::new()
                .family(Family::Name("JetBrains Mono"))
                .font_features(features);
            let line = ShapeLine::new(
                &mut system,
                text,
                &AttrsList::new(&attrs),
                Shaper::Advanced,
                4,
            );
            let mut lines = Vec::new();
            line.layout_to_buffer(
                &mut scratch,
                font_size.into(),
                None,
                Wrap::None,
                None,
                &mut lines,
                None,
            );
            let glyphs = lines[0]
                .glyphs
                .iter()
                .map(|glyph| ShapedGlyph {
                    id: glyph_id(u32::from(glyph.glyph_id)),
                    position: point(px(glyph.x), px(glyph.y)),
                    index: glyph.start,
                    is_emoji: false,
                })
                .collect();
            Arc::new(LineLayout {
                font_size,
                len: text.len(),
                runs: vec![ShapedRun {
                    font_id: FontId(0),
                    glyphs,
                }],
                ..LineLayout::default()
            })
        };
        let mut font = font("JetBrains Mono");
        font.features = ligature_features(false);
        for font_size in [10., 14., 24., 36.] {
            // JetBrains Mono's advance is 0.6 em; the grid rounds it.
            let cell_width = px((font_size * 0.6_f32).round());
            // herdr also checks `!=`, `->` and `|>`, which cosmic-text 0.14's rustybuzz, as
            // GPUI 0.2.2 shapes on Linux, leaves unjoined.
            for text in ["a=>b", "a===b", "a<=b", "a>=b", "a<-b", "a::b", "a==b"] {
                let frame = screen(text.as_bytes());
                let row = cells(&frame, 0);
                let mut variants = Vec::new();
                for ligatures in [false, true] {
                    let shaping = Shaping {
                        font: &font,
                        font_size: px(font_size),
                        cell_width,
                        ligatures,
                        cursor: None,
                    };
                    let ops = row_ops(&row, &shaping, &mut shape);
                    let [
                        RowOp::Text {
                            column: 0, glyphs, ..
                        },
                    ] = ops.as_slice()
                    else {
                        panic!("{text}: expected one run, got {ops:?}");
                    };
                    assert_eq!(glyphs.len(), text.len(), "{text}: one glyph per cell");
                    for (cell, glyph) in glyphs.iter().enumerate() {
                        let drift = f32::from(glyph.offset.x - cell_width * cell as f32);
                        assert!(
                            drift.abs() < 0.1,
                            "{text} at {font_size} px: cell {cell} drifts"
                        );
                    }
                    variants.push(glyphs.iter().map(|glyph| glyph.id).collect::<Vec<_>>());
                }
                assert_ne!(
                    variants[0], variants[1],
                    "{text}: ligatures must change the glyphs"
                );
            }
        }
    }
}
