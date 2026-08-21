//! Durable project-chat composition.

#[path = "chat_hub.rs"]
mod hub_backend;

pub(crate) fn chat_hub_tool() -> impl omp_tool::Tool {
	hub_backend::tool()
}

use std::{
	collections::BTreeMap,
	fs::File,
	io::{BufRead as _, BufReader},
	path::{Path, PathBuf},
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use futures::StreamExt as _;
use miette::IntoDiagnostic as _;
use omp_agent::{
	Agent, AgentKind, AgentSnapshot, AgentState, AgentStatus, AgentTree, Budget, CompletionError,
	CompletionRequest, InProcTurnClient, Journal, RpcTurnClient, TurnClient, TurnId, TurnInput,
	TurnOptions, TurnSession as _, WorkspaceInput, project_journal, resolve_completion,
};
use omp_core::{ExposeSecret as _, Str, sf};
use omp_llm_catalog::GrammarBits;
use omp_llm_inference::{
	Client, Registry as InferenceRegistry, ToolInputConstraint,
	answer::{AuthAnswer, AuthEvent, AuthPromptKind as InferenceAuthPromptKind, AuthResponse},
	call::{AuthInput, AuthRequest, CallMeta, LoginRequest, Target},
	error::{ErrorDetail, ErrorKind},
	id::RequestId,
	receipt::ExecutionBudget,
	router::Router,
};
use omp_proto::{
	inference::v1 as inference_pb,
	thread::v1::{Item, Message, Part, Role, Thread, item, part},
};
use omp_storage::{
	index::{IndexedWriteError, NewSession, SessionIndex, SessionKind},
	transcript::{Header, Kind, SessionId, read_header, read_line},
};
use omp_tool::{CapsBase, LoweringCaps, ModelClass, Registry};
use parking_lot::Mutex;
use serde_json::{Value, json};
use thiserror::Error;
use xutf::IntoAnsiStripped as _;

use crate::{
	chat_ui::{
		self, AuthPromptKind, ChatAuth, ChatAuthCommand, ChatAuthEvent, ChatUiSession, ResumeChoice,
	},
	cli::{ChatArgs, ThinkingLevel},
};

const CHAT_CAPS_BASE: CapsBase = CapsBase {
	maximum_parts:      1,
	maximum_text_bytes: 64 * 1024,
	media:              false,
	model_class:        ModelClass::Standard,
};
const DEFAULT_EVAL_CONCURRENCY_LIMIT: usize = omp_agent::DEFAULT_MAX_CONCURRENCY;

/// Failures while resolving or running one durable project-chat session.
#[derive(Debug, Error)]
pub enum ChatError {
	/// The requested project root could not be canonicalized.
	#[error("could not resolve project root {path}")]
	Project {
		/// Project path supplied by the caller.
		path:   PathBuf,
		/// Filesystem failure.
		#[source]
		source: std::io::Error,
	},
	/// The canonical project path is not a directory.
	#[error("project root is not a directory: {0}")]
	ProjectNotDirectory(PathBuf),
	/// Project-local state could not be accessed.
	#[error("could not access project state {path}")]
	ProjectState {
		/// State path that failed.
		path:   PathBuf,
		/// Filesystem failure.
		#[source]
		source: std::io::Error,
	},
	/// The requested resume identity is not a canonical ULID.
	#[error("invalid chat session id: {0}")]
	InvalidResume(Str),
	/// The requested durable session does not exist.
	#[error("chat session does not exist: {0}")]
	MissingResume(Str),
	/// The journal header did not match the requested session.
	#[error("chat journal identity does not match session {0}")]
	SessionMismatch(Str),
	/// The journal belongs to a different canonical project root.
	#[error("chat session {session} belongs to a different project")]
	SessionProjectMismatch {
		/// Requested session identity.
		session: Str,
	},
	/// Durable transcript state failed to open, create, or project.
	#[error(transparent)]
	Journal(#[from] omp_agent::JournalError),
	/// The authoritative write-time sessions index failed.
	#[error(transparent)]
	SessionIndex(omp_storage::index::Error),
	/// A durable transcript could not be projected into canonical replay items.
	#[error(transparent)]
	Projection(#[from] omp_agent::ProjectionError),
	/// The project environment authority failed to start or connect.
	#[error(transparent)]
	Environment(#[from] crate::envd::EnvdError),
	/// The in-process turn authority could not be constructed.
	#[error(transparent)]
	TurnClient(#[from] omp_agent::Error),
	/// A live tool declaration could not be represented on the turn protocol.
	#[error("tool {0} uses a grammar input unsupported by the turn protocol")]
	GrammarTool(Str),
	/// A tool schema could not be encoded for the turn protocol.
	#[error("could not encode tool schema")]
	ToolSchema(#[source] serde_json::Error),
	/// The live tool registry could not lower its advertised slots.
	#[error(transparent)]
	ToolRegistry(#[from] omp_tool::RegistryError),
	/// The requested model selector names a catalog route, not a model.
	#[error("`{selector}` is a route id, not a model{hint}")]
	ModelSelectorIsRoute {
		/// Selector supplied by the caller.
		selector: Str,
		/// Preformatted candidate-model hint, or empty.
		hint:     Str,
	},
	/// The requested model selector matches no catalog model or alias.
	#[error("unknown model `{selector}`{suggestions}")]
	UnknownModel {
		/// Selector supplied by the caller.
		selector:    Str,
		/// Preformatted nearest-key hint, or empty.
		suggestions: Str,
	},
	/// The session-scoped eval parent bridge could not be bound.
	#[error("eval session bridge failed: {0}")]
	EvalBridge(Str),
	/// The interactive terminal shell failed.
	#[error("interactive chat shell failed: {0}")]
	Ui(miette::Report),
	/// The platform cannot enforce the Phase 3 owner-local environment contract.
	#[error("interactive chat requires Unix owner-local project authorities")]
	UnsupportedPlatform,
}

struct Session {
	id:            Str,
	journal:       Journal,
	initial_items: Vec<omp_proto::thread::v1::Item>,
}

struct ChatScope<'a> {
	catalog:       &'a omp_llm_catalog::snapshot::Catalog,
	root:          &'a Path,
	sessions_dir:  &'a Path,
	session_index: Arc<SessionIndex>,
	registry:      Arc<Registry>,
}
pub(crate) struct ChatAuthWorker {
	ui:   ChatAuth,
	task: Option<tokio::task::JoinHandle<()>>,
}

impl ChatAuthWorker {
	pub(crate) fn start(registry: InferenceRegistry) -> Self {
		let (command_tx, command_rx) = flume::unbounded();
		let (event_tx, event_rx) = flume::unbounded();
		let active = Arc::new(AtomicBool::new(false));
		let worker_active = Arc::clone(&active);
		let task = tokio::spawn(async move {
			while let Ok(command) = command_rx.recv_async().await {
				let ChatAuthCommand::Start(provider) = command else {
					continue;
				};
				let reset = AuthActivity(Arc::clone(&worker_active));
				let result = run_chat_login(&registry, provider, &event_tx, &command_rx).await;
				drain_auth_commands(&command_rx);
				drop(reset);
				let event = match result {
					Ok(message) => ChatAuthEvent::Complete(message),
					Err(ChatLoginFailure::CredentialStorageLocked) => {
						ChatAuthEvent::CredentialStorageLocked
					},
					Err(ChatLoginFailure::Message(error)) => ChatAuthEvent::Failed(error),
				};
				let _ = event_tx.send(event);
			}
		});
		Self { ui: ChatAuth::new(command_tx, event_rx, active), task: Some(task) }
	}

	/// Returns the UI-facing handle for the worker.
	pub(crate) const fn ui(&self) -> &ChatAuth {
		&self.ui
	}

	pub(crate) async fn shutdown(mut self) {
		if let Some(task) = self.task.take() {
			task.abort();
			let _ = task.await;
		}
	}
}

impl Drop for ChatAuthWorker {
	fn drop(&mut self) {
		if let Some(task) = &self.task {
			task.abort();
		}
	}
}

struct AuthActivity(Arc<AtomicBool>);

impl Drop for AuthActivity {
	fn drop(&mut self) {
		self.0.store(false, Ordering::Release);
	}
}

enum ChatLoginFailure {
	CredentialStorageLocked,
	Message(Str),
}

impl From<Str> for ChatLoginFailure {
	fn from(message: Str) -> Self {
		Self::Message(message)
	}
}
fn auth_error_message(error: &omp_llm_inference::Error) -> Str {
	let detail = match error.detail_ref() {
		Some(ErrorDetail::Provider { sanitized_message }) => Some(sanitized_message.as_str()),
		_ => None,
	};
	match (detail, error.status, error.code.as_deref()) {
		(Some(detail), Some(status), Some(code)) => {
			sf!("{error}: {detail} ({status}, {code})")
		},
		(Some(detail), Some(status), None) => sf!("{error}: {detail} ({status})"),
		(Some(detail), None, Some(code)) => sf!("{error}: {detail} ({code})"),
		(Some(detail), None, None) => sf!("{error}: {detail}"),
		(None, ..) => Str::from(error.to_string()),
	}
}
fn chat_login_failure(
	provider: &omp_llm_catalog::ProviderId,
	error: &omp_llm_inference::Error,
) -> ChatLoginFailure {
	if error.kind == ErrorKind::CredentialStorageUnavailable {
		ChatLoginFailure::CredentialStorageLocked
	} else {
		ChatLoginFailure::Message(sf!(
			"Authentication failed for provider `{provider}`. Use `/login {provider}` to try again. \
			 {}",
			auth_error_message(error)
		))
	}
}

async fn run_chat_login(
	registry: &InferenceRegistry,
	provider: Str,
	events: &flume::Sender<ChatAuthEvent>,
	commands: &flume::Receiver<ChatAuthCommand>,
) -> Result<Str, ChatLoginFailure> {
	let provider = omp_llm_catalog::ProviderId::from(provider);
	let planner = Router::new(registry.clone(), Duration::from_secs(30));
	let meta = CallMeta {
		id:       RequestId::from(format!("chat-auth-{}", omp_core::Ulid::generate())),
		target:   Target::ProviderService(provider.clone()),
		deadline: None,
		budget:   ExecutionBudget::default(),
		session:  None,
	};
	let mut client = Client::new(registry.service(), planner, meta);
	let answer = client
		.execute(AuthRequest::Login(LoginRequest { provider: provider.clone(), method: None }))
		.await
		.map_err(|error| chat_login_failure(&provider, &error))?;
	let AuthAnswer::Session(session) = answer else {
		return Err(
			sf!(
				"Provider `{provider}` did not start an interactive login. Use `/login {provider}` to \
				 try again."
			)
			.into(),
		);
	};
	let mut awaiting_prompt = false;
	loop {
		tokio::select! {
			event = session.events.recv_async() => {
				let event = event
					.map_err(|_| {
						sf!(
							"Authentication for provider `{provider}` ended without completing. Use \
							 `/login {provider}` to try again."
						)
					})?
					.map_err(|error| chat_login_failure(&provider, &error))?;
				match event {
					AuthEvent::OpenUrl(url) => {
						// Launch the browser directly (best-effort); the forwarded
						// event keeps the clickable/copyable URL as fallback.
						crate::open::open_path(&url);
						events
							.send(ChatAuthEvent::Url(url))
							.map_err(|_| sf!("chat authentication view closed"))?;
					},
					AuthEvent::ShowDeviceCode { code, verification_url } => {
						// pi opens the verification URL for device flows too; the
						// code stays visible in the forwarded event.
						crate::open::open_path(&verification_url);
						events
							.send(ChatAuthEvent::DeviceCode {
								code: Str::from(code.expose_secret()),
								url:  verification_url,
							})
							.map_err(|_| sf!("chat authentication view closed"))?;
					},
					AuthEvent::Prompt(prompt) => {
						let kind = match prompt.input {
							InferenceAuthPromptKind::ApiKey => AuthPromptKind::ApiKey,
							InferenceAuthPromptKind::AuthorizationCode => {
								AuthPromptKind::AuthorizationCode
							},
							InferenceAuthPromptKind::SessionToken => AuthPromptKind::SessionToken,
							InferenceAuthPromptKind::PlainText => AuthPromptKind::PlainText,
							InferenceAuthPromptKind::OptionalSecret => AuthPromptKind::OptionalSecret,
							InferenceAuthPromptKind::Confirmation => AuthPromptKind::Confirmation,
						};
						events
							.send(ChatAuthEvent::Prompt { message: prompt.message, kind })
							.map_err(|_| sf!("chat authentication view closed"))?;
						awaiting_prompt = true;
					},
					AuthEvent::Waiting => {
						events
							.send(ChatAuthEvent::Notice(sf!(
								"Waiting for `{provider}` authorization…"
							)))
							.map_err(|_| sf!("chat authentication view closed"))?;
					},
					AuthEvent::Complete(account) => {
						return Ok(sf!(
							"Authenticated `{}` for `{}`.",
							account.account,
							account.provider
						));
					},
				}
			},
			command = commands.recv_async() => match command {
				Ok(ChatAuthCommand::Cancel) => {
					send_auth_response(&session, AuthInput::Cancel, &provider).await?;
					return Err(
						sf!("Authentication for provider `{provider}` was cancelled.").into()
					);
				},
				Ok(ChatAuthCommand::Answer(input)) if awaiting_prompt => {
					send_auth_response(&session, input, &provider).await?;
					awaiting_prompt = false;
				},
				Ok(ChatAuthCommand::Answer(_) | ChatAuthCommand::Start(_)) => {},
				Err(_) => {
					return Err(sf!("chat authentication view closed").into());
				},
			},
		}
	}
}

async fn send_auth_response(
	session: &omp_llm_inference::answer::AuthSession,
	input: AuthInput,
	provider: &omp_llm_catalog::ProviderId,
) -> Result<(), Str> {
	session
		.responses
		.send_async(AuthResponse { session: session.id.clone(), input })
		.await
		.map_err(|_| {
			sf!(
				"Authentication provider `{provider}` stopped accepting input. Use `/login \
				 {provider}` to try again."
			)
		})
}

fn drain_auth_commands(commands: &flume::Receiver<ChatAuthCommand>) {
	while commands.try_recv().is_ok() {}
}

#[cfg(test)]
mod auth_worker_tests {
	use super::*;

	#[test]
	fn credential_storage_failure_keeps_typed_ui_signal() {
		let error = omp_llm_inference::Error::new(
			ErrorKind::CredentialStorageUnavailable,
			omp_llm_inference::error::ErrorPhase::Authentication,
			omp_llm_inference::error::RetryAction::Never,
			omp_llm_inference::receipt::ExecutionReceipt::default(),
		);
		let provider = omp_llm_catalog::ProviderId::from("test-provider");
		assert!(matches!(
			chat_login_failure(&provider, &error),
			ChatLoginFailure::CredentialStorageLocked
		));
	}

	#[test]
	fn completed_flow_drops_answers_before_the_next_login() {
		let (commands, receiver) = flume::unbounded();
		commands
			.send(ChatAuthCommand::Answer(AuthInput::DeviceConfirmed))
			.expect("stale prompt answer");
		commands
			.send(ChatAuthCommand::Cancel)
			.expect("stale cancellation");

		drain_auth_commands(&receiver);
		assert!(matches!(receiver.try_recv(), Err(flume::TryRecvError::Empty)));

		commands
			.send(ChatAuthCommand::Start(sf!("next-provider")))
			.expect("next login");
		assert!(matches!(
			receiver.try_recv(),
			Ok(ChatAuthCommand::Start(provider)) if provider == "next-provider"
		));
	}
}
fn discover_chat_agents(root: &Path) -> Arc<BTreeMap<Str, omp_agent::AgentDefinition>> {
	use crate::discovery::manifest::{CapabilityDeclaration, CapabilityKind};

	let mut declarations = vec![CapabilityDeclaration {
		id:       sf!("project-agents"),
		kind:     CapabilityKind::Agents,
		root:     root.join(".omp/agents"),
		priority: 100,
	}];
	if let Some(home) = std::env::var_os("HOME") {
		declarations.push(CapabilityDeclaration {
			id:       sf!("user-agents"),
			kind:     CapabilityKind::Agents,
			root:     PathBuf::from(home).join(".omp/agent/agents"),
			priority: 50,
		});
	}
	let mut definitions = crate::discovery::manifest::discover_agents(&declarations)
		.definitions
		.into_iter()
		.collect::<BTreeMap<_, _>>();
	definitions
		.entry(sf!("task"))
		.or_insert_with(|| omp_agent::AgentDefinition {
			name:           sf!("task"),
			description:    sf!("General-purpose subagent"),
			tools:          Box::default(),
			spawns:         omp_agent::SpawnPolicy::Any,
			model:          None,
			thinking_level: None,
			blocking:       false,
			prompt:         sf!("Complete the delegated task and report the result."),
		});
	Arc::new(definitions)
}

#[derive(Clone)]
struct ChatParentContext {
	state:         AgentState,
	session_id:    Str,
	sessions_dir:  PathBuf,
	root:          PathBuf,
	session_index: Arc<SessionIndex>,
	definitions:   Arc<BTreeMap<Str, omp_agent::AgentDefinition>>,
	tree:          Arc<AgentTree>,
}

pub(crate) struct ChatParentHost<C: TurnClient + Clone + 'static> {
	client:  C,
	env:     omp_env::EnvClient,
	broker:  omp_agent::Broker,
	context: Mutex<ChatParentContext>,
}

impl<C: TurnClient + Clone + 'static> ChatParentHost<C> {
	pub(crate) fn new(
		client: C,
		env: omp_env::EnvClient,
		state: AgentState,
		session_id: Str,
		sessions_dir: PathBuf,
		root: PathBuf,
		session_index: Arc<SessionIndex>,
	) -> Self {
		let definitions = discover_chat_agents(&root);
		Self {
			client,
			env,
			broker: omp_agent::Broker::new(Str::from(root.to_string_lossy().as_ref())),
			context: Mutex::new(ChatParentContext {
				state,
				session_id,
				sessions_dir,
				root,
				session_index,
				definitions,
				tree: Arc::new(AgentTree::new(
					8,
					DEFAULT_EVAL_CONCURRENCY_LIMIT,
					omp_agent::DEFAULT_MAX_ADMISSION_QUEUE,
				)),
			}),
		}
	}

	pub(crate) fn update(&self, state: AgentState, session_id: Str) {
		let mut context = self.context.lock();
		context.state = state;
		context.session_id = session_id;
	}

	/// Shares the append-only subagent roster with the interactive UI bridge.
	pub(crate) fn tree(&self) -> Arc<AgentTree> {
		Arc::clone(&self.context.lock().tree)
	}

	pub(crate) fn broker(&self) -> omp_agent::Broker {
		self.broker.clone()
	}

	pub(crate) fn session_id(&self) -> Str {
		self.context.lock().session_id.clone()
	}
}

fn bridge_message(role: Role, text: &str) -> Item {
	Item {
		seq:           0,
		created_at_ms: now_ms(),
		kind:          Some(item::Kind::Message(Message {
			role:  i32::from(role),
			parts: vec![Part { kind: Some(part::Kind::Text(text.to_owned())) }],
		})),
		props:         None,
	}
}

fn bridge_outcome_text(outcome: &inference_pb::Outcome) -> String {
	let mut text = String::new();
	for item in &outcome.output {
		if let Some(item::Kind::Message(message)) = &item.kind {
			for part in &message.parts {
				if let Some(part::Kind::Text(value)) = &part.kind {
					text.push_str(value);
				}
			}
		}
	}
	text
}

#[async_trait]
impl<C: TurnClient + Clone + 'static> crate::envd::eval::ParentSessionHost for ChatParentHost<C> {
	async fn completion(&self, args: Value) -> Result<Value, crate::envd::eval::BridgeHostError> {
		let choices = args.get("choices").and_then(Value::as_array).map_or_else(
			smallvec::SmallVec::new,
			|values| {
				values
					.iter()
					.filter_map(Value::as_str)
					.map(Str::from)
					.collect()
			},
		);
		let completion = CompletionRequest {
			choices,
			default: args.get("default").and_then(Value::as_str).map(Str::from),
			max_usd_micros: None,
		};
		let prompt = args.get("prompt").and_then(Value::as_str).ok_or_else(|| {
			crate::envd::eval::BridgeHostError::message("completion prompt is required")
		})?;
		let context = self.context.lock().clone();
		let snapshot = context.state.snapshot();
		let mut params = snapshot.turn.params.clone();
		params.tools.clear();
		params.tool_choice = None;
		params.model = match args
			.get("model")
			.and_then(Value::as_str)
			.unwrap_or("default")
		{
			"default" => params.model,
			model @ ("smol" | "slow") => format!("@{model}"),
			other => {
				return Err(crate::envd::eval::BridgeHostError::message(format!(
					"unsupported completion model tier: {other}"
				)));
			},
		};
		if let Some(schema) = args.get("schema") {
			let schema_json = serde_json::to_vec(schema)
				.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
			params.response_format = Some(inference_pb::ResponseFormat {
				kind:           Some(inference_pb::response_format::Kind::JsonSchema(
					inference_pb::response_format::JsonSchema {
						name:        "eval_completion".to_owned(),
						schema_json: schema_json.into(),
						strict:      Some(true),
					},
				)),
				on_unsupported: inference_pb::Fallback::Error as i32,
			});
		}
		let mut items = Vec::new();
		if let Some(system) = args.get("system").and_then(Value::as_str) {
			items.push(bridge_message(Role::System, system));
		}
		items.push(bridge_message(Role::User, prompt));
		let options = TurnOptions { context_id: None, params, executor: None, props: None };
		let mut turn = self
			.client
			.turn(
				TurnId::new(format!("eval-completion-{}", omp_core::Ulid::generate())),
				TurnInput::Full(Thread { items }),
				&options,
			)
			.await
			.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
		let mut events = turn.events();
		while let Some(event) = events.next().await {
			let event = event
				.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
			match event.event {
				Some(inference_pb::turn_event::Event::Outcome(outcome)) => {
					let text = Str::from(bridge_outcome_text(&outcome));
					let completion = resolve_completion(&completion, Ok(text)).map_err(|error| {
						crate::envd::eval::BridgeHostError::message(error.to_string())
					})?;
					if completion.choice.is_none() && !completion.fell_back {
						return Ok(json!({ "text": completion.text }));
					}
					return Ok(json!({
						"text": completion.text,
						"choice": completion.choice,
						"fell_back": completion.fell_back,
					}));
				},
				Some(inference_pb::turn_event::Event::Error(error)) => {
					let completion = resolve_completion(
						&completion,
						Err(CompletionError::Provider(Str::from(error.detail))),
					)
					.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
					return Ok(json!({
						"text": completion.text,
						"choice": completion.choice,
						"fell_back": completion.fell_back,
					}));
				},
				_ => {},
			}
		}
		Err(crate::envd::eval::BridgeHostError::message("completion turn ended without an outcome"))
	}

	async fn agent(&self, args: Value) -> Result<Value, crate::envd::eval::BridgeHostError> {
		let prompt = args
			.get("prompt")
			.and_then(Value::as_str)
			.ok_or_else(|| crate::envd::eval::BridgeHostError::message("agent prompt is required"))?;
		let kind = args.get("agent").and_then(Value::as_str).unwrap_or("task");
		let context = self.context.lock().clone();
		let definition = context
			.definitions
			.iter()
			.find(|(name, _)| name.as_str().eq_ignore_ascii_case(kind))
			.map(|(_, definition)| definition.clone())
			.ok_or_else(|| {
				crate::envd::eval::BridgeHostError::message(format!(
					"agent type '{kind}' is not available in this session"
				))
			})?;
		let system_prompt = match (definition.description.is_empty(), definition.prompt.is_empty()) {
			(true, true) => sf!("Complete the delegated task."),
			(false, true) => definition.description.clone(),
			(true, false) => definition.prompt.clone(),
			(false, false) => {
				Str::from(format!("{}\n\n{}", definition.description, definition.prompt))
			},
		};
		let apply = args.get("apply").and_then(Value::as_bool).unwrap_or(false);
		let merge = args.get("merge").and_then(Value::as_bool).unwrap_or(false);
		if apply && merge {
			return Err(crate::envd::eval::BridgeHostError::message(
				"agent options 'apply' and 'merge' are mutually exclusive",
			));
		}
		let explicitly_isolated = args.get("isolated").and_then(Value::as_bool);
		if explicitly_isolated == Some(false) && (apply || merge) {
			return Err(crate::envd::eval::BridgeHostError::message(
				"agent apply/merge requires an isolated worktree",
			));
		}
		let isolated = explicitly_isolated.unwrap_or(apply || merge);
		if args.get("schema").is_some() || args.get("schemaMode").is_some() {
			return Err(crate::envd::eval::BridgeHostError::message(
				"structured subagent output is not supported by this session",
			));
		}
		let id = args
			.get("_id")
			.and_then(Value::as_str)
			.map_or_else(|| Str::from(omp_core::Ulid::generate().to_string()), Str::from);
		let directory = context.sessions_dir.join("eval-agents");
		let (worktree_id, child_root, isolated_environment) = if isolated {
			let created = self
				.env
				.create_worktree(omp_proto::env::v1::CreateWorktree {
					name: id.to_string(),
					owner_pid: std::process::id(),
					..Default::default()
				})
				.await
				.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
			let worktree = created.worktree.ok_or_else(|| {
				crate::envd::eval::BridgeHostError::message(
					"Environment omitted the created worktree identity",
				)
			})?;
			let root_url = url::Url::parse(&worktree.root_uri)
				.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
			let root = root_url.to_file_path().map_err(|()| {
				crate::envd::eval::BridgeHostError::message(
					"Environment returned a non-file worktree root",
				)
			})?;
			let child_state = context
				.sessions_dir
				.join("eval-agents")
				.join(format!("{id}-env"));
			let environment = crate::envd::ProjectEnvironment::isolated(&root, &child_state)
				.await
				.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
			(Some(Str::from(worktree.id)), root, Some(environment))
		} else {
			(None, context.root.clone(), None)
		};
		let name = args
			.get("label")
			.and_then(Value::as_str)
			.map_or_else(|| id.clone(), Str::from);
		let _permit = context
			.tree
			.admit(1)
			.await
			.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
		let node = context
			.tree
			.register(
				id.clone(),
				name,
				AgentKind::Subagent,
				context
					.tree
					.node(context.session_id.as_str())
					.map(|parent| parent.id.clone()),
				context.session_id.clone(),
				Budget::default(),
			)
			.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
		node.set_status(AgentStatus::Running);
		std::fs::create_dir_all(&directory)
			.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
		let parent = SessionId(context.session_id.clone());
		let journal = create_indexed_journal(
			&directory.join(format!("{id}.jsonl")),
			&context.root,
			&id,
			Arc::clone(&context.session_index),
			SessionKind::Subagent,
			Some(&parent),
		)
		.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
		let mut child_snapshot = context.state.snapshot().as_ref().clone();
		child_snapshot.workspace.cwd = child_root.clone();
		if let Some(model) = definition.effective_model(&BTreeMap::new()) {
			child_snapshot.turn.params.model = model.to_owned();
		}
		if let Some(level) = definition.thinking_level.as_deref() {
			let effort = match level {
				"off" => inference_pb::Effort::Off,
				"minimal" | "min" => inference_pb::Effort::Minimal,
				"low" | "lo" => inference_pb::Effort::Low,
				"medium" | "med" => inference_pb::Effort::Medium,
				"high" | "hi" => inference_pb::Effort::High,
				"max" => inference_pb::Effort::Max,
				"xhigh" => inference_pb::Effort::Xhigh,
				_ => inference_pb::Effort::Unspecified,
			};
			child_snapshot.turn.params.thinking =
				Some(inference_pb::Reasoning { effort: effort as i32, ..Default::default() });
		}
		let may_spawn = definition.spawns.default_definition().is_some();
		let declared_tools = &definition.tools;
		child_snapshot.enabled_tools = child_snapshot
			.enabled_tools
			.iter()
			.filter(|name| {
				!matches!(name.as_str(), "checkpoint" | "rewind")
					&& (may_spawn || name.as_str() != "eval")
					&& (declared_tools.is_empty()
						|| name.as_str() == "hub"
						|| declared_tools.iter().any(|allowed| allowed == *name))
			})
			.cloned()
			.collect::<Vec<_>>()
			.into();
		child_snapshot.turn.params.tools.retain(|tool| {
			!matches!(tool.name.as_str(), "checkpoint" | "rewind")
				&& (may_spawn || tool.name.as_str() != "eval")
				&& (declared_tools.is_empty()
					|| tool.name == "hub"
					|| declared_tools.iter().any(|allowed| allowed == &tool.name))
		});
		let child_env = isolated_environment
			.as_ref()
			.map_or_else(|| self.env.clone(), |environment| environment.client().clone());
		let mut child = Agent::new(
			self.client.clone(),
			child_env.clone(),
			AgentState::new(child_snapshot),
			journal,
			CHAT_CAPS_BASE,
		);
		let _control_binding = if let Some(environment) = &isolated_environment {
			let binding = environment
				.bind_agent_control(child.control())
				.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
			environment.bind_device_availability(child.mailbox());
			Some(binding)
		} else {
			None
		};
		let inbox = self
			.broker
			.register(&node, child.mailbox())
			.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
		let _hub = hub_backend::attach(Arc::new(hub_backend::ChatHubBackend::new(
			self.broker.clone(),
			inbox,
			Arc::clone(child.jobs()),
			child_env.clone(),
			id.clone(),
			context.session_id.clone(),
		)));
		let summary = match child
			.submit(
				[
					bridge_message(Role::System, system_prompt.as_str()),
					bridge_message(Role::User, prompt),
				],
				TurnId::new(format!("eval-agent-{}", omp_core::Ulid::generate())),
			)
			.await
		{
			Ok(summary) => {
				node.set_status(AgentStatus::Completed);
				summary
			},
			Err(error) => {
				node.set_status(AgentStatus::Failed);
				return Err(crate::envd::eval::BridgeHostError::message(error.to_string()));
			},
		};
		let text = summary
			.outcome
			.as_ref()
			.map_or_else(|| "(interrupted)".to_owned(), bridge_outcome_text);
		drop(_hub);
		drop(child);
		drop(child_env);
		drop(isolated_environment);
		let mut disposition = None;
		if let Some(worktree_id) = worktree_id.as_ref()
			&& (apply || merge)
		{
			let mode = if apply {
				omp_proto::env::v1::MergeMode::Patch
			} else {
				omp_proto::env::v1::MergeMode::Branch
			};
			let merged = self
				.env
				.merge_worktree(omp_proto::env::v1::MergeWorktree {
					id: worktree_id.to_string(),
					mode: mode as i32,
					..Default::default()
				})
				.await
				.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
			if !merged.conflicts.is_empty() {
				return Err(crate::envd::eval::BridgeHostError::message(
					"isolated worktree disposition reported conflicts",
				));
			}
			disposition = Some(json!({
				"mode": if apply { "patch" } else { "branch" },
				"artifactHash": omp_core::encoding::hex::encode(&merged.artifact_hash).into_string(),
				"artifactSize": merged.artifact_size,
				"branch": merged.branch,
			}));
			self
				.env
				.destroy_worktree(omp_proto::env::v1::DestroyWorktree {
					id: worktree_id.to_string(),
					force: true,
					..Default::default()
				})
				.await
				.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
		}
		let artifact_dir = context.sessions_dir.join(context.session_id.as_str());
		std::fs::create_dir_all(&artifact_dir)
			.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
		let artifact = artifact_dir.join(format!("{id}.md"));
		let temporary = artifact_dir.join(format!(".{id}.tmp"));
		std::fs::write(&temporary, text.as_bytes())
			.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
		std::fs::rename(&temporary, &artifact)
			.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
		Ok(json!({
			"text": text,
			"details": {
				"id": id,
				"agent": kind,
				"blocking": definition.blocking,
				"isolated": isolated,
				"worktree": worktree_id,
				"root": isolated.then(|| child_root.to_string_lossy().into_owned()),
				"disposition": disposition,
			},
		}))
	}

	async fn concurrency(&self, _args: Value) -> Result<Value, crate::envd::eval::BridgeHostError> {
		let context = self.context.lock();
		Ok(json!({ "limit": context.tree.max_concurrency() }))
	}

	async fn budget(&self, _args: Value) -> Result<Value, crate::envd::eval::BridgeHostError> {
		let context = self.context.lock();
		let budget = context.state.snapshot().turn.params.task_budget;
		let Some(budget) = budget else {
			return Ok(json!({ "total": null, "spent": 0, "hard": false }));
		};
		let remaining = budget.remaining_tokens.unwrap_or(budget.total_tokens);
		Ok(json!({
			"total": budget.total_tokens,
			"spent": budget.total_tokens.saturating_sub(remaining),
			"hard": budget.remaining_tokens.is_some(),
		}))
	}
}

/// Initial surface selected by the command boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChatStart {
	/// Open the inline transcript and composer immediately.
	Session,
	/// Open the alternate-screen session index before the transcript.
	SessionIndex,
}

/// Runs one interactive durable project-chat session.
#[cfg(any(unix, windows))]
#[expect(
	clippy::future_not_send,
	reason = "the interactive chat future owns a thread-confined terminal scene"
)]
pub(crate) async fn run(args: ChatArgs, start: ChatStart) -> miette::Result<()> {
	use miette::{Context as _, IntoDiagnostic as _};
	let root = canonical_project(&args.project).map_err(|e| miette::miette!(e))?;
	let data_dir = crate::cli::data_dir(None)?;
	let catalog =
		omp_llm_catalog::snapshot::Catalog::try_embedded().map_err(|e| miette::miette!(e))?;
	let settings = crate::settings::Settings::load(&data_dir);
	let interrupt_grace = settings.runtime_durations().interrupt_grace;
	let auto_thinking = settings.auto_thinking;
	let model = match args
		.model
		.clone()
		.or_else(|| settings.default_model.map(Str::from))
	{
		Some(model) => model,
		None => crate::wizard::run(&data_dir, catalog)
			.await?
			.ok_or_else(|| miette::miette!("no model configured — run `omp` again to finish setup"))?,
	};
	let model = resolve_model_selector(catalog, model.as_str()).map_err(|e| miette::miette!(e))?;
	let state_dir =
		crate::project_state::directory(&data_dir, &root).map_err(|e| miette::miette!(e))?;
	let sessions_dir = state_dir.join("sessions");
	ensure_state_directory(&state_dir).map_err(|e| miette::miette!(e))?;
	ensure_state_directory(&sessions_dir).map_err(|e| miette::miette!(e))?;
	let resume = args.resume.clone();
	let env_socket = crate::project_state::environment_socket(&state_dir);
	let document_socket = crate::project_state::document_socket(&state_dir);
	let environment = crate::envd::ProjectEnvironment::connect_or_start(
		&root,
		&state_dir,
		&env_socket,
		&document_socket,
		args.py_eval,
		interrupt_grace,
	)
	.await
	.map_err(|e| miette::miette!(e))?;
	let session_index = environment.sessions_index();
	let env = environment.client().clone();
	let eval_bridge = environment.eval_bridge();
	let eval_control = environment.eval_control();

	let registry = environment.registry();
	let session = open_session(
		&root,
		&sessions_dir,
		resume.as_ref(),
		registry.as_ref(),
		Arc::clone(&session_index),
	)
	.map_err(|e| miette::miette!(e))?;
	let mut snapshot =
		agent_snapshot(model.as_str(), catalog, &root, &session.id, Arc::clone(&registry))
			.map_err(|e| miette::miette!(e))?;
	if let Some(level) = args.thinking {
		let effort = thinking_effort(level, auto_thinking);
		snapshot.turn.params.thinking =
			Some(inference_pb::Reasoning { effort: effort as i32, ..Default::default() });
	}
	let state = AgentState::new(snapshot);

	if let Some(endpoint) = args.gateway {
		let channel = omp_rpc::uds::connect(endpoint.as_path())
			.await
			.into_diagnostic()
			.wrap_err_with(|| format!("could not connect to {endpoint}"))?;
		Box::pin(run_ui(
			RpcTurnClient::new(channel),
			&environment,
			env,
			state,
			session,
			Arc::clone(&eval_bridge),
			eval_control.clone(),
			None,
			data_dir.clone(),
			ChatScope {
				catalog,
				root: &root,
				sessions_dir: &sessions_dir,
				session_index: Arc::clone(&session_index),
				registry,
			},
			start,
		))
		.await
		.into_diagnostic()?;
	} else {
		let (inference_registry, inference) =
			crate::daemon::production_inference(&data_dir, Arc::clone(&registry))
				.await
				.into_diagnostic()?;
		let client = InProcTurnClient::new(inference)
			.await
			.map_err(ChatError::from)
			.into_diagnostic()?;
		Box::pin(run_ui(
			client,
			&environment,
			env,
			state,
			session,
			eval_bridge,
			eval_control,
			Some(inference_registry),
			data_dir,
			ChatScope {
				catalog,
				root: &root,
				sessions_dir: &sessions_dir,
				session_index: Arc::clone(&session_index),
				registry,
			},
			start,
		))
		.await
		.into_diagnostic()?;
	}

	// `environment` is deliberately retained until the agent and UI have been
	// dropped. Its Drop implementation only stops authorities this process
	// autostarted; it does not further affect any joined or draining daemon.
	drop(environment);
	Ok(())
}

