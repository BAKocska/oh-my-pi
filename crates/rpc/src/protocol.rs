//! Serde surfaces shared by the stdio RPC server and embedding clients.
//!
//! The protocol intentionally leaves application-owned model, message, and
//! event payloads as [`serde_json::Value`]. This keeps transport consumers from
//! depending on the application crate while preserving typed envelopes and
//! control frames.

use std::{collections::BTreeMap, fmt};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
fn is_false(value: &bool) -> bool {
	!*value
}

/// The baseline, single-frame protocol version.
pub const PROTOCOL_V1: u8 = 1;
/// The chunk-capable protocol version preferred by the SDK.
pub const PROTOCOL_V2: u8 = 2;

/// A negotiated RPC protocol version.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProtocolVersion(pub u8);

impl ProtocolVersion {
	/// The baseline, single-frame protocol.
	pub const V1: Self = Self(PROTOCOL_V1);
	/// The protocol supporting reassembled chunk frames.
	pub const V2: Self = Self(PROTOCOL_V2);
}

impl Default for ProtocolVersion {
	fn default() -> Self {
		Self::V1
	}
}

/// A request identifier used to correlate an RPC response.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestId(pub String);

impl RequestId {
	/// Creates a request identifier.
	pub fn new(id: impl Into<String>) -> Self {
		Self(id.into())
	}

	/// Returns the identifier as text.
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl fmt::Display for RequestId {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(&self.0)
	}
}

/// Server startup handshake emitted before any request is accepted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadyFrame {
	/// Frame discriminator. A conforming peer sends `"ready"`.
	#[serde(rename = "type")]
	pub kind: String,
	/// Protocol active before explicit negotiation.
	pub protocol_version: ProtocolVersion,
	/// Protocol versions accepted by the server.
	pub supported_protocol_versions: Vec<ProtocolVersion>,
	/// Maximum physical Content-Length payload.
	pub max_frame_bytes: usize,
	/// Maximum logical payload after v2 reassembly.
	pub max_reassembled_frame_bytes: usize,
}

impl ReadyFrame {
	/// Builds the standard v1-ready, v1/v2-capable handshake.
	pub fn v2_capable(max_frame_bytes: usize, max_reassembled_frame_bytes: usize) -> Self {
		Self {
			kind: "ready".into(),
			protocol_version: ProtocolVersion::V1,
			supported_protocol_versions: vec![ProtocolVersion::V1, ProtocolVersion::V2],
			max_frame_bytes,
			max_reassembled_frame_bytes,
		}
	}

	/// Returns whether the peer advertises a protocol version.
	pub fn supports(&self, version: ProtocolVersion) -> bool {
		self.supported_protocol_versions.contains(&version)
	}
}

/// Generic request envelope and escape hatch for commands added after this SDK.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RpcRequest {
	/// Correlation identifier. Notifications may omit it.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub id:      Option<RequestId>,
	/// Command discriminator encoded in the JSON `type` field.
	#[serde(rename = "type")]
	pub command: String,
	/// Command-specific fields flattened into the top-level object.
	#[serde(flatten)]
	pub params:  Map<String, Value>,
}

impl RpcRequest {
	/// Creates a request from a command name and serializable parameter object.
	pub fn from_params<P: Serialize>(
		id: Option<RequestId>,
		command: impl Into<String>,
		params: P,
	) -> Result<Self, serde_json::Error> {
		let params = match serde_json::to_value(params)? {
			Value::Null => Map::new(),
			Value::Object(params) => params,
			value => {
				let mut params = Map::new();
				params.insert("value".into(), value);
				params
			},
		};
		Ok(Self { id, command: command.into(), params })
	}

	/// Creates a request from already validated JSON fields.
	pub fn raw(
		id: Option<RequestId>,
		command: impl Into<String>,
		params: Map<String, Value>,
	) -> Self {
		Self { id, command: command.into(), params }
	}

