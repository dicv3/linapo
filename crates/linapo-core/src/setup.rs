//! The complete user configuration: channel layouts, matrix, EQ assignments,
//! master gain. This is what the GUI edits, what is saved as JSON, and what
//! the real-time [`crate::engine::Engine`] is built from.
//!
//! Signal flow: `inputs -> matrix -> per-output-channel EQ -> master gain -> limiter -> outputs`.

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::apo::{Channel, Command, Profile, Selection};
use crate::chain::ChannelChain;
use crate::filter::Filter;
use crate::matrix::Matrix;

pub const SETUP_VERSION: u32 = 1;

fn yes() -> bool {
    true
}

fn current_version() -> u32 {
    SETUP_VERSION
}

/// A named EQ (preamp + filters) applied to one or more *output* channels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EqPreset {
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub preamp_db: f64,
    #[serde(default)]
    pub filters: Vec<Filter>,
    pub channels: Vec<Channel>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Setup {
    #[serde(default = "current_version")]
    pub version: u32,
    pub input_channels: Vec<Channel>,
    pub output_channels: Vec<Channel>,
    /// `matrix[input][output]`, linear gain.
    pub matrix: Matrix,
    #[serde(default)]
    pub eqs: Vec<EqPreset>,
    #[serde(default)]
    pub master_gain_db: f64,
    /// Peak limiter after the master gain, so a hot EQ cannot clip the output.
    #[serde(default = "yes")]
    pub limiter: bool,
    /// A/B switch: when true every EQ is bypassed (filters and preamps do nothing) while
    /// the matrix and master gain still apply. Structure is unchanged, so toggling is live.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bypass_eq: bool,
    /// PipeWire node name of the device the processed audio is sent to.
    /// `None` means "the default output".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_target: Option<String>,
}

#[derive(Debug)]
pub enum SetupError {
    Io(io::Error),
    Json(serde_json::Error),
    Invalid(String),
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetupError::Io(e) => write!(f, "{e}"),
            SetupError::Json(e) => write!(f, "invalid setup file: {e}"),
            SetupError::Invalid(s) => write!(f, "invalid setup: {s}"),
        }
    }
}

impl std::error::Error for SetupError {}

impl Setup {
    /// Identity routing, no EQ. Input and output share the same layout.
    pub fn passthrough(layout: &[Channel]) -> Setup {
        Setup {
            version: SETUP_VERSION,
            input_channels: layout.to_vec(),
            output_channels: layout.to_vec(),
            matrix: Matrix::identity(layout.len(), layout.len()),
            eqs: Vec::new(),
            master_gain_db: 0.0,
            limiter: true,
            bypass_eq: false,
            output_target: None,
        }
    }

    /// Build a setup from an APO profile: `Copy:` lines become the matrix and
    /// `Channel:` sections become EQ presets assigned to those channels.
    ///
    /// Deviation from APO: EQ always runs *after* the matrix, whereas APO
    /// processes lines in file order. For profiles that filter before copying,
    /// results can differ.
    pub fn from_profile(profile: &Profile, layout: &[Channel], name: &str) -> (Setup, Vec<String>) {
        let (matrix, mut warnings) = matrix_from_profile(profile, layout);
        let (eqs, w2) = eqs_from_profile(profile, layout, name);
        warnings.extend(w2);
        let setup = Setup {
            version: SETUP_VERSION,
            input_channels: layout.to_vec(),
            output_channels: layout.to_vec(),
            matrix,
            eqs,
            master_gain_db: 0.0,
            limiter: true,
            bypass_eq: false,
            output_target: None,
        };
        (setup, warnings)
    }

    /// Structural checks. The engine refuses setups that fail this.
    pub fn validate(&self) -> Result<(), SetupError> {
        let bad = |s: String| Err(SetupError::Invalid(s));
        if self.matrix.n_in() != self.input_channels.len() || self.matrix.n_out() != self.output_channels.len() {
            return bad(format!(
                "matrix is {}x{} but there are {} input and {} output channels",
                self.matrix.n_in(),
                self.matrix.n_out(),
                self.input_channels.len(),
                self.output_channels.len()
            ));
        }
        for (what, list) in [("input", &self.input_channels), ("output", &self.output_channels)] {
            for (i, c) in list.iter().enumerate() {
                if list[..i].contains(c) {
                    return bad(format!("duplicate {what} channel {c}"));
                }
            }
        }
        if self.matrix.as_slice().iter().any(|g| !g.is_finite()) {
            return bad("matrix contains a non-finite gain".into());
        }
        if !self.master_gain_db.is_finite() {
            return bad("master gain is not finite".into());
        }
        for eq in &self.eqs {
            if !eq.preamp_db.is_finite() {
                return bad(format!("EQ '{}': preamp is not finite", eq.name));
            }
            for c in &eq.channels {
                if !self.output_channels.contains(c) {
                    return bad(format!("EQ '{}' targets channel {c}, which is not an output channel", eq.name));
                }
            }
        }
        Ok(())
    }

