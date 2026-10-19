//! Equalizer APO `GraphicEQ` support: turn a list of `(frequency, gain dB)` points into a bank
//! of biquad filters (a low shelf, peaking filters, a high shelf) plus a preamp.
//!
//! Equalizer APO builds a linear-phase FIR filter from the points. A biquad bank instead has no
//! latency (an FIR would delay the sound by tens of milliseconds, which hurts lip-sync in videos),
//! and, being ordinary filters, it can be edited, switched off and sent to PipeWire live. The
//! price is that it is an approximation of the magnitude response (the phase differs too);
//! [`Fit`] reports how close it got.
//!
//! The gains are found by Gauss-Newton least squares on a dense log-frequency grid, so
//! neighbouring bands (which overlap) are compensated for each other.

use crate::filter::{Filter, FilterKind};

pub const F_LOW: f64 = 20.0;
pub const F_HIGH: f64 = 20_000.0;

#[derive(Debug, Clone)]
pub struct FitOptions {
    /// Band spacing of the fitted bank. More bands follow fine detail better but cost more CPU.
    pub bands_per_octave: f64,
    /// Band bandwidth as a multiple of the band spacing (wider = smoother, more overlap).
    pub bandwidth_factor: f64,
    /// Sample rate the fit is computed for. Below ~15 kHz the result hardly depends on it.
    pub fs: f64,
}

impl Default for FitOptions {
    fn default() -> Self {
        FitOptions { bands_per_octave: 6.0, bandwidth_factor: 2.0, fs: 48_000.0 }
    }
}

#[derive(Debug, Clone)]
pub struct Fit {
    /// Constant gain taken out of the curve so the bands only carry the shape.
    pub preamp_db: f64,
    pub filters: Vec<Filter>,
    /// Largest and RMS deviation (dB) from the target over 20 Hz - 20 kHz.
    pub max_error_db: f64,
    pub rms_error_db: f64,
}

/// Sort by frequency, drop non-finite values, let a repeated frequency's last value win.
pub fn normalize(points: &[(f64, f64)]) -> Result<Vec<(f64, f64)>, String> {
    let mut p: Vec<(f64, f64)> = points.iter().copied().filter(|(f, g)| f.is_finite() && g.is_finite()).collect();
    if p.is_empty() {
        return Err("GraphicEQ has no usable points".into());
    }
    if p.iter().any(|(f, _)| *f <= 0.0) {
        return Err("GraphicEQ frequencies must be positive".into());
    }
    p.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out: Vec<(f64, f64)> = Vec::with_capacity(p.len());
    for pt in p {
        match out.last_mut() {
            Some(last) if last.0 == pt.0 => *last = pt,
            _ => out.push(pt),
        }
    }
    Ok(out)
}

/// The target curve: linear in dB against log frequency between points, flat (the end values)
/// outside them. `points` must come from [`normalize`].
pub fn target_db(points: &[(f64, f64)], f: f64) -> f64 {
    let (first, last) = (points[0], points[points.len() - 1]);
    if f <= first.0 {
        return first.1;
    }
    if f >= last.0 {
        return last.1;
    }
    let i = points.partition_point(|p| p.0 <= f); // points[i-1].0 <= f < points[i].0
    let (a, b) = (points[i - 1], points[i]);
    let t = (f.log2() - a.0.log2()) / (b.0.log2() - a.0.log2());
    a.1 + t * (b.1 - a.1)
}

fn q_from_octaves(bw: f64) -> f64 {
    let p = 2f64.powf(bw);
    p.sqrt() / (p - 1.0)
}

fn magnitude(f: &Filter, fs: f64, at: f64) -> f64 {
    f.coeffs(fs).map(|c| c.magnitude_db(fs, at)).unwrap_or(0.0)
}

