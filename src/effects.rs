// ABOUTME: Post-process effects state: animation timestamps, uniform packing,
// ABOUTME: animation lifecycle, and inverse mouse mapping for the CRT distortion.

use crate::config::EffectsConfig;
use std::time::Instant;

/// Decay windows for each animated effect (kept short so settling is obvious).
const KEYSTROKE_MS: f32 = 400.0;
const WOBBLE_MS: f32 = 600.0;
const CUBE_MS: f32 = 300.0;
const BELL_MS: f32 = 120.0;
/// CRT barrel coefficient (keep in sync with the WGSL shader's `K`).
pub const CRT_K: f32 = 0.18;

/// Effect-mask bits shared with the WGSL shader.
const FX_CRT: u32 = 1;
const FX_KEYSTROKE: u32 = 2;
const FX_WOBBLE: u32 = 4;
const FX_CUBE: u32 = 8;
const FX_BELL: u32 = 16;

/// `std140`-friendly layout for the effect shader. Field order and padding
/// must mirror the WGSL `struct EU` exactly.
#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct EffectsUniformData {
    pub surface: [f32; 2],
    pub time_ms: f32,
    pub effect_mask: u32,
    pub keystroke_xy: [f32; 2],
    pub keystroke_age_ms: f32,
    pub wobble_age_ms: f32,
    pub cube_age_ms: f32,
    pub cube_direction: f32,
    pub cursor_vel: [f32; 2],
    pub bell_age_ms: f32,
    /// Trailing pad keeps the struct a multiple of 16 bytes (WGSL uniform).
    pub _pad: [f32; 3],
}

#[derive(Debug)]
pub struct EffectsState {
    pub config: EffectsConfig,
    /// Whether the visual bell chrome is enabled (separate from [effects]).
    pub bell_enabled: bool,
    started_at: Instant,
    last_keystroke: Option<(Instant, (f32, f32))>,
    last_resize: Option<Instant>,
    /// Direction: +1.0 entering alt-screen, -1.0 leaving.
    last_alt_toggle: Option<(Instant, f32)>,
    last_bell: Option<Instant>,
    cursor_prev: Option<(Instant, f64, f64)>,
    cursor_vel: (f32, f32),
}

impl EffectsState {
    pub fn new(config: EffectsConfig, bell_enabled: bool) -> Self {
        Self {
            config,
            bell_enabled,
            started_at: Instant::now(),
            last_keystroke: None,
            last_resize: None,
            last_alt_toggle: None,
            last_bell: None,
            cursor_prev: None,
            cursor_vel: (0.0, 0.0),
        }
    }

    pub fn record_bell(&mut self, at: Instant) {
        if self.bell_enabled {
            self.last_bell = Some(at);
        }
    }

    pub fn record_keystroke(&mut self, at: Instant, xy_px: (f32, f32)) {
        if self.config.keystroke {
            self.last_keystroke = Some((at, xy_px));
        }
    }

    pub fn record_resize(&mut self, at: Instant) {
        if self.config.wobble {
            self.last_resize = Some(at);
        }
    }

    pub fn record_alt_toggle(&mut self, at: Instant, entering: bool) {
        if self.config.cube {
            self.last_alt_toggle = Some((at, if entering { 1.0 } else { -1.0 }));
        }
    }

    /// Update cursor velocity from a new physical mouse position (px/s).
    pub fn update_cursor(&mut self, at: Instant, x: f64, y: f64) {
        if let Some((t, px, py)) = self.cursor_prev {
            let dt = at.saturating_duration_since(t).as_secs_f32().max(1e-4);
            self.cursor_vel = (((x - px) as f32) / dt, ((y - py) as f32) / dt);
        }
        self.cursor_prev = Some((at, x, y));
    }

    /// Should the orchestrator request a redraw every frame right now?
    pub fn animation_active(&self, now: Instant) -> bool {
        if self.bell_enabled {
            if let Some(t) = self.last_bell {
                if age_ms(now, t) < BELL_MS {
                    return true;
                }
            }
        }
        if !self.config.any() {
            return false;
        }
        if self.config.keystroke {
            if let Some((t, _)) = self.last_keystroke {
                if age_ms(now, t) < KEYSTROKE_MS {
                    return true;
                }
            }
        }
        if self.config.wobble {
            if let Some(t) = self.last_resize {
                if age_ms(now, t) < WOBBLE_MS {
                    return true;
                }
            }
        }
        if self.config.cube {
            if let Some((t, _)) = self.last_alt_toggle {
                if age_ms(now, t) < CUBE_MS {
                    return true;
                }
            }
        }
        false
    }

