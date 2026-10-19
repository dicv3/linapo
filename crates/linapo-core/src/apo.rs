//! Parser for Equalizer APO configuration files (also what AutoEQ and REW export).
//!
//! Supported: `Preamp`, `Filter` (PK, LP/LPQ, HP/HPQ, BP, NO, AP, LS/LSC/LSQ,
//! HS/HSC/HSQ, LS 6dB/12dB, HS 6dB/12dB), `Channel`, `Copy`, `GraphicEQ`
//! (fitted to a biquad bank, see [`crate::graphic_eq`]) and `Include`.
//!
//! Anything else produces a [`Diagnostic`] and is skipped, like APO ignoring
//! lines it does not understand. Parsing never fails as a whole.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::chain::ChannelChain;
use crate::filter::{shelf_q_from_slope, Filter, FilterKind, DEFAULT_Q};

const MAX_INCLUDE_DEPTH: usize = 8;

// ---------------------------------------------------------------- channels

/// A channel name as used in APO (`L`, `R`, `C`, `SUB`, `RL`, `RR`, `SL`, `SR`, `CH9`...).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Channel(String);

impl TryFrom<String> for Channel {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        Channel::parse(&s).ok_or_else(|| format!("unknown channel '{s}'"))
    }
}

impl From<Channel> for String {
    fn from(c: Channel) -> String {
        c.0
    }
}

impl Channel {
    pub fn parse(s: &str) -> Option<Channel> {
        let up = s.trim().to_ascii_uppercase();
        let canon = match up.as_str() {
            "FL" => "L",
            "FR" => "R",
            "FC" => "C",
            "LFE" => "SUB",
            "BL" => "RL",
            "BR" => "RR",
            other => other,
        };
        let known = ["L", "R", "C", "SUB", "RL", "RR", "SL", "SR"];
        let is_numbered = canon.len() > 2
            && canon.starts_with("CH")
            && canon[2..].chars().all(|c| c.is_ascii_digit());
        if known.contains(&canon) || is_numbered {
            Some(Channel(canon.to_string()))
        } else {
            None
        }
    }

