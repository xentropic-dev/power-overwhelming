//! Wheel step accumulation, scrollbar geometry and mouse button tracking adapted from
//! herdr-app `src/terminal.rs` (Apache-2.0).

use gpui::{MouseButton, Pixels, Point, ScrollDelta, Size, TouchPhase, point};

/// Whole lines to scroll for one event. A touchpad sends many small moves: they add up to a
/// line before one scrolls, and each gesture starts from nothing. The remainder stays under a
/// screen, so turning back is quick.
pub(super) fn scroll_steps(scrolled: &mut f32, phase: TouchPhase, lines: f32, rows: f32) -> i32 {
    match phase {
        TouchPhase::Started => {
            *scrolled = 0.;
            0
        }
        TouchPhase::Moved => {
            let before = scrolled.trunc() as i32;
            *scrolled += lines;
            let after = scrolled.trunc() as i32;
            *scrolled %= rows.max(1.);
            after - before
        }
        TouchPhase::Ended => 0,
    }
}

/// Columns (x) and lines (y) one wheel event scrolls, positive toward the top and left.
/// Vertical lines scale by `speed`.
pub(super) fn scroll_delta_steps(
    scrolled: &mut Point<f32>,
    delta: ScrollDelta,
    phase: TouchPhase,
    cell: Size<Pixels>,
    (cols, rows): (u16, u16),
    speed: f32,
) -> Point<i32> {
    let lines = match delta {
        ScrollDelta::Lines(lines) if cfg!(target_os = "macos") => {
            // macOS reports wheel ticks; a slow notch is a fraction of one. As in Ghostty a
            // tick counts at least once, times three lines.
            let ticks = |t: f32| {
                if t == 0. {
                    0.
                } else {
                    t.signum() * t.abs().max(1.) * 3.
                }
            };
            point(ticks(lines.x) / 3., ticks(lines.y))
        }
        ScrollDelta::Lines(lines) => lines,
        ScrollDelta::Pixels(pixels) => point(
            f32::from(pixels.x) / f32::from(cell.width).max(1.),
            f32::from(pixels.y) / f32::from(cell.height).max(1.),
        ),
    };
    point(
        scroll_steps(&mut scrolled.x, phase, lines.x, f32::from(cols)),
        scroll_steps(&mut scrolled.y, phase, lines.y * speed, f32::from(rows)),
    )
}

/// A scroll thumb's top and length inside a `track` long lane: the scrollable area is `total`
/// rows and the `len` row viewport sits `offset` rows from its top. `None` when nothing
/// scrolls. `min` keeps a huge scrollback grabbable.
pub(super) fn scrollbar_thumb(
    total: u64,
    offset: u64,
    len: u64,
    track: f32,
    min: f32,
) -> Option<(f32, f32)> {
    if total <= len || len == 0 || track <= 0. {
        return None;
    }
    let thumb = (track * len as f32 / total as f32).clamp(min.min(track), track);
    let travel = track - thumb;
    let top = if travel <= 0. {
        0.
    } else {
        offset.min(total - len) as f32 / (total - len) as f32 * travel
    };
    Some((top, thumb))
}

/// Where in the thumb a press at `y` holds it: the point pressed, or its middle when the
/// press lands on the track outside it.
pub(super) fn scrollbar_grab_at(top: f32, len: f32, y: f32) -> f32 {
    if y >= top && y < top + len {
        y - top
    } else {
        len / 2.
    }
}

/// The row from the scrollback's top a thumb dragged to `top` shows first.
pub(super) fn scrollbar_row_at(total: u64, len: u64, track: f32, min: f32, top: f32) -> usize {
    let Some((_, thumb)) = scrollbar_thumb(total, 0, len, track, min) else {
        return 0;
    };
    let travel = track - thumb;
    if travel <= 0. {
        return 0;
    }
    (top.clamp(0., travel) / travel * (total - len) as f32).round() as usize
}

/// Records presses and releases. False for a motion or release of a button this terminal
/// never saw pressed, which the program must not hear about either.
pub(super) fn track_button(
    buttons: &mut Vec<MouseButton>,
    press: Option<bool>,
    button: MouseButton,
) -> bool {
    match press {
        Some(true) => {
            if !buttons.contains(&button) {
                buttons.push(button);
            }
            true
        }
        _ if !buttons.contains(&button) => false,
        Some(false) => {
            buttons.retain(|b| *b != button);
            true
        }
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{px, size};

    #[test]
    fn touchpad_moves_add_up_before_scrolling() {
        let mut scrolled = point(0., 0.);
        let cell = size(px(8.), px(16.));
        let step = |s: &mut Point<f32>, y: f32, phase| {
            scroll_delta_steps(
                s,
                ScrollDelta::Pixels(point(px(0.), px(y))),
                phase,
                cell,
                (80, 24),
                1.,
            )
            .y
        };
        assert_eq!(step(&mut scrolled, 0., TouchPhase::Started), 0);
        assert_eq!(step(&mut scrolled, 10., TouchPhase::Moved), 0);
        assert_eq!(step(&mut scrolled, 10., TouchPhase::Moved), 1);
        assert_eq!(step(&mut scrolled, -40., TouchPhase::Moved), -2);
        assert_eq!(step(&mut scrolled, 0., TouchPhase::Started), 0);
        assert_eq!(scrolled.y, 0.);
    }

    #[test]
    fn speed_scales_vertical_lines_only() {
        let mut scrolled = point(0., 0.);
        let steps = scroll_delta_steps(
            &mut scrolled,
            ScrollDelta::Pixels(point(px(16.), px(32.))),
            TouchPhase::Moved,
            size(px(8.), px(16.)),
            (80, 24),
            2.5,
        );
        assert_eq!(steps, point(2, 5));
    }

    #[test]
    fn thumb_spans_the_viewport_share_of_the_track() {
        assert_eq!(scrollbar_thumb(24, 0, 24, 240., 16.), None);
        assert_eq!(scrollbar_thumb(48, 24, 24, 240., 16.), Some((120., 120.)));
        assert_eq!(scrollbar_thumb(48, 0, 24, 240., 16.), Some((0., 120.)));
        // A huge scrollback keeps the minimum length.
        let (top, len) = scrollbar_thumb(100_000, 100_000 - 24, 24, 240., 16.).unwrap();
        assert_eq!((top, len), (224., 16.));
    }

    #[test]
    fn dragging_the_thumb_maps_back_to_rows() {
        assert_eq!(scrollbar_row_at(48, 24, 240., 16., 0.), 0);
        assert_eq!(scrollbar_row_at(48, 24, 240., 16., 120.), 24);
        assert_eq!(scrollbar_row_at(48, 24, 240., 16., 500.), 24);
        assert_eq!(scrollbar_row_at(48, 24, 240., 16., 60.), 12);
        assert_eq!(scrollbar_grab_at(100., 20., 105.), 5.);
        assert_eq!(scrollbar_grab_at(100., 20., 10.), 10.);
    }

    #[test]
    fn unpressed_buttons_report_nothing() {
        let mut buttons = Vec::new();
        assert!(!track_button(&mut buttons, Some(false), MouseButton::Right));
        assert!(!track_button(&mut buttons, None, MouseButton::Right));
        assert!(track_button(&mut buttons, Some(true), MouseButton::Right));
        assert!(track_button(&mut buttons, None, MouseButton::Right));
        assert!(track_button(&mut buttons, Some(false), MouseButton::Right));
        assert!(buttons.is_empty());
    }
}
