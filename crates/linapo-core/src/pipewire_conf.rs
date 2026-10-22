//! Generate a PipeWire `libpipewire-module-filter-chain` drop-in config from a [`Setup`].
//!
//! PipeWire itself runs the DSP. The graph per setup is:
//!
//! ```text
//! capture ports -> in_<ch> (copy) --\
//!                                    +--> mix_<out> (mixer, 1 gain per input)
//! capture ports -> in_<ch> (copy) --/        -> pre_<out> (linear, EQ preamp)
//!                                            -> eq_<out>_1 .. eq_<out>_<slots> (bq_raw)
//!                                            -> master_<out> (linear) -> playback ports
//! ```
//!
//! # Fixed slots: switching profiles never restarts anything
//!
//! Every output channel has the same number of filter *slots* (at least [`MIN_SLOTS`]). Every
//! slot is a `bq_raw` node whose coefficients this crate computes (so every filter type matches
//! the RBJ cookbook and the engine exactly, which PipeWire's native low/high-pass did not). Slots
//! a profile does not use hold neutral coefficients, and so do disabled filters and the setup-wide
//! EQ bypass. Because the *structure* only depends on the channel layout, the output device and
//! the slot count, you can change any filter, switch the whole profile, or disable things, live;
//! only a layout change, a different output device, or a profile needing more slots than the
//! installed graph has requires a restart.
//!
//! Coefficients are pushed for the sample rate the graph is running at (the caller passes it in
//! [`Options::sample_rate`]); the config file carries tables for the usual rates, used at start.
//!
//! Not supported on this backend: the limiter (reported as a warning), and more than 8 input
//! channels (the builtin mixer has 8 inputs).

use std::fmt::Write;

use crate::apo::Channel;
use crate::biquad::Coeffs;
use crate::filter::Filter;
use crate::matrix::db_to_lin;
use crate::setup::Setup;

/// Maximum number of input channels (builtin `mixer` has 8 inputs).
pub const MAX_INPUTS: usize = 8;

/// Fewest filter slots per output channel.
pub const MIN_SLOTS: usize = 16;
/// Slot counts are rounded up to a multiple of this, so a slightly bigger profile does not
/// force a restart.
pub const SLOT_STEP: usize = 8;

/// Sample rates for which every slot gets a coefficient table in the config.
const RAW_RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

#[derive(Debug, Clone)]
pub struct Options {
    /// Name shown in sound settings.
    pub description: String,
    /// Used in PipeWire node names: `effect_input.<name>` / `effect_output.<name>`.
    pub node_name: String,
    /// Sample rate coefficients are computed for when pushed live, and used to decide whether a
    /// filter is usable (Fc below Nyquist).
    pub sample_rate: f64,
    /// Optional `target.object` for the processed audio (device node name or serial).
    pub target: Option<String>,
    /// Filter slots per output channel. `None` picks the smallest standard size that fits `setup`.
    pub slots: Option<usize>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            description: "LinAPO EQ".to_string(),
            node_name: "linapo".to_string(),
            sample_rate: 48_000.0,
            target: None,
            slots: None,
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

/// Coefficients for one slot at `rate`: neutral when the slot is unused or its filter disabled.
fn slot_coeffs(f: Option<&Filter>, rate: f64) -> Option<Coeffs> {
    match f {
        Some(f) if f.enabled => f.coeffs(rate).ok(),
        _ => Some(Coeffs::IDENTITY),
    }
}

/// The filters of one output channel that can be realised at `rate`, in order. Filters whose
/// frequency is unusable are dropped (with a warning when they were enabled).
fn usable_filters(setup: &Setup, ch: &Channel, rate: f64, warnings: &mut Vec<String>) -> Vec<Filter> {
    let mut out = Vec::new();
    for f in setup.chain_for_output(ch).filters {
        match f.coeffs(rate) {
            Ok(_) => out.push(f),
            Err(e) => {
                if f.enabled {
                    warnings.push(format!("channel {ch}: skipped filter [{f}]: {e}"));
                }
            }
        }
    }
    out
}

/// The largest number of filters any output channel needs.
pub fn required_slots(setup: &Setup, sample_rate: f64) -> usize {
    let mut ignored = Vec::new();
    setup.output_channels.iter().map(|c| usable_filters(setup, c, sample_rate, &mut ignored).len()).max().unwrap_or(0)
}

/// The standard slot count that fits `required` filters.
pub fn slots_for(required: usize) -> usize {
    required.div_ceil(SLOT_STEP).max(MIN_SLOTS / SLOT_STEP) * SLOT_STEP
}

fn resolve_slots(setup: &Setup, opts: &Options) -> usize {
    opts.slots.unwrap_or_else(|| slots_for(required_slots(setup, opts.sample_rate)))
}

/// Number of filter slots per channel in an installed config (`0` if it has none).
pub fn installed_slots(config: &str) -> usize {
    config
        .match_indices("name = eq_")
        .filter_map(|(i, _)| {
            let rest = &config[i + "name = eq_".len()..];
            let name: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
            name.rsplit('_').next()?.parse::<usize>().ok()
        })
        .max()
        .unwrap_or(0)
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
        return Err(format!("{n_in} input channels: the PipeWire builtin mixer supports at most {MAX_INPUTS}"));
    }
    let slots = resolve_slots(setup, opts);

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

