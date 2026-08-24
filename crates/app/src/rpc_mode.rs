//! Stateful Content-Length framed RPC server for headless clients.

use std::{
	collections::{BTreeMap, HashMap, HashSet, VecDeque, hash_map::Entry},
	env,
	future::Future,
	io, mem,
	path::{Path, PathBuf},
	process::{self, Stdio},
	sync::{
		Arc,
		atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
	},
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use flume::{Receiver, Sender};
use futures::StreamExt as _;
use miette::{IntoDiagnostic as _, miette};
use omp_catalog::{ModelKey, ProviderId};
use omp_core::{ExposeSecret as _, SecretString, Str, sf};
use omp_driver::skills::SkillInvocationKind;
use omp_envd::tool_url::host;
use omp_inference::{
	Client, Registry,
	answer::{AccountState, AccountSummary, AuthAnswer, AuthEvent, AuthPromptKind, AuthSession},
	auth::manager::AuthManager,
	call::{
		AuthInput, AuthMethod, AuthRequest, CallMeta, ChatRequest, ContentPart, LoginRequest,
		Message, NegotiationPolicy, Role, Sampling, Setting, Target,
	},
	event::ChatEvent,
	id::{LoginSessionId, RequestId as InferenceRequestId},
	receipt::ExecutionBudget,
	router,
};
use omp_rpc::{
	framing::{
		ContentLengthDecoder, MAX_FRAME_BYTES, MAX_REASSEMBLED_BYTES, RpcFrameDecoder,
		encode_json_v1, encode_json_v2,
	},
	protocol::{
		ExtensionUiRequest, ExtensionUiResponse, HostToolCall, HostToolCancel, HostToolDefinition,
		HostToolResult, HostToolUpdate, HostUriCancel, HostUriOperation, HostUriRequest,
		HostUriResult, HostUriScheme, MAX_HOST_URI_DESCRIPTION_BYTES, MAX_HOST_URI_NOTE_BYTES,
		MAX_HOST_URI_NOTES, MAX_HOST_URI_SCHEME_BYTES, MAX_HOST_URI_SCHEMES, OAuthProvider,
		PROTOCOL_V1, PROTOCOL_V2, ReadyFrame, RequestId, RpcAuthAccount, RpcAuthAnswerFrame,
		RpcAuthEvent, RpcAuthEventFrame, RpcAuthInputKind, RpcAuthMethod, RpcAuthPromptKind,
		RpcAuthTerminalFrame, RpcAuthTerminalOutcome, RpcErrorCode, RpcRequest, RpcResponse,
		RpcTurnOutcome, SubagentMessages,
	},
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::{
	io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, stdin, stdout},
	process::Command,
	sync::oneshot,
	task::JoinHandle,
	time,
};
use tokio_util::sync::CancellationToken;

use crate::{
	chat_ui::commands::{
		CommandFuture, CommandResult, CommandRoster, ConfigCommandHost, ConsumedResult,
		FlowCommandHost, McpRequest, ModelCommandHost, ParsedFlags, SessionCommandHost,
		SessionRequest, ShellCommandHost, WorkspaceRequest,
	},
	cli::{RpcArgs, turn_id},
};

const DEFAULT_PAGE_MESSAGES: usize = 100;
const MAX_PAGE_MESSAGES: usize = 256;
const MAX_PAGE_BYTES: usize = 768 * 1024;
const MAX_SUBAGENT_TRANSCRIPTS: usize = 256;
const SUBAGENT_READ_BYTES: usize = 768 * 1024;
const ORCHESTRATE_NOTICE: &str = "The user explicitly requested orchestration. Treat this as a \
                                  hidden system instruction: delegate independent work to \
                                  available subagents, coordinate their results, and retain \
                                  responsibility for the final answer.";

static STDIN_CLAIMED: AtomicBool = AtomicBool::new(false);

/// Runs the stateful RPC server using stdin exclusively for protocol input and
/// stdout exclusively for protocol output.
pub async fn run(args: RpcArgs) -> miette::Result<()> {
	let _stdin_claim = StdinClaim::claim()?;
	// RPC stdout is protocol-only; process notifications are suppressed by the
	// embedding client before this process starts.

	let data = omp_core::dirs::data_dir(None).into_diagnostic()?;
	let configured_model = omp_driver::settings::current(&data)
		.into_diagnostic()?
		.default_model
		.map(Str::from);
	let model = args
		.model
		.or(configured_model)
		.ok_or_else(|| miette!("rpc mode requires --model or config.default_model"))?;
	let store =
		omp_driver::registry::open_credential_store(data.join("credentials.db")).into_diagnostic()?;
	let (registry, auth) = omp_driver::registry::production_rpc_registry(&data, store)
		.await
		.into_diagnostic()?;
	let models = registry
		.catalog()
		.models()
		.iter()
		.map(|entry| entry.key.as_str().to_owned())
		.collect::<Vec<_>>();
	let authenticated = match auth
		.execute(AuthRequest::ListAccounts { provider: None })
		.await
	{
		Ok(AuthAnswer::Accounts(accounts)) => accounts
			.into_iter()
			.filter(|account| account.state == AccountState::Active)
			.map(|account| account.provider.as_str().to_owned())
			.collect::<HashSet<_>>(),
		_ => HashSet::new(),
	};
	let providers = registry
		.catalog()
		.providers()
		.iter()
		.map(|entry| OAuthProvider {
			id:            entry.id.as_str().to_owned(),
			name:          entry.name.to_string(),
			available:     true,
			authenticated: authenticated.contains(entry.id.as_str()),
		})
		.collect::<Vec<_>>();
	let preferred_provider = args.provider.map(|provider| provider.to_string());
	if let Some(provider) = preferred_provider.as_deref()
		&& !providers.iter().any(|candidate| candidate.id == provider)
	{
		return Err(miette!("unknown RPC provider `{provider}`"));
	}
	let negotiated = Arc::new(AtomicU8::new(PROTOCOL_V1));
	let (output_tx, output_rx) = flume::unbounded();
	let writer = tokio::spawn(write_frames(stdout(), output_rx, negotiated.clone()));
	let ready = serde_json::to_value(ReadyFrame::v2_capable(MAX_FRAME_BYTES, MAX_REASSEMBLED_BYTES))
		.into_diagnostic()?;
	emit(&output_tx, ready)?;

	let host_resources = Arc::new(HostResourceBroker::new(output_tx.clone()));
	let host_resources_authority: Arc<dyn omp_envd::HostResources> = host_resources.clone();
	host::bind(&host_resources_authority)
		.map_err(|_| miette!("RPC host URI resolver is already bound"))?;
	let runtime = Arc::new(Runtime {
		registry,
		auth,
		commands: Mutex::new(rpc_command_roster(&args.project, 1)),
		output: output_tx.clone(),
		host_resources,
		shutdown: ShutdownCoordinator::default(),
		negotiated,
		state: Mutex::new(ServerState::new(
			model.to_string(),
			models,
			providers,
			preferred_provider,
			args.project,
			args.session_dir,
		)),
	});
	runtime.notify_session_start()?;
	runtime.notify_available_commands()?;

	let (input_tx, input_rx) = flume::unbounded();
	let reader = tokio::spawn(read_frames(stdin(), input_tx));
	let dispatch_result = dispatch_inputs(runtime.clone(), input_rx).await;
	{
		let mut state = runtime.state.lock();
		if let Some(active) = state.active.take() {
			active.cancel();
		}
		if let Some(active) = state.active_bash.take() {
			active.cancellation.cancel();
		}
		state.pending_host_tools.clear();
		for pending in state.pending_auth.values() {
			pending.cancellation.cancel();
		}
		state.pending_extension_ui.clear();
	}
	runtime.host_resources.shutdown("RPC client disconnected")?;
	host::unbind(&host_resources_authority);
	runtime.shutdown.shutdown().await;
	let read_result = reader.await.into_diagnostic()?;
	drop(runtime);
	drop(output_tx);
	let write_result = writer.await.into_diagnostic()?;
	dispatch_result?;
	read_result?;
	write_result
}

#[must_use]
struct StdinClaim;

impl StdinClaim {
	fn claim() -> miette::Result<Self> {
		STDIN_CLAIMED
			.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
			.map(|_| Self)
			.map_err(|_| miette!("RPC stdin has already been claimed in this process"))
	}
}

impl Drop for StdinClaim {
	fn drop(&mut self) {
		STDIN_CLAIMED.store(false, Ordering::Release);
	}
}

enum Input {
	Request(Value),
	Malformed(String),
}

async fn read_frames<R>(mut input: R, sender: Sender<Input>) -> miette::Result<()>
where
	R: AsyncRead + Unpin,
{
	let mut physical = ContentLengthDecoder::new();
	let mut logical = RpcFrameDecoder::new();
	let mut buffer = [0_u8; 16 * 1024];
	loop {
		let count = input.read(&mut buffer).await.into_diagnostic()?;
		if count == 0 {
			break;
		}
		let batch = physical.push(&buffer[..count]);
		for diagnostic in batch.diagnostics {
			sender
				.send_async(Input::Malformed(format!(
					"{} (skipped {} bytes)",
					diagnostic.reason, diagnostic.skipped_bytes
				)))
				.await
				.into_diagnostic()?;
		}
		for frame in batch.frames {
			match logical.push_frame(&frame) {
				Ok(Some(value)) => sender
					.send_async(Input::Request(value))
					.await
					.into_diagnostic()?,
				Ok(None) => {},
				Err(error) => {
					logical.reset();
					sender
						.send_async(Input::Malformed(error.to_string()))
						.await
						.into_diagnostic()?;
				},
			}
		}
	}
	Ok(())
}

async fn write_frames<W>(
	mut output: W,
	receiver: Receiver<Value>,
	negotiated: Arc<AtomicU8>,
) -> miette::Result<()>
where
	W: AsyncWrite + Unpin,
{
	let mut sequence = 0_u64;
	let streamed = HashSet::new();
	while let Ok(value) = receiver.recv_async().await {
		let frames = if negotiated.load(Ordering::Acquire) >= PROTOCOL_V2 {
			sequence = sequence.wrapping_add(1);
			encode_json_v2(&value, &format!("server-{sequence}"))
				.map_err(|error| miette!(error.to_string()))?
		} else {
			vec![encode_json_v1(&value, &streamed)]
		};
		for frame in frames {
			output.write_all(&frame).await.into_diagnostic()?;
		}
		output.flush().await.into_diagnostic()?;
	}
	Ok(())
}

async fn dispatch_inputs(runtime: Arc<Runtime>, receiver: Receiver<Input>) -> miette::Result<()> {
	let (ordinary_tx, ordinary_rx) = flume::unbounded();
	let worker_runtime = runtime.clone();
	let worker = async move {
		while let Ok(value) = ordinary_rx.recv_async().await {
			worker_runtime.handle_request(value).await?;
		}
		Ok::<_, miette::Report>(())
	};
	let reader = async move {
		while let Ok(input) = receiver.recv_async().await {
			match input {
				Input::Malformed(message) => {
					runtime.send_error(None, "parse", "parse_error", message)?;
				},
				Input::Request(value) if is_immediate_frame(&value) => {
					runtime.handle_immediate(value).await?;
				},
				Input::Request(value) => ordinary_tx.send_async(value).await.into_diagnostic()?,
			}
		}
		drop(ordinary_tx);
		Ok::<_, miette::Report>(())
	};
	let (worker, reader) = tokio::join!(worker, reader);
	worker.and(reader)
}

fn is_immediate_frame(value: &Value) -> bool {
	matches!(
		value.get("type").and_then(Value::as_str),
		Some(
			"bash"
				| "abort_bash"
				| "extension_ui_response"
				| "host_tool_update"
				| "host_tool_result"
				| "host_tool_cancel"
				| "host_uri_result"
				| "auth_answer"
		)
	)
}

const RESERVED_HOST_URI_SCHEMES: &[&str] = &[
	"agent",
	"artifact",
	"attachment",
	"conflict",
	"file",
	"history",
	"http",
	"https",
	"issue",
	"job",
	"local",
	"mcp",
	"memory",
	"omp",
	"pr",
	"rule",
	"security",
	"skill",
	"ssh",
	"vault",
];

struct PendingHostResource {
	generation: u64,
	settle:     Sender<HostUriResult>,
}

#[derive(Default)]
struct HostResourceState {
	generation: u64,
	schemes:    BTreeMap<String, HostUriScheme>,
	pending:    HashMap<String, PendingHostResource>,
}

/// Generation-fenced broker for virtual resources owned by the RPC host.
pub(crate) struct HostResourceBroker {
	output:   Sender<Value>,
	sequence: AtomicU64,
	state:    Mutex<HostResourceState>,
}

#[must_use]
struct PendingHostRequestGuard<'a> {
	broker:     &'a HostResourceBroker,
	id:         String,
	generation: u64,
}

impl Drop for PendingHostRequestGuard<'_> {
	fn drop(&mut self) {
		if self.broker.state.lock().pending.remove(&self.id).is_some() {
			let _ = self.broker.emit_cancel(self.generation, self.id.clone());
		}
	}
}

impl HostResourceBroker {
	fn new(output: Sender<Value>) -> Self {
		Self { output, sequence: AtomicU64::new(1), state: Mutex::new(HostResourceState::default()) }
	}

