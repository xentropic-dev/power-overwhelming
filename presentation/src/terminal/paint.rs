use super::{
    TerminalView, cell_metrics,
    frame::CellPosition,
    ime::Preedit,
    input::register_mouse_exit,
    runs::{RowOp, RunCell, Shaping, row_ops},
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
        let (foreground, background, cursor_color) = scheme.terminal_ink(&self.terminal);
        let preedit = self
            .marked
            .clone()
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
        let hovered_link = self.hovered_link().cloned();
        let pressed_link = self.pressed_link().cloned();
        let selection = self.selection_range().filter(|_| pressed_link.is_none());
        let ligatures = self.settings.ligatures;
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
                                            (RunCell { cell, fg }, selected)
                                        })
                                        .unzip();
                                    let shaping = Shaping {
                                        font: &terminal_font,
                                        font_size,
                                        cell_width: cell_size.width,
                                        ligatures,
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
                                    if let Some(link) = &cell.link {
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
