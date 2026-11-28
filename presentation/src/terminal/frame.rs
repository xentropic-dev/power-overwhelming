//! Row damage and reuse adapted from herdr-app `src/vt.rs`
//! (https://github.com/ogulcancelik/herdr-app), licensed under Apache-2.0. Minimum
//! contrast is adapted from the same file. Wrapped link scanning adapted from herdr-app
//! `src/vt.rs`, Apache-2.0. Cursor style and wide-tail handling follow herdr-app
//! `src/terminal.rs` (Apache-2.0).

use super::links::{candidate, mark_links};
use super::paint::RowDrawing;
use anyhow::Result;
use ghostty_vt::{
    Error, Terminal,
    render::{
        CellIterator, Colors, CursorVisualStyle, Dirty, ImageIterator, ImagePlacement, RenderState,
        RowId, RowIteration, RowIterator, Snapshot,
    },
    screen::CellWide,
    style::{Style, StyleColor, Underline},
    terminal::{Point as VtPoint, PointCoordinate, PointSpace},
};
use gpui::{Hsla, rgb};
use std::{cell::RefCell, collections::HashMap, ops::Range, rc::Rc, sync::Arc};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct CellPosition {
    pub(super) row: usize,
    pub(super) column: usize,
}

#[derive(Clone)]
pub(super) struct Cell {
    /// The cell's grapheme, or a space when it has none. Concealed (SGR 8) cells keep theirs,
    /// which selection copies; drawing hides them.
    pub(super) text: String,
    pub(super) fg: Hsla,
    pub(super) bg: Option<Hsla>,
    /// The cell's SGR attributes, as Ghostty stores them; `fg` and `bg` are already resolved.
    pub(super) style: Style,
    /// The underline's own color (SGR 58), resolved; `None` follows the text.
    pub(super) underline_color: Option<Hsla>,
    pub(super) wide: CellWide,
    pub(super) link: Option<Arc<str>>,
    /// The link came from scanning text rather than OSC 8, so a rescan replaces it.
    pub(super) text_link: bool,
}

pub(super) struct Row {
    id: RowId,
    pub(super) cells: Vec<Cell>,
    /// Row-local runs of clickable cells, retained with the text.
    pub(super) links: Vec<Range<usize>>,
    /// The row's first link continues the previous row's last one across a soft wrap.
    pub(super) continues_link: bool,
    /// The row's last link runs on into the next row across a soft wrap.
    pub(super) ends_in_link: bool,
    /// Soft-wrap flags: this row continues the one above, or wraps into the one below.
    continues: bool,
    wraps: bool,
    pub(super) drawing: RefCell<Option<Rc<RowDrawing>>>,
}

/// A visible cursor: its cell (the head of a wide cell) and DECSCUSR shape.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Cursor {
    pub(super) position: CellPosition,
    pub(super) style: CursorVisualStyle,
}

#[derive(Default)]
pub(super) struct Frame {
    pub(super) rows: Vec<Rc<Row>>,
    pub(super) cursor: Option<Cursor>,
    /// Where the cursor is, shown or hidden: input methods compose and place their candidate
    /// window there.
    pub(super) ime_cursor: Option<CellPosition>,
    pub(super) colors: Option<Colors>,
    /// Kitty graphics placements with something to draw, in drawing order (layer, then z).
    pub(super) images: Rc<Vec<ImagePlacement>>,
    images_generation: u64,
    cols: u16,
    minimum_contrast: f32,
}

impl Frame {
    /// None means the last frame still describes the viewport. Only acknowledge
    /// Ghostty's damage after every row and its links have been captured.
    #[cfg(test)]
    pub(super) fn update(
        &self,
        renderer: &mut RenderState,
        terminal: &Terminal,
    ) -> Result<Option<Self>> {
        self.update_with(renderer, terminal, 1.)
    }

