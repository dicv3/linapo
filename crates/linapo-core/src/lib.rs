//! linapo-core: DSP math and Equalizer APO profile handling.
//!
//! No audio-backend dependencies, so everything here is unit-testable offline.

pub mod apo;
pub mod biquad;
pub mod chain;
pub mod engine;
pub mod filter;
pub mod graphic_eq;
pub mod handoff;
pub mod matrix;
pub mod pipewire_conf;
pub mod setup;

pub use apo::{layout_for, parse_file, parse_str, Channel, Profile};
pub use biquad::{Biquad, Coeffs};
pub use chain::{response_db, ChannelChain, EqChain};
pub use filter::{Filter, FilterKind};
pub use engine::Engine;
pub use handoff::Handoff;
pub use matrix::Matrix;
pub use setup::{EqPreset, Setup};