	fn replace(&self, params: &Map<String, Value>) -> Result<Value, CommandError> {
		let generation = params
			.get("generation")
			.and_then(Value::as_u64)
			.ok_or_else(|| CommandError::new("invalid_params", "generation must be an integer"))?;
		let raw_schemes = params
			.get("schemes")
			.and_then(Value::as_array)
			.ok_or_else(|| CommandError::new("invalid_params", "schemes must be an array"))?;
		if raw_schemes.len() > MAX_HOST_URI_SCHEMES {
			return Err(CommandError::new(
				"host_uri_limit",
				format!("at most {MAX_HOST_URI_SCHEMES} host URI schemes may be registered"),
			));
		}

		let mut schemes = BTreeMap::new();
		for raw in raw_schemes {
			let mut definition = serde_json::from_value::<HostUriScheme>(raw.clone())
				.map_err(|error| CommandError::new("invalid_params", error.to_string()))?;
			let normalized = normalize_host_uri_scheme(&definition.scheme)?;
			if definition
				.description
				.as_ref()
				.is_some_and(|description| description.len() > MAX_HOST_URI_DESCRIPTION_BYTES)
			{
				return Err(CommandError::new(
					"host_uri_description_limit",
					"host URI scheme description exceeds the protocol limit",
				));
			}
			definition.scheme.clone_from(&normalized);
			if schemes.insert(normalized, definition).is_some() {
				return Err(CommandError::new(
					"duplicate_host_uri_scheme",
					"host URI schemes must be unique after normalization",
				));
			}
		}

		let pending = {
			let mut state = self.state.lock();
			if generation <= state.generation {
				return Err(CommandError::new(
					"stale_generation",
					format!(
						"host URI generation {generation} does not advance generation {}",
						state.generation
					),
				));
			}
			let pending = mem::take(&mut state.pending);
			state.generation = generation;
			state.schemes = schemes;
			pending
		};
		for (target_id, pending) in pending {
			self
				.emit_cancel(pending.generation, target_id)
				.map_err(CommandError::transport)?;
		}
		let schemes = self
			.state
			.lock()
			.schemes
			.keys()
			.cloned()
			.collect::<Vec<_>>();
		Ok(json!({ "generation": generation, "schemes": schemes }))
	}

	/// Resolves a host-owned resource read through the active generation.
	async fn read(
		&self,
		url: &str,
		cancellation: CancellationToken,
	) -> Result<HostUriResult, CommandError> {
		self
			.request(HostUriOperation::Read, url, None, cancellation)
			.await
	}

	/// Dispatches a declared-writable host resource through the active
	/// generation.
	async fn write(
		&self,
		url: &str,
		content: String,
		cancellation: CancellationToken,
	) -> Result<HostUriResult, CommandError> {
		self
			.request(HostUriOperation::Write, url, Some(content), cancellation)
			.await
	}

	/// Resolves one URL for the Environment-owned host fallback.
	pub(crate) async fn resolve_read(&self, url: &str) -> Result<HostUriResult, Str> {
		self
			.read(url, CancellationToken::new())
			.await
			.map_err(|error| Str::from(error.message))
	}

	/// Writes one URL after the active declaration admits mutation.
	pub(crate) async fn resolve_write(
		&self,
		url: &str,
		content: String,
	) -> Result<HostUriResult, Str> {
		self
			.write(url, content, CancellationToken::new())
			.await
			.map_err(|error| Str::from(error.message))
	}

	async fn request(
		&self,
		operation: HostUriOperation,
		url: &str,
		content: Option<String>,
		cancellation: CancellationToken,
	) -> Result<HostUriResult, CommandError> {
		let scheme = url
			.split_once(':')
			.map(|(scheme, _)| scheme)
			.ok_or_else(|| CommandError::new("invalid_host_uri", "host URI requires a scheme"))?;
		let (generation, immutable) = {
			let state = self.state.lock();
			let definition = state.schemes.get(scheme).ok_or_else(|| {
				CommandError::new("host_uri_not_found", "host URI scheme is not registered")
			})?;
			if operation == HostUriOperation::Write && !definition.writable {
				return Err(CommandError::new(
					"host_uri_read_only",
					"host URI scheme did not declare writable access",
				));
			}
			(state.generation, definition.immutable)
		};
		let id = format!("host-uri-{}", self.sequence.fetch_add(1, Ordering::Relaxed));
		let (settle, result) = flume::bounded(1);
		self
			.state
			.lock()
			.pending
			.insert(id.clone(), PendingHostResource { generation, settle });
		let _pending_guard = PendingHostRequestGuard { broker: self, id: id.clone(), generation };
		let frame = HostUriRequest {
			kind: "host_uri_request".into(),
			id: id.clone(),
			generation,
			operation,
			url: url.to_owned(),
			content,
		};
		if let Err(error) =
			emit(&self.output, serde_json::to_value(frame).map_err(CommandError::json)?)
		{
			self.state.lock().pending.remove(&id);
			return Err(CommandError::transport(error));
		}

		tokio::select! {
			() = cancellation.cancelled() => {
				let removed = self.state.lock().pending.remove(&id);
				if removed.is_some() {
					self.emit_cancel(generation, id).map_err(CommandError::transport)?;
				}
				Err(CommandError::new("host_uri_cancelled", "host URI operation was cancelled"))
			},
			result = result.recv_async() => {
				let mut result = result.map_err(|_| {
					CommandError::new("host_uri_unavailable", "host URI route was replaced or disconnected")
				})?;
				if result.is_error {
					return Err(CommandError::new(
						"host_uri_failed",
						result.error.take().or(result.content.take()).unwrap_or_else(|| {
							"host rejected the URI operation".into()
						}),
					));
				}
				if operation == HostUriOperation::Read && result.content.is_none() {
					result.content = Some(String::new());
				}
				result.immutable = result.immutable.or(Some(immutable));
				Ok(result)
			},
		}
	}

	fn resolve(&self, mut result: HostUriResult) -> miette::Result<bool> {
		let invalid_metadata = result.notes.len() > MAX_HOST_URI_NOTES
			|| result
				.notes
				.iter()
				.any(|note| note.len() > MAX_HOST_URI_NOTE_BYTES)
			|| result
				.content_type
				.as_ref()
				.is_some_and(|content_type| content_type.as_str().is_empty());
		let pending = {
			let mut state = self.state.lock();
			if state.generation != result.generation
				|| state
					.pending
					.get(&result.id)
					.is_none_or(|pending| pending.generation != result.generation)
			{
				return Ok(false);
			}
			state.pending.remove(&result.id)
		};
		let Some(pending) = pending else {
			return Ok(false);
		};
		if invalid_metadata {
			result.content = None;
			result.content_type = None;
			result.notes.clear();
			result.immutable = None;
			result.is_error = true;
			result.error = Some("host URI result metadata exceeds the protocol bounds".into());
		}
		Ok(pending.settle.send(result).is_ok())
	}

	fn shutdown(&self, _reason: &str) -> miette::Result<()> {
		let pending = {
			let mut state = self.state.lock();
			state.schemes.clear();
			mem::take(&mut state.pending)
		};
		for (target_id, pending) in pending {
			self.emit_cancel(pending.generation, target_id)?;
		}
		Ok(())
	}

	fn emit_cancel(&self, generation: u64, target_id: String) -> miette::Result<()> {
		let frame = HostUriCancel {
			kind: "host_uri_cancel".into(),
			id: format!("host-uri-cancel-{}", self.sequence.fetch_add(1, Ordering::Relaxed)),
			generation,
			target_id,
		};
		emit(&self.output, serde_json::to_value(frame).into_diagnostic()?)
	}
}

#[async_trait::async_trait]
impl omp_envd::HostResources for HostResourceBroker {
	async fn resolve_read(&self, url: &str) -> Result<omp_envd::HostResourceResult, Str> {
		let result = HostResourceBroker::resolve_read(self, url).await?;
		Ok(omp_envd::HostResourceResult { content: result.content, notes: result.notes })
	}

	async fn resolve_write(
		&self,
		url: &str,
		content: String,
	) -> Result<omp_envd::HostResourceResult, Str> {
		let result = HostResourceBroker::resolve_write(self, url, content).await?;
		Ok(omp_envd::HostResourceResult { content: result.content, notes: result.notes })
	}
}

fn normalize_host_uri_scheme(raw: &str) -> Result<String, CommandError> {
	let scheme = raw.trim().to_ascii_lowercase();
	let mut bytes = scheme.bytes();
	if scheme.is_empty()
		|| scheme.len() > MAX_HOST_URI_SCHEME_BYTES
		|| !bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
		|| !bytes
			.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"+.-".contains(&byte))
	{
		return Err(CommandError::new(
			"invalid_host_uri_scheme",
			"host URI schemes must match ^[a-z][a-z0-9+.-]*$",
		));
	}
	if RESERVED_HOST_URI_SCHEMES
		.binary_search(&scheme.as_str())
		.is_ok()
	{
		return Err(CommandError::new(
			"reserved_host_uri_scheme",
			format!("host URI scheme is owned by the environment: {scheme}://"),
		));
	}
	Ok(scheme)
}

#[derive(Default)]
struct ShutdownCoordinator {
	tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl ShutdownCoordinator {
	fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
		self.tasks.lock().push(tokio::spawn(future));
	}

	async fn shutdown(&self) {
		let tasks = mem::take(&mut *self.tasks.lock());
		for mut task in tasks {
			if time::timeout(Duration::from_secs(1), &mut task)
				.await
				.is_err()
			{
				task.abort();
				let _ = task.await;
			}
		}
	}
}

struct Runtime {
	registry:       Registry,
	auth:           AuthManager,
	commands:       Mutex<CommandRoster>,
	output:         Sender<Value>,
	host_resources: Arc<HostResourceBroker>,
	shutdown:       ShutdownCoordinator,
	negotiated:     Arc<AtomicU8>,
	state:          Mutex<ServerState>,
}

impl Runtime {
	async fn handle_request(self: &Arc<Self>, value: Value) -> miette::Result<()> {
		let request = match serde_json::from_value::<RpcRequest>(value) {
			Ok(request) => request,
			Err(error) => {
				return self.send_error(None, "parse", "invalid_request", error.to_string());
			},
		};
		if request.command == "bash" {
			return self.start_bash(request);
		}
		if request.command == "login" {
			return self.handle_login(request).await;
		}
		let id = request.id.clone();
		let command = request.command.clone();
		let result = self.execute(&command, &request.params).await;
		match result {
			Ok(data) => self.send_success(id, &command, data),
			Err(error) => self.send_error(id, &command, error.code, error.message),
		}
	}

	async fn handle_immediate(self: &Arc<Self>, value: Value) -> miette::Result<()> {
		match value.get("type").and_then(Value::as_str) {
			Some("bash") => {
				let request = match serde_json::from_value::<RpcRequest>(value) {
					Ok(request) => request,
					Err(error) => {
						return self.send_error(None, "bash", "invalid_request", error.to_string());
					},
				};
				self.start_bash(request)
			},
			Some("abort_bash") => {
				let request = match serde_json::from_value::<RpcRequest>(value) {
					Ok(request) => request,
					Err(error) => {
						return self.send_error(None, "abort_bash", "invalid_request", error.to_string());
					},
				};
				let aborted = self.abort_bash();
				self.send_success(request.id, "abort_bash", json!({ "aborted": aborted }))
			},
			Some("extension_ui_response") => {
				let response = match serde_json::from_value::<ExtensionUiResponse>(value) {
					Ok(response) => response,
					Err(error) => {
						return self.send_error(
							None,
							"extension_ui_response",
							"invalid_request",
							error.to_string(),
						);
					},
				};
				let pending = self.state.lock().pending_extension_ui.remove(&response.id);
				if let Some(pending) = pending {
					let _ = pending.send(response);
					Ok(())
				} else {
					self.send_error(
						Some(RequestId::new(response.id)),
						"extension_ui_response",
						"extension_ui_not_pending",
						"no extension UI request is awaiting this response",
					)
				}
			},
			Some("host_tool_update" | "host_tool_result" | "host_tool_cancel") => {
				self.handle_side_channel(value)
			},
			Some("host_uri_result") => {
				let result = match serde_json::from_value::<HostUriResult>(value) {
					Ok(result) => result,
					Err(error) => {
						return self.send_error(
							None,
							"host_uri_result",
							"invalid_request",
							error.to_string(),
						);
					},
				};
				if self.host_resources.resolve(result)? {
					Ok(())
				} else {
					self.send_error(
						None,
						"host_uri_result",
						"stale_host_uri_result",
						"host URI result is stale or unknown",
					)
				}
			},
			Some("auth_answer") => {
				let answer = match serde_json::from_value::<RpcAuthAnswerFrame>(value) {
					Ok(answer) => answer,
					Err(error) => {
						return self.send_error(
							None,
							"auth_answer",
							"invalid_request",
							error.to_string(),
						);
					},
				};
				match self.answer_auth(answer).await {
					Ok(()) => Ok(()),
					Err(error) => self.send_error(None, "auth_answer", error.code, error.message),
				}
			},
			_ => self.send_error(None, "side_channel", "invalid_request", "unknown immediate frame"),
		}
	}

	fn start_bash(self: &Arc<Self>, request: RpcRequest) -> miette::Result<()> {
		let params = match parse_params::<BashParams>(&request.params) {
			Ok(params) => params,
			Err(error) => {
				return self.send_error(request.id, "bash", error.code, error.message);
			},
		};
		let operation_id = new_id("bash");
		let cancellation = CancellationToken::new();
		let project = {
			let mut state = self.state.lock();
			if state.active_bash.is_some() {
				return self.send_error(
					request.id,
					"bash",
					"bash_busy",
					"a shell command is already active",
				);
			}
			state.active_bash = Some(ActiveBash {
				id:           operation_id.clone(),
				cancellation: cancellation.clone(),
			});
			state.project.clone()
		};
		let mut command = shell_command(&params.command);
		command
			.current_dir(project)
			.stdin(Stdio::null())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.kill_on_drop(true);
		let child = match command.spawn() {
			Ok(child) => child,
			Err(error) => {
				self.state.lock().active_bash = None;
				return self.send_error(request.id, "bash", "bash_spawn_failed", error.to_string());
			},
		};
		let runtime = self.clone();
		self.shutdown.spawn(async move {
			let data = tokio::select! {
				() = cancellation.cancelled() => {
					json!({
						"operationId": operation_id.clone(),
						"stdout": "",
						"stderr": "",
						"status": Value::Null,
						"success": false,
						"aborted": true,
					})
				},
				output = child.wait_with_output() => match output {
					Ok(output) => json!({
						"operationId": operation_id.clone(),
						"stdout": String::from_utf8_lossy(&output.stdout),
						"stderr": String::from_utf8_lossy(&output.stderr),
						"status": output.status.code(),
						"success": output.status.success(),
						"aborted": false,
					}),
					Err(error) => {
						let mut state = runtime.state.lock();
						if state.active_bash.as_ref().is_some_and(|active| active.id == operation_id) {
							state.active_bash = None;
						}
						drop(state);
						let _ = runtime.send_error(request.id, "bash", "bash_wait_failed", error.to_string());
						return;
					},
				},
			};
			{
				let mut state = runtime.state.lock();
				if state
					.active_bash
					.as_ref()
					.is_some_and(|active| active.id == operation_id)
				{
					state.active_bash = None;
				}
			}
			let mut event = data.clone();
			if let Some(event) = event.as_object_mut() {
				event.insert("type".into(), Value::String("bash_result".into()));
			}
			let _ = runtime.notify(event);
			let _ = runtime.send_success(request.id, "bash", data);
		});
		Ok(())
	}

