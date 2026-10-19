//! Drives the real UI with simulated clicks (egui_kittest), against a fake backend.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;
use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;
use linapo_core::apo::layout_for;
use linapo_core::{Channel, Setup};

use crate::app::{App, Config, Tab};
use crate::backend::fake::Fake;
use crate::backend::{Applied, Status};
use crate::picker::{FilePicker, Purpose};

fn ch(s: &str) -> Channel {
    Channel::parse(s).unwrap()
}

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("linapo-gui-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn fast() -> Config {
    Config { debounce_apply: Duration::ZERO, debounce_save: Duration::ZERO, history_gap: Duration::ZERO }
}

fn harness(setup: Setup, path: Option<PathBuf>, fake: &Fake) -> Harness<'static, App> {
    let app = App::new(setup, path, fake.clone(), || {}, fast());
    Harness::builder().with_size(egui::vec2(1100.0, 720.0)).build_state(|ctx, app: &mut App| app.ui(ctx), app)
}

/// Let the worker thread answer and the UI pick it up.
fn settle(h: &mut Harness<'static, App>) {
    for _ in 0..6 {
        std::thread::sleep(Duration::from_millis(40));
        h.run();
    }
}

fn stereo() -> Setup {
    Setup::passthrough(&layout_for(2))
}

#[test]
fn shows_live_status_and_the_matrix() {
    let fake = Fake::new();
    let mut h = harness(stereo(), None, &fake);
    settle(&mut h);
    assert_eq!(h.state().status, Some(Status::Live));
    h.get_by_label("Live");
    h.get_by_label("route L to L: 0.0 dB");
    h.get_by_label("route L to R: off");
    h.get_by_label("route R to R: 0.0 dB");
}

#[test]
fn clicking_a_cell_toggles_the_route_and_applies_it() {
    let fake = Fake::new();
    let mut h = harness(stereo(), None, &fake);
    settle(&mut h);
    h.get_by_label("route L to R: off").click();
    settle(&mut h);
    assert_eq!(h.state().setup.matrix.get(0, 1), 1.0);
    h.get_by_label("route L to R: 0.0 dB");
    assert!(fake.calls().iter().any(|c| c.starts_with("apply")), "{:?}", fake.calls());
    h.get_by_label("route L to R: 0.0 dB").click();
    settle(&mut h);
    assert_eq!(h.state().setup.matrix.get(0, 1), 0.0);
}

#[test]
fn edits_are_autosaved_and_reloadable() {
    let dir = tmpdir("save");
    let path = dir.join("s.json");
    let fake = Fake::new();
    let mut h = harness(stereo(), Some(path.clone()), &fake);
    settle(&mut h);
    h.get_by_label("route R to L: off").click();
    settle(&mut h);
    assert!(h.state().saved);
    let loaded = Setup::load(&path).unwrap();
    assert_eq!(loaded.matrix.get(1, 0), 1.0);
    assert_eq!(loaded, h.state().setup);
}

#[test]
fn master_gain_edits_are_pushed_live() {
    let fake = Fake::new();
    let mut h = harness(stereo(), None, &fake);
    settle(&mut h);
    h.state_mut().setup.master_gain_db = -6.0;
    h.state_mut().touch();
    settle(&mut h);
    assert!(fake.calls().iter().any(|c| c == "apply master=-6"), "{:?}", fake.calls());
}

#[test]
fn not_installed_banner_offers_install() {
    let fake = Fake::new();
    *fake.status_result.lock().unwrap() = Ok(Status::NotInstalled);
    let mut h = harness(stereo(), None, &fake);
    settle(&mut h);
    h.get_by_label("LinAPO is not installed in PipeWire yet.");
    *fake.status_result.lock().unwrap() = Ok(Status::Live);
    h.get_by_label("Install").click();
    settle(&mut h);
    assert!(fake.calls().iter().any(|c| c == "install"));
    assert_eq!(h.state().status, Some(Status::Live));
    assert!(h.query_by_label("Install").is_none(), "banner disappears once live");
}

