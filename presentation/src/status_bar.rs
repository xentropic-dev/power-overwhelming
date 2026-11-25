use crate::terminal_view::terminal_font_family;
use gpui::{prelude::*, *};
use infrastructure::plugins::{
    ActiveView,
    ui::{Overflow, UiActions, UiColor, UiPlugins, UiRow, UiSegment, UiUpdate},
};
use infrastructure::{
    config::Sections,
    theme::{self, Colors, Scheme, blend},
};
use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

const PADDING: f32 = 16.;
const CELL_WIDTH: f32 = 7.;
const GROUP_GAP: usize = 3;
const JOIN_WIDTH: f32 = 11.;

#[derive(Clone, Copy)]
enum Separator<'a> {
    Text(&'a str),
    Arrow,
    Round,
}

impl<'a> Separator<'a> {
    fn from_glyph(text: &'a str) -> Self {
        match text.trim() {
            "" | "" => Self::Arrow,
            "" | "" => Self::Round,
            _ => Self::Text(text),
        }
    }

    fn columns(self) -> usize {
        match self {
            Self::Text(text) => text.width(),
            Self::Arrow | Self::Round => (JOIN_WIDTH / CELL_WIDTH).ceil() as usize,
        }
    }
}

fn color(colors: &Colors, role: UiColor) -> u32 {
    match role {
        UiColor::Foreground | UiColor::Contrast => colors.foreground,
        UiColor::Background => colors.background,
        UiColor::Accent => colors.accent,
        UiColor::Muted => colors.muted,
        UiColor::Surface => colors.surface,
        UiColor::Raised => colors.raised,
        UiColor::Ansi(index) => colors.indexed[index],
        UiColor::Hex(value) => value,
    }
}

fn segment_background(colors: &Colors, segment: &UiSegment, background: u32) -> u32 {
    segment
        .bg
        .as_deref()
        .and_then(|value| UiColor::parse(value).ok())
        .map_or(background, |role| color(colors, role))
}

fn style(colors: &Colors, div: Div, segment: &UiSegment, background: u32) -> Div {
    let ink = match segment
        .fg
        .as_deref()
        .and_then(|value| UiColor::parse(value).ok())
    {
        Some(UiColor::Contrast) => colors.contrast(background),
        Some(role) => color(colors, role),
        None => colors.foreground,
    };
    div.text_color(rgb(ink))
        .when(segment.bg.is_some(), |segment| {
            segment.bg(rgb(background)).px_1()
        })
        .when(segment.bold, |segment| {
            segment.font_weight(FontWeight::BOLD)
        })
        .when(segment.italic, |segment| segment.italic())
}

pub enum ConfigChanged<S> {
    Settings(S),
    Error(Option<String>),
}
impl<S: 'static> EventEmitter<ConfigChanged<S>> for StatusBar {}

pub struct StatusBar {
    active: Option<ActiveView>,
    scheme: &'static Scheme,
    font_family: &'static str,
    columns: usize,
    revision: u64,
    row: Option<UiRow>,
    plugins: UiPlugins,
    _updates: Task<()>,
}