	fn abort_bash(&self) -> bool {
		let state = self.state.lock();
		state.active_bash.as_ref().is_some_and(|active| {
			active.cancellation.cancel();
			true
		})
	}

	async fn execute(
		self: &Arc<Self>,
		command: &str,
		params: &Map<String, Value>,
	) -> Result<Value, CommandError> {
		match command {
			"negotiate_protocol" => {
				let version = unsigned(params, "protocolVersion")? as u8;
				if !matches!(version, PROTOCOL_V1 | PROTOCOL_V2) {
					return Err(CommandError::new(
						"unsupported_protocol",
						"only protocol versions 1 and 2 are supported",
					));
				}
				self.negotiated.store(version, Ordering::Release);
				Ok(json!({ "protocolVersion": version }))
			},
			"prompt" => {
				let text = text(params, "message")
					.or_else(|_| text(params, "text"))?
					.to_owned();
				let text = self.expand_skill(&text)?.unwrap_or(text);
				let behavior = params
					.get("streamingBehavior")
					.and_then(Value::as_str)
					.unwrap_or("prompt");
				match self.intercept_command(&text).await? {
					CommandIntercept::Passthrough => {
						self.submit_prompt(text, behavior)?;
						self
							.notify(json!({ "type": "prompt_result", "invoked": true }))
							.map_err(CommandError::transport)?;
						Ok(json!({ "invoked": true }))
					},
					CommandIntercept::Prompt(prompt) => {
						self.submit_prompt(prompt, behavior)?;
						self
							.notify(json!({ "type": "prompt_result", "invoked": true }))
							.map_err(CommandError::transport)?;
						Ok(json!({ "invoked": true, "command": true }))
					},
					CommandIntercept::Consumed(agent_invoked) => {
						Ok(json!({ "invoked": agent_invoked, "command": true }))
					},
					CommandIntercept::Exit => {
						let _ = self.abort(false, None)?;
						Ok(json!({ "invoked": false, "command": true, "exit": true }))
					},
				}
			},
			"steer" => {
				let message = text(params, "message")
					.or_else(|_| text(params, "text"))?
					.to_owned();
				self.submit_prompt(message, "steer")?;
				Ok(json!({ "queued": true, "mode": "steer" }))
			},
			"follow_up" => {
				let message = text(params, "message")
					.or_else(|_| text(params, "text"))?
					.to_owned();
				self.submit_prompt(message, "followUp")?;
				Ok(json!({ "queued": true, "mode": "followUp" }))
			},
			"abort" => Ok(json!({ "aborted": self.abort(false, None)? })),
			"abort_and_prompt" => {
				let message = text(params, "message")
					.or_else(|_| text(params, "text"))?
					.to_owned();
				Ok(json!({ "aborted": self.abort(true, Some(message))? }))
			},
			"new_session" => self.new_session(params),
			"get_state" => Ok(self.state_value()),
			"get_available_models" => {
				let state = self.state.lock();
				Ok(json!({ "models": state.models, "active": state.config.model }))
			},
			"cycle_model" => self.cycle_model(),
			"set_model" => {
				let params = parse_params::<SetModelParams>(params)?;
				self.set_model(&params.provider, &params.model_id)
			},
			"set_fast_mode" => self.set_bool_config("fastMode", boolean(params, "enabled")?),
			"set_thinking_level" => self.set_string_config("thinkingLevel", text(params, "level")?),
			"cycle_thinking_level" => self.cycle_thinking(),
			"set_steering_mode" => self.set_string_config("steeringMode", text(params, "mode")?),
			"set_follow_up_mode" => self.set_string_config("followUpMode", text(params, "mode")?),
			"set_interrupt_mode" => {
				let mode = text(params, "mode")?;
				if !matches!(mode, "immediate" | "wait") {
					return Err(CommandError::new(
						"invalid_params",
						"interrupt mode must be immediate or wait",
					));
				}
				self.set_string_config("interruptMode", mode)
			},
			"set_auto_compaction" => {
				self.set_bool_config("autoCompaction", boolean(params, "enabled")?)
			},
			"set_auto_retry" => self.set_bool_config("autoRetry", boolean(params, "enabled")?),
			"abort_retry" => Ok(json!({ "aborted": false })),
			"set_todos" => self.set_todos(params),
			"compact" => self.compact(),
			"get_session_stats" => self.session_stats(),
			"switch_session" => {
				let params = parse_params::<SwitchSessionParams>(params)?;
				self.switch_session(&params.session_path)
			},
			"branch" => self.branch(params),
			"get_branch_messages" | "get_messages" => self.get_messages(params),
			"get_messages_page" => self.get_messages_page(params),
			"get_last_assistant_text" => self.last_assistant(),
			"set_session_name" => self.rename_session(text(params, "name")?),
			"handoff" => self.handoff(params),
			"export_html" => self.export_html(),
			"get_login_providers" => self.login_providers(),
			"set_host_tools" => self.set_host_tools(params),
			"call_host_tool" => self.call_host_tool(params),
			"set_host_uri_schemes" => self.host_resources.replace(params),
			"set_subagent_subscription" => self.set_subscription(params),
			"get_subagents" => self.get_subagents(),
			"get_subagent_messages" => self.get_subagent_messages(params).await,
			"get_available_commands" => Ok(self.available_commands_value()),
			"reload_extensions" => self.reload_extensions(),
			"extension_ui_request" => self.forward_extension_ui(params).await,
			"extension_error" => self.notify_extension_error(params),
			"bash" | "abort_bash" => Err(CommandError::new(
				"invalid_request",
				"shell commands must be dispatched through the asynchronous command path",
			)),
			_ => Err(CommandError::new("unknown_command", format!("unknown RPC command `{command}`"))),
		}
	}

	async fn intercept_command(
		self: &Arc<Self>,
		text: &str,
	) -> Result<CommandIntercept, CommandError> {
		use crate::chat_ui::commands::{CommandResult, CommandSurface, DispatchResult};
		let roster = self.commands.lock().clone();
		let mut host = RpcCommandHost { runtime: self.clone(), roster: roster.clone() };
		let dispatched = roster
			.dispatch(text, CommandSurface::Text, &mut host)
			.await
			.map_err(|error| CommandError::new("command_failed", error.to_string()))?;
		match dispatched {
			DispatchResult::Passthrough(_) => Ok(CommandIntercept::Passthrough),
			DispatchResult::Handled(CommandResult::Prompt(prompt)) => {
				Ok(CommandIntercept::Prompt(prompt.text.to_string()))
			},
			DispatchResult::Handled(CommandResult::Consumed(result)) => {
				let agent_invoked = result.agent_invoked;
				if let Some(status) = result.status {
					self
						.notify(json!({
							"type": "command_output",
							"stream": "stdout",
							"content": status.as_str(),
							"generation": 0,
						}))
						.map_err(CommandError::transport)?;
				}
				Ok(CommandIntercept::Consumed(agent_invoked))
			},
			DispatchResult::Handled(CommandResult::Exit) => Ok(CommandIntercept::Exit),
		}
	}

	fn available_commands_value(&self) -> Value {
		use crate::chat_ui::commands::{CommandCapability, CommandRole, CommandSurface};
		let mut commands = self
			.commands
			.lock()
			.advertised(CommandSurface::Text, CommandRole::Owner, true, |capability| {
				matches!(capability, CommandCapability::Session)
			})
			.into_iter()
			.map(|command| {
				json!({
					"name": command.name.as_str(),
					"description": command.description.as_str(),
					"argumentHint": command.argument_hint.as_ref().map(Str::as_str),
					"source": command.provenance.source.as_str(),
					"generation": command.provenance.generation,
				})
			})
			.collect::<Vec<_>>();
		let state = self.state.lock();
		commands.extend(state.content.skills.visible().map(|skill| {
			json!({
				"name":format!("skill:{}",skill.name),
				"description":skill.description,
				"argumentHint":"[arguments]",
				"source":skill.source,
				"generation":state.command_generation,
			})
		}));
		let generation = commands
			.iter()
			.filter_map(|command| command.get("generation").and_then(Value::as_u64))
			.max()
			.unwrap_or(0);
		json!({ "generation": generation, "commands": commands })
	}

	fn notify_available_commands(&self) -> miette::Result<()> {
		let mut frame = self.available_commands_value();
		frame
			.as_object_mut()
			.expect("available command projection is an object")
			.insert("type".into(), Value::String("available_commands_update".into()));
		self.notify(frame)
	}

	fn expand_skill(&self, text: &str) -> Result<Option<String>, CommandError> {
		let Some(invocation) = omp_driver::skills::parse_invocation(text) else {
			return Ok(None);
		};
		let skill = self
			.state
			.lock()
			.content
			.skills
			.get(invocation.name.as_str())
			.cloned()
			.ok_or_else(|| {
				CommandError::new("skill_not_found", format!("unknown skill `{}`", invocation.name))
			})?;
		let rendered = omp_driver::skills::render_invocation(
			&skill,
			invocation.args.as_str(),
			SkillInvocationKind::User,
		);
		self
			.notify(json!({
				"type":"command_output",
				"stream":"stdout",
				"content":format!("Skill `{}` loaded for this turn.",skill.name),
				"generation":self.state.lock().command_generation,
			}))
			.map_err(CommandError::transport)?;
		Ok(Some(rendered.to_string()))
	}

	fn reload_extensions(&self) -> Result<Value, CommandError> {
		let (root, generation) = {
			let mut state = self.state.lock();
			state.command_generation = state.command_generation.wrapping_add(1).max(1);
			state.content = omp_driver::discovery::active_content_snapshots(&state.project);
			(state.project.clone(), state.command_generation)
		};
		*self.commands.lock() = rpc_command_roster(&root, generation);
		let (skills, commands) = {
			let state = self.state.lock();
			(state.content.skills.all().len(), state.content.commands.len())
		};
		self
			.notify(json!({
				"type":"extension_generation_update",
				"generation":generation,
				"capabilities":{"skills":skills,"commands":commands}
			}))
			.map_err(CommandError::transport)?;
		self
			.notify_available_commands()
			.map_err(CommandError::transport)?;
		Ok(json!({"generation":generation}))
	}

	async fn forward_extension_ui(
		&self,
		params: &Map<String, Value>,
	) -> Result<Value, CommandError> {
		let method = text(params, "method")?.to_owned();
		let id = params
			.get("id")
			.and_then(Value::as_str)
			.map_or_else(|| new_id("extension-ui"), str::to_owned);
		let mut fields = params.clone();
		fields.remove("id");
		fields.remove("method");
		fields.remove("type");
		let request =
			ExtensionUiRequest { kind: "extension_ui_request".into(), id: id.clone(), method, fields };
		let (reply, response) = oneshot::channel();
		{
			let mut state = self.state.lock();
			match state.pending_extension_ui.entry(id.clone()) {
				Entry::Vacant(entry) => {
					entry.insert(reply);
				},
				Entry::Occupied(_) => {
					return Err(CommandError::new(
						"extension_ui_duplicate",
						format!("extension UI request `{id}` is already pending"),
					));
				},
			}
		}
		if let Err(error) = self.notify(serde_json::to_value(request).map_err(CommandError::json)?) {
			self.state.lock().pending_extension_ui.remove(&id);
			return Err(CommandError::transport(error));
		}
		let response = response.await.map_err(|_| {
			CommandError::new(
				"extension_ui_disconnected",
				"RPC client disconnected before answering the extension UI request",
			)
		})?;
		Ok(Value::Object(response.fields))
	}

	fn notify_extension_error(&self, params: &Map<String, Value>) -> Result<Value, CommandError> {
		let extension = text(params, "extension")?;
		let message = text(params, "message")?;
		self
			.notify(json!({
				"type":"extension_error",
				"extension":extension,
				"message":message,
				"generation":self.state.lock().command_generation,
			}))
			.map_err(CommandError::transport)?;
		Ok(json!({"notified":true}))
	}

	fn submit_prompt(self: &Arc<Self>, message: String, behavior: &str) -> Result<(), CommandError> {
		let mut state = self.state.lock();
		if let Some(active) = state.active.as_ref() {
			match behavior {
				"steer" => {
					active.cancel();
					state.queue.push_front(message);
					return Ok(());
				},
				"followUp" | "follow_up" => {
					state.queue.push_back(message);
					return Ok(());
				},
				_ => return Err(CommandError::new("session_busy", "an agent turn is already active")),
			}
		}
		let cancellation = CancellationToken::new();
		state.active = Some(cancellation.clone());
		drop(state);
		let runtime = self.clone();
		self
			.shutdown
			.spawn(async move { runtime.conversation_loop(message, cancellation).await });
		Ok(())
	}

	async fn conversation_loop(
		self: Arc<Self>,
		mut message: String,
		mut cancellation: CancellationToken,
	) {
		loop {
			let _ = self.run_turn(message, cancellation.clone()).await;
			let next = {
				let mut state = self.state.lock();
				state.active = None;
				state.queue.pop_front()
			};
			let Some(next) = next else { break };
			message = next;
			cancellation = CancellationToken::new();
			self.state.lock().active = Some(cancellation.clone());
		}
	}