#[test]
fn a_structural_change_shows_the_restart_banner() {
    let fake = Fake::new();
    let mut h = harness(stereo(), None, &fake);
    settle(&mut h);
    *fake.apply_result.lock().unwrap() = Ok(Applied::NeedsRestart);
    h.state_mut().touch();
    settle(&mut h);
    assert_eq!(h.state().status, Some(Status::NeedsRestart));
    h.get_by_label("Restart needed");
    *fake.apply_result.lock().unwrap() = Ok(Applied::Live { values: 3 });
    h.get_by_label("Apply and restart audio").click();
    settle(&mut h);
    assert!(fake.calls().iter().any(|c| c == "install"));
}

#[test]
fn equalizer_tab_create_edit_and_delete() {
    let fake = Fake::new();
    let mut h = harness(stereo(), None, &fake);
    settle(&mut h);
    h.get_by_label("Equalizer").click();
    h.run();
    assert_eq!(h.state().tab, Tab::Equalizer);
    h.get_by_label("No equalizers yet");
    h.get_by_label("Create an equalizer").click();
    h.run();
    assert_eq!(h.state().setup.eqs.len(), 1);
    assert_eq!(h.state().setup.eqs[0].channels, layout_for(2));

    h.get_by_label("Add filter").click();
    h.run();
    h.get_by_label("Add filter").click();
    h.run();
    assert_eq!(h.state().setup.eqs[0].filters.len(), 2);
    assert_ne!(h.state().setup.eqs[0].filters[0].fc_hz, h.state().setup.eqs[0].filters[1].fc_hz);

    h.get_by_label("delete filter 1").click();
    h.run();
    assert_eq!(h.state().setup.eqs[0].filters.len(), 1);
    h.state().setup.validate().unwrap();
}

#[test]
fn eq_channel_assignment_chips() {
    let fake = Fake::new();
    let mut s = Setup::passthrough(&layout_for(6));
    linapo_gui_model_add_eq(&mut s);
    let mut h = harness(s, None, &fake);
    settle(&mut h);
    h.get_by_label("Equalizer").click();
    h.run();
    assert!(h.state().setup.eqs[0].channels.contains(&ch("SUB")));
    h.get_by_label("SUB").click();
    h.run();
    assert!(!h.state().setup.eqs[0].channels.contains(&ch("SUB")), "chip removes the channel");
    h.get_by_label("None").click();
    h.run();
    assert!(h.state().setup.eqs[0].channels.is_empty());
    h.get_by_label("Warning: this equalizer is not assigned to any output channel, so it has no effect.");
    h.get_by_label("All").click();
    h.run();
    assert_eq!(h.state().setup.eqs[0].channels.len(), 6);
}

fn linapo_gui_model_add_eq(s: &mut Setup) {
    crate::model::add_eq(s);
}

#[test]
fn eq_list_selection_and_enable_checkbox() {
    let fake = Fake::new();
    let mut s = stereo();
    crate::model::add_eq(&mut s);
    crate::model::add_eq(&mut s);
    s.eqs[1].name = "Headphones".into();
    let mut h = harness(s, None, &fake);
    settle(&mut h);
    h.get_by_label("Equalizer").click();
    h.run();
    h.get_by_label("Headphones").click();
    h.run();
    assert_eq!(h.state().selected_eq, 1);
    h.get_by_label("enable Headphones").click();
    h.run();
    assert!(!h.state().setup.eqs[1].enabled);
    h.get_by_label("Duplicate").click();
    h.run();
    assert_eq!(h.state().setup.eqs.len(), 3);
    h.get_by_label("Delete").click();
    h.run();
    assert_eq!(h.state().setup.eqs.len(), 2);
}

#[test]
fn matrix_presets() {
    let fake = Fake::new();
    let mut s = Setup::passthrough(&layout_for(6));
    crate::model::set_output_layout(&mut s, layout_for(2));
    let mut h = harness(s, None, &fake);
    settle(&mut h);
    h.get_by_label("Downmix to stereo").click();
    h.run();
    assert!((h.state().setup.matrix.get(2, 0) - 0.7071).abs() < 1e-3, "centre goes to L at -3 dB");
    h.get_by_label("route C to L: -3.0 dB");
    h.get_by_label("Clear all").click();
    h.run();
    assert!(h.state().setup.matrix.as_slice().iter().all(|g| *g == 0.0));
    h.get_by_label("Straight through").click();
    h.run();
    assert_eq!(h.state().setup.matrix.get(1, 1), 1.0);
}

