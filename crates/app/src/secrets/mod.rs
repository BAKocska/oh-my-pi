//! Process-level secret policy and composition.

/// Global/project rule loading.
pub mod config;
/// Credential-shaped environment collection.
pub mod env;
pub mod key;
/// Immutable per-session snapshot composition.
pub mod session;
