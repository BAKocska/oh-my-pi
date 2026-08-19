//! Error taxonomy shared by every engine backend.

use std::path::PathBuf;

use omp_core::Str;

/// Crate-wide result alias.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything that can go wrong while creating or driving a web surface.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
	/// No installed browser satisfies the requested engine/surface combination.
	#[error("no usable browser engine found for `{0}` surface")]
	NoEngine(crate::SurfaceKind),

	/// The engine binary failed to start.
	#[error("failed to launch `{binary}`: {source}")]
	Launch {
		/// Underlying spawn failure.
		source: std::io::Error,
		/// Binary that was being launched.
		binary: PathBuf,
	},

	/// The engine process exited or the automation socket closed.
	#[error("engine connection closed")]
	Closed,

	/// The engine sent traffic the driver could not interpret, or answered a
	/// command with an error.
	#[error("protocol error: {0}")]
	Protocol(Str),

	/// Websocket transport failure while talking to a remote engine.
	#[error("websocket error: {0}")]
	WebSocket(#[from] tokio_tungstenite::tungstenite::Error),

	/// The operation is not supported by this engine/surface combination.
	/// See the capability matrix in the crate docs.
	#[error("unsupported operation: {0}")]
	Unsupported(&'static str),

	/// The engine did not reach the expected state in time.
	#[error("timed out while {0}")]
	Timeout(&'static str),

	/// A system-webview operation was invoked off the main thread.
	#[error("system webview operations require the main thread")]
	MainThread,

	/// The host window handle is not usable on this platform.
	#[error("unsupported window handle for this platform")]
	WindowHandle,

	/// Filesystem or process I/O failure (profile dirs, port files, ...).
	#[error(transparent)]
	Io(#[from] std::io::Error),
}