#[test]
fn eq_badge_in_the_matrix_jumps_to_that_outputs_equalizer() {
    let fake = Fake::new();
    let mut s = stereo();
    crate::model::add_eq(&mut s);
    crate::model::add_eq(&mut s);
    s.eqs[0].channels = vec![ch("L")];
    s.eqs[1].channels = vec![ch("R")];
    let mut h = harness(s, None, &fake);
    settle(&mut h);
    h.get_by_label("equalizer for output R").click();
    h.run();
    assert_eq!(h.state().tab, Tab::Equalizer);
    assert_eq!(h.state().selected_eq, 1);
    assert_eq!(h.state().monitor, 1);
}

#[test]
fn setup_tab_changes_the_output_layout_and_keeps_the_setup_valid() {
    let fake = Fake::new();
    let mut h = harness(stereo(), None, &fake);
    settle(&mut h);
    h.get_by_label("Setup").click();
    h.run();
    let combos = h.get_all_by_role(egui::accesskit::Role::ComboBox).collect::<Vec<_>>();
    assert!(combos.len() >= 3, "input layout, output layout and device selectors, got {}", combos.len());
    combos[1].click();
    h.run();
    h.get_by_label("5.1").click();
    h.run();
    assert_eq!(h.state().setup.output_channels, layout_for(6));
    assert_eq!(h.state().setup.input_channels, layout_for(2));
    h.state().setup.validate().unwrap();
    h.get_by_label("Matrix").click();
    h.run();
    h.get_by_label("route L to SUB: off");
}

#[test]
fn apo_profiles_can_be_imported_and_dropped() {
    let dir = tmpdir("apo");
    let p = dir.join("cans.txt");
    std::fs::write(&p, "Preamp: -3 dB\nFilter: ON PK Fc 1000 Hz Gain 3 dB Q 1\nChannel: L\nFilter: ON PK Fc 100 Hz Gain 2 dB Q 1\n").unwrap();
    let fake = Fake::new();
    let mut h = harness(stereo(), None, &fake);
    settle(&mut h);
    h.state_mut().import_apo(&p, false);
    h.run();
    assert_eq!(h.state().setup.eqs.len(), 2);
    assert_eq!(h.state().tab, Tab::Equalizer);
    h.get_by_label("cans (all)");
    h.get_by_label("cans (L)");
    assert!(h.state().message_text().unwrap().contains("Imported 2 EQ(s)"));

    // dropping a file onto the window does the same
    let mut h = harness(stereo(), None, &fake);
    settle(&mut h);
    h.input_mut().dropped_files.push(egui::DroppedFile { path: Some(p.clone()), ..Default::default() });
    h.run();
    assert_eq!(h.state().setup.eqs.len(), 2);

    // a missing file is reported, not fatal
    h.state_mut().import_apo(&dir.join("nope.txt"), false);
    assert!(h.state().message_text().unwrap().contains("Could not read"));
    assert_eq!(h.state().setup.eqs.len(), 2);
}

#[test]
fn replace_everything_turns_copy_lines_into_the_matrix() {
    let dir = tmpdir("replace");
    let p = dir.join("mix.txt");
    std::fs::write(&p, "Copy: L=0.5*L+0.5*R R=0.5*L+0.5*R\n").unwrap();
    let fake = Fake::new();
    let mut s = stereo();
    s.master_gain_db = -4.0;
    s.output_target = Some("dev".into());
    let mut h = harness(s, None, &fake);
    settle(&mut h);
    h.state_mut().import_apo(&p, true);
    assert_eq!(h.state().setup.matrix.get(0, 1), 0.5);
    assert_eq!(h.state().setup.master_gain_db, -4.0, "master gain survives");
    assert_eq!(h.state().setup.output_target.as_deref(), Some("dev"), "device survives");
}

