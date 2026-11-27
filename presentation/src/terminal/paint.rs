//! Row drawing reuse adapted from herdr-app `src/terminal.rs` (Apache-2.0).

use super::{
    CellMetrics, TerminalView, bench, cell_metrics,
    frame::{CellPosition, Row},
    ime::Preedit,
    input::register_mouse_exit,
    links::{LinkAction, link_action},
    runs::{FAINT_OPACITY, RowOp, RunCell, SYMBOLS_FAMILY, Shaping, row_ops},
    sprites,
};
use gpui::{prelude::*, *};
use infrastructure::theme::{Scheme, blend};
use std::{ops::Range, rc::Rc};

/// Only row-local ranges enter the key, so moving a selection or link hover
/// leaves the drawing of unaffected rows intact.
#[derive(Default, PartialEq)]
struct Highlights {
    selected: Range<usize>,
    hovered: Range<usize>,
    pressed: Range<usize>,
}

impl Highlights {
    fn link_color(&self, column: usize, scheme: &Scheme, background: u32) -> u32 {
        let colors = &scheme.colors;
        if self.pressed.contains(&column) {
            blend(colors.link, background, 15)
        } else if self.hovered.contains(&column) {
            blend(colors.link, colors.foreground, 35)
        } else {
            colors.link
        }
    }
}

fn columns(span: Option<(CellPosition, CellPosition)>, y: usize, width: usize) -> Range<usize> {
    let Some((start, end)) = span.filter(|(start, end)| (start.row..=end.row).contains(&y)) else {
        return 0..0;
    };
    let left = if start.row == y {
        start.column.min(width)
    } else {
        0
    };
    let right = if end.row == y {
        end.column.saturating_add(1).min(width)
    } else {
        width
    };
    left..right
}

pub(super) struct RowDrawing {
    revision: u64,
    metrics: CellMetrics,
    highlights: Highlights,
    cursor: Option<usize>,
    ops: Vec<RowOp>,
}

impl Row {
    fn build_ops(
        &self,
        highlights: &Highlights,
        shaping: &Shaping,
        scheme: &Scheme,
        background: u32,
        text_system: &WindowTextSystem,
    ) -> Vec<RowOp> {
        let styled: Vec<_> = self
            .cells
            .iter()
            .enumerate()
            .map(|(column, cell)| {
                let selected = highlights.selected.contains(&column);
                let mut fg = if selected && let Some(color) = scheme.selection_foreground {
                    rgba(color).into()
                } else if cell.link.is_some() {
                    rgb(highlights.link_color(column, scheme, background)).into()
                } else {
                    cell.fg
                };
                let fade = cell.style.faint.then(|| {
                    fg = fg.opacity(FAINT_OPACITY);
                    if selected {
                        rgba(scheme.selection_background).into()
                    } else {
                        cell.bg.unwrap_or_else(|| rgb(background).into())
                    }
                });
                RunCell { cell, fg, fade }
            })
            .collect();
        row_ops(&styled, shaping, |text, size, runs| {
            text_system.layout_line(text, size, runs, None)
        })
    }

