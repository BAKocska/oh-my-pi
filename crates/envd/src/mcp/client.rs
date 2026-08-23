//! MCP initialization and server-request handling.

use std::sync::Arc;

use omp_core::Str;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::transport::{IncomingMessage, McpTransport, ServerResponseError, TransportError};

/// Preferred MCP revision and the explicit downgrade set accepted by OMP.
pub const PREFERRED_PROTOCOL_VERSION: &str = "2025-11-25";
/// Known protocol revisions implemented by this client, newest first.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
	&["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// Validated initialize result.
#[derive(Clone, Debug)]
pub struct InitializedServer {
	/// Negotiated exact protocol revision.
	pub protocol_version: Str,
	/// Server implementation name.
	pub name:             Str,
	/// Optional server implementation version.
	pub version:          Option<Str>,
	/// Advertised capabilities retained for feature gating.
	pub capabilities:     Value,
	/// Bounded device documentation supplied by the server.
	pub instructions:     Option<Str>,
}

/// Environment-scoped MCP protocol client.
pub struct McpClient {
	transport: Arc<dyn McpTransport>,
	roots:     Arc<[Str]>,
}

impl McpClient {
	/// Creates a client with a stable snapshot of Environment workspace roots.
	pub fn new(transport: Arc<dyn McpTransport>, roots: Arc<[Str]>) -> Self {
		Self { transport, roots }
	}

	/// Performs initialize, validates the selected revision, then emits
	/// `notifications/initialized` in protocol order.
	pub async fn initialize(
		&self,
		cancel: CancellationToken,
	) -> Result<InitializedServer, ClientError> {
		let response = self
			.transport
			.request(
				"initialize",
				json!({
					"protocolVersion": PREFERRED_PROTOCOL_VERSION,
					"capabilities": {
						"roots": { "listChanged": false },
						"sampling": {},
						"elicitation": {}
					},
					"clientInfo": { "name": "omp", "version": env!("CARGO_PKG_VERSION") }
				}),
				cancel.child_token(),
			)
			.await?;
		let raw: InitializeResult =
			serde_json::from_value(response.result).map_err(|_| ClientError::MalformedInitialize)?;
		if !SUPPORTED_PROTOCOL_VERSIONS.contains(&raw.protocol_version.as_str()) {
			return Err(ClientError::UnsupportedProtocol(Str::from(raw.protocol_version)));
		}
		if raw.server_info.name.trim().is_empty() || !raw.capabilities.is_object() {
			return Err(ClientError::MalformedInitialize);
		}
		let protocol_version = Str::from(raw.protocol_version);
		self
			.transport
			.set_protocol_version(protocol_version.clone());
		self
			.transport
			.notify("notifications/initialized", json!({}), cancel)
			.await?;
		Ok(InitializedServer {
			protocol_version,
			name: Str::from(raw.server_info.name),
			version: raw.server_info.version.map(Str::from),
			capabilities: raw.capabilities,
			instructions: raw
				.instructions
				.filter(|value| !value.is_empty())
				.map(Str::from),
		})
	}

	/// Handles one server-initiated message. Notifications are returned to the
	/// supervisor; requests are answered before returning.
	pub async fn next(
		&self,
		cancel: CancellationToken,
	) -> Result<Option<(Str, Value)>, ClientError> {
		match self.transport.next_message(cancel.child_token()).await? {
			IncomingMessage::Notification { method, params } => Ok(Some((method, params))),
			IncomingMessage::Closed => Ok(None),
			IncomingMessage::Request { id, method, params: _ } => {
				let answer = match method.as_str() {
					"ping" => Ok(json!({})),
					"roots/list" => Ok(json!({
						"roots": self.roots.iter().map(|root| json!({
							"uri": root,
							"name": root
						})).collect::<Vec<_>>()
					})),
					_ => Err(ServerResponseError {
						code:    -32601,
						message: Str::new_static("Method not found"),
						data:    None,
					}),
				};
				self.transport.respond(id, answer, cancel).await?;
				Ok(Some((method, Value::Null)))
			},
		}
	}

	/// Borrows the shared transport for resource, prompt, and tool clients.
	pub fn transport(&self) -> &Arc<dyn McpTransport> {
		&self.transport
	}
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InitializeResult {
	protocol_version: String,
	capabilities:     Value,
	server_info:      ServerInfo,
	instructions:     Option<String>,
}

#[derive(Deserialize)]
struct ServerInfo {
	name:    String,
	version: Option<String>,
}

/// MCP initialization or message-loop failure.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
	/// Transport failed.
	#[error(transparent)]
	Transport(#[from] TransportError),
	/// Initialize response was structurally invalid.
	#[error("MCP initialize response is malformed")]
	MalformedInitialize,
	/// Server selected a revision outside the explicit compatibility set.
	#[error("MCP server selected unsupported protocol revision {0}")]
	UnsupportedProtocol(Str),
}