#[test]
fn loading_a_bad_setup_file_reports_an_error_and_keeps_the_current_one() {
    let dir = tmpdir("bad");
    let bad = dir.join("bad.json");
    std::fs::write(&bad, "{ nope").unwrap();
    let fake = Fake::new();
    let mut h = harness(stereo(), None, &fake);
    settle(&mut h);
    h.state_mut().load_file(&bad);
    assert!(h.state().message_text().unwrap().starts_with("Could not load"));
    assert_eq!(h.state().setup, stereo());
}

#[test]
fn every_tab_renders_for_every_standard_layout_without_panicking() {
    let fake = Fake::new();
    for n in [1, 2, 4, 6, 8] {
        let mut s = Setup::passthrough(&layout_for(n));
        crate::model::add_eq(&mut s);
        crate::model::add_filter(&mut s.eqs[0]);
        crate::model::add_filter(&mut s.eqs[0]);
        s.eqs[0].filters[1].kind = linapo_core::FilterKind::LowPass;
        let mut h = harness(s, None, &fake);
        settle(&mut h);
        for tab in ["Equalizer", "Setup", "Matrix"] {
            h.get_by_label(tab).click();
            h.run();
        }
    }
}

#[test]
fn per_filter_enable_checkbox_switches_a_filter_off_live() {
    let fake = Fake::new();
    let mut s = stereo();
    crate::model::add_eq(&mut s);
    crate::model::add_filter(&mut s.eqs[0]);
    s.eqs[0].filters[0].gain_db = 6.0;
    let mut h = harness(s, None, &fake);
    settle(&mut h);
    h.get_by_label("Equalizer").click();
    h.run();
    let before = fake.calls().len();
    h.get_by_label("enable filter 1").click();
    settle(&mut h);
    assert!(!h.state().setup.eqs[0].filters[0].enabled);
    assert_eq!(h.state().setup.eqs[0].filters[0].gain_db, 6.0, "its settings are kept");
    assert!(fake.calls().len() > before, "the change was pushed");
    let chain = h.state().setup.chain_for_output(&ch("L"));
    assert!(linapo_core::response_db(&chain, 48_000.0, 1000.0).abs() < 1e-9, "a disabled filter has no effect");
    h.get_by_label("enable filter 1").click();
    settle(&mut h);
    assert!(h.state().setup.eqs[0].filters[0].enabled);
}

#[test]
fn bypass_button_toggles_the_whole_eq_without_changing_structure() {
    let fake = Fake::new();
    let mut s = stereo();
    crate::model::add_eq(&mut s);
    crate::model::add_filter(&mut s.eqs[0]);
    let mut h = harness(s, None, &fake);
    settle(&mut h);
    assert!(!h.state().setup.bypass_eq);
    h.get_by_label("Bypass EQ").click();
    settle(&mut h);
    assert!(h.state().setup.bypass_eq);
    assert_eq!(h.state().setup.eqs[0].filters.len(), 1, "filters are kept");
    assert!(fake.calls().iter().any(|c| c.starts_with("apply")));
    h.get_by_label("Bypass EQ").click();
    settle(&mut h);
    assert!(!h.state().setup.bypass_eq);
}

#[test]
fn undo_and_redo_buttons_walk_through_edits() {
    let fake = Fake::new();
    let mut h = harness(stereo(), None, &fake);
    settle(&mut h);
    assert!(!h.state().can_undo());
    h.get_by_label("route L to R: off").click();
    settle(&mut h);
    h.get_by_label("route R to L: off").click();
    settle(&mut h);
    assert_eq!((h.state().setup.matrix.get(0, 1), h.state().setup.matrix.get(1, 0)), (1.0, 1.0));
    h.get_by_label("Undo").click();
    settle(&mut h);
    assert_eq!((h.state().setup.matrix.get(0, 1), h.state().setup.matrix.get(1, 0)), (1.0, 0.0));
    h.get_by_label("Undo").click();
    settle(&mut h);
    assert_eq!(h.state().setup, stereo());
    assert!(!h.state().can_undo());
    h.get_by_label("Redo").click();
    settle(&mut h);
    assert_eq!(h.state().setup.matrix.get(0, 1), 1.0);
    // a new edit discards the redo branch
    h.get_by_label("route L to L: 0.0 dB").click();
    settle(&mut h);
    assert!(!h.state().can_redo());
    // undo/redo are also pushed to PipeWire and saved
    assert!(fake.calls().iter().filter(|c| c.starts_with("apply")).count() >= 4);
}

