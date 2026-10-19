//! Biquad coefficient design (RBJ Audio EQ Cookbook + first-order shelves)
//! and a real-time-safe transposed direct form II processor.
//!
//! All coefficient functions return coefficients normalised so that `a0 == 1`.
//! Nothing in here validates its inputs; use [`crate::filter::Filter::coeffs`]
//! for a checked entry point.

use std::f64::consts::PI;

/// Normalised biquad coefficients: `H(z) = (b0 + b1 z^-1 + b2 z^-2) / (1 + a1 z^-1 + a2 z^-2)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coeffs {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a1: f64,
    pub a2: f64,
}

impl Coeffs {
    /// A filter that passes the signal through unchanged.
    pub const IDENTITY: Coeffs = Coeffs { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 };

    fn normalized(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Self {
        Coeffs { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
    }

    /// Peaking EQ (APO `PK`).
    pub fn peaking(fs: f64, f0: f64, gain_db: f64, q: f64) -> Self {
        let a = 10f64.powf(gain_db / 40.0);
        let (s, c) = (2.0 * PI * f0 / fs).sin_cos();
        let alpha = s / (2.0 * q);
        Self::normalized(
            1.0 + alpha * a,
            -2.0 * c,
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * c,
            1.0 - alpha / a,
        )
    }

    /// Second-order low shelf (APO `LSC`, `LS`).
    pub fn low_shelf(fs: f64, f0: f64, gain_db: f64, q: f64) -> Self {
        let a = 10f64.powf(gain_db / 40.0);
        let (s, c) = (2.0 * PI * f0 / fs).sin_cos();
        let t = 2.0 * a.sqrt() * (s / (2.0 * q));
        Self::normalized(
            a * ((a + 1.0) - (a - 1.0) * c + t),
            2.0 * a * ((a - 1.0) - (a + 1.0) * c),
            a * ((a + 1.0) - (a - 1.0) * c - t),
            (a + 1.0) + (a - 1.0) * c + t,
            -2.0 * ((a - 1.0) + (a + 1.0) * c),
            (a + 1.0) + (a - 1.0) * c - t,
        )
    }

    /// Second-order high shelf (APO `HSC`, `HS`).
    pub fn high_shelf(fs: f64, f0: f64, gain_db: f64, q: f64) -> Self {
        let a = 10f64.powf(gain_db / 40.0);
        let (s, c) = (2.0 * PI * f0 / fs).sin_cos();
        let t = 2.0 * a.sqrt() * (s / (2.0 * q));
        Self::normalized(
            a * ((a + 1.0) + (a - 1.0) * c + t),
            -2.0 * a * ((a - 1.0) + (a + 1.0) * c),
            a * ((a + 1.0) + (a - 1.0) * c - t),
            (a + 1.0) - (a - 1.0) * c + t,
            2.0 * ((a - 1.0) - (a + 1.0) * c),
            (a + 1.0) - (a - 1.0) * c - t,
        )
    }

    /// Second-order low-pass (APO `LP`, `LPQ`).
    pub fn low_pass(fs: f64, f0: f64, q: f64) -> Self {
        let (s, c) = (2.0 * PI * f0 / fs).sin_cos();
        let alpha = s / (2.0 * q);
        Self::normalized((1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0, 1.0 + alpha, -2.0 * c, 1.0 - alpha)
    }

    /// Second-order high-pass (APO `HP`, `HPQ`).
    pub fn high_pass(fs: f64, f0: f64, q: f64) -> Self {
        let (s, c) = (2.0 * PI * f0 / fs).sin_cos();
        let alpha = s / (2.0 * q);
        Self::normalized((1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0, 1.0 + alpha, -2.0 * c, 1.0 - alpha)
    }

    /// Band-pass with 0 dB peak gain (APO `BP`).
    pub fn band_pass(fs: f64, f0: f64, q: f64) -> Self {
        let (s, c) = (2.0 * PI * f0 / fs).sin_cos();
        let alpha = s / (2.0 * q);
        Self::normalized(alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * c, 1.0 - alpha)
    }

    /// Notch (APO `NO`).
    pub fn notch(fs: f64, f0: f64, q: f64) -> Self {
        let (s, c) = (2.0 * PI * f0 / fs).sin_cos();
        let alpha = s / (2.0 * q);
        Self::normalized(1.0, -2.0 * c, 1.0, 1.0 + alpha, -2.0 * c, 1.0 - alpha)
    }

    /// All-pass (APO `AP`).
    pub fn all_pass(fs: f64, f0: f64, q: f64) -> Self {
        let (s, c) = (2.0 * PI * f0 / fs).sin_cos();
        let alpha = s / (2.0 * q);
        Self::normalized(1.0 - alpha, -2.0 * c, 1.0 + alpha, 1.0 + alpha, -2.0 * c, 1.0 - alpha)
    }

    /// First-order (6 dB/oct) low shelf with corner frequency `fc` (APO `LS 6dB`).
    ///
    /// Boosts are built from the allpass-based design of Zölzer (DAFX); cuts are
    /// the exact inverse of the matching boost, so `+g` and `-g` cancel.
    pub fn low_shelf_1st(fs: f64, fc: f64, gain_db: f64) -> Self {
        let (b0, b1, a1) = first_order_boost(fs, fc, gain_db.abs(), true);
        Self::first_order(b0, b1, a1, gain_db < 0.0)
    }

    /// First-order (6 dB/oct) high shelf with corner frequency `fc` (APO `HS 6dB`).
    pub fn high_shelf_1st(fs: f64, fc: f64, gain_db: f64) -> Self {
        let (b0, b1, a1) = first_order_boost(fs, fc, gain_db.abs(), false);
        Self::first_order(b0, b1, a1, gain_db < 0.0)
    }

    fn first_order(b0: f64, b1: f64, a1: f64, invert: bool) -> Self {
        if invert {
            // H_cut = 1 / H_boost: numerator and denominator swap places.
            Coeffs { b0: 1.0 / b0, b1: a1 / b0, b2: 0.0, a1: b1 / b0, a2: 0.0 }
        } else {
            Coeffs { b0, b1, b2: 0.0, a1, a2: 0.0 }
        }
    }

    /// Magnitude response in dB at frequency `f` (Hz) for sample rate `fs`.
    pub fn magnitude_db(&self, fs: f64, f: f64) -> f64 {
        let w = 2.0 * PI * f / fs;
        let (s1, c1) = w.sin_cos();
        let (s2, c2) = (2.0 * w).sin_cos();
        let nr = self.b0 + self.b1 * c1 + self.b2 * c2;
        let ni = -(self.b1 * s1 + self.b2 * s2);
        let dr = 1.0 + self.a1 * c1 + self.a2 * c2;
        let di = -(self.a1 * s1 + self.a2 * s2);
        let power = (nr * nr + ni * ni) / (dr * dr + di * di);
        10.0 * power.max(1e-30).log10()
    }
}

/// Returns `(b0, b1, a1)` of a first-order shelving *boost* of `gain_db >= 0`.
fn first_order_boost(fs: f64, fc: f64, gain_db: f64, low: bool) -> (f64, f64, f64) {
    let v0 = 10f64.powf(gain_db / 20.0);
    let t = (PI * fc / fs).tan();
    let c = (t - 1.0) / (t + 1.0);
    let k = (v0 - 1.0) / 2.0;
    if low {
        (1.0 + k * (1.0 + c), c + k * (1.0 + c), c)
    } else {
        (1.0 + k * (1.0 - c), c + k * (c - 1.0), c)
    }
}

/// A single biquad section with its own state (transposed direct form II, `f64` state).
///
/// `process` never allocates or locks, so it is safe to call from a real-time thread.
#[derive(Debug, Clone)]
pub struct Biquad {
    c: Coeffs,
    z1: f64,
    z2: f64,
}

impl Biquad {
    pub fn new(c: Coeffs) -> Self {
        Biquad { c, z1: 0.0, z2: 0.0 }
    }

    /// Swap in new coefficients without clearing state (avoids clicks on small changes).
    pub fn set_coeffs(&mut self, c: Coeffs) {
        self.c = c;
    }

    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }

    /// Internal state, so a replacement filter can continue without a click.
    pub fn state(&self) -> (f64, f64) {
        (self.z1, self.z2)
    }

    pub fn set_state(&mut self, s: (f64, f64)) {
        self.z1 = s.0;
        self.z2 = s.1;
    }

    #[inline]
    pub fn process(&mut self, x: f64) -> f64 {
        let c = &self.c;
        let y = c.b0 * x + self.z1;
        self.z1 = c.b1 * x - c.a1 * y + self.z2;
        self.z2 = c.b2 * x - c.a2 * y;
        // Flush denormals: they can make recursive filters slow on silence.
        if self.z1.abs() < 1e-30 {
            self.z1 = 0.0;
        }
        if self.z2.abs() < 1e-30 {
            self.z2 = 0.0;
        }
        y
    }

    pub fn process_block(&mut self, buf: &mut [f32]) {
        for s in buf {
            *s = self.process(*s as f64) as f32;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 48_000.0;
    const Q: f64 = std::f64::consts::FRAC_1_SQRT_2;

    fn close(a: f64, b: f64, tol: f64) {
        assert!((a - b).abs() <= tol, "{a} vs {b} (tol {tol})");
    }

    #[test]
    fn peaking_hits_requested_gain_at_centre() {
        for &(f, g, q) in &[(1000.0, 6.0, 1.0), (60.0, -9.5, 4.0), (12_000.0, 3.0, 2.5)] {
            close(Coeffs::peaking(FS, f, g, q).magnitude_db(FS, f), g, 1e-9);
        }
    }

    #[test]
    fn peaking_is_flat_far_from_centre() {
        let c = Coeffs::peaking(FS, 10_000.0, 12.0, 4.0);
        close(c.magnitude_db(FS, 30.0), 0.0, 0.05);
    }

    #[test]
    fn peaking_zero_gain_is_identity_response() {
        let c = Coeffs::peaking(FS, 1000.0, 0.0, 1.0);
        for f in [20.0, 500.0, 1000.0, 15_000.0] {
            close(c.magnitude_db(FS, f), 0.0, 1e-9);
        }
    }

    #[test]
    fn low_shelf_gain_at_dc_and_nyquist() {
        let c = Coeffs::low_shelf(FS, 200.0, 6.0, Q);
        close(c.magnitude_db(FS, 0.0), 6.0, 1e-9);
        close(c.magnitude_db(FS, FS / 2.0), 0.0, 1e-9);
    }

    #[test]
    fn high_shelf_gain_at_dc_and_nyquist() {
        let c = Coeffs::high_shelf(FS, 8000.0, -4.0, Q);
        close(c.magnitude_db(FS, 0.0), 0.0, 1e-9);
        close(c.magnitude_db(FS, FS / 2.0), -4.0, 1e-9);
    }

    #[test]
    fn low_and_high_pass_are_minus_3db_at_corner() {
        close(Coeffs::low_pass(FS, 1000.0, Q).magnitude_db(FS, 1000.0), -3.0103, 1e-3);
        close(Coeffs::high_pass(FS, 1000.0, Q).magnitude_db(FS, 1000.0), -3.0103, 1e-3);
        close(Coeffs::low_pass(FS, 1000.0, Q).magnitude_db(FS, 10.0), 0.0, 1e-3);
        close(Coeffs::high_pass(FS, 1000.0, Q).magnitude_db(FS, 20_000.0), 0.0, 0.05);
    }

    #[test]
    fn band_pass_peaks_at_zero_db() {
        let c = Coeffs::band_pass(FS, 2000.0, 3.0);
        close(c.magnitude_db(FS, 2000.0), 0.0, 1e-9);
        assert!(c.magnitude_db(FS, 200.0) < -15.0);
    }

    #[test]
    fn notch_is_deep_at_centre_and_flat_elsewhere() {
        let c = Coeffs::notch(FS, 1000.0, 5.0);
        assert!(c.magnitude_db(FS, 1000.0) < -80.0);
        close(c.magnitude_db(FS, 50.0), 0.0, 0.05);
    }

    #[test]
    fn all_pass_has_flat_magnitude() {
        let c = Coeffs::all_pass(FS, 3000.0, 1.2);
        for f in [10.0, 300.0, 3000.0, 9000.0, 23_000.0] {
            close(c.magnitude_db(FS, f), 0.0, 1e-9);
        }
    }

    #[test]
    fn first_order_shelves_have_expected_end_gains() {
        for &g in &[6.0, -6.0, 12.5] {
            let lo = Coeffs::low_shelf_1st(FS, 300.0, g);
            close(lo.magnitude_db(FS, 0.0), g, 1e-9);
            close(lo.magnitude_db(FS, FS / 2.0), 0.0, 1e-9);
            assert!(lo.a1.abs() < 1.0, "unstable");

            let hi = Coeffs::high_shelf_1st(FS, 5000.0, g);
            close(hi.magnitude_db(FS, 0.0), 0.0, 1e-9);
            close(hi.magnitude_db(FS, FS / 2.0), g, 1e-9);
            assert!(hi.a1.abs() < 1.0, "unstable");
        }
    }

    #[test]
    fn first_order_boost_and_cut_cancel() {
        let up = Coeffs::low_shelf_1st(FS, 300.0, 7.0);
        let down = Coeffs::low_shelf_1st(FS, 300.0, -7.0);
        for f in [20.0, 100.0, 300.0, 1000.0, 10_000.0] {
            close(up.magnitude_db(FS, f) + down.magnitude_db(FS, f), 0.0, 1e-9);
        }
    }

    #[test]
    fn processing_a_sine_matches_the_analytic_response() {
        let c = Coeffs::peaking(FS, 1000.0, 6.0, 1.0);
        let mut bq = Biquad::new(c);
        let n = 48_000;
        let mut buf: Vec<f32> =
            (0..n).map(|i| (2.0 * PI * 1000.0 * i as f64 / FS).sin() as f32 * 0.25).collect();
        let reference = buf.clone();
        bq.process_block(&mut buf);
        let rms = |x: &[f32]| (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt();
        let measured = 20.0 * (rms(&buf[4800..]) / rms(&reference[4800..])).log10();
        close(measured, 6.0, 0.01);
    }

    #[test]
    fn identity_passes_signal_unchanged() {
        let mut bq = Biquad::new(Coeffs::IDENTITY);
        let mut buf = [0.1f32, -0.5, 0.9, 0.0];
        let orig = buf;
        bq.process_block(&mut buf);
        assert_eq!(buf, orig);
    }
}
