//! DBSync Studio engine.
//!
//! All domain logic lives here and is deliberately free of any Tauri
//! dependency, so that `engine-cli` has full parity with the GUI. Anything that
//! only works inside the desktop app is a bug in the layering.
//!
//! Layering:
//!   * [`types`], [`profile`] — shared vocabulary.
//!   * [`store`] — SQLite persistence, shared by GUI and CLI.
//!   * [`secrets`] — OS keychain. Never crosses into the webview.
//!   * [`events`], [`job`] — progress reporting and cancellation.
//!   * [`ssh`], [`db`], [`tools`] — connectivity and external binaries.
//!   * [`backup`], [`restore`], [`verify`], [`retention`] — operations.

pub mod backup;
pub mod connect;
pub mod db;
pub mod definer;
pub mod events;
pub mod exec;
pub mod job;
pub mod library;
pub mod manifest;
pub mod ops;
pub mod paths;
pub mod profile;
pub mod restore;
pub mod retention;
pub mod secrets;
pub mod ssh;
pub mod store;
pub mod tools;
pub mod types;
pub mod verify;

pub use profile::ConnectionProfile;
pub use store::Store;
pub use types::{Engine, EnvironmentTag};

/// Version of the engine, surfaced in manifests and the CLI.
pub const ENGINE_VERSION: &str = env!("CARGO_PKG_VERSION");
