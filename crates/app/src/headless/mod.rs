//! Durable non-interactive session assembly shared by print, RPC, and ACP.

pub mod finalize;

use std::{path::PathBuf, sync::Arc};

use miette::{Context as _, IntoDiagnostic as _};
use omp_agent::{
	Agent, AgentEvent, AgentKind, AgentRunSummary, AgentState, AgentStatus, AgentTree, ApprovalBook,
	ApprovalInbox, ApprovalRoute, Budget, EventSubscription, InProcTurnClient, TurnId,
};
use omp_core::{Str, sf};
use omp_llm_inference::Registry as InferenceRegistry;
use omp_proto::thread::v1::Item;
use omp_sdk::{SessionHandle, SessionIdentity, SessionRuntime};

use self::finalize::{FinalizerBudget, FinalizerReport, HeadlessFinalizerHandle};
use crate::{
	chat,
	exthost::lifecycle::{HeadlessLifecycleSink, HeadlessLifecycleSubscription},
	modes::ExecutionModes,
};

/// Inputs required to create one production headless session.
#[derive(Clone, Debug)]
pub struct HeadlessSessionOptions {
	/// Project root whose Environment owns all effects.
	pub project:            PathBuf,
	/// Additional Environment-authorized workspace roots.
	pub additional_roots:   Box<[PathBuf]>,
	/// Resolved catalog model selector.
	pub model:              Str,
	/// Existing durable session to resume, or a fresh journal when absent.
	pub resume:             Option<Str>,
	/// Whether the Python eval device is enabled.
	pub py_eval:            bool,
	/// Session-incarnation fence stamped onto observable events.
	pub session_generation: u64,
}

/// Single owner of every authority needed by a non-interactive agent loop.
///
/// Field order is deliberate: the Agent and its cloned Environment client are
/// dropped before the project Environment authority.
pub struct HeadlessSession {
	session:             SessionHandle,
	env:                 omp_env::EnvClient,
	modes:               Arc<ExecutionModes>,
	tree:                Arc<AgentTree>,
	events:              Option<EventSubscription>,
	lifecycle:           HeadlessLifecycleSink,
	lifecycle_events:    Option<HeadlessLifecycleSubscription>,
	approval_book:       Arc<ApprovalBook>,
	approval_route:      ApprovalRoute,
	approval_inbox:      Option<ApprovalInbox>,
	finalizer:           HeadlessFinalizerHandle,
	session_id:          Str,
	initial_items:       Vec<Item>,
	_inference_registry: InferenceRegistry,
	_environment:        crate::envd::ProjectEnvironment,
}

impl HeadlessSession {
	/// Constructs the production Environment, v4 journal, agent loop, tree,
	/// extension sink, approval route, and lossless event subscription.
	pub async fn open(data_dir: PathBuf, options: HeadlessSessionOptions) -> miette::Result<Self> {
		let root =
			chat::canonical_project(&options.project).map_err(|error| miette::miette!(error))?;
		let catalog = omp_llm_catalog::snapshot::Catalog::try_embedded().into_diagnostic()?;
		let model = chat::resolve_model_selector(catalog, options.model.as_str())
			.map_err(|error| miette::miette!(error))?;
		let settings = crate::settings::current(&data_dir).into_diagnostic()?;
		let state_dir = crate::project_state::directory(&data_dir, &root).into_diagnostic()?;
		let sessions_dir = state_dir.join("sessions");
		chat::ensure_state_directory(&state_dir).map_err(|error| miette::miette!(error))?;
		chat::ensure_state_directory(&sessions_dir).map_err(|error| miette::miette!(error))?;
		let environment = crate::envd::ProjectEnvironment::connect_or_start(
			&root,
			&state_dir,
			&crate::project_state::environment_socket(&state_dir),
			&crate::project_state::document_socket(&state_dir),
			options.py_eval,
			settings.runtime_durations().interrupt_grace,
		)
		.await
		.map_err(|error| miette::miette!(error))
		.wrap_err("could not start the project Environment for headless mode")?;
		let env = environment.client().clone();
		let registry = environment.registry();
		let session = chat::open_session(
			&root,
			&sessions_dir,
			options
				.resume
				.as_ref()
				.map_or(chat::SessionOpen::New, chat::SessionOpen::Resume),
			registry.as_ref(),
			Some(environment.sessions_index()),
		)
		.map_err(|error| miette::miette!(error))?;
		let blueprint = chat::session_blueprint(
			model.as_str(),
			catalog,
			&root,
			&options.additional_roots,
			&session.id,
			Arc::clone(&registry),
		)
		.map_err(|error| miette::miette!(error))?;
		let snapshot =
			chat::agent_snapshot(&blueprint, catalog).map_err(|error| miette::miette!(error))?;
		let mut state = AgentState::new(snapshot);
		let (inference_registry, inference, credential_authority) =
			crate::daemon::production_inference(&data_dir, Arc::clone(&registry), Some(&root))
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
		let client = InProcTurnClient::new(inference).await.into_diagnostic()?;
		let journal_path = sessions_dir.join(format!("{}.jsonl", session.id.as_str()));
		let content = crate::discovery::active_content_snapshots(&root);
		let (ttsr, ttsr_diagnostics) = crate::rulebook::ttsr_registry(content.rules.as_ref());
		for error in ttsr_diagnostics {
			tracing::warn!(%error, "headless TTSR rule condition was rejected");
		}
		let mut agent =
			Agent::new(client, env.clone(), state.clone(), session.journal, chat::CHAT_CAPS_BASE);
		blueprint.configure_agent(&mut agent);
		agent.set_ttsr_registry(ttsr);
		agent
			.events()
			.set_session_generation(options.session_generation);
		let modes = Arc::new(ExecutionModes::new(agent.execution_mode()));
		state.update(|snapshot| {
			snapshot.prompt_source = modes.prompt_source(Arc::clone(&snapshot.prompt_source));
		});
		agent.set_continuation_source(modes.clone());
		let tree = Arc::new(AgentTree::standard(8));
		let node = tree
			.register(
				session.id.clone(),
				sf!("Main"),
				AgentKind::Main,
				None,
				session.id.clone(),
				Budget::default(),
			)
			.into_diagnostic()?;
		node.set_status(AgentStatus::Running);
		let session_handle = blueprint
			.launch(
				SessionIdentity { id: session.id.clone(), journal_path, expected_revision: None },
				SessionRuntime::from_agent(agent),
				None,
			)
			.into_diagnostic()?;
		let events = session_handle.subscribe_lossless();
		let (lifecycle, lifecycle_events) = HeadlessLifecycleSink::new(options.session_generation);
		let approval_book = Arc::new(ApprovalBook::new());
		let (approval_route, approval_inbox) = ApprovalRoute::new(Arc::clone(&approval_book));
		Ok(Self {
			session: session_handle,
			env,
			modes,
			tree,
			events: Some(events),
			lifecycle,
			lifecycle_events: Some(lifecycle_events),
			approval_book,
			approval_route,
			approval_inbox: Some(approval_inbox),
			finalizer: HeadlessFinalizerHandle::new(),
			session_id: session.id,
			initial_items: session.initial_items,
			_inference_registry: inference_registry,
			_environment: environment,
		})
	}

