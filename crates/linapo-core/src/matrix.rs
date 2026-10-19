//! Channel matrix: `gain[input][output]`, stored as *linear* gains.
//!
//! `0.0` means "not routed". Negative values invert polarity (APO `Copy`
//! allows that). Use [`db_to_lin`] / [`lin_to_db`] at the UI boundary.

use serde::{Deserialize, Serialize};

pub fn db_to_lin(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

/// `None` for an unrouted (zero) gain.
pub fn lin_to_db(g: f64) -> Option<f64> {
    if g == 0.0 {
        None
    } else {
        Some(20.0 * g.abs().log10())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "Vec<Vec<f64>>", into = "Vec<Vec<f64>>")]
pub struct Matrix {
    n_in: usize,
    n_out: usize,
    gains: Vec<f64>, // row-major [in][out]
}

impl Matrix {
    pub fn zeros(n_in: usize, n_out: usize) -> Self {
        Matrix { n_in, n_out, gains: vec![0.0; n_in * n_out] }
    }

    /// Input `i` goes to output `i` at unity gain; extra channels are left unrouted.
    pub fn identity(n_in: usize, n_out: usize) -> Self {
        let mut m = Self::zeros(n_in, n_out);
        for i in 0..n_in.min(n_out) {
            m.set(i, i, 1.0);
        }
        m
    }

    pub fn n_in(&self) -> usize {
        self.n_in
    }

    pub fn n_out(&self) -> usize {
        self.n_out
    }

    pub fn get(&self, input: usize, output: usize) -> f64 {
        self.gains[input * self.n_out + output]
    }

    pub fn set(&mut self, input: usize, output: usize, gain: f64) {
        assert!(input < self.n_in && output < self.n_out, "matrix index out of range");
        self.gains[input * self.n_out + output] = gain;
    }

    pub fn set_db(&mut self, input: usize, output: usize, db: f64) {
        self.set(input, output, db_to_lin(db));
    }

    /// Remove a route.
    pub fn clear(&mut self, input: usize, output: usize) {
        self.set(input, output, 0.0);
    }

    pub fn gain_db(&self, input: usize, output: usize) -> Option<f64> {
        lin_to_db(self.get(input, output))
    }

    /// Row-major `[in * n_out + out]` slice, the layout the engine uses.
    pub fn as_slice(&self) -> &[f64] {
        &self.gains
    }
}

impl TryFrom<Vec<Vec<f64>>> for Matrix {
    type Error = String;

    fn try_from(rows: Vec<Vec<f64>>) -> Result<Self, String> {
        let n_in = rows.len();
        let n_out = rows.first().map_or(0, Vec::len);
        if let Some(bad) = rows.iter().position(|r| r.len() != n_out) {
            return Err(format!("matrix row {bad} has {} entries, expected {n_out}", rows[bad].len()));
        }
        Ok(Matrix { n_in, n_out, gains: rows.into_iter().flatten().collect() })
    }
}

impl From<Matrix> for Vec<Vec<f64>> {
    fn from(m: Matrix) -> Self {
        if m.n_out == 0 {
            return vec![Vec::new(); m.n_in];
        }
        m.gains.chunks(m.n_out).map(<[f64]>::to_vec).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_and_access() {
        let m = Matrix::identity(3, 2);
        assert_eq!((m.n_in(), m.n_out()), (3, 2));
        assert_eq!(m.get(0, 0), 1.0);
        assert_eq!(m.get(1, 1), 1.0);
        assert_eq!(m.get(2, 0), 0.0);
        assert_eq!(m.get(2, 1), 0.0);
    }

    #[test]
    fn db_conversion() {
        let mut m = Matrix::zeros(2, 2);
        m.set_db(0, 1, -6.0);
        assert!((m.get(0, 1) - 0.501187).abs() < 1e-5);
        assert!((m.gain_db(0, 1).unwrap() + 6.0).abs() < 1e-9);
        assert_eq!(m.gain_db(1, 0), None);
        m.set(1, 0, -0.5); // inverted polarity
        assert!((m.gain_db(1, 0).unwrap() + 6.0206).abs() < 1e-3);
        m.clear(0, 1);
        assert_eq!(m.get(0, 1), 0.0);
    }

    #[test]
    fn json_round_trip_is_a_plain_2d_array() {
        let mut m = Matrix::identity(2, 3);
        m.set(0, 2, 0.5);
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(json, "[[1.0,0.0,0.5],[0.0,1.0,0.0]]");
        assert_eq!(serde_json::from_str::<Matrix>(&json).unwrap(), m);
    }

    #[test]
    fn ragged_matrix_is_rejected() {
        assert!(serde_json::from_str::<Matrix>("[[1,0],[0]]").is_err());
    }
}
