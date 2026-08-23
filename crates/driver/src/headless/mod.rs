//! Durable non-interactive session assembly shared by print, RPC, and ACP.

pub mod finalize;

use std::{error, path::PathBuf, sync::Arc};

use omp_agent::{
	Agent, AgentEvent, AgentKind, AgentRunSummary, AgentState, AgentStatus, AgentTree, ApprovalBook,
	ApprovalInbox, ApprovalRoute, Budget, EventSubscription, InProcTurnClient, TurnId,
};
use omp_catalog::{ModelKey, ProviderId, snapshot};
use omp_core::{SecretString, Str, sf};
use omp_inference::Registry as InferenceRegistry;
use omp_proto::thread::v1::Item;
use omp_sdk::{SessionHandle, SessionIdentity, SessionRuntime};
use omp_storage::transcript::{
	ModelChange as JournalModelChange, ModelId as JournalModelId, ModelRef as JournalModelRef,
	ProviderId as JournalProviderId,
};

use self::finalize::{FinalizerBudget, FinalizerReport, HeadlessFinalizerHandle};
/// Typed failure while composing or mutating a headless session.
#[derive(Debug, thiserror::Error)]
pub enum HeadlessError {
	/// A typed authority used by headless composition failed.
	#[error("headless session composition failed")]
	Composition(#[source] Box<dyn error::Error + Send + Sync + 'static>),
	/// The requested model selector was not present in the embedded catalog.
	#[error("unknown model `{0}`")]
	UnknownModel(Str),
	/// The selected model had no usable route.
	#[error("model `{0}` has no selectable route")]
	MissingRoute(Str),
}

fn composition(error: impl error::Error + Send + Sync + 'static) -> HeadlessError {
	HeadlessError::Composition(Box::new(error))
}

use std::mem;

use omp_envd::exthost::lifecycle::{HeadlessLifecycleSink, HeadlessLifecycleSubscription};
use omp_proto::inference::{v1, v1::response_format};
use tokio::io;

use crate::{
	bridges::{AgentGoalBinding, AgentGoalControl, InferenceBridge, builtin},
	chat::{self},
	discovery,
	modes::CampaignHandle,
	registry::{
		InferenceSessionOverrides, ProductionInference, production_inference_for_session,
		production_redemption_authority,
	},
	rulebook,
	settings::current,
};

/// Inputs required to create one production headless session.
#[derive(Clone, Debug)]
pub struct HeadlessSessionOptions {
	/// Project root whose Environment owns all effects.
	pub project:               PathBuf,
	/// Additional Environment-authorized workspace roots.
	pub additional_roots:      Box<[PathBuf]>,
	/// Resolved catalog model selector.
	pub model:                 Str,
	/// Built-in regime engaged before the agent moves into its runtime actor.
	pub initial_campaign:      Option<&'static str>,
	/// Optional prompt-slot override for the initial campaign.
	pub initial_prompt_slot:   Option<&'static str>,
	/// One-shot model selection applied when the plan campaign exits.
	pub plan_handoff:          Option<crate::plan::ModelSelection>,
	/// Existing durable session to resume, or a fresh journal when absent.
	pub resume:                Option<Str>,
	/// Existing durable session whose live projection is copied into a fork.
	pub fork:                  Option<Str>,
	/// Whether the Python eval device is enabled.
	pub py_eval:               bool,
	/// Invocation-only tool approval mode that overrides persisted settings.
	pub approval_mode:         Option<omp_envd::tool_settings::ApprovalMode>,
	/// Whether authenticated tool invocations are forbidden from allocating
	/// PTYs.
	pub pty_denied:            bool,
	/// Provider pinned by an invocation API-key lease.
	pub credential_provider:   Option<ProviderId>,
	/// Generic invocation key held only by the inference broker overlay.
	pub api_key:               Option<SecretString>,
	/// Opaque prompt-cache identity lowered by compatible codecs.
	pub prompt_cache_affinity: Option<Str>,
	/// Session-incarnation fence stamped onto observable events.
	pub session_generation:    u64,
}

/// Single owner of every authority needed by a non-interactive agent loop.
///
/// Field order is deliberate: the Agent and its cloned Environment client are
/// dropped before the project Environment authority.
pub struct HeadlessSession {
	session:             SessionHandle,
	advisor_parent:      Arc<chat::ChatParentHost<InProcTurnClient>>,
	advise_queue:        omp_agent::advisor::AdvisorAdviceQueue,
	state:               AgentState,
	control:             omp_agent::ControlSender,
	env:                 omp_env::EnvClient,
	campaigns:           Arc<CampaignHandle>,
	tree:                Arc<AgentTree>,
	events:              Option<EventSubscription>,
	lifecycle:           HeadlessLifecycleSink,
	lifecycle_events:    Option<HeadlessLifecycleSubscription>,
	approval_book:       Arc<ApprovalBook>,
	approval_route:      ApprovalRoute,
	approval_inbox:      Option<ApprovalInbox>,
	finalizer:           HeadlessFinalizerHandle,
	_goal_binding:       AgentGoalBinding,
	session_id:          Str,
	initial_items:       Vec<Item>,
	_inference_registry: InferenceRegistry,
	_environment:        omp_envd::ProjectEnvironment,
}

impl HeadlessSession {
	/// Constructs the production Environment, v4 journal, agent loop, tree,
	/// extension sink, approval route, and lossless event subscription.
	pub async fn open(
		data_dir: PathBuf,
		options: HeadlessSessionOptions,
	) -> Result<Self, HeadlessError> {
		Self::open_inner(data_dir, options, None).await
	}