/// Solve `A x = b` in place (Gaussian elimination, partial pivoting). `a` is n x n row-major.
fn solve(a: &mut [f64], b: &mut [f64], n: usize) -> bool {
    for col in 0..n {
        let piv = (col..n).max_by(|&i, &j| a[i * n + col].abs().total_cmp(&a[j * n + col].abs())).unwrap();
        if a[piv * n + col].abs() < 1e-12 {
            return false;
        }
        if piv != col {
            for k in 0..n {
                a.swap(piv * n + k, col * n + k);
            }
            b.swap(piv, col);
        }
        for row in col + 1..n {
            let m = a[row * n + col] / a[col * n + col];
            if m != 0.0 {
                for k in col..n {
                    a[row * n + k] -= m * a[col * n + k];
                }
                b[row] -= m * b[col];
            }
        }
    }
    for row in (0..n).rev() {
        let mut s = b[row];
        for k in row + 1..n {
            s -= a[row * n + k] * b[k];
        }
        b[row] = s / a[row * n + row];
    }
    true
}

/// Fit a filter bank to the points.
pub fn fit(points: &[(f64, f64)], opts: &FitOptions) -> Result<Fit, String> {
    let pts = normalize(points)?;
    let fs = opts.fs;
    let top = F_HIGH.min(0.45 * fs);

    // Evaluation grid: 24 points per octave.
    let n_grid = ((top / F_LOW).log2() * 24.0).ceil() as usize + 1;
    let grid: Vec<f64> = (0..n_grid).map(|i| F_LOW * (top / F_LOW).powf(i as f64 / (n_grid - 1) as f64)).collect();
    let target: Vec<f64> = grid.iter().map(|f| target_db(&pts, *f)).collect();
    let preamp = target.iter().sum::<f64>() / target.len() as f64;
    let goal: Vec<f64> = target.iter().map(|t| t - preamp).collect();

    let error = |filters: &[Filter]| -> (f64, f64) {
        let (mut max, mut sq) = (0.0f64, 0.0);
        for (m, f) in grid.iter().enumerate() {
            let e = goal[m] - filters.iter().map(|flt| magnitude(flt, fs, *f)).sum::<f64>();
            max = max.max(e.abs());
            sq += e * e;
        }
        (max, (sq / grid.len() as f64).sqrt())
    };

    // Band centres across the data range (clamped to the audible band).
    let lo = pts[0].0.clamp(F_LOW, top);
    let hi = pts[pts.len() - 1].0.clamp(F_LOW, top);
    if goal.iter().all(|g| g.abs() < 1e-9) || hi / lo < 1.2 {
        // A flat curve (or one point): the preamp alone is exact.
        let (max, rms) = error(&[]);
        return Ok(Fit { preamp_db: preamp, filters: Vec::new(), max_error_db: max, rms_error_db: rms });
    }
    let n = ((hi / lo).log2() * opts.bands_per_octave).ceil().max(2.0) as usize + 1;
    let centres: Vec<f64> = (0..n).map(|i| lo * (hi / lo).powf(i as f64 / (n - 1) as f64)).collect();
    let q = q_from_octaves(opts.bandwidth_factor / opts.bands_per_octave);
    let mut filters: Vec<Filter> = centres
        .iter()
        .enumerate()
        .map(|(i, c)| Filter {
            kind: if i == 0 {
                FilterKind::LowShelf
            } else if i == n - 1 {
                FilterKind::HighShelf
            } else {
                FilterKind::Peaking
            },
            fc_hz: *c,
            gain_db: target_db(&pts, *c) - preamp,
            q: if i == 0 || i == n - 1 { std::f64::consts::FRAC_1_SQRT_2 } else { q },
            enabled: true,
        })
        .collect();

    // Gauss-Newton on the band gains.
    let delta = 0.05;
    for _ in 0..10 {
        let resp: Vec<f64> = grid.iter().map(|f| filters.iter().map(|flt| magnitude(flt, fs, *f)).sum()).collect();
        let r: Vec<f64> = goal.iter().zip(&resp).map(|(g, a)| g - a).collect();
        // J[m][j] = d response(m) / d gain(j), by finite differences.
        let mut jac = vec![0.0; grid.len() * n];
        for (j, flt) in filters.iter().enumerate() {
            let bumped = Filter { gain_db: flt.gain_db + delta, ..*flt };
            for (m, f) in grid.iter().enumerate() {
                jac[m * n + j] = (magnitude(&bumped, fs, *f) - magnitude(flt, fs, *f)) / delta;
            }
        }
        let mut a = vec![0.0; n * n];
        let mut b = vec![0.0; n];
        for m in 0..grid.len() {
            for i in 0..n {
                let ji = jac[m * n + i];
                if ji == 0.0 {
                    continue;
                }
                b[i] += ji * r[m];
                for k in 0..n {
                    a[i * n + k] += ji * jac[m * n + k];
                }
            }
        }
        for i in 0..n {
            a[i * n + i] += 1e-6;
        }
        if !solve(&mut a, &mut b, n) {
            break;
        }
        let step = b.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        for (flt, d) in filters.iter_mut().zip(&b) {
            flt.gain_db = (flt.gain_db + d).clamp(-30.0, 30.0);
        }
        if step < 1e-4 {
            break;
        }
    }
    let (max, rms) = error(&filters);
    // Bands that ended up (nearly) neutral add nothing but CPU.
    filters.retain(|f| f.gain_db.abs() >= 0.01);
    Ok(Fit { preamp_db: preamp, filters, max_error_db: max, rms_error_db: rms })
}