/// Reports the platform limitation before touching project state.
#[cfg(not(any(unix, windows)))]
pub(crate) async fn run(_args: ChatArgs, _start: ChatStart) -> miette::Result<()> {
	use miette::IntoDiagnostic as _;
	Err(ChatError::UnsupportedPlatform).into_diagnostic()
}

#[expect(
	clippy::future_not_send,
	reason = "the designed terminal host remains confined to its event-loop thread"
)]
async fn run_ui<C: TurnClient + Clone + 'static>(
	client: C,
	environment: &crate::envd::ProjectEnvironment,
	env: omp_env::EnvClient,
	mut state: AgentState,
	mut session: Session,
	eval_bridge: Arc<crate::envd::eval::SessionBridgeHost>,
	eval_control: omp_tools::eval::EvalSessionControl,
	auth_registry: Option<InferenceRegistry>,
	data_dir: PathBuf,
	scope: ChatScope<'_>,
	mut start: ChatStart,
) -> Result<(), ChatError> {
	let parent = Arc::new(ChatParentHost::new(
		client.clone(),
		env.clone(),
		state.clone(),
		session.id.clone(),
		scope.sessions_dir.to_path_buf(),
		scope.root.to_path_buf(),
		Arc::clone(&scope.session_index),
	));
	eval_bridge
		.bind_parent(parent.clone())
		.map_err(|error| ChatError::EvalBridge(Str::from(error.to_string())))?;
	let auth = auth_registry.map(ChatAuthWorker::start);
	loop {
		parent.update(state.clone(), session.id.clone());
		let session_root = scope.sessions_dir.join(session.id.as_str());
		ensure_state_directory(&session_root)?;
		ensure_state_directory(&session_root.join("local"))?;
		eval_bridge.set_session_config(crate::envd::eval::EvalSessionConfig {
			local_roots_json: Str::from(
				json!({ "local": session_root.join("local").to_string_lossy() }).to_string(),
			),
			artifacts_dir:    Str::from(session_root.to_string_lossy().as_ref()),
			session_file:     Str::from(
				scope
					.sessions_dir
					.join(format!("{}.jsonl", session.id))
					.to_string_lossy()
					.as_ref(),
			),
		});
		let context_window = {
			let current = state.snapshot();
			model_context_window(scope.catalog, &current.turn.params.model)
		};
		let Session { id, journal, initial_items } = session;
		let current_id = id.clone();
		let mut agent =
			Agent::new(client.clone(), env.clone(), state.clone(), journal, CHAT_CAPS_BASE);
		let modes = Arc::new(crate::modes::ExecutionModes::new(agent.execution_mode()));
		state.update(|snapshot| {
			snapshot.prompt_source = modes.prompt_source(Arc::clone(&snapshot.prompt_source));
		});
		agent.set_continuation_source(modes.clone());
		let _control_binding = environment.bind_agent_control(agent.control())?;
		environment.bind_device_availability(agent.mailbox());
		let tree = parent.tree();
		let node = tree
			.register(id.clone(), sf!("Main"), AgentKind::Main, None, id.clone(), Budget::default())
			.map_err(|error| ChatError::EvalBridge(Str::from(error.to_string())))?;
		node.set_status(AgentStatus::Running);
		let broker = parent.broker();
		let inbox = broker
			.register(&node, agent.mailbox())
			.map_err(|error| ChatError::EvalBridge(Str::from(error.to_string())))?;
		let _hub = hub_backend::attach(Arc::new(hub_backend::ChatHubBackend::new(
			broker,
			inbox,
			Arc::clone(agent.jobs()),
			env.clone(),
			id.clone(),
			id.clone(),
		)));
		let _vibe = crate::vibe::attach_chat(Arc::clone(&parent), Arc::clone(&modes));
		let exit = chat_ui::run(
			agent,
			ChatUiSession { session_id: id, initial_items, context_window },
			Arc::clone(&scope.registry),
			parent.tree(),
			modes,
			auth.as_ref().map(|worker| &worker.ui),
			data_dir.clone(),
			Vec::new(),
			|| resume_choices(scope.sessions_dir, scope.root, Some(&current_id)).into_diagnostic(),
			matches!(start, ChatStart::SessionIndex),
		)
		.await
		.map_err(ChatError::Ui)?;
		start = ChatStart::Session;
		match exit {
			omp_chat_ui::host::HostExit::Quit => break,
			omp_chat_ui::host::HostExit::Resume(id) => {
				eval_control.request_reset();
				let model = state.snapshot().turn.params.model.clone();
				session = open_session(
					scope.root,
					scope.sessions_dir,
					Some(&id),
					scope.registry.as_ref(),
					Arc::clone(&scope.session_index),
				)?;
				state = AgentState::new(agent_snapshot(
					&model,
					scope.catalog,
					scope.root,
					&session.id,
					Arc::clone(&scope.registry),
				)?);
			},
			omp_chat_ui::host::HostExit::NewSession => {
				eval_control.request_reset();
				let model = state.snapshot().turn.params.model.clone();
				session = open_session(
					scope.root,
					scope.sessions_dir,
					None,
					scope.registry.as_ref(),
					Arc::clone(&scope.session_index),
				)?;
				state = AgentState::new(agent_snapshot(
					&model,
					scope.catalog,
					scope.root,
					&session.id,
					Arc::clone(&scope.registry),
				)?);
			},
		}
	}
	if let Some(auth) = auth {
		auth.shutdown().await;
	}
	Ok(())
}

