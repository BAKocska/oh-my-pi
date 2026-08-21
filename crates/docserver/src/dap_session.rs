//! Debug-session lifecycle, tree coordination, breakpoint serialization, and
//! output retention.

use std::{
	collections::{BTreeMap, VecDeque},
	sync::{
		Arc, Weak,
		atomic::{AtomicU64, Ordering},
	},
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use omp_core::Str;
use parking_lot::{Mutex, RwLock};
use serde_json::{Map, Value, json};
use thiserror::Error;
use tokio::sync::Mutex as AsyncMutex;

use crate::dap_protocol::{DapInbound, DapProtocol, DapProtocolError};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OUTPUT_BYTES: usize = 128 * 1024;
const IDLE_TIMEOUT: Duration = Duration::from_mins(10);

/// Stable DAP lifecycle state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DapSessionState {
	/// Adapter process and initialize request are starting.
	Launching,
	/// Launch/attach accepted; breakpoints are being configured.
	Configuring,
	/// Debuggee is suspended.
	Stopped,
	/// Debuggee is executing.
	Running,
	/// Adapter or debuggee ended.
	Terminated,
}

/// Debug action exposed to policy and tool layers.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DapAction {
	/// Start a program.
	Launch,
	/// Attach to an existing program.
	Attach,
	/// Add or replace a source breakpoint.
	SetBreakpoint,
	/// Remove a source breakpoint.
	RemoveBreakpoint,
	/// Add an instruction breakpoint.
	SetInstructionBreakpoint,
	/// Remove an instruction breakpoint.
	RemoveInstructionBreakpoint,
	/// Query a data breakpoint identifier.
	DataBreakpointInfo,
	/// Add a data breakpoint.
	SetDataBreakpoint,
	/// Remove a data breakpoint.
	RemoveDataBreakpoint,
	/// Resume execution.
	Continue,
	/// Step over.
	StepOver,
	/// Step into.
	StepIn,
	/// Step out.
	StepOut,
	/// Suspend execution.
	Pause,
	/// Evaluate an expression.
	Evaluate,
	/// Inspect stack frames.
	StackTrace,
	/// Inspect threads.
	Threads,
	/// Inspect scopes.
	Scopes,
	/// Inspect variables.
	Variables,
	/// Inspect instructions.
	Disassemble,
	/// Read process memory.
	ReadMemory,
	/// Write process memory.
	WriteMemory,
	/// Inspect modules.
	Modules,
	/// Inspect loaded sources.
	LoadedSources,
	/// Send an adapter extension request.
	CustomRequest,
	/// Read buffered output.
	Output,
	/// End a session tree.
	Terminate,
	/// List live sessions.
	Sessions,
}

/// Environment-side approval tier attached to each debug action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DapApprovalTier {
	/// Cannot mutate debuggee or adapter state.
	ReadOnly,
	/// May launch, resume, mutate, or terminate.
	Execution,
}

impl DapAction {
	/// Returns immutable env-side tier data; presentation layers do not decide
	/// it.
	#[must_use]
	pub const fn approval_tier(self) -> DapApprovalTier {
		match self {
			Self::DataBreakpointInfo
			| Self::Evaluate
			| Self::StackTrace
			| Self::Threads
			| Self::Scopes
			| Self::Variables
			| Self::Disassemble
			| Self::ReadMemory
			| Self::Modules
			| Self::LoadedSources
			| Self::Output
			| Self::Sessions => DapApprovalTier::ReadOnly,
			Self::Launch
			| Self::Attach
			| Self::SetBreakpoint
			| Self::RemoveBreakpoint
			| Self::SetInstructionBreakpoint
			| Self::RemoveInstructionBreakpoint
			| Self::SetDataBreakpoint
			| Self::RemoveDataBreakpoint
			| Self::Continue
			| Self::StepOver
			| Self::StepIn
			| Self::StepOut
			| Self::Pause
			| Self::WriteMemory
			| Self::CustomRequest
			| Self::Terminate => DapApprovalTier::Execution,
		}
	}

