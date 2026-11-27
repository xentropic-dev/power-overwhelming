//! Mouse selection through Ghostty's selection gestures. Click, double-click (word),
//! triple-click (line), drag, Shift+click extend and drag autoscroll are adapted from
//! herdr-app `src/vt.rs` and `src/terminal.rs` (Apache-2.0).
//!
//! The selection lives in the Ghostty terminal, pinned to its cells, so it stays on the
//! same text while output scrolls it and the viewport moves through history.

use super::frame::CellPosition;
use ghostty_vt::{
    Terminal,
    selection::{
        FormatOptions, Order,
        gesture::{
            Autoscroll, AutoscrollTickEvent, DragEvent, Geometry, Gesture, PressEvent, ReleaseEvent,
        },
    },
    terminal::{Point as VtPoint, PointCoordinate, PointSpace},
};
use std::time::{Duration, Instant};

/// Ghostty takes cell widths in whole units. At fractional zoom a cell is 9.75 px, and 9
/// put a cell's middle in the wrong place, so positions and geometry go in 1/64 px.
const SUBPIXEL: f32 = 64.;
/// GPUI doesn't expose the platform's double-click interval.
const REPEAT_INTERVAL: Duration = Duration::from_millis(500);
/// How far apart, in pixels, the clicks of a double or triple click may land.
const REPEAT_DISTANCE: f32 = 4.;
/// How often a drag held past the top or bottom edge scrolls one more row.
pub(super) const AUTOSCROLL_INTERVAL: Duration = Duration::from_millis(50);

/// The pointer as a selection gesture sees it: a viewport cell and the position inside
/// the grid in logical pixels, which may lie past the grid's edges.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Pointer {
    pub cell: CellPosition,
    pub x: f32,
    pub y: f32,
}

/// The grid the gesture maps pointer positions on, in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Surface {
    pub cell_width: f32,
    pub cell_height: f32,
}

/// One terminal's selection gesture state.
pub(super) struct Selector {
    gesture: Gesture,
    press: PressEvent,
    drag: DragEvent,
    release: ReleaseEvent,
    tick: AutoscrollTickEvent,
    epoch: Instant,
    /// A left-button gesture is in progress.
    pub selecting: bool,
    /// The gesture moves the existing selection's end (Shift+click) instead.
    extending: bool,
}

impl Selector {
    pub fn new() -> Self {
        Self {
            gesture: Gesture::new().expect("allocate selection gesture"),
            press: PressEvent::new().expect("allocate selection press"),
            drag: DragEvent::new().expect("allocate selection drag"),
            release: ReleaseEvent::new().expect("allocate selection release"),
            tick: AutoscrollTickEvent::new().expect("allocate selection autoscroll"),
            epoch: Instant::now(),
            selecting: false,
            extending: false,
        }
    }

    /// Starts a gesture. With `extend` and a selection to extend, the press moves that
    /// selection's end, as in Terminal.app; otherwise a plain click clears the selection
    /// and a repeated click selects the word or line.
    pub fn press(&mut self, terminal: &Terminal, at: Pointer, extend: bool) {
        self.selecting = true;
        self.extending = extend && terminal.selection().ok().flatten().is_some();
        if self.extending {
            // This press bypasses the gesture, so it ends any click sequence.
            self.gesture.reset(terminal);
            extend_to(terminal, at.cell);
            return;
        }
        self.press_at(terminal, at, self.epoch.elapsed());
    }

    fn press_at(&mut self, terminal: &Terminal, at: Pointer, time: Duration) {
        self.selecting = true;
        let Some(grid_ref) = viewport_ref(terminal, at.cell) else {
            return;
        };
        let selection = self
            .press
            .set_position(f64::from(at.x * SUBPIXEL), f64::from(at.y * SUBPIXEL))
            .and_then(|press| press.set_time(time))
            .and_then(|press| press.set_repeat_interval(REPEAT_INTERVAL))
            .and_then(|press| press.set_repeat_distance(f64::from(REPEAT_DISTANCE * SUBPIXEL)))
            .and_then(|press| press.apply(&mut self.gesture, terminal, grid_ref));
        install(terminal, selection.ok().flatten().as_ref(), true);
    }

