//! Everything that talks to a running PipeWire, shared by the CLI and the GUI.
//!
//! It shells out to `pw-dump`, `pw-cli` and `systemctl`, so it needs no native
//! PipeWire libraries. Pure logic (config generation, control lists) lives in
//! `linapo-core`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use linapo_core::pipewire_conf::{generate, installed_slots, live_controls, required_slots, skeleton, slots_for, ControlKey, Options};
use linapo_core::profiles::{decide_switch, ProfileStore, Workspace};
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

/// Send `(control, value)` pairs to the running sink.
pub fn send_controls(controls: &[(ControlKey, f64)]) -> Result<usize> {
    let id = find_node_id(&Options::default().sink_node())?;
    send_to_node(id, controls)?;
    Ok(controls.len())
}

/// Split controls into batches of at most `max` values without ever separating the controls of
/// one node (a biquad's six coefficients must arrive together, or it briefly runs with half-old,
/// half-new coefficients and can ring). A node with more than `max` controls gets its own batch.
fn batches(controls: &[(ControlKey, f64)], max: usize) -> Vec<&[(ControlKey, f64)]> {
    let node = |k: &ControlKey| k.0.split(':').next().unwrap_or("").to_string();
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < controls.len() {
        // the run of controls belonging to the same node
        let n = node(&controls[i].0);
        let mut j = i;
        while j < controls.len() && node(&controls[j].0) == n {
            j += 1;
        }
        if j - start > max && i > start {
            out.push(&controls[start..i]);
            start = i;
        }
        i = j;
    }
    if start < controls.len() {
        out.push(&controls[start..]);
    }
    out
}

