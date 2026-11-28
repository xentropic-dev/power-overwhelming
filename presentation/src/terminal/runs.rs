//! A terminal row's drawing as runs: backgrounds merged into spans of one color, and text
//! shaped once per run of cells that look the same, each glyph then placed on its own cell.
//!
//! Adapted from herdr-app `src/terminal.rs` (`RowOp`, `row_ops`, `ligature_features` and
//! `needs_ligatures`), licensed under the Apache License, Version 2.0. herdr moves glyphs onto
//! their cells with a patched GPUI (`ShapedLine::with_cell_positions`); this port reads the
//! public `LineLayout` from stock GPUI and keeps each glyph's offset from its cell, to paint
//! with `paint_glyph` and `paint_emoji`. Icons from the Private Use Areas that the terminal
//! font lacks are fitted from the bundled Symbols Nerd Font by their outlines (herdr
//! `icon_glyph`, `fallback_icon`), and other fallback glyphs wider than a cell are shrunk to it
//! (`fallback_size`).

use super::{frame::Cell, metrics::CellMetrics, sprites::sprite_char};
use ghostty_vt::{
    screen::CellWide,
    style::{Style, StyleColor, Underline},
};
use gpui::{
    Bounds, Font, FontFeatures, FontId, FontStyle, FontWeight, GlyphId, Hsla, LineLayout, Pixels,
    Point, Size, TextRun, UnderlineStyle, Window, fill, point, px, size,
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
    /// A Nerd Font icon the terminal font lacks, drawn from the bundled symbols font.
    Icon {
        column: usize,
        color: Hsla,
        glyph: PlacedGlyph,
    },
    /// A font-independent shape in one cell.
    Sprite {
        column: usize,
        character: char,
        color: Hsla,
    },
    /// A run of cells shaped together, starting at cell `column`.
    Text {
        column: usize,
        /// How many cells the run covers.
        cells: usize,
        /// Whether the run is a single wide cell.
        wide: bool,
        color: Hsla,
        /// For faint text, the background that color emoji fade into.
        fade: Option<Hsla>,
        glyphs: Vec<PlacedGlyph>,
    },
    /// Underline and strikethrough over cells `from..to` that share both.
    Decoration {
        from: usize,
        to: usize,
        underline: Option<(Underline, Hsla)>,
        strike: Option<Hsla>,
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
    /// Paints a text run's glyphs from `text_origin`, its first cell's top-left corner moved
    /// by `glyph_x` across and the baseline down, or an icon from `cell_origin`, its cell's
    /// top-left corner.
    pub(super) fn paint(
        &self,
        cell_origin: Point<Pixels>,
        text_origin: Point<Pixels>,
        window: &mut Window,
    ) {
        match self {
            Self::Background { .. } | Self::Decoration { .. } | Self::Sprite { .. } => {}
            Self::Icon { color, glyph, .. } => glyph.paint(cell_origin, *color, window),
            Self::Text { color, glyphs, .. } => {
                for glyph in glyphs {
                    glyph.paint(text_origin, *color, window);
                }
            }
        }
    }
}

impl PlacedGlyph {
    fn paint(&self, origin: Point<Pixels>, color: Hsla, window: &mut Window) {
        let at = origin + self.offset;
        let _ = if self.emoji {
            window.paint_emoji(at, self.font, self.id, self.font_size)
        } else {
            window.paint_glyph(at, self.font, self.id, self.font_size, color)
        };
    }
}

/// How much of a faint color emoji shows through, as faint text's color does.
pub(super) const FAINT_OPACITY: f32 = 0.6;

impl RowOp {
    /// The cells of a faint run's color emoji, as `(offset from the run's first cell, count)`,
    /// with the background to fade them into.
    pub(super) fn faded_emoji(&self, cell_width: Pixels) -> Vec<(usize, usize, Hsla)> {
        let Self::Text {
            wide,
            fade: Some(fade),
            glyphs,
            ..
        } = self
        else {
            return Vec::new();
        };
        let mut spans: Vec<(usize, usize, Hsla)> = Vec::new();
        for glyph in glyphs.iter().filter(|glyph| glyph.emoji) {
            // A wide cell is a run of its own, so its emoji covers both columns. In a run of
            // narrow cells, the emoji covers only its own cell.
            let (column, count) = if *wide {
                (0, 2)
            } else {
                let column = f32::from(glyph.offset.x) / f32::from(cell_width) + 0.01;
                (column.floor().max(0.) as usize, 1)
            };
            if !spans.iter().any(|(c, ..)| *c == column) {
                spans.push((column, count, *fade));
            }
        }
        spans
    }

    /// Dims a faint run's color emoji by painting the background back over them. `origin` is
    /// the run's first cell's top-left corner.
    pub(super) fn paint_fade(
        &self,
        origin: Point<Pixels>,
        cell: Size<Pixels>,
        window: &mut Window,
    ) {
        for (column, count, fade) in self.faded_emoji(cell.width) {
            let at = origin + point(cell.width * column as f32, px(0.));
            let veil = fade.opacity(1. - FAINT_OPACITY);
            window.paint_quad(fill(
                Bounds::new(at, size(cell.width * count as f32, cell.height)),
                veil,
            ));
        }
    }

    /// Paints a decoration's underline and strikethrough from `origin`, its first cell's
    /// top-left corner. Underline styles follow Ghostty's sprite drawing.
    pub(super) fn paint_decoration(
        &self,
        origin: Point<Pixels>,
        metrics: &CellMetrics,
        window: &mut Window,
    ) {
        let Self::Decoration {
            from,
            to,
            underline,
            strike,
        } = self
        else {
            return;
        };
        let (cell, scale) = (metrics.size, metrics.scale);
        let width = cell.width * (to - from) as f32;
        let line = |offset: Pixels, thickness: Pixels| {
            Bounds::new(origin + point(px(0.), offset), size(width, thickness))
        };
        let (top, thick) = (metrics.underline, metrics.underline_thickness);
        match *underline {
            None => {}
            Some((Underline::Single, color)) => {
                window.paint_quad(fill(line(top, thick), color));
            }
            Some((Underline::Double, color)) => {
                // A line above and a line below where the single one is.
                let top = top.min(cell.height - thick * 2.);
                window.paint_quad(fill(line(top - thick, thick), color));
                window.paint_quad(fill(line(top + thick, thick), color));
            }
            Some((Underline::Curly, color)) => {
                // GPUI's wave is three thicknesses tall, around its middle.
                let offset = (top - thick).min(cell.height - thick * 3.);
                let style = UnderlineStyle {
                    thickness: thick,
                    color: Some(color),
                    wavy: true,
                };
                window.paint_underline(origin + point(px(0.), offset), width, &style);
            }
            Some((Underline::Dotted, color)) => {
                // Dots wider than the line and about a dot apart, at least one a cell.
                let (cell_px, radius) = (
                    f32::from(cell.width) * scale,
                    std::f32::consts::FRAC_1_SQRT_2 * f32::from(thick) * scale,
                );
                let count = (cell_px / (4. * radius))
                    .ceil()
                    .min((cell_px / (3. * radius)).floor())
                    .min((cell_px / (2. * radius + 1.)).floor())
                    .max(1.);
                let (step, dot) = (cell.width / count, px(2. * radius / scale));
                let offset = (top + (thick - dot) / 2.).min(cell.height - dot);
                for i in 0..(to - from) * count as usize {
                    let at = origin + point(step * (i as f32 + 0.5) - dot / 2., offset);
                    window.paint_quad(
                        fill(Bounds::new(at, size(dot, dot)), color).corner_radii(dot / 2.),
                    );
                }
            }
            Some((Underline::Dashed, color)) => {
                // In every cell a dash, a gap, and the start of the next cell's dash.
                let cell_px = f32::from(cell.width) * scale;
                let dash = (cell_px / 3.).floor() + 1.;
                for column in 0..to - from {
                    let x = cell.width * column as f32;
                    for (start, len) in [
                        (0., dash.min(cell_px)),
                        (2. * dash, (cell_px - 2. * dash).max(0.)),
                    ] {
                        let at = origin + point(x + px(start / scale), top);
                        window
                            .paint_quad(fill(Bounds::new(at, size(px(len / scale), thick)), color));
                    }
                }
            }
            // Underline is non-exhaustive; draw an unknown style as a single line.
            Some((_, color)) => window.paint_quad(fill(line(top, thick), color)),
        }
        if let Some(color) = *strike {
            window.paint_quad(fill(line(metrics.strike, metrics.strike_thickness), color));
        }
    }
}

/// A cell as the row's drawing sees it: its text color after links and selection.
pub(super) struct RunCell<'a> {
    pub(super) cell: &'a Cell,
    pub(super) fg: Hsla,
    /// For faint text, the background behind the cell. Color emoji can't take a text color, so
    /// they are dimmed by laying this over them.
    pub(super) fade: Option<Hsla>,
}

