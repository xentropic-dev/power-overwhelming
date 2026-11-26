//! Where the terminal grid sits on screen and the PTY size it was last given.

use super::{CellMetrics, frame::CellPosition};
use domain::session::Geometry;
use gpui::{Bounds, Pixels, Point, Size};
use infrastructure::client::TerminalControl;

#[derive(Default)]
pub(super) struct PtyGrid {
    /// The grid's bounds inside the padding, origin on a device pixel.
    pub bounds: Bounds<Pixels>,
    /// The cell as last measured.
    pub metrics: CellMetrics,
    /// The size last sent to the PTY, once the grid has been laid out.
    geometry: Option<Geometry>,
}

impl PtyGrid {
    /// Records the grid's bounds and resizes the PTY to the cells that fit.
    pub fn lay_out(
        &mut self,
        bounds: Bounds<Pixels>,
        metrics: CellMetrics,
        commands: &async_channel::Sender<TerminalControl>,
    ) {
        self.bounds = Bounds::new(metrics.snap(bounds.origin), bounds.size);
        self.metrics = metrics;
        self.resize(commands);
    }

    /// Takes a new cell size in the bounds last laid out, resizing the PTY at once rather than
    /// on the next frame, which a terminal in a background tab may not get for a long time.
    /// Does nothing before the first layout, when the bounds aren't known.
    pub fn remeasure(
        &mut self,
        metrics: CellMetrics,
        commands: &async_channel::Sender<TerminalControl>,
    ) {
        if self.geometry.is_some() {
            self.lay_out(self.bounds, metrics, commands);
        }
    }

    /// The PTY grid for the current bounds and cell.
    pub fn geometry(&self) -> Geometry {
        self.metrics.grid(self.bounds.size)
    }

    fn resize(&mut self, commands: &async_channel::Sender<TerminalControl>) {
        let geometry = self.geometry();
        if self.geometry != Some(geometry)
            && commands.try_send(TerminalControl::Resize(geometry)).is_ok()
        {
            self.geometry = Some(geometry);
        }
    }

    /// The cell under `position`, within the PTY's grid.
    pub fn at(&self, position: Point<Pixels>) -> CellPosition {
        let local = position - self.bounds.origin;
        let grid = self.geometry();
        let cell = |offset: Pixels, length: Pixels, count: u16| {
            ((f32::from(offset) / f32::from(length)).max(0.) as usize)
                .min(usize::from(count.max(1)) - 1)
        };
        CellPosition {
            row: cell(local.y, self.metrics.size.height, grid.rows),
            column: cell(local.x, self.metrics.size.width, grid.cols),
        }
    }

    /// The grid's size in whole device pixels.
    pub fn device_size(&self) -> Size<u32> {
        self.metrics.whole_device_size(self.bounds.size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px, size};

    fn metrics(width: f32, height: f32) -> CellMetrics {
        CellMetrics {
            size: size(px(width), px(height)),
            ..CellMetrics::default()
        }
    }

    fn resizes(commands: &async_channel::Receiver<TerminalControl>) -> Vec<Geometry> {
        std::iter::from_fn(|| commands.try_recv().ok())
            .filter_map(|command| match command {
                TerminalControl::Resize(geometry) => Some(geometry),
                _ => None,
            })
            .collect()
    }

    /// Zoom reaches every terminal through `remeasure`; a background one never renders, so
    /// its PTY must resize there, as the visible one's does.
    #[test]
    fn remeasuring_resizes_every_laid_out_pty() {
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(800.), px(468.)));
        let zoomed = metrics(10., 20.);
        let mut ptys = Vec::new();
        for _ in 0..2 {
            let (tx, rx) = async_channel::unbounded();
            let mut grid = PtyGrid::default();
            grid.lay_out(bounds, metrics(8., 18.), &tx);
            assert_eq!(
                resizes(&rx),
                [Geometry {
                    cols: 100,
                    rows: 26,
                    cell_width: 8,
                    cell_height: 18
                }]
            );
            ptys.push((grid, tx, rx));
        }
        for (grid, tx, _) in &mut ptys {
            grid.remeasure(zoomed, tx);
        }
        for (_, _, rx) in &ptys {
            assert_eq!(
                resizes(rx),
                [Geometry {
                    cols: 80,
                    rows: 23,
                    cell_width: 10,
                    cell_height: 20
                }]
            );
        }
    }

    #[test]
    fn remeasuring_before_layout_waits_for_bounds() {
        let (tx, rx) = async_channel::unbounded();
        let mut grid = PtyGrid::default();
        grid.remeasure(metrics(10., 20.), &tx);
        assert!(resizes(&rx).is_empty());
    }

    #[test]
    fn hit_testing_stays_inside_the_capped_grid() {
        let (tx, _rx) = async_channel::unbounded();
        let mut grid = PtyGrid::default();
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(6000.), px(180.)));
        grid.lay_out(bounds, metrics(8., 18.), &tx);
        assert_eq!(grid.geometry().cols, Geometry::MAX_CELLS);
        let cell = grid.at(point(px(5990.), px(5.)));
        assert_eq!((cell.row, cell.column), (0, 499));
    }
}
