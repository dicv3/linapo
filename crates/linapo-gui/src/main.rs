use std::path::PathBuf;

use linapo_core::apo::layout_for;
use linapo_core::profiles::{ProfileStore, Workspace};
use linapo_core::Setup;
use linapo_gui::backend::PwBackend;
use linapo_gui::{app, App, Config, EframeApp};

/// Startup state: a note to show, and how the app edits things.
enum Mode {
    /// `linapo-gui some/setup.json`: edit that one file, no profiles.
    SingleFile(PathBuf, Setup),
    /// The default: named profiles under `~/.config/linapo`.
    Profiles(Workspace, Setup),
}

fn open(arg: Option<PathBuf>) -> Result<(Mode, Option<String>), String> {
    if let Some(path) = arg {
        if !path.exists() {
            return Ok((Mode::SingleFile(path, Setup::passthrough(&layout_for(2))), None));
        }
        return match Setup::load(&path) {
            Ok(s) => Ok((Mode::SingleFile(path, s), None)),
            Err(e) => {
                // Keep the unreadable file instead of overwriting it with the autosave.
                let bad = path.with_extension("json.bad");
                let _ = std::fs::rename(&path, &bad);
                let note = format!("Could not read {} ({e}); moved it to {}", path.display(), bad.display());
                Ok((Mode::SingleFile(path, Setup::passthrough(&layout_for(2))), Some(note)))
            }
        };
    }
    let root = linapo_pw::config_root().map_err(|e| e.to_string())?;
    let ws = Workspace::open(&root).map_err(|e| format!("cannot open {}: {e}", root.display()))?;
    match ws.load_active() {
        Ok(s) => Ok((Mode::Profiles(ws, s), None)),
        Err(e) => {
            let note = format!("Could not read profile '{}': {e}", ws.active_name());
            Ok((Mode::Profiles(ws, Setup::passthrough(&layout_for(2))), Some(note)))
        }
    }
}

fn main() -> eframe::Result {
    let (mode, problem) = match open(std::env::args().nth(1).map(PathBuf::from)) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("linapo-gui: {e}");
            std::process::exit(1);
        }
    };
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1120.0, 780.0])
            .with_min_inner_size([900.0, 560.0])
            .with_title("LinAPO")
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "LinAPO",
        options,
        Box::new(move |cc| {
            app::apply_style(&cc.egui_ctx);
            let ctx = cc.egui_ctx.clone();
            let wake = move || ctx.request_repaint();
            let mut app = match mode {
                Mode::SingleFile(path, setup) => App::new(setup, Some(path), PwBackend::default(), wake, Config::default()),
                Mode::Profiles(ws, setup) => {
                    let backend = PwBackend { store: Some(ProfileStore::new(ws.store.dir())), ..PwBackend::default() };
                    App::new(setup, None, backend, wake, Config::default()).with_workspace(ws)
                }
            };
            if let Some(p) = problem {
                app.say(p, true);
            }
            Ok(Box::new(EframeApp(app)))
        }),
    )
}