/// The Kitty graphics Unicode placeholder character.
const KITTY_PLACEHOLDER: char = '\u{10EEEE}';

impl RunCell<'_> {
    fn blank(&self) -> bool {
        matches!(self.cell.wide, CellWide::SpacerTail | CellWide::SpacerHead)
            || self.cell.style.invisible
            || self.cell.text.trim().is_empty()
            || self.placeholder()
    }

    /// A Kitty Unicode placeholder stands in for an image cell; the image draws there.
    fn placeholder(&self) -> bool {
        self.cell.text.starts_with(KITTY_PLACEHOLDER)
    }

    /// Whether `other` can join a run that this cell starts.
    fn joins(&self, other: &Self, ligatures: bool) -> bool {
        other.cell.wide == CellWide::Narrow
            && sprite_char(&other.cell.text).is_none()
            && other.placeholder() == self.placeholder()
            && other.fg == self.fg
            && other.fade == self.fade
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
    pub(super) cell_height: Pixels,
    /// The bundled symbols font, which draws icons the terminal font lacks.
    pub(super) symbols: FontId,
    /// Whether the terminal font has a glyph for a character, as opposed to a fallback face.
    pub(super) has_glyph: &'a dyn Fn(char) -> bool,
    pub(super) ligatures: bool,
    /// The weight of bold text; regular text keeps `font`'s.
    pub(super) bold_weight: FontWeight,
    /// The cursor's column on this row. With ligatures on it is shaped on its own, so a plain
    /// glyph under the cursor never covers half of a ligature.
    pub(super) cursor: Option<usize>,
}

