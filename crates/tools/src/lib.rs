//! Resource-owning built-in tools for the OMP environment.
//!
//! Executors consume the same streaming invocation contract as extensions:
//! speculative preparation may begin while arguments arrive, while filesystem
//! and process effects remain behind the explicit commitment gate. Durable
//! payloads are revisioned truth and prompt parts are deterministic
//! projections.

/// Stable identity of one production native tool family.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuiltinToolIdentity {
	/// Model-facing family name.
	pub name:   &'static str,
	/// Whether the family is omitted from ordinary user-facing lists.
	pub hidden: bool,
}

const BUILTIN_TOOL_IDENTITIES: &[BuiltinToolIdentity] = &[
	BuiltinToolIdentity { name: "read", hidden: false },
	BuiltinToolIdentity { name: "fetch", hidden: false },
	BuiltinToolIdentity { name: "web_search", hidden: false },
	BuiltinToolIdentity { name: "edit", hidden: false },
	BuiltinToolIdentity { name: "write", hidden: false },
	BuiltinToolIdentity { name: "grep", hidden: false },
	BuiltinToolIdentity { name: "glob", hidden: false },
	BuiltinToolIdentity { name: "shell", hidden: false },
	BuiltinToolIdentity { name: "eval", hidden: false },
	BuiltinToolIdentity { name: "todo", hidden: false },
	BuiltinToolIdentity { name: "ask", hidden: false },
	BuiltinToolIdentity { name: "hub", hidden: false },
	BuiltinToolIdentity { name: "task", hidden: false },
	BuiltinToolIdentity { name: "lsp", hidden: false },
	BuiltinToolIdentity { name: "checkpoint", hidden: false },
	BuiltinToolIdentity { name: "ast_grep", hidden: false },
	BuiltinToolIdentity { name: "ast_edit", hidden: false },
	BuiltinToolIdentity { name: "rewind", hidden: false },
	BuiltinToolIdentity { name: "think", hidden: true },
	BuiltinToolIdentity { name: "goal", hidden: true },
	BuiltinToolIdentity { name: "yield", hidden: true },
	BuiltinToolIdentity { name: "dyn", hidden: true },
	BuiltinToolIdentity { name: "image_gen", hidden: false },
	BuiltinToolIdentity { name: "tts", hidden: false },
	BuiltinToolIdentity { name: "report_issue", hidden: true },
	BuiltinToolIdentity { name: "vibe", hidden: true },
	BuiltinToolIdentity { name: "learn", hidden: true },
	BuiltinToolIdentity { name: "manage_skill", hidden: true },
	BuiltinToolIdentity { name: "computer", hidden: false },
];

/// Returns the stable native builtin and hidden identity set.
#[must_use]
pub const fn builtin_tool_identities() -> &'static [BuiltinToolIdentity] {
	BUILTIN_TOOL_IDENTITIES
}

/// Shared foreground-wait and managed-job transfer helpers.
pub mod auto_background;

/// Interactive user question picker.
pub mod ask;
/// Structural multi-target rewrites.
pub mod ast_edit;
/// Structural multi-target search.
pub mod ast_grep;
/// Durable exploration checkpoint and boundary-rewind tools.
pub mod checkpoint;
/// Workspace-confinement and selector path utilities.
pub mod path;
mod render;
/// Typed policy projection owned by file tools.
pub mod settings;

pub use render::{
	BuiltinRendererIdentities,
	json_tree::{JsonTreeBounds, JsonTreePreview, preview as preview_json_tree},
	register_builtin_renderers,
};

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
/// Hidden durable goal lifecycle tool.
pub mod goal;
/// Workspace byte and pattern search.
pub mod grep;
/// Peer, detached-job, and named-process coordination.
pub mod hub;
/// Revisioned project language-server tool.
pub mod lsp;
/// Pi-compatible reads across local and special sources.
pub mod read;
/// Persistent-session shell execution.
pub mod shell;
/// Pre-authorization guidance for shell intents served by dedicated tools.
pub mod shell_intercept;
/// Internal-resource URI scanner used before environment execution.
pub mod shell_uri;
/// Private no-op reasoning scratch notes.
pub mod think;
/// Phased session task tracking.
pub mod todo;
/// Canonical provider-routed web search.
pub mod web_search;
/// Pi-compatible whole-file writes.
pub mod write;
/// Structured subagent result submission.
#[path = "yield.rs"]
pub mod yield_tool;