	/// Constructs a production session over an exact command-owned tool
	/// registry while retaining the normal Environment and inference owners.
	pub(crate) async fn open_with_registry(
		data_dir: PathBuf,
		options: HeadlessSessionOptions,
		registry: Arc<omp_tool::Registry>,
	) -> Result<Self, HeadlessError> {
		Self::open_inner(data_dir, options, Some(registry)).await
	}

	async fn open_inner(
		data_dir: PathBuf,
		options: HeadlessSessionOptions,
		registry_override: Option<Arc<omp_tool::Registry>>,
	) -> Result<Self, HeadlessError> {
		let root = chat::canonical_project(&options.project).map_err(composition)?;
		let catalog = snapshot::Catalog::try_embedded().map_err(composition)?;
		let model =
			chat::resolve_model_selector(catalog, options.model.as_str()).map_err(composition)?;
		let settings = current(&data_dir).map_err(composition)?;
		let state_dir = omp_env::project_state::directory(&data_dir, &root).map_err(composition)?;
		let sessions_dir = state_dir.join("sessions");
		chat::ensure_state_directory(&state_dir).map_err(composition)?;
		chat::ensure_state_directory(&sessions_dir).map_err(composition)?;
		let search = Arc::new(InferenceBridge::default());
		let goal_control = AgentGoalControl::default();
		let advise_queue = omp_agent::advisor::AdvisorAdviceQueue::default();
		let bridges =
			builtin(&root, Arc::clone(&search), goal_control.clone(), None, advise_queue.clone());
		let environment = omp_envd::ProjectEnvironment::connect_or_start(
			&root,
			&state_dir,
			&omp_env::project_state::environment_socket(&state_dir),
			&omp_env::project_state::document_socket(&state_dir),
			options.py_eval,
			options.approval_mode,
			&[],
			settings.runtime_durations().interrupt_grace,
			bridges,
		)
		.await
		.map_err(composition)?;
		let grant = omp_env::InvocationGrant::unrestricted();
		let grant = if options.pty_denied {
			grant.deny_pty()
		} else {
			grant
		};
		let env = environment.client().with_invocation_grant(grant);
		let registry = registry_override.unwrap_or_else(|| environment.registry());
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
		.map_err(composition)?;
		let blueprint = chat::session_blueprint(
			model.as_str(),
			catalog,
			&root,
			&options.additional_roots,
			&session.id,
			Arc::clone(&registry),
		)
		.map_err(composition)?;
		let mut snapshot = chat::agent_snapshot(&blueprint, catalog, None).map_err(composition)?;
		if options.resume.is_some() || options.fork.is_some() {
			let journal_path = sessions_dir.join(format!("{}.jsonl", session.id.as_str()));
			let revived = omp_agent::revive_existing(&journal_path, session.journal, snapshot)
				.map_err(composition)?;
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
		let ProductionInference {
			registry: inference_registry,
			rpc: inference,
			credential_authority,
			mcp_authority,
			mcp_oauth,
			..
		} = production_inference_for_session(
			&data_dir,
			Arc::clone(&registry),
			Some(&root),
			InferenceSessionOverrides {
				provider:              options.credential_provider,
				api_key:               options.api_key,
				prompt_cache_affinity: options.prompt_cache_affinity,
			},
		)
		.await
		.map_err(composition)?;
		let _ = search.bind(inference.clone());
		let _ = environment.github_credentials().bind(credential_authority);
		environment.bind_mcp_oauth(mcp_authority, mcp_oauth);
		let client = InProcTurnClient::new(inference)
			.await
			.map_err(composition)?;
		let tree = Arc::new(AgentTree::standard(8));
		let advisor_parent = Arc::new(chat::ChatParentHost::new_with_tree(
			client.clone(),
			env.clone(),
			state.clone(),
			session.id.clone(),
			sessions_dir.clone(),
			root.clone(),
			environment.sessions_index(),
			settings.security.enabled,
			Arc::clone(&tree),
		));
		let journal_path = sessions_dir.join(format!("{}.jsonl", session.id.as_str()));
		let content = discovery::active_content_snapshots(&root);
		let (ttsr, ttsr_diagnostics) = rulebook::ttsr_registry(content.rules.as_ref());
		for error in ttsr_diagnostics {
			tracing::warn!(%error, "headless TTSR rule condition was rejected");
		}
		let mut agent =
			Agent::new(client, env.clone(), state.clone(), session.journal, chat::CHAT_CAPS_BASE);
		agent.set_autolearn(autolearn);
		blueprint.configure_agent(&mut agent);
		match production_redemption_authority(&state_dir) {
			Ok(Some(authority)) => agent.set_redemption_authority(authority),
			Ok(None) => {},
			Err(error) => {
				tracing::warn!(%error, "codex redemption authority was not constructed");
			},
		}
		agent.set_ttsr_registry(ttsr);
		agent
			.events()
			.set_session_generation(options.session_generation);
		let control = agent.control();
		agent
			.recover_campaigns(omp_agent::core_regime, now_ms())
			.map_err(composition)?;
		if let Some(spec_id) = options.initial_campaign
			&& agent
				.arbiter()
				.campaigns()
				.slots()
				.owner(&omp_agent::SlotClaim::Mode)
				.is_none()
		{
			let (spec, machine) =
				omp_agent::core_regime(spec_id).expect("headless startup names a core regime");
			let mut spec = spec;
			if let Some(prompt_slot) = options.initial_prompt_slot {
				Arc::make_mut(&mut spec).binds = Arc::from([omp_agent::ScopedBinding {
					slot:  omp_agent::BindSlot::PromptSlot,
					value: Str::new_static(prompt_slot),
				}]);
			}
			let _ = agent
				.engage_campaign(spec, machine, omp_agent::EngageOptions {
					now_ms: now_ms(),
					queue:  false,
				})
				.map_err(composition)?;
		}
		let modes = Arc::new(CampaignHandle::new());
		let goal_binding = goal_control.bind(Arc::clone(&modes), control.clone());
		modes.sync_campaigns(agent.arbiter().campaigns());
		modes.bind_plan_selection(state.clone(), None);
		if let Some(handoff) = options.plan_handoff.clone() {
			modes.bind_plan_handoff(handoff);
		}
		state.update(|snapshot| {
			snapshot.prompt_source = modes.prompt_source(Arc::clone(&snapshot.prompt_source));
		});
		agent.set_continuation_source(modes.clone());
		let node = tree
			.register(
				session.id.clone(),
				sf!("Main"),
				AgentKind::Main,
				None,
				session.id.clone(),
				Budget::default(),
			)
			.map_err(composition)?;
		node.set_status(AgentStatus::Running);
		let session_handle = blueprint
			.launch(
				SessionIdentity { id: session.id.clone(), journal_path, expected_revision: None },
				SessionRuntime::from_agent(agent),
				None,
			)
			.map_err(composition)?;
		let events = session_handle.subscribe_lossless();
		let (lifecycle, lifecycle_events) = HeadlessLifecycleSink::new(options.session_generation);
		let approval_book = Arc::new(ApprovalBook::new());
		let (approval_route, approval_inbox) = ApprovalRoute::new(Arc::clone(&approval_book));
		environment
			.bind_approval_authority(Some(Arc::clone(&approval_book)), Some(approval_route.clone()));
		Ok(Self {
			session: session_handle,
			advisor_parent,
			advise_queue,
			state,
			control,
			env,
			campaigns: modes,
			tree,
			events: Some(events),
			lifecycle,
			lifecycle_events: Some(lifecycle_events),
			approval_book,
			approval_route,
			approval_inbox: Some(approval_inbox),
			finalizer: HeadlessFinalizerHandle::new(),
			_goal_binding: goal_binding,
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

	/// Rewinds and resubmits the latest durable user turn.
	pub async fn retry_last_turn(
		&self,
		turn_id: TurnId,
	) -> Result<Option<(Vec<Item>, Str, AgentRunSummary)>, omp_sdk::SessionHandleError> {
		self.session.retry_last_turn(turn_id).await
	}

	/// Executes and durably commits one manual compaction.
	pub async fn compact_manual(
		&self,
		request: omp_agent::ManualCompactionRequest,
	) -> Result<omp_agent::ManualCompactionOutcome, omp_sdk::SessionHandleError> {
		self.session.compact_manual(request).await
	}

	/// Returns the durable session identifier.
	pub fn session_id(&self) -> &str {
		self.session_id.as_str()
	}

	/// Returns the session-local parent authority used by persistent advisor
	/// children.
	pub fn advisor_parent(&self) -> Arc<chat::ChatParentHost<InProcTurnClient>> {
		Arc::clone(&self.advisor_parent)
	}

	/// Clone-shared session queue backing the environment's `advise@1` device.
	pub fn advise_queue(&self) -> omp_agent::advisor::AdvisorAdviceQueue {
		self.advise_queue.clone()
	}

	/// Lists model-callable environment tools available to advisor grant
	/// evaluation.
	pub fn available_tool_names(&self) -> Vec<Str> {
		self
			._environment
			.registry()
			.devices()
			.map(|device| device.name.clone())
			.collect()
	}

	/// Returns the canonical replay projection loaded before the first turn.
	pub fn initial_items(&self) -> &[Item] {
		&self.initial_items
	}

	/// Returns the Environment client owned alongside the agent.
	pub const fn env(&self) -> &omp_env::EnvClient {
		&self.env
	}

	/// Binds or clears the session-scoped ACP terminal execution capability.
	pub fn bind_acp_exec(&self, backend: Option<Arc<dyn omp_envd::tool_shell::AcpExecBackend>>) {
		self._environment.bind_acp_exec(backend);
	}

	/// Binds or clears the session-scoped ACP document capability.
	pub fn bind_acp_documents(&self, backend: Option<Arc<dyn omp_envd::docs::AcpDocumentBackend>>) {
		self._environment.bind_acp_documents(backend);
	}

	/// Replaces the session environment's ask presentation bridge.
	pub fn bind_ask_presenter(&self, presenter: Arc<dyn omp_tools::ask::AskPresenter>) {
		self._environment.bind_ask_presenter(presenter);
	}

	/// Binds or clears the durable approval authority.
	pub fn bind_approval_authority(
		&self,
		book: Option<Arc<ApprovalBook>>,
		route: Option<ApprovalRoute>,
	) {
		self._environment.bind_approval_authority(book, route);
	}

	/// Returns the current session-effective model selector.
	pub fn model(&self) -> Str {
		Str::new(self.state.snapshot().turn.params.model.as_str())
	}

	/// Applies a validated session-only model override and records it in the
	/// owning v4 journal before changing the live snapshot.
	pub async fn set_model(&self, selector: &str) -> Result<(), HeadlessError> {
		let catalog = snapshot::Catalog::try_embedded().map_err(composition)?;
		let model = chat::resolve_model_selector(catalog, selector).map_err(composition)?;
		let spec = catalog
			.model(ModelKey::from_ref(model.as_str()))
			.ok_or_else(|| HeadlessError::UnknownModel(Str::new(selector)))?;
		let route = spec
			.routes
			.first()
			.and_then(|route| catalog.route(route))
			.ok_or_else(|| HeadlessError::MissingRoute(Str::new(selector)))?;
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
			.map_err(composition)?;
		self
			.state
			.update(|snapshot| snapshot.turn.params.model = model.to_string());
		Ok(())
	}

	/// Replaces the session-only provider reasoning request after the ACP host
	/// has clamped it through the selected model policy.
	pub fn set_thinking(&self, thinking: Option<v1::Reasoning>) {
		self
			.state
			.update(|snapshot| snapshot.turn.params.thinking = thinking);
	}

	/// Installs a strict command-owned JSON response schema for later turns.
	pub(crate) fn set_response_schema(
		&self,
		name: &'static str,
		schema: serde_json::Value,
	) -> Result<(), serde_json::Error> {
		let schema_json = serde_json::to_vec(&schema)?;
		self.state.update(|snapshot| {
			snapshot.turn.params.response_format = Some(v1::ResponseFormat {
				kind:           Some(response_format::Kind::JsonSchema(response_format::JsonSchema {
					name:        name.to_owned(),
					schema_json: schema_json.into(),
					strict:      Some(true),
				})),
				on_unsupported: v1::Fallback::Error as i32,
			});
		});
		Ok(())
	}

	/// Interrupts the active caller submission without waiting for settlement.
	pub fn interrupt(&self) {
		self.session.interrupt();
	}

	/// Returns a cheap interrupt-only capable clone of the durable handle.
	///
	/// Protocol hosts use this before borrowing the session mutably for a
	/// submission so cancellation never contends on their session mutex.
	pub fn interrupt_handle(&self) -> SessionHandle {
		self.session.clone()
	}

	/// Records a user-visible session title through the sole journal owner.
	pub async fn set_title(&self, title: Str) -> Result<(), HeadlessError> {
		self
			.control
			.set_title(now_ms(), title)
			.await
			.map_err(composition)?;
		Ok(())
	}

	/// Returns the session-scoped campaign projection.
	pub fn campaigns(&self) -> &CampaignHandle {
		self.campaigns.as_ref()
	}

	/// Engages a built-in regime on the actor-owned campaign stack.
	pub async fn engage_regime(
		&self,
		spec_id: &'static str,
		queue: bool,
	) -> Result<omp_agent::EngageReceipt, omp_sdk::SessionHandleError> {
		let (spec, machine) =
			omp_agent::core_regime(spec_id).expect("headless command names a built-in regime");
		let (receipt, entries) = self
			.session
			.engage_campaign(spec, machine, omp_agent::EngageOptions { now_ms: now_ms(), queue })
			.await?;
		self.campaigns.sync_entries(&entries);
		Ok(receipt)
	}

	/// Disengages an active campaign on the actor-owned stack.
	pub async fn disengage_campaign(
		&self,
		engagement: Str,
	) -> Result<bool, omp_sdk::SessionHandleError> {
		let (removed, entries) = self
			.session
			.disengage_campaign(engagement, now_ms())
			.await?;
		self.campaigns.sync_entries(&entries);
		Ok(removed)
	}

	/// Returns the append-only agent roster.
	pub fn tree(&self) -> &Arc<AgentTree> {
		&self.tree
	}

	/// Takes the single ordered lossless agent-event subscription.
	pub fn take_events(&mut self) -> Option<EventSubscription> {
		self.events.take()
	}

	/// Returns the generation-fenced extension lifecycle sink.
	pub const fn lifecycle_sink(&self) -> &HeadlessLifecycleSink {
		&self.lifecycle
	}

	/// Takes the single lossless extension lifecycle subscription.
	pub fn take_lifecycle_events(&mut self) -> Option<HeadlessLifecycleSubscription> {
		self.lifecycle_events.take()
	}

	/// Returns the durable approval book.
	pub fn approval_book(&self) -> &Arc<ApprovalBook> {
		&self.approval_book
	}

	/// Returns the awaitable approval route.
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

	/// Disposes the live session without running mode-specific finalizers.
	pub(crate) async fn dispose(&mut self) {
		let _ = self.session.dispose().await;
	}

	/// Runs ordered bounded finalization. Dropping this session afterward
	/// disposes the agent and Environment last.
	pub async fn finalize<W>(&mut self, stdout: &mut W, budget: FinalizerBudget) -> FinalizerReport
	where
		W: io::AsyncWrite + Unpin,
	{
		let report = mem::take(&mut self.finalizer)
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