#[test]
fn keyboard_shortcuts_undo_and_redo() {
    let fake = Fake::new();
    let mut h = harness(stereo(), None, &fake);
    settle(&mut h);
    h.get_by_label("route L to R: off").click();
    settle(&mut h);
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Z);
    settle(&mut h);
    assert_eq!(h.state().setup, stereo(), "Ctrl+Z");
    h.key_press_modifiers(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, egui::Key::Z);
    settle(&mut h);
    assert_eq!(h.state().setup.matrix.get(0, 1), 1.0, "Ctrl+Shift+Z");
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Z);
    settle(&mut h);
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Y);
    settle(&mut h);
    assert_eq!(h.state().setup.matrix.get(0, 1), 1.0, "Ctrl+Y");
}

#[test]
fn a_continuous_gesture_is_a_single_undo_step() {
    let fake = Fake::new();
    let cfg = Config { history_gap: Duration::from_secs(60), ..fast() };
    let mut app = App::new(stereo(), None, fake.clone(), || {}, cfg);
    for i in 1..=30 {
        app.setup.master_gain_db = -(i as f64);
        app.touch(); // like 30 frames of dragging the slider
    }
    app.commit_history();
    assert_eq!(app.setup.master_gain_db, -30.0);
    app.undo();
    assert_eq!(app.setup.master_gain_db, 0.0, "one undo undoes the whole drag");
    assert!(!app.can_undo());
    app.redo();
    assert_eq!(app.setup.master_gain_db, -30.0);
}

#[test]
fn undo_after_structural_edits_keeps_selection_valid() {
    let fake = Fake::new();
    let mut h = harness(stereo(), None, &fake);
    settle(&mut h);
    h.get_by_label("Equalizer").click();
    h.run();
    h.get_by_label("Create an equalizer").click();
    settle(&mut h);
    assert_eq!(h.state().setup.eqs.len(), 1);
    h.get_by_label("Undo").click();
    settle(&mut h);
    assert!(h.state().setup.eqs.is_empty());
    h.run(); // must not panic with selected_eq pointing at a removed EQ
    h.get_by_label("No equalizers yet");
}

#[test]
fn imported_off_filters_show_up_disabled() {
    let dir = tmpdir("off");
    let p = dir.join("p.txt");
    std::fs::write(&p, "Filter 1: OFF PK Fc 1000 Hz Gain 3 dB Q 1\nFilter 2: ON PK Fc 2000 Hz Gain 3 dB Q 1\n").unwrap();
    let fake = Fake::new();
    let mut h = harness(stereo(), None, &fake);
    settle(&mut h);
    h.state_mut().import_apo(&p, false);
    h.run();
    let f = &h.state().setup.eqs[0].filters;
    assert_eq!((f.len(), f[0].enabled, f[1].enabled), (2, false, true));
    h.get_by_label("enable filter 1");
}

#[test]
fn a_graphic_eq_profile_imports_as_a_manageable_filter_bank() {
    let dir = tmpdir("geq");
    let p = dir.join("geq.txt");
    let line: String = (0..127)
        .map(|i| {
            let hz = 20.0 * 1000f64.powf(i as f64 / 126.0);
            format!("{hz:.1} {:.2}", -4.0 + 6.0 * (-((hz / 3000.0).log2() / 0.5).powi(2)).exp())
        })
        .collect::<Vec<_>>()
        .join("; ");
    std::fs::write(&p, format!("GraphicEQ: {line}\n")).unwrap();
    let fake = Fake::new();
    let mut h = harness(stereo(), None, &fake);
    settle(&mut h);
    h.state_mut().import_apo(&p, false);
    settle(&mut h);
    let n = h.state().setup.eqs[0].filters.len();
    assert!((5..=60).contains(&n), "{n} filters");
    assert!(h.state().message_text().unwrap().contains("converted to"), "{:?}", h.state().message_text());
    h.state().setup.validate().unwrap();
    // the editor and the plot render with that many rows, and a row can be selected
    h.get_by_label("2").click();
    h.run();
    assert_eq!(h.state().selected_filter, Some(1));
    // undo removes the whole imported EQ in one step
    h.get_by_label("Undo").click();
    settle(&mut h);
    assert!(h.state().setup.eqs.is_empty());
}