    /// Everything that applies to one output channel: all enabled EQs whose
    /// channel list contains it, in list order.
    pub fn chain_for_output(&self, ch: &Channel) -> ChannelChain {
        let mut chain = ChannelChain::default();
        for eq in self.eqs.iter().filter(|e| e.enabled && e.channels.contains(ch)) {
            if self.bypass_eq {
                // Keep the filters (so the graph structure does not change) but switch them off.
                chain.filters.extend(eq.filters.iter().map(|f| Filter { enabled: false, ..*f }));
            } else {
                chain.gain_db += eq.preamp_db;
                chain.filters.extend_from_slice(&eq.filters);
            }
        }
        chain
    }

    pub fn from_json(text: &str) -> Result<Setup, SetupError> {
        let s: Setup = serde_json::from_str(text).map_err(SetupError::Json)?;
        s.validate()?;
        Ok(s)
    }

    pub fn to_json_pretty(&self) -> String {
        // Serialising plain data cannot fail.
        serde_json::to_string_pretty(self).expect("setup serialises")
    }

    pub fn load(path: &Path) -> Result<Setup, SetupError> {
        Self::from_json(&fs::read_to_string(path).map_err(SetupError::Io)?)
    }

    /// Writes via a temp file + rename so a reader never sees a half-written file.
    pub fn save(&self, path: &Path) -> Result<(), SetupError> {
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, self.to_json_pretty()).map_err(SetupError::Io)?;
        fs::rename(&tmp, path).map_err(SetupError::Io)
    }
}

/// Convert APO `Copy:` lines into a matrix for `layout`.
///
/// Copy lines are applied in order. Within one line, all right-hand sides read
/// the values from before that line (so `Copy: L=R R=L` swaps).
pub fn matrix_from_profile(profile: &Profile, layout: &[Channel]) -> (Matrix, Vec<String>) {
    let n = layout.len();
    let idx = |c: &Channel| layout.iter().position(|x| x == c);
    let mut warnings = Vec::new();
    // state[c][i] = weight of input i in the current signal on channel c
    let mut state: Vec<Vec<f64>> = (0..n).map(|c| (0..n).map(|i| f64::from(c == i)).collect()).collect();

    for item in &profile.items {
        let Command::Copy(assignments) = &item.command else { continue };
        let old = state.clone();
        for a in assignments {
            let Some(d) = idx(&a.dest) else {
                warnings.push(format!("Copy: destination {} is not in the channel layout, skipped", a.dest));
                continue;
            };
            let mut row = vec![0.0; n];
            for src in &a.sources {
                let Some(s) = idx(&src.channel) else {
                    warnings.push(format!("Copy: source {} is not in the channel layout, ignored", src.channel));
                    continue;
                };
                for i in 0..n {
                    row[i] += src.gain * old[s][i];
                }
            }
            state[d] = row;
        }
    }

    let mut m = Matrix::zeros(n, n);
    for i in 0..n {
        for o in 0..n {
            m.set(i, o, state[o][i]);
        }
    }
    (m, warnings)
}