fn model_context_window(catalog: &omp_llm_catalog::snapshot::Catalog, model: &str) -> Option<u64> {
	let key = omp_llm_catalog::ModelKey::from(model);
	catalog
		.model(&key)
		.or_else(|| catalog.resolve_alias(model))
		.and_then(|spec| spec.limits.context_window)
}

/// Returns whether the catalog proves the model cannot accept declared tools.
///
/// Unknown or missing capability evidence keeps tools advertised; only
/// explicit `Unsupported` evidence (e.g. Apple's on-device model) strips them.
fn model_rejects_tools(catalog: &omp_llm_catalog::snapshot::Catalog, model: &str) -> bool {
	let key = omp_llm_catalog::ModelKey::from(model);
	catalog
		.model(&key)
		.or_else(|| catalog.resolve_alias(model))
		.and_then(|spec| spec.capabilities.chat.as_ref())
		.is_some_and(|chat| chat.tools.is_unsupported())
}

/// Canonicalizes a `--model` selector to its exact catalog key.
///
/// Exact keys pass through; declared catalog aliases resolve to their target
/// key; role selectors (`@…`) defer to downstream resolution. A route id or
/// unknown selector fails fast instead of surfacing as a mid-turn
/// `TargetNotFound`.
fn resolve_model_selector(
	catalog: &omp_llm_catalog::snapshot::Catalog,
	selector: &str,
) -> Result<Str, ChatError> {
	if selector.starts_with('@')
		|| catalog
			.model(&omp_llm_catalog::ModelKey::from(selector))
			.is_some()
	{
		return Ok(selector.into());
	}
	if let Some(spec) = catalog.resolve_alias(selector) {
		return Ok(spec.key.as_str().into());
	}
	if let Some(route) = catalog.route(&omp_llm_catalog::RouteId::from(selector)) {
		// Models bound to this exact route, else every model the provider serves.
		let mut candidates: Vec<&str> = catalog
			.models()
			.iter()
			.filter(|spec| spec.routes.contains(&route.id))
			.map(|spec| spec.key.as_str())
			.collect();
		if candidates.is_empty() {
			candidates = catalog
				.models()
				.iter()
				.filter(|spec| {
					spec.routes.iter().any(|id| {
						catalog
							.route(id)
							.is_some_and(|def| def.provider == route.provider)
					})
				})
				.map(|spec| spec.key.as_str())
				.collect();
		}
		let hint = match candidates.as_slice() {
			[] => Default::default(),
			[only] => sf!("; use `--model {only}`"),
			many => sf!(
				"; provider `{}` serves: {}{}",
				route.provider,
				many[..many.len().min(4)].join(", "),
				if many.len() > 4 { ", …" } else { "" },
			),
		};
		return Err(ChatError::ModelSelectorIsRoute { selector: selector.into(), hint });
	}
	let needle = selector
		.rsplit('/')
		.next()
		.unwrap_or(selector)
		.to_ascii_lowercase();
	let mut near = catalog
		.models()
		.iter()
		.filter(|spec| !needle.is_empty() && spec.key.as_str().to_ascii_lowercase().contains(&needle))
		.map(|spec| spec.key.as_str())
		.take(4)
		.peekable();
	let suggestions = if near.peek().is_some() {
		sf!("; closest: {}", near.collect::<Vec<_>>().join(", "))
	} else {
		Default::default()
	};
	Err(ChatError::UnknownModel { selector: selector.into(), suggestions })
}

