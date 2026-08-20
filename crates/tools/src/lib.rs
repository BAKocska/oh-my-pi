//! Resource-owning built-in tools for the OMP environment.
//!
//! Executors consume the same streaming invocation contract as extensions:
//! speculative preparation may begin while arguments arrive, while filesystem
//! and process effects remain behind the explicit commitment gate. Durable
//! payloads are revisioned truth and prompt parts are deterministic
//! projections.

/// Shared foreground-wait and managed-job transfer helpers.
pub mod auto_background;

/// Interactive user question picker.
pub mod ask;
/// Durable exploration checkpoint and boundary-rewind tools.
pub mod checkpoint;
/// Workspace-confinement and selector path utilities.
pub mod path;
mod render;

pub use render::{BuiltinRendererIdentities, register_builtin_renderers};

/// Stable dynamic device transport and catalog rendering.
pub mod device;
/// Hashline document transactions with speculative previews.
pub mod edit;
/// Persistent Python evaluation.
pub mod eval;
/// Reader-mode URL fetching through the shared read conversion pipeline.
pub mod fetch;
/// Deterministic workspace path matching.
pub mod glob;
/// Workspace byte and pattern search.
pub mod grep;
/// Peer, detached-job, and named-process coordination.
pub mod hub;
/// Pi-compatible reads across local and special sources.
pub mod read;
/// Persistent-session shell execution.
pub mod shell;
/// Private no-op reasoning scratch notes.
pub mod think;
/// Phased session task tracking.
pub mod todo;
/// Pi-compatible whole-file writes.
pub mod write;
/// Structured subagent result submission.
#[path = "yield.rs"]
pub mod yield_tool;
