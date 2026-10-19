//! Everything that talks to a running PipeWire, shared by the CLI and the GUI.
//!
//! It shells out to `pw-dump`, `pw-cli` and `systemctl`, so it needs no native
//! PipeWire libraries. Pure logic (config generation, control lists) lives in
//! `linapo-core`.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};
use linapo_core::pipewire_conf::{generate, live_controls, skeleton, ControlKey, Options};
use linapo_core::Setup;

pub const CONF_NAME: &str = "99-linapo.conf";

/// `pw-cli set-param` converts its argument into a small fixed buffer (about 1 KB) and a
/// larger batch is silently dropped, so values are sent in small chunks.
const CHUNK: usize = 12;

/// Where the installed virtual sink stands relative to a setup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// No config file installed.
    NotInstalled,
    /// Installed and identical in structure; values can be pushed live.
    Live,
    /// Installed but the structure differs (channels, filters, device): needs `install`.
    NeedsRestart,
    /// Config matches but the PipeWire node is not running (PipeWire not restarted yet, or down).
    NotRunning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    Live { values: usize },
    NeedsRestart,
    NotInstalled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sink {
    pub node_name: String,
    pub description: String,
}

pub fn conf_dir() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".config"),
    };
    Ok(base.join("pipewire").join("pipewire.conf.d"))
}

pub fn conf_path() -> Result<PathBuf> {
    Ok(conf_dir()?.join(CONF_NAME))
}

fn pw_dump() -> Result<Vec<serde_json::Value>> {
    let out = Command::new("pw-dump").output().context("running pw-dump (is PipeWire installed?)")?;
    if !out.status.success() {
        bail!("pw-dump failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    match serde_json::from_slice(&out.stdout).context("parsing pw-dump output")? {
        serde_json::Value::Array(v) => Ok(v),
        _ => bail!("unexpected pw-dump output"),
    }
}

fn node_id_in(dump: &[serde_json::Value], name: &str) -> Option<u32> {
    dump.iter()
        .find(|o| o["info"]["props"]["node.name"] == name)
        .and_then(|o| o["id"].as_u64())
        .map(|id| id as u32)
}

pub fn find_node_id(name: &str) -> Result<u32> {
    node_id_in(&pw_dump()?, name).with_context(|| format!("PipeWire node '{name}' not found; install the setup first"))
}

/// Real output devices a setup can be sent to (excludes linapo's own sink).
pub fn list_sinks() -> Result<Vec<Sink>> {
    Ok(sinks_in(&pw_dump()?))
}

fn sinks_in(dump: &[serde_json::Value]) -> Vec<Sink> {
    dump.iter()
        .filter(|o| o["info"]["props"]["media.class"] == "Audio/Sink")
        .filter_map(|o| {
            let p = &o["info"]["props"];
            let name = p["node.name"].as_str()?;
            if name.starts_with("effect_input.") {
                return None;
            }
            let desc = p["node.description"].as_str().unwrap_or(name);
            Some(Sink { node_name: name.to_string(), description: desc.to_string() })
        })
        .collect()
}

/// Send `(control, value)` pairs to the running sink in one go.
pub fn send_controls(controls: &[(ControlKey, f64)]) -> Result<usize> {
    let id = find_node_id(&Options::default().sink_node())?;
    for chunk in controls.chunks(CHUNK) {
        let body: Vec<String> = chunk.iter().map(|(k, v)| format!("\"{k}\" {v:?}")).collect();
        let out = Command::new("pw-cli")
            .args(["set-param", &id.to_string(), "Props", &format!("{{ params = [ {} ] }}", body.join(" "))])
            .output()
            .context("running pw-cli (is PipeWire installed?)")?;
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        if !out.status.success() || text.to_ascii_lowercase().contains("error") {
            bail!("pw-cli set-param failed: {}", text.trim());
        }
        // pw-cli echoes the param it parsed; every key must be in there.
        let accepted = text.matches("String \"").count();
        if accepted != chunk.len() {
            bail!("pw-cli accepted only {accepted} of {} values", chunk.len());
        }
    }
    Ok(controls.len())
}

fn installed_text() -> Option<String> {
    std::fs::read_to_string(conf_path().ok()?).ok()
}

/// `None`: nothing installed. `Some(true)`: the installed graph has the same structure as
/// `setup` (only live-editable values may differ). Does not need PipeWire to be running.
pub fn structure_matches(setup: &Setup, opts: &Options) -> Result<Option<bool>> {
    let Some(installed) = installed_text() else { return Ok(None) };
    let (fresh, _) = generate(setup, opts).map_err(anyhow::Error::msg)?;
    Ok(Some(skeleton(&installed) == skeleton(&fresh)))
}

pub fn status(setup: &Setup, opts: &Options) -> Result<Status> {
    Ok(match structure_matches(setup, opts)? {
        None => Status::NotInstalled,
        Some(false) => Status::NeedsRestart,
        Some(true) => {
            let running = node_id_in(&pw_dump()?, &opts.sink_node()).is_some();
            if running {
                Status::Live
            } else {
                Status::NotRunning
            }
        }
    })
}

/// The sample rate the PipeWire graph runs at: `clock.force-rate` if set, else `clock.rate`.
/// If PipeWire is configured with several allowed rates it may switch at runtime; this is the
/// default rate it reports.
pub fn current_rate() -> Option<f64> {
    rate_in(&pw_dump().ok()?)
}

fn json_num(v: &serde_json::Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str()?.trim().parse().ok())
}

