//! Named profiles, the active profile, and per-device bindings.
//!
//! Layout on disk (under `~/.config/linapo/`):
//!
//! ```text
//! profiles/Headphones.json   one Setup per profile
//! profiles/Speakers.json
//! state.json                 active profile, device bindings, auto-switch flag
//! ```
//!
//! This module is plain file handling: no PipeWire, no GUI.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::apo::layout_for;
use crate::pipewire_conf::required_slots;
use crate::setup::{Setup, SetupError};

pub const DEFAULT_PROFILE: &str = "Default";
const MAX_NAME_LEN: usize = 64;

/// Check a profile name and return it trimmed. Names become file names, so path separators,
/// control characters and a leading dot are rejected.
pub fn valid_name(name: &str) -> Result<String, String> {
    let n = name.trim();
    if n.is_empty() {
        return Err("a profile needs a name".into());
    }
    if n.chars().count() > MAX_NAME_LEN {
        return Err(format!("a profile name can have at most {MAX_NAME_LEN} characters"));
    }
    if n.starts_with('.') {
        return Err("a profile name cannot start with a dot".into());
    }
    if let Some(c) = n.chars().find(|c| c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')) {
        return Err(format!("a profile name cannot contain {c:?}"));
    }
    Ok(n.to_string())
}

#[derive(Debug)]
pub struct ProfileStore {
    dir: PathBuf,
}

impl ProfileStore {
    pub fn new(dir: impl Into<PathBuf>) -> ProfileStore {
        ProfileStore { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path_of(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.json"))
    }

    pub fn exists(&self, name: &str) -> bool {
        self.path_of(name).is_file()
    }

    /// Profile names, sorted case-insensitively.
    pub fn list(&self) -> io::Result<Vec<String>> {
        let rd = match fs::read_dir(&self.dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut names: Vec<String> = rd
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let p = e.path();
                (p.extension()? == "json").then(|| p.file_stem()?.to_str().map(str::to_string))?
            })
            .filter(|n| valid_name(n).is_ok())
            .collect();
        names.sort_by_key(|n| n.to_lowercase());
        Ok(names)
    }

    pub fn load(&self, name: &str) -> Result<Setup, SetupError> {
        Setup::load(&self.path_of(&valid_name(name).map_err(SetupError::Invalid)?))
    }

    /// Save (create or overwrite).
    pub fn save(&self, name: &str, setup: &Setup) -> Result<(), SetupError> {
        let name = valid_name(name).map_err(SetupError::Invalid)?;
        fs::create_dir_all(&self.dir).map_err(SetupError::Io)?;
        setup.save(&self.path_of(&name))
    }

    pub fn delete(&self, name: &str) -> io::Result<()> {
        fs::remove_file(self.path_of(name))
    }

    /// Rename without overwriting an existing profile.
    pub fn rename(&self, old: &str, new: &str) -> Result<(), String> {
        let new = valid_name(new)?;
        if !self.exists(old) {
            return Err(format!("no profile named '{old}'"));
        }
        if new != old && self.exists(&new) {
            return Err(format!("a profile named '{new}' already exists"));
        }
        fs::rename(self.path_of(old), self.path_of(&new)).map_err(|e| e.to_string())
    }

    pub fn duplicate(&self, from: &str, to: &str) -> Result<(), String> {
        let to = valid_name(to)?;
        if self.exists(&to) {
            return Err(format!("a profile named '{to}' already exists"));
        }
        let s = self.load(from).map_err(|e| e.to_string())?;
        self.save(&to, &s).map_err(|e| e.to_string())
    }

    /// A free name based on `base`: `base`, `base 2`, `base 3`...
    pub fn unique_name(&self, base: &str) -> String {
        let base = valid_name(base).unwrap_or_else(|_| "Profile".to_string());
        if !self.exists(&base) {
            return base;
        }
        (2..).map(|i| format!("{base} {i}")).find(|n| !self.exists(n)).unwrap()
    }

    /// The most filters any stored profile needs on one channel, so the installed graph can be
    /// sized once and every profile then switches live.
    pub fn max_required_slots(&self, sample_rate: f64) -> usize {
        self.list()
            .unwrap_or_default()
            .iter()
            .filter_map(|n| self.load(n).ok())
            .map(|s| required_slots(&s, sample_rate))
            .max()
            .unwrap_or(0)
    }
}

/// Persistent choices that are not part of any one profile.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct State {
    /// The profile being edited / applied.
    #[serde(default)]
    pub active: Option<String>,
    /// Switch profiles when the output device changes.
    #[serde(default)]
    pub auto_switch: bool,
    /// PipeWire sink node name -> profile name.
    #[serde(default)]
    pub bindings: BTreeMap<String, String>,
    /// Profile for devices without a binding (`None`: leave the active profile alone).
    #[serde(default)]
    pub default_profile: Option<String>,
}