	async fn run_turn(
		&self,
		prompt: String,
		cancellation: CancellationToken,
	) -> Result<(), CommandError> {
		let (session_id, model, request) = {
			let mut state = self.state.lock();
			let session_id = state.current.clone();
			let model = state.config.model.clone();
			let session = state.current_mut();
			session.push_message("user", &prompt);
			let request = build_request(session, contains_orchestrate(&prompt));
			(session_id, model, request)
		};
		self
			.notify(json!({ "type": "agent_start", "sessionId": session_id }))
			.map_err(CommandError::transport)?;
		let planner = router::Router::new(self.registry.clone(), Duration::from_secs(30));
		let meta = CallMeta {
			id:       InferenceRequestId::from(turn_id()),
			target:   Target::Model(ModelKey::from(model)),
			deadline: None,
			budget:   ExecutionBudget::default(),
			session:  None,
		};
		let mut client = Client::new(self.registry.service(), planner, meta);
		let mut events = match client.execute(request).await {
			Ok(events) => events,
			Err(error) => {
				self.notify(json!({ "type": "agent_end", "sessionId": session_id, "outcome": RpcTurnOutcome::Fault, "error": error.to_string(), "aborted": false }))
					.map_err(CommandError::transport)?;
				return Err(CommandError::new("inference_error", error.to_string()));
			},
		};
		let mut assistant = String::new();
		let mut completed = false;
		let mut aborted = false;
		let mut fault = false;
		loop {
			tokio::select! {
				() = cancellation.cancelled() => {
					aborted = true;
					break;
				}
				event = events.next() => {
					let Some(event) = event else { break };
					match event {
						Ok(ChatEvent::TextDelta { text, .. }) => {
							assistant.push_str(text.as_str());
							self.notify(json!({ "type": "agent_event", "event": { "type": "text_delta", "text": text.as_str() }, "sessionId": session_id }))
								.map_err(CommandError::transport)?;
						},
						Ok(ChatEvent::ThinkingDelta { text, .. }) => {
							self.notify(json!({ "type": "agent_event", "event": { "type": "thinking_delta", "text": text.as_str() }, "sessionId": session_id }))
								.map_err(CommandError::transport)?;
						},
						Ok(ChatEvent::Completed(_)) => completed = true,
						Ok(_) => {},
						Err(error) => {
							fault = true;
							self.notify(json!({ "type": "agent_event", "event": { "type": "error", "message": error.to_string() }, "sessionId": session_id }))
								.map_err(CommandError::transport)?;
							break;
						},
					}
				}
			}
		}
		if !assistant.is_empty()
			&& let Some(session) = self.state.lock().sessions.get_mut(&session_id)
		{
			session.push_message("assistant", &assistant);
		}
		let outcome = if aborted {
			RpcTurnOutcome::CallerAbort
		} else if completed && !fault {
			RpcTurnOutcome::Success
		} else {
			RpcTurnOutcome::Fault
		};
		self.notify(json!({
			"type": "agent_end",
			"sessionId": session_id,
			"outcome": outcome,
			"aborted": aborted,
			"completed": completed,
			"message": if assistant.is_empty() { Value::Null } else { json!({ "role": "assistant", "content": assistant }) },
		}))
		.map_err(CommandError::transport)
	}

	fn abort(&self, replace_queue: bool, message: Option<String>) -> Result<bool, CommandError> {
		let (active, pending) = {
			let mut state = self.state.lock();
			let active = state.active.as_ref().is_some_and(|token| {
				token.cancel();
				true
			});
			if replace_queue {
				state.queue.clear();
				if let Some(message) = message {
					state.queue.push_front(message);
				}
			}
			let pending = state.pending_host_tools.keys().cloned().collect::<Vec<_>>();
			state.pending_host_tools.clear();
			(active, pending)
		};
		for target_id in pending {
			let frame = HostToolCancel {
				kind: "host_tool_cancel".into(),
				id: new_id("host-tool-cancel"),
				target_id,
			};
			self
				.notify(serde_json::to_value(frame).map_err(CommandError::json)?)
				.map_err(CommandError::transport)?;
		}
		Ok(active)
	}

	fn new_session(&self, params: &Map<String, Value>) -> Result<Value, CommandError> {
		let mut state = self.state.lock();
		if let Some(active) = state.active.take() {
			active.cancel();
		}
		state.queue.clear();
		let parent = params
			.get("parentSession")
			.and_then(Value::as_str)
			.map(str::to_owned);
		let id = new_id("session");
		state
			.sessions
			.insert(id.clone(), Session::new(id.clone(), parent));
		state.current = id.clone();
		drop(state);
		self
			.notify_session_start()
			.map_err(CommandError::transport)?;
		Ok(json!({ "sessionId": id }))
	}

	fn state_value(&self) -> Value {
		let state = self.state.lock();
		let session = state.current_session();
		json!({
			"model": state.config.model,
			"provider": state.config.provider,
			"thinkingLevel": state.config.thinking_level,
			"isStreaming": state.active.is_some(),
			"isCompacting": false,
			"fastMode": state.config.fast_mode,
			"steeringMode": state.config.steering_mode,
			"followUpMode": state.config.follow_up_mode,
			"interruptMode": state.config.interrupt_mode,
			"autoCompaction": state.config.auto_compaction,
			"autoRetry": state.config.auto_retry,
			"session": { "id": session.id, "name": session.name, "parentSession": session.parent },
			"messageCount": session.messages.len(),
			"tokensPerSecond": Value::Null,
			"todos": state.config.todos,
			"project": state.project,
		})
	}

	fn set_string_config(&self, key: &str, value: &str) -> Result<Value, CommandError> {
		let mut state = self.state.lock();
		match key {
			"model" => {
				if !state.models.iter().any(|candidate| candidate == value) {
					return Err(CommandError::new(
						"model_not_found",
						format!("unknown model `{value}`"),
					));
				}
				state.config.model = value.to_owned();
			},
			"thinkingLevel" => state.config.thinking_level = value.to_owned(),
			"steeringMode" => state.config.steering_mode = value.to_owned(),
			"followUpMode" => state.config.follow_up_mode = value.to_owned(),
			"interruptMode" => state.config.interrupt_mode = value.to_owned(),
			_ => return Err(CommandError::new("invalid_params", "unknown configuration key")),
		}
		let config = serde_json::to_value(&state.config).map_err(CommandError::json)?;
		drop(state);
		self
			.notify(json!({ "type": "config_update", "config": config }))
			.map_err(CommandError::transport)?;
		Ok(json!({ "key": key, "value": value }))
	}

	fn set_model(&self, provider: &str, model_id: &str) -> Result<Value, CommandError> {
		if provider.is_empty() || model_id.is_empty() {
			return Err(CommandError::new("invalid_params", "provider and modelId must not be empty"));
		}
		let key = if model_id.starts_with(&format!("{provider}/")) {
			model_id.to_owned()
		} else {
			format!("{provider}/{model_id}")
		};
		let mut state = self.state.lock();
		if !state
			.providers
			.iter()
			.any(|candidate| candidate.id == provider)
		{
			return Err(CommandError::new(
				"provider_not_found",
				format!("unknown provider `{provider}`"),
			));
		}
		if !state.models.iter().any(|candidate| candidate == &key) {
			return Err(CommandError::new("model_not_found", format!("unknown model `{key}`")));
		}
		state.config.model = key.clone();
		state.config.provider = Some(provider.to_owned());
		let config = serde_json::to_value(&state.config).map_err(CommandError::json)?;
		drop(state);
		self
			.notify(json!({ "type": "config_update", "config": config }))
			.map_err(CommandError::transport)?;
		Ok(json!({ "provider": provider, "modelId": model_id, "model": key }))
	}

	fn set_bool_config(&self, key: &str, value: bool) -> Result<Value, CommandError> {
		let mut state = self.state.lock();
		match key {
			"fastMode" => state.config.fast_mode = value,
			"autoCompaction" => state.config.auto_compaction = value,
			"autoRetry" => state.config.auto_retry = value,
			_ => return Err(CommandError::new("invalid_params", "unknown configuration key")),
		}
		let config = serde_json::to_value(&state.config).map_err(CommandError::json)?;
		drop(state);
		self
			.notify(json!({ "type": "config_update", "config": config }))
			.map_err(CommandError::transport)?;
		Ok(json!({ "enabled": value, "active": value }))
	}

	fn cycle_model(&self) -> Result<Value, CommandError> {
		let next = {
			let state = self.state.lock();
			let index = state
				.models
				.iter()
				.position(|model| model == &state.config.model)
				.unwrap_or(0);
			state
				.models
				.get((index + 1) % state.models.len().max(1))
				.cloned()
		}
		.ok_or_else(|| CommandError::new("model_not_found", "no models are available"))?;
		self.set_string_config("model", &next)
	}

	fn cycle_thinking(&self) -> Result<Value, CommandError> {
		const LEVELS: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh"];
		let next = {
			let state = self.state.lock();
			let index = LEVELS
				.iter()
				.position(|level| *level == state.config.thinking_level)
				.unwrap_or(0);
			LEVELS[(index + 1) % LEVELS.len()]
		};
		self.set_string_config("thinkingLevel", next)
	}

	fn set_todos(&self, params: &Map<String, Value>) -> Result<Value, CommandError> {
		let phases = params
			.get("phases")
			.and_then(Value::as_array)
			.cloned()
			.ok_or_else(|| CommandError::new("invalid_params", "phases must be an array"))?;
		self.state.lock().config.todos = phases.clone();
		self
			.notify(json!({ "type": "config_update", "todos": phases }))
			.map_err(CommandError::transport)?;
		Ok(json!({ "phases": phases }))
	}

	fn compact(&self) -> Result<Value, CommandError> {
		let mut state = self.state.lock();
		if state.active.is_some() {
			return Err(CommandError::new(
				"session_busy",
				"cannot compact while an agent turn is active",
			));
		}
		let session = state.current_mut();
		let removed = session.messages.len().saturating_sub(32);
		if removed > 0 {
			session.messages.drain(..removed);
			session.bump_revision();
		}
		Ok(json!({ "compacted": removed > 0, "removedMessages": removed }))
	}

	fn session_stats(&self) -> Result<Value, CommandError> {
		let state = self.state.lock();
		let session = state.current_session();
		let bytes = serde_json::to_vec(&session.messages)
			.map_err(CommandError::json)?
			.len();
		Ok(json!({
			"sessionId": session.id,
			"name": session.name,
			"messageCount": session.messages.len(),
			"transcriptBytes": bytes,
			"createdAt": session.created_at,
			"updatedAt": session.updated_at,
		}))
	}

	fn switch_session(&self, session_path: &str) -> Result<Value, CommandError> {
		let mut state = self.state.lock();
		if state.active.is_some() {
			return Err(CommandError::new(
				"session_busy",
				"cannot switch sessions during an active turn",
			));
		}
		let id = if state.sessions.contains_key(session_path) {
			session_path.to_owned()
		} else {
			Path::new(session_path)
				.file_stem()
				.and_then(|stem| stem.to_str())
				.filter(|id| state.sessions.contains_key(*id))
				.map(str::to_owned)
				.ok_or_else(|| {
					CommandError::new("session_not_found", format!("unknown session `{session_path}`"))
				})?
		};
		state.current = id.clone();
		drop(state);
		self
			.notify_session_info()
			.map_err(CommandError::transport)?;
		Ok(json!({ "sessionId": id, "sessionPath": session_path }))
	}

	fn branch(&self, params: &Map<String, Value>) -> Result<Value, CommandError> {
		let mut state = self.state.lock();
		if state.active.is_some() {
			return Err(CommandError::new("session_busy", "cannot branch during an active turn"));
		}
		let entry_id = text(params, "entryId")?;
		let source = state.current_session();
		let count = source
			.messages
			.iter()
			.position(|message| message.id == entry_id)
			.map(|index| index + 1)
			.ok_or_else(|| {
				CommandError::new("entry_not_found", format!("unknown entry `{entry_id}`"))
			})?;
		let id = new_id("session");
		let mut branch = Session::new(id.clone(), Some(source.id.clone()));
		branch.messages = source.messages[..count].to_vec();
		branch.bump_revision();
		state.sessions.insert(id.clone(), branch);
		state.current = id.clone();
		drop(state);
		self
			.notify_session_start()
			.map_err(CommandError::transport)?;
		Ok(json!({ "sessionId": id, "messageCount": count }))
	}

	fn get_messages(&self, params: &Map<String, Value>) -> Result<Value, CommandError> {
		let state = self.state.lock();
		let session = session_from_params(&state, params)?;
		Ok(json!({ "sessionId": session.id, "messages": session.messages }))
	}

	fn get_messages_page(&self, params: &Map<String, Value>) -> Result<Value, CommandError> {
		let state = self.state.lock();
		if state.active.is_some() {
			return Err(CommandError::new(
				"session_busy",
				"transcript is changing during an active turn",
			));
		}
		let session = session_from_params(&state, params)?;
		let limit = params
			.get("limit")
			.and_then(Value::as_u64)
			.map_or(DEFAULT_PAGE_MESSAGES, |value| usize::try_from(value).unwrap_or(MAX_PAGE_MESSAGES))
			.clamp(1, MAX_PAGE_MESSAGES);
		let cursor = params.get("cursor").and_then(Value::as_str);
		message_page(session, cursor, limit)
	}

	fn last_assistant(&self) -> Result<Value, CommandError> {
		let state = self.state.lock();
		let text = state
			.current_session()
			.messages
			.iter()
			.rev()
			.find(|message| message.role == "assistant")
			.map(|message| message.content.clone());
		Ok(json!({ "text": text }))
	}

	fn rename_session(&self, name: &str) -> Result<Value, CommandError> {
		if name.trim().is_empty() {
			return Err(CommandError::new("invalid_params", "session name must not be empty"));
		}
		let id = {
			let mut state = self.state.lock();
			let session = state.current_mut();
			session.name = Some(name.to_owned());
			session.id.clone()
		};
		self
			.notify_session_info()
			.map_err(CommandError::transport)?;
		Ok(json!({ "sessionId": id, "name": name }))
	}

	fn handoff(&self, params: &Map<String, Value>) -> Result<Value, CommandError> {
		let instructions = params
			.get("customInstructions")
			.and_then(Value::as_str)
			.unwrap_or("Continue this session from the retained context.");
		let mut state = self.state.lock();
		if state.active.is_some() {
			return Err(CommandError::new("session_busy", "cannot hand off during an active turn"));
		}
		let source = state.current_session();
		let id = new_id("session");
		let mut target = Session::new(id.clone(), Some(source.id.clone()));
		target.messages = source.messages.clone();
		target.push_message("user", instructions);
		state.sessions.insert(id.clone(), target);
		state.current = id.clone();
		drop(state);
		self
			.notify_session_start()
			.map_err(CommandError::transport)?;
		Ok(json!({ "sessionId": id }))
	}