    /// Like `update`, raising text below `minimum_contrast` (a WCAG ratio, 1 disables it).
    pub(super) fn update_with(
        &self,
        renderer: &mut RenderState,
        terminal: &Terminal,
        minimum_contrast: f32,
    ) -> Result<Option<Self>> {
        let state = renderer.update(terminal)?;
        let vt_cursor = state.cursor()?;
        let ime_cursor = vt_cursor.viewport.map(|cursor| CellPosition {
            row: cursor.y.into(),
            column: cursor.x.into(),
        });
        let cursor = vt_cursor
            .viewport
            .filter(|_| vt_cursor.visible)
            .map(|at| Cursor {
                position: CellPosition {
                    row: at.y.into(),
                    column: usize::from(at.x).saturating_sub(usize::from(at.at_wide_tail)),
                },
                style: vt_cursor.visual_style,
            });
        let colors = state.colors()?;
        let dirty = state.dirty()?;
        let cols = state.cols()?;
        let height = usize::from(state.rows()?);
        let images_generation = state.image_placements_generation()?;
        let images = if images_generation == self.images_generation {
            Rc::clone(&self.images)
        } else {
            Rc::new(image_placements(&state)?)
        };
        let full = dirty == Dirty::Full
            || self.colors.as_ref() != Some(&colors)
            || self.cols != cols
            || self.minimum_contrast != minimum_contrast;
        if !full && dirty == Dirty::Clean && self.rows.len() == height {
            if self.cursor == cursor
                && self.ime_cursor == ime_cursor
                && images_generation == self.images_generation
            {
                return Ok(None);
            }
            return Ok(Some(Self {
                rows: self.rows.clone(),
                cursor,
                ime_cursor,
                colors: Some(colors),
                images,
                images_generation,
                cols,
                minimum_contrast,
            }));
        }

        let mut row_iter = RowIterator::new()?;
        let mut cell_iter = CellIterator::new()?;
        let mut source = row_iter.update(&state)?;
        let previous: HashMap<_, _> = self
            .rows
            .iter()
            .filter(|row| !full && !row.id.is_none())
            .map(|row| (row.id, row))
            .collect();
        let mut rows = Vec::with_capacity(height);
        while let Some(row) = source.next() {
            let id = row.id()?;
            if !full
                && !row.dirty()?
                && let Some(kept) = previous.get(&id)
            {
                rows.push((Rc::clone(kept), false));
            } else {
                rows.push((
                    Rc::new(read_row(
                        row,
                        &mut cell_iter,
                        terminal,
                        rows.len() as u32,
                        id,
                        &colors,
                        minimum_contrast,
                    )?),
                    true,
                ));
            }
        }
        state.clean()?;
        let rows = relink(rows, terminal);
        Ok(Some(Self {
            rows,
            cursor,
            ime_cursor,
            colors: Some(colors),
            images,
            images_generation,
            cols,
            minimum_contrast,
        }))
    }
}

/// The snapshot's Kitty placements, bottom first. Ghostty resolves direct placements and
/// Unicode placeholder runs alike into viewport cells and pixel rectangles.
fn image_placements(state: &Snapshot<'_>) -> Result<Vec<ImagePlacement>> {
    let mut iterator = ImageIterator::new()?;
    let mut source = iterator.update(state)?;
    let mut placements = Vec::new();
    while let Some(placement) = source.next() {
        placements.push(placement?);
    }
    placements.sort_by_key(|placement| (placement.layer, placement.z));
    Ok(placements)
}