	/// Deserializes the flattened command fields into a typed parameter object.
	pub fn parse_params<P: for<'de> Deserialize<'de>>(&self) -> Result<P, serde_json::Error> {
		serde_json::from_value(Value::Object(self.params.clone()))
	}
}

/// Machine-readable RPC command failure code.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RpcErrorCode(pub String);

impl RpcErrorCode {
	/// The request was malformed or semantically invalid.
	pub const INVALID_REQUEST: &'static str = "invalid_request";
	/// The session changed while a stable transcript page was being read.
	pub const SESSION_BUSY: &'static str = "session_busy";
	/// A transcript cursor no longer describes the active session snapshot.
	pub const STALE_CURSOR: &'static str = "stale_cursor";
	/// A requested protocol version is not supported.
	pub const UNSUPPORTED_PROTOCOL: &'static str = "unsupported_protocol";

	/// Creates a code while retaining unknown future values.
	pub fn new(code: impl Into<String>) -> Self {
		Self(code.into())
	}

	/// Returns the wire value.
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

/// Recoverable errors returned by stable transcript pagination.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptCursorError {
	/// The cursor describes an older session snapshot.
	StaleCursor,
	/// The session mutated while the page was being produced.
	SessionBusy,
}

impl TranscriptCursorError {
	/// Converts a generic response code into a pagination error when recognized.
	pub fn from_code(code: &RpcErrorCode) -> Option<Self> {
		match code.as_str() {
			RpcErrorCode::STALE_CURSOR => Some(Self::StaleCursor),
			RpcErrorCode::SESSION_BUSY => Some(Self::SessionBusy),
			_ => None,
		}
	}
}

impl fmt::Display for RpcErrorCode {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(&self.0)
	}
}

/// Response envelope shared by every command.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcResponse {
	/// Correlation identifier copied from the request.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub id:      Option<RequestId>,
	/// Frame discriminator. A conforming peer sends `"response"`.
	#[serde(rename = "type")]
	pub kind:    String,
	/// Command being answered.
	pub command: String,
	/// Whether the command completed successfully.
	pub success: bool,
	/// Command-specific success payload.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub data:    Option<Value>,
	/// Human-readable failure description.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub error:   Option<String>,
	/// Optional machine-readable failure reason.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub code:    Option<RpcErrorCode>,
}

impl RpcResponse {
	/// Builds a successful response.
	pub fn success<T: Serialize>(
		id: Option<RequestId>,
		command: impl Into<String>,
		data: T,
	) -> Result<Self, serde_json::Error> {
		Ok(Self {
			id,
			kind: "response".into(),
			command: command.into(),
			success: true,
			data: Some(serde_json::to_value(data)?),
			error: None,
			code: None,
		})
	}

	/// Builds a successful response without a `data` field.
	pub fn success_empty(id: Option<RequestId>, command: impl Into<String>) -> Self {
		Self {
			id,
			kind: "response".into(),
			command: command.into(),
			success: true,
			data: None,
			error: None,
			code: None,
		}
	}

	/// Builds a failed response.
	pub fn error(
		id: Option<RequestId>,
		command: impl Into<String>,
		error: impl Into<String>,
		code: Option<RpcErrorCode>,
	) -> Self {
		Self {
			id,
			kind: "response".into(),
			command: command.into(),
			success: false,
			data: None,
			error: Some(error.into()),
			code,
		}
	}
}

/// Parameters for protocol negotiation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NegotiateProtocolParams {
	/// Requested protocol version.
	pub protocol_version: ProtocolVersion,
}

/// Successful protocol negotiation payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NegotiateProtocolResult {
	/// Activated protocol version.
	pub protocol_version: ProtocolVersion,
}

/// How much subagent activity the server publishes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionLevel {
	/// Disable subagent frames.
	#[default]
	Off,
	/// Publish lifecycle and aggregate progress.
	Progress,
	/// Publish lifecycle, progress, and raw subagent events.
	Events,
}