/// A row's drawing: backgrounds merged into spans of one color, then text in runs of one
/// style, broken at the cursor, at wide cells and their spacers, and at sprites.
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
    let fits = fallback_fits(row, shaping, &mut shape);
    let mut x = 0;
    while x < row.len() {
        let start = x;
        let first = &row[start];
        x += 1;
        if first.blank() {
            continue;
        }
        if let Some(character) = sprite_char(&first.cell.text) {
            ops.push(RowOp::Sprite {
                column: start,
                character,
                color: first.fg,
            });
            continue;
        }
        if let Fit::Icon(icon) = fits[start] {
            // Past the last column there is no room: the icon would be clipped.
            let room = row.get(x).is_some_and(RunCell::blank);
            ops.push(RowOp::Icon {
                column: start,
                color: first.fg,
                glyph: icon.place(shaping, room),
            });
            continue;
        }
        let fitted = match fits[start] {
            Fit::Shrunk(size) => Some(size),
            _ => None,
        };
        let mut end = x;
        if first.cell.wide == CellWide::Narrow && fitted.is_none() && cursor != Some(start) {
            while x < row.len()
                && cursor != Some(x)
                && matches!(fits[x], Fit::Plain)
                && first.joins(&row[x], shaping.ligatures)
            {
                x += 1;
                // Spaces join only when text follows them in the run.
                if !row[x - 1].blank() {
                    end = x;
                }
            }
            x = end;
        }
        let cells = &row[start..end];
        let plain: String = cells.iter().map(|cell| cell.cell.text.as_str()).collect();
        let mut font = shaping.font.clone();
        if first.cell.style.bold {
            font.weight = shaping.bold_weight;
        }
        if first.cell.style.italic {
            font.style = FontStyle::Italic;
        }
        let ligatures = shaping.ligatures && needs_ligatures(&plain, &font);
        font.features = ligature_features(ligatures);
        let text = RunText::new(cells, ligatures);
        let run = TextRun {
            len: text.text.len(),
            font,
            color: first.fg,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let layout = shape(&text.text, fitted.unwrap_or(shaping.font_size), &[run]);
        ops.push(RowOp::Text {
            column: start,
            cells: if first.cell.wide == CellWide::Wide {
                2
            } else {
                end - start
            },
            wide: first.cell.wide == CellWide::Wide,
            color: first.fg,
            fade: first.fade,
            glyphs: place_glyphs(&layout, &text, shaping.cell_width),
        });
    }
    row_decorations(row, &mut ops);
    ops
}

/// How a cell's character meets the terminal font.
#[derive(Clone, Copy, Debug)]
enum Fit {
    /// Drawn as shaped, in the terminal font or a fallback face of about a cell's width.
    Plain,
    /// A Private Use Area icon the font lacks, fitted from the symbols font.
    Icon(IconGlyph),
    /// A fallback glyph wider than its cell, shrunk to fit at this size.
    Shrunk(Pixels),
}

