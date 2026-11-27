//! A modal overlay that draws a panel a plugin opened, such as the dashboard. The workspace
//! polls the daemon overview while it is open and asks the plugin to redraw it.

use crate::{
    keymap::{Command, Keymap, Scope},
    status_bar::ui_color,
};
use gpui::{prelude::*, *};
use infrastructure::{
    plugins::{
        panel::{Panel, PanelBar, PanelItem, PanelLine},
        ui::UiColor,
    },
    theme::Colors,
};
use std::time::Duration;

/// Text size of panel lines, which lay out in monospace columns.
pub const TEXT_SIZE: f32 = 13.;
/// The widest the panel grows, in pixels.
const MAX_WIDTH: f32 = 960.;
/// Horizontal padding inside the panel, in pixels, both sides together.
const PADDING: f32 = 24.;
/// One scroll step, in pixels.
const STEP: f32 = 48.;
/// The default redraw interval when a plugin asks for none.
pub const DEFAULT_REFRESH: Duration = Duration::from_millis(2000);

pub struct PanelView {
    /// Distinguishes this opening from later ones, so stale polls stop.
    pub id: u64,
    pub panel_id: String,
    /// The latest drawing, or `None` until the first one arrives.
    pub panel: Option<Panel>,
    pub error: Option<String>,
    pub refresh: Duration,
    /// The command that opened the panel. Pressing its key again closes it.
    pub opener: Option<Command>,
    scroll: ScrollHandle,
    pub focus: FocusHandle,
}

impl PanelView {
    pub fn new(
        id: u64,
        panel_id: String,
        refresh: Option<Duration>,
        opener: Option<Command>,
        focus: FocusHandle,
    ) -> Self {
        Self {
            id,
            panel_id,
            panel: None,
            error: None,
            refresh: refresh.unwrap_or(DEFAULT_REFRESH),
            opener,
            scroll: ScrollHandle::new(),
            focus,
        }
    }

    /// Shows a new drawing and clears an earlier error.
    pub fn show(&mut self, panel: Panel) {
        self.panel = Some(panel);
        self.error = None;
    }

    /// Scrolls one step up or down.
    pub fn step(&mut self, up: bool) {
        let delta = px(if up { STEP } else { -STEP });
        let offset =
            (self.scroll.offset().y + delta).clamp(-self.scroll.max_offset().height, px(0.));
        self.scroll.set_offset(point(px(0.), offset));
    }

    /// The panel width in monospace columns for a window `viewport` wide.
    pub fn columns(viewport: Pixels, cell_width: Pixels) -> u32 {
        let width = (f32::from(viewport) - 32.).clamp(0., MAX_WIDTH) - PADDING;
        (width / f32::from(cell_width).max(1.)).max(20.) as u32
    }

    pub fn render(
        &self,
        colors: &'static Colors,
        keymap: &Keymap,
        font_family: &'static str,
        cell_width: Pixels,
    ) -> impl IntoElement {
        let close = keymap.first(Scope::Panel, Command::PanelClose);
        let scroll = [Command::PanelDown, Command::PanelUp]
            .map(|command| keymap.first(Scope::Panel, command))
            .join("/");
        let title = self
            .panel
            .as_ref()
            .map_or_else(|| "Loading…".to_owned(), |panel| panel.title.clone());
        let sections = self.panel.iter().flat_map(|panel| {
            panel
                .sections
                .iter()
                .enumerate()
                .map(move |(index, section)| {
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .when(index > 0, |section| {
                            section
                                .mt_2()
                                .pt_2()
                                .border_t_1()
                                .border_color(rgb(colors.border))
                        })
                        .child(
                            div()
                                .text_color(rgb(colors.accent))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(section.heading.clone()),
                        )
                        .children(
                            section
                                .lines
                                .iter()
                                .map(|line| render_line(colors, line, cell_width)),
                        )
                })
        });
        div()
            .absolute()
            .inset_0()
            .occlude()
            .bg(rgba((colors.background << 8) | 0x88))
            .flex()
            .justify_center()
            .items_start()
            .pt(px(48.))
            .px_4()
            .pb_4()
            .child(
                div()
                    .id("panel-view")
                    .track_focus(&self.focus)
                    .w_full()
                    .max_w(px(MAX_WIDTH))
                    .max_h_full()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .overflow_hidden()
                    .bg(rgb(colors.surface))
                    .border_1()
                    .border_color(rgb(colors.border))
                    .shadow_lg()
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .justify_between()
                            .items_baseline()
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(colors.muted))
                                    .child(format!("{close} close · {scroll} scroll")),
                            ),
                    )
                    .when_some(self.error.clone(), |panel, error| {
                        panel.child(
                            div()
                                .flex_none()
                                .text_sm()
                                .text_color(rgb(colors.error))
                                .child(error),
                        )
                    })
                    .child(
                        div()
                            .id("panel-view-body")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .font_family(font_family)
                            .text_size(px(TEXT_SIZE))
                            .flex()
                            .flex_col()
                            .children(sections),
                    ),
            )
    }
}