    fn draw_with(
        &self,
        revision: u64,
        metrics: CellMetrics,
        highlights: Highlights,
        cursor: Option<usize>,
        build: impl FnOnce(&Highlights) -> Vec<RowOp>,
    ) -> Rc<RowDrawing> {
        let mut cached = self.drawing.borrow_mut();
        if let Some(kept) = cached.as_ref()
            && kept.revision == revision
            && kept.metrics == metrics
            && kept.highlights == highlights
            && kept.cursor == cursor
        {
            return Rc::clone(kept);
        }
        bench::count(&bench::ROWS);
        let drawing = Rc::new(RowDrawing {
            revision,
            metrics,
            ops: build(&highlights),
            highlights,
            cursor,
        });
        *cached = Some(Rc::clone(&drawing));
        drawing
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        bench::count(&bench::TERMINAL);
        let metrics = cell_metrics(window.text_system(), &self.font, window.scale_factor());
        self.grid.metrics = metrics;
        let (cell_size, pad) = (metrics.size, px(self.settings.padding));
        let font_size = px(self.font.size);
        let terminal = cx.entity();
        let pointer_owner = terminal.downgrade();
        let frame = self.frame.clone();
        let scheme = self.scheme;
        let colors = &scheme.colors;
        let packed = |c: ghostty_vt::style::RgbColor| u32::from_be_bytes([0, c.r, c.g, c.b]);
        let ink = frame.colors.as_ref();
        let background = ink.map_or(colors.background, |c| packed(c.background));
        let cursor_color = ink.and_then(|c| c.cursor).map_or(colors.cursor, packed);
        let foreground = ink.map_or(colors.foreground, |c| packed(c.foreground));
        let preedit = self
            .typing
            .marked()
            .map(str::to_owned)
            .zip(self.frame.ime_cursor)
            .map(|(text, at)| Preedit {
                text,
                at,
                font: self.font.font.clone(),
                font_size: px(self.font.size),
                foreground: rgb(foreground).into(),
                background: rgb(background).into(),
            });
        let focus = self.focus.clone();
        let input_owner = terminal.clone();
        let selection_background = Hsla::from(rgba(scheme.selection_background));
        let terminal_font = self.font.font.clone();
        let hovered = self.hovered_span(window.modifiers());
        let pressed = self.pressed_span();
        let selection = self.selection_range().filter(|_| pressed.is_none());
        let ligatures = self.settings.ligatures;
        let bold_weight = FontWeight(f32::from(self.settings.font_weight_bold));
        let revision = self.paint_revision;
        let hovering = hovered.is_some();
        div()
            .id("terminal")
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .bg(Hsla::from(rgb(background)).opacity(self.settings.opacity))
            .when(hovering, |terminal| terminal.cursor_pointer())
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .on_key_up(cx.listener(Self::key_up))
            .on_hover(cx.listener(Self::hover))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::mouse_up_out))
            .on_scroll_wheel(cx.listener(Self::scroll_wheel))
            .child(
                canvas(
                    move |bounds, _, cx| {
                        terminal.update(cx, |terminal, _| terminal.set_bounds(bounds, metrics));
                    },
                    move |bounds, (), window, cx| {
                        bench::count(&bench::GRID);
                        register_mouse_exit(pointer_owner.clone(), window);
                        // While focused, input methods type into this terminal.
                        window.handle_input(
                            &focus,
                            ElementInputHandler::new(bounds, input_owner.clone()),
                            cx,
                        );
                        window.with_content_mask(Some(ContentMask { bounds }), |window| {
                            let grid = metrics.snap(bounds.origin);
                            let text_system = window.text_system().clone();
                            let symbols = text_system.resolve_font(&font(SYMBOLS_FAMILY));
                            let primary = text_system.resolve_font(&terminal_font);
                            let has_glyph =
                                |c: char| text_system.advance(primary, font_size, c).is_ok();
                            let rows: Vec<_> = frame
                                .rows
                                .iter()
                                .enumerate()
                                .map(|(row, line)| {
                                    let width = line.cells.len();
                                    let highlights = Highlights {
                                        selected: columns(selection, row, width),
                                        hovered: columns(hovered, row, width),
                                        pressed: columns(pressed, row, width),
                                    };
                                    let cursor = frame
                                        .cursor
                                        .filter(|c| ligatures && c.row == row)
                                        .map(|c| c.column);
                                    let shaping = Shaping {
                                        font: &terminal_font,
                                        font_size,
                                        cell_width: cell_size.width,
                                        cell_height: cell_size.height,
                                        symbols,
                                        has_glyph: &has_glyph,
                                        ligatures,
                                        bold_weight,
                                        cursor,
                                    };
                                    line.draw_with(
                                        revision,
                                        metrics,
                                        highlights,
                                        cursor,
                                        |highlights| {
                                            line.build_ops(
                                                highlights,
                                                &shaping,
                                                scheme,
                                                background,
                                                &text_system,
                                            )
                                        },
                                    )
                                })
                                .collect();
                            let span = |row: usize, from: usize, to: usize| {
                                Bounds::new(
                                    metrics.cell_origin(grid, row, from),
                                    size(cell_size.width * (to - from) as f32, cell_size.height),
                                )
                            };
                            for (row, drawing) in rows.iter().enumerate() {
                                for op in &drawing.ops {
                                    if let RowOp::Background { from, to, color } = op {
                                        window.paint_quad(fill(span(row, *from, *to), *color));
                                    }
                                }
                            }
                            for (row, drawing) in rows.iter().enumerate() {
                                let selected = &drawing.highlights.selected;
                                if !selected.is_empty() {
                                    window.paint_quad(fill(
                                        span(row, selected.start, selected.end),
                                        selection_background,
                                    ));
                                }
                            }
                            // Full-cell blocks can cover the cursor and link underline.
                            for (row, drawing) in rows.iter().enumerate() {
                                for op in &drawing.ops {
                                    if let RowOp::Sprite {
                                        column,
                                        character,
                                        color,
                                    } = op
                                    {
                                        let cell = span(row, *column, *column + 1);
                                        sprites::paint(*character, cell, *color, window);
                                    }
                                }
                            }
                            if let Some(cursor) = frame.cursor {
                                let origin = metrics.cell_origin(grid, cursor.row, cursor.column);
                                let thickness = metrics.underline_thickness.max(px(2.));
                                let bar = Bounds::new(
                                    origin + point(px(0.), cell_size.height - thickness),
                                    size(cell_size.width, thickness),
                                );
                                window.paint_quad(fill(bar, rgb(cursor_color)));
                            }
                            for (row, line) in frame.rows.iter().enumerate() {
                                let highlights = &rows[row].highlights;
                                for link in &line.links {
                                    // Web links are underlined only while Ctrl-hovered.
                                    let web =
                                        line.cells[link.start].link.as_deref().is_some_and(|uri| {
                                            link_action(uri) == LinkAction::OpenBrowser
                                        });
                                    if web && !highlights.hovered.contains(&link.start) {
                                        continue;
                                    }
                                    // Cells with an underline of their own keep its style.
                                    let mut column = link.start;
                                    while column < link.end {
                                        if !link_underline(&line.cells[column]) {
                                            column += 1;
                                            continue;
                                        }
                                        let from = column;
                                        while column < link.end
                                            && link_underline(&line.cells[column])
                                        {
                                            column += 1;
                                        }
                                        let mut underline = span(row, from, column);
                                        underline.origin.y += metrics.underline;
                                        underline.size.height = metrics.underline_thickness;
                                        window.paint_quad(fill(
                                            underline,
                                            rgb(highlights.link_color(from, scheme, background)),
                                        ));
                                    }
                                }
                            }
                            let baseline = point(metrics.glyph_x, metrics.baseline);
                            for (row, drawing) in rows.iter().enumerate() {
                                for op in &drawing.ops {
                                    if let RowOp::Text { column, .. } | RowOp::Icon { column, .. } =
                                        op
                                    {
                                        let origin = metrics.cell_origin(grid, row, *column);
                                        op.paint(origin, origin + baseline, window);
                                        op.paint_fade(origin, cell_size, window);
                                    }
                                }
                            }
                            for (row, drawing) in rows.iter().enumerate() {
                                for op in &drawing.ops {
                                    if let RowOp::Decoration { from, .. } = op {
                                        let origin = metrics.cell_origin(grid, row, *from);
                                        op.paint_decoration(origin, &metrics, window);
                                    }
                                }
                            }
                            if let Some(preedit) = preedit {
                                preedit.paint(grid, metrics, window, cx);
                            }
                        });
                    },
                )
                .absolute()
                .top(pad)
                .left(pad)
                .right(pad)
                .bottom(pad),
            )
            .children(self.hints.iter().enumerate().map(|(index, (position, _))| {
                div()
                    .absolute()
                    .top(pad + cell_size.height * position.row as f32)
                    .left(pad + cell_size.width * position.column as f32)
                    .h(cell_size.height)
                    .px(px(3.))
                    .flex()
                    .items_center()
                    .bg(rgb(colors.accent))
                    .text_color(rgb(colors.contrast(colors.accent)))
                    .text_size((font_size - px(1.)).max(px(8.)))
                    .font_weight(FontWeight::BOLD)
                    .child((index + 1).to_string())
            }))
            .when_some(self.hint_footer.clone(), |this, footer| {
                this.child(
                    div()
                        .absolute()
                        .bottom_1()
                        .left_2()
                        .bg(rgb(colors.raised))
                        .text_color(rgb(colors.foreground))
                        .px_2()
                        .text_xs()
                        .child(footer),
                )
            })
            .when_some(
                self.status
                    .clone()
                    .or_else(|| self.exited.then(|| "Process exited".into())),
                |this, status| {
                    this.child(
                        div()
                            .absolute()
                            .bottom_1()
                            .right_2()
                            .bg(rgb(colors.raised))
                            .text_color(rgb(colors.foreground))
                            .px_2()
                            .text_xs()
                            .child(status),
                    )
                },
            )
    }
}

