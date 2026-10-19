//! Editing operations on a [`Setup`], free of any GUI code so they can be tested directly.

use linapo_core::apo::{layout_for, Profile};
use linapo_core::setup::eqs_from_profile;
use linapo_core::chain::ChannelChain;
use linapo_core::filter::DEFAULT_Q;
use linapo_core::{Channel, EqPreset, Filter, FilterKind, Matrix, Setup};

pub const LAYOUTS: [(&str, usize); 5] = [("Mono", 1), ("Stereo", 2), ("Quad", 4), ("5.1", 6), ("7.1", 8)];

/// Name of a channel list: a standard layout name, or "Custom (n ch)".
pub fn layout_name(chs: &[Channel]) -> String {
    LAYOUTS
        .iter()
        .find(|(_, n)| layout_for(*n) == chs)
        .map(|(name, _)| name.to_string())
        .unwrap_or_else(|| format!("Custom ({} ch)", chs.len()))
}

fn rebuild_matrix(setup: &mut Setup, new_in: Vec<Channel>, new_out: Vec<Channel>) {
    let mut m = Matrix::zeros(new_in.len(), new_out.len());
    for (i, ic) in new_in.iter().enumerate() {
        for (o, oc) in new_out.iter().enumerate() {
            let old_i = setup.input_channels.iter().position(|c| c == ic);
            let old_o = setup.output_channels.iter().position(|c| c == oc);
            match (old_i, old_o) {
                (Some(oi), Some(oo)) => m.set(i, o, setup.matrix.get(oi, oo)), // keep what the user had
                _ if ic == oc => m.set(i, o, 1.0),                              // new cell: same name = straight through
                _ => {}
            }
        }
    }
    setup.matrix = m;
    setup.input_channels = new_in;
}

pub fn set_input_layout(setup: &mut Setup, new_in: Vec<Channel>) {
    let out = setup.output_channels.clone();
    rebuild_matrix(setup, new_in, out);
}

/// Change the output channels. Matrix cells between channels that exist before and after are kept;
/// EQs lose removed channels, and an EQ that covered *every* old output follows the new layout.
pub fn set_output_layout(setup: &mut Setup, new_out: Vec<Channel>) {
    let old_out = setup.output_channels.clone();
    let inp = setup.input_channels.clone();
    rebuild_matrix(setup, inp, new_out.clone());
    for eq in &mut setup.eqs {
        let covered_all = !old_out.is_empty() && old_out.iter().all(|c| eq.channels.contains(c));
        if covered_all {
            eq.channels = new_out.clone();
        } else {
            eq.channels.retain(|c| new_out.contains(c));
        }
    }
    setup.output_channels = new_out;
}

/// Same-named channels straight through, everything else off.
pub fn matrix_identity(setup: &mut Setup) {
    let (i, o) = (setup.input_channels.clone(), setup.output_channels.clone());
    setup.matrix = Matrix::zeros(i.len(), o.len());
    for (ii, ic) in i.iter().enumerate() {
        if let Some(oo) = o.iter().position(|c| c == ic) {
            setup.matrix.set(ii, oo, 1.0);
        }
    }
}

pub fn matrix_clear(setup: &mut Setup) {
    setup.matrix = Matrix::zeros(setup.input_channels.len(), setup.output_channels.len());
}

/// Fold surround inputs into L/R outputs (centre and surrounds at -3 dB, LFE dropped).
/// Returns false if the outputs do not contain both L and R.
pub fn matrix_downmix_to_stereo(setup: &mut Setup) -> bool {
    let (Some(l), Some(r)) = (
        setup.output_channels.iter().position(|c| c.name() == "L"),
        setup.output_channels.iter().position(|c| c.name() == "R"),
    ) else {
        return false;
    };
    matrix_clear(setup);
    let k = std::f64::consts::FRAC_1_SQRT_2;
    for (i, c) in setup.input_channels.clone().iter().enumerate() {
        match c.name() {
            "L" => setup.matrix.set(i, l, 1.0),
            "R" => setup.matrix.set(i, r, 1.0),
            "C" => {
                setup.matrix.set(i, l, k);
                setup.matrix.set(i, r, k);
            }
            "RL" | "SL" => setup.matrix.set(i, l, k),
            "RR" | "SR" => setup.matrix.set(i, r, k),
            _ => {}
        }
    }
    true
}

/// Click on a cell: off -> 0 dB, on -> off.
pub fn toggle_route(setup: &mut Setup, i: usize, o: usize) {
    let g = if setup.matrix.get(i, o) == 0.0 { 1.0 } else { 0.0 };
    setup.matrix.set(i, o, g);
}

