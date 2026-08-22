//! Durable, default-off Mnemopi memory runtime.
//!
//! The crate never discovers Git roots or reads application settings directly.
//! The Environment supplies canonical repository identity and the app composes
//! the selected backend. Durable SQLite rows are authoritative; all recall
//! indexes are rebuildable.

pub mod bank;
pub mod cache;
pub mod config;
pub mod diagnose;
pub mod embedding;
pub mod extract;
pub mod link;
pub mod recall;
pub mod remote;
pub mod retain;
pub mod runtime;
pub mod session;
pub mod store;

pub use bank::{BankId, BankScope, BankScopeInput};
pub use config::{AutolearnSettings, MemoryBackend, MemorySettings, MnemopiSettings};
pub use runtime::{Capabilities, MemoryRuntime, RuntimeRegistry};

/// Mnemopi unavailable status used consistently by every surface.
pub const INACTIVE_MESSAGE: &str = "Memory is off. Set memory.backend = \"mnemopi\" to enable it.";

/// Typed memory failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
	/// Filesystem operation failed.
	#[error(transparent)]
	Io(#[from] std::io::Error),
	/// SQLite operation failed.
	#[error(transparent)]
	Sqlite(#[from] rusqlite::Error),
	/// JSON framing or metadata decoding failed.
	#[error(transparent)]
	Json(#[from] serde_json::Error),
	/// The operation requires a live Mnemopi backend.
	#[error("memory backend is inactive")]
	Inactive,
	/// A requested bank name, memory id, or URL resource was invalid.
	#[error("memory identifier is invalid")]
	InvalidIdentifier,
	/// Input exceeded a documented protocol or projection bound.
	#[error("memory input exceeds its bounded limit")]
	InputTooLarge,
	/// A generation-fenced write raced a newer durable generation.
	#[error("memory index generation changed during rebuild")]
	StaleGeneration,
	/// The embedding worker exited or violated the stdio protocol.
	#[error("embedding worker failed")]
	EmbeddingWorker,
	/// The embedding worker request exceeded its deadline and was hard-reaped.
	#[error("embedding worker request timed out")]
	EmbeddingTimeout,
	/// A configured embedding model is not supported by the worker.
	#[error("embedding model is unsupported")]
	UnsupportedEmbeddingModel,
	/// Recall projection exceeded its explicit byte or token bound.
	#[error("memory projection exceeds its bounded output limit")]
	ProjectionTooLarge,
}

/// Crate-local result alias.
pub type Result<T, E = Error> = std::result::Result<T, E>;
