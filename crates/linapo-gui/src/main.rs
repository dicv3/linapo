use std::path::PathBuf;

use linapo_core::apo::layout_for;
use linapo_core::Setup;
use linapo_gui::backend::PwBackend;
use linapo_gui::{app, App, Config, EframeApp};

fn default_path() -> PathBuf {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"),
    };
    base.join("linapo").join("setup.json")
}

fn load_or_create(path: &PathBuf) -> (Setup, Option<String>) {
    if !path.exists() {
        return (Setup::passthrough(&layout_for(2)), None);
    }
    match Setup::load(path) {
        Ok(s) => (s, None),
        Err(e) => {
            // Keep the unreadable file instead of overwriting it with the autosave.
            let bad = path.with_extension("json.bad");
            let _ = std::fs::rename(path, &bad);
            (Setup::passthrough(&layout_for(2)), Some(format!("Could not read {} ({e}); moved it to {}", path.display(), bad.display())))
        }
    }
}

fn main() -> eframe::Result {
    let path = std::env::args().nth(1).map(PathBuf::from).unwrap_or_else(default_path);
    let (setup, problem) = load_or_create(&path);
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1120.0, 740.0])
            .with_min_inner_size([860.0, 540.0])
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
            let mut app = App::new(setup, Some(path), PwBackend::default(), move || ctx.request_repaint(), Config::default());
            if let Some(p) = problem {
                app.say(p, true);
            }
            Ok(Box::new(EframeApp(app)))
        }),
    )
}
