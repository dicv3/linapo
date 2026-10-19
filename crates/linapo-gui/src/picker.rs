//! Native file dialogs, behind a trait so tests can substitute a fake.
//!
//! The real implementation uses `rfd` with the XDG desktop portal: no GTK or Qt is needed to
//! build, and at runtime it shows the desktop's own file chooser (GNOME, KDE and others provide
//! one). Where no portal is available the dialog simply does not open; the app notices that and
//! points at the paste-a-path box and drag and drop instead.

use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    /// Pick an Equalizer APO profile and add its EQs.
    ImportEqs,
    /// Pick an Equalizer APO profile and replace the whole setup with it.
    ImportReplace,
    OpenSetup,
    SaveSetup,
}

pub trait FilePicker: Send + Sync + 'static {
    /// Show a dialog and wait for it. `None` means cancelled or unavailable.
    fn pick(&self, purpose: Purpose, start_dir: Option<&Path>) -> Option<PathBuf>;
}

pub struct NativePicker;

impl FilePicker for NativePicker {
    fn pick(&self, purpose: Purpose, start_dir: Option<&Path>) -> Option<PathBuf> {
        let mut d = rfd::FileDialog::new();
        if let Some(dir) = start_dir {
            d = d.set_directory(dir);
        }
        match purpose {
            Purpose::ImportEqs | Purpose::ImportReplace => d
                .set_title("Import Equalizer APO profile")
                .add_filter("Equalizer APO profile", &["txt", "cfg", "conf"])
                .pick_file(),
            Purpose::OpenSetup => d.set_title("Open LinAPO setup").add_filter("LinAPO setup", &["json"]).pick_file(),
            Purpose::SaveSetup => d
                .set_title("Save a copy of the setup")
                .set_file_name("setup.json")
                .add_filter("LinAPO setup", &["json"])
                .save_file(),
        }
    }
}

/// Add `.json` when the user typed a bare name in a save dialog.
pub fn with_json_extension(path: PathBuf) -> PathBuf {
    if path.extension().is_none() {
        path.with_extension("json")
    } else {
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_names_get_a_json_extension() {
        assert_eq!(with_json_extension(PathBuf::from("/a/setup")), PathBuf::from("/a/setup.json"));
        assert_eq!(with_json_extension(PathBuf::from("/a/setup.json")), PathBuf::from("/a/setup.json"));
        assert_eq!(with_json_extension(PathBuf::from("/a/setup.txt")), PathBuf::from("/a/setup.txt"));
    }
}
