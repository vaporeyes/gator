// ABOUTME: Encode mouse events as terminal byte sequences (SGR / X10) for
// ABOUTME: passthrough to child apps that requested DEC mouse reporting.

use crate::event::{MouseAction, MouseButton, Mods};
use crate::grid::MouseMode;

/// Encode for the PTY based on active DEC modes. None = no reporting enabled
/// (or a drag while `?1002` is off), so the caller handles it locally.
pub fn encode_mouse(
    mode: MouseMode,
    action: MouseAction,
    button: MouseButton,
    col: usize,
    row: usize,
    mods: Mods,
) -> Option<Vec<u8>> {
    if !mode.enabled() {
        return None;
    }
    if matches!(action, MouseAction::Drag) && !mode.drag {
        return None;
    }
    // Wheel events are always reported as Press with codes 64/65; drag/motion
    // bits do not apply. Real buttons use 0/1/2 and combine with motion.
    let is_wheel = matches!(button, MouseButton::WheelUp | MouseButton::WheelDown);
    let mut cb: u32 = match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::WheelUp => 64,
        MouseButton::WheelDown => 65,
    };
    if mods.shift {
        cb |= 4;
    }
    if mods.alt {
        cb |= 8;
    }
    if mods.ctrl {
        cb |= 16;
    }
    if matches!(action, MouseAction::Drag) && !is_wheel {
        cb |= 32;
    }

    let (cx, cy) = (col + 1, row + 1);
    let bytes = if mode.sgr_encoded {
        let suffix = if matches!(action, MouseAction::Release) { 'm' } else { 'M' };
        format!("\x1b[<{};{};{}{}", cb, cx, cy, suffix).into_bytes()
    } else {
        // Legacy X10: 3 bytes after \x1b[M, each + 32. Release uses button 3.
        let cb_x10 = if matches!(action, MouseAction::Release) { 3 } else { cb };
        let mut v = b"\x1b[M".to_vec();
        v.push((cb_x10 as u8).wrapping_add(32));
        v.push((cx.min(223) as u8).wrapping_add(32));
        v.push((cy.min(223) as u8).wrapping_add(32));
        v
    };
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(sgr: bool) -> MouseMode {
        MouseMode { button: true, drag: true, sgr_encoded: sgr }
    }

    #[test]
    fn no_encoding_when_disabled() {
        let off = MouseMode::default();
        assert!(encode_mouse(
            off,
            MouseAction::Press,
            MouseButton::Left,
            0,
            0,
            Mods::default()
        )
        .is_none());
    }

    #[test]
    fn sgr_press_and_release_use_distinct_suffix() {
        let p = encode_mouse(
            mode(true),
            MouseAction::Press,
            MouseButton::Left,
            5,
            9,
            Mods::default(),
        )
        .unwrap();
        let r = encode_mouse(
            mode(true),
            MouseAction::Release,
            MouseButton::Left,
            5,
            9,
            Mods::default(),
        )
        .unwrap();
        assert_eq!(p, b"\x1b[<0;6;10M");
        assert_eq!(r, b"\x1b[<0;6;10m");
    }

    #[test]
    fn sgr_drag_sets_motion_bit() {
        let d = encode_mouse(
            mode(true),
            MouseAction::Drag,
            MouseButton::Left,
            0,
            0,
            Mods::default(),
        )
        .unwrap();
        // Cb = 0 (left) | 32 (motion) = 32.
        assert_eq!(d, b"\x1b[<32;1;1M");
    }

    #[test]
    fn x10_release_reports_button_3() {
        let r = encode_mouse(
            mode(false),
            MouseAction::Release,
            MouseButton::Left,
            0,
            0,
            Mods::default(),
        )
        .unwrap();
        // \x1b[M then (3+32), (1+32), (1+32) = '#', '!', '!'.
        assert_eq!(r, b"\x1b[M#!!");
    }

    #[test]
    fn wheel_up_encodes_as_button_64() {
        let e = encode_mouse(
            mode(true),
            MouseAction::Press,
            MouseButton::WheelUp,
            0,
            0,
            Mods::default(),
        )
        .unwrap();
        assert_eq!(e, b"\x1b[<64;1;1M");
    }

    #[test]
    fn drag_dropped_when_drag_mode_off() {
        let m = MouseMode { button: true, drag: false, sgr_encoded: true };
        assert!(encode_mouse(
            m,
            MouseAction::Drag,
            MouseButton::Left,
            0,
            0,
            Mods::default()
        )
        .is_none());
    }
}