fn canonical_project(path: &Path) -> Result<PathBuf, ChatError> {
	let root = std::fs::canonicalize(path)
		.map_err(|source| ChatError::Project { path: path.to_owned(), source })?;
	if !root.is_dir() {
		return Err(ChatError::ProjectNotDirectory(root));
	}
	Ok(root)
}

fn open_session(
	root: &Path,
	sessions_dir: &Path,
	resume: Option<&Str>,
	registry: &Registry,
	session_index: Arc<SessionIndex>,
) -> Result<Session, ChatError> {
	let id = match resume {
		Some(id) => strict_session_id(id)?,
		None => Str::from(omp_core::Ulid::generate().to_string()),
	};
	let path = sessions_dir.join(format!("{}.jsonl", id.as_str()));
	let (journal, initial_items) = if resume.is_some() {
		validate_session_file(&path).map_err(|source| {
			if source.kind() == std::io::ErrorKind::NotFound {
				ChatError::MissingResume(id.clone())
			} else {
				ChatError::ProjectState { path: path.clone(), source }
			}
		})?;
		let mut journal = Journal::open(&path)?;
		let view = journal.load()?;
		if view.header().id.0 != id {
			return Err(ChatError::SessionMismatch(id));
		}
		if view.header().cwd != root {
			return Err(ChatError::SessionProjectMismatch { session: id });
		}
		let initial_items = project_journal(&view, view.as_ref(), registry, &CHAT_CAPS_BASE)?.items;
		drop(view);
		journal.attach_session_index(Arc::clone(&session_index), SessionId(id.clone()));
		(journal, initial_items)
	} else {
		let journal = create_indexed_journal(
			&path,
			root,
			&id,
			Arc::clone(&session_index),
			SessionKind::Interactive,
			None,
		)?;
		let view = journal.load()?;
		let initial_items = project_journal(&view, view.as_ref(), registry, &CHAT_CAPS_BASE)?.items;
		drop(view);
		(journal, initial_items)
	};
	Ok(Session { id, journal, initial_items })
}

