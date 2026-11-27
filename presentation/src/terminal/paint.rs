use super::{
    TerminalView, cell_metrics,
    frame::CellPosition,
    input::register_mouse_exit,
    runs::{FAINT_OPACITY, RowOp, RunCell, Shaping, row_ops},
};
use gpui::{prelude::*, *};
use infrastructure::theme::blend;
use std::sync::Arc;

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let metrics = cell_metrics(window.text_system(), &self.font, window.scale_factor());
        self.grid.metrics = metrics;
        let (cell_size, pad) = (metrics.size, px(self.settings.padding));
        let font_size = px(self.font.size);
        let terminal = cx.entity();
        let pointer_owner = terminal.downgrade();
        let frame = self.frame.clone();
        let scheme = self.scheme;
        let colors = &scheme.colors;
        let (_, background, cursor_color) = scheme.terminal_ink(&self.terminal);
        let background_color = Hsla::from(rgb(background));
        let selection_background = Hsla::from(rgba(scheme.selection_background));
        let terminal_font = self.font.font.clone();
        let hovered_link = self.hovered_link().cloned();
        let pressed_link = self.pressed_link().cloned();
        let selection = self.selection_range().filter(|_| pressed_link.is_none());
        let ligatures = self.settings.ligatures;
        let bold_weight = FontWeight(f32::from(self.settings.font_weight_bold));
        let hovering = hovered_link.is_some();
        let link_color = move |link: &Arc<str>| {
            if pressed_link
                .as_ref()
                .is_some_and(|pressed| Arc::ptr_eq(link, pressed))
            {
                blend(colors.link, background, 15)
            } else if hovered_link
                .as_ref()
                .is_some_and(|hovered| Arc::ptr_eq(link, hovered))
            {
                blend(colors.link, colors.foreground, 35)
            } else {
                colors.link
            }
        };
        div()
            .id("terminal")
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .bg(rgb(background))
            .when(hovering, |terminal| terminal.cursor_pointer())
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
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
                    move |bounds, (), window, _| {
                        register_mouse_exit(pointer_owner.clone(), window);
                        window.with_content_mask(Some(ContentMask { bounds }), |window| {
                            let grid = metrics.snap(bounds.origin);
                            let text_system = window.text_system().clone();
                            let rows: Vec<_> = frame
                                .rows
                                .iter()
                                .enumerate()
                                .map(|(row, line)| {
                                    let (styled, selected): (Vec<_>, Vec<_>) = line
                                        .cells
                                        .iter()
                                        .enumerate()
                                        .map(|(column, cell)| {
                                            let position = CellPosition { row, column };
                                            let selected = selection.is_some_and(|(start, end)| {
                                                start <= position && position <= end
                                            });
                                            let mut fg = cell.fg;
                                            if let Some(link) = &cell.link {
                                                fg = rgb(link_color(link)).into();
                                            }
                                            if selected
                                                && let Some(color) = scheme.selection_foreground
                                            {
                                                fg = rgba(color).into();
                                            }
                                            let fade = cell.style.faint.then(|| {
                                                fg = fg.opacity(FAINT_OPACITY);
                                                if selected {
                                                    selection_background
                                                } else {
                                                    cell.bg.unwrap_or(background_color)
                                                }
                                            });
                                            (RunCell { cell, fg, fade }, selected)
                                        })
                                        .unzip();
                                    let shaping = Shaping {
                                        font: &terminal_font,
                                        font_size,
                                        cell_width: cell_size.width,
                                        ligatures,
                                        bold_weight,
                                        cursor: frame
                                            .cursor
                                            .filter(|cursor| cursor.row == row)
                                            .map(|cursor| cursor.column),
                                    };
                                    let ops = row_ops(&styled, &shaping, |text, size, runs| {
                                        text_system.layout_line(text, size, runs, None)
                                    });
                                    (ops, selected)
                                })
                                .collect();
                            let span = |row: usize, from: usize, to: usize| {
                                Bounds::new(
                                    metrics.cell_origin(grid, row, from),
                                    size(cell_size.width * (to - from) as f32, cell_size.height),
                                )
                            };
                            for (row, (ops, _)) in rows.iter().enumerate() {
                                for op in ops {
                                    if let RowOp::Background { from, to, color } = op {
                                        window.paint_quad(fill(span(row, *from, *to), *color));
                                    }
                                }
                            }
                            for (row, (_, selected)) in rows.iter().enumerate() {
                                let mut column = 0;
                                while column < selected.len() {
                                    let from = column;
                                    while column < selected.len() && selected[column] {
                                        column += 1;
                                    }
                                    if column > from {
                                        window.paint_quad(fill(
                                            span(row, from, column),
                                            selection_background,
                                        ));
                                    }
                                    column += 1;
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
                                for (column, cell) in line.cells.iter().enumerate() {
                                    if let Some(link) =
                                        cell.link.as_ref().filter(|_| link_underline(cell))
                                    {
                                        let origin = metrics.cell_origin(grid, row, column);
                                        let underline = Bounds::new(
                                            origin + point(px(0.), metrics.underline),
                                            size(cell_size.width, metrics.underline_thickness),
                                        );
                                        window.paint_quad(fill(underline, rgb(link_color(link))));
                                    }
                                }
                            }
                            let baseline = point(metrics.glyph_x, metrics.baseline);
                            for (row, (ops, _)) in rows.iter().enumerate() {
                                for op in ops {
                                    if let RowOp::Text { column, .. } = op {
                                        let origin = metrics.cell_origin(grid, row, *column);
                                        op.paint(origin + baseline, window);
                                        op.paint_fade(origin, cell_size, window);
                                    }
                                }
                            }
                            for (row, (ops, _)) in rows.iter().enumerate() {
                                for op in ops {
                                    if let RowOp::Decoration { from, .. } = op {
                                        let origin = metrics.cell_origin(grid, row, *from);
                                        op.paint_decoration(origin, &metrics, window);
                                    }
                                }
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
    use super::link_underline;
    use ghostty_vt::{Terminal, render::RenderState, terminal::Options};

    #[test]
    fn link_underline_yields_to_the_cells_own_underline() {
        let mut terminal = Terminal::new(Options::default()).unwrap();
        terminal.vt_write(
            b"\x1b]8;;https://example.com\x1b\\a\x1b[4:4mb\x1b[4:5mc\x1b[21md\x1b[4:1me\x1b]8;;\x1b\\",
        );
        let frame =
            super::super::frame::frame(&mut RenderState::new().unwrap(), &terminal, 0).unwrap();
        let cells = &frame.rows[0].cells;
        let drawn: Vec<_> = cells[..5].iter().map(link_underline).collect();
        assert_eq!(drawn, [true, false, false, false, false]);
    }
}