	/// Returns the standard DAP command for direct request actions.
	#[must_use]
	pub const fn command(self) -> Option<&'static str> {
		match self {
			Self::Continue => Some("continue"),
			Self::StepOver => Some("next"),
			Self::StepIn => Some("stepIn"),
			Self::StepOut => Some("stepOut"),
			Self::Pause => Some("pause"),
			Self::Evaluate => Some("evaluate"),
			Self::StackTrace => Some("stackTrace"),
			Self::Threads => Some("threads"),
			Self::Scopes => Some("scopes"),
			Self::Variables => Some("variables"),
			Self::Disassemble => Some("disassemble"),
			Self::ReadMemory => Some("readMemory"),
			Self::WriteMemory => Some("writeMemory"),
			Self::Modules => Some("modules"),
			Self::LoadedSources => Some("loadedSources"),
			Self::DataBreakpointInfo => Some("dataBreakpointInfo"),
			_ => None,
		}
	}
}

/// Session handshake, state, or protocol failure.
#[derive(Debug, Error)]
pub enum DapSessionError {
	/// Framing or adapter request failure.
	#[error(transparent)]
	Protocol(#[from] DapProtocolError),
	/// Lifecycle transition violated the state machine.
	#[error("invalid DAP session transition {from:?} -> {to:?}")]
	InvalidTransition {
		/// Current state.
		from: DapSessionState,
		/// Rejected next state.
		to:   DapSessionState,
	},
	/// This action requires a higher-level operation.
	#[error("debug action {0:?} has no direct protocol command")]
	UnsupportedAction(DapAction),
	/// Parent-child registration would create a cycle.
	#[error("debug session tree cannot contain a cycle")]
	SessionTreeCycle,
	/// Session identity is absent.
	#[error("debug session {0:?} was not found")]
	NotFound(Str),
}

/// Authority callback for adapter-to-client reverse requests.
#[async_trait]
pub trait DapReverseRequestHandler: Send + Sync + 'static {
	/// Handles one reverse request and returns the DAP response body.
	async fn handle(
		&self,
		session: Arc<DapSession>,
		command: &str,
		arguments: Value,
	) -> Result<Value, Str>;
}

struct RejectReverseRequests;

#[async_trait]
impl DapReverseRequestHandler for RejectReverseRequests {
	async fn handle(
		&self,
		_session: Arc<DapSession>,
		command: &str,
		_arguments: Value,
	) -> Result<Value, Str> {
		Err(Str::from(format!("reverse request {command:?} is not configured")))
	}
}

/// One live DAP session and its child-session subtree.
pub struct DapSession {
	id:                  Str,
	adapter:             Str,
	protocol:            DapProtocol,
	state:               Mutex<DapSessionState>,
	capabilities:        RwLock<Value>,
	output:              Mutex<VecDeque<u8>>,
	last_activity_ms:    AtomicU64,
	parent:              Mutex<Option<Weak<Self>>>,
	children:            Mutex<Vec<Weak<Self>>>,
	breakpoint_mutation: AsyncMutex<()>,
	source_breakpoints:  Mutex<BTreeMap<Str, Vec<Value>>>,
	handler:             Arc<dyn DapReverseRequestHandler>,
}