	fn export_html(&self) -> Result<Value, CommandError> {
		let state = self.state.lock();
		let session = state.current_session();
		let mut html =
			String::from("<!doctype html><meta charset=\"utf-8\"><title>OMP transcript</title><main>");
		for message in &session.messages {
			html.push_str("<article data-role=\"");
			html.push_str(&escape_html(&message.role));
			html.push_str("\"><pre>");
			html.push_str(&escape_html(&message.content));
			html.push_str("</pre></article>");
		}
		html.push_str("</main>");
		Ok(json!({ "sessionId": session.id, "html": html }))
	}

	fn login_providers(&self) -> Result<Value, CommandError> {
		let state = self.state.lock();
		Ok(oauth_providers_value(&state.providers))
	}

	async fn handle_login(self: &Arc<Self>, request: RpcRequest) -> miette::Result<()> {
		let params = match parse_params::<LoginParams>(&request.params) {
			Ok(params) => params,
			Err(error) => {
				return self.send_error(request.id, "login", error.code, error.message);
			},
		};
		if !self
			.state
			.lock()
			.providers
			.iter()
			.any(|candidate| candidate.id == params.provider_id)
		{
			return self.send_error(
				request.id,
				"login",
				"provider_not_found",
				format!("unknown provider `{}`", params.provider_id),
			);
		}
		let provider = ProviderId::from(params.provider_id.as_str());
		let method = params.method.map(auth_method);
		let answer = self
			.auth
			.execute(AuthRequest::Login(LoginRequest { provider, method }))
			.await;
		let session = match answer {
			Ok(AuthAnswer::Session(session)) => session,
			Ok(_) => {
				return self.send_error(
					request.id,
					"login",
					"invalid_auth_answer",
					"authentication manager returned a non-session login answer",
				);
			},
			Err(error) => {
				return self.send_error(request.id, "login", "auth_failed", error.to_string());
			},
		};
		let session_id = session.id.as_str().to_owned();
		{
			let mut state = self.state.lock();
			if state.pending_auth.contains_key(&session_id) {
				return self.send_error(
					request.id,
					"login",
					"auth_session_exists",
					"authentication session is already active",
				);
			}
			state.pending_auth.insert(session_id.clone(), PendingAuth {
				cancellation: CancellationToken::new(),
				prompt:       None,
			});
		}
		self.send_success(
			request.id,
			"login",
			json!({
				"providerId": params.provider_id,
				"sessionId": session_id,
				"outcome": "started",
			}),
		)?;
		self.start_auth_forwarder(session);
		Ok(())
	}

	fn start_auth_forwarder(self: &Arc<Self>, session: AuthSession) {
		let session_id = session.id.as_str().to_owned();
		let cancellation = self
			.state
			.lock()
			.pending_auth
			.get(&session_id)
			.map(|pending| pending.cancellation.clone())
			.expect("authentication session inserted before forwarding");
		let runtime = self.clone();
		self.shutdown.spawn(async move {
			loop {
				let event = tokio::select! {
					() = cancellation.cancelled() => {
						let _ = runtime.auth.execute(AuthRequest::Submit {
							session: LoginSessionId::from(session_id.as_str()),
							input: AuthInput::Cancel,
						}).await;
						let _ = runtime.notify_auth_terminal(
							&session_id,
							RpcAuthTerminalOutcome::Cancelled,
							None,
							None,
						);
						break;
					},
					event = session.events.recv_async() => event,
				};
				let event = match event {
					Ok(Ok(event)) => event,
					Ok(Err(error)) => {
						let _ = runtime.notify_auth_terminal(
							&session_id,
							RpcAuthTerminalOutcome::Failed,
							None,
							Some(error.to_string()),
						);
						break;
					},
					Err(_) => {
						let _ = runtime.notify_auth_terminal(
							&session_id,
							RpcAuthTerminalOutcome::Failed,
							None,
							Some("authentication event stream closed before completion".into()),
						);
						break;
					},
				};
				match event {
					AuthEvent::OpenUrl(url) => {
						let _ = runtime.notify_auth_event(&session_id, RpcAuthEvent::OpenUrl {
							url: url.to_string(),
						});
					},
					AuthEvent::ShowDeviceCode { code, verification_url } => {
						let _ = runtime.notify_auth_event(&session_id, RpcAuthEvent::DeviceCode {
							code:             code.expose_secret().to_owned(),
							verification_url: verification_url.to_string(),
						});
					},
					AuthEvent::Prompt(prompt) => {
						let input = auth_prompt_kind(prompt.input);
						if let Some(pending) = runtime.state.lock().pending_auth.get_mut(&session_id) {
							pending.prompt = Some((prompt.id.to_string(), input));
						}
						let _ = runtime.notify_auth_event(&session_id, RpcAuthEvent::Prompt {
							prompt_id: prompt.id.to_string(),
							message: prompt.message.to_string(),
							input,
						});
					},
					AuthEvent::Waiting => {
						let _ = runtime.notify_auth_event(&session_id, RpcAuthEvent::Waiting);
					},
					AuthEvent::Complete(account) => {
						if let Some(provider) = runtime
							.state
							.lock()
							.providers
							.iter_mut()
							.find(|provider| provider.id == account.provider.as_str())
						{
							provider.authenticated = true;
						}
						let _ = runtime.notify_auth_terminal(
							&session_id,
							RpcAuthTerminalOutcome::Completed,
							Some(auth_account(account)),
							None,
						);
						break;
					},
				}
			}
			runtime.state.lock().pending_auth.remove(&session_id);
		});
	}

	async fn answer_auth(&self, answer: RpcAuthAnswerFrame) -> Result<(), CommandError> {
		let expected = self
			.state
			.lock()
			.pending_auth
			.get(&answer.session_id)
			.map(|pending| pending.prompt.clone())
			.ok_or_else(|| {
				CommandError::new("auth_session_not_found", "authentication session is not active")
			})?;
		if answer.input != RpcAuthInputKind::Cancel {
			let (prompt_id, prompt_kind) = expected.ok_or_else(|| {
				CommandError::new(
					"auth_prompt_not_pending",
					"authentication prompt is not awaiting input",
				)
			})?;
			if answer.prompt_id.as_deref() != Some(prompt_id.as_str()) {
				return Err(CommandError::new(
					"stale_auth_prompt",
					"authentication answer does not match the pending prompt",
				));
			}
			if !auth_input_matches(answer.input, prompt_kind) {
				return Err(CommandError::new(
					"invalid_auth_input",
					"authentication answer kind does not match the pending prompt",
				));
			}
		}
		let input = rpc_auth_input(answer.input, answer.value)?;
		self
			.auth
			.execute(AuthRequest::Submit {
				session: LoginSessionId::from(answer.session_id.as_str()),
				input,
			})
			.await
			.map_err(|error| CommandError::new("auth_failed", error.to_string()))?;
		if answer.input == RpcAuthInputKind::Cancel {
			if let Some(pending) = self.state.lock().pending_auth.get(&answer.session_id) {
				pending.cancellation.cancel();
			}
		} else if let Some(pending) = self.state.lock().pending_auth.get_mut(&answer.session_id) {
			pending.prompt = None;
		}
		Ok(())
	}

	fn notify_auth_event(&self, session_id: &str, event: RpcAuthEvent) -> miette::Result<()> {
		let frame =
			RpcAuthEventFrame { kind: "auth_event".into(), session_id: session_id.to_owned(), event };
		self.notify(serde_json::to_value(frame).into_diagnostic()?)
	}

	fn notify_auth_terminal(
		&self,
		session_id: &str,
		outcome: RpcAuthTerminalOutcome,
		account: Option<RpcAuthAccount>,
		error: Option<String>,
	) -> miette::Result<()> {
		let frame = RpcAuthTerminalFrame {
			kind: "auth_terminal".into(),
			session_id: session_id.to_owned(),
			outcome,
			account,
			code: error.as_ref().map(|_| RpcErrorCode::new("auth_failed")),
			error,
		};
		self.notify(serde_json::to_value(frame).into_diagnostic()?)
	}

	fn set_host_tools(&self, params: &Map<String, Value>) -> Result<Value, CommandError> {
		let tools = params
			.get("tools")
			.and_then(Value::as_array)
			.ok_or_else(|| CommandError::new("invalid_params", "tools must be an array"))?;
		let mut parsed = BTreeMap::new();
		for tool in tools {
			let definition = serde_json::from_value::<HostToolDefinition>(tool.clone())
				.map_err(|error| CommandError::new("invalid_params", error.to_string()))?;
			if !definition.parameters.is_object() {
				return Err(CommandError::new(
					"invalid_params",
					"host tool parameters must be JSON Schema objects",
				));
			}
			parsed.insert(definition.name, tool.clone());
		}
		let tool_names = parsed.keys().cloned().collect::<Vec<_>>();
		self.state.lock().host_tools = parsed;
		Ok(host_tool_names_value(tool_names))
	}

	fn call_host_tool(&self, params: &Map<String, Value>) -> Result<Value, CommandError> {
		let name = text(params, "name")?;
		let arguments = params
			.get("arguments")
			.cloned()
			.unwrap_or_else(|| json!({}))
			.as_object()
			.cloned()
			.ok_or_else(|| CommandError::new("invalid_params", "arguments must be an object"))?;
		let invocation_id = new_id("host-tool");
		let tool_call_id = params
			.get("toolCallId")
			.and_then(Value::as_str)
			.map_or_else(|| new_id("call"), str::to_owned);
		let subagent_started = {
			let mut state = self.state.lock();
			if !state.host_tools.contains_key(name) {
				return Err(CommandError::new(
					"host_tool_not_found",
					format!("host tool `{name}` is not registered"),
				));
			}
			let subagent_id = matches!(name, "task" | "agent").then(|| invocation_id.clone());
			let started = subagent_id.as_ref().map(|id| {
				let snapshot = SubagentSnapshot {
					id:              id.clone(),
					index:           u64::try_from(state.subagents.len()).unwrap_or(u64::MAX),
					status:          "running".into(),
					task:            arguments
						.get("task")
						.and_then(Value::as_str)
						.map(str::to_owned),
					assignment:      arguments
						.get("prompt")
						.or_else(|| arguments.get("assignment"))
						.and_then(Value::as_str)
						.map(str::to_owned),
					progress:        None,
					transcript_path: arguments
						.get("sessionFile")
						.and_then(Value::as_str)
						.map(PathBuf::from),
				};
				state.subagents.insert(id.clone(), snapshot.clone());
				(!matches!(state.subscription, Subscription::Off))
					.then(|| json!({"type":"subagent_lifecycle","event":"started","subagent":snapshot}))
			});
			state
				.pending_host_tools
				.insert(invocation_id.clone(), PendingHostTool {
					name: name.to_owned(),
					updates: Vec::new(),
					subagent_id,
				});
			started.flatten()
		};
		let frame = HostToolCall {
			kind: "host_tool_call".into(),
			id: invocation_id.clone(),
			tool_call_id: tool_call_id.clone(),
			tool_name: name.to_owned(),
			arguments,
		};
		self
			.notify(serde_json::to_value(frame).map_err(CommandError::json)?)
			.map_err(CommandError::transport)?;
		if let Some(started) = subagent_started {
			self.notify(started).map_err(CommandError::transport)?;
		}
		Ok(json!({ "id": invocation_id, "toolCallId": tool_call_id }))
	}

	fn handle_side_channel(&self, value: Value) -> miette::Result<()> {
		let kind = value
			.get("type")
			.and_then(Value::as_str)
			.unwrap_or_default()
			.to_owned();
		let parsed = match kind.as_str() {
			"host_tool_update" => {
				serde_json::from_value::<HostToolUpdate>(value).map(HostSideChannel::Update)
			},
			"host_tool_result" => {
				serde_json::from_value::<HostToolResult>(value).map(HostSideChannel::Result)
			},
			"host_tool_cancel" => {
				serde_json::from_value::<HostToolCancel>(value).map(HostSideChannel::Cancel)
			},
			_ => unreachable!("immediate frame classifier only admits host tool frames"),
		};
		let frame = match parsed {
			Ok(frame) => frame,
			Err(error) => {
				return self.send_error(None, &kind, "invalid_request", error.to_string());
			},
		};
		let (event, subagent_event) = {
			let mut state = self.state.lock();
			match frame {
				HostSideChannel::Update(update) => {
					let Some(pending) = state.pending_host_tools.get_mut(&update.id) else {
						drop(state);
						return self.send_error(
							Some(RequestId::new(update.id.clone())),
							&kind,
							"host_tool_not_pending",
							format!("host tool invocation `{}` is not pending", update.id),
						);
					};
					pending.updates.push(update.partial_result.clone());
					let name = pending.name.clone();
					let subagent_id = pending.subagent_id.clone();
					let subscription = state.subscription;
					let subagent_event = subagent_id.and_then(|id| {
						let snapshot = state.subagents.get_mut(&id)?;
						snapshot.progress = Some(update.partial_result.clone());
						match subscription {
							Subscription::Off => None,
							Subscription::Progress => Some(json!({
								"type":"subagent_progress",
								"subagentId":id,
								"progress":update.partial_result,
								"snapshot":snapshot,
							})),
							Subscription::Events => Some(json!({
								"type":"subagent_event",
								"subagentId":id,
								"event":update.partial_result,
								"snapshot":snapshot,
							})),
						}
					});
					(
						json!({
							"type": "host_tool_progress",
							"invocationId": update.id,
							"name": name,
							"update": update.partial_result,
						}),
						subagent_event,
					)
				},
				HostSideChannel::Result(result) => {
					let Some(pending) = state.pending_host_tools.remove(&result.id) else {
						drop(state);
						return self.send_error(
							Some(RequestId::new(result.id.clone())),
							&kind,
							"host_tool_not_pending",
							format!("host tool invocation `{}` is not pending", result.id),
						);
					};
					let subscription = state.subscription;
					let subagent_event = pending.subagent_id.and_then(|id| {
						let snapshot = state.subagents.get_mut(&id)?;
						snapshot.status = if result.is_error {
							"failed".into()
						} else {
							"completed".into()
						};
						(!matches!(subscription, Subscription::Off)).then(|| {
							json!({
								"type":"subagent_lifecycle",
								"event":snapshot.status,
								"subagent":snapshot,
								"result":result.result.clone(),
							})
						})
					});
					(
						json!({
							"type": "host_tool_complete",
							"invocationId": result.id,
							"name": pending.name,
							"updates": pending.updates,
							"result": result.result,
							"isError": result.is_error,
						}),
						subagent_event,
					)
				},
				HostSideChannel::Cancel(cancel) => {
					let Some(pending) = state.pending_host_tools.remove(&cancel.target_id) else {
						drop(state);
						return self.send_error(
							Some(RequestId::new(cancel.id.clone())),
							&kind,
							"host_tool_not_pending",
							format!("host tool invocation `{}` is not pending", cancel.target_id),
						);
					};
					let subscription = state.subscription;
					let subagent_event = pending.subagent_id.and_then(|id| {
						let snapshot = state.subagents.get_mut(&id)?;
						snapshot.status = "cancelled".into();
						(!matches!(subscription, Subscription::Off)).then(|| {
							json!({
								"type":"subagent_lifecycle",
								"event":"cancelled",
								"subagent":snapshot,
							})
						})
					});
					(
						json!({
							"type": "host_tool_cancelled",
							"invocationId": cancel.target_id,
							"name": pending.name,
						}),
						subagent_event,
					)
				},
			}
		};
		self.notify(event)?;
		if let Some(event) = subagent_event {
			self.notify(event)?;
		}
		Ok(())
	}

