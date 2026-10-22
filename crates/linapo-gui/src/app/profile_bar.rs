//! Profiles in the GUI: the profile bar, dialogs, per-device bindings and automatic switching.
//!
//! This is a child module of `app`, so it can use `App`'s private fields.

use super::*;
use linapo_core::profiles::decide_switch;

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// A small modal asking for a name (or confirming a delete).
#[derive(Clone, Debug, PartialEq)]
pub enum ProfileDialog {
    New(String),
    Duplicate(String),
    Rename(String),
    Delete,
}

impl App {
    /// Use named profiles: the active profile's file is what gets autosaved.
    pub fn with_workspace(mut self, ws: Workspace) -> App {
        self.loaded_profile = ws.active_name().to_string();
        self.path = Some(ws.active_path());
        self.file_field = ws.active_path().display().to_string();
        self.profiles = ws.store.list().unwrap_or_default();
        self.workspace = Some(ws);
        self
    }

    pub fn has_workspace(&self) -> bool {
        self.workspace.is_some()
    }

    pub fn active_profile(&self) -> &str {
        &self.loaded_profile
    }

    pub fn profile_names(&self) -> &[String] {
        &self.profiles
    }

    pub fn auto_switch_enabled(&self) -> bool {
        self.workspace.as_ref().is_some_and(|w| w.state.auto_switch)
    }

    /// This window holds the lock that makes it the one switching profiles automatically.
    pub fn switching_here(&self) -> bool {
        self.watch_lock.is_some()
    }

    pub fn bindings(&self) -> Vec<(String, String)> {
        self.workspace.as_ref().map(|w| w.state.bindings.iter().map(|(d, p)| (d.clone(), p.clone())).collect()).unwrap_or_default()
    }

    fn refresh_profiles(&mut self) {
        if let Some(ws) = &self.workspace {
            self.profiles = ws.store.list().unwrap_or_default();
        }
    }

    /// Replace the working setup by another profile's, as a fresh document (no undo across profiles).
    fn load_profile_into_app(&mut self, setup: Setup, name: &str) {
        let Some(ws) = &self.workspace else { return };
        let path = ws.store.path_of(name);
        self.committed = setup.clone();
        self.setup = setup;
        self.loaded_profile = name.to_string();
        self.file_field = path.display().to_string();
        self.path = Some(path);
        self.pending_before = None;
        self.gesture_at = None;
        self.undo.clear();
        self.redo.clear();
        self.selected_eq = 0;
        self.selected_filter = None;
        self.monitor = 0;
        self.save_since = None;
        self.saved = true;
        self.dirty_since = Some(Instant::now()); // push it to PipeWire
        self.refresh_profiles();
    }

    /// Make `name` the active profile (after saving the one being left).
    pub fn switch_profile(&mut self, name: &str) {
        if self.workspace.is_none() || name == self.loaded_profile {
            return;
        }
        self.commit_history();
        self.save_now();
        let loaded = self.workspace.as_mut().map(|ws| ws.activate(name));
        match loaded {
            Some(Ok(setup)) => {
                self.load_profile_into_app(setup, name);
                self.say(format!("Profile '{name}'"), false);
            }
            Some(Err(e)) => self.say(e, true),
            None => {}
        }
    }

    /// A new profile with the current channels but no EQ, identity matrix and 0 dB master.
    pub fn create_profile(&mut self, name: &str) -> Result<(), String> {
        let name = valid_name(name)?;
        let ws = self.workspace.as_ref().ok_or("profiles are not available")?;
        if ws.store.exists(&name) {
            return Err(format!("a profile named '{name}' already exists"));
        }
        let mut s = self.setup.clone();
        s.eqs.clear();
        s.master_gain_db = 0.0;
        s.bypass_eq = false;
        crate::model::matrix_identity(&mut s);
        ws.store.save(&name, &s).map_err(|e| e.to_string())?;
        self.switch_profile(&name);
        Ok(())
    }

    /// Copy the current profile under a new name and switch to the copy.
    pub fn duplicate_profile(&mut self, name: &str) -> Result<(), String> {
        let name = valid_name(name)?;
        self.commit_history();
        self.save_now();
        let from = self.loaded_profile.clone();
        let ws = self.workspace.as_ref().ok_or("profiles are not available")?;
        ws.store.duplicate(&from, &name)?;
        self.switch_profile(&name);
        Ok(())
    }