/// Broad category assigned to a received event frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventCategory {
	/// Core agent streaming event.
	Agent,
	/// Session lifecycle or configuration event.
	Session,
	/// Subagent lifecycle transition.
	SubagentLifecycle,
	/// Aggregate subagent progress.
	SubagentProgress,
	/// Raw subagent session event.
	SubagentEvent,
	/// Available slash-command roster update.
	AvailableCommands,
	/// Session/config/extension notification.
	Notification,
	/// Extension UI request.
	ExtensionUi,
	/// Host-tool call or cancellation.
	HostTool,
	/// A future event not yet classified by this SDK.
	Other,
}

/// Generic event envelope retaining all application-owned fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RpcEvent {
	/// Event discriminator encoded in the JSON `type` field.
	#[serde(rename = "type")]
	pub kind:   String,
	/// Event-specific fields.
	#[serde(flatten)]
	pub fields: Map<String, Value>,
}

impl RpcEvent {
	/// Classifies a known event while retaining unknown events as
	/// [`EventCategory::Other`].
	pub fn category(&self) -> EventCategory {
		match self.kind.as_str() {
			"agent_start"
			| "agent_end"
			| "turn_start"
			| "turn_end"
			| "message_start"
			| "message_update"
			| "message_end"
			| "tool_execution_start"
			| "tool_execution_update"
			| "tool_execution_end" => EventCategory::Agent,
			"session_start"
			| "auto_compaction_start"
			| "auto_compaction_end"
			| "auto_retry_start"
			| "auto_retry_end"
			| "notice"
			| "irc_message"
			| "thinking_level_changed"
			| "model_changed"
			| "goal_updated" => EventCategory::Session,
			"subagent_lifecycle" => EventCategory::SubagentLifecycle,
			"subagent_progress" => EventCategory::SubagentProgress,
			"subagent_event" => EventCategory::SubagentEvent,
			"available_commands_update" => EventCategory::AvailableCommands,
			"session_info_update" | "config_update" | "extension_error" | "command_output" => {
				EventCategory::Notification
			},
			"extension_ui_request" => EventCategory::ExtensionUi,
			"host_tool_call" | "host_tool_cancel" => EventCategory::HostTool,
			_ => EventCategory::Other,
		}
	}

	/// Returns one field without consuming the event.
	pub fn get(&self, key: &str) -> Option<&Value> {
		self.fields.get(key)
	}
}

/// A typed notification that still accepts application-specific payloads.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NotificationFrame {
	/// Notification discriminator such as `session_info_update` or
	/// `config_update`.
	#[serde(rename = "type")]
	pub kind:   String,
	/// Notification-specific data.
	#[serde(flatten)]
	pub fields: Map<String, Value>,
}

/// Parameters used by prompt-like commands.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptParams {
	/// User-authored message.
	pub message:            String,
	/// Optional application-native image objects.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub images:             Vec<Value>,
	/// Optional prompt streaming behavior (`steer` or `followUp`).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub streaming_behavior: Option<String>,
}

/// Parameters used to create a new session.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionParams {
	/// Optional parent session path for lineage tracking.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub parent_session: Option<String>,
}

/// Stable transcript page returned by `get_messages_page`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptPage {
	/// Messages in this page.
	pub messages:       Vec<Value>,
	/// Cursor for the next page, or `None` at the end.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub next_cursor:    Option<String>,
	/// Message count in the stable snapshot.
	pub total_messages: usize,
}

/// Options for one stable transcript page.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TranscriptPageParams {
	/// Opaque cursor returned by the preceding page.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub cursor: Option<String>,
	/// Requested page size. Servers cap this independently.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub limit:  Option<usize>,
}

/// OAuth provider exposed by the RPC authentication seam.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OAuthProvider {
	/// Stable provider identifier.
	pub id:            String,
	/// Human-readable provider name.
	pub name:          String,
	/// Whether this build can authenticate the provider.
	pub available:     bool,
	/// Whether credentials are currently available.
	pub authenticated: bool,
}