fn rate_in(dump: &[serde_json::Value]) -> Option<f64> {
    let settings = dump.iter().find(|o| {
        o["type"].as_str().is_some_and(|t| t.ends_with("Metadata")) && o["props"]["metadata.name"] == "settings"
    })?;
    let get = |key: &str| {
        settings["metadata"].as_array()?.iter().find(|m| m["key"] == key).and_then(|m| json_num(&m["value"]))
    };
    match get("clock.force-rate") {
        Some(f) if f > 0.0 => Some(f),
        _ => get("clock.rate").filter(|r| *r > 0.0),
    }
}

/// Write `text` to `path` (atomically) unless the file already has exactly that content.
pub fn write_if_changed(path: &std::path::Path, text: &str) -> std::io::Result<bool> {
    if std::fs::read_to_string(path).ok().as_deref() == Some(text) {
        return Ok(false);
    }
    let tmp = path.with_extension("conf.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(true)
}

/// Push every adjustable value of `setup` to the running sink, if the installed
/// structure matches. Never restarts anything. Afterwards the installed config file is
/// brought up to date, so a later PipeWire restart starts from the current values.
pub fn apply_live(setup: &Setup, opts: &Options) -> Result<Applied> {
    let mut o = opts.clone();
    if let Some(rate) = current_rate() {
        o.sample_rate = rate; // raw biquad coefficients depend on the running rate
    }
    match structure_matches(setup, &o)? {
        None => Ok(Applied::NotInstalled),
        Some(false) => Ok(Applied::NeedsRestart),
        Some(true) => {
            let controls = live_controls(setup, &o).map_err(anyhow::Error::msg)?;
            let values = send_controls(&controls)?;
            if let Ok((text, _)) = generate(setup, &o) {
                let _ = conf_path().map(|p| write_if_changed(&p, &text));
            }
            Ok(Applied::Live { values })
        }
    }
}

pub fn restart_pipewire() -> Vec<String> {
    let mut warnings = Vec::new();
    for unit in ["pipewire.service", "wireplumber.service", "pipewire-pulse.service"] {
        match Command::new("systemctl").args(["--user", "restart", unit]).status() {
            Ok(s) if s.success() => {}
            Ok(s) => warnings.push(format!("restarting {unit} exited with {s}")),
            Err(e) => warnings.push(format!("could not run systemctl for {unit}: {e}")),
        }
    }
    warnings
}

/// Write the config; optionally restart PipeWire. Returns warnings (skipped filters, restart problems).
pub fn install(setup: &Setup, opts: &Options, restart: bool) -> Result<Vec<String>> {
    let (text, mut warnings) = generate(setup, opts).map_err(anyhow::Error::msg)?;
    let dir = conf_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(CONF_NAME);
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    if restart {
        warnings.extend(restart_pipewire());
    }
    Ok(warnings)
}

/// Remove the installed config. Returns whether there was one.
pub fn uninstall(restart: bool) -> Result<bool> {
    let path = conf_path()?;
    if !path.exists() {
        return Ok(false);
    }
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    if restart {
        restart_pipewire();
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dump() -> Vec<serde_json::Value> {
        vec![
            json!({"id": 1, "type": "PipeWire:Interface:Core"}),
            json!({"id": 35, "info": {"props": {"node.name": "effect_input.linapo", "media.class": "Audio/Sink", "node.description": "LinAPO EQ"}}}),
            json!({"id": 40, "info": {"props": {"node.name": "alsa_output.pci", "media.class": "Audio/Sink", "node.description": "Built-in Audio"}}}),
            json!({"id": 41, "info": {"props": {"node.name": "alsa_input.pci", "media.class": "Audio/Source"}}}),
            json!({"id": 42, "info": {"props": {"node.name": "bluez_output.x", "media.class": "Audio/Sink"}}}),
        ]
    }

    #[test]
    fn finds_nodes_by_name() {
        assert_eq!(node_id_in(&dump(), "effect_input.linapo"), Some(35));
        assert_eq!(node_id_in(&dump(), "nope"), None);
    }

    fn settings(entries: &[(&str, serde_json::Value)]) -> Vec<serde_json::Value> {
        let md: Vec<_> = entries.iter().map(|(k, v)| json!({"subject": 0, "key": k, "value": v})).collect();
        vec![json!({"id": 33, "type": "PipeWire:Interface:Metadata", "props": {"metadata.name": "settings"}, "metadata": md})]
    }

    #[test]
    fn reads_the_graph_rate_from_settings_metadata() {
        assert_eq!(rate_in(&settings(&[("clock.rate", json!(48000))])), Some(48_000.0));
        assert_eq!(rate_in(&settings(&[("clock.rate", json!("44100"))])), Some(44_100.0), "string values too");
        assert_eq!(
            rate_in(&settings(&[("clock.rate", json!(48000)), ("clock.force-rate", json!(96000))])),
            Some(96_000.0),
            "a forced rate wins"
        );
        assert_eq!(
            rate_in(&settings(&[("clock.rate", json!(48000)), ("clock.force-rate", json!(0))])),
            Some(48_000.0),
            "force-rate 0 means not forced"
        );
        assert_eq!(rate_in(&dump()), None, "no settings metadata");
        assert_eq!(rate_in(&[]), None);
    }

    #[test]
    fn write_if_changed_only_touches_the_file_when_needed() {
        let dir = std::env::temp_dir().join(format!("linapo-pw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("99-linapo.conf");
        assert!(write_if_changed(&path, "a").unwrap(), "created");
        assert!(!write_if_changed(&path, "a").unwrap(), "unchanged");
        assert!(write_if_changed(&path, "b").unwrap(), "updated");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "b");
        assert!(!dir.join("99-linapo.conf.tmp").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lists_real_sinks_only() {
        let s = sinks_in(&dump());
        assert_eq!(
            s,
            vec![
                Sink { node_name: "alsa_output.pci".into(), description: "Built-in Audio".into() },
                Sink { node_name: "bluez_output.x".into(), description: "bluez_output.x".into() },
            ]
        );
    }
}