    pub fn rename_active_profile(&mut self, new: &str) -> Result<(), String> {
        self.commit_history();
        self.save_now();
        let old = self.loaded_profile.clone();
        let ws = self.workspace.as_mut().ok_or("profiles are not available")?;
        ws.rename(&old, new)?;
        let new = valid_name(new)?;
        self.path = Some(ws.store.path_of(&new));
        self.file_field = self.path.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
        self.loaded_profile = new;
        self.refresh_profiles();
        Ok(())
    }

    pub fn delete_active_profile(&mut self) -> Result<(), String> {
        let old = self.loaded_profile.clone();
        let ws = self.workspace.as_mut().ok_or("profiles are not available")?;
        let now = ws.delete(&old)?;
        let setup = ws.store.load(&now).map_err(|e| e.to_string())?;
        // The deleted profile's pending edits must not recreate its file.
        self.pending_before = None;
        self.gesture_at = None;
        self.save_since = None;
        self.load_profile_into_app(setup, &now);
        self.say(format!("Deleted '{old}'; now using '{now}'"), false);
        Ok(())
    }

    pub fn set_auto_switch(&mut self, on: bool) {
        if let Some(ws) = self.workspace.as_mut() {
            if let Err(e) = ws.set_auto_switch(on) {
                self.say(e, true);
            }
        }
        self.refresh_lock();
    }

    /// Use the active profile whenever the current output device is in use.
    pub fn bind_current_device(&mut self) {
        let Some(dev) = self.current_device.clone() else {
            return self.say("No output device is known yet (is LinAPO installed and running?)", true);
        };
        let profile = self.loaded_profile.clone();
        if let Some(ws) = self.workspace.as_mut() {
            match ws.bind(&dev.node_name, &profile) {
                Ok(()) => self.say(format!("{} now uses profile '{profile}'", dev.description), false),
                Err(e) => self.say(e, true),
            }
        }
    }

    pub fn bind_device(&mut self, device: &str, profile: &str) {
        if let Some(ws) = self.workspace.as_mut() {
            if let Err(e) = ws.bind(device, profile) {
                self.say(e, true);
            }
        }
    }

    pub fn unbind_device(&mut self, device: &str) {
        if let Some(ws) = self.workspace.as_mut() {
            if let Err(e) = ws.unbind(device) {
                self.say(e, true);
            }
        }
    }

    pub fn set_default_profile(&mut self, profile: Option<&str>) {
        if let Some(ws) = self.workspace.as_mut() {
            if let Err(e) = ws.set_default_profile(profile) {
                self.say(e, true);
            }
        }
    }

    /// Hold (or release) the lock that makes this window the one switching automatically.
    pub(super) fn refresh_lock(&mut self) {
        let want = self.auto_switch_enabled();
        if !want {
            self.watch_lock = None;
            self.watch_elsewhere = false;
        } else if self.watch_lock.is_none() {
            self.watch_lock = match &self.cfg.watch_lock_path {
                Some(p) => WatchLock::try_acquire_at(p),
                None => WatchLock::try_acquire(),
            };
            self.watch_elsewhere = self.watch_lock.is_none();
        }
    }

    pub(super) fn poll_device_if_due(&mut self, ctx: &egui::Context) {
        if self.workspace.is_none() {
            return;
        }
        let now = Instant::now();
        if now >= self.next_poll {
            self.worker.send(Job::PollDevice);
            self.next_poll = now + self.cfg.poll_interval;
        }
        if self.cfg.repaint_while_idle {
            ctx.request_repaint_after(self.cfg.poll_interval);
        }
    }

    /// The output device was (re)read: follow switches made by another process, and switch
    /// profiles ourselves if auto-switching is on and this window holds the lock.
    pub(super) fn on_device(&mut self, device: Option<Sink>) {
        self.current_device = device.clone();
        let name = device.map(|d| d.node_name);
        let idle = self.dirty_since.is_none() && self.save_since.is_none() && self.pending_before.is_none();

        // Another process (the background service) may have changed the active profile.
        let external = self.workspace.as_mut().and_then(|ws| {
            ws.reload_state();
            (ws.active_name() != self.loaded_profile).then(|| ws.active_name().to_string())
        });
        if let (Some(active), true) = (external, idle) {
            let loaded = self.workspace.as_ref().map(|ws| ws.store.load(&active));
            if let Some(Ok(setup)) = loaded {
                self.load_profile_into_app(setup, &active);
                self.say(format!("Switched to profile '{active}'"), false);
            }
        }

        self.refresh_lock();
        if self.watch_lock.is_none() {
            return;
        }
        let state = self.workspace.as_ref().map(|ws| ws.state.clone());
        if let Some(state) = state {
            let decision = decide_switch(&state, self.last_device.as_deref(), name.as_deref());
            if name.is_some() {
                self.last_device = name.clone();
            }
            if let Some(profile) = decision {
                self.switch_profile(&profile);
                self.say(format!("Output is now {}: switched to profile '{profile}'", name.unwrap_or_default()), false);
            }
        }
    }