fn create_indexed_journal(
	path: &Path,
	root: &Path,
	id: &Str,
	session_index: Arc<SessionIndex>,
	kind: SessionKind,
	parent: Option<&SessionId>,
) -> Result<Journal, ChatError> {
	let session_id = SessionId(id.clone());
	let created_ms = now_ms();
	let root_text = root.to_string_lossy();
	let request = NewSession {
		id: &session_id,
		cwd: root_text.as_ref(),
		project: root_text.as_ref(),
		created_ms,
		kind,
		parent,
		remote: false,
	};
	let result = session_index.create_session(&request, || {
		let journal = Journal::create(path, &Header {
			v:       4,
			id:      session_id.clone(),
			created: created_ms,
			cwd:     root.to_owned(),
		})?;
		let watermark = journal.byte_watermark()?;
		Ok::<_, omp_agent::JournalError>((journal, watermark))
	});
	let mut journal = match result {
		Ok(journal) => journal,
		Err(IndexedWriteError::Journal(error)) => return Err(error.into()),
		Err(
			IndexedWriteError::IndexBeforeJournal(error)
			| IndexedWriteError::IndexAfterJournal { source: error, .. },
		) => {
			return Err(ChatError::SessionIndex(error));
		},
	};
	journal.attach_session_index(session_index, session_id);
	Ok(journal)
}

