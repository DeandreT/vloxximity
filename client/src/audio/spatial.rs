//! 3D spatial audio processing.
//!
//! Three modes, selected per-frame via `SpatialConfig::mode`:
//! - `Off`    — centered mono; only distance attenuation.
//! - `Pan2D`  — legacy equal-power horizontal pan from azimuth.
//! - `Full3D` — ITD (per-ear delay) + front/back one-pole LPF + equal-power
//!              pan from the full 3D direction, with per-frame parameter
//!              ramping to avoid zipper noise.

use crate::position::{Position, Transform};

/// Samples per frame (20 ms at 48 kHz). Must match `audio::capture::FRAME_SIZE`.
pub const FRAME_LEN: usize = 960;
/// Sample rate the spatializer assumes. Must match `audio::capture::SAMPLE_RATE`.
const SAMPLE_RATE: f32 = 48_000.0;
/// Maximum interaural time difference in samples (~0.66 ms at 48 kHz).
pub const ITD_MAX_SAMPLES: usize = 32;
/// Delay-line length — next power of two ≥ `ITD_MAX_SAMPLES + 1`.
pub const DELAY_BUF_LEN: usize = 64;
const DELAY_MASK: usize = DELAY_BUF_LEN - 1;

/// Centered sources play at unity gain per ear (rather than ~0.707). Off-center
/// sources overshoot up to sqrt(2); the mixer's soft-clipper absorbs it. This
/// matches the legacy 2D panner so mode A/B loudness stays consistent.
const CENTER_COMPENSATION: f32 = std::f32::consts::SQRT_2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpatialMode {
    Off,
    Pan2D,
    Full3D,
}