fn send_to_node(id: u32, controls: &[(ControlKey, f64)]) -> Result<()> {
    for chunk in batches(controls, CHUNK) {
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
    Ok(())
}

/// What was last sent to the running daemon, so a live update only sends what changed.
/// Keyed by the daemon instance (its cookie) and the node id; anything else invalidates it.
static LAST_SENT: Mutex<Option<(String, HashMap<String, f64>)>> = Mutex::new(None);

fn core_cookie(dump: &[serde_json::Value]) -> String {
    dump.iter()
        .find(|o| o["type"].as_str().is_some_and(|t| t.ends_with("Interface:Core")))
        .map(|o| o["info"]["cookie"].to_string())
        .unwrap_or_default()
}

/// Forget what was sent (the next live update sends everything).
pub fn forget_sent() {
    *LAST_SENT.lock().unwrap() = None;
}

/// Like [`send_controls`] but sends only values that differ from the last call against the same
/// running daemon. Returns how many values were sent.
pub fn send_controls_diff(controls: &[(ControlKey, f64)]) -> Result<usize> {
    let dump = pw_dump()?;
    let name = Options::default().sink_node();
    let id = node_id_in(&dump, &name).with_context(|| format!("PipeWire node '{name}' not found; install the setup first"))?;
    let tag = format!("{}:{id}", core_cookie(&dump));
    let mut guard = LAST_SENT.lock().unwrap();
    let previous = match guard.as_ref() {
        Some((t, map)) if *t == tag => map.clone(),
        _ => HashMap::new(),
    };
    let changed: Vec<(ControlKey, f64)> =
        controls.iter().filter(|(k, v)| previous.get(&k.0) != Some(v)).cloned().collect();
    if !changed.is_empty() {
        send_to_node(id, &changed)?;
    }
    let mut now = previous;
    for (k, v) in controls {
        now.insert(k.0.clone(), *v);
    }
    *guard = Some((tag, now));
    Ok(changed.len())
}

/// Slot count of the installed config, if any.
pub fn installed_slot_count() -> Option<usize> {
    installed_text().map(|t| installed_slots(&t)).filter(|n| *n > 0)
}

fn installed_text() -> Option<String> {
    std::fs::read_to_string(conf_path().ok()?).ok()
}

/// `None`: nothing installed. `Some(true)`: the installed graph has the same structure as
/// `setup` (only live-editable values differ, including the whole EQ). `Some(false)`: it needs
/// a restart (different channels or device, or more filters than the installed slots hold).
/// Does not need PipeWire to be running.
pub fn structure_matches(setup: &Setup, opts: &Options) -> Result<Option<bool>> {
    let Some(installed) = installed_text() else { return Ok(None) };
    let slots = installed_slots(&installed);
    if slots == 0 || required_slots(setup, opts.sample_rate) > slots {
        return Ok(Some(false));
    }
    let o = Options { slots: Some(slots), ..opts.clone() };
    let (fresh, _) = generate(setup, &o).map_err(anyhow::Error::msg)?;
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
        o.sample_rate = rate; // coefficients depend on the running rate
    }
    match structure_matches(setup, &o)? {
        None => Ok(Applied::NotInstalled),
        Some(false) => Ok(Applied::NeedsRestart),
        Some(true) => {
            o.slots = installed_text().map(|t| installed_slots(&t));
            let controls = live_controls(setup, &o).map_err(anyhow::Error::msg)?;
            let values = send_controls_diff(&controls)?;
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
    forget_sent();
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


// ------------------------------------------------------------------ output device

/// The real device LinAPO is playing to, found by following the link out of its sink. This is
/// the device that matters even when LinAPO itself is the system's default output.
pub fn current_output_in(dump: &[serde_json::Value], opts: &Options) -> Option<Sink> {
    let out_name = format!("effect_output.{}", opts.node_name);
    let out_id = node_id_in(dump, &out_name)?;
    let peer = dump
        .iter()
        .filter(|o| o["type"].as_str().is_some_and(|t| t.ends_with("Interface:Link")))
        .find(|o| o["info"]["output-node-id"].as_u64() == Some(out_id as u64))
        .and_then(|o| o["info"]["input-node-id"].as_u64())?;
    let node = dump.iter().find(|o| o["id"].as_u64() == Some(peer) && o["info"]["props"]["node.name"].is_string())?;
    let p = &node["info"]["props"];
    let name = p["node.name"].as_str()?;
    Some(Sink { node_name: name.to_string(), description: p["node.description"].as_str().unwrap_or(name).to_string() })
}

pub fn current_output(opts: &Options) -> Result<Option<Sink>> {
    Ok(current_output_in(&pw_dump()?, opts))
}

// ------------------------------------------------------------------ profiles

/// `$XDG_CONFIG_HOME/linapo` (or `~/.config/linapo`): profiles and state live here.
pub fn config_root() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".config"),
    };
    Ok(base.join("linapo"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileSwitch {
    /// Applied live, nothing restarted.
    Live,
    /// The profile needed more filter slots (or a different layout), so the engine was reinstalled
    /// and PipeWire restarted.
    Reinstalled,
    /// Needs a restart and the caller did not allow it.
    NeedsRestart,
    /// LinAPO is not installed in PipeWire at all.
    NotInstalled,
}

/// Install with enough slots for `setup` and every stored profile, so that switching between
/// them stays live.
pub fn install_sized(setup: &Setup, opts: &Options, store: &ProfileStore, restart: bool) -> Result<Vec<String>> {
    let needed = required_slots(setup, opts.sample_rate).max(store.max_required_slots(opts.sample_rate));
    let sized = Options { slots: Some(slots_for(needed)), ..opts.clone() };
    install(setup, &sized, restart)
}

/// Make `name` the active profile and put it into effect.
pub fn apply_profile(ws: &mut Workspace, name: &str, opts: &Options, allow_reinstall: bool) -> Result<ProfileSwitch> {
    let setup = ws.activate(name).map_err(anyhow::Error::msg)?;
    Ok(match apply_live(&setup, opts)? {
        Applied::Live { .. } => ProfileSwitch::Live,
        Applied::NotInstalled => ProfileSwitch::NotInstalled,
        Applied::NeedsRestart if allow_reinstall => {
            install_sized(&setup, opts, &ws.store, true)?;
            ProfileSwitch::Reinstalled
        }
        Applied::NeedsRestart => ProfileSwitch::NeedsRestart,
    })
}

// ------------------------------------------------------------------ watching

/// Only one process should switch profiles automatically (the background service or a GUI
/// window). The lock is held for as long as this value lives.
pub struct WatchLock {
    _file: std::fs::File,
}

impl WatchLock {
    fn path() -> PathBuf {
        match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(d) if !d.is_empty() => PathBuf::from(d).join("linapo-watch.lock"),
            _ => std::env::temp_dir().join("linapo-watch.lock"),
        }
    }

    pub fn try_acquire() -> Option<WatchLock> {
        Self::try_acquire_at(&Self::path())
    }

    pub fn try_acquire_at(path: &std::path::Path) -> Option<WatchLock> {
        let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path).ok()?;
        file.try_lock().ok()?;
        Some(WatchLock { _file: file })
    }
}

/// Notices output-device changes and switches profiles according to the bindings.
pub struct Watcher {
    pub ws: Workspace,
    pub opts: Options,
    previous: Option<String>,
}

impl Watcher {
    pub fn new(ws: Workspace, opts: Options) -> Watcher {
        Watcher { ws, opts, previous: None }
    }

    pub fn previous_device(&self) -> Option<&str> {
        self.previous.as_deref()
    }

    /// One poll: look at the output device, and switch profile if the bindings say so.
    /// Returns a human-readable note when something happened.
    pub fn tick(&mut self) -> Result<Option<String>> {
        let device = current_output(&self.opts)?.map(|d| d.node_name);
        let opts = self.opts.clone();
        self.tick_with(device, |ws, name| apply_profile(ws, name, &opts, true))
    }

    /// [`Watcher::tick`] with the device and the switching action supplied (for tests).
    pub fn tick_with(
        &mut self,
        device: Option<String>,
        mut apply: impl FnMut(&mut Workspace, &str) -> Result<ProfileSwitch>,
    ) -> Result<Option<String>> {
        self.ws.reload_state();
        let decision = decide_switch(&self.ws.state, self.previous.as_deref(), device.as_deref());
        // Without a device we keep the old one in mind, so unplugging and re-plugging the same
        // headphones does not look like a change.
        if device.is_some() {
            self.previous = device.clone();
        }
        let Some(profile) = decision else { return Ok(None) };
        let how = apply(&mut self.ws, &profile)?;
        Ok(Some(format!(
            "output is now {}: switched to profile '{profile}'{}",
            device.unwrap_or_default(),
            match how {
                ProfileSwitch::Live => "",
                ProfileSwitch::Reinstalled => " (the profile needed more filter slots; audio engine restarted)",
                ProfileSwitch::NeedsRestart => " (needs a restart to take effect)",
                ProfileSwitch::NotInstalled => " (LinAPO is not installed in PipeWire)",
            }
        )))
    }
}

// ------------------------------------------------------------------ autostart

/// A systemd user unit that runs `linapo watch` at login.
pub fn systemd_unit(linapo_exe: &std::path::Path) -> String {
    format!(
        "[Unit]\nDescription=LinAPO profile switcher (follows the output device)\nAfter=pipewire.service wireplumber.service\nWants=pipewire.service\n\n\
         [Service]\nExecStart=\"{}\" watch\nRestart=on-failure\nRestartSec=3\n\n[Install]\nWantedBy=default.target\n",
        linapo_exe.display()
    )
}

pub fn systemd_unit_path() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".config"),
    };
    Ok(base.join("systemd").join("user").join("linapo-watch.service"))
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

    fn ctl(names: &[&str]) -> Vec<(ControlKey, f64)> {
        names.iter().map(|n| (ControlKey(n.to_string()), 1.0)).collect()
    }

    #[test]
    fn batches_never_split_the_controls_of_one_node() {
        // 3 leading controls shift everything, like the matrix + preamp do in a real update
        let mut names = vec!["mix_l:Gain 1", "mix_l:Gain 2", "pre_l:Mult"];
        for n in 1..=5 {
            for c in ["b0", "b1", "b2", "a0", "a1", "a2"] {
                names.push(Box::leak(format!("eq_l_{n}:{c}").into_boxed_str()));
            }
        }
        let controls = ctl(&names);
        let b = batches(&controls, 12);
        assert_eq!(b.iter().map(|c| c.len()).sum::<usize>(), controls.len(), "nothing lost");
        for chunk in &b {
            assert!(chunk.len() <= 12);
            // every eq node present in a chunk is complete in it
            for n in 1..=5 {
                let have = chunk.iter().filter(|(k, _)| k.0.starts_with(&format!("eq_l_{n}:"))).count();
                assert!(have == 0 || have == 6, "eq_l_{n} was split: {have} of 6 in one batch");
            }
        }
        // order is preserved
        let flat: Vec<&str> = b.iter().flat_map(|c| c.iter().map(|(k, _)| k.0.as_str())).collect();
        assert_eq!(flat, names);
    }

    #[test]
    fn batches_handle_edge_cases() {
        assert!(batches(&[], 12).is_empty());
        let one = ctl(&["eq_l_1:b0"]);
        assert_eq!(batches(&one, 12).len(), 1);
        // a node bigger than the limit still goes out whole, alone
        let big: Vec<_> = (0..20).map(|i| (ControlKey(format!("mix_l:Gain {i}")), 1.0)).collect();
        let mixed: Vec<_> = ctl(&["pre_l:Mult"]).into_iter().chain(big.clone()).collect();
        let b = batches(&mixed, 12);
        assert_eq!(b.len(), 2);
        assert_eq!(b[1].len(), 20);
        // diffed updates: single changed coefficients from many different nodes pack tightly
        let sparse = ctl(&["eq_l_1:b0", "eq_l_2:b0", "eq_l_3:b0", "eq_l_4:b0", "eq_r_1:b0", "master_l:Mult"]);
        assert_eq!(batches(&sparse, 12).len(), 1);
    }

    #[test]
    fn core_cookie_identifies_a_daemon_instance() {
        let d = vec![json!({"id": 0, "type": "PipeWire:Interface:Core", "info": {"cookie": 12345}})];
        assert_eq!(core_cookie(&d), "12345");
        assert_eq!(core_cookie(&[]), "");
        assert_ne!(core_cookie(&d), core_cookie(&[json!({"id": 0, "type": "PipeWire:Interface:Core", "info": {"cookie": 999}})]));
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

    fn link_dump() -> Vec<serde_json::Value> {
        vec![
            json!({"id": 36, "info": {"props": {"node.name": "effect_output.linapo"}}}),
            json!({"id": 40, "info": {"props": {"node.name": "alsa_output.usb", "node.description": "USB DAC"}}}),
            json!({"id": 41, "info": {"props": {"node.name": "bluez_output.cans"}}}),
            json!({"id": 70, "type": "PipeWire:Interface:Link", "info": {"output-node-id": 99, "input-node-id": 41}}),
            json!({"id": 71, "type": "PipeWire:Interface:Link", "info": {"output-node-id": 36, "input-node-id": 40}}),
            json!({"id": 72, "type": "PipeWire:Interface:Link", "info": {"output-node-id": 36, "input-node-id": 40}}),
        ]
    }

    #[test]
    fn the_current_output_is_where_the_linapo_sink_plays_to() {
        let opts = Options::default();
        let d = current_output_in(&link_dump(), &opts).unwrap();
        assert_eq!((d.node_name.as_str(), d.description.as_str()), ("alsa_output.usb", "USB DAC"));
        assert_eq!(current_output_in(&[], &opts), None);
        let unlinked: Vec<_> = link_dump().into_iter().filter(|o| o["type"].is_null()).collect();
        assert_eq!(current_output_in(&unlinked, &opts), None, "sink exists but is not linked anywhere");
        // another instance name finds nothing
        assert_eq!(current_output_in(&link_dump(), &Options { node_name: "other".into(), ..opts }), None);
    }

    #[test]
    fn watch_lock_allows_one_holder_at_a_time() {
        let path = std::env::temp_dir().join(format!("linapo-lock-{}", std::process::id()));
        let first = WatchLock::try_acquire_at(&path).expect("first acquires");
        assert!(WatchLock::try_acquire_at(&path).is_none(), "second is refused while the first lives");
        drop(first);
        assert!(WatchLock::try_acquire_at(&path).is_some(), "free again after drop");
        std::fs::remove_file(&path).ok();
    }

    fn watcher(auto: bool) -> (Watcher, PathBuf) {
        let dir = std::env::temp_dir().join(format!("linapo-watch-{}-{}", std::process::id(), auto));
        let _ = std::fs::remove_dir_all(&dir);
        let mut ws = Workspace::open(&dir).unwrap();
        let s = Setup::passthrough(&linapo_core::apo::layout_for(2));
        ws.store.save("Cans", &s).unwrap();
        ws.store.save("Room", &s).unwrap();
        ws.bind("usb", "Cans").unwrap();
        ws.bind("hdmi", "Room").unwrap();
        ws.set_auto_switch(auto).unwrap();
        ws.activate("Room").unwrap();
        (Watcher::new(ws, Options::default()), dir)
    }

    #[test]
    fn watcher_switches_when_the_device_changes_and_only_then() {
        let (mut w, dir) = watcher(true);
        let mut applied = Vec::new();
        let mut apply = |ws: &mut Workspace, name: &str| {
            applied.push(name.to_string());
            ws.activate(name).unwrap();
            Ok(ProfileSwitch::Live)
        };
        // first sight of hdmi: already the active profile (Room), nothing to do
        assert_eq!(w.tick_with(Some("hdmi".into()), &mut apply).unwrap(), None);
        // headphones plugged in
        let note = w.tick_with(Some("usb".into()), &mut apply).unwrap().unwrap();
        assert!(note.contains("usb") && note.contains("'Cans'"), "{note}");
        // same device again: nothing
        assert_eq!(w.tick_with(Some("usb".into()), &mut apply).unwrap(), None);
        // the device disappears for a moment and comes back: still nothing
        assert_eq!(w.tick_with(None, &mut apply).unwrap(), None);
        assert_eq!(w.previous_device(), Some("usb"));
        assert_eq!(w.tick_with(Some("usb".into()), &mut apply).unwrap(), None);
        // back to the speakers
        assert!(w.tick_with(Some("hdmi".into()), &mut apply).unwrap().unwrap().contains("'Room'"));
        assert_eq!(applied, ["Cans", "Room"]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn watcher_does_nothing_when_auto_switching_is_off_and_notices_it_being_turned_on() {
        let (mut w, dir) = watcher(false);
        let mut calls = 0;
        let mut apply = |_: &mut Workspace, _: &str| {
            calls += 1;
            Ok(ProfileSwitch::Live)
        };
        assert_eq!(w.tick_with(Some("usb".into()), &mut apply).unwrap(), None);
        // another process (the GUI) turns auto-switching on; the watcher re-reads the state file
        let mut other = Workspace::open(&dir).unwrap();
        other.set_auto_switch(true).unwrap();
        assert_eq!(w.tick_with(Some("hdmi".into()), &mut apply).unwrap(), None, "hdmi is the active profile's device");
        assert!(w.tick_with(Some("usb".into()), &mut apply).unwrap().is_some());
        assert_eq!(calls, 1);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn watcher_reports_how_the_switch_went() {
        let (mut w, dir) = watcher(true);
        let note = w
            .tick_with(Some("usb".into()), |ws, name| {
                ws.activate(name).unwrap();
                Ok(ProfileSwitch::Reinstalled)
            })
            .unwrap()
            .unwrap();
        assert!(note.contains("restarted"), "{note}");
        let err = w.tick_with(Some("hdmi".into()), |_, _| Err(anyhow::anyhow!("boom")));
        assert!(err.is_err(), "errors from applying are passed on");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn systemd_unit_runs_watch_and_waits_for_pipewire() {
        let u = systemd_unit(std::path::Path::new("/home/me/.cargo/bin/linapo"));
        assert!(u.contains("ExecStart=\"/home/me/.cargo/bin/linapo\" watch"));
        assert!(u.contains("After=pipewire.service") && u.contains("WantedBy=default.target"));
        assert!(u.contains("Restart=on-failure"));
    }
}