impl DapSession {
	/// Runs initialize + launch/attach + configurationDone with
	/// initialized-event pre-subscription.
	pub async fn start(
		id: impl AsRef<str>,
		adapter: impl AsRef<str>,
		protocol: DapProtocol,
		attach: bool,
		arguments: Map<String, Value>,
		handler: Option<Arc<dyn DapReverseRequestHandler>>,
	) -> Result<Arc<Self>, DapSessionError> {
		let initialized = protocol.subscribe();
		let session = Arc::new(Self {
			id: Str::new(id.as_ref()),
			adapter: Str::new(adapter.as_ref()),
			protocol,
			state: Mutex::new(DapSessionState::Launching),
			capabilities: RwLock::new(Value::Null),
			output: Mutex::new(VecDeque::with_capacity(MAX_OUTPUT_BYTES)),
			last_activity_ms: AtomicU64::new(now_ms()),
			parent: Mutex::new(None),
			children: Mutex::new(Vec::new()),
			breakpoint_mutation: AsyncMutex::new(()),
			source_breakpoints: Mutex::new(BTreeMap::new()),
			handler: handler.unwrap_or_else(|| Arc::new(RejectReverseRequests)),
		});
		Self::spawn_event_loop(&session);
		let capabilities = session
			.protocol
			.request(
				"initialize",
				json!({
					"clientID": "omp",
					"clientName": "Oh My Pi",
					"adapterID": session.adapter,
					"pathFormat": "path",
					"linesStartAt1": true,
					"columnsStartAt1": true,
					"supportsRunInTerminalRequest": true,
					"supportsStartDebuggingRequest": true
				}),
			)
			.await?;
		*session.capabilities.write() = capabilities;
		session.transition(DapSessionState::Configuring)?;
		let launch_protocol = session.protocol.clone();
		let command = if attach { "attach" } else { "launch" };
		let launch = tokio::spawn(async move {
			launch_protocol
				.request(command, Value::Object(arguments))
				.await
		});
		DapProtocol::wait_for_event(initialized, "initialized", HANDSHAKE_TIMEOUT).await?;
		session
			.protocol
			.request("configurationDone", json!({}))
			.await?;
		launch
			.await
			.map_err(|_| DapProtocolError::TransportClosed)??;
		if session.state() == DapSessionState::Configuring {
			session.transition(DapSessionState::Running)?;
		}
		Ok(session)
	}

	fn spawn_event_loop(session: &Arc<Self>) {
		let weak = Arc::downgrade(session);
		let mut events = session.protocol.subscribe();
		tokio::spawn(async move {
			loop {
				let Some(session) = weak.upgrade() else { break };
				tokio::select! {
					() = session.protocol.closed() => {
						*session.state.lock() = DapSessionState::Terminated;
						break;
					},
					event = events.recv() => match event {
						Ok(event) => Self::handle_inbound(&session, event).await,
						Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {},
						Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
					},
				}
			}
		});
	}

	async fn handle_inbound(session: &Arc<Self>, inbound: DapInbound) {
		session.touch();
		match inbound {
			DapInbound::Event { event, body } => match event.as_str() {
				"stopped" => {
					*session.state.lock() = DapSessionState::Stopped;
				},
				"continued" => {
					*session.state.lock() = DapSessionState::Running;
				},
				"terminated" | "exited" => {
					*session.state.lock() = DapSessionState::Terminated;
				},
				"output" => {
					if let Some(output) = body.get("output").and_then(Value::as_str) {
						session.push_output(output.as_bytes());
					}
				},
				_ => {},
			},
			DapInbound::ReverseRequest { seq, command, arguments } => {
				let result = session
					.handler
					.handle(Arc::clone(session), command.as_str(), arguments)
					.await;
				let (success, body, message) = match result {
					Ok(body) => (true, body, None),
					Err(message) => (false, Value::Null, Some(message)),
				};
				let _ = session
					.protocol
					.respond_reverse(seq, command.as_str(), success, body, message)
					.await;
			},
		}
	}

	fn transition(&self, to: DapSessionState) -> Result<(), DapSessionError> {
		let mut state = self.state.lock();
		let valid = matches!(
			(*state, to),
			(DapSessionState::Launching, DapSessionState::Configuring | DapSessionState::Terminated)
				| (
					DapSessionState::Configuring,
					DapSessionState::Running | DapSessionState::Stopped | DapSessionState::Terminated
				) | (DapSessionState::Running, DapSessionState::Stopped | DapSessionState::Terminated)
				| (DapSessionState::Stopped, DapSessionState::Running | DapSessionState::Terminated)
		);
		if !valid {
			return Err(DapSessionError::InvalidTransition { from: *state, to });
		}
		*state = to;
		self.touch();
		Ok(())
	}

	/// Returns the stable session identity.
	#[must_use]
	pub fn id(&self) -> &str {
		self.id.as_str()
	}

	/// Returns the selected adapter name.
	#[must_use]
	pub fn adapter(&self) -> &str {
		self.adapter.as_str()
	}

	/// Returns the current lifecycle state.
	#[must_use]
	pub fn state(&self) -> DapSessionState {
		*self.state.lock()
	}