impl StatusBar {
    pub fn new<S: Send + Sync + 'static>(
        window: &mut Window,
        cx: &mut Context<Self>,
        defaults: Sections,
        validate: fn(&Sections) -> anyhow::Result<S>,
    ) -> Self {
        let (plugins, updates) = UiPlugins::start(defaults, validate);
        let updates = cx.spawn_in(window, async move |bar, cx| {
            while let Ok(update) = updates.recv().await {
                if bar
                    .update_in(cx, |bar, _, cx| match update {
                        UiUpdate::Row { revision, row } if revision == bar.revision => {
                            bar.row = row;
                            cx.notify();
                        }
                        UiUpdate::Row { .. } => {}
                        UiUpdate::Settings(settings) => cx.emit(ConfigChanged::Settings(settings)),
                        UiUpdate::Error(error) => cx.emit(ConfigChanged::<S>::Error(error)),
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            active: None,
            scheme: theme::default_scheme(),
            font_family: terminal_font_family(cx),
            columns: 160,
            revision: 0,
            row: None,
            plugins,
            _updates: updates,
        }
    }

    pub fn reload(&mut self, _: &mut Context<Self>) {
        self.plugins.reload();
    }

    pub fn actions(&self) -> Option<&UiActions> {
        self.row.as_ref()?.actions.as_ref()
    }

    pub fn set_view(
        &mut self,
        active: Option<ActiveView>,
        scheme: &'static Scheme,
        cx: &mut Context<Self>,
    ) {
        let view_changed = self.active != active;
        let scheme_changed = !std::ptr::eq(self.scheme, scheme);
        if !view_changed && !scheme_changed {
            return;
        }
        if scheme_changed
            || self.active.as_ref().map(|view| &view.session)
                != active.as_ref().map(|view| &view.session)
        {
            self.row = None;
        }
        self.scheme = scheme;
        self.revision += 1;
        if view_changed {
            self.active = active;
            self.plugins
                .view(self.revision, self.active.clone(), scheme);
        } else {
            self.plugins.theme(self.revision, scheme);
        }
        cx.notify();
    }

    fn group(
        &self,
        segments: Vec<UiSegment>,
        separator: Separator<'_>,
        background: u32,
        right: bool,
    ) -> Div {
        let colors = &self.scheme.colors;
        let joined = !matches!(separator, Separator::Text(_));
        let mut group = div().flex().items_center().min_w_0().h_full();
        let mut previous = background;
        for (index, segment) in segments.into_iter().enumerate() {
            let next = segment_background(colors, &segment, background);
            match separator {
                Separator::Text(text) if index != 0 && !text.is_empty() => {
                    group = group.child(
                        div()
                            .flex_none()
                            .text_color(rgb(blend(colors.foreground, background, 55)))
                            .child(text.to_owned()),
                    );
                }
                Separator::Arrow | Separator::Round if index != 0 || right => {
                    group = group.child(join(separator, previous, next, right));
                }
                _ => {}
            }
            let cell = div()
                .min_w_0()
                .truncate()
                .flex_none()
                .child(segment.text.clone());
            group = group.child(style(colors, cell, &segment, next).when(joined, |cell| {
                cell.h_full()
                    .flex()
                    .items_center()
                    .bg(rgb(next))
                    .when(segment.bg.is_some(), |cell| cell.px(px(6.)))
            }));
            previous = next;
        }
        if joined && !right {
            group = group.child(join(separator, previous, background, right));
        }
        group
    }
}

fn join(separator: Separator<'_>, left: u32, right: u32, points_left: bool) -> Div {
    let (fill, background) = if points_left {
        (right, left)
    } else {
        (left, right)
    };
    div()
        .flex_none()
        .h_full()
        .w(px(JOIN_WIDTH))
        .bg(rgb(background))
        .when(fill != background, |element| {
            let rounded = matches!(separator, Separator::Round);
            element.child(
                canvas(
                    |_, _, _| {},
                    move |bounds, (), window, _| {
                        let (base, tip) = if points_left {
                            (bounds.right(), bounds.left())
                        } else {
                            (bounds.left(), bounds.right())
                        };
                        let (top, middle, bottom) =
                            (bounds.top(), bounds.center().y, bounds.bottom());
                        let mut path = PathBuilder::fill();
                        path.move_to(point(base, top));
                        if rounded {
                            path.curve_to(point(tip, middle), point(tip, top));
                            path.curve_to(point(base, bottom), point(tip, bottom));
                        } else {
                            path.line_to(point(tip, middle));
                            path.line_to(point(base, bottom));
                        }
                        path.close();
                        if let Ok(path) = path.build() {
                            window.paint_path(path, rgb(fill));
                        }
                    },
                )
                .size_full(),
            )
        })
}

impl Render for StatusBar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(row) = &self.row else {
            return div().into_any_element();
        };
        let mut left = row.left.clone();
        let mut right = row.right.clone();
        let separator = Separator::from_glyph(&row.separator);
        let joined = !matches!(separator, Separator::Text(_));
        let caps = if joined {
            (usize::from(!left.is_empty()) + usize::from(!right.is_empty())) * separator.columns()
        } else {
            0
        };
        fit(
            &mut left,
            &mut right,
            self.columns.saturating_sub(caps),
            separator.columns(),
        );
        if left.is_empty() && right.is_empty() {
            return div().into_any_element();
        }
        let background = self.scheme.colors.surface;
        let border = self.scheme.colors.border;
        let width_owner = cx.weak_entity();
        let padding = if joined { 0. } else { PADDING };
        let current_columns = self.columns;
        div()
            .relative()
            .flex_none()
            .flex()
            .h(px(25.))
            .min_w_0()
            .overflow_hidden()
            .border_t_1()
            .border_color(rgb(border))
            .bg(rgb(background))
            .px(px(padding / 2.))
            .items_center()
            .when(left.is_empty(), |bar| bar.justify_end())
            .when(!left.is_empty(), |bar| bar.justify_between())
            .gap(px(GROUP_GAP as f32 * CELL_WIDTH))
            .font_family(self.font_family)
            .text_size(px(11.))
            .children((!left.is_empty()).then(|| self.group(left, separator, background, false)))
            .children((!right.is_empty()).then(|| self.group(right, separator, background, true)))
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let columns =
                            ((f32::from(bounds.size.width) - padding) / CELL_WIDTH) as usize;
                        if current_columns != columns {
                            window.defer(cx, move |_, cx| {
                                let _ = width_owner.update(cx, |bar, cx| {
                                    if bar.columns != columns {
                                        bar.columns = columns;
                                        cx.notify();
                                    }
                                });
                            });
                        }
                    },
                    |_, (), _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .into_any_element()
    }
}

