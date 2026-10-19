//! Generate a PipeWire `libpipewire-module-filter-chain` drop-in config from a [`Setup`].
//!
//! PipeWire itself runs the DSP. The graph per setup is:
//!
//! ```text
//! capture ports -> in_<ch> (copy) --\
//!                                    +--> mix_<out> (mixer, 1 gain per input)
//! capture ports -> in_<ch> (copy) --/        -> pre_<out> (linear, EQ preamp)
//!                                            -> eq_<out>_<n> (bq_*)...
//!                                            -> master_<out> (linear) -> playback ports
//! ```
//!
//! Every matrix cell, preamp, filter parameter and master gain is a control
//! on a named node, so values can be changed live (see [`ControlKey`]) without
//! restarting PipeWire. Changing the *structure* (channel layout, number or type of
//! filters, first-order shelf settings) needs a regenerate + restart.
//!
//! Peaking and shelf filters use PipeWire's native `bq_*` nodes (they match the RBJ cookbook
//! exactly and have a Gain control that is neutral at 0 dB). Every other kind (low/high-pass,
//! band-pass, notch, all-pass, 6 dB shelves) is a `bq_raw` node whose coefficients are computed
//! here, because PipeWire 1.0.5's native low/high-pass treat Q differently (a requested 0.7071
//! behaved like 0.90). Raw coefficients are live-editable too, for the rate the graph runs at.
//! A disabled filter (or the setup-wide EQ bypass) stays in the graph with neutral values, so
//! switching it on or off never needs a restart.
//!
//! Not supported on this backend: the limiter (reported as a warning), and
//! more than 8 input channels (the builtin mixer has 8 inputs).

use std::fmt::Write;

use crate::apo::Channel;
use crate::filter::FilterKind;
use crate::matrix::db_to_lin;
use crate::setup::Setup;

/// Maximum number of input channels (builtin `mixer` has 8 inputs).
pub const MAX_INPUTS: usize = 8;

/// Sample rates for which first-order shelves (`bq_raw`) get coefficients.
const RAW_RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

#[derive(Debug, Clone)]
pub struct Options {
    /// Name shown in sound settings.
    pub description: String,
    /// Used in PipeWire node names: `effect_input.<name>` / `effect_output.<name>`.
    pub node_name: String,
    /// Sample rate used to decide whether a filter is usable (Fc below Nyquist).
    pub sample_rate: f64,
    /// Optional `target.object` for the processed audio (device node name or serial).
    pub target: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            description: "LinAPO EQ".to_string(),
            node_name: "linapo".to_string(),
            sample_rate: 48_000.0,
            target: None,
        }
    }
}

impl Options {
    /// Name of the virtual sink node apps play into.
    pub fn sink_node(&self) -> String {
        format!("effect_input.{}", self.node_name)
    }
}

/// Address of a live-editable control: the value to put in a `Props` param.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlKey(pub String);

impl std::fmt::Display for ControlKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn tag(ch: &Channel) -> String {
    ch.name().to_ascii_lowercase()
}

/// Gain of `matrix[input_index][output]` (linear).
pub fn matrix_control(output: &Channel, input_index: usize) -> ControlKey {
    ControlKey(format!("mix_{}:Gain {}", tag(output), input_index + 1))
}

/// Master gain multiplier of one output channel (linear).
pub fn master_control(output: &Channel) -> ControlKey {
    ControlKey(format!("master_{}:Mult", tag(output)))
}

/// Combined EQ preamp multiplier of one output channel (linear).
pub fn preamp_control(output: &Channel) -> ControlKey {
    ControlKey(format!("pre_{}:Mult", tag(output)))
}

/// Parameters of filter number `n` (1-based) of an output channel's chain.
/// `param` is `"Freq"`, `"Q"` or `"Gain"`.
pub fn filter_control(output: &Channel, n: usize, param: &str) -> ControlKey {
    ControlKey(format!("eq_{}_{}:{}", tag(output), n, param))
}