// ---------------------------------------------------------------- file dialogs

struct FakePicker {
    answer: Mutex<Option<PathBuf>>,
    delay: Duration,
    calls: Mutex<Vec<(Purpose, Option<PathBuf>)>>,
}

impl FakePicker {
    fn new(answer: Option<PathBuf>, delay: Duration) -> Arc<FakePicker> {
        Arc::new(FakePicker { answer: Mutex::new(answer), delay, calls: Default::default() })
    }
}

impl FilePicker for FakePicker {
    fn pick(&self, purpose: Purpose, start_dir: Option<&Path>) -> Option<PathBuf> {
        std::thread::sleep(self.delay);
        self.calls.lock().unwrap().push((purpose, start_dir.map(Path::to_path_buf)));
        self.answer.lock().unwrap().clone()
    }
}

fn harness_with_picker(setup: Setup, path: Option<PathBuf>, fake: &Fake, picker: Arc<FakePicker>) -> Harness<'static, App> {
    let app = App::new(setup, path, fake.clone(), || {}, fast()).with_picker(picker);
    Harness::builder().with_size(egui::vec2(1100.0, 720.0)).build_state(|ctx, app: &mut App| app.ui(ctx), app)
}

#[test]
fn browse_in_the_equalizer_tab_imports_the_chosen_profile() {
    let dir = tmpdir("browse");
    let p = dir.join("cans.txt");
    std::fs::write(&p, "Preamp: -2 dB\nFilter: ON PK Fc 1000 Hz Gain 3 dB Q 1\n").unwrap();
    let picker = FakePicker::new(Some(p.clone()), Duration::from_millis(5));
    let fake = Fake::new();
    let mut h = harness_with_picker(stereo(), None, &fake, picker.clone());
    settle(&mut h);
    h.get_by_label("Equalizer").click();
    h.run();
    h.get_by_label("Create an equalizer").click();
    h.run();
    h.get_by_label("Browse\u{2026}").click();
    settle(&mut h);
    assert_eq!(picker.calls.lock().unwrap()[0].0, Purpose::ImportEqs);
    assert_eq!(h.state().setup.eqs.len(), 2, "the existing EQ plus the imported one");
    assert_eq!(h.state().import_path, p.display().to_string());
    assert!(h.state().message_text().unwrap().contains("Imported 1 EQ(s)"));
    assert!(!h.state().dialog_open);
}

#[test]
fn setup_tab_can_open_save_a_copy_and_replace_via_dialogs() {
    let dir = tmpdir("dialogs");
    let apo = dir.join("mix.txt");
    std::fs::write(&apo, "Copy: L=0.5*L+0.5*R R=0.5*L+0.5*R\n").unwrap();
    let other = {
        let mut s = stereo();
        s.master_gain_db = -9.0;
        let p = dir.join("other.json");
        s.save(&p).unwrap();
        p
    };
    let picker = FakePicker::new(Some(other.clone()), Duration::from_millis(5));
    let fake = Fake::new();
    let own = dir.join("own.json");
    let mut h = harness_with_picker(stereo(), Some(own.clone()), &fake, picker.clone());
    settle(&mut h);
    h.get_by_label("Setup").click();
    h.run();

    // Open...
    h.get_by_label("Open\u{2026}").click();
    settle(&mut h);
    assert_eq!(h.state().setup.master_gain_db, -9.0);
    assert_eq!(h.state().path.as_deref(), Some(other.as_path()));

    // Save a copy... (a bare name gets .json)
    *picker.answer.lock().unwrap() = Some(dir.join("copy"));
    h.get_by_label("Save a copy\u{2026}").click();
    settle(&mut h);
    let copy = dir.join("copy.json");
    assert_eq!(Setup::load(&copy).unwrap().master_gain_db, -9.0);
    assert_eq!(h.state().path.as_deref(), Some(copy.as_path()));

    // Browse and replace...
    *picker.answer.lock().unwrap() = Some(apo.clone());
    h.get_by_label("Browse and replace\u{2026}").click();
    settle(&mut h);
    assert_eq!(h.state().setup.matrix.get(0, 1), 0.5);
    assert_eq!(h.state().setup.master_gain_db, -9.0, "master gain survives a replace");
}