	fn set_subscription(&self, params: &Map<String, Value>) -> Result<Value, CommandError> {
		let level = text(params, "level")?;
		let subscription = match level {
			"off" => Subscription::Off,
			"progress" => Subscription::Progress,
			"events" => Subscription::Events,
			_ => {
				return Err(CommandError::new(
					"invalid_params",
					"subscription must be off, progress, or events",
				));
			},
		};
		self.state.lock().subscription = subscription;
		Ok(json!({ "level": level }))
	}

	fn get_subagents(&self) -> Result<Value, CommandError> {
		let state = self.state.lock();
		let mut snapshots = state.subagents.values().cloned().collect::<Vec<_>>();
		snapshots.sort_by(|left, right| {
			left
				.index
				.cmp(&right.index)
				.then_with(|| left.id.cmp(&right.id))
		});
		Ok(json!({ "subscription": state.subscription.as_str(), "subagents": snapshots }))
	}

	async fn get_subagent_messages(
		&self,
		params: &Map<String, Value>,
	) -> Result<Value, CommandError> {
		let params = parse_params::<GetSubagentMessagesParams>(params)?;
		let requested_from = params.from_byte.unwrap_or(0);
		let (root, relative) = {
			let state = self.state.lock();
			let root = state.session_dir.clone().ok_or_else(|| {
				CommandError::new("unsupported_operation", "no --session-dir was configured")
			})?;
			let relative = if let Some(session_file) = params.session_file {
				PathBuf::from(session_file)
			} else {
				let id = params.subagent_id.ok_or_else(|| {
					CommandError::new("invalid_params", "subagentId or sessionFile is required")
				})?;
				state
					.subagents
					.get(&id)
					.ok_or_else(|| {
						CommandError::new("subagent_not_found", format!("unknown subagent `{id}`"))
					})?
					.transcript_path
					.clone()
					.ok_or_else(|| {
						CommandError::new("transcript_unavailable", "subagent has no transcript path")
					})?
			};
			(root, relative)
		};
		let root = tokio::fs::canonicalize(root)
			.await
			.map_err(CommandError::io)?;
		let path = tokio::fs::canonicalize(root.join(&relative))
			.await
			.map_err(CommandError::io)?;
		if !path.starts_with(&root) {
			return Err(CommandError::new(
				"invalid_params",
				"subagent transcript escapes --session-dir",
			));
		}
		let session_file = relative.to_string_lossy().into_owned();
		let previous_length = {
			let state = self.state.lock();
			state
				.transcript_lru
				.iter()
				.find(|(entry, _)| entry == &session_file)
				.map(|(_, length)| *length)
		};
		let bytes = tokio::fs::read(&path).await.map_err(CommandError::io)?;
		let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
		let reset =
			requested_from > length || previous_length.is_some_and(|previous| length < previous);
		let from_byte = if reset { 0 } else { requested_from };
		let start = usize::try_from(from_byte)
			.unwrap_or(bytes.len())
			.min(bytes.len());
		let limit = start.saturating_add(SUBAGENT_READ_BYTES).min(bytes.len());
		let end = if limit < bytes.len() {
			bytes[start..limit]
				.iter()
				.rposition(|byte| *byte == b'\n')
				.map_or(limit, |offset| start + offset + 1)
		} else {
			limit
		};
		let (entries, messages) = decode_transcript_entries(&bytes[start..end]);
		{
			let mut state = self.state.lock();
			state.touch_transcript(&session_file, length);
		}
		let result = SubagentMessages {
			session_file,
			from_byte,
			next_byte: u64::try_from(end).unwrap_or(u64::MAX),
			reset,
			entries,
			messages,
		};
		serde_json::to_value(result).map_err(CommandError::json)
	}

	fn notify_session_start(&self) -> miette::Result<()> {
		let state = self.state.lock();
		let session = state.current_session();
		self.notify(json!({ "type": "session_start", "sessionId": session.id, "name": session.name }))
	}

	fn notify_session_info(&self) -> miette::Result<()> {
		let state = self.state.lock();
		let session = state.current_session();
		self.notify(
			json!({ "type": "session_info_update", "sessionId": session.id, "name": session.name }),
		)
	}

	fn notify(&self, value: Value) -> miette::Result<()> {
		emit(&self.output, value)
	}

	fn send_success(&self, id: Option<RequestId>, command: &str, data: Value) -> miette::Result<()> {
		let response = RpcResponse::success(id, command, data).into_diagnostic()?;
		self.notify(serde_json::to_value(response).into_diagnostic()?)
	}

	fn send_error(
		&self,
		id: Option<RequestId>,
		command: &str,
		code: impl Into<String>,
		message: impl Into<String>,
	) -> miette::Result<()> {
		let response = RpcResponse::error(id, command, message, Some(RpcErrorCode::new(code)));
		self.notify(serde_json::to_value(response).into_diagnostic()?)
	}
}

struct RpcCommandHost {
	runtime: Arc<Runtime>,
	roster:  CommandRoster,
}

fn command_status<'a>(status: impl Into<Str>) -> CommandFuture<'a> {
	let status = status.into();
	Box::pin(async move { Ok(CommandResult::Consumed(ConsumedResult::status(status))) })
}

fn unavailable_command<'a>(name: &'static str) -> CommandFuture<'a> {
	Box::pin(async move { Err(miette!("command /{name} is unavailable in this RPC session")) })
}

impl ShellCommandHost for RpcCommandHost {
	fn help(&mut self) -> CommandFuture<'_> {
		use crate::chat_ui::commands::{CommandCapability, CommandRole, CommandSurface};
		command_status(self.roster.help_text(
			CommandSurface::Text,
			CommandRole::Owner,
			true,
			|capability| matches!(capability, CommandCapability::Session),
		))
	}

	fn new_session(&mut self) -> CommandFuture<'_> {
		let runtime = self.runtime.clone();
		Box::pin(async move {
			runtime
				.new_session(&Map::new())
				.map_err(|error| miette!(error.message))?;
			Ok(CommandResult::Consumed(ConsumedResult::status("Started a new session.")))
		})
	}

	fn jobs(&mut self) -> CommandFuture<'_> {
		unavailable_command("jobs")
	}

	fn agents(&mut self) -> CommandFuture<'_> {
		unavailable_command("agents")
	}

	fn pause(&mut self) -> CommandFuture<'_> {
		unavailable_command("pause")
	}

	fn quit(&mut self) -> CommandFuture<'_> {
		Box::pin(async { Ok(CommandResult::Exit) })
	}
}

impl SessionCommandHost for RpcCommandHost {
	fn clear(&mut self) -> CommandFuture<'_> {
		let runtime = self.runtime.clone();
		Box::pin(async move {
			runtime.state.lock().current_mut().messages.clear();
			Ok(CommandResult::Consumed(ConsumedResult::status("Session context cleared.")))
		})
	}

	fn git(&mut self, _revision: Option<Str>) -> CommandFuture<'_> {
		unavailable_command("git")
	}

	fn fresh(&mut self) -> CommandFuture<'_> {
		self.clear()
	}

	fn rename(&mut self, title: Str) -> CommandFuture<'_> {
		let runtime = self.runtime.clone();
		Box::pin(async move {
			runtime
				.rename_session(title.as_str())
				.map_err(|error| miette!(error.message))?;
			Ok(CommandResult::Consumed(ConsumedResult::status("Session renamed.")))
		})
	}

	fn retry(&mut self) -> CommandFuture<'_> {
		let runtime = self.runtime.clone();
		Box::pin(async move {
			let prompt = runtime
				.state
				.lock()
				.current_session()
				.messages
				.iter()
				.rev()
				.find(|message| message.role == "user")
				.map(|message| message.content.clone());
			let Some(prompt) = prompt else {
				return Ok(CommandResult::Consumed(ConsumedResult::status("Nothing to retry.")));
			};
			runtime
				.submit_prompt(prompt, "prompt")
				.map_err(|error| miette!(error.message))?;
			Ok(CommandResult::Consumed(ConsumedResult::agent("Retry started.")))
		})
	}

	fn resume(&mut self, selector: Option<Str>) -> CommandFuture<'_> {
		let Some(selector) = selector else {
			return unavailable_command("resume");
		};
		let runtime = self.runtime.clone();
		Box::pin(async move {
			runtime
				.switch_session(selector.as_str())
				.map_err(|error| miette!(error.message))?;
			Ok(CommandResult::Consumed(ConsumedResult::status("Session resumed.")))
		})
	}

	fn session(&mut self, request: SessionRequest) -> CommandFuture<'_> {
		use crate::chat_ui::commands::SessionRequest;
		if !matches!(request, SessionRequest::Info) {
			return unavailable_command("session");
		}
		let runtime = self.runtime.clone();
		Box::pin(async move {
			let value = runtime
				.session_stats()
				.map_err(|error| miette!(error.message))?;
			command_status(value.to_string()).await
		})
	}

	fn workspace(&mut self, _request: WorkspaceRequest) -> CommandFuture<'_> {
		unavailable_command("workspace")
	}

	fn handoff(&mut self, instructions: Option<Str>) -> CommandFuture<'_> {
		let runtime = self.runtime.clone();
		Box::pin(async move {
			let mut params = Map::new();
			if let Some(instructions) = instructions {
				params.insert("customInstructions".into(), json!(instructions));
			}
			let owner = runtime.clone();
			runtime.shutdown.spawn(async move {
				let status = match owner.handoff(&params) {
					Ok(_) => Str::new_static("Context handed off and compacted in place."),
					Err(error) => sf!("Handoff failed: {}", error.message),
				};
				let _ = owner.notify(json!({
					"type":"command_output",
					"stream":"stdout",
					"content":status,
					"generation":0,
				}));
			});
			Ok(CommandResult::Consumed(ConsumedResult::silent()))
		})
	}
}

impl ModelCommandHost for RpcCommandHost {
	fn model(&mut self, selector: Option<Str>) -> CommandFuture<'_> {
		let Some(selector) = selector else {
			return unavailable_command("model");
		};
		let runtime = self.runtime.clone();
		Box::pin(async move {
			runtime
				.set_string_config("model", selector.as_str())
				.map_err(|error| miette!(error.message))?;
			command_status("Model updated.").await
		})
	}

	fn switch(&mut self, selector: Option<Str>) -> CommandFuture<'_> {
		let Some(selector) = selector else {
			return unavailable_command("switch");
		};
		self.model(Some(selector))
	}
}

impl ConfigCommandHost for RpcCommandHost {
	fn settings(&mut self) -> CommandFuture<'_> {
		unavailable_command("settings")
	}

	fn setup(&mut self) -> CommandFuture<'_> {
		unavailable_command("setup")
	}

	fn providers(&mut self) -> CommandFuture<'_> {
		let providers = oauth_providers_value(&self.runtime.state.lock().providers).to_string();
		command_status(providers)
	}

	fn login(&mut self, _provider: Option<Str>) -> CommandFuture<'_> {
		unavailable_command("login")
	}

	fn logout(&mut self, _provider: Option<Str>) -> CommandFuture<'_> {
		unavailable_command("logout")
	}
}