    pub fn name(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Channel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Channel order used by APO for a given channel count.
pub fn layout_for(n: usize) -> Vec<Channel> {
    let names: Vec<String> = match n {
        1 => vec!["C".into()],
        2 => vec!["L".into(), "R".into()],
        4 => ["L", "R", "RL", "RR"].iter().map(|s| s.to_string()).collect(),
        6 => ["L", "R", "C", "SUB", "RL", "RR"].iter().map(|s| s.to_string()).collect(),
        8 => ["L", "R", "C", "SUB", "RL", "RR", "SL", "SR"].iter().map(|s| s.to_string()).collect(),
        _ => (1..=n).map(|i| format!("CH{i}")).collect(),
    };
    names.into_iter().map(Channel).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    All,
    Only(Vec<Channel>),
}

impl Selection {
    pub fn includes(&self, ch: &Channel) -> bool {
        match self {
            Selection::All => true,
            Selection::Only(list) => list.contains(ch),
        }
    }
}

// ------------------------------------------------------------------ model

#[derive(Debug, Clone, PartialEq)]
pub struct CopySource {
    pub channel: Channel,
    /// Linear gain (dB values are converted on parse).
    pub gain: f64,
}

/// `Copy: dest = gain*src + gain*src ...`
#[derive(Debug, Clone, PartialEq)]
pub struct CopyAssignment {
    pub dest: Channel,
    pub sources: Vec<CopySource>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Preamp { gain_db: f64 },
    Filter(Filter),
    Copy(Vec<CopyAssignment>),
    /// `(frequency Hz, gain dB)` points. Parsed but not applied yet.
    GraphicEq(Vec<(f64, f64)>),
}

/// A command together with the channels that were selected when it appeared.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub channels: Selection,
    pub command: Command,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Diagnostic {
    pub file: Option<PathBuf>,
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.file {
            Some(p) => write!(f, "{}:{}: {}", p.display(), self.line, self.message),
            None => write!(f, "line {}: {}", self.line, self.message),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Profile {
    pub items: Vec<Item>,
    pub diagnostics: Vec<Diagnostic>,
}

impl Profile {
    /// Resolve the preamp + filters that apply to one channel.
    pub fn chain_for(&self, ch: &Channel) -> ChannelChain {
        let mut chain = ChannelChain::default();
        for item in &self.items {
            if !item.channels.includes(ch) {
                continue;
            }
            match &item.command {
                Command::Preamp { gain_db } => chain.gain_db += gain_db,
                Command::Filter(f) => chain.filters.push(*f),
                Command::GraphicEq(points) => {
                    if let Ok(fit) = crate::graphic_eq::fit_default(points) {
                        chain.gain_db += fit.preamp_db;
                        chain.filters.extend(fit.filters);
                    }
                }
                _ => {}
            }
        }
        chain
    }

    /// Channels that the profile names explicitly via `Channel:` lines.
    pub fn referenced_channels(&self) -> Vec<Channel> {
        let mut out: Vec<Channel> = Vec::new();
        for item in &self.items {
            if let Selection::Only(list) = &item.channels {
                for c in list {
                    if !out.contains(c) {
                        out.push(c.clone());
                    }
                }
            }
        }
        out.sort();
        out
    }

    pub fn copy_count(&self) -> usize {
        self.items.iter().filter(|i| matches!(i.command, Command::Copy(_))).count()
    }

    pub fn graphic_eq_count(&self) -> usize {
        self.items.iter().filter(|i| matches!(i.command, Command::GraphicEq(_))).count()
    }
}

// ---------------------------------------------------------------- parsing

/// Parse profile text. `Include:` lines are reported as unsupported here
/// because there is no directory to resolve them against; use [`parse_file`].
pub fn parse_str(text: &str) -> Profile {
    let mut p = Parser::new();
    p.feed(text, None, 0);
    p.profile
}

/// Parse a profile from disk, following `Include:` lines relative to each file.
pub fn parse_file(path: &Path) -> io::Result<Profile> {
    let text = read_text(path)?;
    let mut p = Parser::new();
    p.feed(&text, Some(path), 0);
    Ok(p.profile)
}

/// Read a text file that may be UTF-8 (with or without BOM) or UTF-16, as
/// Windows tools sometimes produce.
fn read_text(path: &Path) -> io::Result<String> {
    let bytes = fs::read(path)?;
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        let le = bytes[0] == 0xFF;
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|b| if le { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) })
            .collect();
        return Ok(String::from_utf16_lossy(&units));
    }
    let s = String::from_utf8_lossy(&bytes).into_owned();
    Ok(s.strip_prefix('\u{feff}').map(str::to_owned).unwrap_or(s))
}

struct Parser {
    profile: Profile,
    selection: Selection,
}

impl Parser {
    fn new() -> Self {
        Parser { profile: Profile::default(), selection: Selection::All }
    }

    fn warn(&mut self, file: Option<&Path>, line: usize, message: impl Into<String>) {
        self.profile.diagnostics.push(Diagnostic {
            file: file.map(Path::to_path_buf),
            line,
            message: message.into(),
        });
    }

    fn push(&mut self, command: Command) {
        self.profile.items.push(Item { channels: self.selection.clone(), command });
    }