#[test]
fn dialogs_remember_the_last_folder() {
    let dir = tmpdir("lastdir");
    let p = dir.join("a.txt");
    std::fs::write(&p, "Preamp: -1 dB\n").unwrap();
    let picker = FakePicker::new(Some(p.clone()), Duration::from_millis(5));
    let fake = Fake::new();
    let mut h = harness_with_picker(stereo(), None, &fake, picker.clone());
    settle(&mut h);
    h.get_by_label("Setup").click();
    h.run();
    h.get_by_label("Browse\u{2026}").click();
    settle(&mut h);
    h.get_by_label("Browse\u{2026}").click();
    settle(&mut h);
    let calls = picker.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].1, None);
    assert_eq!(calls[1].1.as_deref(), Some(dir.as_path()));
}

#[test]
fn cancelling_gets_a_gentle_note_and_a_dialog_that_cannot_open_gets_an_error() {
    let fake = Fake::new();
    // A human cancelling takes a moment.
    let cancel = FakePicker::new(None, Duration::from_millis(400));
    let mut h = harness_with_picker(stereo(), None, &fake, cancel);
    settle(&mut h);
    h.get_by_label("Setup").click();
    h.run();
    h.get_by_label("Open\u{2026}").click();
    h.run(); // the click is handled in this frame, which starts the dialog
    assert!(h.state().dialog_open);
    std::thread::sleep(Duration::from_millis(600));
    settle(&mut h);
    assert!(h.state().message_text().unwrap().contains("No file chosen"));
    assert!(!h.state().message_is_error(), "a cancel is not an error");
    assert!(!h.state().dialog_open);
    assert_eq!(h.state().setup, stereo(), "nothing changed");

    // No portal: returns at once.
    let broken = FakePicker::new(None, Duration::ZERO);
    let mut h = harness_with_picker(stereo(), None, &fake, broken);
    settle(&mut h);
    h.get_by_label("Setup").click();
    h.run();
    h.get_by_label("Open\u{2026}").click();
    settle(&mut h);
    assert!(h.state().message_text().unwrap().contains("No file dialog could be opened"));
    assert!(h.state().message_is_error());
}

#[test]
fn only_one_dialog_at_a_time() {
    let picker = FakePicker::new(None, Duration::from_millis(500));
    let fake = Fake::new();
    let mut h = harness_with_picker(stereo(), None, &fake, picker.clone());
    settle(&mut h);
    h.state_mut().browse(Purpose::OpenSetup);
    h.state_mut().browse(Purpose::OpenSetup);
    h.state_mut().browse(Purpose::SaveSetup);
    assert!(h.state().dialog_open);
    std::thread::sleep(Duration::from_millis(900));
    settle(&mut h);
    assert_eq!(picker.calls.lock().unwrap().len(), 1);
}

#[test]
fn delete_key_removes_the_selected_filter() {
    let fake = Fake::new();
    let mut s = stereo();
    crate::model::add_eq(&mut s);
    for _ in 0..3 {
        crate::model::add_filter(&mut s.eqs[0]);
    }
    let second = s.eqs[0].filters[1];
    let mut h = harness(s, None, &fake);
    settle(&mut h);
    h.get_by_label("Equalizer").click();
    h.run();
    h.key_press(egui::Key::Delete);
    settle(&mut h);
    assert_eq!(h.state().setup.eqs[0].filters.len(), 3, "nothing selected, nothing deleted");
    h.get_by_label("1").click();
    h.run();
    h.key_press(egui::Key::Delete);
    settle(&mut h);
    assert_eq!(h.state().setup.eqs[0].filters.len(), 2);
    assert_eq!(h.state().setup.eqs[0].filters[0], second, "the first filter was the one removed");
    assert_eq!(h.state().selected_filter, None);
    h.get_by_label("Undo").click();
    settle(&mut h);
    assert_eq!(h.state().setup.eqs[0].filters.len(), 3, "and it is undoable");
}