fn render_line(colors: &'static Colors, line: &PanelLine, cell_width: Pixels) -> impl IntoElement {
    let group = |items: &[PanelItem], right: bool| {
        div()
            .flex()
            .items_center()
            .gap(cell_width)
            .when(right, |group| group.flex_none())
            .when(!right, |group| group.min_w_0().overflow_hidden())
            .children(
                items
                    .iter()
                    .map(|item| render_item(colors, item, cell_width)),
            )
    };
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(cell_width * 2.)
        .min_h(px(TEXT_SIZE * 1.5))
        .child(group(&line.left, false))
        .child(group(&line.right, true))
}

fn resolve(colors: &Colors, value: Option<&str>, fallback: u32) -> u32 {
    value
        .and_then(|value| UiColor::parse(value).ok())
        .map_or(fallback, |role| ui_color(colors, role, colors.surface))
}

fn render_item(colors: &'static Colors, item: &PanelItem, cell_width: Pixels) -> AnyElement {
    match item {
        PanelItem::Text(segment) => {
            let background = resolve(colors, segment.bg.as_deref(), colors.surface);
            div()
                .whitespace_nowrap()
                .overflow_hidden()
                .text_ellipsis()
                .text_color(rgb(resolve(
                    colors,
                    segment.fg.as_deref(),
                    colors.foreground,
                )))
                .when(segment.bg.is_some(), |text| text.bg(rgb(background)).px_1())
                .when(segment.bold, |text| text.font_weight(FontWeight::BOLD))
                .when(segment.italic, |text| text.italic())
                .child(segment.text.clone())
                .into_any_element()
        }
        PanelItem::Bar(bar) => render_bar(colors, bar, cell_width).into_any_element(),
        PanelItem::Spacer => div().flex_none().w(cell_width * 2.).into_any_element(),
    }
}

/// A track `width` columns wide, filled to `ratio`.
fn render_bar(colors: &Colors, bar: &PanelBar, cell_width: Pixels) -> impl IntoElement {
    let ratio = if bar.ratio.is_finite() {
        bar.ratio.clamp(0., 1.)
    } else {
        0.
    };
    div()
        .flex_none()
        .w(cell_width * bar.width as f32)
        .h(px(TEXT_SIZE * 0.7))
        .bg(rgb(resolve(colors, bar.bg.as_deref(), colors.raised)))
        .child(div().h_full().w(relative(ratio)).bg(rgb(resolve(
            colors,
            bar.fg.as_deref(),
            colors.accent,
        ))))
}

#[cfg(test)]
mod tests {
    use super::PanelView;
    use gpui::px;

    #[test]
    fn columns_follow_the_viewport() {
        assert_eq!(PanelView::columns(px(2000.), px(8.)), 117);
        assert_eq!(PanelView::columns(px(500.), px(8.)), 55);
        assert_eq!(PanelView::columns(px(100.), px(8.)), 20);
    }
}