    fn feed(&mut self, text: &str, file: Option<&Path>, depth: usize) {
        for (idx, raw) in text.lines().enumerate() {
            let lineno = idx + 1;
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let lower = line.to_ascii_lowercase();
            // REW export headers carry no information for us.
            if lower.starts_with("filter settings file") || lower.starts_with("room eq") {
                continue;
            }
            let Some((key_raw, rest)) = line.split_once(':') else {
                self.warn(file, lineno, format!("ignored line without ':' ({line})"));
                continue;
            };
            let key = key_raw.trim().to_ascii_lowercase();
            let rest = rest.trim();

            if let Some(num) = key.strip_prefix("filter") {
                if num.trim().chars().all(|c| c.is_ascii_digit()) {
                    match parse_filter(rest) {
                        Ok(f) => self.push(Command::Filter(f)),
                        Err(e) => self.warn(file, lineno, format!("filter skipped: {e}")),
                    }
                    continue;
                }
            }

            match key.as_str() {
                "preamp" => match parse_num(rest) {
                    Some(g) => self.push(Command::Preamp { gain_db: g }),
                    None => self.warn(file, lineno, format!("invalid preamp value '{rest}'")),
                },
                "channel" => self.set_channels(rest, file, lineno),
                "copy" => match parse_copy(rest) {
                    Ok(c) => self.push(Command::Copy(c)),
                    Err(e) => self.warn(file, lineno, format!("copy skipped: {e}")),
                },
                "graphiceq" => match parse_graphic_eq(rest) {
                    Ok(pts) => self.push(Command::GraphicEq(pts)),
                    Err(e) => self.warn(file, lineno, format!("GraphicEQ skipped: {e}")),
                },
                "include" => self.include(rest, file, lineno, depth),
                // Header fields in REW exports.
                "equaliser" | "equalizer" | "dated" | "notes" => {}
                "device" | "delay" | "convolution" | "vstplugin" | "loadlibrary" | "stage" | "if"
                | "elseif" | "else" | "endif" | "eq" | "ladspaplugin" | "expand" => {
                    self.warn(file, lineno, format!("'{}' is not supported yet and was ignored", key_raw.trim()));
                }
                _ => self.warn(file, lineno, format!("unknown command '{}'", key_raw.trim())),
            }
        }
    }

    fn set_channels(&mut self, rest: &str, file: Option<&Path>, line: usize) {
        let mut list = Vec::new();
        for tok in rest.split_whitespace() {
            if tok.eq_ignore_ascii_case("all") {
                self.selection = Selection::All;
                return;
            }
            match Channel::parse(tok) {
                Some(c) => list.push(c),
                None => self.warn(file, line, format!("unknown channel '{tok}'")),
            }
        }
        self.selection = Selection::Only(list);
    }

