//! Common cancellation-aware MCP transport contract.

use std::{future::Future, pin::Pin};

use omp_core::Str;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::json_rpc::RequestId;

/// Boxed future at the cold dynamic transport boundary.
pub type TransportFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Evidence about whether an operation may have reached the server.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchState {
	/// Failed before any frame or HTTP body was handed to the transport.
	PreDispatch,
	/// The complete request was handed off or accepted by HTTP.
	Dispatched,
	/// Transport failed after handoff without a correlated response.
	EffectsUnknown,
	/// A correlated response was received.
	Responded,
}

/// Successful JSON-RPC request receipt.
#[derive(Debug)]
pub struct TransportResponse {
	/// Request identity assigned by the transport.
	pub id:       RequestId,
	/// JSON-RPC result value.
	pub result:   Value,
	/// Dispatch evidence; successful responses are always `Responded`.
	pub dispatch: DispatchState,
}

/// Server-initiated message delivered independently of request responses.
#[derive(Debug)]
pub enum IncomingMessage {
	/// Notification without an ID.
	Notification { method: Str, params: Value },
	/// Server-to-client request requiring a response.
	Request { id: RequestId, method: Str, params: Value },
	/// Physical connection ended.
	Closed,
}

/// Cancellation-aware transport used by the MCP client/supervisor.
///
/// The boxed future is confined to this dynamic I/O boundary. Concrete stdio
/// and HTTP internals remain allocation-free per poll.
pub trait McpTransport: Send + Sync {
	/// Sends one JSON-RPC request and waits for its correlated response.
	fn request<'a>(
		&'a self,
		method: &'a str,
		params: Value,
		cancellation: CancellationToken,
	) -> TransportFuture<'a, Result<TransportResponse, TransportError>>;

	/// Sends one JSON-RPC notification.
	fn notify<'a>(
		&'a self,
		method: &'a str,
		params: Value,
		cancellation: CancellationToken,
	) -> TransportFuture<'a, Result<DispatchState, TransportError>>;

	/// Receives the next server-initiated request or notification.
	fn next_message<'a>(
		&'a self,
		cancellation: CancellationToken,
	) -> TransportFuture<'a, Result<IncomingMessage, TransportError>>;

	/// Sends a response to a server-initiated request.
	fn respond<'a>(
		&'a self,
		id: RequestId,
		result: Result<Value, ServerResponseError>,
		cancellation: CancellationToken,
	) -> TransportFuture<'a, Result<DispatchState, TransportError>>;

	/// Closes the logical transport and owned resources.
	fn close(&self) -> TransportFuture<'_, Result<(), TransportError>>;
}

/// JSON-RPC error returned to a server request.
#[derive(Clone, Debug)]
pub struct ServerResponseError {
	/// JSON-RPC error code.
	pub code:    i64,
	/// Redaction-safe static or classified message.
	pub message: Str,
	/// Optional structured error data.
	pub data:    Option<Value>,
}

/// Transport failure with retry-accounting evidence.
#[derive(Debug, thiserror::Error)]
#[error("MCP transport failed after dispatch state {dispatch:?}")]
pub struct TransportError {
	/// Best available dispatch evidence.
	pub dispatch: DispatchState,
	/// Typed failure cause.
	#[source]
	pub cause:    TransportFailure,
}

impl TransportError {
	/// Constructs a pre-dispatch failure.
	#[must_use]
	pub const fn pre_dispatch(cause: TransportFailure) -> Self {
		Self { dispatch: DispatchState::PreDispatch, cause }
	}

	/// Constructs a failure after request handoff.
	#[must_use]
	pub const fn effects_unknown(cause: TransportFailure) -> Self {
		Self { dispatch: DispatchState::EffectsUnknown, cause }
	}
}

/// Typed MCP transport failure cause.
#[derive(Debug, thiserror::Error)]
pub enum TransportFailure {
	/// Caller cancellation won the operation race.
	#[error("MCP transport operation was cancelled")]
	Cancelled,
	/// Configured deadline elapsed.
	#[error("MCP transport operation timed out")]
	TimedOut,
	/// Transport is not connected.
	#[error("MCP transport is not connected")]
	NotConnected,
	/// Connection or process stream closed.
	#[error("MCP transport connection closed")]
	Closed,
	/// A frame exceeded its bounded size.
	#[error("MCP transport frame exceeded its size limit")]
	FrameTooLarge,
	/// Child process could not be spawned.
	#[error("MCP stdio child process could not be started")]
	Spawn(#[source] std::io::Error),
	/// Pipe or socket I/O failed.
	#[error("MCP transport I/O failed")]
	Io(#[source] std::io::Error),
	/// HTTP client failed.
	#[error("MCP HTTP transport failed")]
	Http(#[source] wreq::Error),
	/// HTTP response status is not successful.
	#[error("MCP HTTP endpoint returned status {status}")]
	HttpStatus { status: u16 },
	/// Header policy rejected configuration or redirect behavior.
	#[error("MCP HTTP header or redirect policy rejected the request")]
	HeaderPolicy(#[source] super::header_policy::HeaderPolicyError),
	/// JSON frame was malformed.
	#[error("MCP transport received malformed JSON")]
	Json(#[source] serde_json::Error),
	/// JSON-RPC response did not correlate with the request.
	#[error("MCP transport received an uncorrelated JSON-RPC response")]
	Correlation,
	/// Server returned a JSON-RPC error.
	#[error("MCP server returned JSON-RPC error code {code}")]
	JsonRpc { code: i64 },
	/// SSE stream or endpoint event was malformed.
	#[error("MCP server-sent-event stream was malformed")]
	SseProtocol,
}