impl Default for SpatialMode {
    fn default() -> Self {
        SpatialMode::Full3D
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SpatialConfig {
    pub mode: SpatialMode,
    pub min_distance: f32,
    pub max_distance: f32,
}

impl Default for SpatialConfig {
    fn default() -> Self {
        Self {
            mode: SpatialMode::Full3D,
            min_distance: 100.0,
            max_distance: 5000.0,
        }
    }
}

/// Per-peer filter state for 3D spatial audio.
///
/// Contains the ITD delay lines, one-pole LPF history, and ramping anchors
/// (`prev_*`) so gain/cutoff transitions spread across the 960-sample frame.
pub struct SpatialState {
    delay_l: [f32; DELAY_BUF_LEN],
    delay_r: [f32; DELAY_BUF_LEN],
    write_idx: usize,
    lpf_l_prev: f32,
    lpf_r_prev: f32,
    prev_gain_l: f32,
    prev_gain_r: f32,
    prev_lpf_a: f32,
    itd_samples_l: u8,
    itd_samples_r: u8,
    initialized: bool,
}

impl SpatialState {
    pub fn new() -> Self {
        Self {
            delay_l: [0.0; DELAY_BUF_LEN],
            delay_r: [0.0; DELAY_BUF_LEN],
            write_idx: 0,
            lpf_l_prev: 0.0,
            lpf_r_prev: 0.0,
            prev_gain_l: 0.0,
            prev_gain_r: 0.0,
            prev_lpf_a: 1.0,
            itd_samples_l: 0,
            itd_samples_r: 0,
            initialized: false,
        }
    }

    /// Process one mono frame into an interleaved stereo buffer.
    pub fn process_frame(
        &mut self,
        mono_in: &[f32],
        listener: &Transform,
        source_pos: &Position,
        cfg: &SpatialConfig,
        stereo_out: &mut [f32],
    ) {
        debug_assert_eq!(mono_in.len(), FRAME_LEN);
        debug_assert_eq!(stereo_out.len(), FRAME_LEN * 2);

        match cfg.mode {
            SpatialMode::Off => mono_into(mono_in, listener, source_pos, cfg, stereo_out),
            SpatialMode::Pan2D => pan_2d_into(mono_in, listener, source_pos, cfg, stereo_out),
            SpatialMode::Full3D => self.process_3d(mono_in, listener, source_pos, cfg, stereo_out),
        }
    }

    fn process_3d(
        &mut self,
        mono_in: &[f32],
        listener: &Transform,
        source_pos: &Position,
        cfg: &SpatialConfig,
        stereo_out: &mut [f32],
    ) {
        let distance = listener.position.distance_to(source_pos);
        let attenuation = crate::position::transform::distance_attenuation(
            distance,
            cfg.min_distance,
            cfg.max_distance,
        );

        // Unit direction in listener-local space. Coincident source → neutral
        // "dead ahead" direction so we don't divide by ~0.
        let (right_unit, front_unit) = if distance < 1e-3 {
            (0.0, 1.0)
        } else {
            let (r, _u, f) = listener.relative_position(source_pos);
            let inv = 1.0 / distance;
            (
                sanitize(r * inv).clamp(-1.0, 1.0),
                sanitize(f * inv).clamp(-1.0, 1.0),
            )
        };

        // Equal-power pan from the `right` axis.
        let angle = (right_unit + 1.0) * std::f32::consts::FRAC_PI_4;
        let target_gain_l = angle.cos() * attenuation * CENTER_COMPENSATION;
        let target_gain_r = angle.sin() * attenuation * CENTER_COMPENSATION;

        // Front/back LPF cutoff: geometric lerp 2 kHz → 20 kHz as `front_unit`
        // moves from -1 (behind) to +1 (ahead).
        let fc = 2000.0 * 10.0f32.powf((front_unit + 1.0) * 0.5);
        let two_pi_fc_over_fs = std::f32::consts::TAU * fc / SAMPLE_RATE;
        let mut target_lpf_a = 1.0 - (-two_pi_fc_over_fs).exp();
        if !target_lpf_a.is_finite() {
            target_lpf_a = 1.0;
        }
        let target_lpf_a = target_lpf_a.clamp(0.01, 1.0);

        // ITD held constant across the frame. Contralateral ear is delayed.
        let itd = (right_unit.abs() * ITD_MAX_SAMPLES as f32).round() as usize;
        let itd = itd.min(ITD_MAX_SAMPLES);
        let (itd_l, itd_r) = if right_unit > 0.0 { (itd, 0) } else { (0, itd) };
        self.itd_samples_l = itd_l as u8;
        self.itd_samples_r = itd_r as u8;

        // Snap prev values to targets on the first call so we don't ramp up
        // from silence on a peer's very first frame.
        if !self.initialized {
            self.prev_gain_l = target_gain_l;
            self.prev_gain_r = target_gain_r;
            self.prev_lpf_a = target_lpf_a;
            self.initialized = true;
        }

        let inv_n = 1.0 / FRAME_LEN as f32;
        let d_gl = (target_gain_l - self.prev_gain_l) * inv_n;
        let d_gr = (target_gain_r - self.prev_gain_r) * inv_n;
        let d_a = (target_lpf_a - self.prev_lpf_a) * inv_n;

        let mut gl = self.prev_gain_l;
        let mut gr = self.prev_gain_r;
        let mut a = self.prev_lpf_a;
        let mut w = self.write_idx;

        for (i, &x) in mono_in.iter().enumerate() {
            self.delay_l[w] = x;
            self.delay_r[w] = x;

            let rl = (w + DELAY_BUF_LEN - itd_l) & DELAY_MASK;
            let rr = (w + DELAY_BUF_LEN - itd_r) & DELAY_MASK;
            let xl = self.delay_l[rl];
            let xr = self.delay_r[rr];

            self.lpf_l_prev += a * (xl - self.lpf_l_prev);
            self.lpf_r_prev += a * (xr - self.lpf_r_prev);

            stereo_out[2 * i] = self.lpf_l_prev * gl;
            stereo_out[2 * i + 1] = self.lpf_r_prev * gr;

            w = (w + 1) & DELAY_MASK;
            gl += d_gl;
            gr += d_gr;
            a += d_a;
        }

        self.write_idx = w;
        self.prev_gain_l = target_gain_l;
        self.prev_gain_r = target_gain_r;
        self.prev_lpf_a = target_lpf_a;
    }
}

impl Default for SpatialState {
    fn default() -> Self {
        Self::new()
    }
}

fn sanitize(x: f32) -> f32 {
    if x.is_finite() {
        x
    } else {
        0.0
    }
}

/// Centered mono: both ears receive the attenuated mono signal, no pan.
pub fn mono_into(
    mono_in: &[f32],
    listener: &Transform,
    source_pos: &Position,
    cfg: &SpatialConfig,
    stereo_out: &mut [f32],
) {
    debug_assert_eq!(mono_in.len(), FRAME_LEN);
    debug_assert_eq!(stereo_out.len(), FRAME_LEN * 2);

    let distance = listener.position.distance_to(source_pos);
    let attenuation = crate::position::transform::distance_attenuation(
        distance,
        cfg.min_distance,
        cfg.max_distance,
    );
    for (i, &x) in mono_in.iter().enumerate() {
        let s = x * attenuation;
        stereo_out[2 * i] = s;
        stereo_out[2 * i + 1] = s;
    }
}

/// Legacy equal-power horizontal panning driven by `Transform::azimuth_to`.
pub fn pan_2d_into(
    mono_in: &[f32],
    listener: &Transform,
    source_pos: &Position,
    cfg: &SpatialConfig,
    stereo_out: &mut [f32],
) {
    debug_assert_eq!(mono_in.len(), FRAME_LEN);
    debug_assert_eq!(stereo_out.len(), FRAME_LEN * 2);

    let distance = listener.position.distance_to(source_pos);
    let azimuth = listener.azimuth_to(source_pos);
    let attenuation = crate::position::transform::distance_attenuation(
        distance,
        cfg.min_distance,
        cfg.max_distance,
    );

    let pan = (azimuth / std::f32::consts::FRAC_PI_2).clamp(-1.0, 1.0);
    let angle = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
    let left_gain = angle.cos() * attenuation * CENTER_COMPENSATION;
    let right_gain = angle.sin() * attenuation * CENTER_COMPENSATION;

    for (i, &x) in mono_in.iter().enumerate() {
        stereo_out[2 * i] = x * left_gain;
        stereo_out[2 * i + 1] = x * right_gain;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listener_at_origin() -> Transform {
        Transform {
            position: Position::new(0.0, 0.0, 0.0),
            front: Position::new(0.0, 0.0, 1.0),
            top: Position::new(0.0, 1.0, 0.0),
        }
    }

    fn cfg_full3d() -> SpatialConfig {
        SpatialConfig {
            mode: SpatialMode::Full3D,
            min_distance: 100.0,
            max_distance: 5000.0,
        }
    }

    fn ones_frame() -> Vec<f32> {
        vec![1.0; FRAME_LEN]
    }

    #[test]
    fn dead_front_symmetric() {
        let mut state = SpatialState::new();
        let listener = listener_at_origin();
        // Listener's right is -X, so a source on +Z is dead ahead.
        let source = Position::new(0.0, 0.0, 10.0);
        let cfg = cfg_full3d();
        let mut out = vec![0.0f32; FRAME_LEN * 2];

        state.process_frame(&ones_frame(), &listener, &source, &cfg, &mut out);

        assert_eq!(state.itd_samples_l, 0);
        assert_eq!(state.itd_samples_r, 0);
        // Past the LPF transient, L and R should match.
        for i in 16..FRAME_LEN {
            let l = out[2 * i];
            let r = out[2 * i + 1];
            assert!(
                (l - r).abs() < 1e-5,
                "L/R asymmetric at i={}: l={} r={}",
                i,
                l,
                r
            );
        }
    }

    #[test]
    fn dead_right_max_itd() {
        let mut state = SpatialState::new();
        let listener = listener_at_origin();
        // DirectX LH: listener facing +Z has right = +X, so a source at +X
        // world is dead right.
        let source = Position::new(10.0, 0.0, 0.0);
        let cfg = cfg_full3d();
        let mut out = vec![0.0f32; FRAME_LEN * 2];

        state.process_frame(&ones_frame(), &listener, &source, &cfg, &mut out);

        assert_eq!(state.itd_samples_l, ITD_MAX_SAMPLES as u8);
        assert_eq!(state.itd_samples_r, 0);

        let peak_l = out.iter().step_by(2).fold(0.0f32, |a, &s| a.max(s.abs()));
        let peak_r = out
            .iter()
            .skip(1)
            .step_by(2)
            .fold(0.0f32, |a, &s| a.max(s.abs()));
        assert!(peak_r > 0.9, "right ear too quiet: {}", peak_r);
        assert!(peak_l < 1e-4, "left ear should be silent: {}", peak_l);
    }

    #[test]
    fn dead_behind_lpf_engaged() {
        let mut state_front = SpatialState::new();
        let mut state_back = SpatialState::new();
        let listener = listener_at_origin();
        let cfg = cfg_full3d();

        // 10 kHz sinusoid — well above the ~2 kHz back-cutoff, at/near the
        // ~20 kHz front-cutoff.
        let mono: Vec<f32> = (0..FRAME_LEN)
            .map(|i| (2.0 * std::f32::consts::PI * 10_000.0 * i as f32 / SAMPLE_RATE).sin() * 0.5)
            .collect();

        let front_src = Position::new(0.0, 0.0, 10.0);
        let back_src = Position::new(0.0, 0.0, -10.0);

        let mut out_f = vec![0.0f32; FRAME_LEN * 2];
        let mut out_b = vec![0.0f32; FRAME_LEN * 2];

        // Warm each state with a frame so the LPF stabilizes.
        state_front.process_frame(&mono, &listener, &front_src, &cfg, &mut out_f);
        state_back.process_frame(&mono, &listener, &back_src, &cfg, &mut out_b);
        state_front.process_frame(&mono, &listener, &front_src, &cfg, &mut out_f);
        state_back.process_frame(&mono, &listener, &back_src, &cfg, &mut out_b);

        let rms = |v: &[f32]| -> f32 {
            let sum: f32 = v.iter().step_by(2).map(|s| s * s).sum();
            (sum / (v.len() as f32 / 2.0)).sqrt()
        };
        let rms_f = rms(&out_f);
        let rms_b = rms(&out_b);
        // Expect the back-facing LPF to attenuate 10 kHz by at least 6 dB.
        assert!(
            rms_b < rms_f * 0.5,
            "back-LPF not attenuating enough: front_rms={} back_rms={}",
            rms_f,
            rms_b
        );
    }

    #[test]
    fn direction_flip_no_nan() {
        let mut state = SpatialState::new();
        let listener = listener_at_origin();
        let cfg = cfg_full3d();
        let mut out = vec![0.0f32; FRAME_LEN * 2];

        let a = Position::new(-10.0, 0.0, 0.0);
        let b = Position::new(10.0, 0.0, 0.0);
        for _ in 0..4 {
            state.process_frame(&ones_frame(), &listener, &a, &cfg, &mut out);
            for &s in &out {
                assert!(s.is_finite(), "non-finite sample");
                assert!(s.abs() < 2.0, "sample out of range: {}", s);
            }
            state.process_frame(&ones_frame(), &listener, &b, &cfg, &mut out);
            for &s in &out {
                assert!(s.is_finite());
                assert!(s.abs() < 2.0);
            }
        }
    }

    #[test]
    fn zero_distance_stable() {
        let mut state = SpatialState::new();
        let listener = listener_at_origin();
        let cfg = cfg_full3d();
        let source = Position::new(0.0, 0.0, 0.0);
        let mut out = vec![0.0f32; FRAME_LEN * 2];

        state.process_frame(&ones_frame(), &listener, &source, &cfg, &mut out);
        for &s in &out {
            assert!(s.is_finite());
        }
    }

    #[test]
    fn ring_buffer_wrap() {
        let mut state = SpatialState::new();
        let listener = listener_at_origin();
        let cfg = cfg_full3d();

        // Impulse at sample 0 of frame 1; silence otherwise. Source dead right
        // so itd_l == ITD_MAX_SAMPLES and impulse should appear in left at i=32.
        let mut frame1 = vec![0.0f32; FRAME_LEN];
        frame1[0] = 1.0;
        let frame2 = vec![0.0f32; FRAME_LEN];

        let source = Position::new(10.0, 0.0, 0.0); // listener's right (DX LH)
        let mut out = vec![0.0f32; FRAME_LEN * 2];

        // Two frames — write_idx wraps many times (1920 / 64 = 30).
        state.process_frame(&frame1, &listener, &source, &cfg, &mut out);
        // The impulse response on the left ear should have non-zero energy near
        // sample ITD_MAX_SAMPLES, not at sample 0.
        let early_l: f32 = (0..ITD_MAX_SAMPLES / 2).map(|i| out[2 * i].abs()).sum();
        let around_l: f32 = (ITD_MAX_SAMPLES..ITD_MAX_SAMPLES + 8)
            .map(|i| out[2 * i].abs())
            .sum();
        assert!(
            around_l > early_l,
            "impulse not delayed: early={} around={}",
            early_l,
            around_l
        );

        // Drive a second frame — must not blow up or produce non-finite.
        state.process_frame(&frame2, &listener, &source, &cfg, &mut out);
        for &s in &out {
            assert!(s.is_finite());
        }
    }

    #[test]
    fn mode_off_is_centered_mono() {
        let mut state = SpatialState::new();
        let listener = listener_at_origin();
        let cfg = SpatialConfig {
            mode: SpatialMode::Off,
            min_distance: 100.0,
            max_distance: 5000.0,
        };
        // Source off-center; should still come out balanced.
        let source = Position::new(-10.0, 0.0, 0.0);
        let mut out = vec![0.0f32; FRAME_LEN * 2];

        state.process_frame(&ones_frame(), &listener, &source, &cfg, &mut out);

        for i in 0..FRAME_LEN {
            assert!((out[2 * i] - out[2 * i + 1]).abs() < 1e-6);
        }
    }

    #[test]
    fn mode_pan2d_equals_legacy() {
        // Reproduce the old simple_pan + interleave pipeline and verify Pan2D
        // matches it bit-for-bit on a fixed geometry.
        let listener = listener_at_origin();
        let source = Position::new(-5.0, 0.0, 5.0); // front-right-ish
        let cfg = SpatialConfig {
            mode: SpatialMode::Pan2D,
            min_distance: 100.0,
            max_distance: 5000.0,
        };
        let mono: Vec<f32> = (0..FRAME_LEN).map(|i| ((i as f32) * 0.01).sin()).collect();

        let mut out = vec![0.0f32; FRAME_LEN * 2];
        let mut state = SpatialState::new();
        state.process_frame(&mono, &listener, &source, &cfg, &mut out);

        // Reference path (mirrors the old SpatialProcessor::process).
        let distance = listener.position.distance_to(&source);
        let azimuth = listener.azimuth_to(&source);
        let attenuation = crate::position::transform::distance_attenuation(
            distance,
            cfg.min_distance,
            cfg.max_distance,
        );
        let pan = (azimuth / std::f32::consts::FRAC_PI_2).clamp(-1.0, 1.0);
        let angle = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
        let lg = angle.cos() * attenuation * std::f32::consts::SQRT_2;
        let rg = angle.sin() * attenuation * std::f32::consts::SQRT_2;

        for i in 0..FRAME_LEN {
            let exp_l = mono[i] * lg;
            let exp_r = mono[i] * rg;
            assert!((out[2 * i] - exp_l).abs() < 1e-6);
            assert!((out[2 * i + 1] - exp_r).abs() < 1e-6);
        }
    }
}