fn resume_choices(
	sessions_dir: &Path,
	root: &Path,
	current_id: Option<&Str>,
) -> Result<Vec<ResumeChoice>, ChatError> {
	let entries = std::fs::read_dir(sessions_dir)
		.map_err(|source| ChatError::ProjectState { path: sessions_dir.to_owned(), source })?;
	let mut choices = Vec::new();
	for entry in entries {
		let Ok(entry) = entry else {
			continue;
		};
		let path = entry.path();
		if path.extension().and_then(std::ffi::OsStr::to_str) != Some("jsonl")
			|| validate_session_file(&path).is_err()
		{
			continue;
		}
		let Some(stem) = path.file_stem().and_then(std::ffi::OsStr::to_str) else {
			continue;
		};
		let id = Str::from(stem);
		if strict_session_id(&id).is_err() {
			continue;
		}
		let Some(metadata) = session_metadata(&path) else {
			continue;
		};
		if metadata.header.id.0 != id || metadata.header.cwd != root {
			continue;
		}
		// A journal holding only its header carries nothing to resume: sessions
		// are created eagerly on disk, so a launch-then-quit leaves an empty
		// shell that would resume to a blank conversation. Never advertise it
		// (pi issue #8860: only advertise resume for actually-persisted work).
		if !metadata.has_entries {
			continue;
		}
		let modified = entry
			.metadata()
			.and_then(|metadata| metadata.modified())
			.unwrap_or(UNIX_EPOCH);
		let age = relative_time(modified);
		let label = metadata.label.unwrap_or_else(|| sf!("Untitled session"));
		let detail = if current_id.is_some_and(|current| current == &id) {
			sf!("current · {age} · {id}")
		} else {
			sf!("{age} · {id}")
		};
		choices.push((modified, ResumeChoice { id, label, detail }));
	}
	choices.sort_unstable_by_key(|(modified, _)| std::cmp::Reverse(*modified));
	Ok(choices.into_iter().map(|(_, choice)| choice).collect())
}

/// Streamed session-journal probe results consumed by the resume picker.
struct SessionMetadata {
	/// Parsed first-line journal header.
	header:      Header,
	/// Best display label: latest title, else the first user prompt.
	label:       Option<Str>,
	/// Whether any decodable journal entry follows the header. Journals are
	/// created eagerly with a lone header line, so this distinguishes sessions
	/// with persisted conversation from empty shells.
	has_entries: bool,
}

fn session_metadata(path: &Path) -> Option<SessionMetadata> {
	let mut reader = BufReader::new(File::open(path).ok()?);
	let mut line = Vec::new();
	if reader.read_until(b'\n', &mut line).ok()? == 0 {
		return None;
	}
	while line
		.last()
		.is_some_and(|byte| matches!(*byte, b'\n' | b'\r'))
	{
		line.pop();
	}
	let header = read_header(&line).ok()?;
	let mut title = None;
	let mut first_message = None;
	let mut has_entries = false;
	loop {
		line.clear();
		if reader.read_until(b'\n', &mut line).ok()? == 0 {
			break;
		}
		while line
			.last()
			.is_some_and(|byte| matches!(*byte, b'\n' | b'\r'))
		{
			line.pop();
		}
		let Ok(event) = read_line(&line) else {
			continue;
		};
		has_entries = true;
		match &event.kind {
			Kind::Title { title: value, .. } => title = sanitize_session_label(value),
			Kind::Item(record) if first_message.is_none() => {
				let Some(item::Kind::Message(message)) = &record.item.kind else {
					continue;
				};
				if !matches!(Role::try_from(message.role), Ok(Role::User)) {
					continue;
				}
				first_message = message.parts.iter().find_map(|part| match &part.kind {
					Some(part::Kind::Text(text)) => sanitize_session_label(text),
					_ => None,
				});
			},
			_ => {},
		}
	}
	Some(SessionMetadata { header, label: title.or(first_message), has_entries })
}

fn sanitize_session_label(value: &str) -> Option<Str> {
	let mut clean = value.to_owned().into_ansi_stripped();
	if let Some(end) = clean.find(['\r', '\n']) {
		clean.truncate(end);
	}
	clean.retain(|character| !character.is_control());
	let clean = Str::from(clean).trim();
	(!clean.is_empty()).then_some(clean)
}

fn relative_time(modified: SystemTime) -> Str {
	let seconds = SystemTime::now()
		.duration_since(modified)
		.unwrap_or_default()
		.as_secs();
	match seconds {
		0..60 => sf!("just now"),
		60..3_600 => sf!("{}m ago", seconds / 60),
		3_600..86_400 => sf!("{}h ago", seconds / 3_600),
		86_400..604_800 => sf!("{}d ago", seconds / 86_400),
		_ => sf!("{}w ago", seconds / 604_800),
	}
}

fn strict_session_id(id: &Str) -> Result<Str, ChatError> {
	let parsed = id
		.as_str()
		.parse::<omp_core::Ulid>()
		.map_err(|_| ChatError::InvalidResume(id.clone()))?;
	if parsed.to_string() != id.as_str() {
		return Err(ChatError::InvalidResume(id.clone()));
	}
	Ok(id.clone())
}

fn agent_snapshot(
	model: &str,
	catalog: &omp_llm_catalog::snapshot::Catalog,
	root: &Path,
	session_id: &Str,
	registry: Arc<Registry>,
) -> Result<AgentSnapshot, ChatError> {
	let advertised = if model_rejects_tools(catalog, model) {
		Vec::new()
	} else {
		registry.advertise(LoweringCaps {
			strict_schema:  true,
			grammar:        GrammarBits::LARK | GrammarBits::REGEX | GrammarBits::EBNF,
			maximum_tools:  None,
			maximum_strict: None,
		})?
	};
	let mut enabled_tools = Vec::with_capacity(advertised.len());
	let mut tools = Vec::with_capacity(advertised.len());
	for tool in advertised {
		enabled_tools.push(tool.identity.name.clone());
		let (schema_json, strict) = match tool.definition.input {
			ToolInputConstraint::JsonSchema { parameters, strict } => {
				(serde_json::to_vec(parameters.as_value()).map_err(ChatError::ToolSchema)?, strict)
			},
			ToolInputConstraint::Grammar(_) => {
				return Err(ChatError::GrammarTool(tool.identity.name));
			},
		};
		tools.push(inference_pb::ToolDef {
			name:        tool.definition.name.to_string(),
			description: tool
				.definition
				.description
				.map_or_else(String::new, |value| value.to_string()),
			schema_json: schema_json.into(),
			strict:      Some(strict),
		});
	}
	let turn = TurnOptions {
		context_id: Some(session_id.clone()),
		params: inference_pb::ChatParams {
			model: model.to_owned(),
			tools,
			..inference_pb::ChatParams::default()
		},
		..TurnOptions::default()
	};
	let mut snapshot = AgentSnapshot::new(turn, WorkspaceInput::new(root, Arc::from([])), registry);
	snapshot.enabled_tools = enabled_tools.into();
	Ok(snapshot)
}

fn thinking_effort(
	level: ThinkingLevel,
	auto: crate::settings::AutoThinkingSettings,
) -> inference_pb::Effort {
	match level {
		ThinkingLevel::Off => inference_pb::Effort::Off,
		ThinkingLevel::Minimal => inference_pb::Effort::Minimal,
		ThinkingLevel::Low => inference_pb::Effort::Low,
		ThinkingLevel::Medium => inference_pb::Effort::Medium,
		ThinkingLevel::High => inference_pb::Effort::High,
		ThinkingLevel::Extreme | ThinkingLevel::Max => inference_pb::Effort::Max,
		ThinkingLevel::XHigh => inference_pb::Effort::Xhigh,
		ThinkingLevel::Auto => auto
			.for_turn()
			.provisional
			.provisional(auto.ceiling)
			.effort(),
	}
}

fn now_ms() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or_default()
		.as_millis()
		.try_into()
		.unwrap_or(u64::MAX)
}

fn ensure_state_directory(path: &Path) -> Result<(), ChatError> {
	std::fs::create_dir_all(path)
		.map_err(|source| ChatError::ProjectState { path: path.to_owned(), source })
}

fn validate_session_file(path: &Path) -> std::io::Result<()> {
	if std::fs::metadata(path)?.is_file() {
		Ok(())
	} else {
		Err(std::io::Error::new(
			std::io::ErrorKind::InvalidData,
			"session journal is not a regular file",
		))
	}
}

#[cfg(all(test, unix))]
mod tests {
	use std::collections::VecDeque;

	use futures::{Stream, stream};
	use omp_agent::{InvokeFrame, TurnSession};
	use omp_env::EnvClient;
	use omp_proto::thread::v1::{Item, Message, Part};
	use omp_storage::transcript::{Event, ItemRecord, TitleSource, Writer};

	use super::*;

	#[test]
	fn auto_thinking_installs_a_clamped_provisional_effort() {
		let auto = crate::settings::AutoThinkingSettings {
			provisional: omp_llm_inference::Difficulty::Max,
			ceiling: omp_llm_inference::Difficulty::Max,
			..crate::settings::AutoThinkingSettings::default()
		};
		assert_eq!(thinking_effort(ThinkingLevel::Auto, auto), inference_pb::Effort::High);
		assert_eq!(thinking_effort(ThinkingLevel::XHigh, auto), inference_pb::Effort::Xhigh,);
	}