fn read_row<'s>(
    row: &'s RowIteration<'s>,
    cell_iter: &'s mut CellIterator,
    terminal: &Terminal,
    y: u32,
    id: RowId,
    colors: &Colors,
    minimum_contrast: f32,
) -> Result<Row> {
    let default_background = rgba_color(
        colors.background.r,
        colors.background.g,
        colors.background.b,
    );
    super::bench::count(&super::bench::DECODED_ROWS);
    let raw_row = row.raw_row()?;
    let continues = raw_row.is_wrap_continuation()?;
    let wraps = raw_row.is_wrapped()?;
    let mut osc_link: Option<Arc<str>> = None;
    let mut uri_buffer = [0; 8192];
    let mut source = cell_iter.update(row)?;
    let mut cells = Vec::new();
    while let Some(cell) = source.next() {
        let mut grapheme = String::new();
        cell.graphemes_utf8(&mut grapheme)?;
        if grapheme.is_empty() {
            grapheme.push(' ');
        }
        let raw = cell.raw_cell()?;
        let osc_uri = if raw.has_hyperlink()? {
            terminal
                .grid_ref(VtPoint::Viewport(PointCoordinate {
                    x: cells.len() as u16,
                    y,
                }))
                .ok()
                .and_then(|reference| reference.hyperlink_uri(&mut uri_buffer).ok())
                .and_then(|count| std::str::from_utf8(&uri_buffer[..count]).ok())
                .filter(|uri| candidate(uri))
        } else {
            None
        };
        osc_link = osc_uri.map(|uri| {
            osc_link
                .take()
                .filter(|previous| previous.as_ref() == uri)
                .unwrap_or_else(|| Arc::from(uri))
        });
        let style = cell.style()?;
        let fg_rgb = cell.fg_color()?.unwrap_or(colors.foreground);
        let bg_color = cell.bg_color()?;
        let mut fg = rgba_color(fg_rgb.r, fg_rgb.g, fg_rgb.b);
        let mut bg = bg_color.map(|c| rgba_color(c.r, c.g, c.b));
        let (mut ink, mut paper) = (fg_rgb, bg_color.unwrap_or(colors.background));
        if style.inverse {
            (fg, bg) = (bg.unwrap_or(default_background), Some(fg));
            (ink, paper) = (paper, ink);
        }
        if minimum_contrast > 1. && !preserves_graphics_color(&grapheme) {
            let ink = packed_rgb(ink.r, ink.g, ink.b);
            let readable = readable(ink, packed_rgb(paper.r, paper.g, paper.b), minimum_contrast);
            if readable != ink {
                fg = rgb(readable).into();
            }
        }
        let underline_color = match style.underline_color {
            _ if style.underline == Underline::None => None,
            StyleColor::None => None,
            StyleColor::Palette(index) => {
                let c = colors.palette[usize::from(index.0)];
                Some(rgba_color(c.r, c.g, c.b))
            }
            StyleColor::Rgb(c) => Some(rgba_color(c.r, c.g, c.b)),
        };
        cells.push(Cell {
            text: grapheme,
            fg,
            bg,
            style,
            underline_color,
            wide: raw.wide()?,
            link: osc_link.clone(),
            text_link: false,
        });
    }
    let mut row = Row {
        id,
        cells,
        links: Vec::new(),
        continues_link: false,
        ends_in_link: false,
        continues,
        wraps,
        drawing: RefCell::default(),
    };
    row.index_links();
    Ok(row)
}

impl Row {
    /// Rebuild the row's link runs and its wrap joins from the cells' links.
    fn index_links(&mut self) {
        let cells = &self.cells;
        let mut spans = Vec::new();
        let mut col = 0;
        while col < cells.len() {
            let start = col;
            col += 1;
            if let Some(link) = &cells[start].link {
                while cells
                    .get(col)
                    .and_then(|c| c.link.as_ref())
                    .is_some_and(|next| Arc::ptr_eq(next, link) || **next == **link)
                {
                    col += 1;
                }
                spans.push(start..col);
            }
        }
        // A wide character that does not fit leaves a spacer in the last column.
        let last = cells
            .iter()
            .rposition(|cell| cell.wide != CellWide::SpacerHead)
            .map_or(0, |last| last + 1);
        self.continues_link = self.continues && spans.first().is_some_and(|span| span.start == 0);
        self.ends_in_link = self.wraps && spans.last().is_some_and(|span| span.end >= last);
        self.links = spans;
    }
}

/// Rescan text links along each soft-wrapped logical line that holds a newly read row, or
/// that runs past the viewport, so URLs that wrap across rows stay one link.
fn relink(rows: Vec<(Rc<Row>, bool)>, terminal: &Terminal) -> Vec<Rc<Row>> {
    let count = rows.len();
    let mut out = Vec::with_capacity(count);
    let mut start = 0;
    while start < count {
        let mut end = start + 1;
        while end < count && rows[end - 1].0.wraps && rows[end].0.continues {
            end += 1;
        }
        let group = &rows[start..end];
        let past_top = start == 0 && group[0].0.continues;
        let past_bottom = end == count && group[end - start - 1].0.wraps;
        if group.iter().any(|(_, fresh)| *fresh) || past_top || past_bottom {
            let mut owned: Vec<Row> = group.iter().map(|(row, _)| row.unlinked()).collect();
            let width = owned[0].cells.len() as u16;
            let prefix = if past_top {
                wrap_prefix(terminal, width)
            } else {
                (String::new(), true)
            };
            let suffix = if past_bottom {
                wrap_suffix(terminal, (end - 1) as u32, width)
            } else {
                (String::new(), true)
            };
            mark_links(&mut owned, prefix, suffix);
            for (mut row, (kept, _)) in owned.into_iter().zip(group) {
                row.index_links();
                if row.same_links(kept) {
                    out.push(Rc::clone(kept));
                } else {
                    out.push(Rc::new(row));
                }
            }
        } else {
            out.extend(group.iter().map(|(row, _)| Rc::clone(row)));
        }
        start = end;
    }
    out
}

