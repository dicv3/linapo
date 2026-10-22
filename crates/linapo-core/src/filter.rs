//! Backend-independent description of a single EQ filter.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::biquad::Coeffs;

/// Q of a Butterworth section; used when a profile does not specify one.
pub const DEFAULT_Q: f64 = std::f64::consts::FRAC_1_SQRT_2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterKind {
    Peaking,
    /// Second-order low shelf (RBJ), Q-controlled.
    LowShelf,
    /// Second-order high shelf (RBJ), Q-controlled.
    HighShelf,
    /// First-order (6 dB/oct) low shelf.
    LowShelf1,
    /// First-order (6 dB/oct) high shelf.
    HighShelf1,
    LowPass,
    HighPass,
    BandPass,
    Notch,
    AllPass,
}

impl FilterKind {
    pub fn short_name(self) -> &'static str {
        match self {
            FilterKind::Peaking => "PK",
            FilterKind::LowShelf => "LSC",
            FilterKind::HighShelf => "HSC",
            FilterKind::LowShelf1 => "LS 6dB",
            FilterKind::HighShelf1 => "HS 6dB",
            FilterKind::LowPass => "LP",
            FilterKind::HighPass => "HP",
            FilterKind::BandPass => "BP",
            FilterKind::Notch => "NO",
            FilterKind::AllPass => "AP",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Filter {
    pub kind: FilterKind,
    pub fc_hz: f64,
    /// Ignored by pass/notch/all-pass kinds.
    #[serde(default)]
    pub gain_db: f64,
    /// Ignored by first-order shelves.
    #[serde(default = "default_q")]
    pub q: f64,
    /// A disabled filter stays in the list (and in the PipeWire graph, so switching it
    /// back on is a live edit) but does nothing.
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

fn default_q() -> f64 {
    DEFAULT_Q
}

impl fmt::Display for Filter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} Fc {} Hz Gain {} dB Q {}", self.kind.short_name(), self.fc_hz, self.gain_db, self.q)?;
        if !self.enabled {
            write!(f, " [off]")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FilterError(pub String);

impl fmt::Display for FilterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FilterError {}

impl Filter {
    /// Compute biquad coefficients for sample rate `fs`, rejecting unusable parameters.
    pub fn coeffs(&self, fs: f64) -> Result<Coeffs, FilterError> {
        if !(fs > 0.0) {
            return Err(FilterError(format!("invalid sample rate {fs}")));
        }
        if !(self.fc_hz > 0.0 && self.fc_hz < fs / 2.0) {
            return Err(FilterError(format!(
                "Fc {} Hz is outside (0, Nyquist = {} Hz)",
                self.fc_hz,
                fs / 2.0
            )));
        }
        if !(self.q > 0.0 && self.q.is_finite()) {
            return Err(FilterError(format!("Q must be positive, got {}", self.q)));
        }
        if !self.gain_db.is_finite() {
            return Err(FilterError("gain must be finite".into()));
        }
        let (f, g, q) = (self.fc_hz, self.gain_db, self.q);
        Ok(match self.kind {
            FilterKind::Peaking => Coeffs::peaking(fs, f, g, q),
            FilterKind::LowShelf => Coeffs::low_shelf(fs, f, g, q),
            FilterKind::HighShelf => Coeffs::high_shelf(fs, f, g, q),
            FilterKind::LowShelf1 => Coeffs::low_shelf_1st(fs, f, g),
            FilterKind::HighShelf1 => Coeffs::high_shelf_1st(fs, f, g),
            FilterKind::LowPass => Coeffs::low_pass(fs, f, q),
            FilterKind::HighPass => Coeffs::high_pass(fs, f, q),
            FilterKind::BandPass => Coeffs::band_pass(fs, f, q),
            FilterKind::Notch => Coeffs::notch(fs, f, q),
            FilterKind::AllPass => Coeffs::all_pass(fs, f, q),
        })
    }
}

/// Approximate Q for a shelf specified by a slope in dB/octave (APO `LSC x dB`).
///
/// Uses the RBJ shelf-slope relation with `S = slope / 12`, so 12 dB/oct gives
/// the Butterworth Q (0.7071). This is an approximation of what Equalizer APO
/// does internally; verify against APO if you depend on odd slopes.
pub fn shelf_q_from_slope(gain_db: f64, slope_db_per_oct: f64) -> f64 {
    let s = slope_db_per_oct / 12.0;
    if !(s > 0.0) || (s - 1.0).abs() < 1e-9 {
        return DEFAULT_Q;
    }
    let a = 10f64.powf(gain_db / 40.0);
    let radicand = (a + 1.0 / a) * (1.0 / s - 1.0) + 2.0;
    if radicand > 0.0 {
        1.0 / radicand.sqrt()
    } else {
        DEFAULT_Q
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(fc: f64, g: f64, q: f64) -> Filter {
        Filter { kind: FilterKind::Peaking, fc_hz: fc, gain_db: g, q, enabled: true }
    }

    #[test]
    fn rejects_frequency_at_or_above_nyquist() {
        assert!(pk(24_000.0, 3.0, 1.0).coeffs(48_000.0).is_err());
        assert!(pk(30_000.0, 3.0, 1.0).coeffs(48_000.0).is_err());
        assert!(pk(20_000.0, 3.0, 1.0).coeffs(48_000.0).is_ok());
        // Same filter is not representable at 32 kHz.
        assert!(pk(20_000.0, 3.0, 1.0).coeffs(32_000.0).is_err());
    }

    #[test]
    fn rejects_bad_q_and_fc() {
        assert!(pk(1000.0, 3.0, 0.0).coeffs(48_000.0).is_err());
        assert!(pk(1000.0, 3.0, -1.0).coeffs(48_000.0).is_err());
        assert!(pk(0.0, 3.0, 1.0).coeffs(48_000.0).is_err());
        assert!(pk(1000.0, f64::NAN, 1.0).coeffs(48_000.0).is_err());
    }

    #[test]
    fn twelve_db_per_octave_slope_is_butterworth_q() {
        for g in [-10.0, -3.0, 0.0, 6.0] {
            assert!((shelf_q_from_slope(g, 12.0) - DEFAULT_Q).abs() < 1e-12);
        }
    }

    #[test]
    fn gentler_slope_gives_lower_q() {
        assert!(shelf_q_from_slope(6.0, 6.0) < DEFAULT_Q);
    }
}