/// Set a route's volume in dB, keeping its polarity. Values at or below -80 dB switch it off.
pub fn set_route_db(setup: &mut Setup, i: usize, o: usize, db: f64) {
    let sign = if setup.matrix.get(i, o) < 0.0 { -1.0 } else { 1.0 };
    if db <= -80.0 {
        setup.matrix.set(i, o, 0.0);
    } else {
        setup.matrix.set(i, o, sign * linapo_core::matrix::db_to_lin(db));
    }
}

pub fn set_route_inverted(setup: &mut Setup, i: usize, o: usize, inverted: bool) {
    let g = setup.matrix.get(i, o).abs();
    setup.matrix.set(i, o, if inverted { -g } else { g });
}

// ------------------------------------------------------------------ EQs

pub fn add_eq(setup: &mut Setup) -> usize {
    let n = setup.eqs.len() + 1;
    setup.eqs.push(EqPreset {
        name: format!("EQ {n}"),
        enabled: true,
        preamp_db: 0.0,
        filters: Vec::new(),
        channels: setup.output_channels.clone(),
    });
    setup.eqs.len() - 1
}

pub fn duplicate_eq(setup: &mut Setup, idx: usize) -> usize {
    let mut copy = setup.eqs[idx].clone();
    copy.name = format!("{} copy", copy.name);
    setup.eqs.insert(idx + 1, copy);
    idx + 1
}

/// Remove an EQ; returns the index that should be selected afterwards.
pub fn remove_eq(setup: &mut Setup, idx: usize) -> usize {
    setup.eqs.remove(idx);
    idx.min(setup.eqs.len().saturating_sub(1))
}

const NEW_FILTER_FREQS: [f64; 8] = [1000.0, 250.0, 4000.0, 100.0, 8000.0, 500.0, 2000.0, 60.0];

/// Add a peaking filter at a free-ish frequency; returns its index.
pub fn add_filter(eq: &mut EqPreset) -> usize {
    let fc = NEW_FILTER_FREQS[eq.filters.len() % NEW_FILTER_FREQS.len()];
    eq.filters.push(Filter { kind: FilterKind::Peaking, fc_hz: fc, gain_db: 0.0, q: 1.0, enabled: true });
    eq.filters.len() - 1
}

pub fn add_filter_at(eq: &mut EqPreset, fc_hz: f64, gain_db: f64) -> usize {
    let mut f = Filter { kind: FilterKind::Peaking, fc_hz, gain_db, q: 1.0, enabled: true };
    clamp_filter(&mut f);
    eq.filters.push(f);
    eq.filters.len() - 1
}

pub fn toggle_eq_channel(eq: &mut EqPreset, ch: &Channel, outputs: &[Channel]) {
    if let Some(p) = eq.channels.iter().position(|c| c == ch) {
        eq.channels.remove(p);
    } else {
        eq.channels.push(ch.clone());
        eq.channels.sort_by_key(|c| outputs.iter().position(|o| o == c).unwrap_or(usize::MAX));
    }
}

pub fn uses_gain(kind: FilterKind) -> bool {
    matches!(
        kind,
        FilterKind::Peaking | FilterKind::LowShelf | FilterKind::HighShelf | FilterKind::LowShelf1 | FilterKind::HighShelf1
    )
}

pub fn uses_q(kind: FilterKind) -> bool {
    !matches!(kind, FilterKind::LowShelf1 | FilterKind::HighShelf1)
}

pub const KINDS: [(FilterKind, &str); 10] = [
    (FilterKind::Peaking, "Peak"),
    (FilterKind::LowShelf, "Low shelf"),
    (FilterKind::HighShelf, "High shelf"),
    (FilterKind::LowShelf1, "Low shelf 6 dB"),
    (FilterKind::HighShelf1, "High shelf 6 dB"),
    (FilterKind::LowPass, "Low pass"),
    (FilterKind::HighPass, "High pass"),
    (FilterKind::BandPass, "Band pass"),
    (FilterKind::Notch, "Notch"),
    (FilterKind::AllPass, "All pass"),
];

pub fn kind_name(kind: FilterKind) -> &'static str {
    KINDS.iter().find(|(k, _)| *k == kind).map(|(_, n)| *n).unwrap_or("?")
}

/// Keep a filter inside ranges that are both audible-sane and valid at 44.1 kHz and up.
pub fn clamp_filter(f: &mut Filter) {
    f.fc_hz = f.fc_hz.clamp(10.0, 20_000.0);
    f.gain_db = f.gain_db.clamp(-30.0, 30.0);
    f.q = if f.q.is_finite() { f.q.clamp(0.1, 30.0) } else { DEFAULT_Q };
}

/// Chain of one EQ alone (for drawing its own curve).
pub fn eq_chain(eq: &EqPreset) -> ChannelChain {
    ChannelChain { gain_db: eq.preamp_db, filters: eq.filters.clone() }
}