        // EQ preamp, then the fixed filter slots.
        let chain = setup.chain_for_output(out_ch);
        let _ = writeln!(
            nodes,
            "                    {{ type = builtin name = pre_{t} label = linear control = {{ \"Mult\" = {} \"Add\" = 0.0 }} }}",
            num(db_to_lin(chain.gain_db))
        );
        let _ = writeln!(links, "                    {{ output = \"mix_{t}:Out\" input = \"pre_{t}:In\" }}");
        let mut prev = format!("pre_{t}");

        let mut filters = usable_filters(setup, out_ch, opts.sample_rate, &mut warnings);
        if filters.len() > slots {
            warnings.push(format!("channel {out_ch}: {} filters but only {slots} slots; the last {} are ignored", filters.len(), filters.len() - slots));
            filters.truncate(slots);
        }
        for n in 1..=slots {
            let name = format!("eq_{t}_{n}");
            let f = filters.get(n - 1);
            let mut entries = String::new();
            for rate in RAW_RATES {
                if let Some(c) = slot_coeffs(f, rate as f64) {
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
            let _ = writeln!(
                nodes,
                "                    {{ type = builtin name = {name} label = bq_raw config = {{ coefficients = [ {entries}] }} }}"
            );
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

/// Every adjustable value of the generated graph, as `(control, value)` pairs ready to send in
/// one `Props` param. Valid as long as [`skeleton`] of the installed config equals the skeleton of
/// a freshly generated one (with the installed slot count in `opts.slots`).
pub fn live_controls(setup: &Setup, opts: &Options) -> Result<Vec<(ControlKey, f64)>, String> {
    setup.validate().map_err(|e| e.to_string())?;
    let n_in = setup.input_channels.len();
    if n_in == 0 || setup.output_channels.is_empty() || n_in > MAX_INPUTS {
        return Err("setup cannot be expressed on this backend".into());
    }
    let slots = resolve_slots(setup, opts);
    let mut out = Vec::new();
    for (o, ch) in setup.output_channels.iter().enumerate() {
        for k in 0..n_in {
            out.push((matrix_control(ch, k), setup.matrix.get(k, o)));
        }
        let chain = setup.chain_for_output(ch);
        out.push((preamp_control(ch), db_to_lin(chain.gain_db)));
        let mut ignored = Vec::new();
        let mut filters = usable_filters(setup, ch, opts.sample_rate, &mut ignored);
        filters.truncate(slots);
        for n in 1..=slots {
            let c = slot_coeffs(filters.get(n - 1), opts.sample_rate).unwrap_or(Coeffs::IDENTITY);
            for (name, v) in [("b0", c.b0), ("b1", c.b1), ("b2", c.b2), ("a0", 1.0), ("a1", c.a1), ("a2", c.a2)] {
                out.push((filter_control(ch, n, name), v));
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
    use crate::filter::FilterKind;
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

    fn line<'a>(cfg: &'a str, node: &str) -> &'a str {
        cfg.lines().find(|l| l.contains(&format!("name = {node} "))).unwrap_or_else(|| panic!("no node {node}"))
    }

    fn pk(fc: f64, g: f64) -> Filter {
        Filter { kind: FilterKind::Peaking, fc_hz: fc, gain_db: g, q: 1.0, enabled: true }
    }

    const IDENTITY_48K: &str = "rate = 48000 b0 = 1.0 b1 = 0.0 b2 = 0.0 a0 = 1.0 a1 = 0.0 a2 = 0.0";

    #[test]
    fn stereo_config_has_expected_structure() {
        let cfg = gen(&from_apo("Preamp: -6 dB\nFilter 1: ON PK Fc 1000 Hz Gain 3 dB Q 1.41\nFilter 2: ON LSC Fc 100 Hz Gain 4 dB Q 0.7\n", 2));
        assert!(cfg.contains("libpipewire-module-filter-chain"));
        assert!(cfg.contains("name = in_l label = copy"));
        assert!(cfg.contains("name = mix_l label = mixer control = { \"Gain 1\" = 1.0 \"Gain 2\" = 0.0 \"Gain 3\" = 0.0"));
        assert!(cfg.contains("name = mix_r label = mixer control = { \"Gain 1\" = 0.0 \"Gain 2\" = 1.0"));
        assert!(cfg.contains("name = pre_l label = linear control = { \"Mult\" = 0.501187"));
        assert!(cfg.contains("{ output = \"in_l:Out\" input = \"mix_l:In 1\" }"));
        assert!(cfg.contains("{ output = \"in_r:Out\" input = \"mix_l:In 2\" }"));
        assert!(cfg.contains("{ output = \"mix_l:Out\" input = \"pre_l:In\" }"));
        assert!(cfg.contains("{ output = \"pre_l:Out\" input = \"eq_l_1:In\" }"));
        assert!(cfg.contains("{ output = \"eq_l_1:Out\" input = \"eq_l_2:In\" }"));
        assert!(cfg.contains(&format!("{{ output = \"eq_r_{MIN_SLOTS}:Out\" input = \"master_r:In\" }}")));
        assert!(cfg.contains("inputs  = [ \"in_l:In\" \"in_r:In\" ]"));
        assert!(cfg.contains("outputs = [ \"master_l:Out\" \"master_r:Out\" ]"));
        assert!(cfg.contains("audio.position = [ FL FR ]"));
        assert!(cfg.contains("media.class    = Audio/Sink"));
        // slot 1 carries the peaking filter, slot 3 is neutral
        assert!(!line(&cfg, "eq_l_1").contains(IDENTITY_48K));
        assert!(line(&cfg, "eq_l_3").contains(IDENTITY_48K));
    }

    #[test]
    fn every_filter_is_a_raw_biquad_and_no_native_filter_nodes_remain() {
        let cfg = gen(&from_apo("Filter: ON PK Fc 1000 Hz Gain 3 dB Q 1\nFilter: ON LP Fc 5000 Hz\nFilter: ON NO Fc 60 Hz Q 5\n", 2));
        for native in ["bq_peaking", "bq_lowshelf", "bq_highshelf", "bq_lowpass", "bq_highpass", "bq_bandpass", "bq_notch", "bq_allpass"] {
            assert!(!cfg.contains(native), "{native}");
        }
        assert_eq!(cfg.matches("label = bq_raw").count(), 2 * MIN_SLOTS);
    }

    #[test]
    fn braces_and_brackets_balance() {
        let cfg = gen(&from_apo("Filter: ON PK Fc 1000 Hz Gain 3 dB Q 1\nFilter: ON LS 6dB Fc 50 Hz Gain 5 dB\n", 6));
        let count = |c: char| cfg.chars().filter(|x| *x == c).count();
        assert_eq!(count('{'), count('}'));
        assert_eq!(count('['), count(']'));
    }

    #[test]
    fn empty_setup_is_a_passthrough_graph_with_neutral_slots() {
        let cfg = gen(&Setup::passthrough(&layout_for(2)));
        assert!(cfg.contains("name = master_l label = linear control = { \"Mult\" = 1.0"));
        for n in 1..=MIN_SLOTS {
            let l = line(&cfg, &format!("eq_l_{n}"));
            assert!(l.contains(IDENTITY_48K) && l.contains("rate = 44100") && l.contains("rate = 192000"), "slot {n}");
        }
    }

    #[test]
    fn matrix_routes_and_volumes_land_in_mixer_gains() {
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
        assert!(cfg.contains("audio.channels = 2") && cfg.contains("audio.channels = 6"));
        assert!(cfg.contains("audio.position = [ FL FR ]"));
        assert!(cfg.contains("audio.position = [ FL FR FC LFE RL RR ]"));
        assert!(cfg.contains("{ output = \"in_r:Out\" input = \"mix_rl:In 2\" }"));
    }

    #[test]
    fn eq_assignment_is_per_output_channel() {
        let mut s = Setup::passthrough(&layout_for(2));
        s.limiter = false;
        s.eqs.push(EqPreset { name: "a".into(), enabled: true, preamp_db: 0.0, filters: vec![pk(100.0, 3.0)], channels: vec![ch("L")] });
        s.eqs.push(EqPreset { name: "b".into(), enabled: true, preamp_db: 0.0, filters: vec![pk(200.0, 3.0)], channels: vec![ch("L"), ch("R")] });
        let cfg = gen(&s);
        let b0 = |node: &str| {
            let l = line(&cfg, node);
            let i = l.find("rate = 48000 b0 = ").unwrap() + "rate = 48000 b0 = ".len();
            l[i..].split_whitespace().next().unwrap().to_string()
        };
        assert_ne!(b0("eq_l_1"), "1.0");
        assert_ne!(b0("eq_l_2"), "1.0");
        assert_ne!(b0("eq_r_1"), "1.0", "R has the shared EQ");
        assert_eq!(b0("eq_r_2"), "1.0", "but not the L-only one");
        assert_ne!(b0("eq_l_1"), b0("eq_r_1"), "L's first filter is 100 Hz, R's is 200 Hz");
    }

    #[test]
    fn master_gain_goes_to_every_output() {
        let mut s = Setup::passthrough(&layout_for(2));
        s.master_gain_db = -6.0;
        s.limiter = false;
        assert_eq!(gen(&s).matches("\"Mult\" = 0.501187").count(), 2);
    }

    #[test]
    fn unusable_filters_are_skipped_with_a_warning_and_do_not_use_a_slot() {
        let (cfg, w) = generate(&from_apo("Filter: ON PK Fc 30000 Hz Gain 3 dB Q 1\nFilter: ON PK Fc 500 Hz Gain 3 dB Q 1\n", 2), &Options::default()).unwrap();
        assert_eq!(w.len(), 2); // once per channel
        assert!(!line(&cfg, "eq_l_1").contains(IDENTITY_48K), "the 500 Hz filter moved up into slot 1");
        assert!(line(&cfg, "eq_l_2").contains(IDENTITY_48K));
    }

    #[test]
    fn limiter_is_reported_as_unsupported() {
        let (_, w) = generate(&Setup::passthrough(&layout_for(2)), &Options::default()).unwrap();
        assert!(w.iter().any(|m| m.contains("limiter")));
    }

    #[test]
    fn too_many_inputs_is_an_error() {
        assert!(generate(&Setup::passthrough(&layout_for(10)), &Options::default()).unwrap_err().contains("at most 8"));
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
        let a = skeleton(&generate(&Setup::passthrough(&layout_for(2)), &Options::default()).unwrap().0);
        assert_ne!(a, skeleton(&generate(&s, &Options::default()).unwrap().0), "a different device is structural");
    }

    #[test]
    fn control_keys() {
        assert_eq!(matrix_control(&ch("R"), 0).to_string(), "mix_r:Gain 1");
        assert_eq!(master_control(&ch("SUB")).to_string(), "master_sub:Mult");
        assert_eq!(preamp_control(&ch("L")).to_string(), "pre_l:Mult");
        assert_eq!(filter_control(&ch("L"), 3, "b0").to_string(), "eq_l_3:b0");
    }

    #[test]
    fn position_names() {
        assert_eq!(position(&ch("L")), "FL");
        assert_eq!(position(&ch("SUB")), "LFE");
        assert_eq!(position(&ch("RL")), "RL");
        assert_eq!(position(&ch("CH1")), "AUX0");
    }

    // ------------------------------------------------------------ slots

    #[test]
    fn slot_counts() {
        assert_eq!(slots_for(0), 16);
        assert_eq!(slots_for(16), 16);
        assert_eq!(slots_for(17), 24);
        assert_eq!(slots_for(31), 32);
        assert_eq!(slots_for(40), 40);
        let s = from_apo("Filter: ON PK Fc 1000 Hz Gain 3 dB Q 1\nChannel: R\nFilter: ON PK Fc 2000 Hz Gain 3 dB Q 1\nFilter: ON PK Fc 3000 Hz Gain 3 dB Q 1\n", 2);
        assert_eq!(required_slots(&s, 48_000.0), 3, "the busiest channel counts");
    }

    #[test]
    fn installed_slot_count_is_read_back_from_a_config() {
        let s = from_apo("Filter: ON PK Fc 1000 Hz Gain 3 dB Q 1\n", 6);
        assert_eq!(installed_slots(&gen(&s)), MIN_SLOTS);
        let opts = Options { slots: Some(24), ..Options::default() };
        assert_eq!(installed_slots(&generate(&s, &opts).unwrap().0), 24);
        assert_eq!(installed_slots("nothing here"), 0);
    }

    #[test]
    fn a_profile_with_more_filters_than_slots_warns_and_is_truncated() {
        let mut s = Setup::passthrough(&layout_for(2));
        s.limiter = false;
        s.eqs.push(EqPreset { name: "big".into(), enabled: true, preamp_db: 0.0, filters: (0..20).map(|i| pk(100.0 + 50.0 * i as f64, 2.0)).collect(), channels: layout_for(2) });
        let (_, w) = generate(&s, &Options { slots: Some(16), ..Options::default() }).unwrap();
        assert_eq!(w.iter().filter(|m| m.contains("only 16 slots")).count(), 2);
        // with automatic sizing it simply gets 24 slots
        let (cfg, w) = generate(&s, &Options::default()).unwrap();
        assert!(w.is_empty() && cfg.contains("name = eq_l_24 "));
    }

    #[test]
    fn live_controls_cover_matrix_preamp_every_slot_and_master() {
        let s = from_apo("Preamp: -6 dB\nFilter: ON PK Fc 1000 Hz Gain 6 dB Q 1\nFilter: ON LP Fc 5000 Hz\n", 2);
        let opts = Options::default();
        let ctl = live_controls(&s, &opts).unwrap();
        assert_eq!(ctl.len(), 2 * (2 + 1 + MIN_SLOTS * 6 + 1));
        let get = |k: &str| ctl.iter().find(|(c, _)| c.0 == k).map(|(_, v)| *v).unwrap_or_else(|| panic!("{k}"));
        assert_eq!(get("mix_l:Gain 1"), 1.0);
        assert!((get("pre_l:Mult") - 0.501187).abs() < 1e-5);
        let want = s.eqs[0].filters[0].coeffs(48_000.0).unwrap();
        assert_eq!((get("eq_l_1:b0"), get("eq_l_1:a1"), get("eq_l_1:a2")), (want.b0, want.a1, want.a2));
        let lp = s.eqs[0].filters[1].coeffs(48_000.0).unwrap();
        assert_eq!(get("eq_r_2:b1"), lp.b1);
        assert_eq!((get("eq_l_3:b0"), get("eq_l_3:b1"), get("eq_l_3:a1")), (1.0, 0.0, 0.0), "unused slot is neutral");
        assert_eq!(get("eq_l_1:a0"), 1.0);
        assert_eq!(get("master_r:Mult"), 1.0);
        // every key names a node that exists in the config
        let cfg = gen(&s);
        for (k, _) in &ctl {
            let node = k.0.split(':').next().unwrap();
            assert!(cfg.contains(&format!("name = {node} ")), "{k}");
        }
    }

    #[test]
    fn live_coefficients_follow_the_requested_rate() {
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
    fn disabled_filters_and_bypass_become_neutral_slots_without_changing_the_graph() {
        let mut s = from_apo("Filter: ON PK Fc 1000 Hz Gain 6 dB Q 1\nFilter: ON HP Fc 80 Hz\n", 2);
        let opts = Options::default();
        let neutral = |s: &Setup, node: &str| {
            let ctl = live_controls(s, &opts).unwrap();
            let get = |p: &str| ctl.iter().find(|(c, _)| c.0 == format!("{node}:{p}")).unwrap().1;
            (get("b0"), get("b1"), get("b2"), get("a1"), get("a2")) == (1.0, 0.0, 0.0, 0.0, 0.0)
        };
        assert!(!neutral(&s, "eq_l_1") && !neutral(&s, "eq_l_2"));
        s.eqs[0].filters[0].enabled = false;
        assert!(neutral(&s, "eq_l_1") && !neutral(&s, "eq_l_2"));
        s.eqs[0].filters[0].enabled = true;
        s.bypass_eq = true;
        assert!(neutral(&s, "eq_l_1") && neutral(&s, "eq_l_2"));
        let (cfg, w) = generate(&s, &opts).unwrap();
        assert!(w.is_empty() && line(&cfg, "eq_l_1").contains(IDENTITY_48K));
    }

    #[test]
    fn changing_filters_or_switching_profiles_never_changes_the_structure_within_capacity() {
        let a = from_apo("Preamp: -3 dB\nFilter: ON PK Fc 1000 Hz Gain 6 dB Q 1\nFilter: ON LP Fc 5000 Hz\n", 2);
        let b = from_apo("Filter: ON HSC Fc 8000 Hz Gain -4 dB Q 0.7\nFilter: ON NO Fc 60 Hz Q 5\nFilter: ON PK Fc 200 Hz Gain 2 dB Q 2\nFilter: ON AP Fc 500 Hz Q 1\n", 2);
        let opts = Options::default();
        let sk = |s: &Setup| skeleton(&generate(s, &opts).unwrap().0);
        assert_eq!(sk(&a), sk(&b), "a completely different EQ: same graph");
        let mut c = a.clone();
        c.bypass_eq = true;
        c.master_gain_db = -3.0;
        c.matrix.set(0, 1, 0.3);
        c.eqs[0].filters[0].enabled = false;
        c.eqs[0].filters[1].kind = FilterKind::HighPass;
        assert_eq!(sk(&a), sk(&c));
        assert_eq!(sk(&a), sk(&Setup::passthrough(&layout_for(2))), "even an empty profile");
    }

    #[test]
    fn layout_device_and_slot_count_are_the_structural_parts() {
        let a = from_apo("Filter: ON PK Fc 1000 Hz Gain 6 dB Q 1\n", 2);
        let sk = |s: &Setup, o: &Options| skeleton(&generate(s, o).unwrap().0);
        let o = Options::default();
        assert_ne!(sk(&a, &o), sk(&from_apo("", 6), &o), "channel layout");
        let mut dev = a.clone();
        dev.output_target = Some("alsa_output.x".into());
        assert_ne!(sk(&a, &o), sk(&dev, &o), "output device");
        assert_ne!(sk(&a, &o), sk(&a, &Options { slots: Some(24), ..Options::default() }), "slot count");
        // a profile needing 17 filters picks 24 slots automatically: structural vs the 16-slot graph
        let big = {
            let mut s = a.clone();
            for i in 0..17 {
                s.eqs[0].filters.push(pk(100.0 + 30.0 * i as f64, 1.0));
            }
            s
        };
        assert_ne!(sk(&a, &o), sk(&big, &o));
    }
}