    pub fn build_uniform(&self, now: Instant, surface: (f32, f32)) -> EffectsUniformData {
        let time_ms = age_ms(now, self.started_at);

        let mut mask = 0u32;
        if self.config.crt {
            mask |= FX_CRT;
        }
        let (ks_xy, ks_age) = self
            .last_keystroke
            .map(|(t, xy)| (xy, age_ms(now, t)))
            .unwrap_or(((0.0, 0.0), f32::INFINITY));
        if self.config.keystroke && ks_age < KEYSTROKE_MS {
            mask |= FX_KEYSTROKE;
        }
        let wb_age = self
            .last_resize
            .map(|t| age_ms(now, t))
            .unwrap_or(f32::INFINITY);
        if self.config.wobble && wb_age < WOBBLE_MS {
            mask |= FX_WOBBLE;
        }
        let (cb_age, cb_dir) = self
            .last_alt_toggle
            .map(|(t, d)| (age_ms(now, t), d))
            .unwrap_or((f32::INFINITY, 0.0));
        if self.config.cube && cb_age < CUBE_MS {
            mask |= FX_CUBE;
        }

        let bell_age = self
            .last_bell
            .map(|t| age_ms(now, t))
            .unwrap_or(f32::INFINITY);
        if self.bell_enabled && bell_age < BELL_MS {
            mask |= FX_BELL;
        }

        EffectsUniformData {
            surface: [surface.0, surface.1],
            time_ms,
            effect_mask: mask,
            keystroke_xy: [ks_xy.0, ks_xy.1],
            keystroke_age_ms: ks_age.min(1e6),
            wobble_age_ms: wb_age.min(1e6),
            cube_age_ms: cb_age.min(1e6),
            cube_direction: cb_dir,
            cursor_vel: [self.cursor_vel.0, self.cursor_vel.1],
            bell_age_ms: bell_age.min(1e6),
            _pad: [0.0; 3],
        }
    }

    /// Translate a physical-pixel mouse point on the screen back through the
    /// distortion to the corresponding pre-effect pixel. Only CRT bends
    /// space persistently, so we only invert when CRT is enabled.
    pub fn inverse_pick(&self, x_px: f64, y_px: f64, surface: (f64, f64)) -> (f64, f64) {
        if !self.config.crt || surface.0 <= 0.0 || surface.1 <= 0.0 {
            return (x_px, y_px);
        }
        // Newton iteration on the barrel: forward maps p -> p * (1 + K * |p|^2)
        // in centered [-1, 1] coords. Inverse: divide by the same factor.
        let mut px = x_px / surface.0 * 2.0 - 1.0;
        let mut py = y_px / surface.1 * 2.0 - 1.0;
        let k = CRT_K as f64;
        for _ in 0..2 {
            let r2 = px * px + py * py;
            let s = 1.0 + k * r2;
            px /= s;
            py /= s;
        }
        ((px + 1.0) * 0.5 * surface.0, (py + 1.0) * 0.5 * surface.1)
    }
}

fn age_ms(now: Instant, t: Instant) -> f32 {
    now.saturating_duration_since(t).as_secs_f32() * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn cfg_all() -> EffectsConfig {
        EffectsConfig { crt: true, keystroke: true, wobble: true, cube: true }
    }

    #[test]
    fn animation_active_only_while_recent() {
        let mut s = EffectsState::new(cfg_all(), false);
        let t0 = Instant::now();
        s.record_keystroke(t0, (10.0, 10.0));
        assert!(s.animation_active(t0));
        // Long after the window, it settles.
        assert!(!s.animation_active(t0 + Duration::from_millis(500)));
    }

    #[test]
    fn mask_drops_bits_after_decay() {
        let mut s = EffectsState::new(cfg_all(), false);
        let t0 = Instant::now();
        s.record_keystroke(t0, (0.0, 0.0));
        let u_now = s.build_uniform(t0, (100.0, 100.0));
        assert!(u_now.effect_mask & FX_KEYSTROKE != 0);
        let u_late = s.build_uniform(t0 + Duration::from_secs(1), (100.0, 100.0));
        assert!(u_late.effect_mask & FX_KEYSTROKE == 0);
        // CRT is persistent.
        assert!(u_late.effect_mask & FX_CRT != 0);
    }

    #[test]
    fn inverse_pick_is_noop_without_crt() {
        let s = EffectsState::new(EffectsConfig::default(), false);
        let p = s.inverse_pick(123.0, 456.0, (1000.0, 800.0));
        assert!((p.0 - 123.0).abs() < 1e-9 && (p.1 - 456.0).abs() < 1e-9);
    }

    #[test]
    fn inverse_pick_round_trips_to_screen_center() {
        // The center is a fixed point of the barrel map; inverse should
        // return the same coordinates exactly.
        let s = EffectsState::new(EffectsConfig { crt: true, ..Default::default() }, false);
        let p = s.inverse_pick(500.0, 400.0, (1000.0, 800.0));
        assert!((p.0 - 500.0).abs() < 1e-6);
        assert!((p.1 - 400.0).abs() < 1e-6);
    }
}
