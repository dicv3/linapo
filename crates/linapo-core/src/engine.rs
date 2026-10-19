//! Backend-independent real-time processing core.
//!
//! `Engine::process` never allocates, locks or does I/O, so a PipeWire (or any
//! other) callback can call it directly. Build a new `Engine` on a control
//! thread, then hand it over with [`crate::handoff::Handoff`] and call
//! [`Engine::inherit_state`] on the audio thread so edits do not click.

use crate::chain::EqChain;
use crate::matrix::db_to_lin;
use crate::setup::Setup;

/// Linear peak ceiling of the limiter (-0.1 dBFS).
const LIMITER_CEILING: f32 = 0.988_553_1;
const LIMITER_RELEASE_SECS: f64 = 0.100;

#[derive(Debug, Clone)]
struct Limiter {
    ceiling: f32,
    release: f32, // per-sample smoothing coefficient
    gain: f32,
}

impl Limiter {
    fn new(fs: f64) -> Self {
        Limiter {
            ceiling: LIMITER_CEILING,
            release: (-1.0 / (fs * LIMITER_RELEASE_SECS)).exp() as f32,
            gain: 1.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Engine {
    fs: f64,
    n_in: usize,
    n_out: usize,
    matrix_tgt: Vec<f32>, // [in * n_out + out]
    matrix_cur: Vec<f32>,
    eq: Vec<EqChain>,
    master_tgt: f32,
    master_cur: f32,
    limiter: Option<Limiter>,
}

impl Engine {
    /// Build an engine for sample rate `fs`. Returns warnings for filters that
    /// had to be skipped (e.g. above Nyquist at this rate).
    pub fn new(setup: &Setup, fs: f64) -> Result<(Engine, Vec<String>), String> {
        setup.validate().map_err(|e| e.to_string())?;
        let (n_in, n_out) = (setup.input_channels.len(), setup.output_channels.len());
        let matrix: Vec<f32> = setup.matrix.as_slice().iter().map(|g| *g as f32).collect();
        let mut warnings = Vec::new();
        let eq = setup
            .output_channels
            .iter()
            .map(|c| {
                let (chain, w) = EqChain::build(&setup.chain_for_output(c), fs);
                warnings.extend(w.into_iter().map(|m| format!("channel {c}: {m}")));
                chain
            })
            .collect();
        let master = db_to_lin(setup.master_gain_db) as f32;
        let engine = Engine {
            fs,
            n_in,
            n_out,
            matrix_cur: matrix.clone(),
            matrix_tgt: matrix,
            eq,
            master_tgt: master,
            master_cur: master,
            limiter: setup.limiter.then(|| Limiter::new(fs)),
        };
        Ok((engine, warnings))
    }

    pub fn n_in(&self) -> usize {
        self.n_in
    }

    pub fn n_out(&self) -> usize {
        self.n_out
    }

    pub fn sample_rate(&self) -> f64 {
        self.fs
    }

    /// Continue from `old`'s running state so a live edit is smooth: matrix and
    /// master gain ramp from their previous values, filters keep their history
    /// (when their structure is unchanged), and the limiter keeps its envelope.
    /// Does nothing if the channel counts or sample rate differ.
    pub fn inherit_state(&mut self, old: &Engine) {
        if self.n_in != old.n_in || self.n_out != old.n_out || self.fs != old.fs {
            return;
        }
        self.matrix_cur.copy_from_slice(&old.matrix_cur);
        self.master_cur = old.master_cur;
        for (n, o) in self.eq.iter_mut().zip(&old.eq) {
            n.inherit_state(o);
        }
        if let (Some(n), Some(o)) = (self.limiter.as_mut(), old.limiter.as_ref()) {
            n.gain = o.gain;
        }
    }

    /// Process one block. `outputs[0].len()` (the shortest output) sets the
    /// number of frames; an input that is missing or shorter is treated as silence.
    pub fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        let frames = outputs.iter().map(|o| o.len()).min().unwrap_or(0);
        if frames == 0 {
            return;
        }
        let n_out = self.n_out.min(outputs.len());

        for o in 0..n_out {
            let out = &mut outputs[o][..frames];
            out.fill(0.0);
            for i in 0..self.n_in {
                let idx = i * self.n_out + o;
                let (cur, tgt) = (self.matrix_cur[idx], self.matrix_tgt[idx]);
                let inp = match inputs.get(i) {
                    Some(inp) if inp.len() >= frames => &inp[..frames],
                    _ => {
                        self.matrix_cur[idx] = tgt; // silent input: nothing to smooth
                        continue;
                    }
                };
                if cur == 0.0 && tgt == 0.0 {
                    continue;
                }
                if cur == tgt {
                    for (y, x) in out.iter_mut().zip(inp) {
                        *y += x * tgt;
                    }
                } else {
                    let step = (tgt - cur) / frames as f32;
                    let mut g = cur;
                    for (y, x) in out.iter_mut().zip(inp) {
                        g += step;
                        *y += x * g;
                    }
                    self.matrix_cur[idx] = tgt;
                }
            }
            self.eq[o].process_block(out);
        }

        // Master gain, ramped across the block when it changed.
        let (mc, mt) = (self.master_cur, self.master_tgt);
        if mc != mt || mt != 1.0 {
            let step = (mt - mc) / frames as f32;
            for out in outputs.iter_mut().take(n_out) {
                let mut g = mc;
                for y in out[..frames].iter_mut() {
                    g += step;
                    *y *= g;
                }
            }
            self.master_cur = mt;
        }

        // Linked peak limiter: gain never exceeds ceiling / peak, so the output
        // cannot exceed the ceiling.
        if let Some(lim) = self.limiter.as_mut() {
            for k in 0..frames {
                let mut peak = 0f32;
                for out in outputs.iter().take(n_out) {
                    peak = peak.max(out[k].abs());
                }
                let need = if peak > lim.ceiling { lim.ceiling / peak } else { 1.0 };
                let released = 1.0 - (1.0 - lim.gain) * lim.release;
                lim.gain = need.min(released);
                if lim.gain < 1.0 {
                    for out in outputs.iter_mut().take(n_out) {
                        out[k] *= lim.gain;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apo::{layout_for, parse_str, Channel};
    use crate::filter::{Filter, FilterKind};
    use crate::matrix::Matrix;
    use crate::setup::EqPreset;
    use std::f64::consts::PI;

    const FS: f64 = 48_000.0;

    fn ch(s: &str) -> Channel {
        Channel::parse(s).unwrap()
    }

    fn sine(f: f64, amp: f32, n: usize) -> Vec<f32> {
        (0..n).map(|i| (2.0 * PI * f * i as f64 / FS).sin() as f32 * amp).collect()
    }

    /// Run `inputs` through the engine in blocks of `block` frames.
    fn run(e: &mut Engine, inputs: &[Vec<f32>], block: usize) -> Vec<Vec<f32>> {
        let n = inputs[0].len();
        let mut out = vec![vec![0f32; n]; e.n_out()];
        let mut pos = 0;
        while pos < n {
            let end = (pos + block).min(n);
            let ins: Vec<&[f32]> = inputs.iter().map(|v| &v[pos..end]).collect();
            let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|v| &mut v[pos..end]).collect();
            e.process(&ins, &mut outs);
            pos = end;
        }
        out
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
    }

    fn engine(setup: &Setup) -> Engine {
        Engine::new(setup, FS).unwrap().0
    }

    fn quiet(layout: usize) -> Setup {
        let mut s = Setup::passthrough(&layout_for(layout));
        s.limiter = false;
        s
    }

    #[test]
    fn passthrough_is_bit_exact() {
        let mut e = engine(&quiet(2));
        let a = sine(440.0, 0.5, 1000);
        let b = sine(660.0, 0.3, 1000);
        let out = run(&mut e, &[a.clone(), b.clone()], 256);
        assert_eq!(out[0], a);
        assert_eq!(out[1], b);
    }

    #[test]
    fn matrix_can_swap_channels() {
        let mut s = quiet(2);
        s.matrix = Matrix::zeros(2, 2);
        s.matrix.set(0, 1, 1.0);
        s.matrix.set(1, 0, 1.0);
        let mut e = engine(&s);
        let (l, r) = (sine(300.0, 0.5, 512), sine(900.0, 0.25, 512));
        let out = run(&mut e, &[l.clone(), r.clone()], 128);
        assert_eq!(out[0], r);
        assert_eq!(out[1], l);
    }

    #[test]
    fn matrix_mixes_and_applies_per_route_volume() {
        // 3 in -> 1 out:  out = 0.5*a + 0.25*b - 1*c
        let mut s = quiet(1);
        s.input_channels = vec![ch("L"), ch("R"), ch("C")];
        s.output_channels = vec![ch("C")];
        s.matrix = Matrix::zeros(3, 1);
        s.matrix.set(0, 0, 0.5);
        s.matrix.set(1, 0, 0.25);
        s.matrix.set(2, 0, -1.0);
        let mut e = engine(&s);
        let out = run(&mut e, &[vec![1.0; 64], vec![1.0; 64], vec![0.5; 64]], 64);
        assert!(out[0].iter().all(|v| (*v - (0.5 + 0.25 - 0.5)).abs() < 1e-6));
    }

    #[test]
    fn route_volume_in_db() {
        let mut s = quiet(2);
        s.matrix.set_db(0, 0, -6.0206);
        let mut e = engine(&s);
        let out = run(&mut e, &[vec![1.0; 64], vec![1.0; 64]], 64);
        assert!((out[0][10] - 0.5).abs() < 1e-4);
        assert_eq!(out[1][10], 1.0);
    }

    #[test]
    fn stereo_to_5_1_upmix() {
        let mut s = quiet(6);
        s.input_channels = layout_for(2);
        s.matrix = Matrix::zeros(2, 6);
        s.matrix.set(0, 0, 1.0); // L -> L
        s.matrix.set(1, 1, 1.0); // R -> R
        s.matrix.set(0, 4, 0.5); // L -> RL
        s.matrix.set(1, 5, 0.5); // R -> RR
        s.matrix.set(0, 3, 0.5); // L+R -> SUB
        s.matrix.set(1, 3, 0.5);
        s.validate().unwrap();
        let mut e = engine(&s);
        assert_eq!((e.n_in(), e.n_out()), (2, 6));
        let out = run(&mut e, &[vec![1.0; 32], vec![0.5; 32]], 32);
        assert_eq!(out[0][5], 1.0);
        assert_eq!(out[1][5], 0.5);
        assert_eq!(out[2][5], 0.0, "C is unrouted");
        assert_eq!(out[3][5], 0.75);
        assert_eq!(out[4][5], 0.5);
        assert_eq!(out[5][5], 0.25);
    }

    #[test]
    fn eq_only_affects_assigned_channels() {
        let mut s = quiet(2);
        s.eqs.push(EqPreset {
            name: "right only".into(),
            enabled: true,
            preamp_db: -6.0206,
            filters: vec![],
            channels: vec![ch("R")],
        });
        let mut e = engine(&s);
        let out = run(&mut e, &[vec![1.0; 64], vec![1.0; 64]], 64);
        assert_eq!(out[0][3], 1.0);
        assert!((out[1][3] - 0.5).abs() < 1e-4);
    }

    #[test]
    fn eq_gain_matches_response_through_the_engine() {
        let mut s = quiet(2);
        s.eqs.push(EqPreset {
            name: "pk".into(),
            enabled: true,
            preamp_db: 0.0,
            filters: vec![Filter { kind: FilterKind::Peaking, fc_hz: 1000.0, gain_db: 6.0, q: 1.0, enabled: true }],
            channels: vec![ch("L"), ch("R")], // one EQ, many channels
        });
        let mut e = engine(&s);
        let x = sine(1000.0, 0.2, 48_000);
        let out = run(&mut e, &[x.clone(), x.clone()], 480);
        for c in 0..2 {
            let db = 20.0 * (rms(&out[c][9600..]) / rms(&x[9600..])).log10();
            assert!((db - 6.0).abs() < 0.02, "channel {c}: {db}");
        }
    }

    #[test]
    fn shared_eq_keeps_independent_state_per_channel() {
        let mut s = quiet(2);
        s.eqs.push(EqPreset {
            name: "pk".into(),
            enabled: true,
            preamp_db: 0.0,
            filters: vec![Filter { kind: FilterKind::Peaking, fc_hz: 200.0, gain_db: 9.0, q: 1.0, enabled: true }],
            channels: vec![ch("L"), ch("R")],
        });
        let mut e = engine(&s);
        let mut imp = vec![0f32; 256];
        imp[0] = 1.0;
        let out = run(&mut e, &[imp, vec![0.0; 256]], 64);
        assert!(out[1].iter().all(|v| *v == 0.0), "R must not hear L's impulse response");
        assert!(out[0].iter().any(|v| *v != 0.0));
    }

    #[test]
    fn master_gain() {
        let mut s = quiet(2);
        s.master_gain_db = -6.0206;
        let mut e = engine(&s);
        let out = run(&mut e, &[vec![0.8; 64], vec![0.8; 64]], 64);
        assert!((out[0][1] - 0.4).abs() < 1e-4 && (out[1][63] - 0.4).abs() < 1e-4);
    }

    #[test]
    fn limiter_never_exceeds_ceiling_and_leaves_quiet_signals_alone() {
        let mut s = Setup::passthrough(&layout_for(2));
        s.master_gain_db = 12.0;
        let mut e = engine(&s);
        let loud = sine(200.0, 0.9, 20_000);
        let out = run(&mut e, &[loud.clone(), loud], 480);
        let peak = out.iter().flatten().fold(0f32, |m, v| m.max(v.abs()));
        assert!(peak <= LIMITER_CEILING + 1e-6, "peak {peak}");
        assert!(peak > 0.9, "limiter should not crush the signal: {peak}");

        let mut s = Setup::passthrough(&layout_for(2));
        s.master_gain_db = 0.0;
        let mut e = engine(&s);
        let soft = sine(200.0, 0.3, 4800);
        let out = run(&mut e, &[soft.clone(), soft.clone()], 480);
        assert_eq!(out[0], soft);
    }

    #[test]
    fn result_does_not_depend_on_block_size() {
        let (s, _) = Setup::from_profile(
            &parse_str("Preamp: -3 dB\nFilter: ON PK Fc 1000 Hz Gain 5 dB Q 2\nFilter: ON LSC Fc 120 Hz Gain 4 dB Q 0.7\nCopy: L=0.7*L+0.3*R\n"),
            &layout_for(2),
            "t",
        );
        let x = [sine(110.0, 0.4, 5000), sine(1000.0, 0.3, 5000)];
        let a = run(&mut engine(&s), &x, 5000);
        let b = run(&mut engine(&s), &x, 64);
        let c = run(&mut engine(&s), &x, 333);
        for ch in 0..2 {
            for i in 0..5000 {
                assert!((a[ch][i] - b[ch][i]).abs() < 1e-5 && (a[ch][i] - c[ch][i]).abs() < 1e-5);
            }
        }
    }

    #[test]
    fn live_matrix_change_ramps_smoothly() {
        let mut quiet_setup = quiet(2);
        quiet_setup.matrix.set(0, 0, 0.0);
        let mut old = engine(&quiet_setup);
        let _ = run(&mut old, &[vec![1.0; 480], vec![1.0; 480]], 480);

        let mut new = engine(&quiet(2)); // gain 1.0 on L->L
        new.inherit_state(&old);
        let out = run(&mut new, &[vec![1.0; 480], vec![1.0; 480]], 480);
        let l = &out[0];
        assert!(l[0] < 0.01, "must start near the old gain, got {}", l[0]);
        assert!((l[479] - 1.0).abs() < 1e-3);
        let max_step = l.windows(2).map(|w| w[1] - w[0]).fold(0f32, f32::max);
        assert!(max_step < 0.003 && l.windows(2).all(|w| w[1] >= w[0]), "ramp must be monotonic and gentle");
        // afterwards it is steady
        let out = run(&mut new, &[vec![1.0; 64], vec![1.0; 64]], 64);
        assert!(out[0].iter().all(|v| *v == 1.0));
    }

    #[test]
    fn inheriting_filter_state_avoids_a_switching_transient() {
        let mk = |gain| {
            let mut s = quiet(2);
            s.eqs.push(EqPreset {
                name: "pk".into(),
                enabled: true,
                preamp_db: 0.0,
                filters: vec![Filter { kind: FilterKind::Peaking, fc_hz: 500.0, gain_db: gain, q: 1.0, enabled: true }],
                channels: vec![ch("L")],
            });
            s
        };
        let x = sine(500.0, 0.5, 4800);
        let silence = vec![0.0f32; 4800];

        // Reference: the new filter running the whole time.
        let reference = run(&mut engine(&mk(6.0)), &[x.clone(), silence.clone()], 480);

        // Old engine for the first half, then rebuild with the same filter (as when
        // some *other* part of the setup is edited) and carry on.
        let mut old = engine(&mk(6.0));
        let _ = run(&mut old, &[x[..2400].to_vec(), silence[..2400].to_vec()], 480);
        let tail = [x[2400..].to_vec(), silence[2400..].to_vec()];

        let mut with = engine(&mk(6.0));
        with.inherit_state(&old);
        let a = run(&mut with, &tail, 480);
        let b = run(&mut engine(&mk(6.0)), &tail, 480); // cold start

        let err = |got: &[f32]| (0..200).map(|i| (got[i] - reference[0][2400 + i]).abs()).fold(0f32, f32::max);
        let (e_with, e_without) = (err(&a[0]), err(&b[0]));
        assert!(e_with < 1e-5, "inherited state must continue seamlessly: {e_with}");
        assert!(e_without > 0.01, "a cold start should show a transient: {e_without}");
    }

    #[test]
    fn inherit_is_ignored_when_shapes_differ() {
        let old = engine(&quiet(2));
        let mut new = engine(&quiet(6));
        new.inherit_state(&old); // must not panic
        assert_eq!(new.n_out(), 6);
    }

    #[test]
    fn missing_or_short_inputs_are_silence() {
        let mut e = engine(&quiet(2));
        let mut l = vec![9.0f32; 64];
        let mut r = vec![9.0f32; 64];
        let one = vec![1.0f32; 64];
        e.process(&[&one], &mut [&mut l, &mut r]);
        assert!(l.iter().all(|v| *v == 1.0));
        assert!(r.iter().all(|v| *v == 0.0));
        let short = vec![1.0f32; 10];
        e.process(&[&short, &short], &mut [&mut l, &mut r]);
        assert!(l.iter().all(|v| *v == 0.0));
    }

    #[test]
    fn invalid_setup_is_rejected() {
        let mut s = quiet(2);
        s.matrix = Matrix::identity(3, 3);
        assert!(Engine::new(&s, FS).is_err());
    }

    #[test]
    fn unusable_filters_are_skipped_with_a_warning() {
        let mut s = quiet(2);
        s.eqs.push(EqPreset {
            name: "hi".into(),
            enabled: true,
            preamp_db: 0.0,
            filters: vec![Filter { kind: FilterKind::Peaking, fc_hz: 20_000.0, gain_db: 3.0, q: 1.0, enabled: true }],
            channels: vec![ch("L")],
        });
        let (_, w) = Engine::new(&s, 32_000.0).unwrap();
        assert_eq!(w.len(), 1);
    }
}