/// Group a profile's `Preamp`/`Filter` commands into one [`EqPreset`] per
/// distinct `Channel:` selection, expanding `all` to the full layout.
pub fn eqs_from_profile(profile: &Profile, layout: &[Channel], base_name: &str) -> (Vec<EqPreset>, Vec<String>) {
    let mut warnings = Vec::new();
    let mut groups: Vec<(Selection, EqPreset)> = Vec::new();

    for item in &profile.items {
        let touch = match &item.command {
            Command::Preamp { .. } | Command::Filter(_) | Command::GraphicEq(_) => true,
            Command::Copy(_) => false,
        };
        if !touch {
            continue;
        }
        let pos = match groups.iter().position(|(s, _)| *s == item.channels) {
            Some(p) => p,
            None => {
                let (label, channels) = match &item.channels {
                    Selection::All => ("all".to_string(), layout.to_vec()),
                    Selection::Only(list) => {
                        let mut kept = Vec::new();
                        for c in list {
                            if layout.contains(c) {
                                kept.push(c.clone());
                            } else {
                                warnings.push(format!("Channel {c} is not in the channel layout, ignored"));
                            }
                        }
                        let label = list.iter().map(|c| c.name()).collect::<Vec<_>>().join(" ");
                        (label, kept)
                    }
                };
                groups.push((
                    item.channels.clone(),
                    EqPreset {
                        name: format!("{base_name} ({label})"),
                        enabled: true,
                        preamp_db: 0.0,
                        filters: Vec::new(),
                        channels,
                    },
                ));
                groups.len() - 1
            }
        };
        match &item.command {
            Command::Preamp { gain_db } => groups[pos].1.preamp_db += gain_db,
            Command::Filter(f) => groups[pos].1.filters.push(*f),
            Command::GraphicEq(points) => match crate::graphic_eq::fit_default(points) {
                Ok(fit) => {
                    groups[pos].1.preamp_db += fit.preamp_db;
                    warnings.push(format!(
                        "GraphicEQ ({} points) converted to {} filters (max error {:.2} dB, no latency)",
                        points.len(),
                        fit.filters.len(),
                        fit.max_error_db
                    ));
                    groups[pos].1.filters.extend(fit.filters);
                }
                Err(e) => warnings.push(format!("GraphicEQ skipped: {e}")),
            },
            _ => {}
        }
    }
    (groups.into_iter().map(|(_, p)| p).collect(), warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apo::{layout_for, parse_str};
    use crate::chain::response_db;

    fn ch(s: &str) -> Channel {
        Channel::parse(s).unwrap()
    }

    #[test]
    fn passthrough_is_valid_identity() {
        let s = Setup::passthrough(&layout_for(6));
        s.validate().unwrap();
        assert_eq!(s.matrix, Matrix::identity(6, 6));
        assert!(s.limiter);
    }

    #[test]
    fn json_round_trip() {
        let (mut s, _) = Setup::from_profile(
            &parse_str("Preamp: -3 dB\nFilter: ON PK Fc 1000 Hz Gain 3 dB Q 1\n"),
            &layout_for(2),
            "test",
        );
        s.master_gain_db = -2.5;
        let back = Setup::from_json(&s.to_json_pretty()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn hand_written_minimal_json_gets_defaults() {
        let s = Setup::from_json(
            r#"{
                "input_channels": ["L", "R"],
                "output_channels": ["L", "R"],
                "matrix": [[1, 0], [0, 1]],
                "eqs": [{ "name": "bass", "channels": ["L", "R"],
                          "filters": [{ "kind": "low_shelf", "fc_hz": 100, "gain_db": 4 }] }]
            }"#,
        )
        .unwrap();
        assert!(s.limiter);
        assert_eq!(s.master_gain_db, 0.0);
        assert!(s.eqs[0].enabled);
        assert_eq!(s.eqs[0].filters[0].q, crate::filter::DEFAULT_Q);
    }

    #[test]
    fn validation_catches_mistakes() {
        let base = Setup::passthrough(&layout_for(2));

        let mut s = base.clone();
        s.matrix = Matrix::identity(3, 2);
        assert!(s.validate().is_err(), "dimension mismatch");

        let mut s = base.clone();
        s.output_channels = vec![ch("L"), ch("L")];
        assert!(s.validate().is_err(), "duplicate channel");

        let mut s = base.clone();
        s.eqs.push(EqPreset { name: "x".into(), enabled: true, preamp_db: 0.0, filters: vec![], channels: vec![ch("SUB")] });
        assert!(s.validate().is_err(), "EQ on a channel that does not exist");

        let mut s = base.clone();
        s.matrix.set(0, 0, f64::NAN);
        assert!(s.validate().is_err(), "NaN gain");

        assert!(Setup::from_json(r#"{"input_channels":["L","banana"],"output_channels":[],"matrix":[]}"#).is_err());
    }

    #[test]
    fn copy_swap_is_simultaneous() {
        let (m, w) = matrix_from_profile(&parse_str("Copy: L=R R=L\n"), &layout_for(2));
        assert!(w.is_empty());
        assert_eq!(m, {
            let mut e = Matrix::zeros(2, 2);
            e.set(0, 1, 1.0);
            e.set(1, 0, 1.0);
            e
        });
    }

    #[test]
    fn copy_lines_compose_in_order() {
        // After `L=R` both outputs carry R; then `R=L` copies the (new) L, which is still R.
        let (m, _) = matrix_from_profile(&parse_str("Copy: L=R\nCopy: R=L\n"), &layout_for(2));
        assert_eq!(m.get(0, 0), 0.0);
        assert_eq!(m.get(0, 1), 0.0);
        assert_eq!(m.get(1, 0), 1.0);
        assert_eq!(m.get(1, 1), 1.0);
    }

    #[test]
    fn copy_mixes_with_gains() {
        let (m, _) = matrix_from_profile(&parse_str("Copy: L=0.5*L+0.5*R R=0.5*L+0.5*R\n"), &layout_for(2));
        for i in 0..2 {
            for o in 0..2 {
                assert_eq!(m.get(i, o), 0.5);
            }
        }
    }

    #[test]
    fn copy_downmix_5_1_style() {
        let p = parse_str("Copy: L=L+0.707*C+0.707*RL R=R+0.707*C+0.707*RR\n");
        let (m, w) = matrix_from_profile(&p, &layout_for(6));
        assert!(w.is_empty());
        // inputs: L R C SUB RL RR   outputs: same order
        assert_eq!(m.get(0, 0), 1.0);
        assert_eq!(m.get(2, 0), 0.707);
        assert_eq!(m.get(4, 0), 0.707);
        assert_eq!(m.get(2, 1), 0.707);
        assert_eq!(m.get(5, 1), 0.707);
        assert_eq!(m.get(4, 1), 0.0);
        assert_eq!(m.get(2, 2), 1.0, "unassigned channels pass through");
    }

    #[test]
    fn copy_with_unknown_channels_warns() {
        let (_, w) = matrix_from_profile(&parse_str("Copy: SL=L\nCopy: L=SR\n"), &layout_for(2));
        assert_eq!(w.len(), 2);
    }

    #[test]
    fn eq_groups_follow_channel_sections() {
        let p = parse_str(
            "Preamp: -3 dB\nFilter: ON PK Fc 1000 Hz Gain 3 dB Q 1\n\
             Channel: L\nFilter: ON PK Fc 100 Hz Gain 4 dB Q 1\n\
             Channel: all\nFilter: ON PK Fc 5000 Hz Gain 1 dB Q 1\n\
             Channel: R SUB\nPreamp: -1 dB\n",
        );
        let (eqs, w) = eqs_from_profile(&p, &layout_for(2), "apo");
        assert_eq!(w, vec!["Channel SUB is not in the channel layout, ignored".to_string()]);
        assert_eq!(eqs.len(), 3);
        assert_eq!(eqs[0].name, "apo (all)");
        assert_eq!(eqs[0].channels, vec![ch("L"), ch("R")]);
        assert_eq!((eqs[0].preamp_db, eqs[0].filters.len()), (-3.0, 2));
        assert_eq!(eqs[1].channels, vec![ch("L")]);
        assert_eq!(eqs[2].channels, vec![ch("R")]);
        assert_eq!(eqs[2].preamp_db, -1.0);
    }

    #[test]
    fn setup_chains_match_profile_chains() {
        let p = parse_str(
            "Preamp: -3 dB\nFilter: ON PK Fc 1000 Hz Gain 3 dB Q 1\n\
             Channel: L\nFilter: ON PK Fc 100 Hz Gain 4 dB Q 1\n\
             Channel: all\nFilter: ON PK Fc 5000 Hz Gain 1 dB Q 1\n\
             Channel: R\nPreamp: -1 dB\nFilter: ON HSC Fc 9000 Hz Gain -2 dB Q 0.7\n",
        );
        let layout = layout_for(2);
        let (setup, _) = Setup::from_profile(&p, &layout, "apo");
        for c in &layout {
            let a = p.chain_for(c);
            let b = setup.chain_for_output(c);
            assert_eq!(a.filters.len(), b.filters.len(), "{c}");
            assert!((a.gain_db - b.gain_db).abs() < 1e-12);
            for f in [30.0, 100.0, 800.0, 1000.0, 5000.0, 12_000.0] {
                let (ra, rb) = (response_db(&a, 48_000.0, f), response_db(&b, 48_000.0, f));
                assert!((ra - rb).abs() < 1e-9, "{c} @ {f}: {ra} vs {rb}");
            }
        }
    }

    #[test]
    fn bypass_keeps_structure_but_neutralises_every_eq() {
        let (mut s, _) = Setup::from_profile(
            &parse_str("Preamp: -4 dB\nFilter: ON PK Fc 1000 Hz Gain 6 dB Q 1\nFilter: ON LP Fc 5000 Hz\n"),
            &layout_for(2),
            "x",
        );
        let normal = s.chain_for_output(&ch("L"));
        assert_eq!((normal.gain_db, normal.filters.len()), (-4.0, 2));
        s.bypass_eq = true;
        let b = s.chain_for_output(&ch("L"));
        assert_eq!(b.filters.len(), 2, "structure unchanged");
        assert!(b.filters.iter().all(|f| !f.enabled));
        assert_eq!(b.gain_db, 0.0);
        for f in [100.0, 1000.0, 8000.0] {
            assert!(response_db(&b, 48_000.0, f).abs() < 1e-9);
        }
        // and the flag round-trips, but only appears in the file when set
        assert!(Setup::from_json(&s.to_json_pretty()).unwrap().bypass_eq);
        s.bypass_eq = false;
        assert!(!s.to_json_pretty().contains("bypass_eq"));
    }

    #[test]
    fn graphic_eq_becomes_filters_on_the_selected_channels() {
        let line: String = (0..40)
            .map(|i| {
                let hz = 20.0 * 1000f64.powf(i as f64 / 39.0);
                format!("{hz:.1} {:.2}", -3.0 + 5.0 * (-((hz / 3000.0).log2() / 0.7).powi(2)).exp())
            })
            .collect::<Vec<_>>()
            .join("; ");
        let p = parse_str(&format!("Channel: L\nGraphicEQ: {line}\n"));
        let (eqs, warnings) = eqs_from_profile(&p, &layout_for(2), "geq");
        assert_eq!(eqs.len(), 1);
        assert_eq!(eqs[0].channels, vec![ch("L")]);
        assert!(eqs[0].filters.len() >= 5 && eqs[0].filters.len() <= 60, "{}", eqs[0].filters.len());
        assert!(warnings.len() == 1 && warnings[0].contains("converted to"), "{warnings:?}");
        // The resulting setup follows the curve: about +2 dB at 3 kHz, about -3 dB at 100 Hz.
        let (s, _) = Setup::from_profile(&p, &layout_for(2), "geq");
        let chain = s.chain_for_output(&ch("L"));
        let at = |hz| response_db(&chain, 48_000.0, hz);
        assert!((at(3000.0) - 2.0).abs() < 0.6, "{}", at(3000.0));
        assert!((at(100.0) + 3.0).abs() < 0.6, "{}", at(100.0));
        assert!(s.chain_for_output(&ch("R")).is_empty(), "the R channel is untouched");
    }

    #[test]
    fn bad_graphic_eq_is_reported_not_fatal() {
        let (eqs, w) = eqs_from_profile(&parse_str("GraphicEQ: 0 3; 100 2\nFilter: ON PK Fc 1000 Hz Gain 3 dB Q 1\n"), &layout_for(2), "x");
        assert_eq!(eqs[0].filters.len(), 1, "the normal filter still comes through");
        assert!(w.iter().any(|m| m.contains("GraphicEQ skipped")), "{w:?}");
    }

    #[test]
    fn disabled_eq_is_ignored() {
        let (mut s, _) = Setup::from_profile(&parse_str("Preamp: -3 dB\n"), &layout_for(2), "x");
        assert_eq!(s.chain_for_output(&ch("L")).gain_db, -3.0);
        s.eqs[0].enabled = false;
        assert_eq!(s.chain_for_output(&ch("L")).gain_db, 0.0);
    }

    #[test]
    fn one_eq_can_target_many_channels_and_stack() {
        let mut s = Setup::passthrough(&layout_for(6));
        let pk = |g| Filter { kind: crate::filter::FilterKind::Peaking, fc_hz: 1000.0, gain_db: g, q: 1.0, enabled: true };
        s.eqs.push(EqPreset { name: "a".into(), enabled: true, preamp_db: -1.0, filters: vec![pk(3.0)], channels: vec![ch("L"), ch("R"), ch("C")] });
        s.eqs.push(EqPreset { name: "b".into(), enabled: true, preamp_db: -2.0, filters: vec![pk(-1.0)], channels: vec![ch("C")] });
        s.validate().unwrap();
        assert_eq!(s.chain_for_output(&ch("L")).filters.len(), 1);
        let c = s.chain_for_output(&ch("C"));
        assert_eq!((c.filters.len(), c.gain_db), (2, -3.0));
        assert!(s.chain_for_output(&ch("SUB")).is_empty());
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("linapo-setup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.json");
        let s = Setup::passthrough(&layout_for(8));
        s.save(&path).unwrap();
        assert_eq!(Setup::load(&path).unwrap(), s);
        assert!(!dir.join("s.json.tmp").exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
