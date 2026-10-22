//! LinAPO GUI: a graphical editor for linapo setups.

pub mod app;
pub mod backend;
pub mod model;
pub mod picker;
pub mod plot_math;

#[cfg(test)]
mod ui_tests;

pub use app::{App, Config, EframeApp, ProfileDialog, Tab};
