//! A per-channel EQ chain: one gain stage followed by biquads.

use crate::biquad::Biquad;
use crate::filter::Filter;

/// What a profile resolves to for one channel.
///
/// All `Preamp:` lines that apply to the channel are summed into `gain_db`
/// (gain commutes with linear filters, so order does not matter).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChannelChain {
    pub gain_db: f64,
    pub filters: Vec<Filter>,
}

impl ChannelChain {
    pub fn is_empty(&self) -> bool {
        self.gain_db == 0.0 && self.filters.is_empty()
    }
}

/// Combined magnitude response in dB at `f` Hz. Filters that are invalid at
/// `fs` are skipped, matching what [`EqChain::build`] does.
pub fn response_db(chain: &ChannelChain, fs: f64, f: f64) -> f64 {
    chain.gain_db
        + chain
            .filters
            .iter()
            .filter(|flt| flt.enabled)
            .filter_map(|flt| flt.coeffs(fs).ok())
            .map(|c| c.magnitude_db(fs, f))
            .sum::<f64>()
}

/// Runtime processor for one channel. Owns its filter state, so two channels
/// that share the same [`ChannelChain`] still get independent `EqChain`s.
#[derive(Debug, Clone)]
pub struct EqChain {
    gain: f64,
    stages: Vec<Biquad>,
}

impl EqChain {
    /// Build a processor for sample rate `fs`. Returns the processor plus a
    /// warning for every filter that had to be skipped.
    pub fn build(chain: &ChannelChain, fs: f64) -> (Self, Vec<String>) {
        let mut stages = Vec::with_capacity(chain.filters.len());
        let mut warnings = Vec::new();
        for f in chain.filters.iter().filter(|f| f.enabled) {
            match f.coeffs(fs) {
                Ok(c) => stages.push(Biquad::new(c)),
                Err(e) => warnings.push(format!("skipped filter [{f}]: {e}")),
            }
        }
        (EqChain { gain: 10f64.powf(chain.gain_db / 20.0), stages }, warnings)
    }

    pub fn reset(&mut self) {
        self.stages.iter_mut().for_each(Biquad::reset);
    }

    /// Continue from `old`'s filter state when the structure is unchanged
    /// (same number of biquads), so live parameter edits do not click.
    pub fn inherit_state(&mut self, old: &EqChain) {
        if self.stages.len() == old.stages.len() {
            for (n, o) in self.stages.iter_mut().zip(&old.stages) {
                n.set_state(o.state());
            }
        }
    }

    /// Process in place. Allocation-free.
    pub fn process_block(&mut self, buf: &mut [f32]) {
        let g = self.gain as f32;
        if g != 1.0 {
            buf.iter_mut().for_each(|s| *s *= g);
        }
        for st in &mut self.stages {
            st.process_block(buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::FilterKind;

    fn pk(fc: f64, g: f64) -> Filter {
        Filter { kind: FilterKind::Peaking, fc_hz: fc, gain_db: g, q: 1.0, enabled: true }
    }

    #[test]
    fn response_adds_gain_and_filters() {
        let chain = ChannelChain { gain_db: -3.0, filters: vec![pk(1000.0, 6.0)] };
        let r = response_db(&chain, 48_000.0, 1000.0);
        assert!((r - 3.0).abs() < 1e-9, "{r}");
    }

    #[test]
    fn invalid_filters_are_skipped_with_warning() {
        let chain = ChannelChain { gain_db: 0.0, filters: vec![pk(1000.0, 6.0), pk(30_000.0, 6.0)] };
        let (_, warnings) = EqChain::build(&chain, 48_000.0);
        assert_eq!(warnings.len(), 1);
        assert!((response_db(&chain, 48_000.0, 1000.0) - 6.0).abs() < 1e-6);
    }

    #[test]
    fn disabled_filters_do_nothing() {
        let off = Filter { enabled: false, ..pk(1000.0, 12.0) };
        let chain = ChannelChain { gain_db: 0.0, filters: vec![off, pk(4000.0, 3.0)] };
        assert!(response_db(&chain, 48_000.0, 1000.0).abs() < 0.5);
        let (mut eq, w) = EqChain::build(&chain, 48_000.0);
        assert!(w.is_empty());
        let mut x = [0.0f32; 64];
        x[0] = 1.0;
        let mut y = x;
        eq.process_block(&mut y);
        let (mut only, _) = EqChain::build(&ChannelChain { gain_db: 0.0, filters: vec![pk(4000.0, 3.0)] }, 48_000.0);
        only.process_block(&mut x);
        assert_eq!(x, y, "a disabled filter must not even touch the signal");
    }

    #[test]
    fn gain_scales_signal() {
        let chain = ChannelChain { gain_db: -6.0206, filters: vec![] };
        let (mut eq, _) = EqChain::build(&chain, 48_000.0);
        let mut buf = [1.0f32; 4];
        eq.process_block(&mut buf);
        assert!((buf[0] - 0.5).abs() < 1e-4);
    }

    #[test]
    fn independent_state_per_instance() {
        let chain = ChannelChain { gain_db: 0.0, filters: vec![pk(200.0, 9.0)] };
        let (mut a, _) = EqChain::build(&chain, 48_000.0);
        let (mut b, _) = EqChain::build(&chain, 48_000.0);
        let mut x = [1.0f32, 0.0, 0.0, 0.0];
        let mut y = [0.0f32; 4];
        a.process_block(&mut x);
        b.process_block(&mut y);
        assert!(y.iter().all(|v| *v == 0.0), "b must not see a's state");
    }
}
