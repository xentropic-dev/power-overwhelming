//! Row damage and reuse adapted from herdr-app `src/vt.rs` (Apache-2.0).

use super::links::{candidate, links};
use super::paint::RowDrawing;
use anyhow::Result;
use ghostty_vt::{
    Terminal,
    render::{CellIterator, Colors, Dirty, RenderState, RowId, RowIteration, RowIterator},
    screen::CellWide,
    style::Style,
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
    /// The cell's grapheme, or a space when it has none.
    pub(super) text: String,
    pub(super) fg: Hsla,
    pub(super) bg: Option<Hsla>,
    /// The cell's SGR attributes, as Ghostty stores them; `fg` and `bg` are already resolved.
    pub(super) style: Style,
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
}

impl Frame {
    /// None means the last frame still describes the viewport. Only acknowledge
    /// Ghostty's damage after every row and its links have been captured.
    pub(super) fn update(
        &self,
        renderer: &mut RenderState,
        terminal: &Terminal,
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
        let full =
            dirty == Dirty::Full || self.colors.as_ref() != Some(&colors) || self.cols != cols;
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
        let fg = colors.foreground;
        let foreground = rgba_color(fg.r, fg.g, fg.b);
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
                    foreground,
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
        }))
    }
}

fn read_row<'s>(
    row: &'s RowIteration<'s>,
    cell_iter: &'s mut CellIterator,
    terminal: &Terminal,
    y: u32,
    id: RowId,
    foreground: Hsla,
) -> Result<Row> {
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
        cells.push(Cell {
            text: grapheme,
            fg: cell
                .fg_color()?
                .map_or(foreground, |c| rgba_color(c.r, c.g, c.b)),
            bg: cell.bg_color()?.map(|c| rgba_color(c.r, c.g, c.b)),
            style: cell.style()?,
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

fn rgba_color(r: u8, g: u8, b: u8) -> Hsla {
    rgb(u32::from_be_bytes([0, r, g, b])).into()
}

#[cfg(test)]
mod tests {
    use super::{Frame, RenderState, Terminal};
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