	/// Submits caller-authored items through the durable agent loop.
	pub async fn submit(
		&mut self,
		items: impl IntoIterator<Item = Item>,
		turn_id: TurnId,
	) -> Result<AgentRunSummary, omp_sdk::SessionHandleError> {
		self.session.submit(items, turn_id).await
	}

	/// Returns the durable session identifier.
	#[must_use]
	pub fn session_id(&self) -> &str {
		self.session_id.as_str()
	}

	/// Returns the canonical replay projection loaded before the first turn.
	#[must_use]
	pub fn initial_items(&self) -> &[Item] {
		&self.initial_items
	}

	/// Returns the Environment client owned alongside the agent.
	#[must_use]
	pub const fn env(&self) -> &omp_env::EnvClient {
		&self.env
	}

	/// Returns the session-scoped execution modes.
	#[must_use]
	pub fn modes(&self) -> &ExecutionModes {
		self.modes.as_ref()
	}

	/// Returns the append-only agent roster.
	#[must_use]
	pub fn tree(&self) -> &Arc<AgentTree> {
		&self.tree
	}

	/// Takes the single ordered lossless agent-event subscription.
	pub fn take_events(&mut self) -> Option<EventSubscription> {
		self.events.take()
	}

	/// Returns the generation-fenced extension lifecycle sink.
	#[must_use]
	pub const fn lifecycle_sink(&self) -> &HeadlessLifecycleSink {
		&self.lifecycle
	}

	/// Takes the single lossless extension lifecycle subscription.
	pub fn take_lifecycle_events(&mut self) -> Option<HeadlessLifecycleSubscription> {
		self.lifecycle_events.take()
	}

	/// Returns the durable approval book.
	#[must_use]
	pub fn approval_book(&self) -> &Arc<ApprovalBook> {
		&self.approval_book
	}

	/// Returns the awaitable approval route.
	#[must_use]
	pub const fn approval_route(&self) -> &ApprovalRoute {
		&self.approval_route
	}

	/// Takes the single host-facing approval inbox.
	pub fn take_approval_inbox(&mut self) -> Option<ApprovalInbox> {
		self.approval_inbox.take()
	}

	/// Returns the session-owned finalizer for authority registration.
	pub const fn finalizer_mut(&mut self) -> &mut HeadlessFinalizerHandle {
		&mut self.finalizer
	}

	/// Runs ordered bounded finalization. Dropping this session afterward
	/// disposes the agent and Environment last.
	pub async fn finalize<W>(&mut self, stdout: &mut W, budget: FinalizerBudget) -> FinalizerReport
	where
		W: tokio::io::AsyncWrite + Unpin,
	{
		let report = std::mem::take(&mut self.finalizer)
			.finalize(stdout, budget)
			.await;
		let _ = self.session.dispose().await;
		report
	}

	/// Publishes an additional event through the session's generation-stamped
	/// event bus. Intended for typed mode transitions owned by protocol hosts.
	pub fn publish(&self, event: AgentEvent) {
		self.session.publish(event);
	}
}
