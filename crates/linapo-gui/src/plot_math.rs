//! Math behind the frequency-response plot: curve sampling, draggable handles, scroll-to-Q.
//! Plot x coordinates are `log10(frequency Hz)`, y is dB.

use linapo_core::chain::{response_db, ChannelChain};
use linapo_core::Filter;

use crate::model::{clamp_filter, uses_gain};

/// Sample rate the plot is drawn for. Responses below ~20 kHz are nearly rate independent.
pub const FS: f64 = 48_000.0;
pub const F_MIN: f64 = 20.0;
pub const F_MAX: f64 = 20_000.0;

pub fn x_of(freq: f64) -> f64 {
    freq.log10()
}

pub fn freq_of(x: f64) -> f64 {
    10f64.powf(x)
}

/// `[log10 f, dB]` points, log-spaced from 20 Hz to 20 kHz.
pub fn curve(chain: &ChannelChain, n: usize) -> Vec<[f64; 2]> {
    let (lo, hi) = (x_of(F_MIN), x_of(F_MAX));
    (0..n)
        .map(|i| {
            let x = lo + (hi - lo) * i as f64 / (n - 1) as f64;
            [x, response_db(chain, FS, freq_of(x))]
        })
        .collect()
}

/// Where a filter's drag handle sits. Gain filters sit at (Fc, gain); the others sit on
/// their own response at Fc (so the handle is on the curve and only moves horizontally).
pub fn handle_position(f: &Filter) -> [f64; 2] {
    let y = if uses_gain(f.kind) {
        f.gain_db
    } else {
        f.coeffs(FS).map(|c| c.magnitude_db(FS, f.fc_hz)).unwrap_or(0.0)
    };
    [x_of(f.fc_hz), y]
}

/// Move a filter to a dragged plot position. Non-gain filters only take the frequency.
pub fn apply_drag(f: &mut Filter, pos: [f64; 2]) {
    f.fc_hz = freq_of(pos[0]);
    if uses_gain(f.kind) {
        f.gain_db = pos[1];
    }
    clamp_filter(f);
}

/// Index (the tag) of the handle closest to `p` within `radius`, in screen units.
pub fn nearest_handle(handles: &[(usize, [f32; 2])], p: [f32; 2], radius: f32) -> Option<usize> {
    handles
        .iter()
        .map(|(i, h)| (*i, ((h[0] - p[0]).powi(2) + (h[1] - p[1]).powi(2)).sqrt()))
        .filter(|(_, d)| *d <= radius)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i)
}

/// New Q after `scroll` points of mouse wheel (positive = up = narrower... sharper).
pub fn scroll_q(q: f64, scroll: f32) -> f64 {
    (q * (scroll as f64 * 0.003).exp()).clamp(0.1, 30.0)
}

/// "20", "500", "1k", "2.5k", "20k".
pub fn freq_label(hz: f64) -> String {
    if hz >= 1000.0 {
        let k = hz / 1000.0;
        if (k - k.round()).abs() < 0.05 {
            format!("{}k", k.round() as i64)
        } else {
            format!("{k:.1}k")
        }
    } else if (hz - hz.round()).abs() < 0.05 {
        format!("{}", hz.round() as i64)
    } else {
        format!("{hz:.1}")
    }
}

/// Grid lines for the log axis: `(x, major)` for every 1..9 x 10^k inside `[x_min, x_max]`.
pub fn grid_marks(x_min: f64, x_max: f64) -> Vec<(f64, bool)> {
    let mut out = Vec::new();
    let mut decade = x_min.floor() as i32;
    while (decade as f64) <= x_max.ceil() {
        for m in 1..=9 {
            let x = (m as f64 * 10f64.powi(decade)).log10();
            if x >= x_min - 1e-9 && x <= x_max + 1e-9 {
                out.push((x, m == 1));
            }
        }
        decade += 1;
    }
    out
}