/// Largest deviation (dB) [`fit_default`] accepts from its smallest bank.
pub const GOOD_ENOUGH_DB: f64 = 0.5;

/// Try small banks first (3, 4, 6, 8 bands per octave, bandwidth twice the spacing) and keep the
/// first one within [`GOOD_ENOUGH_DB`]; if none is, the most accurate one.
pub fn fit_default(points: &[(f64, f64)]) -> Result<Fit, String> {
    let mut best: Option<Fit> = None;
    for bpo in [3.0, 4.0, 6.0, 8.0] {
        let f = fit(points, &FitOptions { bands_per_octave: bpo, ..FitOptions::default() })?;
        if f.max_error_db <= GOOD_ENOUGH_DB {
            return Ok(f);
        }
        if best.as_ref().is_none_or(|b| f.max_error_db < b.max_error_db) {
            best = Some(f);
        }
    }
    Ok(best.expect("at least one candidate"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::{response_db, ChannelChain};

    /// 127 points, ~1/12 octave apart from 20 Hz to 20 kHz.
    fn log_points(f: impl Fn(f64) -> f64) -> Vec<(f64, f64)> {
        (0..127).map(|i| 20.0 * 1000f64.powf(i as f64 / 126.0)).map(|hz| (hz, f(hz))).collect()
    }

    fn bump(hz: f64, centre: f64, width_oct: f64, gain: f64) -> f64 {
        let x = (hz / centre).log2() / width_oct;
        gain * (-x * x).exp()
    }

    /// Something shaped like a headphone correction: bass shelf, a dip, a treble peak.
    fn headphone_like(hz: f64) -> f64 {
        4.0 + bump(hz, 60.0, 1.2, 5.0) + bump(hz, 800.0, 0.8, -3.0) + bump(hz, 3000.0, 0.5, 6.0) + bump(hz, 9000.0, 0.7, -5.0)
    }

    fn achieved(fit: &Fit, hz: f64) -> f64 {
        response_db(&ChannelChain { gain_db: fit.preamp_db, filters: fit.filters.clone() }, 48_000.0, hz)
    }

    #[test]
    fn target_interpolates_linearly_in_log_frequency_and_holds_the_ends() {
        let p = normalize(&[(1000.0, 0.0), (100.0, 10.0), (10_000.0, -10.0)]).unwrap();
        assert_eq!(target_db(&p, 100.0), 10.0);
        assert_eq!(target_db(&p, 1000.0), 0.0);
        assert!((target_db(&p, 316.227766) - 5.0).abs() < 1e-6, "halfway (in log) between 100 and 1000");
        assert!((target_db(&p, 3162.27766) + 5.0).abs() < 1e-6);
        assert_eq!(target_db(&p, 20.0), 10.0, "below the first point: flat at its gain");
        assert_eq!(target_db(&p, 20_000.0), -10.0);
    }

    #[test]
    fn normalize_sorts_dedupes_and_rejects_bad_input() {
        assert_eq!(normalize(&[(200.0, 1.0), (100.0, 2.0), (200.0, 3.0)]).unwrap(), vec![(100.0, 2.0), (200.0, 3.0)]);
        assert_eq!(normalize(&[(f64::NAN, 1.0), (100.0, 2.0)]).unwrap(), vec![(100.0, 2.0)]);
        assert!(normalize(&[]).is_err());
        assert!(normalize(&[(0.0, 1.0)]).is_err());
        assert!(normalize(&[(-5.0, 1.0)]).is_err());
    }

    #[test]
    fn smooth_headphone_like_curve_is_followed_closely_with_few_filters() {
        let pts = log_points(headphone_like);
        let fit = fit_default(&pts).unwrap();
        assert!(fit.max_error_db <= 0.5, "max error {}", fit.max_error_db);
        assert!(fit.rms_error_db <= 0.2, "rms error {}", fit.rms_error_db);
        assert!(fit.filters.len() <= 45, "{} filters", fit.filters.len());
        // check the achieved response against the target at points that are not band centres
        let norm = normalize(&pts).unwrap();
        for hz in [33.0, 77.0, 210.0, 555.0, 1234.0, 2718.0, 4321.0, 7777.0, 13_000.0] {
            let (got, want) = (achieved(&fit, hz), target_db(&norm, hz));
            assert!((got - want).abs() < 0.6, "{hz} Hz: got {got}, want {want}");
        }
    }

    #[test]
    fn a_narrow_peak_is_followed_to_within_one_db() {
        let fit = fit_default(&log_points(|h| headphone_like(h) + bump(h, 6000.0, 0.12, 4.0))).unwrap();
        assert!(fit.max_error_db < 1.0, "max error {}", fit.max_error_db);
    }

    #[test]
    fn the_fit_is_valid_at_every_common_sample_rate() {
        let fit = fit_default(&log_points(headphone_like)).unwrap();
        for fs in [44_100.0, 48_000.0, 88_200.0, 96_000.0, 192_000.0] {
            assert!(fit.filters.iter().all(|f| f.coeffs(fs).is_ok()), "{fs}");
        }
        assert!(fit.filters.iter().all(|f| f.gain_db.abs() <= 30.0));
    }

    #[test]
    fn the_ends_hold_their_gain_beyond_the_given_points() {
        // Points only from 100 Hz to 5 kHz, both ends +6 dB: below and above stay +6.
        let pts = vec![(100.0, 6.0), (1000.0, 0.0), (5000.0, 6.0)];
        let fit = fit_default(&pts).unwrap();
        for hz in [20.0, 40.0, 12_000.0, 19_000.0] {
            assert!((achieved(&fit, hz) - 6.0).abs() < 0.6, "{hz}: {}", achieved(&fit, hz));
        }
    }

    #[test]
    fn extreme_jittery_input_still_gives_a_valid_bounded_fit() {
        let pts: Vec<(f64, f64)> = (0..127).map(|i| (20.0 * 1000f64.powf(i as f64 / 126.0), if i % 2 == 0 { 8.0 } else { -8.0 })).collect();
        let fit = fit_default(&pts).unwrap();
        assert!(fit.max_error_db.is_finite() && fit.filters.iter().all(|f| f.gain_db.is_finite() && f.gain_db.abs() <= 30.0));
        assert!(fit.max_error_db > 1.0, "this cannot be followed by smooth bands and the error says so");
    }

    #[test]
    fn flat_and_constant_curves_need_no_filters() {
        let f = fit_default(&log_points(|_| 0.0)).unwrap();
        assert!(f.filters.is_empty() && f.preamp_db.abs() < 1e-9 && f.max_error_db < 1e-9);
        let f = fit_default(&log_points(|_| -6.0)).unwrap();
        assert!(f.filters.is_empty());
        assert!((f.preamp_db + 6.0).abs() < 1e-9, "a constant offset becomes the preamp");
        assert!(f.max_error_db < 1e-9);
        let f = fit_default(&[(1000.0, 3.0)]).unwrap();
        assert!(f.filters.is_empty() && (f.preamp_db - 3.0).abs() < 1e-9, "a single point is a constant");
    }
}