	/// Returns the adapter initialize capabilities.
	#[must_use]
	pub fn capabilities(&self) -> Value {
		self.capabilities.read().clone()
	}

	/// Executes one direct action; callers can inspect `approval_tier` first.
	pub async fn execute(
		&self,
		action: DapAction,
		arguments: Value,
	) -> Result<Value, DapSessionError> {
		let command = action
			.command()
			.ok_or(DapSessionError::UnsupportedAction(action))?;
		self.touch();
		Ok(self.protocol.request(command, arguments).await?)
	}

	/// Sends an adapter-specific request without rewriting its payload.
	pub async fn custom_request(
		&self,
		command: &str,
		arguments: Value,
	) -> Result<Value, DapSessionError> {
		self.touch();
		Ok(self.protocol.request(command, arguments).await?)
	}

	/// Replaces source breakpoints atomically and synchronizes every live child.
	pub async fn set_source_breakpoints(
		self: &Arc<Self>,
		source: impl AsRef<str>,
		breakpoints: Vec<Value>,
	) -> Result<Value, DapSessionError> {
		let source = Str::new(source.as_ref());
		let (response, mut pending) = self
			.replace_source_breakpoints(&source, &breakpoints)
			.await?;
		pending.reverse();
		while let Some(session) = pending.pop() {
			let (_, children) = session
				.replace_source_breakpoints(&source, &breakpoints)
				.await?;
			pending.extend(children.into_iter().rev());
		}
		Ok(response)
	}

	async fn replace_source_breakpoints(
		&self,
		source: &Str,
		breakpoints: &[Value],
	) -> Result<(Value, Vec<Arc<Self>>), DapSessionError> {
		let _guard = self.breakpoint_mutation.lock().await;
		self
			.source_breakpoints
			.lock()
			.insert(source.clone(), breakpoints.to_vec());
		let response = self
			.protocol
			.request("setBreakpoints", json!({"source": {"path": source}, "breakpoints": breakpoints}))
			.await?;
		let children = self
			.children
			.lock()
			.iter()
			.filter_map(Weak::upgrade)
			.collect();
		drop(_guard);
		Ok((response, children))
	}

	/// Adds a child and replays current source breakpoints before exposing it.
	pub async fn add_child(self: &Arc<Self>, child: &Arc<Self>) -> Result<(), DapSessionError> {
		if Arc::ptr_eq(self, child) || self.has_ancestor(child) {
			return Err(DapSessionError::SessionTreeCycle);
		}
		*child.parent.lock() = Some(Arc::downgrade(self));
		self.children.lock().push(Arc::downgrade(child));
		let breakpoints = self.source_breakpoints.lock().clone();
		for (source, values) in breakpoints {
			child
				.set_source_breakpoints(source.as_str(), values)
				.await?;
		}
		Ok(())
	}

	/// Cascades termination through children, then disconnects this adapter.
	pub async fn terminate(self: &Arc<Self>) -> Result<(), DapSessionError> {
		let children = self
			.children
			.lock()
			.iter()
			.filter_map(Weak::upgrade)
			.collect::<Vec<_>>();
		for child in children {
			Box::pin(child.terminate()).await?;
		}
		if self.state() != DapSessionState::Terminated {
			let _ = self
				.protocol
				.request("terminate", json!({"restart": false}))
				.await;
			let _ = self
				.protocol
				.request("disconnect", json!({"restart": false, "terminateDebuggee": true}))
				.await;
			*self.state.lock() = DapSessionState::Terminated;
		}
		self.protocol.shutdown();
		Ok(())
	}

	/// Returns the retained tail of adapter/debuggee output.
	#[must_use]
	pub fn output_snapshot(&self) -> Vec<u8> {
		self.output.lock().iter().copied().collect()
	}

	fn push_output(&self, bytes: &[u8]) {
		let mut output = self.output.lock();
		let overflow = output
			.len()
			.saturating_add(bytes.len())
			.saturating_sub(MAX_OUTPUT_BYTES);
		let retained = overflow.min(output.len());
		output.drain(..retained);
		if bytes.len() >= MAX_OUTPUT_BYTES {
			output.extend(bytes[bytes.len() - MAX_OUTPUT_BYTES..].iter().copied());
		} else {
			output.extend(bytes.iter().copied());
		}
	}