impl FlowCommandHost for RpcCommandHost {
	fn context(&mut self) -> CommandFuture<'_> {
		let value = self
			.runtime
			.last_assistant()
			.map_or_else(|error| error.message, |value| value.to_string());
		command_status(value)
	}

	fn compact(&mut self, _request: omp_agent::ManualCompactionRequest) -> CommandFuture<'_> {
		let runtime = self.runtime.clone();
		Box::pin(async move {
			runtime.compact().map_err(|error| miette!(error.message))?;
			command_status("Context compacted.").await
		})
	}

	fn shake(&mut self, _args: Str) -> CommandFuture<'_> {
		unavailable_command("shake")
	}

	fn usage(&mut self, _args: Str) -> CommandFuture<'_> {
		unavailable_command("usage")
	}

	fn stats(&mut self, _flags: ParsedFlags) -> CommandFuture<'_> {
		unavailable_command("stats")
	}

	fn plan(&mut self, _args: Str) -> CommandFuture<'_> {
		unavailable_command("plan")
	}

	fn vibe(&mut self, _args: Str) -> CommandFuture<'_> {
		unavailable_command("vibe")
	}

	fn todo(&mut self, _args: Str) -> CommandFuture<'_> {
		unavailable_command("todo")
	}

	fn plan_review(&mut self, _args: Str) -> CommandFuture<'_> {
		unavailable_command("plan-review")
	}

	fn guided_goal(&mut self, _args: Str) -> CommandFuture<'_> {
		unavailable_command("guided-goal")
	}

	fn loop_command(&mut self, _args: Str) -> CommandFuture<'_> {
		unavailable_command("loop")
	}

	fn queue(&mut self, prompt: Str) -> CommandFuture<'_> {
		let runtime = self.runtime.clone();
		Box::pin(async move {
			runtime
				.submit_prompt(prompt.to_string(), "followUp")
				.map_err(|error| miette!(error.message))?;
			command_status("Prompt queued.").await
		})
	}

	fn force(&mut self, _tool: Str) -> CommandFuture<'_> {
		unavailable_command("force")
	}

	fn fast(&mut self, args: Str) -> CommandFuture<'_> {
		let enabled = matches!(args.trim().as_str(), "" | "on" | "true");
		let runtime = self.runtime.clone();
		Box::pin(async move {
			runtime
				.set_bool_config("fastMode", enabled)
				.map_err(|error| miette!(error.message))?;
			command_status(if enabled {
				"Fast mode enabled."
			} else {
				"Fast mode disabled."
			})
			.await
		})
	}

	fn prewalk(&mut self, _args: Str) -> CommandFuture<'_> {
		unavailable_command("prewalk")
	}

	fn btw(&mut self, _prompt: Str) -> CommandFuture<'_> {
		unavailable_command("btw")
	}

	fn tan(&mut self, _prompt: Str) -> CommandFuture<'_> {
		unavailable_command("tan")
	}

	fn omfg(&mut self, _instruction: Str) -> CommandFuture<'_> {
		unavailable_command("omfg")
	}

	fn live(&mut self, _args: Str) -> CommandFuture<'_> {
		unavailable_command("live")
	}

	fn mcp(&mut self, _request: McpRequest) -> CommandFuture<'_> {
		unavailable_command("mcp")
	}

	fn memory(&mut self, _args: Str) -> CommandFuture<'_> {
		unavailable_command("memory")
	}
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetModelParams {
	provider: String,
	model_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SwitchSessionParams {
	session_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoginParams {
	provider_id: String,
	#[serde(default)]
	method:      Option<RpcAuthMethod>,
}

#[derive(Debug, Deserialize)]
struct BashParams {
	command: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetSubagentMessagesParams {
	#[serde(default)]
	subagent_id:  Option<String>,
	#[serde(default)]
	session_file: Option<String>,
	#[serde(default)]
	from_byte:    Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigState {
	model:           String,
	provider:        Option<String>,
	thinking_level:  String,
	fast_mode:       bool,
	steering_mode:   String,
	follow_up_mode:  String,
	interrupt_mode:  String,
	auto_compaction: bool,
	auto_retry:      bool,
	todos:           Vec<Value>,
}

enum CommandIntercept {
	Passthrough,
	Prompt(String),
	Consumed(bool),
	Exit,
}

struct ActiveBash {
	id:           String,
	cancellation: CancellationToken,
}

struct PendingAuth {
	cancellation: CancellationToken,
	prompt:       Option<(String, RpcAuthPromptKind)>,
}

enum HostSideChannel {
	Update(HostToolUpdate),
	Result(HostToolResult),
	Cancel(HostToolCancel),
}

struct ServerState {
	current:              String,
	sessions:             HashMap<String, Session>,
	active:               Option<CancellationToken>,
	active_bash:          Option<ActiveBash>,
	queue:                VecDeque<String>,
	config:               ConfigState,
	models:               Vec<String>,
	providers:            Vec<OAuthProvider>,
	project:              PathBuf,
	session_dir:          Option<PathBuf>,
	host_tools:           BTreeMap<String, Value>,
	pending_host_tools:   HashMap<String, PendingHostTool>,
	pending_auth:         HashMap<String, PendingAuth>,
	pending_extension_ui: HashMap<String, oneshot::Sender<ExtensionUiResponse>>,
	subscription:         Subscription,
	command_generation:   u64,
	content:              omp_driver::discovery::ActiveContentSnapshots,
	subagents:            HashMap<String, SubagentSnapshot>,
	transcript_lru:       VecDeque<(String, u64)>,
}

impl ServerState {
	fn new(
		model: String,
		models: Vec<String>,
		providers: Vec<OAuthProvider>,
		preferred_provider: Option<String>,
		project: PathBuf,
		session_dir: Option<PathBuf>,
	) -> Self {
		let id = new_id("session");
		let content = omp_driver::discovery::active_content_snapshots(&project);
		let mut sessions = HashMap::new();
		sessions.insert(id.clone(), Session::new(id.clone(), None));
		Self {
			current: id,
			sessions,
			active: None,
			active_bash: None,
			queue: VecDeque::new(),
			config: ConfigState {
				model,
				provider: preferred_provider,
				thinking_level: "medium".into(),
				fast_mode: false,
				steering_mode: "steer".into(),
				follow_up_mode: "followUp".into(),
				interrupt_mode: "immediate".into(),
				auto_compaction: true,
				auto_retry: true,
				todos: Vec::new(),
			},
			models,
			providers,
			project,
			session_dir,
			host_tools: BTreeMap::new(),
			pending_host_tools: HashMap::new(),
			pending_auth: HashMap::new(),
			pending_extension_ui: HashMap::new(),
			subscription: Subscription::Off,
			command_generation: 1,
			content,
			subagents: HashMap::new(),
			transcript_lru: VecDeque::new(),
		}
	}

	fn current_session(&self) -> &Session {
		self
			.sessions
			.get(&self.current)
			.expect("current session is retained")
	}

	fn current_mut(&mut self) -> &mut Session {
		self
			.sessions
			.get_mut(&self.current)
			.expect("current session is retained")
	}

	fn touch_transcript(&mut self, id: &str, length: u64) {
		self.transcript_lru.retain(|(entry, _)| entry != id);
		self.transcript_lru.push_back((id.to_owned(), length));
		while self.transcript_lru.len() > MAX_SUBAGENT_TRANSCRIPTS {
			self.transcript_lru.pop_front();
		}
	}
}

struct PendingHostTool {
	name:        String,
	updates:     Vec<Value>,
	subagent_id: Option<String>,
}

#[derive(Clone, Copy)]
enum Subscription {
	Off,
	Progress,
	Events,
}

impl Subscription {
	const fn as_str(self) -> &'static str {
		match self {
			Self::Off => "off",
			Self::Progress => "progress",
			Self::Events => "events",
		}
	}
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SubagentSnapshot {
	id:              String,
	index:           u64,
	status:          String,
	task:            Option<String>,
	assignment:      Option<String>,
	progress:        Option<Value>,
	transcript_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TranscriptMessage {
	id:        String,
	role:      String,
	content:   String,
	timestamp: u64,
}

#[derive(Clone)]
struct Session {
	id:         String,
	name:       Option<String>,
	parent:     Option<String>,
	messages:   Vec<TranscriptMessage>,
	revision:   u64,
	leaf_id:    String,
	created_at: u64,
	updated_at: u64,
}

impl Session {
	fn new(id: String, parent: Option<String>) -> Self {
		let now = unix_millis();
		Self {
			id,
			name: None,
			parent,
			messages: Vec::new(),
			revision: 0,
			leaf_id: new_id("leaf"),
			created_at: now,
			updated_at: now,
		}
	}

	fn push_message(&mut self, role: &str, content: &str) {
		self.messages.push(TranscriptMessage {
			id:        new_id("message"),
			role:      role.to_owned(),
			content:   content.to_owned(),
			timestamp: unix_millis(),
		});
		self.bump_revision();
	}

	fn bump_revision(&mut self) {
		self.revision = self.revision.wrapping_add(1);
		self.leaf_id = new_id("leaf");
		self.updated_at = unix_millis();
	}
}

fn build_request(session: &Session, orchestration: bool) -> ChatRequest {
	let mut messages = Vec::with_capacity(session.messages.len() + usize::from(orchestration));
	if orchestration {
		messages.push(Message {
			role:    Role::System,
			content: Arc::from([ContentPart::Text {
				text:  Str::from(ORCHESTRATE_NOTICE),
				proof: None,
			}]),
			name:    None,
		});
	}
	messages.extend(session.messages.iter().map(|message| Message {
		role:    match message.role.as_str() {
			"assistant" => Role::Assistant,
			"system" => Role::System,
			_ => Role::User,
		},
		content: Arc::from([ContentPart::Text {
			text:  Str::from(message.content.clone()),
			proof: None,
		}]),
		name:    None,
	}));
	ChatRequest {
		messages:          Arc::from(messages),
		tools:             Arc::from([]),
		hosted_tools:      Arc::from([]),
		tool_choice:       Setting::Unset,
		output:            Setting::Unset,
		reasoning:         Setting::Unset,
		verbosity:         Setting::Unset,
		cache_retention:   Setting::Unset,
		service_tier:      Setting::Unset,
		sampling:          Sampling::default(),
		max_output_tokens: None,
		top_logprobs:      None,
		safety:            Arc::from([]),
		negotiation:       NegotiationPolicy::default(),
	}
}

fn session_from_params<'a>(
	state: &'a ServerState,
	params: &Map<String, Value>,
) -> Result<&'a Session, CommandError> {
	let id = params
		.get("sessionId")
		.and_then(Value::as_str)
		.unwrap_or(&state.current);
	state
		.sessions
		.get(id)
		.ok_or_else(|| CommandError::new("session_not_found", format!("unknown session `{id}`")))
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageCursor {
	version:       u8,
	session_id:    String,
	leaf_id:       String,
	message_count: usize,
	revision:      u64,
	offset:        usize,
}

fn message_page(
	session: &Session,
	encoded: Option<&str>,
	limit: usize,
) -> Result<Value, CommandError> {
	let offset = if let Some(encoded) = encoded {
		let cursor: PageCursor = serde_json::from_slice(&decode_base64url(encoded)?)
			.map_err(|_| CommandError::new("stale_cursor", "cursor is invalid"))?;
		if cursor.version != 1
			|| cursor.session_id != session.id
			|| cursor.leaf_id != session.leaf_id
			|| cursor.message_count != session.messages.len()
			|| cursor.revision != session.revision
		{
			return Err(CommandError::new(
				"stale_cursor",
				"transcript changed since the cursor was issued",
			));
		}
		cursor.offset
	} else {
		0
	};
	if offset > session.messages.len() {
		return Err(CommandError::new("stale_cursor", "cursor offset is outside the transcript"));
	}
	let mut messages = Vec::new();
	let mut bytes = 0;
	for message in session.messages.iter().skip(offset).take(limit) {
		let size = serde_json::to_vec(message)
			.map_err(CommandError::json)?
			.len();
		if !messages.is_empty() && bytes + size > MAX_PAGE_BYTES {
			break;
		}
		bytes += size;
		messages.push(message.clone());
	}
	let next_offset = offset + messages.len();
	let cursor = if next_offset < session.messages.len() {
		let cursor = PageCursor {
			version:       1,
			session_id:    session.id.clone(),
			leaf_id:       session.leaf_id.clone(),
			message_count: session.messages.len(),
			revision:      session.revision,
			offset:        next_offset,
		};
		Some(
			omp_core::base64_url::encode_raw(
				&serde_json::to_vec(&cursor).map_err(CommandError::json)?,
			)
			.into_string(),
		)
	} else {
		None
	};
	Ok(
		json!({ "sessionId": session.id, "messages": messages, "nextCursor": cursor, "bytes": bytes }),
	)
}

fn rpc_command_roster(root: &Path, generation: u64) -> CommandRoster {
	use crate::chat_ui::commands::{
		CommandDeclaration, CommandGeneration, CommandImplementation, CommandProvenance,
		CommandSourceKind, CommandSurface, ShadowPolicy,
	};
	let content = omp_driver::discovery::active_content_snapshots(root);
	let generations = content
		.commands
		.iter()
		.filter_map(|command| {
			let template = command.template.clone()?;
			let provenance = CommandProvenance {
				source: sf!("{}:{}", command.origin, command.name),
				label: command.origin.clone(),
				kind: CommandSourceKind::Markdown,
				generation,
			};
			let declaration = CommandDeclaration {
				order:           0,
				name:            command.name.clone(),
				aliases:         command.aliases.iter().cloned().collect::<Vec<_>>().into(),
				description:     command.description.clone(),
				argument_hint:   command.hint.clone(),
				hints:           Arc::from([]),
				capabilities:    Arc::from([]),
				surfaces:        Arc::from([CommandSurface::Text, CommandSurface::Acp]),
				guest_visible:   false,
				acp_description: None,
				provenance:      provenance.clone(),
				implementation:  CommandImplementation::Prompt(template),
			};
			Some(CommandGeneration { provenance, declarations: Arc::from([declaration]) })
		})
		.collect::<Vec<_>>();
	CommandRoster::with_contributions(generations, &ShadowPolicy::default())
}

fn contains_orchestrate(input: &str) -> bool {
	let bytes = input.as_bytes();
	let mut index = 0;
	let mut fenced: Option<u8> = None;
	let mut inline = false;
	let mut tag = false;
	while index < bytes.len() {
		if !inline
			&& !tag
			&& (bytes[index..].starts_with(b"```") || bytes[index..].starts_with(b"~~~"))
		{
			let marker = bytes[index];
			if fenced == Some(marker) {
				fenced = None;
			} else if fenced.is_none() {
				fenced = Some(marker);
			}
			index += 3;
			continue;
		}
		if fenced.is_some() {
			index += 1;
			continue;
		}
		match bytes[index] {
			b'`' if !tag => {
				inline = !inline;
				index += 1;
				continue;
			},
			b'<' if !inline => {
				tag = true;
				index += 1;
				continue;
			},
			b'>' if tag => {
				tag = false;
				index += 1;
				continue;
			},
			_ => {},
		}
		if !inline && !tag && bytes[index..].starts_with(b"orchestrate") {
			let before = index.checked_sub(1).and_then(|at| bytes.get(at)).copied();
			let after = bytes.get(index + "orchestrate".len()).copied();
			if !before.is_some_and(is_word_byte) && !after.is_some_and(is_word_byte) {
				return true;
			}
		}
		index += 1;
	}
	false
}

const fn is_word_byte(byte: u8) -> bool {
	byte.is_ascii_alphanumeric() || byte == b'_'
}

fn auth_method(method: RpcAuthMethod) -> AuthMethod {
	match method {
		RpcAuthMethod::ApiKey => AuthMethod::ApiKey,
		RpcAuthMethod::OauthPkce => AuthMethod::OAuthPkce,
		RpcAuthMethod::OauthDevice => AuthMethod::OAuthDevice,
		RpcAuthMethod::ApplicationDefault => AuthMethod::ApplicationDefault,
		RpcAuthMethod::AwsCredentialChain => AuthMethod::AwsCredentialChain,
		RpcAuthMethod::SessionToken => AuthMethod::SessionToken,
	}
}

fn auth_prompt_kind(kind: AuthPromptKind) -> RpcAuthPromptKind {
	match kind {
		AuthPromptKind::AuthorizationCode => RpcAuthPromptKind::AuthorizationCode,
		AuthPromptKind::ApiKey => RpcAuthPromptKind::ApiKey,
		AuthPromptKind::SessionToken => RpcAuthPromptKind::SessionToken,
		AuthPromptKind::PlainText => RpcAuthPromptKind::PlainText,
		AuthPromptKind::OptionalSecret => RpcAuthPromptKind::OptionalSecret,
		AuthPromptKind::Confirmation => RpcAuthPromptKind::Confirmation,
	}
}

fn auth_input_matches(input: RpcAuthInputKind, prompt: RpcAuthPromptKind) -> bool {
	matches!(
		(input, prompt),
		(RpcAuthInputKind::AuthorizationCode, RpcAuthPromptKind::AuthorizationCode)
			| (RpcAuthInputKind::ApiKey, RpcAuthPromptKind::ApiKey)
			| (RpcAuthInputKind::SessionToken, RpcAuthPromptKind::SessionToken)
			| (RpcAuthInputKind::PlainText, RpcAuthPromptKind::PlainText)
			| (RpcAuthInputKind::OptionalSecret, RpcAuthPromptKind::OptionalSecret)
			| (RpcAuthInputKind::DeviceConfirmed, RpcAuthPromptKind::Confirmation)
			| (RpcAuthInputKind::CallbackUrl, RpcAuthPromptKind::AuthorizationCode)
	)
}

fn rpc_auth_input(
	kind: RpcAuthInputKind,
	value: Option<String>,
) -> Result<AuthInput, CommandError> {
	let required = |value: Option<String>| {
		value.ok_or_else(|| {
			CommandError::new("invalid_auth_input", "authentication input requires a value")
		})
	};
	match kind {
		RpcAuthInputKind::AuthorizationCode => {
			Ok(AuthInput::AuthorizationCode(SecretString::from(required(value)?)))
		},
		RpcAuthInputKind::ApiKey => Ok(AuthInput::ApiKey(SecretString::from(required(value)?))),
		RpcAuthInputKind::SessionToken => {
			Ok(AuthInput::SessionToken(SecretString::from(required(value)?)))
		},
		RpcAuthInputKind::CallbackUrl => {
			Ok(AuthInput::CallbackUrl(SecretString::from(required(value)?)))
		},
		RpcAuthInputKind::PlainText => Ok(AuthInput::PlainText(Str::from(required(value)?))),
		RpcAuthInputKind::OptionalSecret => {
			Ok(AuthInput::OptionalSecret(SecretString::from(value.unwrap_or_default())))
		},
		RpcAuthInputKind::DeviceConfirmed => {
			if value.is_some() {
				return Err(CommandError::new(
					"invalid_auth_input",
					"device confirmation does not accept a value",
				));
			}
			Ok(AuthInput::DeviceConfirmed)
		},
		RpcAuthInputKind::Cancel => {
			if value.is_some() {
				return Err(CommandError::new(
					"invalid_auth_input",
					"authentication cancellation does not accept a value",
				));
			}
			Ok(AuthInput::Cancel)
		},
	}
}

fn auth_account(account: AccountSummary) -> RpcAuthAccount {
	RpcAuthAccount {
		account_id:  account.account.as_str().to_owned(),
		provider_id: account.provider.as_str().to_owned(),
		principal:   account
			.principal
			.map(|principal| principal.as_str().to_owned()),
		label:       account.label.map(|label| label.to_string()),
	}
}

fn oauth_providers_value(providers: &[OAuthProvider]) -> Value {
	json!({ "providers": providers })
}

fn host_tool_names_value(tool_names: Vec<String>) -> Value {
	json!({ "toolNames": tool_names })
}

fn parse_params<P>(params: &Map<String, Value>) -> Result<P, CommandError>
where
	P: for<'de> Deserialize<'de>,
{
	serde_json::from_value(Value::Object(params.clone()))
		.map_err(|error| CommandError::new("invalid_params", error.to_string()))
}

#[cfg(not(windows))]
fn shell_command(command: &str) -> Command {
	let shell = env::var_os("SHELL")
		.filter(|value| !value.is_empty())
		.unwrap_or_else(|| "/bin/sh".into());
	let mut process = Command::new(shell);
	process.arg("-lc").arg(command);
	process
}

#[cfg(windows)]
fn shell_command(command: &str) -> Command {
	let mut process = Command::new("cmd");
	process.arg("/C").arg(command);
	process
}

fn decode_transcript_entries(bytes: &[u8]) -> (Vec<Value>, Vec<Value>) {
	let entries = bytes
		.split(|byte| *byte == b'\n')
		.filter_map(|line| {
			let line = line.strip_suffix(b"\r").unwrap_or(line);
			(!line.is_empty())
				.then(|| serde_json::from_slice::<Value>(line).ok())
				.flatten()
		})
		.collect::<Vec<_>>();
	let messages = entries.iter().filter_map(renderable_message).collect();
	(entries, messages)
}

fn renderable_message(entry: &Value) -> Option<Value> {
	if entry.get("role").and_then(Value::as_str).is_some() {
		return Some(entry.clone());
	}
	["/message", "/data/message", "/item/message", "/data/item/message"]
		.into_iter()
		.find_map(|pointer| {
			entry
				.pointer(pointer)
				.filter(|value| value.is_object())
				.cloned()
		})
}

fn text<'a>(params: &'a Map<String, Value>, key: &str) -> Result<&'a str, CommandError> {
	params
		.get(key)
		.and_then(Value::as_str)
		.filter(|value| !value.is_empty())
		.ok_or_else(|| {
			CommandError::new("invalid_params", format!("`{key}` must be a non-empty string"))
		})
}

fn boolean(params: &Map<String, Value>, key: &str) -> Result<bool, CommandError> {
	params
		.get(key)
		.and_then(Value::as_bool)
		.ok_or_else(|| CommandError::new("invalid_params", format!("`{key}` must be a boolean")))
}

fn unsigned(params: &Map<String, Value>, key: &str) -> Result<u64, CommandError> {
	params.get(key).and_then(Value::as_u64).ok_or_else(|| {
		CommandError::new("invalid_params", format!("`{key}` must be an unsigned integer"))
	})
}
fn emit(sender: &Sender<Value>, value: Value) -> miette::Result<()> {
	sender.send(value).into_diagnostic()
}

#[derive(Debug)]
struct CommandError {
	code:    &'static str,
	message: String,
}

impl CommandError {
	fn new(code: &'static str, message: impl Into<String>) -> Self {
		Self { code, message: message.into() }
	}

	fn transport(error: miette::Report) -> Self {
		Self::new("transport_error", error.to_string())
	}

	fn json(error: serde_json::Error) -> Self {
		Self::new("serialization_error", error.to_string())
	}

	fn io(error: io::Error) -> Self {
		Self::new("transcript_unavailable", error.to_string())
	}
}

fn unix_millis() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or_default()
		.as_millis()
		.try_into()
		.unwrap_or(u64::MAX)
}

fn new_id(prefix: &str) -> String {
	format!("{prefix}-{}-{}", process::id(), turn_id())
}

fn escape_html(text: &str) -> String {
	text
		.replace('&', "&amp;")
		.replace('<', "&lt;")
		.replace('>', "&gt;")
		.replace('"', "&quot;")
}

fn decode_base64url(input: &str) -> Result<Vec<u8>, CommandError> {
	omp_core::base64_url::decode_raw(input)
		.into_vec()
		.map_err(|_| CommandError::new("stale_cursor", "cursor is not base64url"))
}

#[cfg(test)]
mod tests {

	use std::slice;

	use super::*;

	#[test]
	fn detects_only_standalone_lowercase_orchestrate_in_prose() {
		assert!(contains_orchestrate("please orchestrate this work"));
		assert!(contains_orchestrate("(orchestrate)"));
		assert!(!contains_orchestrate("Orchestrate this"));
		assert!(!contains_orchestrate("orchestrated work"));
		assert!(!contains_orchestrate("`orchestrate`"));
		assert!(!contains_orchestrate("```text\norchestrate\n```"));
		assert!(!contains_orchestrate("<orchestrate value=\"yes\">"));
	}

	#[test]
	fn transcript_cursor_paginates_and_invalidates_on_mutation() {
		let mut session = Session::new("session-a".into(), None);
		for index in 0..5 {
			session.push_message("user", &format!("message {index}"));
		}
		let first = message_page(&session, None, 2).expect("first page");
		assert_eq!(first["messages"].as_array().expect("messages").len(), 2);
		let cursor = first["nextCursor"].as_str().expect("cursor").to_owned();
		let second = message_page(&session, Some(&cursor), 2).expect("second page");
		assert_eq!(second["messages"][0]["content"], "message 2");
		session.push_message("assistant", "changed");
		assert_eq!(
			message_page(&session, Some(&cursor), 2)
				.expect_err("stale")
				.code,
			"stale_cursor"
		);
	}

	#[tokio::test]
	async fn ordinary_dispatch_channel_preserves_fifo_order() {
		let (sender, receiver) = flume::unbounded();
		for value in 0..64_u64 {
			sender.send_async(value).await.expect("send");
		}
		drop(sender);
		let mut observed = Vec::new();
		while let Ok(value) = receiver.recv_async().await {
			observed.push(value);
		}
		assert_eq!(observed, (0..64).collect::<Vec<_>>());
	}

	#[test]
	fn corrected_command_params_use_sdk_field_names() {
		let set_model = json!({ "provider": "anthropic", "modelId": "claude" });
		let parsed =
			parse_params::<SetModelParams>(set_model.as_object().expect("object")).expect("set model");
		assert_eq!(parsed.provider, "anthropic");
		assert_eq!(parsed.model_id, "claude");

		let switch = json!({ "sessionPath": "sessions/one.jsonl" });
		let parsed = parse_params::<SwitchSessionParams>(switch.as_object().expect("object"))
			.expect("switch session");
		assert_eq!(parsed.session_path, "sessions/one.jsonl");

		let login = json!({ "providerId": "openai" });
		let parsed = parse_params::<LoginParams>(login.as_object().expect("object")).expect("login");
		assert_eq!(parsed.provider_id, "openai");

		let subagent = json!({
			"subagentId": "sub-1",
			"sessionFile": "sub-1.jsonl",
			"fromByte": 42,
		});
		let parsed = parse_params::<GetSubagentMessagesParams>(subagent.as_object().expect("object"))
			.expect("subagent messages");
		assert_eq!(parsed.subagent_id.as_deref(), Some("sub-1"));
		assert_eq!(parsed.session_file.as_deref(), Some("sub-1.jsonl"));
		assert_eq!(parsed.from_byte, Some(42));
	}

	#[test]
	fn corrected_results_match_sdk_wire_types() {
		let provider = OAuthProvider {
			id:            "openai".into(),
			name:          "OpenAI".into(),
			available:     true,
			authenticated: false,
		};
		let providers = oauth_providers_value(slice::from_ref(&provider));
		assert_eq!(
			providers,
			json!({
				"providers": [{
					"id": "openai",
					"name": "OpenAI",
					"available": true,
					"authenticated": false,
				}],
			})
		);
		let decoded: OAuthProvider =
			serde_json::from_value(providers["providers"][0].clone()).expect("OAuth provider");
		assert_eq!(decoded, provider);

		assert_eq!(host_tool_names_value(vec!["search".into()]), json!({ "toolNames": ["search"] }));

		let messages = SubagentMessages {
			session_file: "sub-1.jsonl".into(),
			from_byte:    4,
			next_byte:    9,
			reset:        false,
			entries:      vec![json!({ "type": "entry" })],
			messages:     vec![json!({ "role": "assistant", "content": "done" })],
		};
		let value = serde_json::to_value(&messages).expect("subagent result");
		assert_eq!(value["sessionFile"], "sub-1.jsonl");
		assert_eq!(value["fromByte"], 4);
		assert_eq!(value["nextByte"], 9);
		assert!(value.get("entries").is_some());
		assert!(value.get("messages").is_some());
		let decoded: SubagentMessages = serde_json::from_value(value).expect("SDK subagent result");
		assert_eq!(decoded, messages);
	}

	#[test]
	fn host_side_channel_frames_use_canonical_fields() {
		let call = HostToolCall {
			kind:         "host_tool_call".into(),
			id:           "inv-1".into(),
			tool_call_id: "call-1".into(),
			tool_name:    "search".into(),
			arguments:    Map::from_iter([("query".into(), json!("rust"))]),
		};
		assert_eq!(
			serde_json::to_value(call).expect("host call"),
			json!({
				"type": "host_tool_call",
				"id": "inv-1",
				"toolCallId": "call-1",
				"toolName": "search",
				"arguments": { "query": "rust" },
			})
		);

		let update: HostToolUpdate = serde_json::from_value(json!({
			"type": "host_tool_update",
			"id": "inv-1",
			"partialResult": { "progress": 1 },
		}))
		.expect("host update");
		assert_eq!(update.id, "inv-1");
		assert_eq!(update.partial_result["progress"], 1);

		let result: HostToolResult = serde_json::from_value(json!({
			"type": "host_tool_result",
			"id": "inv-1",
			"result": "done",
			"isError": false,
		}))
		.expect("host result");
		assert_eq!(result.result, "done");

		let cancel: HostToolCancel = serde_json::from_value(json!({
			"type": "host_tool_cancel",
			"id": "cancel-1",
			"targetId": "inv-1",
		}))
		.expect("host cancel");
		assert_eq!(cancel.target_id, "inv-1");
	}

	#[test]
	fn shell_commands_bypass_the_ordinary_fifo() {
		assert!(is_immediate_frame(&json!({ "type": "bash", "command": "echo ok" })));
		assert!(is_immediate_frame(&json!({ "type": "abort_bash" })));
		assert!(is_immediate_frame(&json!({
			"type": "extension_ui_response",
			"id": "ui-1",
		})));
		assert!(!is_immediate_frame(&json!({ "type": "get_state" })));
	}

	#[test]
	fn stdin_claim_is_exclusive_and_released_by_guard() {
		let first = StdinClaim::claim().expect("first claim");
		assert!(StdinClaim::claim().is_err());
		drop(first);
		assert!(StdinClaim::claim().is_ok());
	}
}
