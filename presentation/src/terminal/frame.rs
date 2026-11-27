//! Row damage and reuse adapted from herdr-app `src/vt.rs`
//! (https://github.com/ogulcancelik/herdr-app), licensed under Apache-2.0. Minimum
//! contrast is adapted from the same file.

use super::links::{candidate, links};
use super::paint::RowDrawing;
use anyhow::Result;
use ghostty_vt::{
    Terminal,
    render::{CellIterator, Colors, Dirty, RenderState, RowId, RowIteration, RowIterator},
    screen::CellWide,
    style::{Style, StyleColor, Underline},
    terminal::{Point as VtPoint, PointCoordinate},
};
use gpui::{Hsla, rgb};
use std::{cell::RefCell, collections::HashMap, ops::Range, rc::Rc, sync::Arc};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct CellPosition {
    pub(super) row: usize,
    pub(super) column: usize,
}

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
}

pub(super) struct Row {
    id: RowId,
    pub(super) cells: Vec<Cell>,
    /// Row-local runs of clickable cells, retained with the text.
    pub(super) links: Vec<Range<usize>>,
    pub(super) continues_osc: bool,
    pub(super) ends_in_osc: bool,
    pub(super) drawing: RefCell<Option<Rc<RowDrawing>>>,
}

#[derive(Default)]
pub(super) struct Frame {
    pub(super) rows: Vec<Rc<Row>>,
    pub(super) cursor: Option<CellPosition>,
    /// Where the cursor is, shown or hidden: input methods compose and place their candidate
    /// window there.
    pub(super) ime_cursor: Option<CellPosition>,
    pub(super) colors: Option<Colors>,
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
        let cursor = ime_cursor.filter(|_| vt_cursor.visible);
        let colors = state.colors()?;
        let dirty = state.dirty()?;
        let cols = state.cols()?;
        let height = usize::from(state.rows()?);
        let full = dirty == Dirty::Full
            || self.colors.as_ref() != Some(&colors)
            || self.cols != cols
            || self.minimum_contrast != minimum_contrast;
        if !full && dirty == Dirty::Clean && self.rows.len() == height {
            if self.cursor == cursor && self.ime_cursor == ime_cursor {
                return Ok(None);
            }
            return Ok(Some(Self {
                rows: self.rows.clone(),
                cursor,
                ime_cursor,
                colors: Some(colors),
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
                rows.push(Rc::clone(kept));
            } else {
                rows.push(Rc::new(read_row(
                    row,
                    &mut cell_iter,
                    terminal,
                    rows.len() as u32,
                    id,
                    &colors,
                    minimum_contrast,
                )?));
            }
        }
        state.clean()?;
        Ok(Some(Self {
            rows,
            cursor,
            ime_cursor,
            colors: Some(colors),
            cols,
            minimum_contrast,
        }))
    }
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
    let wrapped = row.raw_row()?.is_wrap_continuation()?;
    let mut osc_link: Option<Arc<str>> = None;
    let mut continues_osc = false;
    let mut uri_buffer = [0; 8192];
    let mut source = cell_iter.update(row)?;
    let mut cells = Vec::new();
    let mut text = String::new();
    let mut offsets = Vec::new();
    while let Some(cell) = source.next() {
        let mut grapheme = String::new();
        cell.graphemes_utf8(&mut grapheme)?;
        if grapheme.is_empty() {
            grapheme.push(' ');
        }
        offsets.push(text.len());
        text.push_str(&grapheme);
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
        if cells.is_empty() {
            continues_osc = wrapped && osc_link.is_some();
        }
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
        });
    }
    for (range, uri) in links(&text) {
        let start = offsets.partition_point(|offset| *offset < range.start);
        let end = offsets.partition_point(|offset| *offset < range.end);
        let link = Arc::from(uri);
        for cell in &mut cells[start..end] {
            if cell.link.is_none() {
                cell.link = Some(Arc::clone(&link));
            }
        }
    }
    let mut spans = Vec::new();
    let mut col = 0;
    while col < cells.len() {
        let start = col;
        col += 1;
        if let Some(link) = &cells[start].link {
            while cells
                .get(col)
                .and_then(|c| c.link.as_ref())
                .is_some_and(|next| Arc::ptr_eq(next, link))
            {
                col += 1;
            }
            spans.push(start..col);
        }
    }
    Ok(Row {
        id,
        cells,
        links: spans,
        continues_osc,
        ends_in_osc: osc_link.is_some(),
        drawing: RefCell::default(),
    })
}

fn packed_rgb(r: u8, g: u8, b: u8) -> u32 {
    u32::from_be_bytes([0, r, g, b])
}

/// Box drawing, blocks, braille, powerline and legacy computing sprites keep their authored
/// colors even when that color has low contrast.
fn preserves_graphics_color(text: &str) -> bool {
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => {
            ('\u{2500}'..='\u{259f}').contains(&c)
                || ('\u{2800}'..='\u{28ff}').contains(&c)
                || ('\u{e0b0}'..='\u{e0bf}').contains(&c)
                || c == '\u{23fa}'
                || ('\u{1fb00}'..='\u{1fbff}').contains(&c)
                || ('\u{1cc00}'..='\u{1cebf}').contains(&c)
        }
        _ => false,
    }
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
        assert!(cells[30..36].iter().all(|cell| cell.link.is_none()));
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
}