impl State {
    pub fn profile_for_device(&self, device: &str) -> Option<&str> {
        self.bindings.get(device).map(String::as_str).or(self.default_profile.as_deref())
    }
}

/// What to do when the output device is (now) `device` and was `previous` before.
/// Returns the profile to switch to, or `None` to leave things alone: nothing changed, no
/// auto-switching, no matching profile, or it is already the active one.
pub fn decide_switch(state: &State, previous: Option<&str>, device: Option<&str>) -> Option<String> {
    if !state.auto_switch || device == previous {
        return None;
    }
    let target = state.profile_for_device(device?)?;
    (state.active.as_deref() != Some(target)).then(|| target.to_string())
}

/// The store plus its state file.
#[derive(Debug)]
pub struct Workspace {
    pub store: ProfileStore,
    state_path: PathBuf,
    pub state: State,
}

impl Workspace {
    /// Open (creating as needed) the workspace under `root`, e.g. `~/.config/linapo`.
    ///
    /// A legacy single `setup.json` in `root` becomes the profile "Default". If there are still
    /// no profiles, a pass-through stereo "Default" is created. `active` always names an
    /// existing profile afterwards.
    pub fn open(root: &Path) -> io::Result<Workspace> {
        fs::create_dir_all(root)?;
        let store = ProfileStore::new(root.join("profiles"));
        let state_path = root.join("state.json");
        let mut state: State = fs::read_to_string(&state_path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();

        let mut names = store.list()?;
        if names.is_empty() {
            let legacy = root.join("setup.json");
            let setup = Setup::load(&legacy).unwrap_or_else(|_| Setup::passthrough(&layout_for(2)));
            store.save(DEFAULT_PROFILE, &setup).map_err(|e| io::Error::other(e.to_string()))?;
            if legacy.is_file() {
                let _ = fs::rename(&legacy, root.join("setup.json.migrated"));
            }
            names = store.list()?;
        }
        if !state.active.as_ref().is_some_and(|a| names.contains(a)) {
            state.active = names.first().cloned();
        }
        let ws = Workspace { store, state_path, state };
        ws.save_state()?;
        Ok(ws)
    }

    pub fn save_state(&self) -> io::Result<()> {
        let tmp = self.state_path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(&self.state).expect("state serialises"))?;
        fs::rename(&tmp, &self.state_path)
    }

    /// Re-read the state file (another process may have changed it). Returns whether it changed.
    pub fn reload_state(&mut self) -> bool {
        let Some(new) = fs::read_to_string(&self.state_path).ok().and_then(|t| serde_json::from_str::<State>(&t).ok()) else {
            return false;
        };
        let changed = new != self.state;
        self.state = new;
        changed
    }

    pub fn active_name(&self) -> &str {
        self.state.active.as_deref().unwrap_or(DEFAULT_PROFILE)
    }

    pub fn active_path(&self) -> PathBuf {
        self.store.path_of(self.active_name())
    }

    pub fn load_active(&self) -> Result<Setup, SetupError> {
        self.store.load(self.active_name())
    }

    /// Make `name` the active profile and return its setup.
    pub fn activate(&mut self, name: &str) -> Result<Setup, String> {
        let setup = self.store.load(name).map_err(|e| e.to_string())?;
        self.state.active = Some(name.to_string());
        self.save_state().map_err(|e| e.to_string())?;
        Ok(setup)
    }

    /// Rename a profile and keep the active name, bindings and default pointing at it.
    pub fn rename(&mut self, old: &str, new: &str) -> Result<(), String> {
        let new = valid_name(new)?;
        self.store.rename(old, &new)?;
        let fix = |s: &mut String| {
            if s == old {
                *s = new.clone();
            }
        };
        if let Some(a) = self.state.active.as_mut() {
            fix(a);
        }
        if let Some(d) = self.state.default_profile.as_mut() {
            fix(d);
        }
        self.state.bindings.values_mut().for_each(fix);
        self.save_state().map_err(|e| e.to_string())
    }