/// Symmetric dB range, at least +-12 and at most +-36, in steps of 6. Curve points beyond
/// +-18 dB (a low-pass's deep slope, say) are treated as off-scale and do not stretch the
/// plot; `extra` values (filter handles) always count.
pub fn y_range(curves: &[&[[f64; 2]]], extra: &[f64]) -> f64 {
    let m = curves
        .iter()
        .flat_map(|c| c.iter().map(|p| p[1].abs()).filter(|v| *v <= 18.0))
        .chain(extra.iter().map(|v| v.abs()))
        .fold(0.0f64, f64::max);
    ((m / 6.0).ceil() * 6.0 + 3.0).clamp(12.0, 36.0).max(12.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use linapo_core::FilterKind;

    fn pk(fc: f64, g: f64) -> Filter {
        Filter { kind: FilterKind::Peaking, fc_hz: fc, gain_db: g, q: 1.0, enabled: true }
    }

    #[test]
    fn curve_is_log_spaced_and_reflects_the_chain() {
        let chain = ChannelChain { gain_db: -2.0, filters: vec![pk(1000.0, 6.0)] };
        let c = curve(&chain, 201);
        assert_eq!(c.len(), 201);
        assert!((c[0][0] - 20f64.log10()).abs() < 1e-12 && (c[200][0] - 20_000f64.log10()).abs() < 1e-12);
        let at_1k = c.iter().min_by(|a, b| (a[0] - 3.0).abs().total_cmp(&(b[0] - 3.0).abs())).unwrap();
        assert!((at_1k[1] - 4.0).abs() < 0.05, "{at_1k:?}");
    }

    #[test]
    fn handles_for_gain_and_non_gain_filters() {
        let h = handle_position(&pk(1000.0, 5.0));
        assert!((h[0] - 3.0).abs() < 1e-12 && h[1] == 5.0);
        let lp = Filter { kind: FilterKind::LowPass, fc_hz: 1000.0, gain_db: 9.0, q: std::f64::consts::FRAC_1_SQRT_2, enabled: true };
        let h = handle_position(&lp);
        assert!((h[1] + 3.01).abs() < 0.01, "low-pass handle sits on the -3 dB point, got {h:?}");
    }

    #[test]
    fn dragging_moves_fc_and_gain_but_respects_kind() {
        let mut f = pk(1000.0, 0.0);
        apply_drag(&mut f, [x_of(2000.0), -4.0]);
        assert!((f.fc_hz - 2000.0).abs() < 1e-6 && f.gain_db == -4.0);
        let mut lp = Filter { kind: FilterKind::LowPass, fc_hz: 1000.0, gain_db: 0.0, q: 0.7, enabled: true };
        apply_drag(&mut lp, [x_of(500.0), 8.0]);
        assert!((lp.fc_hz - 500.0).abs() < 1e-6 && lp.gain_db == 0.0, "gain is not used by a low-pass");
    }

    #[test]
    fn dragging_past_the_edges_is_clamped() {
        let mut f = pk(1000.0, 0.0);
        apply_drag(&mut f, [x_of(1e6), 500.0]);
        assert_eq!((f.fc_hz, f.gain_db), (20_000.0, 30.0));
        apply_drag(&mut f, [x_of(0.001), -500.0]);
        assert_eq!((f.fc_hz, f.gain_db), (10.0, -30.0));
        assert!(f.coeffs(44_100.0).is_ok());
    }

    #[test]
    fn nearest_handle_picks_the_closest_within_radius() {
        let hs = [(0, [100.0, 100.0]), (1, [110.0, 100.0]), (2, [300.0, 50.0])];
        assert_eq!(nearest_handle(&hs, [108.0, 101.0], 14.0), Some(1));
        assert_eq!(nearest_handle(&hs, [103.0, 100.0], 14.0), Some(0));
        assert_eq!(nearest_handle(&hs, [200.0, 200.0], 14.0), None);
        assert_eq!(nearest_handle(&[], [0.0, 0.0], 14.0), None);
    }

    #[test]
    fn scroll_changes_q_smoothly_in_both_directions_and_clamps() {
        assert!(scroll_q(1.0, 50.0) > 1.1 && scroll_q(1.0, -50.0) < 0.9);
        assert!((scroll_q(scroll_q(1.0, 40.0), -40.0) - 1.0).abs() < 1e-9);
        assert_eq!(scroll_q(29.0, 5000.0), 30.0);
        assert_eq!(scroll_q(0.2, -5000.0), 0.1);
        assert_eq!(scroll_q(1.0, 0.0), 1.0);
    }

    #[test]
    fn frequency_labels() {
        assert_eq!(freq_label(20.0), "20");
        assert_eq!(freq_label(500.0), "500");
        assert_eq!(freq_label(1000.0), "1k");
        assert_eq!(freq_label(2500.0), "2.5k");
        assert_eq!(freq_label(20_000.0), "20k");
    }

    #[test]
    fn grid_marks_cover_the_audio_band_with_decades_major() {
        let g = grid_marks(x_of(20.0), x_of(20_000.0));
        let majors: Vec<f64> = g.iter().filter(|(_, m)| *m).map(|(x, _)| freq_of(*x).round()).collect();
        assert_eq!(majors, vec![100.0, 1000.0, 10_000.0]);
        assert!(g.iter().any(|(x, _)| (freq_of(*x) - 50.0).abs() < 1e-6));
        assert!(g.iter().all(|(x, _)| *x >= x_of(20.0) - 1e-9 && *x <= x_of(20_000.0) + 1e-9));
    }

    #[test]
    fn y_range_grows_with_the_data() {
        let small = vec![[0.0, 3.0], [1.0, -2.0]];
        let big = vec![[0.0, 15.0]];
        assert_eq!(y_range(&[&small], &[]), 12.0);
        assert_eq!(y_range(&[&big], &[]), 21.0);
        assert_eq!(y_range(&[&[[0.0, 20.0]]], &[]), 12.0, "off-scale");
        assert_eq!(y_range(&[&[[0.0, 500.0]]], &[]), 12.0, "off-scale curve points are ignored");
        assert_eq!(y_range(&[], &[500.0]), 36.0, "but a handle that far out still counts");
        assert_eq!(y_range(&[&small], &[18.0]), 21.0);
    }
}