/// PipeWire position name for an APO channel.
pub fn position(ch: &Channel) -> String {
    match ch.name() {
        "L" => "FL".to_string(),
        "R" => "FR".to_string(),
        "C" => "FC".to_string(),
        "SUB" => "LFE".to_string(),
        n @ ("RL" | "RR" | "SL" | "SR") => n.to_string(),
        n => format!("AUX{}", n.trim_start_matches("CH").parse::<u32>().unwrap_or(1).saturating_sub(1)),
    }
}

/// SPA-JSON wants floats to look like floats.
fn num(x: f64) -> String {
    format!("{x:?}")
}

fn positions(chs: &[Channel]) -> String {
    chs.iter().map(position).collect::<Vec<_>>().join(" ")
}

/// Gain a native filter node should have: a disabled filter sits at 0 dB, which is exactly neutral.
fn effective_gain(f: &crate::filter::Filter) -> f64 {
    if f.enabled {
        f.gain_db
    } else {
        0.0
    }
}

/// Coefficients for a `bq_raw` node at `rate`: identity when the filter is disabled.
fn raw_coeffs(f: &crate::filter::Filter, rate: f64) -> Option<crate::biquad::Coeffs> {
    if f.enabled {
        f.coeffs(rate).ok()
    } else {
        Some(crate::biquad::Coeffs::IDENTITY)
    }
}