    fn include(&mut self, rest: &str, file: Option<&Path>, line: usize, depth: usize) {
        let name = rest.trim_matches('"').replace('\\', "/");
        let Some(base) = file else {
            self.warn(file, line, "Include ignored: parsing from a string, no base directory");
            return;
        };
        if depth >= MAX_INCLUDE_DEPTH {
            self.warn(file, line, "Include nesting too deep, ignored");
            return;
        }
        let path = base.parent().unwrap_or_else(|| Path::new(".")).join(&name);
        match read_text(&path) {
            Ok(text) => self.feed(&text, Some(&path), depth + 1),
            Err(e) => self.warn(file, line, format!("cannot include '{}': {e}", path.display())),
        }
    }
}

/// Parse a number, tolerating a decimal comma and a trailing unit (`1000Hz`, `3dB`).
fn parse_num(tok: &str) -> Option<f64> {
    let t = tok.trim().replace(',', ".");
    t.trim_end_matches(|c: char| c.is_ascii_alphabetic()).trim().parse().ok()
}

fn looks_numeric(tok: &str) -> bool {
    tok.chars().next().map_or(false, |c| c.is_ascii_digit() || c == '.' || c == '-')
}

fn next_num(toks: &[&str], i: &mut usize, what: &str) -> Result<f64, String> {
    let tok = toks.get(*i).ok_or_else(|| format!("missing value after {what}"))?;
    *i += 1;
    parse_num(tok).ok_or_else(|| format!("invalid number '{tok}' after {what}"))
}

fn skip_unit(toks: &[&str], i: &mut usize, unit: &str) {
    if toks.get(*i).map_or(false, |t| t.eq_ignore_ascii_case(unit)) {
        *i += 1;
    }
}

/// Parse everything after `Filter N:`. `OFF` filters are kept, with `enabled: false`.
fn parse_filter(rest: &str) -> Result<Filter, String> {
    let toks: Vec<&str> = rest.split_whitespace().collect();
    let state = toks.first().ok_or("empty filter definition")?;
    let enabled = match state.to_ascii_uppercase().as_str() {
        "ON" => true,
        "OFF" => false,
        _ => return Err(format!("expected ON or OFF, found '{state}'")),
    };
    let kind_tok = toks.get(1).ok_or("missing filter type")?.to_ascii_uppercase();
    let mut i = 2;

    // Optional slope right after the type: `LS 6dB`, `LSC 10.8 dB`.
    let mut slope = None;
    if matches!(kind_tok.as_str(), "LS" | "HS" | "LSC" | "HSC") {
        if let Some(t) = toks.get(i) {
            if looks_numeric(t) {
                slope = parse_num(t);
                i += 1;
                skip_unit(&toks, &mut i, "dB");
            }
        }
    }

    let (mut fc, mut gain, mut q, mut bw) = (None, None, None, None);
    while i < toks.len() {
        let tok = toks[i];
        i += 1;
        match tok.to_ascii_lowercase().as_str() {
            "fc" => {
                fc = Some(next_num(&toks, &mut i, "Fc")?);
                skip_unit(&toks, &mut i, "Hz");
            }
            "gain" => {
                gain = Some(next_num(&toks, &mut i, "Gain")?);
                skip_unit(&toks, &mut i, "dB");
            }
            "q" => q = Some(next_num(&toks, &mut i, "Q")?),
            "bw" => {
                if toks.get(i).map_or(false, |t| t.eq_ignore_ascii_case("oct")) {
                    i += 1;
                    bw = Some(next_num(&toks, &mut i, "BW Oct")?);
                } else {
                    return Err("only 'BW Oct' bandwidth is supported".into());
                }
            }
            "hz" | "db" => {}
            _ => return Err(format!("unexpected token '{tok}'")),
        }
    }

    let fc_hz = fc.ok_or("missing Fc")?;
    let gain_db = gain.unwrap_or(0.0);
    let q_from_bw = bw.map(|n| {
        let p = 2f64.powf(n);
        p.sqrt() / (p - 1.0)
    });
    let explicit_q = q.or(q_from_bw);

    let (kind, q) = match kind_tok.as_str() {
        "PK" => (FilterKind::Peaking, explicit_q.ok_or("PK needs Q or BW Oct")?),
        "LP" | "LPQ" => (FilterKind::LowPass, explicit_q.unwrap_or(DEFAULT_Q)),
        "HP" | "HPQ" => (FilterKind::HighPass, explicit_q.unwrap_or(DEFAULT_Q)),
        "BP" => (FilterKind::BandPass, explicit_q.unwrap_or(DEFAULT_Q)),
        "NO" => (FilterKind::Notch, explicit_q.unwrap_or(DEFAULT_Q)),
        "AP" => (FilterKind::AllPass, explicit_q.unwrap_or(DEFAULT_Q)),
        "LS" | "LSC" | "LSQ" | "HS" | "HSC" | "HSQ" => {
            let low = kind_tok.starts_with('L');
            let corner_form = matches!(kind_tok.as_str(), "LS" | "HS");
            let first_order = corner_form && slope.map_or(false, |s| (s - 6.0).abs() < 1e-9);
            if first_order {
                let k = if low { FilterKind::LowShelf1 } else { FilterKind::HighShelf1 };
                (k, DEFAULT_Q)
            } else {
                let k = if low { FilterKind::LowShelf } else { FilterKind::HighShelf };
                let q = match (explicit_q, slope) {
                    (Some(q), _) => q,
                    (None, Some(s)) => shelf_q_from_slope(gain_db, s),
                    (None, None) => DEFAULT_Q,
                };
                (k, q)
            }
        }
        other => return Err(format!("unsupported filter type '{other}'")),
    };

    Ok(Filter { kind, fc_hz, gain_db, q, enabled })
}

/// `L=0.5*L+0.5*R R=R`, `L=R+-6dB*C`, spaces around operators tolerated.
fn parse_copy(rest: &str) -> Result<Vec<CopyAssignment>, String> {
    let mut s = rest.to_string();
    for pat in [" =", "= ", " +", "+ ", " *", "* "] {
        let tight = pat.trim();
        while s.contains(pat) {
            s = s.replace(pat, tight);
        }
    }
    let mut out = Vec::new();
    for tok in s.split_whitespace() {
        let (dest, expr) = tok.split_once('=').ok_or_else(|| format!("expected dest=expression, found '{tok}'"))?;
        let dest = Channel::parse(dest).ok_or_else(|| format!("unknown channel '{dest}'"))?;
        let mut sources = Vec::new();
        for term in expr.split('+') {
            if term.is_empty() {
                return Err(format!("empty term in '{tok}'"));
            }
            let (gain, ch) = match term.split_once('*') {
                Some((g, c)) => (parse_gain(g)?, c),
                None => (1.0, term),
            };
            let channel = Channel::parse(ch).ok_or_else(|| format!("unknown channel '{ch}'"))?;
            sources.push(CopySource { channel, gain });
        }
        out.push(CopyAssignment { dest, sources });
    }
    if out.is_empty() {
        return Err("empty Copy".into());
    }
    Ok(out)
}

/// A gain factor: plain linear (`0.5`) or in dB (`-6dB`). Returns linear.
fn parse_gain(s: &str) -> Result<f64, String> {
    let v = parse_num(s).ok_or_else(|| format!("invalid gain '{s}'"))?;
    if s.trim().to_ascii_lowercase().ends_with("db") {
        Ok(10f64.powf(v / 20.0))
    } else {
        Ok(v)
    }
}

fn parse_graphic_eq(rest: &str) -> Result<Vec<(f64, f64)>, String> {
    let mut pts = Vec::new();
    for pair in rest.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let mut it = pair.split_whitespace();
        let (f, g) = (it.next(), it.next());
        match (f.and_then(parse_num), g.and_then(parse_num), it.next()) {
            (Some(f), Some(g), None) => pts.push((f, g)),
            _ => return Err(format!("invalid point '{pair}'")),
        }
    }
    if pts.is_empty() {
        return Err("no points".into());
    }
    Ok(pts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(s: &str) -> Channel {
        Channel::parse(s).unwrap()
    }

    fn filters(p: &Profile) -> Vec<Filter> {
        p.chain_for(&ch("L")).filters
    }

    #[test]
    fn parses_autoeq_style_profile() {
        let p = parse_str(
            "Preamp: -6.2 dB\n\
             Filter 1: ON LSC Fc 105 Hz Gain 5.5 dB Q 0.70\n\
             Filter 2: ON PK Fc 1000 Hz Gain -3.0 dB Q 1.41\n\
             Filter 3: ON HSC Fc 10000 Hz Gain 2 dB Q 0.7\n",
        );
        assert!(p.diagnostics.is_empty(), "{:?}", p.diagnostics);
        let c = p.chain_for(&ch("R"));
        assert_eq!(c.gain_db, -6.2);
        assert_eq!(c.filters.len(), 3);
        assert_eq!(c.filters[0].kind, FilterKind::LowShelf);
        assert_eq!(c.filters[1], Filter { kind: FilterKind::Peaking, fc_hz: 1000.0, gain_db: -3.0, q: 1.41, enabled: true });
        assert_eq!(c.filters[2].kind, FilterKind::HighShelf);
    }

    #[test]
    fn parses_rew_export_with_tabs_and_header() {
        let p = parse_str(
            "Filter Settings file\n\nRoom EQ V5.20\nDated: 1 Jan 2024\nNotes:\nEqualiser: Generic\n\
             Filter  1: ON  PK\tFc   13058 Hz  Gain  -3.7 dB  Q  1.0\n\
             Filter  2: ON  PK\tFc   12260 Hz  Gain  2.3 dB  Q  1.0\n",
        );
        assert!(p.diagnostics.is_empty(), "{:?}", p.diagnostics);
        assert_eq!(filters(&p).len(), 2);
    }

    #[test]
    fn off_filters_are_kept_but_disabled() {
        let p = parse_str("Filter 1: OFF PK Fc 1000 Hz Gain 3 dB Q 1\nFilter 2: ON PK Fc 2000 Hz Gain 3 dB Q 1\n");
        let f = filters(&p);
        assert_eq!(f.len(), 2);
        assert!(!f[0].enabled && f[1].enabled);
        assert_eq!(f[0].fc_hz, 1000.0, "settings survive so the filter can be switched back on");
        // ...and they do nothing:
        let r = crate::chain::response_db(&p.chain_for(&ch("L")), 48_000.0, 1000.0);
        assert!(r.abs() < 1.0, "only the 2 kHz filter acts at 1 kHz, got {r}");
    }

    #[test]
    fn filter_without_number_and_case_insensitive() {
        let p = parse_str("filter: on pk fc 1000 hz gain 3 db q 1.41\n");
        assert_eq!(filters(&p).len(), 1);
    }

    #[test]
    fn comments_commas_and_glued_units() {
        let p = parse_str(
            "# whole line comment\n\
             Filter 1: ON PK Fc 1000Hz Gain 3,5dB Q 1,41  # trailing comment\n",
        );
        assert!(p.diagnostics.is_empty(), "{:?}", p.diagnostics);
        let f = filters(&p)[0];
        assert_eq!((f.fc_hz, f.gain_db, f.q), (1000.0, 3.5, 1.41));
    }

    #[test]
    fn lp_hp_default_q_and_lpq() {
        let p = parse_str(
            "Filter: ON LP Fc 8000 Hz\nFilter: ON HPQ Fc 100 Hz Q 0.4\nFilter: ON NO Fc 50 Hz Q 10\n",
        );
        let f = filters(&p);
        assert_eq!(f[0].kind, FilterKind::LowPass);
        assert_eq!(f[0].q, DEFAULT_Q);
        assert_eq!(f[1].kind, FilterKind::HighPass);
        assert_eq!(f[1].q, 0.4);
        assert_eq!(f[2].kind, FilterKind::Notch);
    }

    #[test]
    fn shelf_variants() {
        let p = parse_str(
            "Filter: ON LS 6dB Fc 50 Hz Gain 7.2 dB\n\
             Filter: ON HS 6dB Fc 12000 Hz Gain 10 dB\n\
             Filter: ON LS 12dB Fc 2000 Hz Gain -5 dB\n\
             Filter: ON LSC 12 dB Fc 300 Hz Gain 5 dB\n\
             Filter: ON HSC Fc 100 Hz Gain -6 dB Q 0.4272\n\
             Filter: ON LSQ Fc 100 Hz Gain 3 dB Q 0.9\n\
             Filter: ON LS Fc 300 Hz Gain 5 dB\n",
        );
        assert!(p.diagnostics.is_empty(), "{:?}", p.diagnostics);
        let f = filters(&p);
        assert_eq!(f[0].kind, FilterKind::LowShelf1);
        assert_eq!(f[1].kind, FilterKind::HighShelf1);
        assert_eq!((f[2].kind, f[2].q), (FilterKind::LowShelf, DEFAULT_Q));
        assert!((f[3].q - DEFAULT_Q).abs() < 1e-12);
        assert_eq!((f[4].kind, f[4].q), (FilterKind::HighShelf, 0.4272));
        assert_eq!((f[5].kind, f[5].q), (FilterKind::LowShelf, 0.9));
        assert_eq!(f[6].q, DEFAULT_Q);
    }

    #[test]
    fn bandwidth_in_octaves_converts_to_q() {
        let p = parse_str("Filter: ON PK Fc 1000 Hz Gain 3 dB BW Oct 1\n");
        // Q for one octave is sqrt(2)/(2-1) = 1.4142...
        assert!((filters(&p)[0].q - 2f64.sqrt()).abs() < 1e-12);
    }

    #[test]
    fn bad_lines_produce_diagnostics_but_keep_going() {
        let p = parse_str(
            "Filter 1: ON PK Fc abc Hz Gain 3 dB Q 1\n\
             Filter 2: ON XX Fc 1000 Hz Gain 3 dB Q 1\n\
             Filter 3: ON PK Gain 3 dB Q 1\n\
             Filter 4: ON PK Fc 1000 Hz Gain 3 dB\n\
             Filter 5: MAYBE PK Fc 1000 Hz Gain 3 dB Q 1\n\
             Bogus: 1\n\
             Device: Speakers\n\
             no colon here\n\
             Filter 6: ON PK Fc 1000 Hz Gain 3 dB Q 1\n",
        );
        assert_eq!(filters(&p).len(), 1);
        assert_eq!(p.diagnostics.len(), 8, "{:#?}", p.diagnostics);
        assert_eq!(p.diagnostics[0].line, 1);
    }

    #[test]
    fn channel_sections_scope_commands() {
        let p = parse_str(
            "Preamp: -3 dB\n\
             Channel: L\n\
             Filter: ON PK Fc 100 Hz Gain 4 dB Q 1\n\
             Channel: R SUB\n\
             Preamp: -1 dB\n\
             Filter: ON PK Fc 200 Hz Gain -4 dB Q 1\n\
             Channel: all\n\
             Filter: ON PK Fc 5000 Hz Gain 1 dB Q 1\n",
        );
        let l = p.chain_for(&ch("L"));
        let r = p.chain_for(&ch("R"));
        let sub = p.chain_for(&ch("SUB"));
        let c = p.chain_for(&ch("C"));
        assert_eq!((l.gain_db, l.filters.len()), (-3.0, 2));
        assert_eq!(l.filters[0].fc_hz, 100.0);
        assert_eq!((r.gain_db, r.filters.len()), (-4.0, 2));
        assert_eq!(r.filters[0].fc_hz, 200.0);
        assert_eq!(sub, r);
        assert_eq!((c.gain_db, c.filters.len()), (-3.0, 1));
        assert_eq!(p.referenced_channels(), vec![ch("L"), ch("R"), ch("SUB")]);
    }

    #[test]
    fn channel_aliases_and_unknown_names() {
        assert_eq!(Channel::parse("fl"), Some(ch("L")));
        assert_eq!(Channel::parse("LFE"), Some(ch("SUB")));
        assert_eq!(Channel::parse("ch12").unwrap().name(), "CH12");
        assert_eq!(Channel::parse("banana"), None);
        let p = parse_str("Channel: L banana\nFilter: ON PK Fc 100 Hz Gain 1 dB Q 1\n");
        assert_eq!(p.diagnostics.len(), 1);
        assert_eq!(p.chain_for(&ch("L")).filters.len(), 1);
        assert_eq!(p.chain_for(&ch("R")).filters.len(), 0);
    }

    #[test]
    fn layouts() {
        let names = |n| layout_for(n).iter().map(|c| c.to_string()).collect::<Vec<_>>().join(" ");
        assert_eq!(names(1), "C");
        assert_eq!(names(2), "L R");
        assert_eq!(names(6), "L R C SUB RL RR");
        assert_eq!(names(8), "L R C SUB RL RR SL SR");
        assert_eq!(names(3), "CH1 CH2 CH3");
    }

    #[test]
    fn copy_command() {
        let p = parse_str("Copy: L=0.5*L+0.5*R R=R\nCopy: C = -6dB*C + R\n");
        assert!(p.diagnostics.is_empty(), "{:?}", p.diagnostics);
        assert_eq!(p.copy_count(), 2);
        let Command::Copy(a) = &p.items[0].command else { panic!() };
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].dest, ch("L"));
        assert_eq!(a[0].sources, vec![
            CopySource { channel: ch("L"), gain: 0.5 },
            CopySource { channel: ch("R"), gain: 0.5 },
        ]);
        assert_eq!(a[1].sources, vec![CopySource { channel: ch("R"), gain: 1.0 }]);
        let Command::Copy(b) = &p.items[1].command else { panic!() };
        assert!((b[0].sources[0].gain - 0.501187).abs() < 1e-5);
        assert_eq!(b[0].sources[1].gain, 1.0);
    }

    #[test]
    fn copy_with_negative_db_after_plus() {
        let p = parse_str("Copy: L=R+-6dB*C\n");
        let Command::Copy(a) = &p.items[0].command else { panic!() };
        assert_eq!(a[0].sources.len(), 2);
        assert!((a[0].sources[1].gain - 0.501187).abs() < 1e-5);
    }

    #[test]
    fn invalid_copy_is_reported() {
        let p = parse_str("Copy: L=banana\nCopy: nonsense\n");
        assert_eq!(p.copy_count(), 0);
        assert_eq!(p.diagnostics.len(), 2);
    }

    #[test]
    fn graphic_eq_points() {
        let p = parse_str("GraphicEQ: 25 -2; 40 0; 63 1.5\n");
        assert_eq!(p.graphic_eq_count(), 1);
        let Command::GraphicEq(pts) = &p.items[0].command else { panic!() };
        assert_eq!(pts, &vec![(25.0, -2.0), (40.0, 0.0), (63.0, 1.5)]);
    }

    #[test]
    fn unsupported_commands_warn() {
        let p = parse_str("Delay: 10 ms\nConvolution: x.wav\nIf: foo\nEndIf:\n");
        assert_eq!(p.diagnostics.len(), 4);
        assert!(p.items.is_empty());
    }

    #[test]
    fn include_follows_relative_paths_and_shares_channel_state() {
        let dir = std::env::temp_dir().join(format!("linapo-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("main.txt"), "Preamp: -2 dB\nChannel: L\nInclude: sub/inc.txt\n").unwrap();
        std::fs::write(dir.join("sub/inc.txt"), "Filter: ON PK Fc 500 Hz Gain 2 dB Q 1\n").unwrap();
        let p = parse_file(&dir.join("main.txt")).unwrap();
        assert!(p.diagnostics.is_empty(), "{:?}", p.diagnostics);
        assert_eq!(p.chain_for(&ch("L")).filters.len(), 1);
        assert_eq!(p.chain_for(&ch("R")).filters.len(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_loops_and_missing_files_do_not_hang() {
        let dir = std::env::temp_dir().join(format!("linapo-loop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "Include: a.txt\nInclude: missing.txt\n").unwrap();
        let p = parse_file(&dir.join("a.txt")).unwrap();
        assert!(p.diagnostics.iter().any(|d| d.message.contains("too deep")));
        assert!(p.diagnostics.iter().any(|d| d.message.contains("cannot include")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_from_string_warns() {
        let p = parse_str("Include: x.txt\n");
        assert_eq!(p.diagnostics.len(), 1);
    }

    #[test]
    fn reads_utf8_bom_and_utf16() {
        let dir = std::env::temp_dir().join(format!("linapo-enc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let line = "Preamp: -4 dB\r\nFilter: ON PK Fc 1000 Hz Gain 3 dB Q 1\r\n";
        let mut bom = vec![0xEF, 0xBB, 0xBF];
        bom.extend_from_slice(line.as_bytes());
        std::fs::write(dir.join("bom.txt"), bom).unwrap();
        let mut u16le = vec![0xFF, 0xFE];
        for u in line.encode_utf16() {
            u16le.extend_from_slice(&u.to_le_bytes());
        }
        std::fs::write(dir.join("u16.txt"), u16le).unwrap();
        for name in ["bom.txt", "u16.txt"] {
            let p = parse_file(&dir.join(name)).unwrap();
            assert!(p.diagnostics.is_empty(), "{name}: {:?}", p.diagnostics);
            let c = p.chain_for(&ch("L"));
            assert_eq!((c.gain_db, c.filters.len()), (-4.0, 1), "{name}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
