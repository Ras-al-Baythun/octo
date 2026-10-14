//! Octo's settings, models and pure logic: song identity, matching, tagging plans, lyrics text.

pub mod charts;
pub mod common;
pub mod config;
pub mod family;
pub mod fingerprint;
pub mod json;
pub mod last_fm;
pub mod library;
pub mod lidarr;
pub mod lyrics;
pub mod metadata;
pub mod models;
pub mod notifications;
pub mod outside;
pub mod radio;
pub mod settings;
pub mod tagging;
pub mod updates;
pub mod validation;

/// The release this build is, as the dashboard and the User-Agent show it (`2026.10.04`).
/// `build.rs` takes it from `OCTO_VERSION` at build time when that is set, else from the
/// repository's `VERSION` file (`CARGO_PKG_VERSION` cannot hold the leading zero).
pub const VERSION: &str = env!("OCTO_RELEASE_VERSION");