/// Returns the config text and a list of warnings, or an error if the setup
/// cannot be expressed with PipeWire's builtin nodes.
pub fn generate(setup: &Setup, opts: &Options) -> Result<(String, Vec<String>), String> {
    setup.validate().map_err(|e| e.to_string())?;
    let n_in = setup.input_channels.len();
    if n_in == 0 || setup.output_channels.is_empty() {
        return Err("setup needs at least one input and one output channel".into());
    }
    if n_in > MAX_INPUTS {
        return Err(format!(
            "{n_in} input channels: the PipeWire builtin mixer supports at most {MAX_INPUTS}"
        ));
    }

    let mut warnings = Vec::new();
    if setup.limiter {
        warnings.push("the limiter is not available on the PipeWire filter-chain backend and was ignored".to_string());
    }

    let mut nodes = String::new();
    let mut links = String::new();
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();

    for ch in &setup.input_channels {
        let _ = writeln!(nodes, "                    {{ type = builtin name = in_{} label = copy }}", tag(ch));
        inputs.push(format!("\"in_{}:In\"", tag(ch)));
    }

    for (o, out_ch) in setup.output_channels.iter().enumerate() {
        let t = tag(out_ch);

        // Matrix column -> mixer.
        let mut gains = String::new();
        for k in 0..MAX_INPUTS {
            let g = if k < n_in { setup.matrix.get(k, o) } else { 0.0 };
            let _ = write!(gains, "\"Gain {}\" = {} ", k + 1, num(g));
        }
        let _ = writeln!(nodes, "                    {{ type = builtin name = mix_{t} label = mixer control = {{ {}}} }}", gains);
        for (k, in_ch) in setup.input_channels.iter().enumerate() {
            let _ = writeln!(links, "                    {{ output = \"in_{}:Out\" input = \"mix_{t}:In {}\" }}", tag(in_ch), k + 1);
        }

        // EQ chain.
        let chain = setup.chain_for_output(out_ch);
        let _ = writeln!(
            nodes,
            "                    {{ type = builtin name = pre_{t} label = linear control = {{ \"Mult\" = {} \"Add\" = 0.0 }} }}",
            num(db_to_lin(chain.gain_db))
        );
        let _ = writeln!(links, "                    {{ output = \"mix_{t}:Out\" input = \"pre_{t}:In\" }}");
        let mut prev = format!("pre_{t}");

        let mut n = 0;
        for f in &chain.filters {
            if let Err(e) = f.coeffs(opts.sample_rate) {
                if f.enabled {
                    warnings.push(format!("channel {out_ch}: skipped filter [{f}]: {e}"));
                }
                continue;
            }
            n += 1;
            let name = format!("eq_{t}_{n}");
            let native = |label: &str| {
                format!(
                    "                    {{ type = builtin name = {name} label = {label} control = {{ \"Freq\" = {} \"Q\" = {} \"Gain\" = {} }} }}\n",
                    num(f.fc_hz),
                    num(f.q),
                    num(effective_gain(f))
                )
            };
            nodes.push_str(&match f.kind {
                FilterKind::Peaking => native("bq_peaking"),
                FilterKind::LowShelf => native("bq_lowshelf"),
                FilterKind::HighShelf => native("bq_highshelf"),
                _ => {
                    // bq_raw: coefficients per sample rate in a `config` section (used at start and
                    // on rate changes); live edits set b0..a2 as controls for the running rate.
                    let mut entries = String::new();
                    for rate in RAW_RATES {
                        if let Some(c) = raw_coeffs(f, rate as f64) {
                            let _ = write!(
                                entries,
                                "{{ rate = {rate} b0 = {} b1 = {} b2 = {} a0 = 1.0 a1 = {} a2 = {} }} ",
                                num(c.b0),
                                num(c.b1),
                                num(c.b2),
                                num(c.a1),
                                num(c.a2)
                            );
                        }
                    }
                    format!(
                        "                    {{ type = builtin name = {name} label = bq_raw config = {{ coefficients = [ {entries}] }} }}\n"
                    )
                }
            });
            let _ = writeln!(links, "                    {{ output = \"{prev}:Out\" input = \"{name}:In\" }}");
            prev = name;
        }

        let _ = writeln!(
            nodes,
            "                    {{ type = builtin name = master_{t} label = linear control = {{ \"Mult\" = {} \"Add\" = 0.0 }} }}",
            num(db_to_lin(setup.master_gain_db))
        );
        let _ = writeln!(links, "                    {{ output = \"{prev}:Out\" input = \"master_{t}:In\" }}");
        outputs.push(format!("\"master_{t}:Out\""));
    }

    let desc = opts.description.replace('"', "'");
    let mut out = String::new();
    let _ = writeln!(out, "# Generated by linapo. Do not edit; regenerate with `linapo install`.");
    let _ = writeln!(out, "context.modules = [");
    let _ = writeln!(out, "    {{ name = libpipewire-module-filter-chain");
    let _ = writeln!(out, "        args = {{");
    let _ = writeln!(out, "            node.description = \"{desc}\"");
    let _ = writeln!(out, "            media.name       = \"{desc}\"");
    let _ = writeln!(out, "            filter.graph = {{");
    let _ = writeln!(out, "                nodes = [");
    out.push_str(&nodes);
    let _ = writeln!(out, "                ]");
    let _ = writeln!(out, "                links = [");
    out.push_str(&links);
    let _ = writeln!(out, "                ]");
    let _ = writeln!(out, "                inputs  = [ {} ]", inputs.join(" "));
    let _ = writeln!(out, "                outputs = [ {} ]", outputs.join(" "));
    let _ = writeln!(out, "            }}");
    let _ = writeln!(out, "            capture.props = {{");
    let _ = writeln!(out, "                node.name      = \"effect_input.{}\"", opts.node_name);
    let _ = writeln!(out, "                media.class    = Audio/Sink");
    let _ = writeln!(out, "                audio.channels = {}", setup.input_channels.len());
    let _ = writeln!(out, "                audio.position = [ {} ]", positions(&setup.input_channels));
    let _ = writeln!(out, "            }}");
    let _ = writeln!(out, "            playback.props = {{");
    let _ = writeln!(out, "                node.name      = \"effect_output.{}\"", opts.node_name);
    let _ = writeln!(out, "                node.passive   = true");
    let _ = writeln!(out, "                audio.channels = {}", setup.output_channels.len());
    let _ = writeln!(out, "                audio.position = [ {} ]", positions(&setup.output_channels));
    if let Some(t) = opts.target.as_ref().or(setup.output_target.as_ref()) {
        let _ = writeln!(out, "                target.object  = \"{}\"", t.replace('"', "'"));
    }
    let _ = writeln!(out, "            }}");
    let _ = writeln!(out, "        }}");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out, "]");
    Ok((out, warnings))
}