fn segment_width(segment: &UiSegment) -> usize {
    if segment.text.is_empty() {
        0
    } else {
        segment.text.width() + usize::from(segment.bg.is_some()) * 2
    }
}

fn group_width(group: &[UiSegment], separator: usize) -> usize {
    let visible = group
        .iter()
        .filter(|segment| !segment.text.is_empty())
        .count();
    group.iter().map(segment_width).sum::<usize>() + visible.saturating_sub(1) * separator
}

fn fit(left: &mut Vec<UiSegment>, right: &mut Vec<UiSegment>, columns: usize, separator: usize) {
    let left_width = group_width(left, separator);
    let right_width = group_width(right, separator);
    let gap = if left_width > 0 && right_width > 0 {
        GROUP_GAP
    } else {
        0
    };
    let mut excess = (left_width + right_width + gap).saturating_sub(columns);
    let mut counts = [left.len(), right.len()];
    for (overflow, preserve_basename) in [
        (Overflow::Path, true),
        (Overflow::Text, false),
        (Overflow::Path, false),
        (Overflow::Keep, false),
    ] {
        for (side, group) in [(1, &mut *right), (0, &mut *left)] {
            for segment in group.iter_mut().rev().filter(|segment| {
                std::mem::discriminant(&segment.overflow) == std::mem::discriminant(&overflow)
            }) {
                if excess == 0 {
                    break;
                }
                let before = segment_width(segment);
                let width = segment.text.width();
                let minimum = if preserve_basename {
                    segment
                        .text
                        .rsplit('/')
                        .next()
                        .map_or(0, |tail| tail.width() + 2)
                } else {
                    0
                };
                if width <= minimum {
                    continue;
                }
                let budget = width.saturating_sub(excess).max(minimum);
                segment.text = if matches!(overflow, Overflow::Path) {
                    shorten_path(&segment.text, budget)
                } else {
                    clip(&segment.text, budget, false)
                };
                excess = excess.saturating_sub(before - segment_width(segment));
                if segment.text.is_empty() {
                    counts[side] -= 1;
                    if counts[side] > 0 {
                        excess = excess.saturating_sub(separator);
                    } else if counts[1 - side] > 0 {
                        excess = excess.saturating_sub(GROUP_GAP);
                    }
                }
            }
        }
    }
    left.retain(|segment| !segment.text.is_empty());
    right.retain(|segment| !segment.text.is_empty());
}

fn shorten_path(path: &str, budget: usize) -> String {
    if path.width() <= budget {
        return path.into();
    }
    if let Some(suffix) = path
        .match_indices('/')
        .map(|(index, _)| &path[index..])
        .find(|suffix| suffix.width() < budget)
    {
        return format!("…{suffix}");
    }
    clip(path.rsplit('/').next().unwrap_or(path), budget, true)
}

fn clip(text: &str, budget: usize, keep_end: bool) -> String {
    if budget == 0 {
        return String::new();
    }
    if text.width() <= budget {
        return text.into();
    }
    let mut available = budget - 1;
    let mut take_char = |ch: &char| {
        let width = ch.width().unwrap_or(0);
        if width > available {
            return false;
        }
        available -= width;
        true
    };
    if keep_end {
        let tail: Vec<_> = text.chars().rev().take_while(&mut take_char).collect();
        std::iter::once('…').chain(tail.into_iter().rev()).collect()
    } else {
        text.chars()
            .take_while(take_char)
            .chain(std::iter::once('…'))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{GROUP_GAP, Overflow, UiSegment, fit, group_width};

    #[test]
    fn row_fitting_respects_plugin_overflow_priorities() {
        let segment = |text: &str, overflow| UiSegment {
            text: text.into(),
            fg: None,
            bg: None,
            bold: false,
            italic: false,
            overflow,
        };
        let mut left = vec![
            segment("Context", Overflow::Keep),
            segment("/some/long/path/to/worktree", Overflow::Path),
        ];
        let mut right = vec![segment("Provider", Overflow::Keep)];
        fit(&mut left, &mut right, 32, 3);
        assert_eq!(left[0].text, "Context");
        assert_eq!(left[1].text, "…/worktree");
        assert_eq!(right[0].text, "Provider");
        assert!(group_width(&left, 3) + group_width(&right, 3) + GROUP_GAP <= 32);
    }
}