	#[test]
	fn model_selector_resolution_covers_keys_aliases_routes_and_unknowns() {
		let catalog = omp_llm_catalog::snapshot::Catalog::try_embedded().expect("embedded catalog");
		assert_eq!(
			resolve_model_selector(catalog, "apple-intelligence/apple-intelligence")
				.expect("exact key resolves")
				.as_str(),
			"apple-intelligence/apple-intelligence",
		);
		assert_eq!(
			resolve_model_selector(catalog, "@smol")
				.expect("role selector passes through")
				.as_str(),
			"@smol",
		);

		// A route serving exactly one model recommends that model.
		let unique = resolve_model_selector(catalog, "apple-intelligence/primary").unwrap_err();
		let ChatError::ModelSelectorIsRoute { hint, .. } = &unique else {
			panic!("expected route error, got {unique}");
		};
		assert_eq!(hint.as_str(), "; use `--model apple-intelligence/apple-intelligence`");

		// A route shared by a multi-model provider must not recommend one
		// arbitrary model.
		let shared = resolve_model_selector(catalog, "agnes-plan/primary").unwrap_err();
		let ChatError::ModelSelectorIsRoute { hint, .. } = &shared else {
			panic!("expected route error, got {shared}");
		};
		assert!(
			hint.starts_with("; provider `agnes-plan` serves: "),
			"shared route hint lists candidates: {hint}",
		);

		let unknown = resolve_model_selector(catalog, "apple/apple-intelligence").unwrap_err();
		let ChatError::UnknownModel { suggestions, .. } = &unknown else {
			panic!("expected unknown-model error, got {unknown}");
		};
		assert!(
			suggestions.contains("apple-intelligence/apple-intelligence"),
			"suggestions name the canonical key: {suggestions}",
		);
	}

	/// Port of pi PR #8833: a provider-qualified selector must resolve within
	/// its named provider or fail closed — it must never shadow onto an
	/// aggregator's verbatim flat id (e.g. `anthropic/claude-fable-5` re-binding
	/// to `openrouter/anthropic/claude-fable-5`), which would silently bill the
	/// aggregator instead of failing a misconfigured provider.
	#[test]
	fn provider_qualified_selectors_never_shadow_onto_aggregator_flat_ids() {
		let catalog = omp_llm_catalog::snapshot::Catalog::try_embedded().expect("embedded catalog");

		// Explicit precedence pair: the same flat id exists both as a canonical
		// provider key and verbatim under the aggregator.
		let native = omp_llm_catalog::ModelKey::from("anthropic/claude-fable-5");
		let shadowed = omp_llm_catalog::ModelKey::from("openrouter/anthropic/claude-fable-5");
		assert!(catalog.model(&native).is_some(), "fixture key missing from catalog");
		assert!(catalog.model(&shadowed).is_some(), "fixture aggregator key missing from catalog");
		assert_eq!(
			resolve_model_selector(catalog, "anthropic/claude-fable-5")
				.expect("canonical provider key resolves")
				.as_str(),
			"anthropic/claude-fable-5",
			"the named provider wins over the aggregator's flat id",
		);
		assert_eq!(
			resolve_model_selector(catalog, "openrouter/anthropic/claude-fable-5")
				.expect("explicit aggregator selection resolves")
				.as_str(),
			"openrouter/anthropic/claude-fable-5",
			"an explicit aggregator prefix still selects the aggregator",
		);

		// Matrix over every aggregator-hosted flat id whose named provider is a
		// real catalog provider: the bare flat id either resolves within that
		// provider or fails closed; it never re-binds to the aggregator copy.
		// `resolve_model_selector` can only produce a model through these two
		// exact lookups (key, then declared alias) before failing closed, so the
		// matrix checks them directly instead of paying the unknown-selector
		// suggestion scan a thousand times over.
		let mut flat_ids = std::collections::BTreeSet::new();
		for spec in catalog.models() {
			let Some((_aggregator, flat_id)) = spec.key.as_str().split_once('/') else {
				continue;
			};
			let Some((named_provider, _)) = flat_id.split_once('/') else {
				continue;
			};
			if catalog
				.provider(&omp_llm_catalog::ProviderId::from(named_provider))
				.is_some()
			{
				flat_ids.insert((flat_id, named_provider));
			}
		}
		assert!(!flat_ids.is_empty(), "the catalog carries aggregator flat ids to check");
		for (flat_id, named_provider) in flat_ids {
			let resolved = catalog
				.model(&omp_llm_catalog::ModelKey::from(flat_id))
				.or_else(|| catalog.resolve_alias(flat_id));
			if let Some(spec) = resolved {
				assert!(
					spec
						.key
						.as_str()
						.strip_prefix(named_provider)
						.is_some_and(|rest| rest.starts_with('/')),
					"`{flat_id}` must stay locked to `{named_provider}`, resolved `{}`",
					spec.key.as_str(),
				);
			}
		}
	}

	#[derive(Clone)]
	struct ScriptedParentClient {
		outcomes: Arc<Mutex<VecDeque<inference_pb::Outcome>>>,
		inputs:   Arc<Mutex<Vec<TurnInput>>>,
		options:  Arc<Mutex<Vec<TurnOptions>>>,
	}

	struct ScriptedParentSession {
		events: Vec<Result<inference_pb::TurnEvent, omp_agent::Error>>,
	}

	impl TurnSession for ScriptedParentSession {
		fn events(
			&mut self,
		) -> impl Stream<Item = Result<inference_pb::TurnEvent, omp_agent::Error>> + Send + Unpin + '_
		{
			stream::iter(std::mem::take(&mut self.events))
		}