	fn has_ancestor(self: &Arc<Self>, candidate: &Arc<Self>) -> bool {
		let mut cursor = self.parent.lock().as_ref().and_then(Weak::upgrade);
		while let Some(ancestor) = cursor {
			if Arc::ptr_eq(&ancestor, candidate) {
				return true;
			}
			cursor = ancestor.parent.lock().as_ref().and_then(Weak::upgrade);
		}
		false
	}

	fn touch(&self) {
		self.last_activity_ms.store(now_ms(), Ordering::Relaxed);
	}

	fn is_idle(&self, now: u64) -> bool {
		now.saturating_sub(self.last_activity_ms.load(Ordering::Relaxed))
			> IDLE_TIMEOUT.as_millis() as u64
	}
}

/// Project-scoped live debug-session registry.
#[derive(Default)]
pub struct DapSessionRegistry {
	sessions: RwLock<BTreeMap<Str, Arc<DapSession>>>,
}

impl DapSessionRegistry {
	/// Installs or replaces one stable session identity.
	pub fn insert(&self, session: Arc<DapSession>) -> Option<Arc<DapSession>> {
		self.sessions.write().insert(session.id.clone(), session)
	}

	/// Looks up one session.
	pub fn get(&self, id: &str) -> Result<Arc<DapSession>, DapSessionError> {
		self
			.sessions
			.read()
			.get(id)
			.cloned()
			.ok_or_else(|| DapSessionError::NotFound(Str::new(id)))
	}

	/// Lists sessions in stable identity order.
	pub fn list(&self) -> Vec<Arc<DapSession>> {
		self.sessions.read().values().cloned().collect()
	}

	/// Removes terminated sessions and idle sessions whose transport is already
	/// closed.
	pub fn cleanup(&self) -> Vec<Str> {
		let now = now_ms();
		let removed = self
			.sessions
			.read()
			.iter()
			.filter(|&(_id, session)| {
				(session.state() == DapSessionState::Terminated)
					|| (session.is_idle(now) && session.protocol.is_closed())
			})
			.map(|(id, _session)| id.clone())
			.collect::<Vec<_>>();
		let mut sessions = self.sessions.write();
		for id in &removed {
			sessions.remove(id);
		}
		removed
	}
}

fn now_ms() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map_or(0, |duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
	use omp_core::sf;

	use super::*;

	#[test]
	fn policy_tiers_are_environment_data() {
		assert_eq!(DapAction::Variables.approval_tier(), DapApprovalTier::ReadOnly);
		assert_eq!(DapAction::Continue.approval_tier(), DapApprovalTier::Execution);
		assert_eq!(DapAction::SetBreakpoint.approval_tier(), DapApprovalTier::Execution);
	}

	#[tokio::test]
	async fn output_ring_keeps_only_the_newest_bytes() {
		let (stream, _) = tokio::io::duplex(64);
		let (reader, writer) = tokio::io::split(stream);
		let session = DapSession {
			id:                  sf!("test"),
			adapter:             sf!("test"),
			protocol:            DapProtocol::from_streams(reader, writer),
			state:               Mutex::new(DapSessionState::Running),
			capabilities:        RwLock::new(Value::Null),
			output:              Mutex::new(VecDeque::new()),
			last_activity_ms:    AtomicU64::new(0),
			parent:              Mutex::new(None),
			children:            Mutex::new(Vec::new()),
			breakpoint_mutation: AsyncMutex::new(()),
			source_breakpoints:  Mutex::new(BTreeMap::new()),
			handler:             Arc::new(RejectReverseRequests),
		};
		session.push_output(&vec![b'a'; MAX_OUTPUT_BYTES]);
		session.push_output(b"tail");
		let output = session.output_snapshot();
		assert_eq!(output.len(), MAX_OUTPUT_BYTES);
		assert_eq!(&output[output.len() - 4..], b"tail");
	}
}