    /// Delete a profile (the last one cannot be deleted). Bindings to it are removed and the
    /// active profile moves to another one if needed; returns the active profile afterwards.
    pub fn delete(&mut self, name: &str) -> Result<String, String> {
        let names = self.store.list().map_err(|e| e.to_string())?;
        if !names.iter().any(|n| n == name) {
            return Err(format!("no profile named '{name}'"));
        }
        if names.len() == 1 {
            return Err("the last profile cannot be deleted".into());
        }
        self.store.delete(name).map_err(|e| e.to_string())?;
        self.state.bindings.retain(|_, p| p != name);
        if self.state.default_profile.as_deref() == Some(name) {
            self.state.default_profile = None;
        }
        if self.state.active.as_deref() == Some(name) {
            self.state.active = names.into_iter().find(|n| n != name);
        }
        self.save_state().map_err(|e| e.to_string())?;
        Ok(self.active_name().to_string())
    }

    pub fn bind(&mut self, device: &str, profile: &str) -> Result<(), String> {
        if !self.store.exists(profile) {
            return Err(format!("no profile named '{profile}'"));
        }
        self.state.bindings.insert(device.to_string(), profile.to_string());
        self.save_state().map_err(|e| e.to_string())
    }

    pub fn unbind(&mut self, device: &str) -> Result<bool, String> {
        let had = self.state.bindings.remove(device).is_some();
        self.save_state().map_err(|e| e.to_string())?;
        Ok(had)
    }

    pub fn set_default_profile(&mut self, profile: Option<&str>) -> Result<(), String> {
        if let Some(p) = profile {
            if !self.store.exists(p) {
                return Err(format!("no profile named '{p}'"));
            }
        }
        self.state.default_profile = profile.map(str::to_string);
        self.save_state().map_err(|e| e.to_string())
    }

