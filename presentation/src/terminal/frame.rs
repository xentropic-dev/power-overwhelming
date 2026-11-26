use super::links::{candidate, links};
use anyhow::Result;
use ghostty_vt::{
    Terminal,
    render::{CellIterator, RenderState, RowIterator},
    screen::CellWide,
    style::{Style, StyleColor, Underline},
    terminal::{Point as VtPoint, PointCoordinate},
};
use gpui::{Hsla, rgb};
use std::sync::Arc;

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
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
    /// The underline's own color (SGR 58), resolved; `None` follows the text.
    pub(super) underline_color: Option<Hsla>,
    pub(super) wide: CellWide,
    pub(super) link: Option<Arc<str>>,
}

pub(super) struct Row {
    pub(super) cells: Vec<Cell>,
}

#[derive(Default)]
pub(super) struct Frame {
    pub(super) rows: Vec<Row>,
    pub(super) cursor: Option<CellPosition>,
}

pub(super) fn frame(
    renderer: &mut RenderState,
    terminal: &Terminal,
    foreground: u32,
) -> Result<Frame> {
    let state = renderer.update(terminal)?;
    let cursor = state
        .cursor_viewport()?
        .filter(|_| state.cursor_visible().unwrap_or(false))
        .map(|cursor| CellPosition {
            row: cursor.y.into(),
            column: cursor.x.into(),
        });
    let colors = state.colors()?;
    let default_background = rgba_color(
        colors.background.r,
        colors.background.g,
        colors.background.b,
    );
    let mut row_iter = RowIterator::new()?;
    let mut cell_iter = CellIterator::new()?;
    let mut source = row_iter.update(&state)?;
    let mut rows = Vec::new();
    let mut osc_link: Option<Arc<str>> = None;
    let mut uri_buffer = [0; 8192];
    while let Some(row) = source.next() {
        if !row.raw_row()?.is_wrap_continuation()? {
            osc_link = None;
        }
        let y = rows.len() as u32;
        let mut source = cell_iter.update(row)?;
        let mut cells = Vec::new();
        let mut text = String::new();
        let mut offsets = Vec::new();
        while let Some(cell) = source.next() {
            let mut grapheme = String::new();
            cell.graphemes_utf8(&mut grapheme)?;
            let style = cell.style()?;
            if grapheme.is_empty() || style.invisible {
                grapheme.clear();
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
            let mut fg = cell
                .fg_color()?
                .map_or(rgb(foreground).into(), |c| rgba_color(c.r, c.g, c.b));
            let mut bg = cell.bg_color()?.map(|c| rgba_color(c.r, c.g, c.b));
            if style.inverse {
                (fg, bg) = (bg.unwrap_or(default_background), Some(fg));
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
        rows.push(Row { cells });
    }
    Ok(Frame { rows, cursor })
}

fn rgba_color(r: u8, g: u8, b: u8) -> Hsla {
    rgb(u32::from_be_bytes([0, r, g, b])).into()
}

#[cfg(test)]
mod tests {
    use super::{RenderState, Terminal, frame};
    use ghostty_vt::terminal::Options;
    use std::sync::Arc;

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
        for scheme in [dark, light] {
            scheme.apply_to(&mut terminal).unwrap();
            let rendered = frame(&mut renderer, &terminal, scheme.colors.foreground).unwrap();
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
        let frame = frame(&mut RenderState::new().unwrap(), &terminal, 0xdbe3ee).unwrap();
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