    /// Follows the pointer while the button is held.
    pub fn drag(&mut self, terminal: &Terminal, at: Pointer, surface: Surface) {
        if !self.selecting {
            return;
        }
        if self.extending {
            extend_to(terminal, at.cell);
            return;
        }
        let Some(grid_ref) = viewport_ref(terminal, at.cell) else {
            return;
        };
        let Some(geometry) = geometry(terminal, surface) else {
            return;
        };
        let selection = self
            .drag
            .set_position(f64::from(at.x * SUBPIXEL), f64::from(at.y * SUBPIXEL))
            .and_then(|drag| drag.apply(&mut self.gesture, terminal, grid_ref, geometry));
        install(terminal, selection.ok().flatten().as_ref(), false);
    }

    /// Scrolls one row toward the pointer when a drag is held past the grid's top or
    /// bottom, and extends the selection there. False when there is nothing to scroll.
    pub fn autoscroll(&mut self, terminal: &mut Terminal, at: Pointer, surface: Surface) -> bool {
        if !self.selecting {
            return false;
        }
        if self.extending {
            // Shift+drag bypasses the gesture, so scroll and extend here.
            let rows = terminal.rows().unwrap_or(1).max(1);
            let delta = if at.y < 0. {
                -1
            } else if at.y >= surface.cell_height * f32::from(rows) {
                1
            } else {
                return false;
            };
            terminal.scroll_viewport(ghostty_vt::terminal::ScrollViewport::Delta(delta));
            extend_to(terminal, at.cell);
            return true;
        }
        if self
            .gesture
            .autoscroll(terminal)
            .is_ok_and(|direction| direction == Autoscroll::None)
        {
            return false;
        }
        let Some(geometry) = geometry(terminal, surface) else {
            return false;
        };
        let viewport = PointCoordinate {
            x: at.cell.column as u16,
            y: at.cell.row as u32,
        };
        let selection = self
            .tick
            .set_position(f64::from(at.x * SUBPIXEL), f64::from(at.y * SUBPIXEL))
            .and_then(|tick| tick.apply(&mut self.gesture, terminal, viewport, geometry));
        match selection {
            Ok(Some(selection)) => {
                install(terminal, Some(&selection), false);
                true
            }
            _ => false,
        }
    }

    /// Ends the gesture. True when it left a selection to copy.
    pub fn release(&mut self, terminal: &Terminal, at: Option<Pointer>) -> bool {
        if !self.selecting {
            return false;
        }
        self.selecting = false;
        if !self.extending {
            let grid_ref = at.and_then(|at| viewport_ref(terminal, at.cell));
            let _ = self.release.apply(&mut self.gesture, terminal, grid_ref);
        }
        self.extending = false;
        terminal.selection().ok().flatten().is_some()
    }

    /// Whether the last gesture moved far enough to count as a drag rather than a click.
    pub fn dragged(&self, terminal: &Terminal) -> bool {
        self.gesture.dragged(terminal).unwrap_or(false)
    }

    /// Forgets the gesture and its click sequence, as when the terminal is replaced.
    pub fn reset(&mut self, terminal: &Terminal) {
        self.selecting = false;
        self.extending = false;
        self.gesture.reset(terminal);
    }
}

fn viewport_ref(
    terminal: &Terminal,
    cell: CellPosition,
) -> Option<ghostty_vt::screen::GridRef<'_>> {
    let cols = terminal.cols().ok()?;
    let rows = terminal.rows().ok()?;
    terminal
        .grid_ref(VtPoint::Viewport(PointCoordinate {
            x: (cell.column as u16).min(cols.saturating_sub(1)),
            y: (cell.row as u32).min(u32::from(rows.saturating_sub(1))),
        }))
        .ok()
}

fn geometry(terminal: &Terminal, surface: Surface) -> Option<Geometry> {
    let cols = terminal.cols().ok()?;
    let rows = terminal.rows().ok()?;
    Some(Geometry {
        columns: u32::from(cols.max(1)),
        cell_width: (surface.cell_width.max(1.) * SUBPIXEL).round() as u32,
        padding_left: 0,
        screen_height: (surface.cell_height * f32::from(rows.max(1)) * SUBPIXEL)
            .round()
            .max(1.) as u32,
    })
}

/// Installs a gesture's result. A press with no result clears the selection; a drag
/// with none keeps what is there.
fn install(
    terminal: &Terminal,
    selection: Option<&ghostty_vt::selection::Selection<'_>>,
    press: bool,
) {
    if selection.is_some() || press {
        let _ = terminal.set_selection(selection);
    }
}

