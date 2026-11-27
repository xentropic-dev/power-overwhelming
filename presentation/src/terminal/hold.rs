//! Synchronized output (mode 2026). Following Ghostty's `set_render_hold` and herdr-app
//! `src/vt.rs` (Apache-2.0), the frame is captured inside `vt_write` at the moment a hold
//! begins, so the screen keeps exactly what the program left there, not an older paint.

use super::frame::Frame;
use ghostty_vt::{Terminal, render::RenderState};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

/// The render state and frame captured when the latest hold began. The render state was
/// cleaned at the capture, so it stays consistent with the frame for later damage tracking.
pub(super) type Captured = Rc<RefCell<Option<(RenderState, Frame)>>>;

/// Captures the frame synchronously whenever the program starts synchronized output.
pub(super) fn install(terminal: &mut Terminal, captured: Captured, contrast: Rc<Cell<f32>>) {
    terminal.set_render_hold(Some(Box::new(move |terminal, held| {
        if !held {
            return;
        }
        let Ok(mut renderer) = RenderState::new() else {
            return;
        };
        if let Ok(Some(frame)) =
            Frame::default().update_with(&mut renderer, terminal, contrast.get())
        {
            *captured.borrow_mut() = Some((renderer, frame));
        }
    })));
}

#[cfg(test)]
mod tests {
    use super::*;
    use ghostty_vt::terminal::{Event, Mode, Options};

    fn line(frame: &Frame) -> String {
        frame.rows[0]
            .cells
            .iter()
            .map(|cell| cell.text.as_str())
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    fn terminal() -> (Terminal, Captured, RenderState, Frame) {
        let mut terminal = Terminal::new(Options {
            cols: 20,
            rows: 2,
            ..Options::default()
        })
        .unwrap();
        let captured = Captured::default();
        install(&mut terminal, captured.clone(), Rc::new(Cell::new(1.)));
        let mut renderer = RenderState::new().unwrap();
        terminal.vt_write(b"PREVIOUS");
        let frame = Frame::default()
            .update(&mut renderer, &terminal)
            .unwrap()
            .unwrap();
        (terminal, captured, renderer, frame)
    }

    fn holds(terminal: &mut Terminal) -> Vec<bool> {
        terminal
            .events()
            .filter_map(|event| match event {
                Event::RenderHold(held) => Some(held),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_hold_started_mid_chunk_shows_the_frame_before_it_not_the_last_paint() {
        let (mut terminal, captured, _, stale) = terminal();
        assert_eq!(line(&stale), "PREVIOUS");
        terminal.vt_write(b"\r\x1b[KREADY\x1b[?2026h\r\x1b[KUPDATING");
        assert_eq!(holds(&mut terminal), [true]);
        let (_, frame) = captured.borrow_mut().take().unwrap();
        assert_eq!(line(&frame), "READY");
    }

    #[test]
    fn explicit_release_shows_the_finished_update() {
        let (mut terminal, captured, _, _) = terminal();
        terminal.vt_write(b"\r\x1b[KREADY\x1b[?2026h\r\x1b[KUPDATING");
        let (mut renderer, held) = captured.borrow_mut().take().unwrap();
        assert_eq!(line(&held), "READY");
        terminal.vt_write(b"\x1b[?2026l");
        assert_eq!(holds(&mut terminal), [true, false]);
        assert!(captured.borrow().is_none());
        let after = held.update(&mut renderer, &terminal).unwrap().unwrap();
        assert_eq!(line(&after), "UPDATING");
    }

    #[test]
    fn deadline_release_shows_the_partial_update() {
        let (mut terminal, captured, _, _) = terminal();
        terminal.vt_write(b"\r\x1b[KREADY\x1b[?2026h\r\x1b[KUPDATING");
        let (mut renderer, held) = captured.borrow_mut().take().unwrap();
        // The view's timer ends a hold the program never released this way, then repaints
        // from the captured state.
        terminal.set_mode(Mode::SYNC_OUTPUT, false).unwrap();
        assert_eq!(holds(&mut terminal), [true]);
        assert!(!terminal.mode(Mode::SYNC_OUTPUT).unwrap());
        let after = held.update(&mut renderer, &terminal).unwrap().unwrap();
        assert_eq!(line(&after), "UPDATING");
    }
}
