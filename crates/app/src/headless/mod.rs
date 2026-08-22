//! Durable non-interactive session assembly shared by print, RPC, and ACP.

pub mod finalize;

use std::{path::PathBuf, sync::Arc};

use miette::{Context as _, IntoDiagnostic as _};
use omp_agent::{
	Agent, AgentEvent, AgentKind, AgentRunSummary, AgentState, AgentStatus, AgentTree, ApprovalBook,
	ApprovalInbox, ApprovalRoute, Budget, EventSubscription, InProcTurnClient, TurnId,
};
use omp_core::{Str, sf};
use omp_llm_catalog::ModelKey;
use omp_llm_inference::Registry as InferenceRegistry;
use omp_proto::thread::v1::Item;
use omp_sdk::{SessionHandle, SessionIdentity, SessionRuntime};
use omp_storage::transcript::{
	ModelChange as JournalModelChange, ModelId as JournalModelId, ModelRef as JournalModelRef,
	ProviderId as JournalProviderId,
};

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
	/// Existing durable session whose live projection is copied into a fork.
	pub fork:               Option<Str>,
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
	state:               AgentState,
	control:             omp_agent::ControlSender,
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
		let open = if let Some(source) = options.fork.as_ref() {
			chat::SessionOpen::Fork(source)
		} else if let Some(source) = options.resume.as_ref() {
			chat::SessionOpen::Resume(source)
		} else {
			chat::SessionOpen::New
		};
		let mut session = chat::open_session(
			&root,
			&sessions_dir,
			open,
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
		let mut snapshot =
			chat::agent_snapshot(&blueprint, catalog).map_err(|error| miette::miette!(error))?;
		if options.resume.is_some() || options.fork.is_some() {
			let journal_path = sessions_dir.join(format!("{}.jsonl", session.id.as_str()));
			let revived = omp_agent::revive_existing(&journal_path, session.journal, snapshot)
				.map_err(|error| miette::miette!(error))?;
			session.journal = revived.journal;
			session.initial_items = revived.live_items;
			snapshot = revived.snapshot;
			if let Some(model) = revived.model_override
				&& !model.fallback
			{
				snapshot.turn.params.model =
					format!("{}/{}", model.model.provider.0, model.model.model.0);
			}
		}
		let autolearn = omp_agent::AutolearnSettings {
			enabled:        settings.autolearn.enabled
				&& registry
					.devices()
					.any(|device| device.name.as_str() == "manage_skill"),
			auto_continue:  settings.autolearn.auto_continue,
			min_tool_calls: settings.autolearn.min_tool_calls,
		};
		let state = AgentState::new(snapshot);
		let (inference_registry, inference, credential_authority) =
			crate::daemon::production_inference(&data_dir, Arc::clone(&registry), Some(&root))
				.await
				.into_diagnostic()?;
		let _ = environment.search_bridge().bind(inference.clone());
		let _ = environment.github_credentials().bind(credential_authority);
		let client = InProcTurnClient::new(inference).await.into_diagnostic()?;
		let journal_path = sessions_dir.join(format!("{}.jsonl", session.id.as_str()));
		let content = crate::discovery::active_content_snapshots(&root);
		let (ttsr, ttsr_diagnostics) = crate::rulebook::ttsr_registry(content.rules.as_ref());
		for error in ttsr_diagnostics {
			tracing::warn!(%error, "headless TTSR rule condition was rejected");
		}
		let mut agent =
			Agent::new(client, env.clone(), state.clone(), session.journal, chat::CHAT_CAPS_BASE);
		agent.set_autolearn(autolearn);
		blueprint.configure_agent(&mut agent);
		agent.set_ttsr_registry(ttsr);
		agent
			.events()
			.set_session_generation(options.session_generation);
		let control = agent.control();
		let (mode_store, projection) = crate::modes::persistence::ModePersistence::open(
			agent.journal(),
			control.clone(),
			session.id.as_str(),
			root.to_string_lossy().as_ref(),
		)
		.map_err(|error| miette::miette!(error))?;
		let modes = Arc::new(ExecutionModes::from_projection(
			agent.execution_mode(),
			projection.unwrap_or_default(),
		));
		modes.attach_persistence(mode_store);
		modes.bind_plan_selection(state.clone(), None);
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
		environment
			.bind_approval_authority(Some(Arc::clone(&approval_book)), Some(approval_route.clone()));
		Ok(Self {
			session: session_handle,
			state,
			control,
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

	/// Binds or clears the session-scoped ACP terminal execution capability.
	pub(crate) fn bind_acp_exec(
		&self,
		backend: Option<Arc<dyn crate::envd::tool_shell::AcpExecBackend>>,
	) {
		self._environment.bind_acp_exec(backend);
	}

	/// Binds or clears the session-scoped ACP document capability.
	pub(crate) fn bind_acp_documents(
		&self,
		backend: Option<Arc<dyn crate::envd::docs::AcpDocumentBackend>>,
	) {
		self._environment.bind_acp_documents(backend);
	}

	/// Binds or clears the durable approval authority.
	pub(crate) fn bind_approval_authority(
		&self,
		book: Option<Arc<ApprovalBook>>,
		route: Option<ApprovalRoute>,
	) {
		self._environment.bind_approval_authority(book, route);
	}

	/// Returns the current session-effective model selector.
	#[must_use]
	pub fn model(&self) -> Str {
		Str::new(self.state.snapshot().turn.params.model.as_str())
	}

	/// Applies a validated session-only model override and records it in the
	/// owning v4 journal before changing the live snapshot.
	pub async fn set_model(&self, selector: &str) -> miette::Result<()> {
		let catalog = omp_llm_catalog::snapshot::Catalog::try_embedded().into_diagnostic()?;
		let model =
			chat::resolve_model_selector(catalog, selector).map_err(|error| miette::miette!(error))?;
		let spec = catalog
			.model(ModelKey::from_ref(model.as_str()))
			.ok_or_else(|| miette::miette!("unknown model `{selector}`"))?;
		let route = spec
			.routes
			.first()
			.and_then(|route| catalog.route(route))
			.ok_or_else(|| miette::miette!("model `{selector}` has no selectable route"))?;
		self
			.control
			.model_override(now_ms(), JournalModelChange {
				role:     sf!("temporary"),
				model:    JournalModelRef {
					provider: JournalProviderId(Str::new(route.provider.as_str())),
					api:      Str::new(route.codec.as_str()),
					model:    JournalModelId(Str::new(spec.key.as_str())),
				},
				fallback: false,
			})
			.await
			.into_diagnostic()?;
		self
			.state
			.update(|snapshot| snapshot.turn.params.model = model.to_string());
		Ok(())
	}

	/// Replaces the session-only provider reasoning request after the ACP host
	/// has clamped it through the selected model policy.
	pub fn set_thinking(&self, thinking: Option<omp_proto::inference::v1::Reasoning>) {
		self
			.state
			.update(|snapshot| snapshot.turn.params.thinking = thinking);
	}

	/// Interrupts the active caller submission without waiting for settlement.
	pub fn interrupt(&self) {
		self.session.interrupt();
	}

	/// Returns a cheap interrupt-only capable clone of the durable handle.
	///
	/// Protocol hosts use this before borrowing the session mutably for a
	/// submission so cancellation never contends on their session mutex.
	#[must_use]
	pub fn interrupt_handle(&self) -> SessionHandle {
		self.session.clone()
	}

	/// Records a user-visible session title through the sole journal owner.
	pub async fn set_title(&self, title: Str) -> miette::Result<()> {
		self
			.control
			.set_title(now_ms(), title)
			.await
			.into_diagnostic()?;
		Ok(())
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

fn now_ms() -> u64 {
	use std::time::{SystemTime, UNIX_EPOCH};

	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or_default()
		.as_millis()
		.try_into()
		.unwrap_or(u64::MAX)
}