/// Host-owned tool advertised to the agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostToolDefinition {
	/// Tool name used in model calls.
	pub name:        String,
	/// Optional short display label.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub label:       Option<String>,
	/// Description shown to the model.
	pub description: String,
	/// JSON Schema parameters object.
	pub parameters:  Value,
	/// Whether normal tool rosters hide this tool.
	#[serde(default, skip_serializing_if = "is_false")]
	pub hidden:      bool,
	/// Optional application-defined load mode.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub load_mode:   Option<String>,
}

/// Server request to execute a host-owned tool.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostToolCall {
	/// Frame discriminator.
	#[serde(rename = "type")]
	pub kind:         String,
	/// Invocation identifier used by update/result/cancel frames.
	pub id:           String,
	/// Model tool-call identifier.
	pub tool_call_id: String,
	/// Registered tool name.
	pub tool_name:    String,
	/// Validated tool arguments.
	pub arguments:    Map<String, Value>,
}

/// Server request to cancel an in-flight host tool invocation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostToolCancel {
	/// Frame discriminator.
	#[serde(rename = "type")]
	pub kind:      String,
	/// Cancellation frame identifier.
	pub id:        String,
	/// Invocation identifier to cancel.
	pub target_id: String,
}

/// Streaming update sent while a host tool runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostToolUpdate {
	/// Frame discriminator.
	#[serde(rename = "type")]
	pub kind:           String,
	/// Invocation identifier.
	pub id:             String,
	/// Application-native partial tool result.
	pub partial_result: Value,
}

/// Terminal result of a host tool invocation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostToolResult {
	/// Frame discriminator.
	#[serde(rename = "type")]
	pub kind:     String,
	/// Invocation identifier.
	pub id:       String,
	/// Application-native tool result.
	pub result:   Value,
	/// Whether the result represents a failure.
	#[serde(default, skip_serializing_if = "is_false")]
	pub is_error: bool,
}

/// One subagent entry in the current in-memory snapshot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentSnapshot {
	/// Stable subagent identifier.
	pub id:     String,
	/// Display ordering index.
	pub index:  usize,
	/// Agent kind or name.
	pub agent:  String,
	/// Current lifecycle status.
	pub status: String,
	/// Remaining application-specific snapshot fields.
	#[serde(flatten)]
	pub fields: Map<String, Value>,
}

/// Incremental persisted transcript read for a subagent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentMessages {
	/// Session file backing the transcript.
	pub session_file: String,
	/// Requested starting byte offset.
	pub from_byte:    u64,
	/// Byte offset for the next incremental read.
	pub next_byte:    u64,
	/// Whether the file reset and the caller must discard prior data.
	pub reset:        bool,
	/// Raw persisted entries.
	pub entries:      Vec<Value>,
	/// Renderable agent messages derived from the entries.
	pub messages:     Vec<Value>,
}

/// Extension UI request forwarded to the embedding host.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExtensionUiRequest {
	/// Frame discriminator.
	#[serde(rename = "type")]
	pub kind:   String,
	/// Request identifier copied into the UI response.
	pub id:     String,
	/// UI method such as `select`, `input`, or `open_url`.
	pub method: String,
	/// Method-specific fields.
	#[serde(flatten)]
	pub fields: Map<String, Value>,
}

/// Extension UI response sent by the embedding host.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExtensionUiResponse {
	/// Frame discriminator.
	#[serde(rename = "type")]
	pub kind:   String,
	/// Request identifier.
	pub id:     String,
	/// Response-specific fields (`value`, `confirmed`, or `cancelled`).
	#[serde(flatten)]
	pub fields: Map<String, Value>,
}

/// Ordered environment overrides used when spawning the RPC child.
pub type Environment = BTreeMap<String, String>;
