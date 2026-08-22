//! Durable project-chat composition.

mod agents;
#[path = "chat_hub.rs"]
mod hub_backend;

pub(crate) fn chat_hub_tool() -> impl omp_tool::Tool {
	hub_backend::tool()
}

use std::{
	collections::{BTreeMap, BTreeSet},
	fs::File,
	io::{BufRead as _, BufReader},
	path::{Path, PathBuf},
	pin::Pin,
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
	Agent, AgentKind, AgentNode, AgentSnapshot, AgentState, AgentStatus, AgentTree, Budget,
	ChildKind, CompletionError, CompletionRequest, InProcTurnClient, Journal,
	MAX_YIELD_SCHEMA_RETRIES, RegistryStatus, RpcTurnClient, SubagentDisposition, SubagentLifecycle,
	SubagentProgressSnapshot, SubagentTerminalKind, SubagentTerminalStatus, TurnClient, TurnId,
	TurnInput, TurnOptions, TurnSession as _, WorkspaceInput, WorkspaceRootInput,
	WorkspaceRootsInput, YieldPayloadValidator, project_journal, resolve_completion,
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
use omp_sdk::{SessionBlueprint, SessionBuilder, SessionOptions};
use omp_storage::{
	blob::BlobStore,
	index::{IndexedWriteError, NewSession, SessionIndex, SessionKind},
	transcript::{Header, Kind, SessionId, read_header, read_line},
};
use omp_tool::{CapsBase, LoweringCaps, ModelClass, Registry};
use parking_lot::Mutex;
use prost::Message as _;
use serde_json::{Value, json};
use thiserror::Error;
use xutf::IntoAnsiStripped as _;

use crate::{
	chat_ui::{
		self, AuthPromptKind, ChatAuth, ChatAuthCommand, ChatAuthEvent, ChatUiSession, ResumeChoice,
	},
	cli::{ChatArgs, ThinkingLevel},
};

pub(crate) const CHAT_CAPS_BASE: CapsBase = CapsBase {
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
	/// The requested resume identity is not a canonical ULID or lowercase UUID.
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
	/// Durable compaction blob placement could not be initialized.
	#[error(transparent)]
	Blob(#[from] omp_storage::blob::Error),
	/// Session artifact metadata authority could not be initialized.
	#[error(transparent)]
	Artifact(#[from] omp_storage::gc::Error),
	/// Owner-local session discovery state failed.
	#[error(transparent)]
	SessionResolve(#[from] crate::project_state::SessionResolveError),
	/// Owner-local draft persistence failed.
	#[error(transparent)]
	Draft(#[from] crate::session_manager::DraftError),
	/// Cross-process loop revival failed.
	#[error(transparent)]
	Revival(#[from] omp_agent::RevivalError),
	/// The authoritative write-time sessions index failed.
	#[error(transparent)]
	SessionIndex(#[from] omp_storage::index::Error),
	/// A durable session was requested without an authoritative write-time
	/// index.
	#[error("durable session storage has no authoritative index")]
	MissingSessionIndex,
	/// A durable transcript could not be projected into canonical replay items.
	#[error(transparent)]
	Projection(#[from] omp_agent::ProjectionError),
	/// The project environment authority failed to start or connect.
	#[error(transparent)]
	Environment(#[from] crate::envd::EnvdError),
	/// The in-process turn authority could not be constructed.
	#[error(transparent)]
	TurnClient(#[from] omp_agent::Error),
	/// Typed settings projection failed while composing a session boundary.
	#[error(transparent)]
	Settings(#[from] crate::settings::manager::SettingsManagerError),
	/// The session-local secret transform could not be assembled.
	#[error(transparent)]
	Secrets(#[from] crate::secrets::session::SecretSessionError),
	/// A live tool declaration could not be represented on the turn protocol.
	#[error("tool {0} uses a grammar input unsupported by the turn protocol")]
	GrammarTool(Str),
	/// A tool schema could not be encoded for the turn protocol.
	#[error("could not encode tool schema")]
	ToolSchema(#[source] serde_json::Error),
	/// A requested tool is absent after native and extension discovery.
	#[error("unknown tool `{name}`; valid tools: {valid:?}")]
	UnknownTool {
		/// Requested normalized name.
		name:  Str,
		/// Fully discovered valid names.
		valid: Vec<Str>,
	},
	/// The live tool registry could not lower its advertised slots.
	#[error(transparent)]
	ToolRegistry(#[from] omp_tool::RegistryError),
	/// Shared SDK session planning failed before loop construction.
	#[error(transparent)]
	SessionBuild(#[from] omp_sdk::SessionBuildError),
	/// Process-global parked-session discovery failed.
	#[error(transparent)]
	AgentRegistry(#[from] omp_agent::RegistryError),
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
	/// The selected model has no route for the requested credential provider.
	#[error("model `{model}` is not served by provider `{provider}`")]
	ModelProviderUnavailable {
		/// Canonical selected model.
		model:    Str,
		/// Provider requested or selected for the invocation credential.
		provider: omp_llm_catalog::ProviderId,
	},
	/// The selected model has no concrete provider route.
	#[error("model `{model}` has no provider route")]
	ModelHasNoProvider {
		/// Canonical selected model.
		model: Str,
	},
	/// The session-scoped eval parent bridge could not be bound.
	#[error("eval session bridge failed: {0}")]
	EvalBridge(Str),
	/// The session-scoped memory reflection bridge could not be bound.
	#[error(transparent)]
	MemoryReflection(#[from] crate::memory::ReflectionBindingError),
	/// Mnemopi prompt snapshot construction failed.
	#[error(transparent)]
	Memory(#[from] omp_memory::Error),
	/// The interactive terminal shell failed.
	#[error("interactive chat shell failed: {0}")]
	Ui(miette::Report),
	/// Startup automation mode conflicts with the active execution state.
	#[error(transparent)]
	Mode(#[from] crate::modes::RegimeError),
	/// Campaign recovery or durable lifecycle mutation failed.
	#[error(transparent)]
	Campaign(#[from] omp_agent::AgentError),
	/// The platform cannot enforce the Phase 3 owner-local environment contract.
	#[error("interactive chat requires Unix owner-local project authorities")]
	UnsupportedPlatform,
}

#[derive(Debug, Error)]
enum ChildInitError {
	#[error(transparent)]
	Blob(#[from] omp_storage::blob::Error),
	#[error(transparent)]
	Journal(#[from] omp_agent::JournalError),
	#[error("child output schema could not be encoded")]
	Schema(#[source] serde_json::Error),
	#[error("child workspace root cannot be represented as a file URI")]
	WorkspaceRoot,
}

pub(crate) struct Session {
	pub(crate) id:            Str,
	pub(crate) journal:       Journal,
	pub(crate) initial_items: Vec<omp_proto::thread::v1::Item>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum SessionOpen<'a> {
	New,
	Resume(&'a Str),
	ResumeMoved(&'a Str),
	Fork(&'a Str),
	Ephemeral,
}

struct EphemeralSessions {
	path: Option<PathBuf>,
}

impl EphemeralSessions {
	fn create() -> Result<Self, ChatError> {
		let path = std::env::temp_dir()
			.join("omp")
			.join("sessions")
			.join(omp_core::Ulid::generate().to_string());
		ensure_state_directory(&path)?;
		Ok(Self { path: Some(path) })
	}

	fn path(&self) -> &Path {
		self
			.path
			.as_deref()
			.expect("ephemeral session path remains live")
	}
}

impl Drop for EphemeralSessions {
	fn drop(&mut self) {
		if let Some(path) = self.path.take() {
			let _ = std::fs::remove_dir_all(path);
		}
	}
}

struct ChatScope<'a> {
	catalog:          &'a omp_llm_catalog::snapshot::Catalog,
	root:             &'a Path,
	sessions_dir:     &'a Path,
	session_index:    Arc<SessionIndex>,
	registry:         Arc<Registry>,
	persist_sessions: bool,
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

#[must_use]
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
	provider: &omp_llm_catalog::ProviderId<str>,
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
	provider: &omp_llm_catalog::ProviderId<str>,
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
		let provider = omp_llm_catalog::ProviderId::from_ref("test-provider");
		assert!(matches!(
			chat_login_failure(provider, &error),
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
fn discover_chat_agents(
	root: &Path,
	security_enabled: bool,
) -> Arc<BTreeMap<Str, omp_agent::AgentDefinition>> {
	agents::discover(root, security_enabled)
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
	task_settings: crate::subagent::settings::LiveTaskSettings,
	campaigns:     Option<Arc<crate::modes::CampaignHandle>>,
}
/// Core-backed facts consumed by the retained agent-hub presentation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AgentHubFacts {
	/// Stable agent identity.
	pub id:                 Str,
	/// Session-local display name.
	pub name:               Str,
	/// Parent identity, absent for the session root.
	pub parent:             Option<Str>,
	/// Tree depth, with the session root at zero.
	pub depth:              u16,
	/// Definition badge shown for delegated agents.
	pub definition:         Option<Str>,
	/// Requested model role or selector.
	pub model:              Option<Str>,
	/// Model which actually served the latest request.
	pub serving_model:      Option<Str>,
	/// Deterministic assignment summary recovered from the journal.
	pub assignment:         Option<Str>,
	/// Bounded terminal or activity preview.
	pub transcript_preview: Option<Str>,
	/// Core roster lifecycle.
	pub status:             AgentStatus,
	/// Retained supervisor lifecycle, when this process owns the child.
	pub lifecycle:          Option<SubagentLifecycle>,
	/// Request/tool/usage/context/model counters retained by core.
	pub progress:           Option<SubagentProgressSnapshot>,
	/// Structured terminal result retained across listener detach and revival.
	pub terminal:           Option<SubagentTerminalStatus>,
	/// Actions allowed by the current lifecycle.
	pub capabilities:       AgentHubCapabilities,
}

/// Lifecycle-derived controls for one retained agent-hub row.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct AgentHubCapabilities {
	/// An active turn may receive an immediate steer.
	pub steer:  bool,
	/// A settled or cold identity may run a follow-up turn.
	pub revive: bool,
	/// A live active generation may be cancelled.
	pub kill:   bool,
}

pub(crate) struct ChatParentHost<C: TurnClient + Clone + Send + 'static> {
	client:     C,
	env:        omp_env::EnvClient,
	broker:     omp_agent::Broker,
	supervisor: Arc<crate::subagent::supervisor::SessionSupervisor<C>>,
	context:    Mutex<ChatParentContext>,
	revival:    Mutex<BTreeMap<Str, flume::Sender<omp_agent::RevivalRequest>>>,
}
struct ProductionChildReviver<C: TurnClient + Clone + Send + 'static> {
	client:         C,
	base_env:       omp_env::EnvClient,
	broker:         omp_agent::Broker,
	supervisor:     Arc<crate::subagent::supervisor::SessionSupervisor<C>>,
	node:           Arc<AgentNode>,
	snapshot:       AgentSnapshot,
	journal_path:   PathBuf,
	project_root:   PathBuf,
	workspace_root: PathBuf,
	isolated_state: Option<PathBuf>,
	session_index:  Arc<SessionIndex>,
	parent_session: SessionId,
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecoveredChildGrants {
	enabled_tools: Vec<Str>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecoveredChildPolicy {
	defer_interrupts: bool,
	retry:            RecoveredRetryPolicy,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecoveredRetryPolicy {
	max_attempts:       u32,
	initial_backoff_ms: u64,
	max_backoff_ms:     u64,
}

impl<C: TurnClient + Clone + Send + 'static> crate::subagent::supervisor::ChildReviver<C>
	for ProductionChildReviver<C>
{
	fn revive(&self) -> crate::subagent::supervisor::RevivalFuture<C> {
		let client = self.client.clone();
		let base_env = self.base_env.clone();
		let broker = self.broker.clone();
		let supervisor = Arc::clone(&self.supervisor);
		let node = Arc::clone(&self.node);
		let snapshot = self.snapshot.clone();
		let journal_path = self.journal_path.clone();
		let project_root = self.project_root.clone();
		let workspace_root = self.workspace_root.clone();
		let isolated_state = self.isolated_state.clone();
		let session_index = Arc::clone(&self.session_index);
		let parent_session = self.parent_session.clone();
		Box::pin(async move {
			let isolated_environment = if let Some(state) = isolated_state {
				Some(
					crate::envd::ProjectEnvironment::isolated(&workspace_root, &state)
						.await
						.map_err(|error| {
							tracing::warn!(agent = %node.id, %error, "isolated child revival failed");
							crate::subagent::supervisor::SupervisorError::RevivalFailed {
								id: node.id.clone(),
							}
						})?,
				)
			} else {
				None
			};
			let child_env = isolated_environment
				.as_ref()
				.map_or_else(|| base_env.clone(), |environment| environment.client().clone());
			let journal = create_indexed_journal(
				&journal_path,
				&project_root,
				&node.id,
				session_index,
				SessionKind::Subagent,
				Some(&parent_session),
			)
			.map_err(|error| {
				tracing::warn!(agent = %node.id, %error, "child journal revival failed");
				crate::subagent::supervisor::SupervisorError::RevivalFailed { id: node.id.clone() }
			})?;
			let child_content = crate::discovery::active_content_snapshots(&workspace_root);
			let (ttsr, diagnostics) = crate::rulebook::ttsr_registry(child_content.rules.as_ref());
			for error in diagnostics {
				tracing::warn!(%error, agent = %node.id, "revived subagent TTSR rule was rejected");
			}
			let mut child = Agent::new(
				client,
				child_env.clone(),
				AgentState::new(snapshot),
				journal,
				CHAT_CAPS_BASE,
			);
			child.set_ttsr_registry(ttsr);
			let control_binding = if let Some(environment) = &isolated_environment {
				let binding = environment
					.bind_agent_control(child.control())
					.map_err(|error| {
						tracing::warn!(agent = %node.id, %error, "revived child control bind failed");
						crate::subagent::supervisor::SupervisorError::RevivalFailed {
							id: node.id.clone(),
						}
					})?;
				environment.bind_device_availability(child.mailbox());
				Some(binding)
			} else {
				None
			};
			let revision = broker
				.registry()
				.record(node.id.as_str())
				.map(|(_, revision)| revision)
				.ok_or_else(|| crate::subagent::supervisor::SupervisorError::RevivalFailed {
					id: node.id.clone(),
				})?;
			let inbox = broker
				.attach_live(node.id.as_str(), revision, child.mailbox())
				.map_err(|error| {
					tracing::warn!(agent = %node.id, %error, "revived child broker bind failed");
					crate::subagent::supervisor::SupervisorError::RevivalFailed { id: node.id.clone() }
				})?;
			let hub = hub_backend::attach_for(
				node.id.clone(),
				Arc::new(hub_backend::ChatHubBackend::new(
					broker,
					inbox,
					Arc::clone(child.jobs()),
					child_env,
					node.id.clone(),
					Str::new(parent_session.0.as_str()),
					None,
					Some(supervisor),
				)),
			);
			let mut runtime = crate::subagent::supervisor::SupervisedRuntime::new(child);
			if let Some(binding) = control_binding {
				runtime.retain(binding);
			}
			runtime.retain(hub);
			if let Some(environment) = isolated_environment {
				runtime.retain(environment);
			}
			Ok(runtime)
		})
	}
}

impl<C: TurnClient + Clone + Send + 'static> ChatParentHost<C> {
	pub(crate) fn new(
		client: C,
		env: omp_env::EnvClient,
		state: AgentState,
		session_id: Str,
		sessions_dir: PathBuf,
		root: PathBuf,
		session_index: Arc<SessionIndex>,
		security_enabled: bool,
	) -> Self {
		let definitions = discover_chat_agents(&root, security_enabled);
		let tree = Arc::new(AgentTree::new(
			8,
			DEFAULT_EVAL_CONCURRENCY_LIMIT,
			omp_agent::DEFAULT_MAX_ADMISSION_QUEUE,
		));
		if let Err(error) = crate::subagent::artifacts::reserve_historical_stems(
			tree.as_ref(),
			&sessions_dir.join("eval-agents"),
		) {
			tracing::warn!(error = %error, "could not reserve historical subagent artifact names");
		}
		let supervisor =
			Arc::new(crate::subagent::supervisor::SessionSupervisor::new(Arc::clone(&tree)));
		Self {
			client,
			env,
			broker: omp_agent::Broker::new(Str::from(root.to_string_lossy().as_ref())),
			supervisor,
			context: Mutex::new(ChatParentContext {
				state,
				session_id,
				sessions_dir,
				root,
				session_index,
				definitions,
				task_settings: crate::subagent::settings::LiveTaskSettings::new(
					Arc::new(crate::subagent::settings::TaskSettings::default()),
					Arc::clone(&tree),
				),
				campaigns: None,
				tree,
			}),
			revival: Mutex::new(BTreeMap::new()),
		}
	}

	/// Applies a reloaded task projection to admission and later child
	/// snapshots.
	pub(crate) fn apply_task_settings(
		&self,
		settings: Arc<crate::subagent::settings::TaskSettings>,
	) {
		self.supervisor.apply_settings(Arc::clone(&settings));
		self.context.lock().task_settings.apply(settings);
	}

	fn bind_campaigns(&self, campaigns: Arc<crate::modes::CampaignHandle>) {
		self.context.lock().campaigns = Some(campaigns);
	}

	fn approved_plan_reference(&self) -> Option<crate::plan::OverallPlanReference> {
		let context = self.context.lock();
		let state = context.campaigns.as_ref()?.plan()?;
		if state.enabled {
			return None;
		}
		let store = crate::plan::PlanArtifactStore::new(
			context
				.sessions_dir
				.join(context.session_id.as_str())
				.join("local"),
		);
		let artifact = store.resolve(None, state.artifact.as_str()).ok()?;
		crate::plan::OverallPlanReference::resolve(&state, &artifact).ok()
	}

	fn bind_parent_jobs(&self, jobs: Arc<omp_agent::JobBoard>) {
		self.supervisor.bind_parent_jobs(jobs);
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

	pub(crate) fn task_settings(&self) -> Arc<crate::subagent::settings::TaskSettings> {
		self.context.lock().task_settings.snapshot()
	}

	pub(crate) fn job_board(&self) -> Option<Arc<omp_agent::JobBoard>> {
		self.supervisor.parent_jobs()
	}

	pub(crate) fn child_registry_status(&self, id: &str) -> Option<RegistryStatus> {
		self
			.broker
			.registry()
			.record(id)
			.map(|(record, _)| record.status)
	}

	/// Projects typed retained facts without granting the UI execution
	/// authority.
	pub(crate) fn agent_hub_facts(&self, session: &str) -> Vec<AgentHubFacts> {
		let tree = Arc::clone(&self.context.lock().tree);
		tree
			.roster()
			.filter(|node| node.session == session)
			.map(|node| {
				let record = self
					.broker
					.registry()
					.record(node.id.as_str())
					.map(|(record, _)| record);
				let state = self.supervisor.state(node.id.as_str());
				let lifecycle = state.as_ref().map(|state| state.lifecycle());
				let terminal = state
					.as_ref()
					.and_then(|state| state.terminal())
					.or_else(|| {
						record
							.as_ref()
							.and_then(|record| record.history.terminal.clone())
					});
				let is_child = node.kind == AgentKind::Subagent;
				let capabilities = AgentHubCapabilities {
					steer:  is_child
						&& matches!(
							lifecycle,
							Some(
								SubagentLifecycle::Starting
									| SubagentLifecycle::Running
									| SubagentLifecycle::Waiting
							)
						),
					revive: is_child
						&& matches!(
							lifecycle,
							Some(SubagentLifecycle::Parked | SubagentLifecycle::Settled)
						),
					kill:   is_child
						&& matches!(
							lifecycle,
							Some(
								SubagentLifecycle::Starting
									| SubagentLifecycle::Running
									| SubagentLifecycle::Waiting
							)
						),
				};
				AgentHubFacts {
					id: node.id.clone(),
					name: node.name.clone(),
					parent: node.parent.clone(),
					depth: node.depth,
					definition: record
						.as_ref()
						.and_then(|record| record.definition.clone())
						.or_else(|| node.definition.clone()),
					model: record.as_ref().and_then(|record| record.model.clone()),
					serving_model: state
						.as_ref()
						.and_then(|state| state.progress().serving_model)
						.or_else(|| {
							record
								.as_ref()
								.and_then(|record| record.serving_model.clone())
						}),
					assignment: record.as_ref().and_then(|record| record.task.clone()),
					transcript_preview: terminal
						.clone()
						.and_then(|terminal| {
							terminal
								.disposition
								.preview
								.or_else(|| (!terminal.summary.is_empty()).then_some(terminal.summary))
						})
						.or_else(|| {
							let activity = node.activity();
							(!activity.is_empty()).then_some(activity)
						}),
					status: node.status(),
					lifecycle,
					progress: state.as_ref().map(|state| state.progress()),
					terminal,
					capabilities,
				}
			})
			.collect()
	}

	pub(crate) fn cancel_child(&self, id: &str) {
		let _ = self.supervisor.cancel(id);
	}

	fn ensure_revival_transport(&self, id: &Str) {
		if self.revival.lock().contains_key(id) {
			return;
		}
		let (sender, receiver) = flume::unbounded::<omp_agent::RevivalRequest>();
		self.revival.lock().insert(id.clone(), sender);
		let child_id = id.clone();
		let supervisor = Arc::clone(&self.supervisor);
		let broker = self.broker.clone();
		drop(tokio::spawn(async move {
			while let Ok(request) = receiver.recv_async().await {
				if request.recipient != child_id {
					continue;
				}
				let result = supervisor
					.run(
						child_id.as_str(),
						vec![omp_agent::peer_item(&request.message)],
						TurnId::new(format!("agent-revival-{}", omp_core::Ulid::generate())),
					)
					.await;
				let _ = broker.set_idle(child_id.as_str(), true);
				if let Some(terminal) = supervisor
					.state(child_id.as_str())
					.and_then(|state| state.terminal())
				{
					let _ = broker.registry().set_terminal(child_id.as_str(), terminal);
				}
				if let Err(error) = result {
					tracing::warn!(agent = %child_id, %error, "cold-revived child turn failed");
				}
			}
		}));
	}

	fn bind_parked_transport(&self, record: omp_agent::AgentRecord) {
		let sender = self.revival.lock().get(&record.id).cloned();
		let Some(sender) = sender else {
			return;
		};
		self.broker.unregister(record.id.as_str());
		if let Err(error) = self.broker.register_parked(record.clone(), sender) {
			tracing::warn!(agent = %record.id, %error, "parked child revival bind failed");
		}
	}

	async fn recover_parked_children(&self) {
		let context = self.context.lock().clone();
		let directory = context.sessions_dir.join("eval-agents");
		if !directory.is_dir() {
			return;
		}
		if let Err(error) = self.broker.registry().discover_transcripts(&directory) {
			tracing::warn!(%error, "durable child transcript discovery failed");
			return;
		}
		let blob_root = context
			.sessions_dir
			.parent()
			.unwrap_or(context.sessions_dir.as_path());
		let blob_store = match BlobStore::open(blob_root) {
			Ok(store) => store,
			Err(error) => {
				tracing::warn!(%error, "durable child blob store could not be opened");
				return;
			},
		};
		for record in self.broker.registry().roster(false) {
			if record.kind != AgentKind::Subagent
				|| record.parent.as_deref() != Some(context.session_id.as_str())
				|| self.supervisor.state(record.id.as_str()).is_some()
			{
				continue;
			}
			if let Err(error) = self
				.recover_parked_child(&context, &blob_store, record.clone())
				.await
			{
				tracing::warn!(agent = %record.id, %error, "durable child was not recovered");
			}
		}
	}

	async fn recover_parked_child(
		&self,
		context: &ChatParentContext,
		blob_store: &BlobStore,
		record: omp_agent::AgentRecord,
	) -> Result<(), crate::subagent::supervisor::SupervisorError> {
		let journal_path = record.transcript.clone().ok_or_else(|| {
			crate::subagent::supervisor::SupervisorError::RevivalFailed { id: record.id.clone() }
		})?;
		let journal = Journal::open(&journal_path).map_err(|error| {
			tracing::warn!(agent = %record.id, %error, "recovered child journal open failed");
			crate::subagent::supervisor::SupervisorError::RevivalFailed { id: record.id.clone() }
		})?;
		let log = journal.load().map_err(|error| {
			tracing::warn!(agent = %record.id, %error, "recovered child journal read failed");
			crate::subagent::supervisor::SupervisorError::RevivalFailed { id: record.id.clone() }
		})?;
		let revival = (0..log.len() as u64)
			.filter_map(|index| log.get(index))
			.find_map(|entry| match entry {
				omp_storage::transcript::Entry::Ok(event) => match &event.kind {
					Kind::Init { revival: Some(revival), .. } => Some(revival.clone()),
					_ => None,
				},
				_ => None,
			})
			.ok_or_else(|| crate::subagent::supervisor::SupervisorError::RevivalFailed {
				id: record.id.clone(),
			})?;
		let definition = context
			.definitions
			.iter()
			.find(|(name, _)| {
				name
					.as_str()
					.eq_ignore_ascii_case(revival.definition.as_str())
			})
			.map(|(_, definition)| definition.clone())
			.ok_or_else(|| crate::subagent::supervisor::SupervisorError::RevivalFailed {
				id: record.id.clone(),
			})?;
		let workspace_root = url::Url::parse(revival.workspace.root_uri.as_str())
			.ok()
			.and_then(|url| url.to_file_path().ok())
			.ok_or_else(|| crate::subagent::supervisor::SupervisorError::RevivalFailed {
				id: record.id.clone(),
			})?;
		let mut snapshot = context.state.snapshot().as_ref().clone();
		snapshot.workspace.cwd = workspace_root.clone();
		snapshot.turn.params.model = revival.model_role.to_string();
		let grants = blob_store
			.get(&revival.grant_snapshot_ref)
			.ok()
			.and_then(|bytes| serde_json::from_slice::<RecoveredChildGrants>(&bytes).ok())
			.ok_or_else(|| crate::subagent::supervisor::SupervisorError::RevivalFailed {
				id: record.id.clone(),
			})?;
		snapshot.enabled_tools = grants.enabled_tools.into();
		let tools = blob_store
			.get(&revival.tool_snapshot_ref)
			.ok()
			.and_then(|bytes| inference_pb::ChatParams::decode(bytes).ok())
			.ok_or_else(|| crate::subagent::supervisor::SupervisorError::RevivalFailed {
				id: record.id.clone(),
			})?;
		snapshot.turn.params.tools = tools.tools;
		let policy = blob_store
			.get(&revival.policy_snapshot_ref)
			.ok()
			.and_then(|bytes| serde_json::from_slice::<RecoveredChildPolicy>(&bytes).ok())
			.ok_or_else(|| crate::subagent::supervisor::SupervisorError::RevivalFailed {
				id: record.id.clone(),
			})?;
		snapshot.defer_interrupts = policy.defer_interrupts;
		let max_attempts = std::num::NonZeroU32::new(policy.retry.max_attempts).ok_or_else(|| {
			crate::subagent::supervisor::SupervisorError::RevivalFailed { id: record.id.clone() }
		})?;
		snapshot.retry = omp_agent::RetryPolicy::new(
			max_attempts,
			Duration::from_millis(policy.retry.initial_backoff_ms),
			Duration::from_millis(policy.retry.max_backoff_ms),
		)
		.map_err(|_| crate::subagent::supervisor::SupervisorError::RevivalFailed {
			id: record.id.clone(),
		})?;
		if let Some(schema_ref) = revival.schema_ref.as_ref() {
			let schema = blob_store.get(schema_ref).map_err(|_| {
				crate::subagent::supervisor::SupervisorError::RevivalFailed { id: record.id.clone() }
			})?;
			snapshot.turn.params.response_format = Some(inference_pb::ResponseFormat {
				kind:           Some(inference_pb::response_format::Kind::JsonSchema(
					inference_pb::response_format::JsonSchema {
						name:        "subagent_output".to_owned(),
						schema_json: schema.to_vec().into(),
						strict:      Some(true),
					},
				)),
				on_unsupported: inference_pb::Fallback::Error as i32,
			});
		}
		let parent = revival.parent_id.clone();
		let node = context
			.tree
			.register_child(
				record.id.clone(),
				Some(revival.display_name.as_str()),
				&definition,
				parent,
				record.session.clone(),
				Budget::default(),
			)
			.map_err(crate::subagent::supervisor::SupervisorError::Admission)?;
		node.set_status(AgentStatus::Settled);
		let isolated_state = revival.workspace.isolation_id.as_ref().map(|_| {
			context
				.sessions_dir
				.join("eval-agents")
				.join(format!("{}-env", record.id))
		});
		let reviver: Arc<dyn crate::subagent::supervisor::ChildReviver<C>> =
			Arc::new(ProductionChildReviver {
				client: self.client.clone(),
				base_env: self.env.clone(),
				broker: self.broker.clone(),
				supervisor: Arc::clone(&self.supervisor),
				node: Arc::clone(&node),
				snapshot,
				journal_path,
				project_root: context.root.clone(),
				workspace_root,
				isolated_state,
				session_index: Arc::clone(&context.session_index),
				parent_session: SessionId(record.session.clone()),
			});
		self.supervisor.register_parked(node, reviver)?;
		self.ensure_revival_transport(&record.id);
		self.bind_parked_transport(record);
		Ok(())
	}

	pub(crate) async fn release_child(&self, id: &str) {
		let _ = self.supervisor.cancel(id);
		let settled = tokio::time::timeout(Duration::from_secs(5), async {
			loop {
				let Some(state) = self.supervisor.state(id) else {
					return false;
				};
				if state.lifecycle() == SubagentLifecycle::Settled {
					return true;
				}
				tokio::time::sleep(Duration::from_millis(25)).await;
			}
		})
		.await
		.unwrap_or(false);
		if !settled || self.supervisor.park_stopped(id).await.is_err() {
			return;
		}
		if let Some((record, _)) = self.broker.registry().record(id) {
			self.bind_parked_transport(record);
		}
	}

	pub(crate) async fn park_expired_children(&self, ttl: Duration) {
		for lease in self
			.broker
			.registry()
			.park_expired(omp_agent::broker_now_ms(), ttl)
		{
			let id = lease.record.id.clone();
			if self.supervisor.park(id.as_str()).await.is_ok() {
				self.bind_parked_transport(lease.record);
			} else {
				let _ = self.broker.registry().set_status(
					id.as_str(),
					Some(lease.revision),
					RegistryStatus::Idle,
				);
			}
		}
	}

	/// Starts the session-owned idle loop parking scheduler.
	///
	/// The broker registry is the idle-time authority. Each lease carries the
	/// exact generation revision that `bind_parked_transport` preserves while
	/// the supervisor releases the live loop resources.
	pub(crate) fn start_idle_parking(self: &Arc<Self>) {
		let parent = Arc::downgrade(self);
		drop(tokio::spawn(async move {
			let mut tick = tokio::time::interval(Duration::from_secs(1));
			tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
			loop {
				tick.tick().await;
				let Some(parent) = parent.upgrade() else {
					return;
				};
				let ttl_ms = parent.task_settings().agent_idle_ttl_ms;
				if ttl_ms != 0 {
					parent
						.park_expired_children(Duration::from_millis(ttl_ms))
						.await;
				}
			}
		}));
	}

	async fn run_eval_agent(
		&self,
		id: &str,
		items: Vec<Item>,
		turn_id: TurnId,
	) -> Result<omp_agent::AgentRunSummary, crate::envd::eval::BridgeHostError> {
		let mut budget = self.context.lock().state.subscribe();
		let _ = self.broker.set_idle(id, false);
		let run = self.supervisor.run(id, items, turn_id);
		tokio::pin!(run);
		loop {
			tokio::select! {
							result = &mut run => {
													let _ = self.broker.set_idle(id, true);
								if let Some(terminal) = self
									.supervisor
									.state(id)
									.and_then(|state| state.terminal())
								{
									let _ = self.broker.registry().set_terminal(id, terminal);
								}
			return result.map_err(|error| {
									crate::envd::eval::BridgeHostError::message(error.to_string())
								});
							},
							changed = budget.changed() => {
								if changed.is_err() {
									continue;
								}
								let exhausted = budget
									.borrow_and_update()
									.turn
									.params
									.task_budget
									.is_some_and(|budget| budget.remaining_tokens == Some(0));
								if exhausted {
									let _ = self.supervisor.cancel(id);
								}
							},
						}
		}
	}

	async fn validate_agent_summary(
		&self,
		id: &str,
		schema: Option<Value>,
		strict: bool,
		mut summary: omp_agent::AgentRunSummary,
	) -> Result<(String, Option<Value>, Option<Value>), crate::envd::eval::BridgeHostError> {
		if let Some(outcome) = summary.outcome.as_ref() {
			self
				.context
				.lock()
				.tree
				.debit_outcome(id, outcome)
				.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
		}
		let Some(schema) = schema else {
			let text = summary
				.outcome
				.as_ref()
				.map_or_else(|| "(interrupted)".to_owned(), bridge_outcome_text);
			return Ok((text, None, None));
		};
		let mut validator = YieldPayloadValidator::new(Some(schema), strict);
		let mut retries = 0_u8;
		loop {
			let text = summary
				.outcome
				.as_ref()
				.map_or_else(|| "(interrupted)".to_owned(), bridge_outcome_text);
			match summary.yield_payload(&mut validator) {
				Ok(Some(payload)) => {
					if let Some(error) = payload.error {
						return Ok((text, None, Some(json!({ "status": "failed", "error": error }))));
					}
					if let Some(data) = payload.data {
						let schema_status = payload.schema_overridden.then(|| {
							json!({
								"status": "invalid",
								"mode": "permissive",
								"salvaged": true,
								"warning": crate::subagent::yield_driver::WARNING_SCHEMA_OVERRIDDEN,
							})
						});
						return Ok((text, Some(data), schema_status));
					}
				},
				Ok(None) => {},
				Err(error) if retries >= MAX_YIELD_SCHEMA_RETRIES => {
					let warning = matches!(&error, omp_agent::YieldPayloadError::MissingData)
						.then_some(crate::subagent::yield_driver::WARNING_NULL_YIELD);
					return Ok((
						text,
						None,
						Some(json!({
							"status": "invalid",
							"mode": if strict { "strict" } else { "permissive" },
							"error": error.to_string(),
							"warning": warning,
						})),
					));
				},
				Err(_) => {},
			}
			if retries >= MAX_YIELD_SCHEMA_RETRIES {
				return Ok((
					text,
					None,
					Some(json!({
						"status": "unavailable",
						"mode": if strict { "strict" } else { "permissive" },
						"error": "child did not submit a terminal structured yield",
						"warning": crate::subagent::yield_driver::WARNING_MISSING_YIELD,
					})),
				));
			}
			retries = retries.saturating_add(1);
			summary = self
				.run_eval_agent(
					id,
					vec![bridge_message(
						Role::User,
						"Your terminal yield did not satisfy the requested JSON Schema. Submit the \
						 complete corrected object as result.data now.",
					)],
					TurnId::new(format!("eval-agent-schema-retry-{}", omp_core::Ulid::generate())),
				)
				.await?;
		}
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
fn deterministic_isolation_recovery(
	worktree: &str,
	artifact: Option<&str>,
	branch: Option<&str>,
	conflicts: &[omp_proto::env::v1::WorkspaceConflict],
) -> Str {
	use std::fmt::Write as _;

	let mut summary =
		String::from("Isolated workspace disposition conflicted; changes remain recoverable");
	if let Some(artifact) = artifact {
		let _ = write!(summary, " from patch {artifact}");
	}
	if let Some(branch) = branch {
		let _ = write!(summary, " from branch {branch}");
	}
	if artifact.is_none() && branch.is_none() {
		let _ = write!(summary, " from workspace {worktree}");
	}
	summary.push_str(". Conflicts:");
	for conflict in conflicts.iter().take(8) {
		let reason = omp_proto::env::v1::ConflictReason::try_from(conflict.reason)
			.unwrap_or(omp_proto::env::v1::ConflictReason::Unspecified);
		let _ = write!(summary, " {} ({})", conflict.path, reason.as_str_name());
	}
	if conflicts.len() > 8 {
		let _ = write!(summary, " and {} more", conflicts.len() - 8);
	}
	summary.push('.');
	Str::from(summary)
}

fn deterministic_task_summary(prompt: &str) -> Str {
	const MAX_CHARS: usize = 160;

	let mut summary = String::with_capacity(prompt.len().min(MAX_CHARS));
	let mut chars = 0_usize;
	for word in prompt.split_whitespace() {
		let word_chars = word.chars().count();
		let separator = if summary.is_empty() { 0 } else { 1 };
		if chars.saturating_add(separator).saturating_add(word_chars) > MAX_CHARS {
			break;
		}
		if separator != 0 {
			summary.push(' ');
		}
		summary.push_str(word);
		chars = chars.saturating_add(separator).saturating_add(word_chars);
	}
	Str::from(summary)
}

fn append_production_child_init(
	journal: &mut Journal,
	blob_store: &BlobStore,
	node: &omp_agent::AgentNode,
	definition: &omp_agent::AgentDefinition,
	snapshot: &AgentSnapshot,
	system_prompt: &str,
	output_schema: Option<&Value>,
	model_role: &str,
	child_root: &Path,
	isolation_id: Option<Str>,
) -> Result<(), ChildInitError> {
	let system_prompt = blob_store.put(system_prompt.as_bytes())?;
	let retry = snapshot.retry;
	let policy = serde_json::to_vec(&json!({
		"deferInterrupts": snapshot.defer_interrupts,
		"retry": {
			"maxAttempts": retry.max_attempts().get(),
			"initialBackoffMs": retry.initial_backoff().as_millis(),
			"maxBackoffMs": retry.max_backoff().as_millis(),
		},
	}))
	.map_err(ChildInitError::Schema)?;
	let policy_snapshot_ref = blob_store.put(&policy)?;
	let grants = serde_json::to_vec(&json!({
		"enabledTools": snapshot.enabled_tools.as_ref(),
	}))
	.map_err(ChildInitError::Schema)?;
	let grant_snapshot_ref = blob_store.put(&grants)?;
	let tools = inference_pb::ChatParams {
		tools: snapshot.turn.params.tools.clone(),
		..inference_pb::ChatParams::default()
	}
	.encode_to_vec();
	let tool_snapshot_ref = blob_store.put(&tools)?;
	let schema = output_schema
		.map(serde_json::to_string)
		.transpose()
		.map_err(ChildInitError::Schema)?;
	let schema_ref = schema
		.as_deref()
		.map(str::as_bytes)
		.map(|bytes| blob_store.put(bytes))
		.transpose()?;
	let output_schema = schema
		.map(serde_json::value::RawValue::from_string)
		.transpose()
		.map_err(ChildInitError::Schema)?;
	let root_uri = url::Url::from_file_path(child_root)
		.map_err(|()| ChildInitError::WorkspaceRoot)?
		.to_string();
	let revival = omp_storage::transcript::ChildSessionInit {
		display_name: node.name.clone(),
		parent_id: node.parent.clone().unwrap_or_default(),
		definition: definition.name.clone(),
		depth: node.depth,
		prompt_ref: system_prompt,
		schema_ref,
		policy_snapshot_ref,
		grant_snapshot_ref,
		tool_snapshot_ref,
		model_role: Str::new(model_role),
		workspace: omp_storage::transcript::ChildWorkspaceIdentity {
			root_uri: Str::new(root_uri),
			isolation_id,
			revision: None,
		},
		serving_model: None,
	};
	journal.append_child_init(
		now_ms(),
		system_prompt,
		snapshot.enabled_tools.iter().cloned().collect(),
		output_schema,
		revival,
	)?;
	Ok(())
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

fn retain_security_review_result(
	definition: &omp_agent::AgentDefinition,
	data: Option<&Value>,
	root: &Path,
	blobs: &BlobStore,
	id: &str,
) -> Result<Option<(Value, Str, Str)>, crate::envd::eval::BridgeHostError> {
	if definition.name != crate::security_review::profile::PROFILE_ID {
		return Ok(None);
	}
	let Some(data) = data else {
		return Ok(None);
	};
	let scope = crate::security_review::result::ReviewScope::resolve(root, None)
		.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
	let validated = crate::security_review::result::validate_and_retain(
		data.clone(),
		&scope,
		sf!("agent://{}", id),
		blobs,
	)
	.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
	let data = serde_json::to_value(validated.output)
		.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
	Ok(Some((data, validated.report, validated.artifact_uri)))
}

#[async_trait]
impl<C: TurnClient + Clone + Send + 'static> omp_tools::memory::ReflectionHost
	for ChatParentHost<C>
{
	async fn reflect(
		&self,
		request: omp_tools::memory::ReflectionRequest,
	) -> Result<Str, omp_tools::memory::ReflectionHostError> {
		let params = self.context.lock().state.snapshot().turn.params.clone();
		let lane = crate::memory::InferenceExtractionLane::with_selector(
			self.client.clone(),
			params,
			"@memory",
		);
		omp_tools::memory::ReflectionHost::reflect(&lane, request).await
	}
}

impl<C: TurnClient + Clone + Send + 'static> crate::session_title::OnlineTitleCompletion
	for ChatParentHost<C>
{
	fn complete_title<'a>(
		&'a self,
		roles: &'static [&'static str],
		system_prompt: &'a str,
		input: &'a str,
	) -> Pin<Box<dyn Future<Output = Result<Option<Str>, Str>> + Send + 'a>> {
		Box::pin(async move {
			let context = self.context.lock().clone();
			let mut params = context.state.snapshot().turn.params.clone();
			params.tools.clear();
			params.tool_choice = None;
			params.response_format = None;
			let role = roles.first().copied().unwrap_or("tiny");
			params.model = format!("@{role}");
			let options = TurnOptions {
				context_id: None,
				params,
				executor: None,
				props: None,
				provider_reset: false,
				stream_watchdog: omp_agent::StreamWatchdog::default(),
			};
			let items =
				vec![bridge_message(Role::System, system_prompt), bridge_message(Role::User, input)];
			let mut turn = self
				.client
				.turn(
					TurnId::new(format!("session-title-{}", omp_core::Ulid::generate())),
					TurnInput::Full(Thread { items }),
					&options,
				)
				.await
				.map_err(|error| Str::from(error.to_string()))?;
			let mut events = turn.events();
			while let Some(event) = events.next().await {
				let event = event.map_err(|error| Str::from(error.to_string()))?;
				match event.event {
					Some(inference_pb::turn_event::Event::Outcome(outcome)) => {
						return Ok(Some(Str::from(bridge_outcome_text(&outcome))));
					},
					Some(inference_pb::turn_event::Event::Error(error)) => {
						return Err(Str::from(error.detail));
					},
					_ => {},
				}
			}
			Ok(None)
		})
	}
}

#[async_trait]
impl<C: TurnClient + Clone + Send + 'static> crate::envd::eval::ParentSessionHost
	for ChatParentHost<C>
{
	fn eval_session_config(
		&self,
	) -> Result<crate::envd::eval::EvalSessionConfig, crate::envd::eval::BridgeHostError> {
		let context = self.context.lock();
		let session_root = context.sessions_dir.join(context.session_id.as_str());
		Ok(crate::envd::eval::EvalSessionConfig {
			cwd:              context.root.clone(),
			local_roots_json: Some(Str::from(
				json!({ "local": session_root.join("local").to_string_lossy() }).to_string(),
			)),
			artifacts_dir:    Some(Str::from(session_root.to_string_lossy().as_ref())),
			session_file:     Some(Str::from(
				context
					.sessions_dir
					.join(format!("{}.jsonl", context.session_id))
					.to_string_lossy()
					.as_ref(),
			)),
		})
	}

	async fn completion(
		&self,
		args: Value,
		_progress: &dyn crate::envd::eval::BridgeProgressSink,
	) -> Result<Value, crate::envd::eval::BridgeHostError> {
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
		let options = TurnOptions {
			context_id: None,
			params,
			executor: None,
			props: None,
			provider_reset: false,
			stream_watchdog: omp_agent::StreamWatchdog::default(),
		};
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

	async fn agent(
		&self,
		args: Value,
		progress: &dyn crate::envd::eval::BridgeProgressSink,
	) -> Result<Value, crate::envd::eval::BridgeHostError> {
		let mut request = crate::envd::eval::spawn::SpawnRequestV1::from_bridge_args(&args)
			.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
		if let Some(plan) = self.approved_plan_reference() {
			request.prompt = sf!(
				"{}\n\nApproved overall plan: {}. Read and follow only this approved plan reference; \
				 do not consume drafts.",
				request.prompt,
				plan.artifact
			);
		}
		let prompt = request.prompt.as_str();
		let kind = request.agent.as_str();
		let context = self.context.lock().clone();
		let task_settings = context.task_settings.snapshot();
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
		if task_settings
			.disabled_agents
			.iter()
			.any(|name| name.as_str().eq_ignore_ascii_case(definition.name.as_str()))
		{
			return Err(crate::envd::eval::BridgeHostError::message(
				"requested agent is disabled by live task settings",
			));
		}
		let session_schema = context
			.state
			.snapshot()
			.turn
			.params
			.response_format
			.as_ref()
			.and_then(|format| format.kind.as_ref())
			.and_then(|kind| match kind {
				inference_pb::response_format::Kind::JsonSchema(schema) => {
					serde_json::from_slice(&schema.schema_json).ok()
				},
				inference_pb::response_format::Kind::Grammar(_) => None,
			});
		let schema_resolution = omp_agent::resolve_output_schema(
			request.output_schema.as_ref(),
			definition.output_schema.as_ref(),
			session_schema.as_ref(),
		);
		request.output_schema = schema_resolution.schema;
		if definition.name == crate::security_review::profile::PROFILE_ID {
			if !crate::security_review::profile::is_canonical(&definition) {
				return Err(crate::envd::eval::BridgeHostError::message(
					"security reviewer profile authority was widened",
				));
			}
			if request.isolation.requested || request.isolation.apply || request.isolation.merge {
				return Err(crate::envd::eval::BridgeHostError::message(
					"local security reviews use the current workspace",
				));
			}
			request.output_schema = definition.output_schema.clone();
			request.schema_mode = crate::envd::eval::spawn::SpawnSchemaMode::Strict;
			request.enable_lsp = true;
		}
		let explicit_patch = request.isolation.apply;
		let explicit_branch = request.isolation.merge;
		let isolated = request.isolation.requested
			|| task_settings.isolation.mode != crate::subagent::settings::TaskIsolationMode::None;
		let auto_apply =
			isolated && !explicit_patch && !explicit_branch && task_settings.isolation.apply;
		let apply = explicit_patch
			|| (auto_apply
				&& task_settings.isolation.merge
					== crate::subagent::settings::TaskIsolationMerge::Patch);
		let merge = explicit_branch
			|| (auto_apply
				&& task_settings.isolation.merge
					== crate::subagent::settings::TaskIsolationMerge::Branch);
		let id = request
			.stable_id
			.clone()
			.unwrap_or_else(|| Str::from(omp_core::Ulid::generate().to_string()));
		let mut display_name = request
			.name
			.clone()
			.or_else(|| context.tree.node(id.as_str()).map(|node| node.name.clone()))
			.unwrap_or_else(|| id.clone());
		let security_follow_up = context
			.tree
			.node(id.as_str())
			.and_then(|node| node.definition.clone())
			.is_some_and(|name| name == crate::security_review::profile::PROFILE_ID);
		if security_follow_up && definition.name != crate::security_review::profile::PROFILE_ID {
			return Err(crate::envd::eval::BridgeHostError::message(
				"security reviewer follow-up must retain its canonical profile",
			));
		}
		progress.progress(json!({
			"op": "agent",
			"id": id,
			"name": display_name,
			"agent": kind,
			"status": "running",
		}))?;
		if self.supervisor.state(&id).is_some() {
			if request.isolation.requested || explicit_patch || explicit_branch {
				return Err(crate::envd::eval::BridgeHostError::message(
					"follow-up turns retain their existing workspace disposition",
				));
			}
			let summary = self
				.run_eval_agent(
					id.as_str(),
					vec![bridge_message(Role::User, prompt)],
					TurnId::new(format!("eval-agent-followup-{}", omp_core::Ulid::generate())),
				)
				.await?;
			let (mut text, mut data, schema_status) = self
				.validate_agent_summary(
					id.as_str(),
					request.output_schema.clone(),
					matches!(request.schema_mode, crate::envd::eval::spawn::SpawnSchemaMode::Strict),
					summary,
				)
				.await?;
			let blob_root = context
				.sessions_dir
				.parent()
				.unwrap_or(context.sessions_dir.as_path());
			let blob_store = BlobStore::open(blob_root)
				.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
			let mut security_artifact = None;
			if let Some((validated, report, artifact)) = retain_security_review_result(
				&definition,
				data.as_ref(),
				&context.root,
				&blob_store,
				id.as_str(),
			)? {
				data = Some(validated);
				text = report.to_string();
				security_artifact = Some(artifact);
			}
			let artifact_dir = context.sessions_dir.join(context.session_id.as_str());
			let artifact = artifact_dir.join(format!("{id}.md"));
			let bounded = crate::subagent::output::persist_bounded(
				&artifact,
				sf!("agent://{}", id),
				&text,
				None,
				false,
			)
			.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
			let visible_text = bounded.preview.unwrap_or_default();
			progress.progress(json!({
				"op": "agent",
				"id": id,
				"name": display_name,
				"agent": kind,
				"status": if schema_status.is_some() && data.is_none() { "failed" } else { "completed" },
			}))?;
			return Ok(json!({
							"text": visible_text,
							"data": data,
							"schema": schema_status,
							"handle": format!("agent://{id}"),
							"details": {
								"id": id,
								"name": display_name,
								"agent": kind,
								"followUp": true,
								"output": format!("agent://{id}"),
												"artifact": security_artifact,
			},
						}));
		}
		if context
			.state
			.snapshot()
			.turn
			.params
			.task_budget
			.is_some_and(|budget| budget.remaining_tokens == Some(0))
		{
			return Err(crate::envd::eval::BridgeHostError::message(
				"hard turn token budget is exhausted; subagent spawn refused",
			));
		}
		let directory = context.sessions_dir.join("eval-agents");
		let (worktree_id, child_root, isolated_state, isolated_environment) = if isolated {
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
			(Some(Str::from(worktree.id)), root, Some(child_state), Some(environment))
		} else {
			(None, context.root.clone(), None, None)
		};
		let child_budget = context
			.state
			.snapshot()
			.turn
			.params
			.task_budget
			.and_then(|budget| budget.remaining_tokens)
			.map_or_else(Budget::default, |remaining| Budget {
				max_output_tokens: Some(remaining),
				..Budget::default()
			});
		let parent_id = context
			.tree
			.node(context.session_id.as_str())
			.map(|parent| parent.id.clone())
			.ok_or_else(|| {
				crate::envd::eval::BridgeHostError::message(
					"parent agent is not registered for subagent admission",
				)
			})?;
		let node = context
			.tree
			.register_child(
				id.clone(),
				request.name.as_deref(),
				&definition,
				parent_id,
				context.session_id.clone(),
				child_budget,
			)
			.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
		display_name = node.name.clone();
		std::fs::create_dir_all(&directory)
			.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
		let parent = SessionId(context.session_id.clone());
		let journal_path = directory.join(format!("{id}.jsonl"));
		let mut journal = create_indexed_journal(
			&journal_path,
			&context.root,
			&id,
			Arc::clone(&context.session_index),
			SessionKind::Subagent,
			Some(&parent),
		)
		.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
		let parent_snapshot = context.state.snapshot();
		let selected_model = definition.effective_model(&task_settings.agent_model_overrides);
		let inherited_pattern = parent_snapshot.turn.params.model.as_str();
		let prewalk = crate::subagent::prewalk::PrewalkGate::resolve(&definition, &task_settings);
		let inference_role = sf!("subagent:{}", id);
		let security_task_settings = (definition.name == crate::security_review::profile::PROFILE_ID)
			.then(|| {
				let mut settings = task_settings.as_ref().clone();
				settings.enable_lsp = true;
				settings
			});
		let child_settings = security_task_settings
			.as_ref()
			.unwrap_or(task_settings.as_ref());

		let child_snapshot = crate::subagent::snapshot::child_snapshot(
			parent_snapshot.as_ref(),
			crate::subagent::snapshot::ChildSnapshotOptions {
				definition: &definition,
				settings: child_settings,
				cwd: &child_root,
				selected_model,
				inference_role: Some(inference_role.as_str()),
				inherited_pattern: Some(inherited_pattern),
				caller_effort: request.effort,
				model_ceiling: None,
				plan_mode: false,
				enable_lsp: request.enable_lsp,
				prewalk_gate: prewalk.armed(),
			},
		);
		let child_env = isolated_environment
			.as_ref()
			.map_or_else(|| self.env.clone(), |environment| environment.client().clone());
		let child_content = crate::discovery::active_content_snapshots(&child_root);
		let (child_ttsr, child_ttsr_diagnostics) =
			crate::rulebook::ttsr_registry(child_content.rules.as_ref());
		for error in child_ttsr_diagnostics {
			tracing::warn!(%error, agent = %id, "subagent TTSR rule condition was rejected");
		}
		let peer_values = context
			.tree
			.roster()
			.map(|node| crate::subagent::prompt::peer_from_node(node))
			.collect::<Vec<_>>();
		let peers = peer_values
			.iter()
			.map(|(name, role, status, activity)| crate::subagent::prompt::PromptPeer {
				name:     name.as_str(),
				role:     role.as_str(),
				status:   status.as_str(),
				activity: activity.as_str(),
			})
			.collect::<Vec<_>>();
		let model = selected_model.unwrap_or(inherited_pattern);
		let codex_style = {
			let model = model.to_ascii_lowercase();
			model.contains("codex") || model.contains("gpt-5")
		};
		let system_prompt =
			crate::subagent::prompt::compose(crate::subagent::prompt::SubagentPromptInput {
				definition:        &definition,
				shared_context:    None,
				plan_path:         None,
				plan_content:      None,
				workspace_root:    &child_root,
				output_schema:     request.output_schema.as_ref(),
				self_name:         node.name.as_str(),
				self_role:         definition.name.as_str(),
				irc_enabled:       child_settings.max_recursion_depth == -1
					|| i32::from(node.depth) < i32::from(child_settings.max_recursion_depth),
				roster_generation: context.tree.roster_generation(),
				peers:             &peers,
				capabilities:      crate::subagent::prompt::ModelFamilyCapabilities {
					codex_style,
					parallel_tool_calls: true,
					structured_yield: request.output_schema.is_some(),
				},
				plan_mode:         false,
				eager:             task_settings.eager,
			});
		let blob_root = context
			.sessions_dir
			.parent()
			.unwrap_or(context.sessions_dir.as_path());
		let blob_store = BlobStore::open(blob_root)
			.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
		append_production_child_init(
			&mut journal,
			&blob_store,
			node.as_ref(),
			&definition,
			&child_snapshot,
			system_prompt.as_str(),
			request.output_schema.as_ref(),
			model,
			&child_root,
			worktree_id.clone(),
		)
		.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
		let mut child = Agent::new(
			self.client.clone(),
			child_env.clone(),
			AgentState::new(child_snapshot.clone()),
			journal,
			CHAT_CAPS_BASE,
		);
		child.set_ttsr_registry(child_ttsr);
		let control_binding = if let Some(environment) = &isolated_environment {
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
		let _ = self.broker.registry().set_history(
			id.as_str(),
			Some(journal_path.clone()),
			Some(Str::from(model)),
			Some(deterministic_task_summary(prompt)),
			omp_agent::AgentHistory::default(),
		);
		let hub = hub_backend::attach_for(
			id.clone(),
			Arc::new(hub_backend::ChatHubBackend::new(
				self.broker.clone(),
				inbox,
				Arc::clone(child.jobs()),
				child_env.clone(),
				id.clone(),
				context.session_id.clone(),
				None,
				Some(self.supervisor.clone()),
			)),
		);
		let mut runtime = crate::subagent::supervisor::SupervisedRuntime::new(child);
		if let Some(binding) = control_binding {
			runtime.retain(binding);
		}
		runtime.retain(hub);
		if let Some(environment) = isolated_environment {
			runtime.retain(environment);
		}
		let reviver: Arc<dyn crate::subagent::supervisor::ChildReviver<C>> =
			Arc::new(ProductionChildReviver {
				client: self.client.clone(),
				base_env: self.env.clone(),
				broker: self.broker.clone(),
				supervisor: Arc::clone(&self.supervisor),
				node: Arc::clone(&node),
				snapshot: child_snapshot,
				journal_path,
				project_root: context.root.clone(),
				workspace_root: child_root.clone(),
				isolated_state,
				session_index: Arc::clone(&context.session_index),
				parent_session: parent,
			});
		self
			.supervisor
			.register(Arc::clone(&node), runtime, Some(reviver))
			.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
		self.ensure_revival_transport(&id);
		let summary = self
			.run_eval_agent(
				id.as_str(),
				vec![
					bridge_message(Role::System, system_prompt.as_str()),
					bridge_message(Role::User, prompt),
				],
				TurnId::new(format!("eval-agent-{}", omp_core::Ulid::generate())),
			)
			.await?;
		let (mut text, mut data, schema_status) = self
			.validate_agent_summary(
				id.as_str(),
				request.output_schema.clone(),
				matches!(request.schema_mode, crate::envd::eval::spawn::SpawnSchemaMode::Strict),
				summary,
			)
			.await?;
		let mut security_artifact = None;
		if let Some((validated, report, artifact)) = retain_security_review_result(
			&definition,
			data.as_ref(),
			&child_root,
			&blob_store,
			id.as_str(),
		)? {
			data = Some(validated);
			text = report.to_string();
			security_artifact = Some(artifact);
		}
		let mut disposition = None;
		let mut disposition_conflict = None;
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
			let mut conflicts = merged.conflicts;
			conflicts.sort_by(|left, right| {
				left
					.path
					.cmp(&right.path)
					.then_with(|| left.reason.cmp(&right.reason))
					.then_with(|| left.detail.cmp(&right.detail))
			});
			let artifact_hash = (!merged.artifact_hash.is_empty())
				.then(|| omp_core::encoding::hex::encode(&merged.artifact_hash).into_string());
			let artifact_uri = artifact_hash
				.as_deref()
				.map(|hash| sf!("artifact://b3/{}", hash));
			let conflict_count = conflicts.len();
			let conflict_facts = conflicts
				.iter()
				.take(32)
				.map(|conflict| {
					json!({
						"path": conflict.path.as_str(),
						"reason": omp_proto::env::v1::ConflictReason::try_from(conflict.reason)
							.unwrap_or(omp_proto::env::v1::ConflictReason::Unspecified)
							.as_str_name(),
						"detail": conflict.detail.as_deref(),
					})
				})
				.collect::<Vec<_>>();
			disposition = Some(json!({
				"mode": if apply { "patch" } else { "branch" },
				"status": if conflict_count == 0 { "ready" } else { "conflict" },
				"artifact": artifact_uri.as_deref(),
				"artifactHash": artifact_hash.as_deref(),
				"artifactSize": merged.artifact_size,
				"branch": merged.branch.as_deref(),
				"conflictCount": conflict_count,
				"conflicts": conflict_facts,
				"conflictsTruncated": conflict_count > 32,
			}));
			if conflict_count != 0 {
				let recovery = deterministic_isolation_recovery(
					id.as_str(),
					artifact_uri.as_deref(),
					merged.branch.as_deref(),
					&conflicts,
				);
				text.push_str("\n\n");
				text.push_str(recovery.as_str());
				disposition_conflict = Some((recovery, artifact_uri, merged.branch));
			}
		}
		let artifact_dir = context.sessions_dir.join(context.session_id.as_str());
		let artifact = artifact_dir.join(format!("{id}.md"));
		let bounded = crate::subagent::output::persist_bounded(
			&artifact,
			sf!("agent://{}", id),
			&text,
			worktree_id.clone(),
			false,
		)
		.map_err(|error| crate::envd::eval::BridgeHostError::message(error.to_string()))?;
		let visible_text = bounded.preview.unwrap_or_default();
		let disposition_failed = disposition_conflict.is_some();
		if let Some((summary, artifact_uri, branch)) = disposition_conflict {
			if let Some((record, _)) = self.broker.registry().record(id.as_str()) {
				let mut history = record.history;
				history.output_path = Some(artifact.clone());
				history.branch = branch.map(Str::from);
				let _ = self.broker.registry().set_history(
					id.as_str(),
					record.transcript,
					record.model,
					record.task,
					history,
				);
			}
			let _ = self
				.broker
				.registry()
				.set_terminal(id.as_str(), SubagentTerminalStatus {
					kind:        SubagentTerminalKind::Failed,
					summary:     summary.clone(),
					disposition: SubagentDisposition {
						artifact_uri,
						preview: Some(summary),
						truncated: false,
						workspace: worktree_id.clone(),
					},
				});
		}
		progress.progress(json!({
			"op": "agent",
			"id": id,
			"name": display_name,
			"agent": kind,
			"status": if schema_status.is_some() && data.is_none() || disposition_failed {
				"failed"
			} else {
				"completed"
			},
		}))?;
		Ok(json!({
					"text": visible_text,
					"data": data,
					"schema": schema_status,
					"handle": format!("agent://{id}"),
					"details": {
						"id": id,
						"name": display_name,
						"agent": kind,
						"blocking": definition.blocking,
						"isolated": isolated,
						"worktree": worktree_id,
						"root": isolated.then(|| child_root.to_string_lossy().into_owned()),
						"disposition": disposition,
						"output": format!("agent://{id}"),
									"artifact": security_artifact,
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
/// Presentation selected for the interactive project-chat session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChatPresentation {
	/// Render through the inline terminal host.
	Terminal,
	/// Render through the native GPU window host.
	Gui,
}

/// Runs one interactive durable project-chat session.
#[cfg(any(unix, windows))]
#[expect(
	clippy::future_not_send,
	reason = "the interactive chat future owns a thread-confined terminal scene"
)]
pub(crate) async fn run(
	args: ChatArgs,
	mut start: ChatStart,
	presentation: ChatPresentation,
) -> miette::Result<()> {
	use miette::{Context as _, IntoDiagnostic as _};
	let launch_root = canonical_project(&args.project).map_err(|e| miette::miette!(e))?;
	let data_dir = crate::cli::data_dir(None)?;
	let mut root = launch_root.clone();
	let mut selected_sessions_dir = None;
	let mut selected_index_path = None;
	let mut picked_resume = None;
	let mut resume_moved = false;
	if start == ChatStart::SessionIndex {
		let Some(selection) = crate::pickers::pick_session(&data_dir, args.session_dir.as_deref())
			.await
			.map_err(|error| miette::miette!(error))?
		else {
			return Ok(());
		};
		picked_resume = Some(selection.session.id.0.clone());
		selected_sessions_dir = Some(selection.sessions_dir);
		selected_index_path = Some(selection.database_path);
		start = ChatStart::Session;
		let recorded_root = PathBuf::from(selection.session.project.as_str());
		if recorded_root.is_dir() {
			root = canonical_project(&recorded_root).map_err(|error| miette::miette!(error))?;
		} else {
			let choices = [
				omp_chat_ui::ListRow {
					key:    sf!("move"),
					label:  sf!("Move session"),
					detail: Str::from(launch_root.to_string_lossy().as_ref()),
				},
				omp_chat_ui::ListRow {
					key:    sf!("cancel"),
					label:  sf!("Cancel"),
					detail: sf!("Keep the journal unchanged"),
				},
			];
			if crate::pickers::run_list("Project missing", &choices)
				.await
				.map_err(|error| miette::miette!(error))?
				!= Some(0)
			{
				return Ok(());
			}
			resume_moved = true;
			eprintln!(
				"Session project `{}` no longer exists; moving future workspace access to `{}`.",
				recorded_root.display(),
				launch_root.display()
			);
		}
	}
	let catalog =
		omp_llm_catalog::snapshot::Catalog::try_embedded().map_err(|e| miette::miette!(e))?;
	let settings =
		crate::settings::current_with_overlays(&data_dir, &args.config).into_diagnostic()?;
	let security_enabled = settings.security.enabled;
	let roles = crate::discovery::roles::resolve_launch_roles(
		catalog,
		args.model.as_deref(),
		args.smol.as_deref(),
		args.slow.as_deref(),
		args.plan.as_deref(),
	)
	.map_err(|error| miette::miette!(error))?;
	for selector in args
		.models
		.as_ref()
		.into_iter()
		.flat_map(|selectors| selectors.0.iter())
	{
		resolve_model_selector(catalog, selector).map_err(|error| miette::miette!(error))?;
	}
	for root in &args.add_dir {
		std::fs::canonicalize(root)
			.into_diagnostic()
			.wrap_err_with(|| {
				format!("additional workspace root `{}` is unavailable", root.display())
			})?;
	}
	let plan_selection = roles
		.plan
		.as_ref()
		.map(|model| {
			crate::plan::ModelSelection::resolved(model.as_str(), roles.plan_thinking.as_deref())
		})
		.transpose()
		.map_err(|error| miette::miette!(error))?;
	let interrupt_grace = settings.runtime_durations().interrupt_grace;
	let auto_thinking = settings.auto_thinking;
	let power_mode = crate::power::configured(&data_dir).into_diagnostic()?;
	let explicit_model = roles
		.primary
		.as_ref()
		.map(|model| Str::from(model.as_str()))
		.or_else(|| args.model.clone());
	let model = match explicit_model
		.clone()
		.or_else(|| settings.default_model.map(Str::from))
	{
		Some(model) => model,
		None => crate::wizard::run(&data_dir, catalog)
			.await?
			.ok_or_else(|| miette::miette!("no model configured — run `omp` again to finish setup"))?,
	};
	let model = match resolve_model_selector(catalog, model.as_str()) {
		Ok(model) => model,
		Err(error) if explicit_model.is_none() => {
			let fallback = fallback_model_selector(catalog).ok_or_else(|| miette::miette!(error))?;
			eprintln!(
				"Saved model `{}` is unavailable; using `{}` for this session without changing the \
				 saved preference.",
				model, fallback
			);
			fallback
		},
		Err(error) => return Err(miette::miette!(error).into()),
	};
	if args.api_key.is_some() && args.model.is_none() && args.models.is_none() {
		return Err(miette::miette!(
			"--api-key requires a model to be specified via --model or --models"
		));
	}
	let credential_provider = args
		.api_key
		.as_ref()
		.map(|_| resolve_model_provider(catalog, model.as_str(), args.provider.as_deref()))
		.transpose()
		.map_err(|error| miette::miette!(error))?;
	let state_dir =
		crate::project_state::directory(&data_dir, &root).map_err(|e| miette::miette!(e))?;
	ensure_state_directory(&state_dir).map_err(|e| miette::miette!(e))?;
	let ephemeral_sessions = if args.no_session {
		Some(EphemeralSessions::create().map_err(|error| miette::miette!(error))?)
	} else {
		None
	};
	let sessions_dir = if let Some(ephemeral) = &ephemeral_sessions {
		ephemeral.path().to_owned()
	} else if let Some(selected) = selected_sessions_dir {
		selected
	} else if let Some(configured) = args.session_dir.as_deref() {
		ensure_state_directory(configured).map_err(|error| miette::miette!(error))?;
		std::fs::canonicalize(configured).into_diagnostic()?
	} else {
		state_dir.join("sessions")
	};
	ensure_state_directory(&sessions_dir).map_err(|e| miette::miette!(e))?;
	let requested_resume = picked_resume.or_else(|| args.resume.clone());
	let env_socket = crate::project_state::environment_socket(&state_dir);
	let document_socket = crate::project_state::document_socket(&state_dir);
	let environment = crate::envd::ProjectEnvironment::connect_or_start(
		&root,
		&state_dir,
		&env_socket,
		&document_socket,
		args.py_eval,
		&args.trusted_extensions,
		interrupt_grace,
	)
	.await
	.map_err(|e| miette::miette!(e))?;
	let session_index = if let Some(database) = selected_index_path {
		Arc::new(
			SessionIndex::open(database)
				.map_err(|error| miette::miette!(ChatError::SessionIndex(error)))?,
		)
	} else if args.session_dir.is_some() && !args.no_session {
		Arc::new(
			SessionIndex::open(sessions_dir.join("sessions.sqlite3"))
				.map_err(|error| miette::miette!(ChatError::SessionIndex(error)))?,
		)
	} else {
		environment.sessions_index()
	};
	let breadcrumbs = crate::project_state::TerminalBreadcrumbs::new(&data_dir)
		.map_err(|error| miette::miette!(error))?;
	let terminal_id = omp_tui::ttyid::terminal_id();
	let resume = if let Some(resume) = requested_resume {
		if strict_session_id(&resume).is_ok() {
			Some(resume)
		} else {
			let root_text = root.to_string_lossy();
			let page = session_index
				.list(&omp_storage::index::SessionFilter {
					project: Some(Str::from(root_text.as_ref())),
					limit: 200,
					..Default::default()
				})
				.map_err(|error| miette::miette!(error))?;
			Some(
				crate::project_state::resolve_session_selector(&page.sessions, resume.as_str())
					.map_err(|error| miette::miette!(error))?
					.0,
			)
		}
	} else if let Some(selector) = args.continue_session.as_deref() {
		if selector == "@terminal" {
			breadcrumbs
				.read(terminal_id.as_str())
				.map_err(|error| miette::miette!(error))?
				.map(|session| session.0)
		} else {
			let root_text = root.to_string_lossy();
			let page = session_index
				.list(&omp_storage::index::SessionFilter {
					project: Some(Str::from(root_text.as_ref())),
					limit: 200,
					..Default::default()
				})
				.map_err(|error| miette::miette!(error))?;
			Some(
				crate::project_state::resolve_session_selector(&page.sessions, selector)
					.map_err(|error| miette::miette!(error))?
					.0,
			)
		}
	} else {
		None
	};
	let fork = if let Some(selector) = args.fork.as_ref() {
		if strict_session_id(selector).is_ok() {
			Some(selector.clone())
		} else {
			let root_text = root.to_string_lossy();
			let page = session_index
				.list(&omp_storage::index::SessionFilter {
					project: Some(Str::from(root_text.as_ref())),
					limit: 200,
					..Default::default()
				})
				.map_err(|error| miette::miette!(error))?;
			Some(
				crate::project_state::resolve_session_selector(&page.sessions, selector.as_str())
					.map_err(|error| miette::miette!(error))?
					.0,
			)
		}
	} else {
		None
	};
	let eval_bridge = environment.eval_bridge();
	let eval_control = environment.eval_control();

	let registry = environment.registry();
	let session_open = if args.no_session {
		SessionOpen::Ephemeral
	} else if let Some(source) = fork.as_ref() {
		SessionOpen::Fork(source)
	} else if let Some(source) = resume.as_ref() {
		if resume_moved {
			SessionOpen::ResumeMoved(source)
		} else {
			SessionOpen::Resume(source)
		}
	} else {
		SessionOpen::New
	};
	let mut session = open_session(
		&root,
		&sessions_dir,
		session_open,
		registry.as_ref(),
		(!args.no_session).then(|| Arc::clone(&session_index)),
	)
	.map_err(|e| miette::miette!(e))?;
	if matches!(session_open, SessionOpen::Resume(_) | SessionOpen::ResumeMoved(_)) {
		let pending_turn = session.journal.pending_turn().is_some();
		let pending_jobs = session.journal.pending_jobs().count();
		if pending_turn || pending_jobs != 0 {
			eprintln!(
				"Warning: resumed session has {} pending tool call(s){}.",
				pending_jobs,
				if pending_turn {
					" and an interrupted turn"
				} else {
					""
				}
			);
		}
	}
	let blueprint = session_blueprint(
		model.as_str(),
		catalog,
		&root,
		&args.add_dir,
		&session.id,
		Arc::clone(&registry),
	)
	.map_err(|error| miette::miette!(error))?;
	let mut snapshot =
		agent_snapshot(&blueprint, catalog).map_err(|error| miette::miette!(error))?;
	let home = std::env::var_os("HOME").map_or_else(|| root.clone(), PathBuf::from);
	let prompt_settings = crate::prompt_prep::settings::PromptSettings::default()
		.with_cli(&args.prompt_settings)
		.resolve_inputs(&root, &home)
		.map_err(|error| miette::miette!(error))?;
	snapshot.workspace.settings = prompt_settings.into();
	snapshot.workspace.model = omp_agent::ModelPromptInput {
		identifier:        model.clone(),
		codex_task_policy: crate::task::prompt_policy::uses_codex_task_prompt(model.as_str()),
	};
	if let Some(level) = args.thinking {
		let effort = thinking_effort(level, auto_thinking);
		snapshot.turn.params.thinking =
			Some(inference_pb::Reasoning { effort: effort as i32, ..Default::default() });
	}
	if resume.is_some() {
		let path = sessions_dir.join(format!("{}.jsonl", session.id.as_str()));
		let Session { id, journal, initial_items: _ } = session;
		let revived = omp_agent::revive_existing(&path, journal, snapshot)
			.map_err(|error| miette::miette!(error))?;
		session = Session { id, journal: revived.journal, initial_items: revived.live_items };
		snapshot = revived.snapshot;
		if let Some(model) = revived.model_override
			&& !model.fallback
		{
			snapshot.turn.params.model = format!("{}/{}", model.model.provider.0, model.model.model.0);
		}
		if !model_selector_is_selectable(catalog, &snapshot.turn.params.model) {
			let saved = snapshot.turn.params.model.clone();
			let fallback = fallback_model_selector(catalog)
				.ok_or_else(|| miette::miette!("no selectable model is available to resume"))?;
			snapshot.turn.params.model = fallback.as_str().to_owned();
			eprintln!(
				"Session model `{saved}` is unavailable; resumed with `{fallback}` without changing \
				 the session pin."
			);
		}
	}
	snapshot.workspace.model.identifier = Str::new(&snapshot.turn.params.model);
	snapshot.workspace.model.codex_task_policy =
		crate::task::prompt_policy::uses_codex_task_prompt(&snapshot.turn.params.model);
	let invocation_grant = apply_launch_tool_selection(&mut snapshot, &args, registry.as_ref())
		.map_err(|error| miette::miette!(error))?;
	let env = environment.client().with_invocation_grant(invocation_grant);
	let settings_manager = crate::settings::manager::SettingsManager::open(
		crate::settings::manager::SettingsPaths::discover(&data_dir, Some(&root)),
	)
	.map_err(|error| miette::miette!(error))?;
	let configured_autolearn = settings_manager
		.snapshot()
		.project::<crate::settings::Settings>()
		.map_err(|error| miette::miette!(error))?
		.get()
		.autolearn;
	let manage_skill_available = registry
		.devices()
		.any(|device| device.name.as_str() == "manage_skill");
	let autolearn = omp_agent::AutolearnSettings {
		enabled:        configured_autolearn.enabled && manage_skill_available,
		auto_continue:  configured_autolearn.auto_continue,
		min_tool_calls: configured_autolearn.min_tool_calls,
	};
	let active_content = crate::discovery::active_content_snapshots(&root);
	for warning in active_content.warnings.iter() {
		eprintln!("Extension load warning: {warning}");
	}
	let prompt_rules = if args.no_rules {
		Arc::from([])
	} else {
		crate::rulebook::prompt_inputs(&active_content.rules)
	};
	let prompt_skills = if args.no_skills {
		Arc::from([])
	} else {
		let discovered = crate::skills::prompt_inputs(&active_content.skills);
		match args.skills.as_ref() {
			Some(selected) => discovered
				.iter()
				.filter(|skill| selected.0.iter().any(|selector| selector == &skill.id))
				.cloned()
				.collect::<Vec<_>>()
				.into(),
			None => discovered,
		}
	};
	let context_roots = std::iter::once(&root)
		.chain(args.add_dir.iter())
		.map(|path| crate::discovery::context::GrantedContextRoot {
			root:  path.clone(),
			start: path.clone(),
		})
		.collect::<Vec<_>>();
	let context = crate::discovery::context::discover(
		&context_roots,
		&crate::discovery::context::ContextDiscoveryOptions::default(),
	);
	snapshot.workspace.context_files = crate::discovery::context::prompt_files(&context);
	snapshot.workspace = crate::prompt_prep::PromptSnapshot::freeze(
		snapshot.workspace.clone(),
		registry.as_ref(),
		Some(&snapshot.enabled_tools),
		Arc::from([]),
		Default::default(),
		Default::default(),
		Default::default(),
		prompt_rules,
		prompt_skills,
		Arc::from([]),
	)
	.workspace;
	let prepared =
		crate::prompt_prep::prepare_environment_inputs_bounded(&env, &session.journal, &root).await;
	snapshot.workspace.host = prepared.host;
	snapshot.workspace.roots = prepared.roots;
	let state = AgentState::new(snapshot);
	let initial_campaign = args.plan_mode.then_some("plan");

	if let Some(endpoint) = args.gateway {
		if args.api_key.is_some() || args.prompt_cache_key.is_some() {
			return Err(miette::miette!(
				"--api-key and --prompt-cache-key require in-process inference"
			));
		}
		let channel = endpoint
			.connect()
			.await
			.into_diagnostic()
			.wrap_err_with(|| format!("could not connect to {endpoint}"))?;
		environment
			.search_bridge()
			.bind_remote(channel.clone())
			.into_diagnostic()?;
		Box::pin(run_ui(
			RpcTurnClient::new(channel),
			&environment,
			env,
			state,
			autolearn,
			session,
			blueprint,
			Arc::clone(&eval_bridge),
			eval_control.clone(),
			None,
			data_dir.clone(),
			power_mode,
			initial_campaign,
			plan_selection,
			security_enabled,
			!args.no_title,
			ChatScope {
				catalog,
				root: &root,
				sessions_dir: &sessions_dir,
				session_index: Arc::clone(&session_index),
				registry,
				persist_sessions: !args.no_session,
			},
			start,
			presentation,
		))
		.await
		.into_diagnostic()?;
	} else {
		let (inference_registry, inference, credential_authority) =
			crate::daemon::production_inference_for_session(
				&data_dir,
				Arc::clone(&registry),
				Some(&root),
				crate::daemon::InferenceSessionOverrides {
					provider:              credential_provider,
					api_key:               args.api_key.clone(),
					prompt_cache_affinity: args.prompt_cache_key.clone(),
				},
			)
			.await
			.into_diagnostic()?;
		environment
			.search_bridge()
			.bind(inference.clone())
			.into_diagnostic()?;
		environment
			.github_credentials()
			.bind(credential_authority)
			.map_err(|_| miette::miette!("GitHub credential authority is already bound"))?;
		let client = InProcTurnClient::new(inference)
			.await
			.map_err(ChatError::from)
			.into_diagnostic()?;
		Box::pin(run_ui(
			client,
			&environment,
			env,
			state,
			autolearn,
			session,
			blueprint,
			eval_bridge,
			eval_control,
			Some(inference_registry),
			data_dir,
			power_mode,
			initial_campaign,
			plan_selection,
			security_enabled,
			!args.no_title,
			ChatScope {
				catalog,
				root: &root,
				sessions_dir: &sessions_dir,
				session_index: Arc::clone(&session_index),
				registry,
				persist_sessions: !args.no_session,
			},
			start,
			presentation,
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
pub(crate) async fn run(
	_args: ChatArgs,
	_start: ChatStart,
	_presentation: ChatPresentation,
) -> miette::Result<()> {
	use miette::IntoDiagnostic as _;
	Err(ChatError::UnsupportedPlatform).into_diagnostic()
}

fn bind_goal_todo_context(
	events: omp_agent::EventSubscription,
	modes: std::sync::Weak<crate::modes::CampaignHandle>,
) {
	drop(tokio::spawn(async move {
		while let Ok(event) = events.recv().await {
			let omp_agent::AgentEvent::ToolFinished { item, .. } = event.as_ref() else {
				continue;
			};
			let Some(item::Kind::ToolResult(result)) = item.kind.as_ref() else {
				continue;
			};
			if result.name != "todo" || result.is_error {
				continue;
			}
			let mut rendered = String::new();
			for part in &result.parts {
				if let Some(part::Kind::Text(text)) = part.kind.as_ref() {
					if !rendered.is_empty() {
						rendered.push('\n');
					}
					rendered.push_str(text);
				}
			}
			let Some(modes) = modes.upgrade() else {
				break;
			};
			modes.set_goal_todo_context(
				(!rendered.trim().is_empty()).then(|| Str::new(rendered.trim())),
			);
		}
	}));
}

#[expect(
	clippy::future_not_send,
	reason = "the designed terminal host remains confined to its event-loop thread"
)]
async fn run_ui<C: TurnClient + Clone + Send + 'static>(
	client: C,
	environment: &crate::envd::ProjectEnvironment,
	env: omp_env::EnvClient,
	mut state: AgentState,
	autolearn: omp_agent::AutolearnSettings,
	mut session: Session,
	mut blueprint: SessionBlueprint,
	eval_bridge: Arc<crate::envd::eval::SessionBridgeHost>,
	eval_control: omp_tools::eval::EvalSessionControl,
	auth_registry: Option<InferenceRegistry>,
	data_dir: PathBuf,
	power_mode: crate::power::SleepPrevention,
	initial_campaign: Option<&'static str>,
	plan_selection: Option<crate::plan::ModelSelection>,
	security_enabled: bool,
	title_enabled: bool,
	scope: ChatScope<'_>,
	mut start: ChatStart,
	presentation: ChatPresentation,
) -> Result<(), ChatError> {
	let memory_source = Arc::new(crate::memory::RuntimePromptMemorySource::new(
		environment.memory_runtime(),
		usize::MAX,
	));
	let memory_prompt = crate::memory::prompt_snapshot(
		environment.memory_runtime().as_ref(),
		None,
		None,
		usize::MAX,
	)?;
	state.update(|snapshot| snapshot.workspace.memory = memory_prompt);
	let parent = Arc::new(ChatParentHost::new(
		client.clone(),
		env.clone(),
		state.clone(),
		session.id.clone(),
		scope.sessions_dir.to_path_buf(),
		scope.root.to_path_buf(),
		Arc::clone(&scope.session_index),
		security_enabled,
	));
	parent.start_idle_parking();
	let _eval_parent_binding = eval_bridge
		.bind_sdk_parent(parent.session_id(), parent.clone())
		.map_err(|error| ChatError::EvalBridge(Str::from(error.to_string())))?;
	environment.reflection_bridge().bind(parent.clone())?;
	let cold_agents = scope.sessions_dir.join("eval-agents");
	if cold_agents.is_dir() {
		omp_agent::AgentRegistry::global().discover_transcripts(&cold_agents)?;
	}
	let auth = auth_registry.map(ChatAuthWorker::start);
	let drafts = crate::session_manager::DraftStore::new(&data_dir)?;
	let breadcrumbs = crate::project_state::TerminalBreadcrumbs::new(&data_dir)?;
	let terminal_id = omp_tui::ttyid::terminal_id();
	loop {
		if scope.persist_sessions {
			breadcrumbs.restamp(terminal_id.as_str(), &SessionId(session.id.clone()))?;
		}
		parent.update(state.clone(), session.id.clone());
		let session_root = scope.sessions_dir.join(session.id.as_str());
		ensure_state_directory(&session_root)?;
		ensure_state_directory(&session_root.join("local"))?;
		let context_window = {
			let current = state.snapshot();
			model_context_window(scope.catalog, &current.turn.params.model)
		};
		let Session { id, journal, initial_items } = session;
		let current_id = id.clone();
		let content = crate::discovery::active_content_snapshots(scope.root);
		let (ttsr, ttsr_diagnostics) = crate::rulebook::ttsr_registry(content.rules.as_ref());
		for error in ttsr_diagnostics {
			tracing::warn!(%error, "TTSR rule condition was rejected");
		}
		let mut agent =
			Agent::new(client.clone(), env.clone(), state.clone(), journal, CHAT_CAPS_BASE);
		if crate::settings::current(&data_dir)?.secrets.enabled {
			let secrets = crate::secrets::session::SecretSessionSnapshot::build(
				0,
				&data_dir.join("secrets.toml"),
				&scope.root.join(".omp/secrets.toml"),
				std::iter::empty(),
			)?;
			agent.set_secret_obfuscator(secrets.transform_handle());
		}
		agent.set_autolearn(omp_agent::AutolearnSettings {
			enabled:        false,
			auto_continue:  false,
			min_tool_calls: autolearn.min_tool_calls,
		});
		agent.set_ttsr_registry(ttsr);
		agent.set_prompt_memory_source(memory_source.clone());
		blueprint.configure_agent(&mut agent);
		match crate::daemon::production_redemption_authority(&data_dir) {
			Ok(Some(authority)) => agent.set_redemption_authority(authority),
			Ok(None) => {},
			Err(error) => {
				tracing::warn!(%error, "codex redemption authority was not constructed");
			},
		}
		parent.bind_parent_jobs(Arc::clone(agent.jobs()));
		let blob_store = omp_storage::blob::BlobStore::open(&data_dir)?;
		let artifact_catalog =
			Arc::new(parking_lot::Mutex::new(omp_storage::gc::ArtifactCatalog::open(&blob_store)?));
		agent.set_artifact_catalog(Arc::clone(&artifact_catalog));
		agent.set_blob_store(blob_store.clone());
		let capture_rx =
			omp_llm_inference::transport::global_provider_capture().subscribe(Some(id.as_str()));
		let capture_store = blob_store;
		let capture_catalog = Arc::clone(&artifact_catalog);
		let capture_session = SessionId(id.clone());
		let capture_task = tokio::spawn(async move {
			while let Ok(frame) = capture_rx.recv_async().await {
				let body = serde_json::json!({
					"sequence": frame.sequence,
					"event": frame.event,
					"payload": frame.payload,
				})
				.to_string();
				let Ok(reference) = capture_store.put(body.as_bytes()) else {
					continue;
				};
				let _ = capture_catalog.lock().adopt(
					&capture_session,
					reference.hash.into_bytes(),
					Some(reference.size),
					omp_tool::ArtifactLifetime::Session,
				);
			}
		});
		agent.set_run_activity(crate::power::PowerActivity::new(power_mode));
		let autolearn_campaign = autolearn
			.enabled
			.then(|| crate::autolearn::AutolearnCampaign::new(autolearn));
		let mut recovered_autolearn = false;
		agent.recover_campaigns(
			|spec_id| {
				if let Some(core) = omp_agent::core_regime(spec_id) {
					return Some(core);
				}
				let Some((spec, machine, _)) = autolearn_campaign.as_ref() else {
					return None;
				};
				if spec_id != crate::autolearn::AUTOLEARN_CAMPAIGN_ID || recovered_autolearn {
					return None;
				}
				recovered_autolearn = true;
				Some((
					Arc::clone(spec),
					Box::new(machine.clone()) as Box<dyn omp_agent::CampaignMachine>,
				))
			},
			now_ms(),
		)?;
		if let Some((spec, machine, _)) = autolearn_campaign.as_ref()
			&& !agent
				.arbiter()
				.campaigns()
				.entries()
				.iter()
				.any(|entry| entry.spec_id == crate::autolearn::AUTOLEARN_CAMPAIGN_ID)
		{
			let _ = agent.engage_campaign(
				Arc::clone(spec),
				Box::new(machine.clone()),
				omp_agent::EngageOptions { now_ms: now_ms(), queue: false },
			)?;
		}
		let autolearn_task = autolearn_campaign.as_ref().map(|(_, _, handle)| {
			let events = agent.events().subscribe_lossless();
			let handle = handle.clone();
			tokio::spawn(async move {
				while let Ok(event) = events.recv().await {
					handle.observe(event.as_ref());
				}
			})
		});
		if let Some(spec_id) = initial_campaign
			&& agent
				.arbiter()
				.campaigns()
				.slots()
				.owner(&omp_agent::SlotClaim::Mode)
				.is_none()
		{
			let (spec, machine) =
				omp_agent::core_regime(spec_id).expect("startup names a built-in regime");
			let _ = agent.engage_campaign(spec, machine, omp_agent::EngageOptions {
				now_ms: now_ms(),
				queue:  false,
			})?;
		}
		let modes = Arc::new(crate::modes::CampaignHandle::new());
		modes.sync_campaigns(agent.arbiter().campaigns());
		bind_goal_todo_context(agent.events().subscribe_lossless(), Arc::downgrade(&modes));
		modes.bind_plan_selection(state.clone(), plan_selection.clone());
		parent.bind_campaigns(Arc::clone(&modes));
		let _goal_binding = environment
			.goal_control()
			.bind(Arc::clone(&modes), agent.control());
		state.update(|snapshot| {
			snapshot.prompt_source = modes.prompt_source(Arc::clone(&snapshot.prompt_source));
		});
		agent.set_continuation_source(modes.clone());
		let _control_binding = environment.bind_agent_control(agent.control())?;
		environment.bind_device_availability(agent.mailbox());
		let tree = parent.tree();
		let root_budget = state
			.snapshot()
			.turn
			.params
			.task_budget
			.and_then(|budget| budget.remaining_tokens)
			.map_or_else(Budget::default, |remaining| Budget {
				max_output_tokens: Some(remaining),
				..Budget::default()
			});
		let node = tree
			.register(id.clone(), sf!("Main"), AgentKind::Main, None, id.clone(), root_budget)
			.map_err(|error| ChatError::EvalBridge(Str::from(error.to_string())))?;
		node.set_status(AgentStatus::Running);
		let broker = parent.broker();
		let inbox = broker
			.register(&node, agent.mailbox())
			.map_err(|error| ChatError::EvalBridge(Str::from(error.to_string())))?;
		parent.recover_parked_children().await;
		let _hub = hub_backend::attach(Arc::new(hub_backend::ChatHubBackend::new(
			broker,
			inbox,
			Arc::clone(agent.jobs()),
			env.clone(),
			id.clone(),
			id.clone(),
			Some(agent.events().clone()),
			Some(parent.supervisor.clone()),
		)));
		let _vibe = crate::vibe::attach_chat(Arc::clone(&parent), Arc::clone(&modes));
		let initial_draft = if scope.persist_sessions {
			drafts
				.consume(&SessionId(current_id.clone()))?
				.unwrap_or_default()
		} else {
			String::new()
		};
		let approval_book = Arc::new(omp_agent::ApprovalBook::new());
		let (_approval_route, approval_inbox) =
			omp_agent::ApprovalRoute::new(Arc::clone(&approval_book));
		let (replica_pump, replica) = omp_collab::guest::GuestRelayPump::new(
			data_dir.join("collab"),
			scope.root.to_path_buf(),
			now_ms(),
		);
		let replica_shutdown = replica.clone();
		let mut replica_task = tokio::spawn(replica_pump.run());
		let (collab_authority, collab) =
			crate::collab::session::CollabSessionAuthority::with_guest_replica(Some(replica));
		let mut collab_task = crate::collab::session::spawn_session_owner(collab_authority);
		let title = scope
			.session_index
			.subagent_tree(&SessionId(id.clone()))?
			.into_iter()
			.next()
			.map_or_else(crate::session_title::SessionTitleState::default, |session| {
				crate::session_title::SessionTitleState {
					title:  session.title,
					source: session.title_source,
				}
			});
		let outcome = chat_ui::run(
			agent,
			ChatUiSession { session_id: id, initial_items, context_window, title },
			Arc::clone(&scope.registry),
			parent.tree(),
			Arc::clone(&parent),
			Some(collab),
			modes,
			auth.as_ref().map(|worker| worker.ui().clone()),
			data_dir.clone(),
			scope.root.to_path_buf(),
			session_root.join("local"),
			security_enabled,
			title_enabled,
			vec![content.commands.to_vec()],
			content.skills,
			Some(approval_inbox),
			{
				let sessions_dir = scope.sessions_dir.to_path_buf();
				let root = scope.root.to_path_buf();
				let current_id = current_id.clone();
				move || resume_choices(&sessions_dir, &root, Some(&current_id)).into_diagnostic()
			},
			matches!(start, ChatStart::SessionIndex),
			Str::from(initial_draft),
			presentation,
		)
		.await;
		replica_shutdown.stop().await;
		if tokio::time::timeout(Duration::from_secs(3), &mut replica_task)
			.await
			.is_err()
		{
			replica_task.abort();
			let _ = replica_task.await;
		}
		if let Some(task) = autolearn_task {
			task.abort();
			let _ = task.await;
		}
		capture_task.abort();
		let _ = capture_task.await;
		if tokio::time::timeout(Duration::from_secs(3), &mut collab_task)
			.await
			.is_err()
		{
			collab_task.abort();
			let _ = collab_task.await;
		}
		let outcome = outcome.map_err(ChatError::Ui)?;
		if scope.persist_sessions {
			drafts.save(&SessionId(current_id.clone()), outcome.draft.as_str())?;
		}
		start = ChatStart::Session;
		match outcome.exit {
			omp_chat_ui::host::HostExit::Quit => break,
			omp_chat_ui::host::HostExit::Suspend => {
				#[cfg(unix)]
				if let Err(error) = nix::sys::signal::kill(
					nix::unistd::Pid::from_raw(0),
					nix::sys::signal::Signal::SIGSTOP,
				) {
					tracing::warn!(%error, "failed to suspend process group");
				}
				eval_control.request_reset();
				let model = state.snapshot().turn.params.model.clone();
				let prompt_workspace = state.snapshot().workspace.clone();
				session = open_session(
					scope.root,
					scope.sessions_dir,
					SessionOpen::Resume(&current_id),
					scope.registry.as_ref(),
					scope
						.persist_sessions
						.then(|| Arc::clone(&scope.session_index)),
				)?;
				let additional_roots = blueprint.options().additional_roots.clone();
				blueprint = session_blueprint(
					&model,
					scope.catalog,
					scope.root,
					&additional_roots,
					&session.id,
					Arc::clone(&scope.registry),
				)?;
				let mut next = agent_snapshot(&blueprint, scope.catalog)?;
				next.workspace = prompt_workspace;
				next.workspace.model.identifier = Str::new(&model);
				state = AgentState::new(next);
			},
			omp_chat_ui::host::HostExit::Resume(id) => {
				eval_control.request_reset();
				let model = state.snapshot().turn.params.model.clone();
				let prompt_workspace = state.snapshot().workspace.clone();
				session = open_session(
					scope.root,
					scope.sessions_dir,
					SessionOpen::Resume(&id),
					scope.registry.as_ref(),
					scope
						.persist_sessions
						.then(|| Arc::clone(&scope.session_index)),
				)?;
				crate::envd::migrate_session_artifacts(
					scope.sessions_dir,
					current_id.as_str(),
					session.id.as_str(),
				)
				.map_err(|source| ChatError::ProjectState {
					path: scope.sessions_dir.to_owned(),
					source,
				})?;
				let additional_roots = blueprint.options().additional_roots.clone();
				blueprint = session_blueprint(
					&model,
					scope.catalog,
					scope.root,
					&additional_roots,
					&session.id,
					Arc::clone(&scope.registry),
				)?;
				let mut next = agent_snapshot(&blueprint, scope.catalog)?;
				next.workspace = prompt_workspace;
				next.workspace.model.identifier = Str::new(&model);
				state = AgentState::new(next);
			},
			omp_chat_ui::host::HostExit::NewSession => {
				eval_control.request_reset();
				let model = state.snapshot().turn.params.model.clone();
				let prompt_workspace = state.snapshot().workspace.clone();
				session = open_session(
					scope.root,
					scope.sessions_dir,
					if scope.persist_sessions {
						SessionOpen::New
					} else {
						SessionOpen::Ephemeral
					},
					scope.registry.as_ref(),
					scope
						.persist_sessions
						.then(|| Arc::clone(&scope.session_index)),
				)?;
				crate::envd::migrate_session_artifacts(
					scope.sessions_dir,
					current_id.as_str(),
					session.id.as_str(),
				)
				.map_err(|source| ChatError::ProjectState {
					path: scope.sessions_dir.to_owned(),
					source,
				})?;
				let additional_roots = blueprint.options().additional_roots.clone();
				blueprint = session_blueprint(
					&model,
					scope.catalog,
					scope.root,
					&additional_roots,
					&session.id,
					Arc::clone(&scope.registry),
				)?;
				let mut next = agent_snapshot(&blueprint, scope.catalog)?;
				next.workspace = prompt_workspace;
				next.workspace.model.identifier = Str::new(&model);
				state = AgentState::new(next);
			},
		}
	}
	if let Some(auth) = auth {
		auth.shutdown().await;
	}
	Ok(())
}

/// Resolves the catalog streaming watchdog for one model's primary route.
///
/// Absent providers, routes, or policies leave both bounds unset, which
/// disables the loop's stream watchdog entirely.
pub(crate) fn model_stream_watchdog(
	catalog: &omp_llm_catalog::snapshot::Catalog,
	model: &str,
) -> omp_agent::StreamWatchdog {
	let watchdog = catalog
		.model(omp_llm_catalog::ModelKey::from_ref(model))
		.or_else(|| catalog.resolve_alias(model))
		.and_then(|spec| spec.routes.first())
		.and_then(|route| catalog.route(route))
		.and_then(|route| catalog.provider(&route.provider))
		.and_then(|provider| catalog.wire_policy(&provider.wire_policy))
		.and_then(|policy| policy.streaming.watchdog);
	watchdog.map_or_else(omp_agent::StreamWatchdog::default, |watchdog| omp_agent::StreamWatchdog {
		first_event_ms: watchdog.first_event_ms,
		idle_ms:        watchdog.idle_ms,
	})
}

pub(crate) fn model_context_window(
	catalog: &omp_llm_catalog::snapshot::Catalog,
	model: &str,
) -> Option<u64> {
	catalog
		.model(omp_llm_catalog::ModelKey::from_ref(model))
		.or_else(|| catalog.resolve_alias(model))
		.and_then(|spec| spec.limits.context_window)
}

/// Returns whether the catalog proves the model cannot accept declared tools.
///
/// Unknown or missing capability evidence keeps tools advertised; only
/// explicit `Unsupported` evidence (e.g. Apple's on-device model) strips them.
fn model_rejects_tools(catalog: &omp_llm_catalog::snapshot::Catalog, model: &str) -> bool {
	catalog
		.model(omp_llm_catalog::ModelKey::from_ref(model))
		.or_else(|| catalog.resolve_alias(model))
		.and_then(|spec| spec.capabilities.chat.as_ref())
		.is_some_and(|chat| chat.tools.is_unsupported())
}

fn model_selector_is_selectable(
	catalog: &omp_llm_catalog::snapshot::Catalog,
	selector: &str,
) -> bool {
	if selector.starts_with('@') {
		return true;
	}
	catalog
		.model(omp_llm_catalog::ModelKey::from_ref(selector))
		.or_else(|| catalog.resolve_alias(selector))
		.is_some_and(|model| {
			model.availability != omp_llm_catalog::ModelAvailability::Disabled
				&& model
					.routes
					.iter()
					.any(|route| catalog.route(route).is_some())
		})
}

fn fallback_model_selector(catalog: &omp_llm_catalog::snapshot::Catalog) -> Option<Str> {
	let mru = BTreeMap::new();
	omp_llm_catalog::find_smol(catalog.models(), catalog.routes(), &mru)
		.or_else(|| omp_llm_catalog::pick_default(catalog.models(), catalog.routes(), &mru))
		.map(|selected| Str::from(selected.model.as_str()))
}

/// Canonicalizes a `--model` selector to its exact catalog key.
///
/// Exact keys pass through; declared catalog aliases resolve to their target
/// key; role selectors (`@…`) defer to downstream resolution. A route id or
/// unknown selector fails fast instead of surfacing as a mid-turn
/// `TargetNotFound`.
pub(crate) fn resolve_model_selector(
	catalog: &omp_llm_catalog::snapshot::Catalog,
	selector: &str,
) -> Result<Str, ChatError> {
	if selector.starts_with('@')
		|| catalog
			.model(omp_llm_catalog::ModelKey::from_ref(selector))
			.is_some()
	{
		return Ok(selector.into());
	}
	if let Some(spec) = catalog.resolve_alias(selector) {
		return Ok(spec.key.as_str().into());
	}
	if let Some(route) = catalog.route(omp_llm_catalog::RouteId::from_ref(selector)) {
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
/// Selects the exact provider domain receiving an invocation credential.
pub(crate) fn resolve_model_provider(
	catalog: &omp_llm_catalog::snapshot::Catalog,
	model: &str,
	requested: Option<&str>,
) -> Result<omp_llm_catalog::ProviderId, ChatError> {
	let spec = catalog
		.model(omp_llm_catalog::ModelKey::from_ref(model))
		.ok_or_else(|| ChatError::UnknownModel {
			selector:    model.into(),
			suggestions: Str::empty(),
		})?;
	if let Some(requested) = requested {
		let provider = omp_llm_catalog::ProviderId::from(requested);
		if spec.routes.iter().any(|route| {
			catalog
				.route(route)
				.is_some_and(|route| route.provider == provider)
		}) {
			return Ok(provider);
		}
		return Err(ChatError::ModelProviderUnavailable { model: model.into(), provider });
	}
	spec
		.routes
		.iter()
		.filter_map(|route| catalog.route(route))
		.next()
		.map(|route| route.provider.clone())
		.ok_or_else(|| ChatError::ModelHasNoProvider { model: model.into() })
}

pub(crate) fn canonical_project(path: &Path) -> Result<PathBuf, ChatError> {
	let root = std::fs::canonicalize(path)
		.map_err(|source| ChatError::Project { path: path.to_owned(), source })?;
	if !root.is_dir() {
		return Err(ChatError::ProjectNotDirectory(root));
	}
	Ok(root)
}

pub(crate) fn open_session(
	root: &Path,
	sessions_dir: &Path,
	open: SessionOpen<'_>,
	registry: &Registry,
	session_index: Option<Arc<SessionIndex>>,
) -> Result<Session, ChatError> {
	let source = match open {
		SessionOpen::Resume(id) | SessionOpen::ResumeMoved(id) | SessionOpen::Fork(id) => {
			Some(strict_session_id(id)?)
		},
		SessionOpen::New | SessionOpen::Ephemeral => None,
	};
	let id = if matches!(open, SessionOpen::Resume(_) | SessionOpen::ResumeMoved(_)) {
		source.clone().expect("resume has a validated source")
	} else {
		Str::from(omp_core::Ulid::generate().to_string())
	};
	let path = sessions_dir.join(format!("{}.jsonl", id.as_str()));
	let mut journal = match open {
		SessionOpen::Resume(_) | SessionOpen::ResumeMoved(_) => {
			validate_session_file(&path).map_err(|source_error| {
				if source_error.kind() == std::io::ErrorKind::NotFound {
					ChatError::MissingResume(id.clone())
				} else {
					ChatError::ProjectState { path: path.clone(), source: source_error }
				}
			})?;
			let mut journal = Journal::open(&path)?;
			let view = journal.load()?;
			if view.header().id.0 != id {
				return Err(ChatError::SessionMismatch(id));
			}
			let recorded_root = view.header().cwd.clone();
			drop(view);
			let current_root = journal.workspace_roots(&recorded_root)?;
			if current_root.primary() != root && !matches!(open, SessionOpen::ResumeMoved(_)) {
				return Err(ChatError::SessionProjectMismatch { session: id });
			}
			let index = session_index.ok_or(ChatError::MissingSessionIndex)?;
			journal.attach_session_index(index, SessionId(id.clone()));
			if current_root.primary() != root {
				journal.move_workspace_root(now_ms(), root.to_owned())?;
			}
			journal
		},
		SessionOpen::Fork(_) => {
			let source_id = source.as_ref().expect("fork has a validated source");
			let source_path = sessions_dir.join(format!("{}.jsonl", source_id.as_str()));
			validate_session_file(&source_path).map_err(|source_error| {
				if source_error.kind() == std::io::ErrorKind::NotFound {
					ChatError::MissingResume(source_id.clone())
				} else {
					ChatError::ProjectState { path: source_path.clone(), source: source_error }
				}
			})?;
			let index = session_index.ok_or(ChatError::MissingSessionIndex)?;
			create_indexed_fork(&source_path, &path, root, &id, source_id, index)?
		},
		SessionOpen::New => create_indexed_journal(
			&path,
			root,
			&id,
			session_index.ok_or(ChatError::MissingSessionIndex)?,
			SessionKind::Interactive,
			None,
		)?,
		SessionOpen::Ephemeral => Journal::create(&path, &Header {
			v:       4,
			id:      SessionId(id.clone()),
			created: now_ms(),
			cwd:     root.to_owned(),
		})?,
	};
	let view = journal.load()?;
	let initial_items = project_journal(&view, view.as_ref(), registry, &CHAT_CAPS_BASE)?.items;
	drop(view);
	Ok(Session { id, journal, initial_items })
}

pub(crate) fn create_indexed_journal(
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

pub(crate) fn create_indexed_handoff(
	parent: &Journal,
	path: &Path,
	root: &Path,
	commit: omp_agent::handoff::HandoffCommit,
	tokens_before: u64,
	tokens_after: Option<u64>,
	session_index: Arc<SessionIndex>,
	save_to_disk: bool,
) -> Result<Option<Journal>, ChatError> {
	if !save_to_disk {
		return Ok(None);
	}
	let child_id = SessionId(commit.child_session_id.clone());
	let parent_id = SessionId(commit.request.parent_session_id.clone());
	let created_ms = now_ms();
	let root_text = root.to_string_lossy();
	let request = NewSession {
		id: &child_id,
		cwd: root_text.as_ref(),
		project: root_text.as_ref(),
		created_ms,
		kind: SessionKind::Interactive,
		parent: Some(&parent_id),
		remote: false,
	};
	let header = Header {
		v:       4,
		id:      child_id.clone(),
		created: created_ms,
		cwd:     root.to_owned(),
	};
	let checkpoint = commit.request.parent_checkpoint;
	let compact = commit.compact(tokens_before, tokens_after);
	let result = session_index.create_session(&request, || {
		let journal = parent.create_handoff_child(path, &header, created_ms, checkpoint, compact)?;
		let watermark = journal.byte_watermark()?;
		Ok::<_, omp_agent::JournalError>((journal, watermark))
	});
	let mut journal = match result {
		Ok(journal) => journal,
		Err(IndexedWriteError::Journal(error)) => return Err(error.into()),
		Err(
			IndexedWriteError::IndexBeforeJournal(error)
			| IndexedWriteError::IndexAfterJournal { source: error, .. },
		) => return Err(ChatError::SessionIndex(error)),
	};
	journal.attach_session_index(session_index, child_id);
	Ok(Some(journal))
}

fn create_indexed_fork(
	source_path: &Path,
	child_path: &Path,
	root: &Path,
	child_id: &Str,
	source_id: &Str,
	session_index: Arc<SessionIndex>,
) -> Result<Journal, ChatError> {
	let source = Journal::open(source_path)?;
	let source_view = source.load()?;
	if source_view.header().id.0 != *source_id {
		return Err(ChatError::SessionMismatch(source_id.clone()));
	}
	let recorded_root = source_view.header().cwd.clone();
	drop(source_view);
	if source.workspace_roots(&recorded_root)?.primary() != root {
		return Err(ChatError::SessionProjectMismatch { session: source_id.clone() });
	}
	let session_id = SessionId(child_id.clone());
	let parent_id = SessionId(source_id.clone());
	let created_ms = now_ms();
	let root_text = root.to_string_lossy();
	let request = NewSession {
		id: &session_id,
		cwd: root_text.as_ref(),
		project: root_text.as_ref(),
		created_ms,
		kind: SessionKind::Interactive,
		parent: Some(&parent_id),
		remote: false,
	};
	let result = session_index.create_session(&request, || {
		let journal = source.create_child(
			child_path,
			&Header {
				v:       4,
				id:      session_id.clone(),
				created: created_ms,
				cwd:     root.to_owned(),
			},
			created_ms,
			ChildKind::Fork,
		)?;
		let watermark = journal.byte_watermark()?;
		Ok::<_, omp_agent::JournalError>((journal, watermark))
	});
	let mut journal = match result {
		Ok(journal) => journal,
		Err(IndexedWriteError::Journal(error)) => return Err(error.into()),
		Err(
			IndexedWriteError::IndexBeforeJournal(error)
			| IndexedWriteError::IndexAfterJournal { source: error, .. },
		) => return Err(ChatError::SessionIndex(error)),
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
	if let Ok(parsed) = id.as_str().parse::<omp_core::Ulid>()
		&& parsed.to_string() == id.as_str()
	{
		return Ok(id.clone());
	}
	let bytes = id.as_bytes();
	let canonical_uuid = bytes.len() == 36
		&& bytes.iter().enumerate().all(|(index, byte)| {
			if matches!(index, 8 | 13 | 18 | 23) {
				*byte == b'-'
			} else {
				byte.is_ascii_digit() || matches!(*byte, b'a'..=b'f')
			}
		});
	if canonical_uuid {
		Ok(id.clone())
	} else {
		Err(ChatError::InvalidResume(id.clone()))
	}
}

/// Resolves the shared SDK blueprint over the production registry and durable
/// journal identity used by chat and print construction.
pub(crate) fn session_blueprint(
	model: &str,
	catalog: &omp_llm_catalog::snapshot::Catalog,
	root: &Path,
	additional_roots: &[PathBuf],
	session_id: &Str,
	registry: Arc<Registry>,
) -> Result<SessionBlueprint, ChatError> {
	let mut options = SessionOptions::new(root);
	options.additional_roots = additional_roots
		.iter()
		.map(|path| {
			std::fs::canonicalize(path)
				.map_err(|source| ChatError::Project { path: path.clone(), source })
		})
		.collect::<Result<Vec<_>, _>>()?
		.into_boxed_slice();
	options.identity.id = Some(session_id.clone());
	options.model_selectors = Box::new([Str::new(model)]);
	let mut workspace = WorkspaceInput::new(root, Arc::from([]));
	let mut roots = Vec::with_capacity(options.additional_roots.len() + 1);
	for (index, path) in std::iter::once(&options.cwd)
		.chain(options.additional_roots.iter())
		.enumerate()
	{
		let uri = omp_sdk::Url::from_directory_path(path).map_err(|()| {
			ChatError::SessionBuild(omp_sdk::SessionBuildError::InvalidRoot { path: path.clone() })
		})?;
		let grant = if index == 0 {
			sf!("primary")
		} else {
			sf!("root-{index}")
		};
		roots.push(WorkspaceRootInput::new(
			Str::new(uri.as_str()),
			bytes::Bytes::copy_from_slice(grant.as_bytes()),
		));
	}
	workspace.roots =
		WorkspaceRootsInput { revision: 0, primary: roots.first().cloned(), roots: roots.into() };
	SessionBuilder::new(options, registry)
		.firehose(Arc::new(omp_telemetry::firehose::Firehose::new()))
		.build(catalog, &workspace)
		.map_err(Into::into)
}

pub(crate) fn agent_snapshot(
	blueprint: &SessionBlueprint,
	catalog: &omp_llm_catalog::snapshot::Catalog,
) -> Result<AgentSnapshot, ChatError> {
	let model = blueprint
		.model_plan()
		.candidates()
		.first()
		.map(|candidate| candidate.selector.as_str())
		.ok_or(omp_sdk::SessionBuildError::NoDefaultModel)?;
	let registry = blueprint.registry();
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
	let session_id = blueprint
		.options()
		.identity
		.id
		.as_ref()
		.expect("SessionBuilder always assigns a session id");
	let turn = TurnOptions {
		context_id: Some(session_id.clone()),
		params: inference_pb::ChatParams {
			model: model.to_owned(),
			tools,
			..inference_pb::ChatParams::default()
		},
		stream_watchdog: model_stream_watchdog(catalog, model),
		..TurnOptions::default()
	};
	let mut snapshot = AgentSnapshot::new(turn, blueprint.workspace().clone(), Arc::clone(registry));
	snapshot.workspace = crate::prompt_prep::PromptSnapshot::freeze(
		snapshot.workspace.clone(),
		registry,
		Some(&enabled_tools),
		Arc::from([]),
		Default::default(),
		Default::default(),
		Default::default(),
		Arc::from([]),
		Arc::from([]),
		Arc::from([]),
	)
	.workspace;
	snapshot.enabled_tools = enabled_tools.into();
	Ok(snapshot)
}

fn apply_launch_tool_selection(
	snapshot: &mut AgentSnapshot,
	args: &ChatArgs,
	registry: &Registry,
) -> Result<omp_env::InvocationGrant, ChatError> {
	let known = registry
		.prompt_projection(None)
		.entries()
		.map(|entry| entry.name.clone())
		.collect::<BTreeSet<_>>();
	let requested = args
		.tools
		.as_ref()
		.map(|tools| tools.0.iter().cloned().collect::<BTreeSet<_>>());
	if let Some(requested) = &requested
		&& let Some(unknown) = requested.iter().find(|name| !known.contains(*name))
	{
		return Err(ChatError::UnknownTool {
			name:  unknown.clone(),
			valid: known.into_iter().collect(),
		});
	}
	let allowed = |name: &str| {
		if args.no_tools {
			return false;
		}
		if args.no_lsp && (name.contains("lsp") || matches!(name, "diagnostics" | "format")) {
			return false;
		}
		requested
			.as_ref()
			.is_none_or(|requested| requested.contains(name))
	};
	snapshot
		.turn
		.params
		.tools
		.retain(|tool| allowed(&tool.name));
	snapshot.enabled_tools = snapshot
		.enabled_tools
		.iter()
		.filter(|name| allowed(name))
		.cloned()
		.collect();
	let grant = omp_env::InvocationGrant::unrestricted();
	Ok(if args.no_pty { grant.deny_pty() } else { grant })
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

pub(crate) fn ensure_state_directory(path: &Path) -> Result<(), ChatError> {
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
		let native = omp_llm_catalog::ModelKey::from_ref("anthropic/claude-fable-5");
		let shadowed = omp_llm_catalog::ModelKey::from_ref("openrouter/anthropic/claude-fable-5");
		assert!(catalog.model(native).is_some(), "fixture key missing from catalog");
		assert!(catalog.model(shadowed).is_some(), "fixture aggregator key missing from catalog");
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
				.provider(omp_llm_catalog::ProviderId::from_ref(named_provider))
				.is_some()
			{
				flat_ids.insert((flat_id, named_provider));
			}
		}
		assert!(!flat_ids.is_empty(), "the catalog carries aggregator flat ids to check");
		for (flat_id, named_provider) in flat_ids {
			let resolved = catalog
				.model(omp_llm_catalog::ModelKey::from_ref(flat_id))
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

		let provider = omp_llm_catalog::ProviderId::from_ref("kimi-code");
		let error = Error::new(
			ErrorKind::Authentication,
			ErrorPhase::Authentication,
			RetryAction::Never,
			ExecutionReceipt::default(),
		)
		.status(Some(401))
		.code(sf!("invalid_grant"))
		.detail(ErrorDetail::provider(sf!("device authorization expired")));
		let ChatLoginFailure::Message(message) = chat_login_failure(provider, &error) else {
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
			SessionOpen::Resume(&id),
			&Registry::new(),
			Some(Arc::new(
				SessionIndex::open(state_dir.join("sessions.sqlite3")).expect("session index"),
			)),
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
			SessionOpen::Resume(&id),
			&Registry::new(),
			Some(Arc::new(
				SessionIndex::open(scratch.path().join("sessions.sqlite3")).expect("session index"),
			)),
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
				parent_outcome("follow-up answer"),
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
			false,
		);
		host
			.tree()
			.register(
				sf!("parent-session"),
				sf!("Main"),
				AgentKind::Main,
				None,
				sf!("parent-session"),
				Budget::default(),
			)
			.expect("root registration");

		let completion = crate::envd::eval::ParentSessionHost::completion(
			&host,
			json!({"prompt":"complete this","model":"default"}),
			&crate::envd::eval::NoopBridgeProgress,
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
				&crate::envd::eval::NoopBridgeProgress,
			),
		)
		.await
		.expect("child agent must not deadlock on the occupied parent eval kernel")
		.expect("live agent call");
		assert_eq!(agent["text"], "agent answer");
		assert_eq!(agent["details"]["agent"], "task");
		let stable_id = agent["details"]["id"]
			.as_str()
			.filter(|id| !id.is_empty())
			.expect("agent bridge did not return its durable child id");
		let follow_up = crate::envd::eval::ParentSessionHost::agent(
			&host,
			json!({"prompt":"follow up","agent":"task","stableId":stable_id}),
			&crate::envd::eval::NoopBridgeProgress,
		)
		.await
		.expect("retained child follow-up");
		assert_eq!(follow_up["text"], "follow-up answer");
		assert_eq!(follow_up["details"]["id"], stable_id);
		assert_eq!(follow_up["details"]["followUp"], true);

		let options = options.lock();
		assert_eq!(options.len(), 3);
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
		assert_eq!(inputs.len(), 3);
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
		assert!(matches!(&inputs[2], TurnInput::Full(thread)
			if thread.items.iter().any(|item| matches!(
				&item.kind,
				Some(item::Kind::Message(message))
					if message.role == i32::from(Role::User)
						&& message.parts.iter().any(|part| matches!(
							&part.kind,
							Some(part::Kind::Text(text)) if text == "follow up"
						))
			))
		));
	}
}