/// Fallback fitting for each cell, as herdr's `fallback_icon` and `fallback_size`: only text
/// the terminal font has no glyph for is touched, so the configured font keeps its own icon
/// proportions.
fn fallback_fits(
    row: &[RunCell],
    shaping: &Shaping,
    shape: &mut impl FnMut(&str, Pixels, &[TextRun]) -> Arc<LineLayout>,
) -> Vec<Fit> {
    row.iter()
        .map(|cell| {
            let text = cell.cell.text.as_str();
            if text.is_ascii() || cell.blank() {
                return Fit::Plain;
            }
            let Some(first) = text.chars().next() else {
                return Fit::Plain;
            };
            if (shaping.has_glyph)(first) {
                return Fit::Plain;
            }
            if is_icon(text)
                && let Some(icon) = icon_glyph(first)
            {
                return Fit::Icon(icon);
            }
            if cell.cell.wide != CellWide::Narrow {
                return Fit::Plain;
            }
            let mut font = shaping.font.clone();
            font.features = ligature_features(false);
            let run = TextRun {
                len: text.len(),
                font,
                color: cell.fg,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let width = shape(text, shaping.font_size, &[run]).width;
            if width > shaping.cell_width {
                Fit::Shrunk(shaping.font_size * f32::from(shaping.cell_width) / f32::from(width))
            } else {
                Fit::Plain
            }
        })
        .collect()
}

/// A Nerd Font icon: one character in the Unicode Private Use Areas.
fn is_icon(text: &str) -> bool {
    let mut chars = text.chars();
    matches!(
        (chars.next(), chars.next()),
        (Some(c), None) if matches!(c as u32, 0xE000..=0xF8FF | 0xF0000..=0x10FFFD)
    )
}

/// The bundled symbols font's family, as GPUI names it.
pub(crate) const SYMBOLS_FAMILY: &str = "Symbols Nerd Font";

/// An icon's outline box in the symbols font, in font units, read from the file itself and not
/// through shaping, which may take the character from another installed font (old Nerd Fonts
/// put a squirrel where the markdown icon is now).
#[derive(Clone, Copy, Debug, PartialEq)]
struct IconGlyph {
    id: u16,
    units_per_em: f32,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

fn icon_glyph(c: char) -> Option<IconGlyph> {
    static FACE: LazyLock<Option<ttf_parser::Face<'static>>> =
        LazyLock::new(|| ttf_parser::Face::parse(super::SYMBOLS_FONT, 0).ok());
    let face = FACE.as_ref()?;
    let id = face.glyph_index(c)?;
    let b = face.glyph_bounding_box(id)?;
    Some(IconGlyph {
        id: id.0,
        units_per_em: f32::from(face.units_per_em()),
        x0: f32::from(b.x_min),
        y0: f32::from(b.y_min),
        x1: f32::from(b.x_max),
        y1: f32::from(b.y_max),
    })
}

impl IconGlyph {
    /// Fits the icon by its outline: centered in its cell, as tall as fits, and, when a blank
    /// follows (`room`), up to a cell and a half wide so it doesn't touch the next word.
    fn place(&self, shaping: &Shaping, room: bool) -> PlacedGlyph {
        let cell_width = f32::from(shaping.cell_width);
        let (width, height) = (
            cell_width * if room { 2. } else { 1. },
            f32::from(shaping.cell_height),
        );
        let (gw, gh) = ((self.x1 - self.x0).max(1.), (self.y1 - self.y0).max(1.));
        let scale = (width * if room { 0.75 } else { 0.9 } / gw).min(height * 0.72 / gh);
        let left = ((cell_width - gw * scale) / 2.).max(0.) - self.x0 * scale;
        let baseline = (height - gh * scale) / 2. + self.y1 * scale;
        PlacedGlyph {
            font: shaping.symbols,
            id: glyph_id(u32::from(self.id)),
            font_size: px(scale * self.units_per_em),
            offset: point(px(left), px(baseline)),
            emoji: false,
        }
    }
}

/// A row's underlines and strikethroughs, from its cells alone, so blanks and wide cells'
/// spacers get them too. Neighbours that look the same share one.
fn row_decorations(row: &[RunCell], ops: &mut Vec<RowOp>) {
    let mut x = 0;
    while x < row.len() {
        let run = &row[x];
        let start = x;
        x += if run.cell.wide == CellWide::Wide {
            2
        } else {
            1
        };
        // The image draws over a placeholder, so it shows no decorations either.
        if run.placeholder() {
            continue;
        }
        let style = run.cell.style;
        let underline = (style.underline != Underline::None).then(|| {
            let color = run
                .cell
                .underline_color
                .map_or(run.fg, |c| c.opacity(run.fg.a));
            (style.underline, color)
        });
        let strike = style.strikethrough.then_some(run.fg);
        if underline.is_none() && strike.is_none() {
            continue;
        }
        match ops.last_mut() {
            Some(RowOp::Decoration {
                to,
                underline: u,
                strike: s,
                ..
            }) if *to == start && *u == underline && *s == strike => *to = x,
            _ => ops.push(RowOp::Decoration {
                from: start,
                to: x,
                underline,
                strike,
            }),
        }
    }
}

/// GPUI keeps `GlyphId`'s field private; only its text systems make one.
pub(super) fn glyph_id(id: u32) -> GlyphId {
    // SAFETY: `GlyphId` is a `#[repr(C)]` struct of one `u32`.
    unsafe { std::mem::transmute::<u32, GlyphId>(id) }
}

/// A WORD JOINER. cosmic-text 0.14, which GPUI 0.2.2 shapes with on Linux, shapes each
/// UAX #14 segment of a line on its own, and `!`, `-` and `|` end a segment, so `!=`, `->` and
/// `|>` reach the font in two pieces and never join. A joiner between them forbids the break,
/// and the shaper skips it, as a default-ignorable character, when matching the font's
/// ligature and contextual-alternate lookups.
const JOINER: char = '\u{2060}';

/// A run's text as it is shaped: its cells' text, with a joiner between two punctuation
/// marks when ligatures are on.
pub(super) struct RunText {
    pub(super) text: String,
    /// The byte offset where each cell's text starts.
    pub(super) offsets: Vec<usize>,
    /// The byte offsets of the joiners, whose glyphs aren't drawn.
    pub(super) joiners: Vec<usize>,
}

impl RunText {
    fn new(cells: &[RunCell], ligatures: bool) -> Self {
        let mut run = Self {
            text: String::new(),
            offsets: Vec::with_capacity(cells.len()),
            joiners: Vec::new(),
        };
        for cell in cells {
            let text = &cell.cell.text;
            if ligatures
                && run.text.ends_with(|c: char| c.is_ascii_punctuation())
                && text.starts_with(|c: char| c.is_ascii_punctuation())
            {
                run.joiners.push(run.text.len());
                run.text.push(JOINER);
            }
            run.offsets.push(run.text.len());
            run.text.push_str(text);
        }
        run
    }
}

/// Moves each glyph cluster onto its cell. A glyph belongs to the last cell starting at or
/// before its index. Glyphs after the first of a cell keep their shaped distance from it.
pub(super) fn place_glyphs(
    layout: &LineLayout,
    text: &RunText,
    cell_width: Pixels,
) -> Vec<PlacedGlyph> {
    let mut placed = Vec::new();
    let mut cluster: Option<(usize, Pixels)> = None;
    for run in &layout.runs {
        for glyph in &run.glyphs {
            if text.joiners.binary_search(&glyph.index).is_ok() {
                continue;
            }
            let cell = text
                .offsets
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
    use crate::terminal::frame::Frame;
    use ghostty_vt::{Terminal, render::RenderState, terminal::Options};
    use gpui::{ShapedGlyph, ShapedRun, font, px, rgb};

    fn screen(output: &[u8]) -> Frame {
        let mut terminal = Terminal::new(Options::default()).unwrap();
        terminal.vt_write(output);
        Frame::default()
            .update(&mut RenderState::new().unwrap(), &terminal)
            .unwrap()
            .unwrap()
    }

    fn cells(frame: &Frame, row: usize) -> Vec<RunCell<'_>> {
        frame.rows[row]
            .cells
            .iter()
            .map(|cell| RunCell {
                cell,
                fg: cell.fg,
                fade: None,
            })
            .collect()
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
            cell_height: px(18.),
            symbols: FontId(1),
            has_glyph: &|c| c != '\u{e0a0}' && c != '\u{f09b}',
            ligatures,
            bold_weight: FontWeight::BOLD,
            cursor,
        }
    }

    #[test]
    fn underlines_and_strikethrough_merge_across_neighbours_that_match() {
        let frame = screen(b"\x1b[4:3;58;2;255;0;0mab\x1b[24;9mcd\x1b[0;4mef \x1b[0;21;9mg");
        let row = cells(&frame, 0);
        let font = gpui::font("Test");
        let mut texts = Vec::new();
        let ops = row_ops(&row, &shaping(&font, false, None), monospace(&mut texts));
        let decorations: Vec<_> = ops
            .iter()
            .filter_map(|op| match op {
                RowOp::Decoration {
                    from,
                    to,
                    underline,
                    strike,
                } => Some((*from, *to, underline.map(|(u, _)| u), strike.is_some())),
                _ => None,
            })
            .collect();
        assert_eq!(
            decorations,
            [
                (0, 2, Some(Underline::Curly), false),
                (2, 4, None, true),
                (4, 7, Some(Underline::Single), false),
                (7, 8, Some(Underline::Double), true),
            ]
        );
        let RowOp::Decoration { underline, .. } = ops
            .iter()
            .find(|op| matches!(op, RowOp::Decoration { from: 0, .. }))
            .unwrap()
        else {
            unreachable!()
        };
        assert_eq!(underline.unwrap().1, Hsla::from(rgb(0xff0000)));
    }

    #[test]
    fn inverse_swaps_colors_invisible_hides_text_and_faint_is_a_style() {
        let frame = screen(b"\x1b[7mA\x1b[0;8mB\x1b[0;2mC");
        let cells = &frame.rows[0].cells;
        // The default foreground becomes the background.
        assert_eq!(cells[0].bg, Some(cells[1].fg));
        assert_ne!(cells[0].fg, cells[0].bg.unwrap());
        assert_eq!(cells[1].text, "B");
        assert!(cells[1].style.invisible);
        assert!(cells[2].style.faint);
    }

    #[test]
    fn concealed_text_draws_nothing() {
        let frame = screen(b"a\x1b[8mBC\x1b[0mz");
        let row = cells(&frame, 0);
        let font = gpui::font("Test");
        let mut texts = Vec::new();
        row_ops(&row, &shaping(&font, false, None), monospace(&mut texts));
        assert_eq!(texts, ["a", "z"]);
    }

    #[test]
    fn kitty_placeholders_draw_no_glyph() {
        let frame = screen("a\u{10EEEE}\u{0305}\u{0305}z".as_bytes());
        let row = cells(&frame, 0);
        let font = gpui::font("Test");
        let mut texts = Vec::new();
        row_ops(&row, &shaping(&font, false, None), monospace(&mut texts));
        assert_eq!(texts, ["a", "z"]);
    }

    #[test]
    fn styled_kitty_placeholders_draw_no_underline_or_strikethrough() {
        let frame =
            screen("\x1b[4;9ma\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\u{0305}\u{030D}z".as_bytes());
        let row = cells(&frame, 0);
        let font = gpui::font("Test");
        let mut texts = Vec::new();
        let ops = row_ops(&row, &shaping(&font, false, None), monospace(&mut texts));
        assert_eq!(texts, ["a", "z"]);
        let decorations: Vec<_> = ops
            .iter()
            .filter_map(|op| match op {
                RowOp::Decoration { from, to, .. } => Some((*from, *to)),
                _ => None,
            })
            .collect();
        assert_eq!(decorations, [(0, 1), (3, 4)]);
    }

    #[test]
    fn faint_color_emoji_fade_into_their_background() {
        let frame = screen("\x1b[2m中\x1b[0m".as_bytes());
        let mut row = cells(&frame, 0);
        let veil = Hsla::from(rgb(0x102030));
        row[0].fade = Some(veil);
        let font = gpui::font("Test");
        let mut ops = row_ops(
            &row,
            &shaping(&font, false, None),
            monospace(&mut Vec::new()),
        );
        let RowOp::Text { glyphs, fade, .. } = ops
            .iter_mut()
            .find(|op| matches!(op, RowOp::Text { .. }))
            .unwrap()
        else {
            unreachable!()
        };
        assert_eq!(*fade, Some(veil));
        glyphs[0].emoji = true;
        let op = ops
            .iter()
            .find(|op| matches!(op, RowOp::Text { .. }))
            .unwrap();
        assert_eq!(op.faded_emoji(px(8.)), [(0, 2, veil)]);
        // Without a fade (regular text), nothing is veiled.
        let RowOp::Text { fade, .. } = ops
            .iter_mut()
            .find(|op| matches!(op, RowOp::Text { .. }))
            .unwrap()
        else {
            unreachable!()
        };
        *fade = None;
        let op = ops
            .iter()
            .find(|op| matches!(op, RowOp::Text { .. }))
            .unwrap();
        assert!(op.faded_emoji(px(8.)).is_empty());
    }

    #[test]
    fn faint_narrow_emoji_veil_only_its_own_cell() {
        let frame = screen("\x1b[2mA\u{2665}Z\x1b[0m".as_bytes());
        let mut row = cells(&frame, 0);
        let veil = Hsla::from(rgb(0x102030));
        for cell in &mut row {
            cell.fade = Some(veil);
        }
        let font = gpui::font("Test");
        let mut ops = row_ops(
            &row,
            &shaping(&font, false, None),
            monospace(&mut Vec::new()),
        );
        let RowOp::Text { glyphs, cells, .. } = ops
            .iter_mut()
            .find(|op| matches!(op, RowOp::Text { .. }))
            .unwrap()
        else {
            unreachable!()
        };
        assert_eq!(*cells, 3);
        glyphs[1].emoji = true;
        let op = ops
            .iter()
            .find(|op| matches!(op, RowOp::Text { .. }))
            .unwrap();
        assert_eq!(op.faded_emoji(px(7.)), [(1, 1, veil)]);
    }

    #[test]
    fn bold_and_italic_text_shape_in_their_own_runs() {
        let frame = screen(b"a\x1b[1mb\x1b[0;3mc");
        let row = cells(&frame, 0);
        let font = gpui::font("Test");
        let mut fonts = Vec::new();
        let ops = row_ops(&row, &shaping(&font, false, None), |text, size, runs| {
            fonts.push((runs[0].font.weight, runs[0].font.style));
            monospace(&mut Vec::new())(text, size, runs)
        });
        assert_eq!(ops.len(), 3);
        assert_eq!(
            fonts,
            [
                (font.weight, FontStyle::Normal),
                (FontWeight::BOLD, FontStyle::Normal),
                (font.weight, FontStyle::Italic),
            ]
        );
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
        let text = RunText {
            text: "x\u{301}b".into(),
            offsets: vec![0, 3],
            joiners: Vec::new(),
        };
        let placed = place_glyphs(&layout, &text, px(8.));
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
                RowOp::Background { .. }
                | RowOp::Icon { .. }
                | RowOp::Sprite { .. }
                | RowOp::Decoration { .. } => None,
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
    fn sprites_break_text_runs_and_keep_their_cell_and_colors() {
        let font = font("JetBrains Mono");
        let frame = screen("\x1b[44mab─█⣿\u{e0b0}⏺cd\x1b[0m".as_bytes());
        let mut row = cells(&frame, 0);
        let color = rgb(0xaabbee).into();
        row[3].fg = color;
        let background = row[3].cell.bg.unwrap();
        for ligatures in [false, true] {
            let mut texts = Vec::new();
            let ops = row_ops(
                &row,
                &shaping(&font, ligatures, None),
                monospace(&mut texts),
            );
            assert_eq!(texts, ["ab", "cd"]);
            assert_eq!(
                ops[0],
                RowOp::Background {
                    from: 0,
                    to: 9,
                    color: background,
                },
            );
            assert!(matches!(ops[1], RowOp::Text { column: 0, .. }));
            for (index, character) in ['─', '█', '⣿', '\u{e0b0}', '⏺'].into_iter().enumerate()
            {
                let column = index + 2;
                assert_eq!(
                    ops[index + 2],
                    RowOp::Sprite {
                        column,
                        character,
                        color: row[column].fg,
                    },
                );
            }
            assert!(matches!(ops[7], RowOp::Text { column: 7, .. }));
            assert_eq!(ops.len(), 8);
        }
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
    fn missing_icons_are_fitted_from_the_symbols_font() {
        let font = font("JetBrains Mono");
        // A powerline branch the font lacks, then text: it is drawn on its own, over two cells
        // when a blank follows, and breaks the text run around it.
        let frame = screen("ab\u{e0a0} cd\u{f09b}x".as_bytes());
        let mut texts = Vec::new();
        let ops = row_ops(
            &cells(&frame, 0),
            &shaping(&font, false, None),
            monospace(&mut texts),
        );
        assert_eq!(texts, ["ab", "cd", "x"]);
        let icons: Vec<_> = ops
            .iter()
            .filter_map(|op| match op {
                RowOp::Icon { column, glyph, .. } => Some((*column, glyph.font)),
                _ => None,
            })
            .collect();
        assert_eq!(icons, [(2, FontId(1)), (6, FontId(1))]);
        let sizes: Vec<_> = ops
            .iter()
            .filter_map(|op| match op {
                RowOp::Icon { glyph, .. } => Some(f32::from(glyph.font_size)),
                _ => None,
            })
            .collect();
        // Room to spare after the first, none after the second: it is no larger.
        assert!(sizes[0] >= sizes[1], "{sizes:?}");
        assert!(sizes.iter().all(|size| *size > 0.));
    }

    #[test]
    fn icons_the_font_has_stay_in_their_text_run() {
        let font = font("JetBrains Mono");
        let frame = screen("a\u{e0a0}b".as_bytes());
        let mut texts = Vec::new();
        let has_glyph = |_: char| true;
        let shaping = Shaping {
            has_glyph: &has_glyph,
            ..shaping(&font, false, None)
        };
        row_ops(&cells(&frame, 0), &shaping, monospace(&mut texts));
        assert_eq!(texts, ["a\u{e0a0}b"]);
    }

    #[test]
    fn icons_in_the_last_column_stay_inside_their_cell() {
        let font = font("JetBrains Mono");
        let frame = screen("ab\u{f141}".as_bytes());
        let mut row = cells(&frame, 0);
        row.truncate(3);
        let has_glyph = |c: char| c != '\u{f141}';
        let icon_shaping = Shaping {
            has_glyph: &has_glyph,
            ..shaping(&font, false, None)
        };
        let size = |row: &[RunCell]| {
            let ops = row_ops(row, &icon_shaping, monospace(&mut Vec::new()));
            ops.iter()
                .find_map(|op| match op {
                    RowOp::Icon { glyph, .. } => Some(glyph.font_size),
                    _ => None,
                })
                .unwrap()
        };
        let last = size(&row);
        let padded_frame = screen("ab\u{f141} ".as_bytes());
        let mut padded = cells(&padded_frame, 0);
        padded.truncate(4);
        // With a blank after it the icon may be larger; at the edge it fits one cell.
        assert!(last < size(&padded), "{last:?}");
        let icon = icon_glyph('\u{f141}').unwrap();
        let placed = icon.place(&shaping(&font, false, None), false);
        let scale = f32::from(placed.font_size) / icon.units_per_em;
        let right = f32::from(placed.offset.x) + icon.x1 * scale;
        assert!(
            right <= f32::from(shaping(&font, false, None).cell_width) + 0.01,
            "{right}"
        );
    }

    #[test]
    fn emoji_keep_the_face_gpui_falls_back_to() {
        // Emoji are shaped with the terminal font, so GPUI's own fallback picks the color
        // face: naming Noto Color Emoji drops it on Linux, which has no `m` glyph.
        let font = font("JetBrains Mono");
        let frame = screen("a\u{2764}\u{fe0f}b".as_bytes());
        let has_glyph = |_: char| true;
        let shaping = Shaping {
            has_glyph: &has_glyph,
            ..shaping(&font, false, None)
        };
        let emoji_face = FontId(7);
        let ops = row_ops(&cells(&frame, 0), &shaping, |text, size, runs| {
            assert_eq!(runs[0].font.family, font.family, "{text}");
            let glyphs = text
                .char_indices()
                .filter(|(_, c)| *c != '\u{fe0f}')
                .enumerate()
                .map(|(n, (index, c))| ShapedGlyph {
                    is_emoji: c == '\u{2764}',
                    ..glyph(index, 7. * n as f32)
                })
                .collect::<Vec<_>>();
            let (emoji, text_glyphs): (Vec<_>, Vec<_>) =
                glyphs.into_iter().partition(|glyph| glyph.is_emoji);
            Arc::new(LineLayout {
                font_size: size,
                len: text.len(),
                runs: vec![
                    ShapedRun {
                        font_id: FontId(0),
                        glyphs: text_glyphs,
                    },
                    ShapedRun {
                        font_id: emoji_face,
                        glyphs: emoji,
                    },
                ],
                ..LineLayout::default()
            })
        });
        let placed: Vec<_> = ops
            .iter()
            .filter_map(|op| match op {
                RowOp::Text { glyphs, .. } => Some(glyphs),
                _ => None,
            })
            .flatten()
            .collect();
        let heart = placed.iter().find(|glyph| glyph.emoji).expect("{placed:?}");
        assert_eq!(heart.font, emoji_face);
        assert_eq!(
            placed
                .iter()
                .filter(|glyph| glyph.font == FontId(0))
                .count(),
            2
        );
    }
    #[test]
    fn symbols_font_has_a_glyph_for_common_icons() {
        for c in ['\u{e0b0}', '\u{f09b}', '\u{e62b}'] {
            assert!(icon_glyph(c).is_some(), "{c:?}");
        }
        assert!(icon_glyph('a').is_none());
        assert!(is_icon("\u{e0b0}") && !is_icon("a") && !is_icon("\u{e0b0}\u{e0b0}"));
    }

    #[test]
    fn wide_characters_and_emoji_take_two_cells_and_skip_their_spacers() {
        let font = font("JetBrains Mono");
        // CJK, an emoji, a VS16 text-presentation heart (2027 makes it two cells), then text.
        let frame = screen("中😀\u{2764}\u{fe0f}ab".as_bytes());
        let cells = cells(&frame, 0);
        let wide: Vec<_> = cells.iter().map(|cell| cell.cell.wide).collect();
        assert_eq!(
            wide[..7],
            [
                CellWide::Wide,
                CellWide::SpacerTail,
                CellWide::Wide,
                CellWide::SpacerTail,
                CellWide::Wide,
                CellWide::SpacerTail,
                CellWide::Narrow
            ]
        );
        let mut texts = Vec::new();
        let ops = row_ops(&cells, &shaping(&font, false, None), monospace(&mut texts));
        assert_eq!(texts, ["中", "😀", "\u{2764}\u{fe0f}", "ab"]);
        let columns: Vec<_> = ops
            .iter()
            .filter_map(|op| match op {
                RowOp::Text { column, .. } => Some(*column),
                _ => None,
            })
            .collect();
        assert_eq!(columns, [0, 2, 4, 6]);
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

    #[test]
    fn ligatures_join_punctuation_across_segment_breaks() {
        let frame = screen(b"a!=b x");
        let row = cells(&frame, 0);
        let joined = RunText::new(&row[..4], true);
        assert_eq!(joined.text, "a!\u{2060}=b");
        assert_eq!(joined.offsets, [0, 1, 5, 6]);
        assert_eq!(joined.joiners, [2]);
        assert_eq!(RunText::new(&row[..4], false).text, "a!=b");
        // A joiner's glyph isn't drawn; the cells after it keep their places.
        let layout = LineLayout {
            font_size: px(13.),
            runs: vec![ShapedRun {
                font_id: FontId(0),
                glyphs: vec![glyph(0, 0.), glyph(1, 7.), glyph(2, 14.), glyph(5, 14.)],
            }],
            ..LineLayout::default()
        };
        let xs: Vec<_> = place_glyphs(&layout, &joined, px(8.))
            .iter()
            .map(|glyph| f32::from(glyph.offset.x))
            .collect();
        assert_eq!(xs, [0., 8., 16.]);
    }

    /// Port of herdr-app's `ligatures_change_glyphs_without_moving_terminal_cells`, in a
    /// headless GPUI app with the system's fonts and the bundled one, as Power runs. The font
    /// is chosen by `terminal_font` and resolved by GPUI's text system, and the cell is
    /// measured from it as the grid is. Stock GPUI shapes only through a window, so runs are
    /// shaped by cosmic-text as `CosmicTextSystemState::layout_line` does, with the family
    /// and features GPUI resolved.
    #[cfg(target_os = "linux")]
    #[test]
    fn ligatures_change_glyphs_without_moving_terminal_cells() {
        use gpui::Application;

        // A headless app's `quit` doesn't end its run loop, so the app runs on its own thread
        // and hands back the outcome.
        let (done, outcome) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            Application::headless().run(move |cx| {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    shape_on_the_terminal_font(cx)
                }));
                let _ = done.send(result);
                cx.quit();
            });
        });
        if let Err(panic) = outcome.recv().expect("the app ran") {
            std::panic::resume_unwind(panic);
        }
    }

    /// The same test with no system fonts, so the bundled font is the only face GPUI has and
    /// none of its fallback stack resolves. Fontconfig is read once per process, so the test
    /// reruns this binary with an empty configuration.
    #[cfg(target_os = "linux")]
    #[test]
    fn ligatures_shape_with_only_the_bundled_font() {
        let root = std::env::temp_dir().join(format!("power-no-fonts-{}", std::process::id()));
        std::fs::create_dir_all(root.join("fonts")).unwrap();
        let config = root.join("fonts.conf");
        std::fs::write(
            &config,
            format!(
                "<fontconfig><dir>{0}/fonts</dir><cachedir>{0}/cache</cachedir></fontconfig>\n",
                root.display()
            ),
        )
        .unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "terminal::runs::tests::ligatures_change_glyphs_without_moving_terminal_cells",
                "--test-threads=1",
            ])
            .env("FONTCONFIG_FILE", &config)
            .env_remove("FONTCONFIG_PATH")
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&root);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(target_os = "linux")]
    fn shape_on_the_terminal_font(cx: &mut gpui::App) {
        use crate::{
            settings::TerminalSettings,
            terminal::{TerminalFont, cell_metrics, load_bundled_fonts, terminal_font},
        };
        use cosmic_text::{
            Attrs, AttrsList, Family, FeatureTag, FontFeatures as Features, FontSystem,
            ShapeBuffer, ShapeLine, Shaping as Shaper, Weight, Wrap,
        };
        {
            load_bundled_fonts(cx);
            let mut system = FontSystem::new();
            system
                .db_mut()
                .load_font_data(super::super::BUNDLED_FONT.to_vec());
            let mut scratch = ShapeBuffer::default();
            let text_system = cx.text_system().clone();
            let mut shape = |text: &str, font_size: Pixels, runs: &[TextRun]| {
                // The face GPUI resolves, never its fallback stack.
                let id = text_system.resolve_font(&runs[0].font);
                let resolved = text_system.get_font_for_id(id).unwrap();
                assert_eq!(
                    resolved.family, runs[0].font.family,
                    "{text}: fallback font"
                );
                let mut features = Features::new();
                for (tag, value) in resolved.features.0.iter() {
                    let tag: [u8; 4] = tag.as_bytes().try_into().unwrap();
                    features.set(FeatureTag::new(&tag), *value);
                }
                let attrs = Attrs::new()
                    .family(Family::Name(&resolved.family))
                    .weight(Weight(resolved.weight.0 as u16))
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
                        font_id: id,
                        glyphs,
                    }],
                    ..LineLayout::default()
                })
            };

            // An unset family and one no machine has resolve to the bundled font, whose features
            // the runs then set. Installed families (e.g. Helvetica) legitimately resolve to
            // themselves, so the test names none.
            for family in [None, Some("No Such Font")] {
                let settings = TerminalSettings {
                    font_family: family.map(str::to_owned),
                    ..TerminalSettings::default()
                };
                let font = terminal_font(&settings, cx).font;
                assert!(font.family.contains("JetBrains"), "{family:?}: {font:?}");
            }
            // A family the machine has is used as named, even one GPUI's fallback stack lists.
            for family in ["Noto Sans", "Helvetica", "Liberation Mono"] {
                if crate::terminal::has_family(family) {
                    let settings = TerminalSettings {
                        font_family: Some(family.into()),
                        ..TerminalSettings::default()
                    };
                    assert_eq!(terminal_font(&settings, cx).font.family.as_ref(), family);
                }
            }
            let settings = TerminalSettings {
                font_family: Some("No Such Font".into()),
                ..TerminalSettings::default()
            };
            for font_size in [10., 14., 24., 36.] {
                let font = TerminalFont {
                    size: font_size,
                    ..terminal_font(&settings, cx)
                };
                let cell_width = cell_metrics(&text_system, &font, 1.).size.width;
                for (text, joins) in [
                    ("a=>b", true),
                    ("a!=b", true),
                    ("a===b", true),
                    ("a->b", true),
                    ("a<=b", true),
                    ("a>=b", true),
                    ("a|>b", true),
                    // JetBrains Mono's `liga` would join nothing here; a fallback's would.
                    ("office", false),
                ] {
                    let frame = screen(text.as_bytes());
                    let row = cells(&frame, 0);
                    let mut variants = Vec::new();
                    for ligatures in [false, true] {
                        let shaping = Shaping {
                            font: &font.font,
                            font_size: px(font_size),
                            cell_width,
                            cell_height: px(font_size * 1.4),
                            symbols: FontId(1),
                            has_glyph: &|_| true,
                            ligatures,
                            bold_weight: FontWeight::BOLD,
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
                    assert_eq!(
                        variants[0] != variants[1],
                        joins,
                        "{text} at {font_size} px: ligatures change the glyphs"
                    );
                }
            }
        }
    }
}