/// Moves the selection's end to a viewport cell, keeping its start.
fn extend_to(terminal: &Terminal, cell: CellPosition) {
    let Some(end) = viewport_ref(terminal, cell) else {
        return;
    };
    let Ok(Some(selection)) = terminal.selection() else {
        return;
    };
    let extended =
        ghostty_vt::selection::Selection::new(selection.start(), end, selection.is_rectangle());
    let _ = terminal.set_selection(Some(&extended));
}

/// Selects the screen and its history.
pub(super) fn select_all(terminal: &Terminal) -> bool {
    match terminal.select_all() {
        Ok(Some(selection)) => terminal.set_selection(Some(&selection)).is_ok(),
        _ => false,
    }
}

pub(super) fn clear(terminal: &Terminal) -> bool {
    let had = terminal.selection().ok().flatten().is_some();
    if had {
        let _ = terminal.set_selection(None);
    }
    had
}

/// The selection's text, soft wraps joined and trailing blanks trimmed.
pub(super) fn text(terminal: &Terminal) -> Option<String> {
    let bytes = terminal
        .format_selection_alloc(FormatOptions::new().with_unwrap(true).with_trim(true))
        .ok()??;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    (!text.is_empty()).then_some(text)
}

/// The selection's ends in viewport cells, first to last. Rows run past the viewport
/// (negative ends are clamped to its top) when the selection reaches into history or
/// below it, so painting can clip each row's range.
pub(super) fn viewport_range(terminal: &Terminal) -> Option<(CellPosition, CellPosition)> {
    let selection = terminal.selection().ok()??;
    let ordered = selection.to_ordered(terminal, Order::Forward).ok()?;
    let offset = terminal.scrollbar().ok()?.offset;
    let rows = u64::from(terminal.rows().ok()?);
    let cols = usize::from(terminal.cols().ok()?);
    let screen = |grid_ref| {
        terminal
            .point_from_grid_ref(&grid_ref, PointSpace::Screen)
            .ok()
            .flatten()
    };
    let start = screen(ordered.start())?;
    let end = screen(ordered.end())?;
    let (start_row, end_row) = (u64::from(start.y), u64::from(end.y));
    if end_row < offset || start_row >= offset + rows {
        return None;
    }
    let start = if start_row < offset {
        CellPosition { row: 0, column: 0 }
    } else {
        CellPosition {
            row: (start_row - offset) as usize,
            column: start.x.into(),
        }
    };
    let end = if end_row >= offset + rows {
        CellPosition {
            row: (rows - 1) as usize,
            column: cols.saturating_sub(1),
        }
    } else {
        CellPosition {
            row: (end_row - offset) as usize,
            column: end.x.into(),
        }
    };
    Some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ghostty_vt::terminal::{Options, ScrollViewport};

    const SURFACE: Surface = Surface {
        cell_width: 10.,
        cell_height: 20.,
    };

    fn terminal(text: &str) -> Terminal {
        let mut terminal = Terminal::new(Options {
            cols: 20,
            rows: 4,
            ..Options::default()
        })
        .unwrap();
        terminal.vt_write(text.as_bytes());
        terminal
    }

    fn at(row: usize, column: usize) -> Pointer {
        Pointer {
            cell: CellPosition { row, column },
            x: column as f32 * SURFACE.cell_width + 5.,
            y: row as f32 * SURFACE.cell_height + 10.,
        }
    }

    fn clicks(selector: &mut Selector, terminal: &Terminal, pointer: Pointer, count: u64) {
        for n in 0..count {
            selector.press_at(terminal, pointer, Duration::from_millis(100 * n));
            selector.release(terminal, Some(pointer));
        }
    }

    #[test]
    fn double_click_selects_a_word_and_triple_click_a_line() {
        let terminal = terminal("hello brave world");
        let mut selector = Selector::new();
        clicks(&mut selector, &terminal, at(0, 7), 2);
        assert_eq!(text(&terminal).as_deref(), Some("brave"));
        let mut selector = Selector::new();
        clicks(&mut selector, &terminal, at(0, 7), 3);
        assert_eq!(text(&terminal).as_deref(), Some("hello brave world"));
    }

    #[test]
    fn single_click_clears_and_drag_selects_cells() {
        let terminal = terminal("hello brave world");
        let mut selector = Selector::new();
        clicks(&mut selector, &terminal, at(0, 7), 2);
        selector.press_at(&terminal, at(0, 1), Duration::from_secs(5));
        assert_eq!(text(&terminal), None);
        selector.drag(&terminal, at(0, 4), SURFACE);
        assert!(selector.release(&terminal, Some(at(0, 4))));
        assert!(selector.dragged(&terminal));
        // Ghostty selects a cell once the pointer passes its middle: from the middle of
        // column 1 to the middle of column 4 covers columns 1 to 3.
        assert_eq!(text(&terminal).as_deref(), Some("ell"));
    }

    #[test]
    fn shift_click_moves_the_end_of_the_selection() {
        let terminal = terminal("hello brave world");
        let mut selector = Selector::new();
        clicks(&mut selector, &terminal, at(0, 1), 2);
        assert_eq!(text(&terminal).as_deref(), Some("hello"));
        selector.press(&terminal, at(0, 8), true);
        selector.release(&terminal, Some(at(0, 8)));
        assert_eq!(text(&terminal).as_deref(), Some("hello bra"));
    }

    #[test]
    fn selection_follows_its_text_as_output_scrolls() {
        let mut terminal = terminal("one\r\ntwo");
        let mut selector = Selector::new();
        clicks(&mut selector, &terminal, at(1, 1), 2);
        assert_eq!(
            viewport_range(&terminal),
            Some((
                CellPosition { row: 1, column: 0 },
                CellPosition { row: 1, column: 2 }
            ))
        );
        terminal.vt_write(b"\r\nthree\r\nfour\r\nfive\r\nsix");
        assert_eq!(text(&terminal).as_deref(), Some("two"));
        assert_eq!(viewport_range(&terminal), None, "scrolled into history");
        terminal.scroll_viewport(ScrollViewport::Top);
        assert_eq!(
            viewport_range(&terminal),
            Some((
                CellPosition { row: 1, column: 0 },
                CellPosition { row: 1, column: 2 }
            ))
        );
    }

    #[test]
    fn autoscroll_extends_a_drag_held_above_the_grid() {
        let mut terminal = terminal("a\r\nb\r\nc\r\nd\r\ne\r\nf\r\ng");
        let mut selector = Selector::new();
        selector.press_at(&terminal, at(2, 0), Duration::ZERO);
        let above = Pointer {
            cell: CellPosition { row: 0, column: 0 },
            x: 0.,
            y: -30.,
        };
        selector.drag(&terminal, above, SURFACE);
        assert!(selector.autoscroll(&mut terminal, above, SURFACE));
        assert!(!terminal.is_viewport_active().unwrap());
        selector.release(&terminal, Some(above));
        assert_eq!(text(&terminal).as_deref(), Some("c\nd\ne"));
        terminal.scroll_viewport(ScrollViewport::Bottom);
    }

    #[test]
    fn autoscroll_extends_a_shift_drag_held_below_the_grid() {
        let mut terminal = terminal("a\r\nb\r\nc\r\nd\r\ne\r\nf\r\ng");
        terminal.scroll_viewport(ScrollViewport::Top);
        let mut selector = Selector::new();
        clicks(&mut selector, &terminal, at(0, 0), 2);
        selector.press(&terminal, at(1, 0), true);
        let below = Pointer {
            cell: CellPosition { row: 3, column: 0 },
            x: 0.,
            y: SURFACE.cell_height * 4. + 30.,
        };
        selector.drag(&terminal, below, SURFACE);
        assert!(selector.autoscroll(&mut terminal, below, SURFACE));
        selector.release(&terminal, Some(below));
        assert_eq!(text(&terminal).as_deref(), Some("a\nb\nc\nd\ne"));
        terminal.scroll_viewport(ScrollViewport::Bottom);
    }

    #[test]
    fn select_all_covers_history_and_clear_drops_it() {
        let terminal = terminal("a\r\nb\r\nc\r\nd\r\ne");
        assert!(select_all(&terminal));
        assert_eq!(text(&terminal).as_deref(), Some("a\nb\nc\nd\ne"));
        assert!(clear(&terminal));
        assert_eq!(text(&terminal), None);
    }
}