		fn submit(
			&mut self,
			_frame: InvokeFrame,
		) -> impl Future<Output = Result<(), omp_agent::Error>> + Send + '_ {
			std::future::ready(Ok(()))
		}
	}

	impl TurnClient for ScriptedParentClient {
		type Session<'client> = ScriptedParentSession;

		fn turn<'client>(
			&'client self,
			_turn_id: TurnId,
			input: TurnInput,
			options: &'client TurnOptions,
		) -> impl Future<Output = Result<Self::Session<'client>, omp_agent::Error>> + Send + 'client
		{
			self.inputs.lock().push(input);
			self.options.lock().push(options.clone());
			let outcome = self
				.outcomes
				.lock()
				.pop_front()
				.expect("one scripted parent outcome");
			std::future::ready(Ok(ScriptedParentSession {
				events: vec![Ok(inference_pb::TurnEvent {
					event: Some(inference_pb::turn_event::Event::Outcome(outcome)),
				})],
			}))
		}
	}

	fn parent_outcome(text: &str) -> inference_pb::Outcome {
		let mut output = bridge_message(Role::Assistant, text);
		output.seq = 1;
		inference_pb::Outcome {
			output: vec![output],
			stop: inference_pb::StopReason::StopEndTurn as i32,
			usage: Some(inference_pb::Usage::default()),
			cost: Some(inference_pb::Cost::default()),
			provider: "test".to_owned(),
			model: "scripted".to_owned(),
			..inference_pb::Outcome::default()
		}
	}

	fn write_session(sessions_dir: &Path, root: &Path, prompt: &str, title: Option<&str>) -> Str {
		let id = Str::from(omp_core::Ulid::generate().to_string());
		let path = sessions_dir.join(format!("{id}.jsonl"));
		let mut writer = Writer::create(&path, &Header {
			v:       4,
			id:      SessionId(id.clone()),
			created: 1,
			cwd:     root.to_owned(),
		})
		.expect("create transcript");
		writer
			.append(&Event {
				ts:   2,
				kind: Kind::Item(ItemRecord {
					item:        Item {
						seq:           0,
						created_at_ms: 2,
						kind:          Some(item::Kind::Message(Message {
							role:  i32::from(Role::User),
							parts: vec![Part { kind: Some(part::Kind::Text(prompt.to_owned())) }],
						})),
						props:         None,
					},
					turn_id:     None,
					prompt_hash: None,
				}),
			})
			.expect("append prompt");
		if let Some(title) = title {
			writer
				.append(&Event {
					ts:   3,
					kind: Kind::Title { title: Str::from(title), source: TitleSource::User },
				})
				.expect("append title");
		}
		drop(writer);
		id
	}

	#[test]
	fn chat_login_failure_names_provider_command_and_sanitized_detail() {
		use omp_llm_inference::{
			error::{Error, ErrorKind, ErrorPhase, RetryAction},
			receipt::ExecutionReceipt,
		};

		let provider = omp_llm_catalog::ProviderId::from("kimi-code");
		let error = Error::new(
			ErrorKind::Authentication,
			ErrorPhase::Authentication,
			RetryAction::Never,
			ExecutionReceipt::default(),
		)
		.status(Some(401))
		.code(sf!("invalid_grant"))
		.detail(ErrorDetail::provider(sf!("device authorization expired")));
		let ChatLoginFailure::Message(message) = chat_login_failure(&provider, &error) else {
			panic!("an authentication error is a plain login failure message");
		};
		assert!(message.contains("provider `kimi-code`"));
		assert!(message.contains("`/login kimi-code`"));
		assert!(message.contains("device authorization expired"));
		assert!(message.contains("401"));
		assert!(message.contains("invalid_grant"));
	}

	#[test]
	fn project_state_is_external_and_accepts_standard_permissions() {
		use std::os::unix::fs::PermissionsExt as _;

		let scratch = tempfile::tempdir().expect("scratch directory");
		let root = scratch.path().join("project");
		let metadata_dir = root.join(".omp");
		std::fs::create_dir_all(&metadata_dir).expect("project metadata");
		std::fs::set_permissions(&metadata_dir, std::fs::Permissions::from_mode(0o755))
			.expect("standard project metadata permissions");

		let state_dir = crate::project_state::directory(&scratch.path().join("data"), &root)
			.expect("project state path");
		let sessions_dir = state_dir.join("sessions");
		ensure_state_directory(&sessions_dir).expect("project state");
		std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o755))
			.expect("standard project state permissions");
		std::fs::set_permissions(&sessions_dir, std::fs::Permissions::from_mode(0o755))
			.expect("standard session directory permissions");
		ensure_state_directory(&state_dir).expect("existing project state directory");
		ensure_state_directory(&sessions_dir).expect("existing session directory");

		assert!(!state_dir.starts_with(&root));
		assert_eq!(
			std::fs::metadata(&metadata_dir)
				.expect("project metadata")
				.permissions()
				.mode() & 0o777,
			0o755
		);
		assert_eq!(
			std::fs::metadata(&state_dir)
				.expect("project state")
				.permissions()
				.mode() & 0o777,
			0o755
		);

		let id = write_session(&sessions_dir, &root, "resume me", None);
		let path = sessions_dir.join(format!("{id}.jsonl"));
		std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
			.expect("standard journal permissions");
		let session = open_session(
			&root,
			&sessions_dir,
			Some(&id),
			&Registry::new(),
			Arc::new(SessionIndex::open(state_dir.join("sessions.sqlite3")).expect("session index")),
		)
		.expect("resume session");
		assert_eq!(session.id, id);
		assert_eq!(
			std::fs::metadata(path)
				.expect("session journal")
				.permissions()
				.mode() & 0o777,
			0o644
		);
	}

	#[test]
	fn resume_choices_use_titles_then_prompts_and_strip_terminal_controls() {
		let scratch = tempfile::tempdir().expect("scratch directory");
		let root = scratch.path().join("project");
		let sessions_dir = root.join("sessions");
		std::fs::create_dir_all(&sessions_dir).expect("session directory");
		let prompt_id = write_session(&sessions_dir, &root, "  first prompt\nignored", None);
		let titled_id = write_session(
			&sessions_dir,
			&root,
			"unused prompt",
			Some("\u{1b}[31mRenamed\u{1b}[0m\nignored"),
		);

		let choices = resume_choices(&sessions_dir, &root, Some(&titled_id)).expect("list sessions");
		assert_eq!(choices.len(), 2);
		let prompt = choices
			.iter()
			.find(|choice| choice.id == prompt_id)
			.expect("prompt-named session");
		assert_eq!(prompt.label, "first prompt");
		let titled = choices
			.iter()
			.find(|choice| choice.id == titled_id)
			.expect("title-named session");
		assert_eq!(titled.label, "Renamed");
		assert!(titled.detail.starts_with("current · "));
	}

	#[test]
	fn session_metadata_streams_past_torn_records_and_keeps_latest_title() {
		let scratch = tempfile::tempdir().expect("scratch directory");
		let root = scratch.path().join("project");
		let sessions_dir = root.join("sessions");
		std::fs::create_dir_all(&sessions_dir).expect("session directory");
		let id = write_session(&sessions_dir, &root, "first prompt", Some("Early title"));
		let path = sessions_dir.join(format!("{id}.jsonl"));

		// A malformed mid-file record, a later title, and a torn trailing append
		// must not stop the streamed probe or lose title updates behind them.
		let mut fixture = Vec::new();
		fixture.extend_from_slice(b"{not json}\n");
		omp_storage::transcript::write_line(
			&Event {
				ts:   4,
				kind: Kind::Title { title: sf!("Recovered title"), source: TitleSource::User },
			},
			&mut fixture,
		)
		.expect("title line encodes");
		fixture.extend_from_slice(b"\n{\"ts\":5,\"k\":\"title\",\"title\":\"torn");
		let mut file = std::fs::OpenOptions::new()
			.append(true)
			.open(&path)
			.expect("append fixture");
		std::io::Write::write_all(&mut file, &fixture).expect("append torn records");
		drop(file);

		let metadata = session_metadata(&path).expect("probe survives torn records");
		assert_eq!(metadata.header.id.0, id);
		assert_eq!(metadata.label.expect("latest title wins").as_str(), "Recovered title");
		assert!(metadata.has_entries, "real entries behind torn records still count");
	}

	/// Port of pi issue #8860: never advertise resuming a session that has no
	/// persisted conversation. Journals are created eagerly with a lone header
	/// line, so a launch-then-quit leaves an empty shell on disk; the resume
	/// picker must skip it until an actual journal entry lands.
	#[test]
	fn resume_choices_skip_header_only_sessions() {
		let scratch = tempfile::tempdir().expect("scratch directory");
		let root = scratch.path().join("project");
		let sessions_dir = root.join("sessions");
		std::fs::create_dir_all(&sessions_dir).expect("session directory");

		// An eagerly created, immediately abandoned session: header only.
		let empty_id = Str::from(omp_core::Ulid::generate().to_string());
		let empty_path = sessions_dir.join(format!("{empty_id}.jsonl"));
		drop(
			Writer::create(&empty_path, &Header {
				v:       4,
				id:      SessionId(empty_id.clone()),
				created: 1,
				cwd:     root.clone(),
			})
			.expect("create header-only transcript"),
		);
		let probe = session_metadata(&empty_path).expect("header-only journal still probes");
		assert!(!probe.has_entries, "a lone header carries no entries");

		// A session with persisted conversation is still advertised.
		let real_id = write_session(&sessions_dir, &root, "kept prompt", None);

		let choices = resume_choices(&sessions_dir, &root, None).expect("list sessions");
		assert_eq!(choices.len(), 1, "header-only session must not be advertised");
		assert_eq!(choices[0].id, real_id);

		// The current session is not exempt: an empty current session resumes
		// to nothing and must not be offered either.
		let current = resume_choices(&sessions_dir, &root, Some(&empty_id)).expect("list sessions");
		assert!(current.iter().all(|choice| choice.id != empty_id));
	}

	#[test]
	fn session_metadata_rejects_files_without_a_valid_header() {
		let scratch = tempfile::tempdir().expect("scratch directory");
		let empty = scratch.path().join("empty.jsonl");
		std::fs::write(&empty, b"").expect("empty fixture");
		assert!(session_metadata(&empty).is_none());

		let garbage = scratch.path().join("garbage.jsonl");
		std::fs::write(&garbage, b"{not a header}\n{\"ts\":1,\"k\":\"reset\"}\n")
			.expect("garbage fixture");
		assert!(session_metadata(&garbage).is_none());
	}

	#[test]
	fn resume_repairs_torn_trailing_append() {
		let scratch = tempfile::tempdir().expect("scratch directory");
		let root = scratch.path().join("project");
		let sessions_dir = root.join("sessions");
		std::fs::create_dir_all(&sessions_dir).expect("session directory");
		let id = write_session(&sessions_dir, &root, "resume me", None);
		let path = sessions_dir.join(format!("{id}.jsonl"));
		let mut file = std::fs::OpenOptions::new()
			.append(true)
			.open(&path)
			.expect("append torn fragment");
		std::io::Write::write_all(&mut file, br#"{"ts":9,"k":"title","title":"tor"#)
			.expect("write torn fragment");
		drop(file);

		let session = open_session(
			&root,
			&sessions_dir,
			Some(&id),
			&Registry::new(),
			Arc::new(
				SessionIndex::open(scratch.path().join("sessions.sqlite3")).expect("session index"),
			),
		)
		.expect("torn session resumes");
		assert_eq!(session.id, id);
		let log = session.journal.load().expect("repaired journal loads");
		assert_eq!(log.len(), 1, "the torn fragment is truncated, intact events remain");
	}

	#[tokio::test]
	async fn session_bound_parent_runs_live_completion_and_agent_turns() {
		let scratch = tempfile::tempdir().expect("chat parent scratch");
		let root = scratch.path().join("project");
		let sessions_dir = root.join("sessions");
		std::fs::create_dir_all(&sessions_dir).expect("session directory");
		let inputs = Arc::new(Mutex::new(Vec::new()));
		let options = Arc::new(Mutex::new(Vec::new()));
		let client = ScriptedParentClient {
			outcomes: Arc::new(Mutex::new(VecDeque::from([
				parent_outcome("completion answer"),
				parent_outcome("agent answer"),
			]))),
			inputs:   Arc::clone(&inputs),
			options:  Arc::clone(&options),
		};
		let registry = Arc::new(Registry::new());
		let mut snapshot = AgentSnapshot::new(
			TurnOptions::default(),
			WorkspaceInput::new(&root, Arc::from([])),
			registry,
		);
		snapshot.enabled_tools = Arc::from([sf!("eval")]);
		let state = AgentState::new(snapshot);
		let (env, _transport) = EnvClient::in_process(1);
		let host = ChatParentHost::new(
			client,
			env,
			state,
			sf!("parent-session"),
			sessions_dir,
			root,
			Arc::new(
				SessionIndex::open(scratch.path().join("sessions.sqlite3")).expect("session index"),
			),
		);

		let completion = crate::envd::eval::ParentSessionHost::completion(
			&host,
			json!({"prompt":"complete this","model":"default"}),
		)
		.await
		.expect("live completion call");
		assert_eq!(completion, json!({"text":"completion answer"}));

		let concurrency = crate::envd::eval::ParentSessionHost::concurrency(&host, json!({}))
			.await
			.expect("concurrency bridge call");
		assert_eq!(concurrency, json!({ "limit": DEFAULT_EVAL_CONCURRENCY_LIMIT }));

		let agent = tokio::time::timeout(
			std::time::Duration::from_secs(1),
			crate::envd::eval::ParentSessionHost::agent(
				&host,
				json!({"prompt":"delegate this","agent":"task"}),
			),
		)
		.await
		.expect("child agent must not deadlock on the occupied parent eval kernel")
		.expect("live agent call");
		assert_eq!(agent["text"], "agent answer");
		assert_eq!(agent["details"]["agent"], "task");
		assert!(
			agent["details"]["id"]
				.as_str()
				.is_some_and(|id| !id.is_empty()),
			"agent bridge did not return its durable child id"
		);

		let options = options.lock();
		assert_eq!(options.len(), 2);
		assert!(
			options[1]
				.params
				.tools
				.iter()
				.all(|tool| tool.name != "eval"),
			"child agent must not advertise the parent's occupied eval kernel"
		);
		drop(options);
		let inputs = inputs.lock();
		assert_eq!(inputs.len(), 2);
		assert!(matches!(&inputs[0], TurnInput::Full(thread)
			if bridge_outcome_text(&inference_pb::Outcome {
				output: thread.items.clone(),
				..inference_pb::Outcome::default()
			}) == "complete this"
		));
		assert!(matches!(&inputs[1], TurnInput::Full(thread)
			if thread.items.iter().any(|item| matches!(
				&item.kind,
				Some(item::Kind::Message(message))
					if message.role == i32::from(Role::User)
						&& message.parts.iter().any(|part| matches!(
							&part.kind,
							Some(part::Kind::Text(text)) if text == "delegate this"
						))
			))
		));
	}
}
