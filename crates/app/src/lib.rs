#![recursion_limit = "256"]

//! The production Codex route intentionally keeps its concrete Tower type;
//! this recursion limit covers compiler trait normalization without boxing the
//! runtime transport path.
//! Production application composition and command dispatch.

pub mod acp_mode;
pub mod audio_coordinator;
pub mod auth_backend;
pub mod auth_rpc;
pub mod blob_rpc;
pub mod build_id;
pub mod chat;
mod chat_ui;
pub mod cli;
pub mod complete_cmd;
pub mod completions;
pub mod config_cmd;
pub mod cursor_bridge;
pub mod daemon;
pub mod discovery;
pub mod editor;
pub mod endpoint;
pub mod envd;
pub mod ext;
pub mod ext_cli;
pub mod exthost;
pub mod goal;
pub mod headless;
pub mod help_extra;
pub mod image_attachment;
pub mod keybindings;
pub mod memory;
pub mod model_controls;
pub mod models_cmd;
pub mod modes;
mod open;
pub(crate) mod pickers;
pub mod plan;
pub mod power;
pub mod print_mode;
pub mod profile_alias;
pub mod progress_reporter;
pub mod project_state;
pub mod prompt_prep;
pub mod rpc_adapter;
pub mod rpc_mode;
pub mod rulebook;
pub mod rules;
/// Process-level secret key and masking composition.
pub mod secrets;
pub mod session_import;
pub mod session_manager;
pub mod settings;
/// Irreversible share-snapshot leakage boundary.
pub mod share;
pub mod skills;
pub mod smoke_test;
pub mod spec;
pub mod startup_notice;
pub mod subagent;
pub mod task;
pub mod theme_watcher;
pub mod usage_error;
pub mod vibe;
pub mod voice;
pub mod wizard;
pub mod workspace_roots;
pub mod worktree_cmd;

pub use miette::{IntoDiagnostic, Report, Result};

/// Parses process arguments and runs the selected production operation.
#[expect(
	clippy::future_not_send,
	reason = "the chat command runs a thread-confined terminal UI future"
)]
pub async fn run() -> Result<()> {
	let cli = match cli::parse_from_os(std::env::args_os()) {
		Ok(cli) => cli,
		Err(error) => error.exit(),
	};
	cli::dispatch(cli).await
}