/// Every adjustable value of the generated graph, as `(control, value)` pairs
/// ready to send in one `Props` param. Mirrors the node numbering of
/// [`generate`], so it is valid as long as [`skeleton`] of the installed
/// config equals the skeleton of a freshly generated one.
pub fn live_controls(setup: &Setup, opts: &Options) -> Result<Vec<(ControlKey, f64)>, String> {
    setup.validate().map_err(|e| e.to_string())?;
    let n_in = setup.input_channels.len();
    if n_in == 0 || setup.output_channels.is_empty() || n_in > MAX_INPUTS {
        return Err("setup cannot be expressed on this backend".into());
    }
    let mut out = Vec::new();
    for (o, ch) in setup.output_channels.iter().enumerate() {
        for k in 0..n_in {
            out.push((matrix_control(ch, k), setup.matrix.get(k, o)));
        }
        let chain = setup.chain_for_output(ch);
        out.push((preamp_control(ch), db_to_lin(chain.gain_db)));
        let mut n = 0;
        for f in &chain.filters {
            if f.coeffs(opts.sample_rate).is_err() {
                continue;
            }
            n += 1;
            match f.kind {
                FilterKind::Peaking | FilterKind::LowShelf | FilterKind::HighShelf => {
                    out.push((filter_control(ch, n, "Freq"), f.fc_hz));
                    out.push((filter_control(ch, n, "Q"), f.q));
                    out.push((filter_control(ch, n, "Gain"), effective_gain(f)));
                }
                _ => {
                    // bq_raw: coefficients for the rate the graph is running at (`opts.sample_rate`).
                    let c = raw_coeffs(f, opts.sample_rate).unwrap_or(crate::biquad::Coeffs::IDENTITY);
                    for (name, v) in [("b0", c.b0), ("b1", c.b1), ("b2", c.b2), ("a0", 1.0), ("a1", c.a1), ("a2", c.a2)] {
                        out.push((filter_control(ch, n, name), v));
                    }
                }
            }
        }
        out.push((master_control(ch), db_to_lin(setup.master_gain_db)));
    }
    Ok(out)
}