/// Whether a link cell gets the link's own underline: not when the cell has an underline of its
/// own, whose style (dotted, dashed, double) a solid line would fill in.
pub(super) fn link_underline(cell: &super::frame::Cell) -> bool {
    cell.style.underline == ghostty_vt::style::Underline::None
}

#[cfg(test)]
mod tests {
    use super::{CellMetrics, Highlights, Rc, link_underline};
    use crate::terminal::frame::Frame;
    use ghostty_vt::{Terminal, render::RenderState, terminal::Options};

    #[test]
    fn link_underline_yields_to_the_cells_own_underline() {
        let mut terminal = Terminal::new(Options::default()).unwrap();
        terminal.vt_write(
            b"\x1b]8;;https://example.com\x1b\\a\x1b[4:4mb\x1b[4:5mc\x1b[21md\x1b[4:1me\x1b]8;;\x1b\\",
        );
        let frame = Frame::default()
            .update(&mut RenderState::new().unwrap(), &terminal)
            .unwrap()
            .unwrap();
        let cells = &frame.rows[0].cells;
        let drawn: Vec<_> = cells[..5].iter().map(link_underline).collect();
        assert_eq!(drawn, [true, false, false, false, false]);
    }

    #[test]
    fn cached_ops_follow_cursor_highlights_metrics_and_settings_revision() {
        let terminal = Terminal::new(Options::default()).unwrap();
        let frame = Frame::default()
            .update(&mut RenderState::new().unwrap(), &terminal)
            .unwrap()
            .unwrap();
        let row = &frame.rows[0];
        let metrics = CellMetrics::default();
        let cached = row.draw_with(0, metrics, Highlights::default(), None, |_| Vec::new());
        let kept = row.draw_with(0, metrics, Highlights::default(), None, |_| {
            panic!("reshaped a clean row")
        });
        assert!(Rc::ptr_eq(&cached, &kept));
        let cursor = row.draw_with(0, metrics, Highlights::default(), Some(2), |_| Vec::new());
        assert!(!Rc::ptr_eq(&kept, &cursor));
        let hovered = || Highlights {
            hovered: 2..9,
            ..Highlights::default()
        };
        let link = row.draw_with(0, metrics, hovered(), Some(2), |_| Vec::new());
        assert!(!Rc::ptr_eq(&cursor, &link));
        let scaled = CellMetrics {
            scale: 2.,
            ..metrics
        };
        let zoom = row.draw_with(0, scaled, hovered(), Some(2), |_| Vec::new());
        assert!(!Rc::ptr_eq(&link, &zoom));
        let font = row.draw_with(1, scaled, hovered(), Some(2), |_| Vec::new());
        assert!(!Rc::ptr_eq(&zoom, &font));
    }
}