    pub fn set_auto_switch(&mut self, on: bool) -> Result<(), String> {
        self.state.auto_switch = on;
        self.save_state().map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::{Filter, FilterKind};
    use crate::setup::EqPreset;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("linapo-prof-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn setup_with_gain(g: f64) -> Setup {
        let mut s = Setup::passthrough(&layout_for(2));
        s.master_gain_db = g;
        s
    }

    #[test]
    fn names_are_validated_because_they_become_file_names() {
        assert_eq!(valid_name("  Headphones  ").unwrap(), "Headphones");
        assert_eq!(valid_name("Night mode (quiet)").unwrap(), "Night mode (quiet)");
        for bad in ["", "   ", "a/b", "a\\b", "..", ".hidden", "a:b", "tab\there", "x*", &"n".repeat(65)] {
            assert!(valid_name(bad).is_err(), "{bad:?}");
        }
        assert!(valid_name(&"n".repeat(64)).is_ok());
    }

    #[test]
    fn save_load_list_delete() {
        let dir = tmp("crud");
        let st = ProfileStore::new(&dir);
        assert!(st.list().unwrap().is_empty(), "missing directory is an empty list");
        st.save("speakers", &setup_with_gain(-1.0)).unwrap();
        st.save("Headphones", &setup_with_gain(-2.0)).unwrap();
        st.save("alpha", &setup_with_gain(0.0)).unwrap();
        assert_eq!(st.list().unwrap(), ["alpha", "Headphones", "speakers"], "case-insensitive order");
        assert_eq!(st.load("Headphones").unwrap().master_gain_db, -2.0);
        st.save("Headphones", &setup_with_gain(-5.0)).unwrap();
        assert_eq!(st.load("Headphones").unwrap().master_gain_db, -5.0, "overwrite");
        st.delete("alpha").unwrap();
        assert_eq!(st.list().unwrap().len(), 2);
        assert!(st.save("../escape", &setup_with_gain(0.0)).is_err());
        assert!(!dir.parent().unwrap().join("escape.json").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_ignores_other_files() {
        let dir = tmp("junk");
        let st = ProfileStore::new(&dir);
        st.save("real", &setup_with_gain(0.0)).unwrap();
        fs::write(dir.join("notes.txt"), "x").unwrap();
        fs::write(dir.join(".hidden.json"), "{}").unwrap();
        fs::write(dir.join("real.json.tmp"), "{}").unwrap();
        assert_eq!(st.list().unwrap(), ["real"]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rename_duplicate_and_unique_names_never_overwrite() {
        let dir = tmp("rename");
        let st = ProfileStore::new(&dir);
        st.save("a", &setup_with_gain(1.0)).unwrap();
        st.save("b", &setup_with_gain(2.0)).unwrap();
        assert!(st.rename("a", "b").unwrap_err().contains("already exists"));
        assert!(st.rename("nope", "c").unwrap_err().contains("no profile"));
        st.rename("a", "c").unwrap();
        assert_eq!(st.list().unwrap(), ["b", "c"]);
        assert!(st.duplicate("c", "b").is_err());
        st.duplicate("c", "d").unwrap();
        assert_eq!(st.load("d").unwrap().master_gain_db, 1.0);
        assert_eq!(st.unique_name("e"), "e");
        assert_eq!(st.unique_name("b"), "b 2");
        st.save("b 2", &setup_with_gain(0.0)).unwrap();
        assert_eq!(st.unique_name("b"), "b 3");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_broken_profile_file_is_an_error_not_a_panic() {
        let dir = tmp("broken");
        let st = ProfileStore::new(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("bad.json"), "{ nope").unwrap();
        assert!(st.load("bad").is_err());
        assert_eq!(st.list().unwrap(), ["bad"]);
        assert_eq!(st.max_required_slots(48_000.0), 0, "unreadable profiles are skipped");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn max_required_slots_looks_at_every_profile() {
        let dir = tmp("slots");
        let st = ProfileStore::new(&dir);
        let mut big = setup_with_gain(0.0);
        big.eqs.push(EqPreset {
            name: "x".into(),
            enabled: true,
            preamp_db: 0.0,
            filters: (0..20).map(|i| Filter { kind: FilterKind::Peaking, fc_hz: 100.0 + 40.0 * i as f64, gain_db: 1.0, q: 1.0, enabled: true }).collect(),
            channels: layout_for(2),
        });
        st.save("small", &setup_with_gain(0.0)).unwrap();
        st.save("big", &big).unwrap();
        assert_eq!(st.max_required_slots(48_000.0), 20);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn workspace_creates_a_default_profile_on_first_run() {
        let dir = tmp("first");
        let ws = Workspace::open(&dir).unwrap();
        assert_eq!(ws.store.list().unwrap(), [DEFAULT_PROFILE]);
        assert_eq!(ws.active_name(), DEFAULT_PROFILE);
        assert_eq!(ws.load_active().unwrap(), Setup::passthrough(&layout_for(2)));
        assert!(dir.join("state.json").is_file());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn workspace_migrates_a_legacy_setup_file() {
        let dir = tmp("legacy");
        fs::create_dir_all(&dir).unwrap();
        setup_with_gain(-7.0).save(&dir.join("setup.json")).unwrap();
        let ws = Workspace::open(&dir).unwrap();
        assert_eq!(ws.load_active().unwrap().master_gain_db, -7.0, "the old setup became the Default profile");
        assert!(!dir.join("setup.json").exists() && dir.join("setup.json.migrated").exists());
        let ws2 = Workspace::open(&dir).unwrap();
        assert_eq!(ws2.store.list().unwrap().len(), 1, "no second migration");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn state_survives_reopening_and_a_missing_active_profile_is_repaired() {
        let dir = tmp("state");
        let mut ws = Workspace::open(&dir).unwrap();
        ws.store.save("Cans", &setup_with_gain(-3.0)).unwrap();
        let s = ws.activate("Cans").unwrap();
        assert_eq!(s.master_gain_db, -3.0);
        ws.bind("dev1", "Cans").unwrap();
        ws.set_auto_switch(true).unwrap();
        let again = Workspace::open(&dir).unwrap();
        assert_eq!(again.active_name(), "Cans");
        assert!(again.state.auto_switch);
        assert_eq!(again.state.bindings["dev1"], "Cans");
        // active points at a profile that was deleted behind our back
        fs::remove_file(again.store.path_of("Cans")).unwrap();
        let repaired = Workspace::open(&dir).unwrap();
        assert_eq!(repaired.active_name(), DEFAULT_PROFILE);
        assert!(activate_missing(&repaired));
        fs::remove_dir_all(&dir).ok();
    }

    fn activate_missing(ws: &Workspace) -> bool {
        ws.store.load("ghost").is_err()
    }

    #[test]
    fn corrupt_state_file_falls_back_to_defaults() {
        let dir = tmp("corrupt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("state.json"), "not json").unwrap();
        let ws = Workspace::open(&dir).unwrap();
        assert_eq!(ws.active_name(), DEFAULT_PROFILE);
        assert!(!ws.state.auto_switch);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rename_and_delete_keep_state_consistent() {
        let dir = tmp("consistency");
        let mut ws = Workspace::open(&dir).unwrap();
        ws.store.save("Cans", &setup_with_gain(0.0)).unwrap();
        ws.store.save("Room", &setup_with_gain(0.0)).unwrap();
        ws.activate("Cans").unwrap();
        ws.bind("usb", "Cans").unwrap();
        ws.bind("hdmi", "Room").unwrap();
        ws.set_default_profile(Some("Cans")).unwrap();

        ws.rename("Cans", "Headphones").unwrap();
        assert_eq!(ws.active_name(), "Headphones");
        assert_eq!(ws.state.bindings["usb"], "Headphones");
        assert_eq!(ws.state.default_profile.as_deref(), Some("Headphones"));
        assert!(ws.rename("Headphones", "Room").is_err());

        let now_active = ws.delete("Headphones").unwrap();
        assert_ne!(now_active, "Headphones");
        assert!(ws.store.exists(&now_active));
        assert!(!ws.state.bindings.contains_key("usb"), "bindings to a deleted profile go away");
        assert_eq!(ws.state.default_profile, None);
        assert_eq!(ws.state.bindings["hdmi"], "Room");

        ws.delete("Room").unwrap();
        assert!(ws.delete(&ws.active_name().to_string()).unwrap_err().contains("last profile"));
        assert!(ws.delete("ghost").is_err());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bind_validates_profiles() {
        let dir = tmp("bind");
        let mut ws = Workspace::open(&dir).unwrap();
        assert!(ws.bind("dev", "ghost").is_err());
        assert!(ws.set_default_profile(Some("ghost")).is_err());
        ws.bind("dev", DEFAULT_PROFILE).unwrap();
        assert!(ws.unbind("dev").unwrap());
        assert!(!ws.unbind("dev").unwrap());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reload_state_sees_changes_made_by_another_process() {
        let dir = tmp("reload");
        let mut a = Workspace::open(&dir).unwrap();
        let mut b = Workspace::open(&dir).unwrap();
        a.store.save("Other", &setup_with_gain(0.0)).unwrap();
        a.activate("Other").unwrap();
        assert!(b.reload_state());
        assert_eq!(b.active_name(), "Other");
        assert!(!b.reload_state(), "nothing new");
        fs::remove_dir_all(&dir).ok();
    }

    fn state(auto: bool, active: &str) -> State {
        let mut s = State { active: Some(active.into()), auto_switch: auto, ..State::default() };
        s.bindings.insert("usb".into(), "Cans".into());
        s.bindings.insert("hdmi".into(), "Room".into());
        s
    }

    #[test]
    fn switching_decisions() {
        let s = state(true, "Room");
        assert_eq!(decide_switch(&s, Some("hdmi"), Some("usb")).as_deref(), Some("Cans"));
        assert_eq!(decide_switch(&s, None, Some("usb")).as_deref(), Some("Cans"), "at startup");
        assert_eq!(decide_switch(&s, Some("usb"), Some("usb")), None, "no change of device");
        assert_eq!(decide_switch(&s, Some("usb"), Some("hdmi")), None, "already the active profile");
        assert_eq!(decide_switch(&s, Some("usb"), Some("unknown")), None, "no binding and no default");
        assert_eq!(decide_switch(&s, Some("usb"), None), None, "no device at all");
        assert_eq!(decide_switch(&state(false, "Room"), None, Some("usb")), None, "auto-switch is off");
        let mut with_default = state(true, "Cans");
        with_default.default_profile = Some("Room".into());
        assert_eq!(decide_switch(&with_default, Some("usb"), Some("bluetooth")).as_deref(), Some("Room"), "unbound devices use the default");
    }
}