/// The config with every `control = { ... }` body and `coefficients = [ ... ]` table removed: what remains is the
/// graph *structure* (nodes, types, links, channel layout). Two configs with
/// equal skeletons differ only in live-editable values.
pub fn skeleton(config: &str) -> String {
    let mut out = String::with_capacity(config.len());
    let mut rest = config;
    while let Some(i) = rest.find("control = {") {
        out.push_str(&rest[..i]);
        out.push_str("control = {}");
        let after = &rest[i + "control = {".len()..];
        match after.find('}') {
            Some(j) => rest = &after[j + 1..],
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    // `bq_raw` coefficient tables are values too (they are live-editable).
    let mut out2 = String::with_capacity(out.len());
    let mut rest = out.as_str();
    while let Some(i) = rest.find("coefficients = [") {
        out2.push_str(&rest[..i]);
        out2.push_str("coefficients = []");
        let after = &rest[i + "coefficients = [".len()..];
        match after.find(']') {
            Some(j) => rest = &after[j + 1..],
            None => rest = "",
        }
    }
    out2.push_str(rest);
    out2.lines().filter(|l| !l.starts_with('#')).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apo::{layout_for, parse_str};
    use crate::filter::{Filter, FilterKind};
    use crate::matrix::Matrix;
    use crate::setup::EqPreset;

    fn ch(s: &str) -> Channel {
        Channel::parse(s).unwrap()
    }

    fn from_apo(text: &str, n: usize) -> Setup {
        let mut s = Setup::from_profile(&parse_str(text), &layout_for(n), "t").0;
        s.limiter = false;
        s
    }

    fn gen(s: &Setup) -> String {
        generate(s, &Options::default()).unwrap().0
    }

    #[test]
    fn stereo_config_has_expected_structure() {
        let cfg = gen(&from_apo(
            "Preamp: -6 dB\nFilter 1: ON PK Fc 1000 Hz Gain 3 dB Q 1.41\nFilter 2: ON LSC Fc 100 Hz Gain 4 dB Q 0.7\n",
            2,
        ));
        assert!(cfg.contains("libpipewire-module-filter-chain"));
        assert!(cfg.contains("name = in_l label = copy"));
        assert!(cfg.contains("name = mix_l label = mixer control = { \"Gain 1\" = 1.0 \"Gain 2\" = 0.0 \"Gain 3\" = 0.0"));
        assert!(cfg.contains("name = mix_r label = mixer control = { \"Gain 1\" = 0.0 \"Gain 2\" = 1.0"));
        assert!(cfg.contains("name = pre_l label = linear control = { \"Mult\" = 0.501187"));
        assert!(cfg.contains("name = eq_l_1 label = bq_peaking control = { \"Freq\" = 1000.0 \"Q\" = 1.41 \"Gain\" = 3.0 }"));
        assert!(cfg.contains("name = eq_r_2 label = bq_lowshelf"));
        assert!(cfg.contains("{ output = \"in_l:Out\" input = \"mix_l:In 1\" }"));
        assert!(cfg.contains("{ output = \"in_r:Out\" input = \"mix_l:In 2\" }"));
        assert!(cfg.contains("{ output = \"mix_l:Out\" input = \"pre_l:In\" }"));
        assert!(cfg.contains("{ output = \"pre_l:Out\" input = \"eq_l_1:In\" }"));
        assert!(cfg.contains("{ output = \"eq_l_2:Out\" input = \"master_l:In\" }"));
        assert!(cfg.contains("inputs  = [ \"in_l:In\" \"in_r:In\" ]"));
        assert!(cfg.contains("outputs = [ \"master_l:Out\" \"master_r:Out\" ]"));
        assert!(cfg.contains("audio.position = [ FL FR ]"));
        assert!(cfg.contains("media.class    = Audio/Sink"));
    }

    #[test]
    fn braces_and_brackets_balance() {
        let cfg = gen(&from_apo("Filter: ON PK Fc 1000 Hz Gain 3 dB Q 1\nFilter: ON LS 6dB Fc 50 Hz Gain 5 dB\n", 6));
        let count = |c: char| cfg.chars().filter(|x| *x == c).count();
        assert_eq!(count('{'), count('}'));
        assert_eq!(count('['), count(']'));
    }

    #[test]
    fn empty_setup_is_a_passthrough_graph() {
        let s = Setup::passthrough(&layout_for(2));
        let cfg = gen(&s);
        assert!(cfg.contains("name = master_l label = linear control = { \"Mult\" = 1.0"));
        assert!(cfg.contains("{ output = \"pre_l:Out\" input = \"master_l:In\" }"));
    }

    #[test]
    fn matrix_routes_and_volumes_land_in_mixer_gains() {
        // stereo in -> 5.1 out, with a -6 dB route and a polarity flip
        let mut s = Setup::passthrough(&layout_for(6));
        s.input_channels = layout_for(2);
        s.matrix = Matrix::zeros(2, 6);
        s.matrix.set(0, 0, 1.0);
        s.matrix.set(1, 1, 1.0);
        s.matrix.set_db(0, 4, -6.0);
        s.matrix.set(1, 4, -0.5);
        s.limiter = false;
        let cfg = gen(&s);
        assert!(cfg.contains("name = mix_rl label = mixer control = { \"Gain 1\" = 0.501187"));
        assert!(cfg.contains("\"Gain 2\" = -0.5 "));
        assert!(cfg.contains("name = mix_c label = mixer control = { \"Gain 1\" = 0.0 \"Gain 2\" = 0.0 "));
        // capture side has 2 channels, playback side 6
        assert!(cfg.contains("audio.channels = 2"));
        assert!(cfg.contains("audio.channels = 6"));
        assert!(cfg.contains("audio.position = [ FL FR ]"));
        assert!(cfg.contains("audio.position = [ FL FR FC LFE RL RR ]"));
        assert!(cfg.contains("{ output = \"in_r:Out\" input = \"mix_rl:In 2\" }"));
    }

    #[test]
    fn eq_assignment_is_per_output_channel() {
        let mut s = Setup::passthrough(&layout_for(2));
        s.limiter = false;
        let pk = |f| Filter { kind: FilterKind::Peaking, fc_hz: f, gain_db: 3.0, q: 1.0, enabled: true };
        s.eqs.push(EqPreset { name: "a".into(), enabled: true, preamp_db: 0.0, filters: vec![pk(100.0)], channels: vec![ch("L")] });
        s.eqs.push(EqPreset { name: "b".into(), enabled: true, preamp_db: 0.0, filters: vec![pk(200.0)], channels: vec![ch("L"), ch("R")] });
        let cfg = gen(&s);
        assert!(cfg.contains("name = eq_l_1 label = bq_peaking control = { \"Freq\" = 100.0"));
        assert!(cfg.contains("name = eq_l_2 label = bq_peaking control = { \"Freq\" = 200.0"));
        assert!(cfg.contains("name = eq_r_1 label = bq_peaking control = { \"Freq\" = 200.0"));
        assert!(!cfg.contains("eq_r_2"));
    }

    #[test]
    fn master_gain_goes_to_every_output() {
        let mut s = Setup::passthrough(&layout_for(2));
        s.master_gain_db = -6.0;
        s.limiter = false;
        let cfg = gen(&s);
        assert_eq!(cfg.matches("\"Mult\" = 0.501187").count(), 2);
    }

    #[test]
    fn first_order_shelf_becomes_bq_raw_and_bad_filters_are_skipped() {
        let (cfg, warnings) = generate(
            &from_apo("Filter: ON LS 6dB Fc 50 Hz Gain 5 dB\nFilter: ON PK Fc 30000 Hz Gain 3 dB Q 1\n", 2),
            &Options::default(),
        )
        .unwrap();
        assert!(cfg.contains("label = bq_raw config = { coefficients = [ { rate = 44100 "));
        for r in ["44100", "48000", "96000", "192000"] {
            assert!(cfg.contains(&format!("rate = {r} ")), "{r}");
        }
        assert!(cfg.contains("a0 = 1.0"));
        assert_eq!(warnings.len(), 2); // once per channel
        assert!(!cfg.contains("30000"));
    }

    #[test]
    fn first_order_shelf_coefficients_follow_the_sample_rate() {
        let (cfg, _) = generate(&from_apo("Filter: ON LS 6dB Fc 100 Hz Gain 5 dB\n", 2), &Options::default()).unwrap();
        let line = cfg.lines().find(|l| l.contains("name = eq_l_1")).unwrap();
        let b0_of = |rate: &str| -> String {
            let i = line.find(&format!("rate = {rate} b0 = ")).unwrap();
            line[i..].split_whitespace().nth(5).unwrap().to_string()
        };
        assert_ne!(b0_of("44100"), b0_of("96000"));
    }

    #[test]
    fn non_gain_kinds_use_exact_raw_coefficients_not_pipewires_native_nodes() {
        let (cfg, _) = generate(
            &from_apo("Filter: ON LP Fc 1000 Hz\nFilter: ON HPQ Fc 80 Hz Q 0.9\nFilter: ON BP Fc 500 Hz Q 2\nFilter: ON PK Fc 200 Hz Gain 2 dB Q 1\n", 2),
            &Options::default(),
        )
        .unwrap();
        for bad in ["bq_lowpass", "bq_highpass", "bq_bandpass", "bq_notch", "bq_allpass"] {
            assert!(!cfg.contains(bad), "{bad}");
        }
        for n in 1..=3 {
            assert!(cfg.contains(&format!("name = eq_l_{n} label = bq_raw")), "{n}");
        }
        assert!(cfg.contains("name = eq_l_4 label = bq_peaking"));
        // low-pass is now live-editable through its coefficients
        let ctl = live_controls(&from_apo("Filter: ON LP Fc 1000 Hz\n", 2), &Options::default()).unwrap();
        assert!(ctl.iter().any(|(k, _)| k.0 == "eq_l_1:b0"));
    }

    #[test]
    fn live_raw_coefficients_match_the_filter_at_the_requested_rate() {
        let s = from_apo("Filter: ON LP Fc 1000 Hz\n", 2);
        for rate in [44_100.0, 48_000.0, 96_000.0] {
            let opts = Options { sample_rate: rate, ..Options::default() };
            let ctl = live_controls(&s, &opts).unwrap();
            let get = |k: &str| ctl.iter().find(|(c, _)| c.0 == k).unwrap().1;
            let want = s.eqs[0].filters[0].coeffs(rate).unwrap();
            assert_eq!((get("eq_l_1:b0"), get("eq_l_1:a1"), get("eq_l_1:a2")), (want.b0, want.a1, want.a2));
        }
    }

    #[test]
    fn disabled_filters_stay_in_the_graph_with_neutral_values() {
        let mut s = from_apo("Filter: ON PK Fc 1000 Hz Gain 6 dB Q 1\nFilter: ON HP Fc 80 Hz\n", 2);
        s.eqs[0].filters[0].enabled = false;
        s.eqs[0].filters[1].enabled = false;
        let opts = Options::default();
        let (cfg, w) = generate(&s, &opts).unwrap();
        assert!(w.is_empty());
        assert!(cfg.contains("name = eq_l_1 label = bq_peaking control = { \"Freq\" = 1000.0 \"Q\" = 1.0 \"Gain\" = 0.0 }"), "gain 0 = neutral");
        assert!(cfg.contains("rate = 48000 b0 = 1.0 b1 = 0.0 b2 = 0.0 a0 = 1.0 a1 = 0.0 a2 = 0.0"), "identity coefficients");
        let ctl = live_controls(&s, &opts).unwrap();
        let get = |k: &str| ctl.iter().find(|(c, _)| c.0 == k).unwrap().1;
        assert_eq!(get("eq_l_1:Gain"), 0.0);
        assert_eq!((get("eq_l_2:b0"), get("eq_l_2:b1"), get("eq_l_2:a1")), (1.0, 0.0, 0.0));
    }

    #[test]
    fn toggling_a_filter_or_the_bypass_never_changes_the_structure() {
        let on = from_apo("Preamp: -3 dB\nFilter: ON PK Fc 1000 Hz Gain 6 dB Q 1\nFilter: ON LP Fc 5000 Hz\nFilter: ON NO Fc 60 Hz Q 5\n", 2);
        let opts = Options::default();
        let sk = |s: &Setup| skeleton(&generate(s, &opts).unwrap().0);
        for i in 0..3 {
            let mut off = on.clone();
            off.eqs[0].filters[i].enabled = false;
            assert_eq!(sk(&on), sk(&off), "filter {i}");
        }
        let mut b = on.clone();
        b.bypass_eq = true;
        assert_eq!(sk(&on), sk(&b), "bypass");
        // but changing a low-pass frequency (a coefficient table value) is a value, not structure
        let mut moved = on.clone();
        moved.eqs[0].filters[1].fc_hz = 9000.0;
        assert_eq!(sk(&on), sk(&moved));
        // while turning a peak into a low-pass is a different node type
        let mut kind = on.clone();
        kind.eqs[0].filters[0].kind = FilterKind::LowPass;
        assert_ne!(sk(&on), sk(&kind));
    }

    #[test]
    fn limiter_is_reported_as_unsupported() {
        let s = Setup::passthrough(&layout_for(2)); // limiter on by default
        let (_, w) = generate(&s, &Options::default()).unwrap();
        assert!(w.iter().any(|m| m.contains("limiter")));
    }

    #[test]
    fn too_many_inputs_is_an_error() {
        let s = Setup::passthrough(&layout_for(10));
        assert!(generate(&s, &Options::default()).unwrap_err().contains("at most 8"));
    }

    #[test]
    fn target_and_names_are_configurable() {
        let s = Setup::passthrough(&layout_for(2));
        let opts = Options { description: "My \"EQ\"".into(), node_name: "mine".into(), target: Some("alsa_output.pci".into()), ..Options::default() };
        let (cfg, _) = generate(&s, &opts).unwrap();
        assert!(cfg.contains("node.name      = \"effect_input.mine\""));
        assert!(cfg.contains("target.object  = \"alsa_output.pci\""));
        assert!(cfg.contains("node.description = \"My 'EQ'\""));
        assert_eq!(opts.sink_node(), "effect_input.mine");
    }

    #[test]
    fn setup_output_target_is_used_unless_overridden() {
        let mut s = Setup::passthrough(&layout_for(2));
        s.output_target = Some("alsa_output.usb".into());
        let (cfg, _) = generate(&s, &Options::default()).unwrap();
        assert!(cfg.contains("target.object  = \"alsa_output.usb\""));
        let opts = Options { target: Some("other".into()), ..Options::default() };
        assert!(generate(&s, &opts).unwrap().0.contains("target.object  = \"other\""));
        // and a different device is a structural change
        let a = skeleton(&generate(&Setup::passthrough(&layout_for(2)), &Options::default()).unwrap().0);
        assert_ne!(a, skeleton(&generate(&s, &Options::default()).unwrap().0));
    }

    #[test]
    fn control_keys() {
        assert_eq!(matrix_control(&ch("R"), 0).to_string(), "mix_r:Gain 1");
        assert_eq!(master_control(&ch("SUB")).to_string(), "master_sub:Mult");
        assert_eq!(preamp_control(&ch("L")).to_string(), "pre_l:Mult");
        assert_eq!(filter_control(&ch("L"), 3, "Freq").to_string(), "eq_l_3:Freq");
    }

    #[test]
    fn position_names() {
        assert_eq!(position(&ch("L")), "FL");
        assert_eq!(position(&ch("SUB")), "LFE");
        assert_eq!(position(&ch("RL")), "RL");
        assert_eq!(position(&ch("CH1")), "AUX0");
    }

    #[test]
    fn live_controls_cover_every_value_and_match_config() {
        let s = from_apo(
            "Preamp: -6 dB\nFilter: ON PK Fc 1000 Hz Gain 3 dB Q 1.41\nFilter: ON LS 6dB Fc 80 Hz Gain 4 dB\n\
             Filter: ON PK Fc 30000 Hz Gain 3 dB Q 1\n",
            2,
        );
        let opts = Options::default();
        let ctl = live_controls(&s, &opts).unwrap();
        let get = |k: &str| ctl.iter().find(|(c, _)| c.0 == k).map(|(_, v)| *v);
        assert_eq!(get("mix_l:Gain 1"), Some(1.0));
        assert_eq!(get("mix_l:Gain 2"), Some(0.0));
        assert!((get("pre_l:Mult").unwrap() - 0.501187).abs() < 1e-5);
        assert_eq!(get("eq_l_1:Freq"), Some(1000.0));
        assert_eq!(get("eq_l_1:Gain"), Some(3.0));
        assert!(get("eq_l_2:b0").is_some() && get("eq_l_2:a0") == Some(1.0), "first-order shelves are live via raw coefficients");
        assert_eq!(get("eq_l_3:Freq"), None, "the invalid 30 kHz filter is skipped, like in generate()");
        assert_eq!(get("master_r:Mult"), Some(1.0));
        // every control key refers to a node that exists in the generated config
        let (cfg, _) = generate(&s, &opts).unwrap();
        for (k, _) in &ctl {
            let node = k.0.split(':').next().unwrap();
            assert!(cfg.contains(&format!("name = {node} ")), "{k}");
        }
    }

    #[test]
    fn skeleton_ignores_values_but_not_structure() {
        let a = from_apo("Filter: ON PK Fc 1000 Hz Gain 3 dB Q 1\n", 2);
        let mut b = a.clone();
        b.eqs[0].filters[0].gain_db = -4.0;
        b.eqs[0].filters[0].fc_hz = 2500.0;
        b.master_gain_db = -3.0;
        b.matrix.set(0, 1, 0.3);
        let opts = Options::default();
        let sk = |s: &Setup| skeleton(&generate(s, &opts).unwrap().0);
        assert_eq!(sk(&a), sk(&b));

        let mut c = a.clone();
        let first = c.eqs[0].filters[0];
        c.eqs[0].filters.push(first);
        assert_ne!(sk(&a), sk(&c), "an extra filter changes the structure");
        let mut d = a.clone();
        d.eqs[0].filters[0].kind = FilterKind::LowShelf;
        assert_ne!(sk(&a), sk(&d), "a different filter type changes the structure");
        assert_ne!(sk(&a), sk(&from_apo("", 6)), "channel layout changes the structure");
    }
}