    // ------------------------------------------------------------ UI

    pub(super) fn profile_bar(&mut self, ui: &mut Ui) {
        let mut switch_to: Option<String> = None;
        ui.add_space(3.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Profile").weak());
            let active = self.loaded_profile.clone();
            egui::ComboBox::from_id_salt("profile").selected_text(active.clone()).width(170.0).show_ui(ui, |ui| {
                for n in &self.profiles {
                    if ui.selectable_label(*n == active, n).clicked() {
                        switch_to = Some(n.clone());
                    }
                }
            });
            if ui.button("New profile").on_hover_text("An empty profile with the current channels").clicked() {
                self.profile_dialog = Some(ProfileDialog::New(String::new()));
            }
            if ui.button("Duplicate profile").clicked() {
                self.profile_dialog = Some(ProfileDialog::Duplicate(format!("{active} copy")));
            }
            if ui.button("Rename profile").clicked() {
                self.profile_dialog = Some(ProfileDialog::Rename(active.clone()));
            }
            if ui.button("Delete profile").clicked() {
                self.profile_dialog = Some(ProfileDialog::Delete);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (text, bound) = match &self.current_device {
                    Some(d) => {
                        let bound = self.workspace.as_ref().and_then(|w| w.state.profile_for_device(&d.node_name)).map(str::to_string);
                        (d.description.clone(), bound)
                    }
                    None => ("unknown".to_string(), None),
                };
                let mut line = format!("Output: {text}");
                if let Some(b) = &bound {
                    line.push_str(&format!("  (profile {b})"));
                }
                ui.label(RichText::new(line).weak());
                if self.auto_switch_enabled() {
                    let note = if self.watch_lock.is_some() { "auto" } else { "auto (background service)" };
                    ui.label(RichText::new(note).color(ACCENT).small());
                }
            });
        });
        ui.add_space(3.0);
        if let Some(n) = switch_to {
            self.switch_profile(&n);
        }
    }

    pub(super) fn profile_dialog_window(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.profile_dialog.clone() else { return };
        let (title, ok_label) = match &dialog {
            ProfileDialog::New(_) => ("New profile", "Create"),
            ProfileDialog::Duplicate(_) => ("Duplicate profile", "Duplicate"),
            ProfileDialog::Rename(_) => ("Rename profile", "Rename"),
            ProfileDialog::Delete => ("Delete profile", "Delete"),
        };
        let mut text = match &dialog {
            ProfileDialog::New(t) | ProfileDialog::Duplicate(t) | ProfileDialog::Rename(t) => t.clone(),
            ProfileDialog::Delete => String::new(),
        };
        let mut result: Option<bool> = None; // Some(true) = confirmed, Some(false) = cancelled
        let mut problem: Option<String> = None;
        let existing = &self.profiles;
        let active = self.loaded_profile.clone();
        egui::Window::new(title).collapsible(false).resizable(false).anchor(Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
            match &dialog {
                ProfileDialog::Delete => {
                    ui.label(format!("Delete the profile '{active}'? This cannot be undone."));
                    if existing.len() <= 1 {
                        problem = Some("This is the only profile; create another one first.".into());
                    }
                }
                _ => {
                    let r = ui.add(egui::TextEdit::singleline(&mut text).hint_text("profile name").desired_width(260.0));
                    r.request_focus();
                    match valid_name(&text) {
                        Err(e) if !text.trim().is_empty() => problem = Some(capitalize(&e)),
                        Err(_) => problem = Some("Type a name.".into()),
                        Ok(n) => {
                            let taken = existing.contains(&n) && !(matches!(dialog, ProfileDialog::Rename(_)) && n == active);
                            if taken {
                                problem = Some(format!("A profile named '{n}' already exists."));
                            }
                        }
                    }
                }
            }
            if let Some(p) = &problem {
                ui.label(RichText::new(p).color(WARN));
            }
            ui.horizontal(|ui| {
                if ui.add_enabled(problem.is_none(), egui::Button::new(ok_label)).clicked() {
                    result = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    result = Some(false);
                }
            });
        });
        // keep what was typed between frames
        self.profile_dialog = match dialog {
            ProfileDialog::New(_) => Some(ProfileDialog::New(text.clone())),
            ProfileDialog::Duplicate(_) => Some(ProfileDialog::Duplicate(text.clone())),
            ProfileDialog::Rename(_) => Some(ProfileDialog::Rename(text.clone())),
            ProfileDialog::Delete => Some(ProfileDialog::Delete),
        };
        match result {
            Some(false) => self.profile_dialog = None,
            Some(true) => {
                let outcome = match dialog {
                    ProfileDialog::New(_) => self.create_profile(&text),
                    ProfileDialog::Duplicate(_) => self.duplicate_profile(&text),
                    ProfileDialog::Rename(_) => self.rename_active_profile(&text),
                    ProfileDialog::Delete => self.delete_active_profile(),
                };
                match outcome {
                    Ok(()) => self.profile_dialog = None,
                    Err(e) => self.say(e, true),
                }
            }
            None => {}
        }
    }

    /// The "Profiles and devices" block of the Setup tab.
    pub(super) fn profiles_section(&mut self, ui: &mut Ui) {
        if self.workspace.is_none() {
            return;
        }
        ui.label(RichText::new("Profiles and output devices").heading());
        ui.add_space(4.0);
        let names = self.profiles.clone();
        let mut auto = self.auto_switch_enabled();
        if ui.checkbox(&mut auto, "Switch profiles automatically when the output device changes").changed() {
            self.set_auto_switch(auto);
        }
        if auto {
            let note = if self.watch_lock.is_some() {
                "Handled by this window. To keep switching when it is closed, run `linapo autostart enable`."
            } else if self.watch_elsewhere {
                "Handled by the background service (`linapo watch`)."
            } else {
                "Starting..."
            };
            ui.label(RichText::new(note).weak().small());
        }
        ui.add_space(4.0);

        // The device in use right now.
        ui.horizontal(|ui| {
            match &self.current_device {
                Some(d) => ui.label(format!("Output device now: {}", d.description)),
                None => ui.label("Output device now: unknown (is LinAPO installed and running?)"),
            };
            if ui.add_enabled(self.current_device.is_some(), egui::Button::new("Use this profile for this device")).clicked() {
                self.bind_current_device();
            }
        });

        // Existing bindings.
        let descriptions: std::collections::HashMap<String, String> =
            self.sinks.iter().map(|s| (s.node_name.clone(), s.description.clone())).collect();
        let mut rebind: Option<(String, String)> = None;
        let mut remove: Option<String> = None;
        for (device, profile) in self.bindings() {
            ui.horizontal(|ui| {
                let label = descriptions.get(&device).cloned().unwrap_or_else(|| device.clone());
                ui.label(RichText::new(label).strong()).on_hover_text(&device);
                ui.label("uses");
                egui::ComboBox::from_id_salt(("bind", &device)).selected_text(profile.clone()).show_ui(ui, |ui| {
                    for n in &names {
                        if ui.selectable_label(*n == profile, n).clicked() {
                            rebind = Some((device.clone(), n.clone()));
                        }
                    }
                });
                let del = ui.small_button("×");
                del.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, format!("remove binding for {device}")));
                if del.clicked() {
                    remove = Some(device.clone());
                }
            });
        }
        if let Some((d, p)) = rebind {
            self.bind_device(&d, &p);
        }
        if let Some(d) = remove {
            self.unbind_device(&d);
        }

        // Profile for everything else.
        let current_default = self.workspace.as_ref().and_then(|w| w.state.default_profile.clone());
        let mut new_default: Option<Option<String>> = None;
        ui.horizontal(|ui| {
            ui.label("Other devices");
            egui::ComboBox::from_id_salt("default_profile")
                .selected_text(current_default.clone().unwrap_or_else(|| "keep the current profile".to_string()))
                .show_ui(ui, |ui| {
                    if ui.selectable_label(current_default.is_none(), "keep the current profile").clicked() {
                        new_default = Some(None);
                    }
                    for n in &names {
                        if ui.selectable_label(current_default.as_deref() == Some(n.as_str()), n).clicked() {
                            new_default = Some(Some(n.clone()));
                        }
                    }
                });
        });
        if let Some(d) = new_default {
            self.set_default_profile(d.as_deref());
        }
        ui.add_space(14.0);
    }
}