impl Row {
    /// A copy without the links found in text, ready for a rescan.
    fn unlinked(&self) -> Row {
        let mut cells = self.cells.clone();
        for cell in &mut cells {
            if cell.text_link {
                cell.link = None;
                cell.text_link = false;
            }
        }
        Row {
            id: self.id,
            cells,
            links: Vec::new(),
            continues_link: false,
            ends_in_link: false,
            continues: self.continues,
            wraps: self.wraps,
            drawing: RefCell::default(),
        }
    }

    fn same_links(&self, other: &Row) -> bool {
        self.links == other.links
            && self.continues_link == other.continues_link
            && self.ends_in_link == other.ends_in_link
            && self
                .cells
                .iter()
                .zip(&other.cells)
                .all(|(a, b)| a.link == b.link)
    }
}

// Bound how much scrollback a single viewport edge can add to a logical line.
const WRAP_REACH: u32 = 32;

/// Scrollback text before the viewport and whether it reaches the logical line's start.
fn wrap_prefix(terminal: &Terminal, width: u16) -> (String, bool) {
    let Some(top) = screen_y(terminal, 0) else {
        return (String::new(), true);
    };
    let mut lines = Vec::new();
    let mut complete = top < WRAP_REACH;
    for y in (top.saturating_sub(WRAP_REACH)..top).rev() {
        let Some((row, text)) = screen_line(terminal, y, width) else {
            complete = true;
            break;
        };
        if !row.is_wrapped().unwrap_or(false) {
            complete = true;
            break;
        }
        lines.push(text);
        if !row.is_wrap_continuation().unwrap_or(false) {
            complete = true;
            break;
        }
    }
    (lines.into_iter().rev().collect(), complete)
}

/// Text after the viewport and whether it reaches the logical line's end.
fn wrap_suffix(terminal: &Terminal, last_visible: u32, width: u16) -> (String, bool) {
    let Some(bottom) = screen_y(terminal, last_visible) else {
        return (String::new(), true);
    };
    let mut suffix = String::new();
    let mut complete = false;
    for y in bottom + 1..=bottom.saturating_add(WRAP_REACH) {
        let Some((row, text)) = screen_line(terminal, y, width) else {
            complete = true;
            break;
        };
        if !row.is_wrap_continuation().unwrap_or(false) {
            complete = true;
            break;
        }
        suffix.push_str(&text);
        if !row.is_wrapped().unwrap_or(false) {
            complete = true;
            break;
        }
    }
    (suffix, complete)
}

fn screen_y(terminal: &Terminal, viewport_y: u32) -> Option<u32> {
    let reference = terminal
        .grid_ref(VtPoint::Viewport(PointCoordinate {
            x: 0,
            y: viewport_y,
        }))
        .ok()?;
    Some(
        terminal
            .point_from_grid_ref(&reference, PointSpace::Screen)
            .ok()??
            .y,
    )
}

fn screen_line(
    terminal: &Terminal,
    y: u32,
    width: u16,
) -> Option<(ghostty_vt::screen::Row, String)> {
    let mut text = String::new();
    let mut grapheme = vec![' '; 16];
    let row = terminal
        .grid_ref(VtPoint::Screen(PointCoordinate { x: 0, y }))
        .ok()?
        .row()
        .ok()?;
    for x in 0..width {
        let reference = terminal
            .grid_ref(VtPoint::Screen(PointCoordinate { x, y }))
            .ok()?;
        if matches!(
            reference.cell().ok()?.wide().ok()?,
            CellWide::SpacerHead | CellWide::SpacerTail
        ) {
            continue;
        }
        let count = match reference.graphemes(&mut grapheme) {
            Err(Error::OutOfSpace { required }) => {
                grapheme.resize(required, ' ');
                reference.graphemes(&mut grapheme)
            }
            result => result,
        }
        .ok()?;
        if count == 0 {
            text.push(' ');
        } else {
            text.extend(grapheme[..count].iter());
        }
    }
    Some((row, text))
}

fn packed_rgb(r: u8, g: u8, b: u8) -> u32 {
    u32::from_be_bytes([0, r, g, b])
}

