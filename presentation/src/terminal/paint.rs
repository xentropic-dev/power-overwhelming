use super::{TerminalView, cell_metrics, frame::CellPosition, input::register_mouse_exit};
use gpui::{prelude::*, *};
use infrastructure::theme::blend;
use std::sync::Arc;

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let metrics = cell_metrics(window.text_system(), &self.font, window.scale_factor());
        self.metrics = metrics;
        let (cell_size, pad) = (metrics.size, px(self.settings.padding));
        let font_size = px(self.font.size);
        let terminal = cx.entity();
        let pointer_owner = terminal.downgrade();
        let frame = self.frame.clone();
        let scheme = self.scheme;
        let colors = &scheme.colors;
        let (_, background, cursor_color) = scheme.terminal_ink(&self.terminal);
        let selection_background = Hsla::from(rgba(scheme.selection_background));
        let terminal_font = self.font.font.clone();
        let hovered_link = self.hovered_link().cloned();
        let pressed_link = self.pressed_link().cloned();
        let selection = self.selection_range().filter(|_| pressed_link.is_none());
        div()
            .id("terminal")
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .bg(rgb(background))
            .when(hovered_link.is_some(), |terminal| terminal.cursor_pointer())
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
                    move |bounds, (), window, cx| {
                        register_mouse_exit(pointer_owner.clone(), window);
                        window.with_content_mask(Some(ContentMask { bounds }), |window| {
                            let grid = metrics.snap(bounds.origin);
                            for (row, line) in frame.rows.iter().enumerate() {
                                for (column, cell) in line.cells.iter().enumerate() {
                                    let position = CellPosition { row, column };
                                    let origin = metrics.cell_origin(grid, row, column);
                                    let selected = selection.is_some_and(|(start, end)| {
                                        start <= position && position <= end
                                    });
                                    let cursor = frame.cursor == Some(position);
                                    if let Some(color) = cell.bg {
                                        window.paint_quad(fill(
                                            Bounds::new(origin, cell_size),
                                            color,
                                        ));
                                    }
                                    if selected {
                                        window.paint_quad(fill(
                                            Bounds::new(origin, cell_size),
                                            selection_background,
                                        ));
                                    }
                                    if cursor {
                                        let thickness = metrics.underline_thickness.max(px(2.));
                                        let bar = Bounds::new(
                                            origin + point(px(0.), cell_size.height - thickness),
                                            size(cell_size.width, thickness),
                                        );
                                        window.paint_quad(fill(bar, rgb(cursor_color)));
                                    }
                                    let mut fg = cell.fg;
                                    if let Some(link) = &cell.link {
                                        let color = if pressed_link
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
                                        };
                                        fg = rgb(color).into();
                                        let underline = Bounds::new(
                                            origin + point(px(0.), metrics.underline),
                                            size(cell_size.width, metrics.underline_thickness),
                                        );
                                        window.paint_quad(fill(underline, fg));
                                    }
                                    if selected && let Some(color) = scheme.selection_foreground {
                                        fg = rgba(color).into();
                                    }
                                    if cell.text.trim().is_empty() {
                                        continue;
                                    }
                                    let run = TextRun {
                                        len: cell.text.len(),
                                        font: terminal_font.clone(),
                                        color: fg,
                                        background_color: None,
                                        underline: None,
                                        strikethrough: None,
                                    };
                                    let shaped = window.text_system().shape_line(
                                        cell.text.clone().into(),
                                        font_size,
                                        &[run],
                                        None,
                                    );
                                    let _ = shaped.paint(
                                        metrics.text_origin(&shaped, origin),
                                        cell_size.height,
                                        window,
                                        cx,
                                    );
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