/// Add the EQs found in an APO profile (one per `Channel:` selection). Returns how many were added.
pub fn import_eqs(setup: &mut Setup, profile: &Profile, name: &str) -> (usize, Vec<String>) {
    let (eqs, warnings) = eqs_from_profile(profile, &setup.output_channels, name);
    let n = eqs.len();
    setup.eqs.extend(eqs);
    (n, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use linapo_core::apo::parse_str;

    fn ch(s: &str) -> Channel {
        Channel::parse(s).unwrap()
    }

    fn stereo() -> Setup {
        Setup::passthrough(&layout_for(2))
    }

    #[test]
    fn layout_names() {
        assert_eq!(layout_name(&layout_for(2)), "Stereo");
        assert_eq!(layout_name(&layout_for(6)), "5.1");
        assert_eq!(layout_name(&layout_for(3)), "Custom (3 ch)");
    }

    #[test]
    fn growing_outputs_keeps_user_routes_and_adds_straight_through_for_new_matching_names() {
        let mut s = stereo();
        s.matrix.set(0, 1, 0.5); // user routed L -> R at 0.5
        s.matrix.set(0, 0, 0.0); // and removed L -> L
        set_output_layout(&mut s, layout_for(6));
        s.validate().unwrap();
        assert_eq!(s.matrix.n_out(), 6);
        assert_eq!(s.matrix.get(0, 1), 0.5, "existing cell kept");
        assert_eq!(s.matrix.get(0, 0), 0.0, "existing off cell stays off");
        assert_eq!(s.matrix.get(1, 1), 1.0);
        assert_eq!(s.matrix.get(0, 2), 0.0, "new outputs have no same-named input");
    }

    #[test]
    fn growing_inputs_to_surround_then_downmix() {
        let mut s = stereo();
        set_input_layout(&mut s, layout_for(6));
        s.validate().unwrap();
        assert_eq!((s.matrix.n_in(), s.matrix.n_out()), (6, 2));
        assert_eq!(s.matrix.get(0, 0), 1.0);
        assert_eq!(s.matrix.get(2, 0), 0.0, "centre is not routed until asked");
        assert!(matrix_downmix_to_stereo(&mut s));
        assert!((s.matrix.get(2, 0) - 0.7071).abs() < 1e-3 && (s.matrix.get(2, 1) - 0.7071).abs() < 1e-3);
        assert!((s.matrix.get(4, 0) - 0.7071).abs() < 1e-3 && s.matrix.get(4, 1) == 0.0);
        assert_eq!(s.matrix.get(3, 0), 0.0, "LFE dropped");
    }

    #[test]
    fn downmix_needs_lr_outputs() {
        let mut s = Setup::passthrough(&layout_for(1));
        assert!(!matrix_downmix_to_stereo(&mut s));
    }

    #[test]
    fn shrinking_outputs_updates_eq_assignments() {
        let mut s = Setup::passthrough(&layout_for(6));
        let all = add_eq(&mut s);
        let some = add_eq(&mut s);
        s.eqs[some].channels = vec![ch("L"), ch("SUB"), ch("RR")];
        set_output_layout(&mut s, layout_for(2));
        s.validate().unwrap();
        assert_eq!(s.eqs[all].channels, layout_for(2), "an EQ on every channel follows the layout");
        assert_eq!(s.eqs[some].channels, vec![ch("L")], "removed channels are dropped");
    }

    #[test]
    fn growing_outputs_extends_all_channel_eqs_only() {
        let mut s = stereo();
        let all = add_eq(&mut s);
        let only_l = add_eq(&mut s);
        s.eqs[only_l].channels = vec![ch("L")];
        set_output_layout(&mut s, layout_for(6));
        assert_eq!(s.eqs[all].channels.len(), 6);
        assert_eq!(s.eqs[only_l].channels, vec![ch("L")]);
        s.validate().unwrap();
    }

    #[test]
    fn layout_changes_always_leave_a_valid_setup() {
        let mut s = stereo();
        add_eq(&mut s);
        for n in [6, 8, 1, 4, 2, 8, 6] {
            set_output_layout(&mut s, layout_for(n));
            set_input_layout(&mut s, layout_for(n));
            s.validate().unwrap_or_else(|e| panic!("{n}: {e}"));
        }
    }

    #[test]
    fn identity_clear_and_toggle() {
        let mut s = Setup::passthrough(&layout_for(6));
        set_input_layout(&mut s, layout_for(2));
        matrix_clear(&mut s);
        assert!(s.matrix.as_slice().iter().all(|g| *g == 0.0));
        matrix_identity(&mut s);
        assert_eq!(s.matrix.get(0, 0), 1.0);
        assert_eq!(s.matrix.get(1, 1), 1.0);
        assert_eq!(s.matrix.get(0, 2), 0.0);
        toggle_route(&mut s, 0, 3);
        assert_eq!(s.matrix.get(0, 3), 1.0);
        toggle_route(&mut s, 0, 3);
        assert_eq!(s.matrix.get(0, 3), 0.0);
    }

    #[test]
    fn route_volume_and_polarity_are_independent() {
        let mut s = stereo();
        set_route_db(&mut s, 0, 1, -6.0);
        assert!((s.matrix.get(0, 1) - 0.501).abs() < 1e-3);
        set_route_inverted(&mut s, 0, 1, true);
        assert!((s.matrix.get(0, 1) + 0.501).abs() < 1e-3);
        set_route_db(&mut s, 0, 1, -3.0);
        assert!(s.matrix.get(0, 1) < 0.0, "polarity survives a volume change");
        set_route_inverted(&mut s, 0, 1, false);
        assert!(s.matrix.get(0, 1) > 0.0);
        set_route_db(&mut s, 0, 1, -100.0);
        assert_eq!(s.matrix.get(0, 1), 0.0, "very low = off");
    }

    #[test]
    fn eq_management() {
        let mut s = stereo();
        let a = add_eq(&mut s);
        let b = add_eq(&mut s);
        assert_eq!((a, b), (0, 1));
        assert_eq!(s.eqs[1].name, "EQ 2");
        let f = add_filter(&mut s.eqs[0]);
        assert_eq!(f, 0);
        let d = duplicate_eq(&mut s, 0);
        assert_eq!(d, 1);
        assert_eq!(s.eqs[1].name, "EQ 1 copy");
        assert_eq!(s.eqs[1].filters.len(), 1);
        assert_eq!(remove_eq(&mut s, 2), 1, "selection moves to the last remaining");
        assert_eq!(s.eqs.len(), 2);
        s.validate().unwrap();
    }

    #[test]
    fn new_filters_spread_across_frequencies() {
        let mut eq = add_eq_standalone();
        for _ in 0..4 {
            add_filter(&mut eq);
        }
        let mut f: Vec<_> = eq.filters.iter().map(|f| f.fc_hz).collect();
        f.dedup();
        assert_eq!(f.len(), 4, "no two new filters start at the same frequency");
    }

    fn add_eq_standalone() -> EqPreset {
        let mut s = stereo();
        let i = add_eq(&mut s);
        s.eqs.remove(i)
    }

    #[test]
    fn channel_chips_keep_layout_order() {
        let mut s = Setup::passthrough(&layout_for(6));
        let i = add_eq(&mut s);
        s.eqs[i].channels.clear();
        let outs = s.output_channels.clone();
        for c in ["RR", "L", "SUB"] {
            toggle_eq_channel(&mut s.eqs[i], &ch(c), &outs);
        }
        assert_eq!(s.eqs[i].channels, vec![ch("L"), ch("SUB"), ch("RR")]);
        toggle_eq_channel(&mut s.eqs[i], &ch("SUB"), &outs);
        assert_eq!(s.eqs[i].channels, vec![ch("L"), ch("RR")]);
    }

    #[test]
    fn clamping_makes_any_filter_valid() {
        let mut f = Filter { kind: FilterKind::Peaking, fc_hz: 1e9, gain_db: 999.0, q: f64::NAN, enabled: true };
        clamp_filter(&mut f);
        assert!(f.coeffs(44_100.0).is_ok(), "{f:?}");
        let mut f = Filter { kind: FilterKind::Peaking, fc_hz: -5.0, gain_db: -999.0, q: 0.0, enabled: true };
        clamp_filter(&mut f);
        assert!(f.coeffs(44_100.0).is_ok());
        assert_eq!((f.fc_hz, f.gain_db, f.q), (10.0, -30.0, 0.1));
    }

    #[test]
    fn filter_capabilities() {
        assert!(uses_gain(FilterKind::Peaking) && !uses_gain(FilterKind::LowPass));
        assert!(uses_q(FilterKind::Peaking) && !uses_q(FilterKind::LowShelf1));
        assert_eq!(KINDS.len(), 10);
        assert_eq!(kind_name(FilterKind::Notch), "Notch");
    }

    #[test]
    fn importing_apo_adds_eqs_for_the_current_outputs() {
        let mut s = Setup::passthrough(&layout_for(2));
        let p = parse_str("Preamp: -3 dB\nFilter: ON PK Fc 1000 Hz Gain 3 dB Q 1\nChannel: L\nFilter: ON PK Fc 100 Hz Gain 2 dB Q 1\n");
        let (n, w) = import_eqs(&mut s, &p, "headphones");
        assert_eq!(n, 2);
        assert!(w.is_empty());
        assert_eq!(s.eqs[0].name, "headphones (all)");
        assert_eq!(s.eqs[1].channels, vec![ch("L")]);
        s.validate().unwrap();
    }
}