/// Box drawing, blocks, braille, powerline and legacy computing sprites keep their authored
/// colors even when that color has low contrast.
fn preserves_graphics_color(text: &str) -> bool {
    text.chars().next().is_some_and(|c| {
        ('\u{2500}'..='\u{259f}').contains(&c)
            || ('\u{2800}'..='\u{28ff}').contains(&c)
            || ('\u{e0b0}'..='\u{e0d7}').contains(&c)
            || c == '\u{23fa}'
            || ('\u{1fb00}'..='\u{1fbff}').contains(&c)
            || ('\u{1cc00}'..='\u{1cebf}').contains(&c)
    })
}

/// `foreground`, or near-white / near-black when it would vanish into `background`.
fn readable(foreground: u32, background: u32, minimum: f32) -> u32 {
    if contrast(foreground, background) >= minimum {
        return foreground;
    }
    let (light, dark) = (0xf2f2f4, 0x1b1b1e);
    if contrast(light, background) >= contrast(dark, background) {
        light
    } else {
        dark
    }
}

fn contrast(a: u32, b: u32) -> f32 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

fn luminance(color: u32) -> f32 {
    let [_, r, g, b] = color.to_be_bytes();
    let linear = |value: u8| {
        let value = f32::from(value) / 255.0;
        if value <= 0.03928 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
}

fn rgba_color(r: u8, g: u8, b: u8) -> Hsla {
    rgb(u32::from_be_bytes([0, r, g, b])).into()
}

#[cfg(test)]
mod tests {
    use super::{Frame, RenderState, Terminal, contrast, preserves_graphics_color, readable};
    use gpui::{Hsla, rgb};

    #[test]
    fn minimum_contrast_replaces_low_contrast_text_but_keeps_graphics_colors() {
        let mut terminal = Terminal::new(Options::default()).unwrap();
        terminal.vt_write(b"\x1b[48;2;0;0;0m\x1b[38;2;16;16;16mA\xe2\xa0\x81");
        let frame = Frame::default()
            .update_with(&mut RenderState::new().unwrap(), &terminal, 4.5)
            .unwrap()
            .unwrap();
        let cells = &frame.rows[0].cells;
        assert_eq!(cells[0].fg, Hsla::from(rgb(0xf2f2f4)));
        assert_eq!(cells[1].fg, Hsla::from(rgb(0x101010)));
    }

    #[test]
    fn contrast_ratio_one_leaves_program_colors_unchanged() {
        assert_eq!(readable(0x101010, 0x000000, 1.), 0x101010);
    }

    #[test]
    fn minimum_contrast_selects_a_dark_fallback_on_light_backgrounds() {
        assert_eq!(readable(0xffffff, 0xffffff, 4.5), 0x1b1b1e);
        assert!(contrast(readable(0xffffff, 0xffffff, 4.5), 0xffffff) >= 4.5);
    }

    #[test]
    fn minimum_contrast_exempts_sprite_and_powerline_ranges() {
        for glyph in ["─", "▅", "⠁", "\u{e0b0}", "⏺", "🬀", "\u{1cc00}"] {
            assert!(preserves_graphics_color(glyph), "{glyph}");
        }
        assert!(!preserves_graphics_color("A"));
    }

    fn low_contrast_cell(glyph: &str) -> Hsla {
        let mut terminal = Terminal::new(Options::default()).unwrap();
        terminal.vt_write(format!("\x1b[48;2;0;0;0m\x1b[38;2;16;16;16m{glyph}").as_bytes());
        let frame = Frame::default()
            .update_with(&mut RenderState::new().unwrap(), &terminal, 4.5)
            .unwrap()
            .unwrap();
        frame.rows[0].cells[0].fg
    }

    #[test]
    fn minimum_contrast_keeps_extended_powerline_colors() {
        for glyph in ["\u{e0c0}", "\u{e0d7}"] {
            assert_eq!(low_contrast_cell(glyph), Hsla::from(rgb(0x101010)));
        }
    }

    #[test]
    fn minimum_contrast_keeps_graphics_colors_with_combining_marks() {
        assert_eq!(
            low_contrast_cell("\u{2500}\u{0301}"),
            Hsla::from(rgb(0x101010))
        );
    }
    use ghostty_vt::terminal::Options;
    use std::sync::Arc;

    #[test]
    fn title_only_output_skips_the_frame_and_one_edit_retains_other_rows() {
        use std::rc::Rc;
        let mut terminal = Terminal::new(Options {
            cols: 32,
            rows: 4,
            ..Options::default()
        })
        .unwrap();
        let mut renderer = RenderState::new().unwrap();
        terminal.vt_write(b"pwr://artifact/old\r\nvalue");
        let first = Frame::default()
            .update(&mut renderer, &terminal)
            .unwrap()
            .unwrap();
        terminal.vt_write(b"\x1b]2;Working...\x07");
        assert!(first.update(&mut renderer, &terminal).unwrap().is_none());
        terminal.vt_write(b"\rValue");
        let second = first.update(&mut renderer, &terminal).unwrap().unwrap();
        assert!(!Rc::ptr_eq(&first.rows[1], &second.rows[1]));
        for y in [0, 2, 3] {
            assert!(Rc::ptr_eq(&first.rows[y], &second.rows[y]));
        }
        assert_eq!(
            second.rows[0].cells[0].link.as_deref(),
            Some("pwr://artifact/old")
        );
        assert_eq!(second.rows[1].cells[0].text, "V");
        terminal.vt_write(b"\x1b[?25l");
        let hidden = second.update(&mut renderer, &terminal).unwrap().unwrap();
        assert!(hidden.cursor.is_none());
        assert!(hidden.update(&mut renderer, &terminal).unwrap().is_none());
    }

    #[test]
    fn resize_and_screen_switches_replace_the_visible_rows() {
        let mut terminal = Terminal::new(Options {
            cols: 32,
            rows: 4,
            ..Options::default()
        })
        .unwrap();
        let mut renderer = RenderState::new().unwrap();
        terminal.vt_write(b"pwr://artifact/main");
        let mut frame = Frame::default()
            .update(&mut renderer, &terminal)
            .unwrap()
            .unwrap();
        terminal.vt_write(b"\x1b[?1049h\x1b[Halt");
        frame = frame.update(&mut renderer, &terminal).unwrap().unwrap();
        assert_eq!(frame.rows[0].cells[0].text, "a");
        assert!(frame.rows[0].links.is_empty());
        terminal.vt_write(b"\x1b[?1049l");
        frame = frame.update(&mut renderer, &terminal).unwrap().unwrap();
        assert_eq!(
            frame.rows[0].cells[0].link.as_deref(),
            Some("pwr://artifact/main")
        );
        terminal.resize(24, 3, 8, 16).unwrap();
        frame = frame.update(&mut renderer, &terminal).unwrap().unwrap();
        assert_eq!(frame.rows.len(), 3);
        assert!(frame.rows.iter().all(|row| row.cells.len() == 24));
    }

    #[test]
    fn dirty_and_clean_halves_of_a_wrapped_osc_link_share_a_highlight() {
        use super::CellPosition;
        let mut terminal = Terminal::new(Options {
            cols: 8,
            rows: 4,
            ..Options::default()
        })
        .unwrap();
        let mut renderer = RenderState::new().unwrap();
        terminal.vt_write(b"xx\x1b]8;;pwr://artifact/a\x07abcdefghijkl\x1b]8;;\x07\x1b[H");
        let first = Frame::default()
            .update(&mut renderer, &terminal)
            .unwrap()
            .unwrap();
        // With the cursor already on the first row, overwriting the prefix
        // damages just that row and leaves the continuation cached.
        terminal.vt_write(b"X");
        let second = first.update(&mut renderer, &terminal).unwrap().unwrap();
        let start = CellPosition { row: 0, column: 2 };
        let end = CellPosition { row: 1, column: 5 };
        assert_eq!(second.link_span(start), Some((start, end)));
        assert_eq!(second.link_span(end), Some((start, end)));
        assert!(std::rc::Rc::ptr_eq(&first.rows[1], &second.rows[1]));
    }

    #[test]
    fn existing_indexed_cells_follow_the_scheme_and_truecolor_cells_keep_their_color() {
        use gpui::{Hsla, rgb};
        use infrastructure::theme;

        let mut terminal = Terminal::new(Options::default()).unwrap();
        let mut renderer = RenderState::new().unwrap();
        let dark = theme::default_scheme();
        let light = theme::find("Catppuccin Latte").unwrap();
        dark.apply_to(&mut terminal).unwrap();
        terminal.vt_write(b"\x1b[31mA\x1b[38;2;12;34;56mB");
        let mut rendered = Frame::default();
        for scheme in [dark, light] {
            scheme.apply_to(&mut terminal).unwrap();
            rendered = rendered.update(&mut renderer, &terminal).unwrap().unwrap();
            let cells = &rendered.rows[0].cells;
            assert_eq!(cells[0].fg, Hsla::from(rgb(scheme.colors.indexed[1])));
            assert_eq!(cells[1].fg, Hsla::from(rgb(0x0c2238)));
        }
    }

    #[test]
    fn clickable_cells_share_their_link_span() {
        let mut terminal = Terminal::new(Options::default()).unwrap();
        terminal.vt_write(
            b"\x1b]8;;pwr://artifact/one\x1b\\First link\x1b]8;;\x1b\\ pwr://artifact/two \x1b]8;;https://example.com\x1b\\Remote\x1b]8;;\x1b\\",
        );
        let frame = Frame::default()
            .update(&mut RenderState::new().unwrap(), &terminal)
            .unwrap()
            .unwrap();
        let cells = &frame.rows[0].cells;
        let first = cells[0].link.as_ref().unwrap();
        let second = cells[11].link.as_ref().unwrap();
        assert_eq!(first.as_ref(), "pwr://artifact/one");
        assert!(Arc::ptr_eq(first, cells[9].link.as_ref().unwrap()));
        assert!(cells[10].link.is_none());
        assert_eq!(second.as_ref(), "pwr://artifact/two");
        assert!(Arc::ptr_eq(second, cells[28].link.as_ref().unwrap()));
        assert!(
            cells[30..36]
                .iter()
                .all(|cell| cell.link.as_deref() == Some("https://example.com"))
        );
    }
}

#[cfg(test)]
mod conceal_tests {
    use super::{Frame, RenderState, Terminal};
    use ghostty_vt::terminal::Options;

    #[test]
    fn concealed_cells_keep_their_graphemes() {
        let mut terminal = Terminal::new(Options::default()).unwrap();
        terminal.vt_write(b"a\x1b[8mSECRET\x1b[0mz");
        let frame = Frame::default()
            .update(&mut RenderState::new().unwrap(), &terminal)
            .unwrap()
            .unwrap();
        let text: String = frame.rows[0]
            .cells
            .iter()
            .map(|c| c.text.as_str())
            .collect();
        assert!(text.starts_with("aSECRETz"));
        assert!(frame.rows[0].cells[1].style.invisible);
    }

    #[test]
    fn cursor_tracks_decscusr_shape_and_wide_cell_head() {
        use ghostty_vt::render::CursorVisualStyle;
        let mut terminal = Terminal::new(Options::default()).unwrap();
        let mut renderer = RenderState::new().unwrap();
        let mut frame = Frame::default();
        for (sequence, expected) in [
            (b"\x1b[1 q".as_slice(), CursorVisualStyle::Block),
            (b"\x1b[3 q".as_slice(), CursorVisualStyle::Underline),
            (b"\x1b[5 q".as_slice(), CursorVisualStyle::Bar),
        ] {
            terminal.vt_write(sequence);
            frame = frame.update(&mut renderer, &terminal).unwrap().unwrap();
            assert_eq!(frame.cursor.unwrap().style, expected);
        }
        terminal.vt_write("\x1b[2 q中\x1b[D".as_bytes());
        let frame = frame.update(&mut renderer, &terminal).unwrap().unwrap();
        let cursor = frame.cursor.unwrap();
        assert_eq!(cursor.position.column, 0);
        assert_eq!(cursor.style, CursorVisualStyle::Block);
    }
}

#[cfg(test)]
mod wrap_tests {
    use super::{Frame, RenderState, Terminal};
    use ghostty_vt::terminal::{Options, ScrollViewport};
    use std::sync::Arc;

    fn frame(renderer: &mut RenderState, terminal: &Terminal) -> Frame {
        Frame::default()
            .update(renderer, terminal)
            .unwrap()
            .unwrap()
    }

    fn sized(cols: u16, rows: u16) -> Terminal {
        Terminal::new(Options {
            cols,
            rows,
            ..Options::default()
        })
        .unwrap()
    }

    #[test]
    fn wrapped_links_share_a_span_and_skip_the_wide_wrap_spacer() {
        let mut terminal = sized(10, 4);
        let uri = "https://x界.test/path";
        terminal.vt_write(format!("{uri} rest").as_bytes());
        let frame = frame(&mut RenderState::new().unwrap(), &terminal);
        let first = frame.rows[0].cells[0].link.as_ref().unwrap();
        assert_eq!(first.as_ref(), uri);
        assert!(frame.rows[0].cells[9].link.is_none());
        for (row, columns) in [(0, 0..9), (1, 0..10), (2, 0..2)] {
            for cell in &frame.rows[row].cells[columns] {
                assert!(Arc::ptr_eq(first, cell.link.as_ref().unwrap()));
            }
        }
        assert!(frame.rows[2].cells[2].link.is_none());
        use super::CellPosition;
        let start = CellPosition { row: 0, column: 0 };
        let end = CellPosition { row: 2, column: 1 };
        assert_eq!(
            frame.link_span(CellPosition { row: 1, column: 4 }),
            Some((start, end))
        );
    }

    #[test]
    fn wrapped_osc_link_takes_precedence_over_a_printed_url() {
        let mut terminal = sized(12, 3);
        terminal.vt_write(b"\x1b]8;;pwr://dag/run\x1b\\https://example.org\x1b]8;;\x1b\\");
        let frame = frame(&mut RenderState::new().unwrap(), &terminal);
        let first = frame.rows[0].cells[0].link.as_ref().unwrap();
        assert_eq!(first.as_ref(), "pwr://dag/run");
        assert_eq!(
            frame.rows[1].cells[6].link.as_deref(),
            Some("pwr://dag/run")
        );
        assert!(frame.rows[1].cells[7].link.is_none());
    }

    #[test]
    fn wrapped_links_include_scrollback_on_both_sides_of_the_viewport() {
        let mut terminal = sized(12, 2);
        let uri = "https://example.com/a/long/path";
        terminal.vt_write(format!("{uri}\r\nend").as_bytes());
        let mut renderer = RenderState::new().unwrap();
        for position in [ScrollViewport::Top, ScrollViewport::Bottom] {
            terminal.scroll_viewport(position);
            let frame = frame(&mut renderer, &terminal);
            assert_eq!(frame.rows[0].cells[0].link.as_deref(), Some(uri));
        }
    }

    #[test]
    fn urls_running_past_the_scan_reach_stay_inert() {
        let mut terminal = sized(10, 2);
        let uri = format!("https://example.com/{}", "a".repeat(10 * 40));
        terminal.vt_write(format!("{uri}\r\nend").as_bytes());
        let mut renderer = RenderState::new().unwrap();
        terminal.scroll_viewport(ScrollViewport::Top);
        let top = frame(&mut renderer, &terminal);
        assert!(top.rows.iter().all(|row| row.links.is_empty()));
        terminal.scroll_viewport(ScrollViewport::Bottom);
        let bottom = frame(&mut renderer, &terminal);
        assert!(bottom.rows[0].links.is_empty());

        // Punctuation on the last scanned cell must not hide the truncation.
        let mut terminal = sized(10, 2);
        let uri = format!("https://example.com/{};{}", "a".repeat(319), "a".repeat(80));
        terminal.vt_write(format!("{uri}\r\nend").as_bytes());
        terminal.scroll_viewport(ScrollViewport::Top);
        let top = frame(&mut RenderState::new().unwrap(), &terminal);
        assert!(top.rows.iter().all(|row| row.links.is_empty()));
    }

    #[test]
    fn hard_newline_at_the_right_edge_keeps_urls_on_their_own_lines() {
        let mut terminal = sized(16, 2);
        let uri = "https://a.test/x";
        terminal.vt_write(format!("{uri}\r\nmore").as_bytes());
        let frame = frame(&mut RenderState::new().unwrap(), &terminal);
        assert_eq!(frame.rows[0].cells[15].link.as_deref(), Some(uri));
        assert!(frame.rows[1].cells[0].link.is_none());
    }

    #[test]
    fn editing_one_half_of_a_wrapped_url_relinks_the_cached_half() {
        let mut terminal = sized(10, 4);
        let mut renderer = RenderState::new().unwrap();
        terminal.vt_write(b"https://a.test/long\x1b[H");
        let first = frame(&mut renderer, &terminal);
        assert_eq!(
            first.rows[1].cells[0].link.as_deref(),
            Some("https://a.test/long")
        );
        // Breaking the scheme damages the first row only; the cached second row loses its link.
        terminal.vt_write(b"X");
        let second = first.update(&mut renderer, &terminal).unwrap().unwrap();
        assert!(second.rows[1].cells[0].link.is_none());
        assert!(second.rows[0].cells[0].link.is_none());
    }
}
