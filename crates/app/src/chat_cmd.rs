//! Interactive terminal and GUI host for durable project chat.

use std::{collections::BTreeMap, env, fs, iter, path::PathBuf, sync, sync::Arc, time::Duration};

use miette::IntoDiagnostic as _;
#[cfg(unix)]
use nix::sys::signal;
#[cfg(unix)]
use nix::unistd::Pid;
use omp_agent::{
	Agent, AgentKind, AgentState, AgentStatus, Budget, InProcTurnClient, RpcTurnClient, TurnClient,
};
use omp_catalog::snapshot;
use omp_chat_ui::host;
use omp_collab::guest::GuestRelayPump;
use omp_core::{Str, sf};
use omp_driver::{
	autolearn::AutolearnCampaign,
	bridges::{AgentGoalControl, InferenceBridge},
	chat::{
		AgentCampaignControlBackend, AgentsControlAuthority, CHAT_CAPS_BASE,
		CampaignControlAuthorityFactory, ChatAuthWorker, ChatError as DriverChatError,
		ChatParentHost, ChatProviderControlBackend, ChatScope, EphemeralSessions,
		LaunchToolSelection, Session, SessionOpen, agent_snapshot, apply_launch_tool_selection,
		canonical_project, ensure_state_directory, fallback_model_selector,
		interrupted_reasoning_dialect, model_context_window, model_selector_is_selectable, now_ms,
		open_session, resolve_model_provider, resolve_model_selector, resume_choices,
		session_blueprint, strict_session_id, thinking_effort,
	},
	collab::session::{self, CollabSessionAuthority},
	discovery::{
		context::{self, ContextDiscoveryOptions, GrantedContextRoot},
		roles, runtime,
	},
	hub as hub_backend,
	memory::RuntimePromptMemorySource,
	model_controls::{ProductionProviderApplicationOwner, ProviderControlAuthorityFactory},
	modes::CampaignHandle,
	plan::ModelSelection,
	power::PowerActivity,
	prompt_head::ProductionPromptHead,
	prompt_prep::{PromptSnapshot, settings::PromptSettings},
	rulebook::PromptControlOwner,
	secrets::session::{SecretSessionError, SecretSessionSnapshot},
	session_state::TerminalBreadcrumbs,
	session_title::SessionTitleState,
	stats_api::{
		telemetry_backend::TelemetryIndexQuery,
		verdict_authority::{
			AgentDurableJobRegistrar, ControlPromptProjectionDispatcher, VerdictAuthority,
			VerdictAuthorityIdentity,
		},
	},
	task::prompt_policy,
};
use omp_envd::exthost::{
	TelemetryControlAuthority, UiControlAuthority, VerdictControlAuthority,
	backends::EnvdHostOwnerBackends,
	control::{
		ControlAuthority, ControlAuthorityFactory, ControlConnectionIdentity, ControlProtocolError,
	},
	dispatch::CallbackDispatcher,
};
use omp_inference::{Registry as InferenceRegistry, layer::stack::BuiltinConfig};
use omp_proto::{
	inference::v1 as inference_pb,
	thread::v1::{item, part},
};
use omp_sdk::SessionBlueprint;
use omp_settings::manager::{SettingsManager, SettingsManagerError, SettingsPaths};
use omp_storage::{
	blob, blob::BlobStore, gc, gc::ArtifactCatalog, index, index::SessionIndex,
	telemetry_index::TelemetryIndex, transcript::SessionId,
};
use omp_tools::eval::EvalSessionControl;
use parking_lot::Mutex;
use tokio::time;
use tonic::transport;

use crate::{
	chat_ui::{
		self, ChatUiSession,
		presentation::{ControlPresentationCallbackDispatcher, PresentationBridge},
		presentation_authority::{PresentationAuthority, PresentationIdentity},
	},
	cli::ChatArgs,
	pickers::{pick_session, run_list},
	session_manager::{DraftError, DraftStore},
	wizard,
};

/// Complete app-owned CONTROL factory bundle for one production chat session.
///
/// Keeping every field required makes it impossible for a session entry point
/// to accidentally activate an extension host with a silently absent domain.
#[doc(hidden)]
pub struct SessionControlFactories {
	/// Policy mutation and approval decisions.
	pub policy:            Arc<dyn ControlAuthorityFactory>,
	/// Invocation parameter cursors.
	pub parameters:        Arc<dyn ControlAuthorityFactory>,
	/// Named worker placement and process ownership.
	pub workers:           Arc<dyn ControlAuthorityFactory>,
	/// Audited trusted direct-filesystem operations.
	pub direct_filesystem: Arc<dyn ControlAuthorityFactory>,
	/// Credential and secret resolution.
	pub credentials:       Arc<dyn ControlAuthorityFactory>,
	/// Typed prompt-head invalidation.
	pub prompts:           Arc<dyn ControlAuthorityFactory>,
	/// Interactive presentation composition.
	pub ui:                Arc<dyn ControlAuthorityFactory>,
	/// Durable telemetry query and export.
	pub telemetry:         Arc<dyn ControlAuthorityFactory>,
	/// Verdict projection and durable job registration.
	pub verdicts:          Arc<dyn ControlAuthorityFactory>,
	/// Inference provider declaration and request ownership.
	pub provider:          Arc<dyn ControlAuthorityFactory>,
	/// Session and turn campaign ownership.
	pub campaigns:         Arc<dyn ControlAuthorityFactory>,
}

impl SessionControlFactories {
	/// Atomically replaces agents and every app-owned domain under one lease.
	#[must_use]
	pub fn bind(
		self,
		environment: &omp_envd::ProjectEnvironment,
		agents: Arc<dyn ControlAuthorityFactory>,
	) -> omp_envd::exthost::ExternalControlAuthorityBinding {
		environment.bind_external_control_authorities(
			agents,
			omp_envd::exthost::ExternalDomainControlFactories {
				policy:            Some(self.policy),
				parameters:        Some(self.parameters),
				workers:           Some(self.workers),
				direct_filesystem: Some(self.direct_filesystem),
				credentials:       Some(self.credentials),
				prompts:           Some(self.prompts),
				ui:                Some(self.ui),
				telemetry:         Some(self.telemetry),
				verdicts:          Some(self.verdicts),
				provider:          Some(self.provider),
				campaigns:         Some(self.campaigns),
				services:          None,
			},
		)
	}
}

fn presentation_control_factory(
	executor: omp_executor::Executor,
	bridge: Arc<PresentationBridge>,
	dispatcher: Arc<dyn CallbackDispatcher>,
) -> Arc<dyn ControlAuthorityFactory> {
	Arc::new(move |identity: Arc<ControlConnectionIdentity>| {
		let presentation_identity = Arc::new(PresentationIdentity {
			principal:          Str::new(identity.principal.id()),
			extension:          identity.extension.clone(),
			artifact_digest:    identity.artifact_digest.clone(),
			host_generation:    identity.host_generation,
			session_generation: identity.session_generation,
			capabilities:       identity.capabilities.clone(),
		});
		let callbacks = Arc::new(ControlPresentationCallbackDispatcher::new(
			Arc::clone(&identity),
			Arc::clone(&dispatcher),
		));
		let owner = Arc::new(PresentationAuthority::new(
			executor.clone(),
			presentation_identity,
			bridge.clone(),
			callbacks,
		));
		Ok(Arc::new(UiControlAuthority::new(identity, owner)) as Arc<dyn ControlAuthority>)
	})
}

fn telemetry_control_factory(
	query: Arc<dyn omp_telemetry::authority::DurableTelemetryQuery>,
) -> Arc<dyn ControlAuthorityFactory> {
	Arc::new(move |identity: Arc<ControlConnectionIdentity>| {
		Ok(Arc::new(TelemetryControlAuthority::new(identity, now_ms(), Arc::clone(&query)))
			as Arc<dyn ControlAuthority>)
	})
}

fn prompt_control_factory(
	head: Arc<dyn omp_driver::rulebook::PromptHeadAuthority>,
) -> Arc<dyn ControlAuthorityFactory> {
	Arc::new(move |identity: Arc<ControlConnectionIdentity>| {
		Ok(Arc::new(PromptControlOwner::new(identity, Arc::clone(&head)))
			as Arc<dyn ControlAuthority>)
	})
}

fn verdict_control_factory(
	session: Str,
	jobs: omp_agent::JobBoard,
	control: omp_agent::AgentHostControl,
	dispatcher: Arc<dyn CallbackDispatcher>,
) -> Arc<dyn ControlAuthorityFactory> {
	Arc::new(move |identity: Arc<ControlConnectionIdentity>| {
		let verdict_identity = Arc::new(VerdictAuthorityIdentity {
			principal:          Str::new(identity.principal.id()),
			extension:          identity.extension.clone(),
			artifact_digest:    identity.artifact_digest.clone(),
			host_generation:    identity.host_generation,
			session_generation: identity.session_generation,
			session:            session.clone(),
			capabilities:       identity.capabilities.clone(),
		});
		let registrar = Arc::new(AgentDurableJobRegistrar::new(control.clone()));
		let projection = Arc::new(ControlPromptProjectionDispatcher::new(
			Arc::clone(&identity),
			Arc::clone(&dispatcher),
		));
		let owner =
			Arc::new(VerdictAuthority::new(verdict_identity, jobs.clone(), registrar, projection));
		Ok(Arc::new(VerdictControlAuthority::new(identity, owner)) as Arc<dyn ControlAuthority>)
	})
}

fn provider_control_factory(
	registry: omp_inference::Registry,
	builtins: BuiltinConfig,
	blobs: BlobStore,
) -> Arc<dyn ControlAuthorityFactory> {
	let owner = Arc::new(ProductionProviderApplicationOwner::new(registry, builtins, blobs));
	let backend = Arc::new(ChatProviderControlBackend::new(owner));
	Arc::new(ProviderControlAuthorityFactory::new(backend))
}

struct SessionCampaignResolver(Arc<omp_envd::worker::ExtensionCampaignResolver>);

impl omp_driver::chat::CampaignControlResolver for SessionCampaignResolver {
	fn resolve(
		&self,
		identity: &ControlConnectionIdentity,
		campaign: &str,
		state: Option<&str>,
	) -> Result<
		(Arc<omp_agent::CampaignSpec>, Box<dyn omp_agent::CampaignMachine>),
		ControlProtocolError,
	> {
		self.0.resolve(identity, campaign, state)
	}

	fn owner(&self, campaign: &str) -> Option<Str> {
		self.0.owner(campaign)
	}
}

fn campaign_control_factory(
	control: omp_agent::ControlSender,
	resolver: Arc<omp_envd::worker::ExtensionCampaignResolver>,
) -> Arc<dyn ControlAuthorityFactory> {
	let backend = Arc::new(AgentCampaignControlBackend::new(
		control,
		Arc::new(SessionCampaignResolver(resolver)),
	));
	Arc::new(CampaignControlAuthorityFactory::new(backend))
}

fn replace_model_props(mut props: omp_scribe::Props, model: &str) -> omp_scribe::Props {
	let mut fields = match props.get(omp_agent::prompt_keys::MODEL) {
		Some(omp_scribe::Value::Map(fields)) => fields.clone(),
		_ => iter::empty::<(Str, omp_scribe::Value)>().collect(),
	};
	fields.insert(Str::new_static("identifier"), omp_scribe::Value::from(Str::new(model)));
	fields.insert(
		Str::new_static("codex_task_policy"),
		omp_scribe::Value::from(prompt_policy::uses_codex_task_prompt(model)),
	);
	props.set(omp_agent::prompt_keys::MODEL, omp_scribe::Value::Map(fields));
	props
}
/// Failures owned by the interactive presentation boundary.
#[derive(Debug, thiserror::Error)]
enum ChatError {
	/// Live agent composition failed.
	#[error(transparent)]
	Agent(#[from] omp_agent::Error),
	/// Driver-owned durable session composition failed.
	#[error(transparent)]
	Driver(#[from] DriverChatError),
	/// Owner-local draft persistence failed.
	#[error(transparent)]
	Draft(#[from] DraftError),
	/// Session-local secret transformation failed.
	#[error(transparent)]
	Secrets(#[from] SecretSessionError),
	/// Typed settings projection failed.
	#[error(transparent)]
	Settings(#[from] SettingsManagerError),
	/// Owner-local session discovery failed.
	#[error(transparent)]
	SessionResolve(#[from] omp_driver::session_state::SessionResolveError),
	/// Process-global parked session discovery failed.
	#[error(transparent)]
	AgentRegistry(#[from] omp_agent::RegistryError),
	/// Session artifact metadata failed.
	#[error(transparent)]
	Artifact(#[from] gc::Error),
	/// Session blob storage failed.
	#[error(transparent)]
	Blob(#[from] blob::Error),
	/// Durable session telemetry index failed.
	#[error(transparent)]
	Telemetry(#[from] omp_storage::telemetry_index::QueryError),
	/// Campaign lifecycle mutation failed.
	#[error(transparent)]
	Campaign(#[from] omp_agent::AgentError),
	/// Environment authority binding failed.
	#[error(transparent)]
	Environment(#[from] omp_envd::EnvdError),
	/// A production session entry omitted a required CONTROL owner.
	#[error("required production CONTROL authority `{0}` was not composed")]
	MissingAuthority(&'static str),
	/// Session index mutation failed.
	#[error(transparent)]
	SessionIndex(#[from] index::Error),
	/// Interactive terminal or GUI host failed.
	#[error("interactive chat shell failed: {0}")]
	Ui(miette::Report),
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
pub enum ChatPresentation {
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
	executor: omp_executor::Executor,
	args: ChatArgs,
	mut start: ChatStart,
	presentation: ChatPresentation,
) -> miette::Result<()> {
	use miette::{Context as _, IntoDiagnostic as _};
	let launch_root = canonical_project(&args.project).map_err(|e| miette::miette!(e))?;
	let data_dir = omp_core::dirs::data_dir(None).into_diagnostic()?;
	let mut root = launch_root.clone();
	let mut selected_sessions_dir = None;
	let mut selected_index_path = None;
	let mut picked_resume = None;
	let mut resume_moved = false;
	if start == ChatStart::SessionIndex {
		let Some(selection) = pick_session(&data_dir, args.session_dir.as_deref())
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
			if run_list("Project missing", &choices)
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
	let catalog = snapshot::Catalog::try_embedded().map_err(|e| miette::miette!(e))?;
	let settings =
		omp_driver::settings::current_with_overlays(&data_dir, &args.config).into_diagnostic()?;
	let security_enabled = settings.security.enabled;
	let roles = roles::resolve_launch_roles(
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
		fs::canonicalize(root).into_diagnostic().wrap_err_with(|| {
			format!("additional workspace root `{}` is unavailable", root.display())
		})?;
	}
	let plan_selection = roles
		.plan
		.as_ref()
		.map(|model| ModelSelection::resolved(model.as_str(), roles.plan_thinking.as_deref()))
		.transpose()
		.map_err(|error| miette::miette!(error))?;
	let interrupt_grace = settings.runtime_durations().interrupt_grace;
	let auto_thinking = settings.auto_thinking;
	let power_mode = omp_driver::power::configured(&data_dir).into_diagnostic()?;
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
		None => wizard::run(&data_dir, catalog)
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
		omp_env::project_state::directory(&data_dir, &root).map_err(|e| miette::miette!(e))?;
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
		fs::canonicalize(configured).into_diagnostic()?
	} else {
		state_dir.join("sessions")
	};
	ensure_state_directory(&sessions_dir).map_err(|e| miette::miette!(e))?;
	let requested_resume = picked_resume.or_else(|| args.resume.clone());
	let env_socket = omp_env::project_state::environment_socket(&state_dir);
	let document_socket = omp_env::project_state::document_socket(&state_dir);
	let search_bridge = Arc::new(InferenceBridge::default());
	let goal_control = AgentGoalControl::default();
	let bridges =
		omp_driver::bridges::builtin(&root, Arc::clone(&search_bridge), goal_control.clone(), None);
	let bridges =
		omp_envd::RegistryBridges { ask_presenter: Some(omp_chat_ui::ask::presenter()), ..bridges };
	let prompt_head = Arc::new(ProductionPromptHead::from_extension_specs(&args.trusted_extensions));
	let environment = omp_envd::ProjectEnvironment::connect_or_start(
		&root,
		&state_dir,
		&env_socket,
		&document_socket,
		args.py_eval,
		&args.trusted_extensions,
		interrupt_grace,
		bridges,
	)
	.await
	.map_err(|e| miette::miette!(e))?;
	let credential_control_grants =
		omp_driver::secrets::credential_control_grants(&args.trusted_extensions);
	let session_index = if let Some(database) = selected_index_path {
		Arc::new(
			SessionIndex::open(database)
				.map_err(|error| miette::miette!(DriverChatError::SessionIndex(error)))?,
		)
	} else if args.session_dir.is_some() && !args.no_session {
		Arc::new(
			SessionIndex::open(sessions_dir.join("sessions.sqlite3"))
				.map_err(|error| miette::miette!(DriverChatError::SessionIndex(error)))?,
		)
	} else {
		environment.sessions_index()
	};
	let breadcrumbs = TerminalBreadcrumbs::new(&data_dir).map_err(|error| miette::miette!(error))?;
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
				omp_driver::session_state::resolve_session_selector(&page.sessions, resume.as_str())
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
				omp_driver::session_state::resolve_session_selector(&page.sessions, selector)
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
				omp_driver::session_state::resolve_session_selector(&page.sessions, selector.as_str())
					.map_err(|error| miette::miette!(error))?
					.0,
			)
		}
	} else {
		None
	};
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
	let mut prompt_facts = blueprint.prompt_facts().clone();
	let home = env::var_os("HOME").map_or_else(|| root.clone(), PathBuf::from);
	let prompt_settings = PromptSettings::default()
		.with_cli(&args.prompt_settings)
		.resolve_inputs(&root, &home)
		.map_err(|error| miette::miette!(error))?;
	prompt_facts.settings = prompt_settings.into();
	prompt_facts.model = omp_agent::ModelPromptInput {
		identifier:        model.clone(),
		codex_task_policy: prompt_policy::uses_codex_task_prompt(model.as_str()),
	};
	if let Some(level) = args.thinking {
		let effort = thinking_effort(level.into(), auto_thinking);
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
	snapshot.reasoning_dialect = interrupted_reasoning_dialect(catalog, &snapshot.turn.params.model);
	prompt_facts.model.identifier = Str::new(&snapshot.turn.params.model);
	prompt_facts.model.codex_task_policy =
		prompt_policy::uses_codex_task_prompt(&snapshot.turn.params.model);
	let invocation_grant = apply_launch_tool_selection(
		&mut snapshot,
		LaunchToolSelection {
			tools:    args.tools.as_ref().map(|tools| tools.0.as_slice()),
			no_tools: args.no_tools,
			no_lsp:   args.no_lsp,
			no_pty:   args.no_pty,
		},
		registry.as_ref(),
	)
	.map_err(|error| miette::miette!(error))?;
	let env = environment.client().with_invocation_grant(invocation_grant);
	let settings_manager = SettingsManager::open(SettingsPaths::discover(&data_dir, Some(&root)))
		.map_err(|error| miette::miette!(error))?;
	let configured_autolearn = settings_manager
		.snapshot()
		.project::<omp_driver::settings::Settings>()
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
	let active_content = omp_driver::discovery::active_content_snapshots(&root);
	for warning in active_content.warnings.iter() {
		eprintln!("Extension load warning: {warning}");
	}
	let prompt_rules = if args.no_rules {
		Arc::from([])
	} else {
		omp_driver::rulebook::prompt_inputs(&active_content.rules)
	};
	let prompt_skills = if args.no_skills {
		Arc::from([])
	} else {
		let discovered = omp_driver::skills::prompt_inputs(&active_content.skills);
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
	let context_roots = iter::once(&root)
		.chain(args.add_dir.iter())
		.map(|path| GrantedContextRoot { root: path.clone(), start: path.clone() })
		.collect::<Vec<_>>();
	let context = context::discover(&context_roots, &ContextDiscoveryOptions::default());
	prompt_facts.context_files = context::prompt_files(&context);
	let prepared_prompt = PromptSnapshot::freeze(
		prompt_facts,
		registry.as_ref(),
		Some(&snapshot.enabled_tools),
		Arc::from([]),
		Default::default(),
		Default::default(),
		Default::default(),
		prompt_rules,
		prompt_skills,
		Arc::from([]),
	);
	let mut prompt_facts = prepared_prompt.workspace;
	let prepared =
		omp_driver::prompt_prep::prepare_environment_inputs_bounded(&env, &session.journal, &root)
			.await;
	prompt_facts.host = prepared.host;
	prompt_facts.roots = prepared.roots;
	snapshot.props = prompt_facts
		.props()
		.map_err(|error| miette::miette!(error))?;
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
			executor.clone(),
			RpcTurnClient::new(channel.clone()),
			&environment,
			env,
			state,
			autolearn,
			session,
			blueprint,
			eval_control.clone(),
			None,
			goal_control.clone(),
			None,
			Some(channel.clone()),
			Some(runtime::gateway_provider_control_factory(channel.clone())),
			None,
			credential_control_grants,
			Arc::clone(&prompt_head),
			data_dir.clone(),
			state_dir.clone(),
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
		let omp_driver::registry::ProductionInference {
			registry: inference_registry,
			rpc: inference,
			credential_authority,
			auth_control,
			builtins,
			..
		} = omp_driver::registry::production_inference_for_session(
			&data_dir,
			Arc::clone(&registry),
			Some(&root),
			omp_driver::registry::InferenceSessionOverrides {
				provider:              credential_provider,
				api_key:               args.api_key.clone(),
				prompt_cache_affinity: args.prompt_cache_key.clone(),
			},
		)
		.await
		.into_diagnostic()?;
		search_bridge
			.bind(inference.clone())
			.map_err(|_| miette::miette!("workspace search inference is already bound"))?;
		environment
			.github_credentials()
			.bind(credential_authority)
			.map_err(|_| miette::miette!("GitHub credential authority is already bound"))?;
		let client = InProcTurnClient::new(inference)
			.await
			.map_err(ChatError::from)
			.into_diagnostic()?;
		Box::pin(run_ui(
			executor,
			client,
			&environment,
			env,
			state,
			autolearn,
			session,
			blueprint,
			eval_control,
			Some(inference_registry),
			goal_control,
			Some(auth_control),
			None,
			None,
			Some(builtins),
			credential_control_grants,
			prompt_head,
			data_dir,
			state_dir,
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
	_executor: omp_executor::Executor,
	_args: ChatArgs,
	_start: ChatStart,
	_presentation: ChatPresentation,
) -> miette::Result<()> {
	use miette::IntoDiagnostic as _;
	Err(DriverChatError::UnsupportedPlatform).into_diagnostic()
}

fn bind_goal_todo_context(events: omp_agent::EventSubscription, modes: sync::Weak<CampaignHandle>) {
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
async fn run_ui<C: TurnClient + Clone + Send + Sync + 'static>(
	executor: omp_executor::Executor,
	client: C,
	environment: &omp_envd::ProjectEnvironment,
	env: omp_env::EnvClient,
	mut state: AgentState,
	autolearn: omp_agent::AutolearnSettings,
	mut session: Session,
	mut blueprint: SessionBlueprint,
	eval_control: EvalSessionControl,
	auth_registry: Option<InferenceRegistry>,
	goal_control: AgentGoalControl,
	auth_control: Option<omp_inference::auth::AuthControlHandle>,
	gateway_channel: Option<transport::Channel>,
	gateway_provider_factory: Option<Arc<dyn ControlAuthorityFactory>>,
	provider_builtins: Option<BuiltinConfig>,
	credential_control_grants: BTreeMap<Str, omp_driver::auth_backend::CredentialControlGrant>,
	prompt_head: Arc<ProductionPromptHead>,
	data_dir: PathBuf,
	state_dir: PathBuf,
	power_mode: omp_driver::power::SleepPrevention,
	initial_campaign: Option<&'static str>,
	plan_selection: Option<ModelSelection>,
	security_enabled: bool,
	title_enabled: bool,
	scope: ChatScope<'_>,
	mut start: ChatStart,
	presentation: ChatPresentation,
) -> Result<(), ChatError> {
	let memory_source =
		Arc::new(RuntimePromptMemorySource::new(environment.memory_runtime(), usize::MAX));
	let memory_prompt = omp_driver::memory::prompt_snapshot(
		environment.memory_runtime().as_ref(),
		None,
		None,
		usize::MAX,
	)
	.map_err(DriverChatError::from)?;
	state.update(|snapshot| {
		let values = [
			("memory", memory_prompt.memory.content.clone()),
			("standing", memory_prompt.standing.content.clone()),
			("recall", memory_prompt.recall.content.clone()),
		]
		.into_iter()
		.filter_map(|(name, content)| content.map(|content| (name, omp_scribe::Value::from(content))))
		.collect::<omp_scribe::Value>();
		snapshot.props.set(omp_agent::prompt_keys::MEMORY, values);
	});
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
	environment
		.bind_schedule_delivery(parent.schedule_delivery_backend())
		.await?;
	let mut _external_control_binding: Option<omp_envd::exthost::ExternalControlAuthorityBinding> =
		None;
	parent.start_idle_parking();
	let _eval_parent_binding = environment
		.bind_eval_sdk_parent(parent.session_id(), parent.clone())
		.map_err(|error| DriverChatError::EvalBridge(Str::from(error.to_string())))?;
	environment
		.reflection_bridge()
		.bind(parent.clone())
		.map_err(DriverChatError::from)?;
	let cold_agents = scope.sessions_dir.join("eval-agents");
	if cold_agents.is_dir() {
		omp_agent::AgentRegistry::global().discover_transcripts(&cold_agents)?;
	}
	let provider_registry = auth_registry.clone();
	let auth = auth_registry.map(ChatAuthWorker::start);
	let presentation_bridge = Arc::new(PresentationBridge::new(64));
	let extension_callbacks = environment.extension_callback_dispatcher();
	let drafts = DraftStore::new(&data_dir)?;
	let breadcrumbs = TerminalBreadcrumbs::new(&data_dir)?;
	let terminal_id = omp_tui::ttyid::terminal_id();
	loop {
		if scope.persist_sessions {
			breadcrumbs.restamp(terminal_id.as_str(), &SessionId(session.id.clone()))?;
		}
		parent.update(state.clone(), session.id.clone());
		let approval_book = Arc::new(omp_agent::ApprovalBook::new());
		state.update(|snapshot| {
			snapshot.prompt_source =
				prompt_head.wrap_prompt_source(Arc::clone(&snapshot.prompt_source));
		});
		let session_root = scope.sessions_dir.join(session.id.as_str());
		ensure_state_directory(&session_root)?;
		ensure_state_directory(&session_root.join("local"))?;
		let host_backends = EnvdHostOwnerBackends::production(
			&session_root.join("control"),
			Arc::clone(&approval_book),
		);
		let telemetry_index = Arc::new(TelemetryIndex::open(
			&state_dir.join("telemetry"),
			&state_dir.join("telemetry.sqlite3"),
		)?);
		let context_window = {
			let current = state.snapshot();
			model_context_window(scope.catalog, &current.turn.params.model)
		};
		let Session { id, journal, initial_items } = session;
		let current_id = id.clone();
		let content = omp_driver::discovery::active_content_snapshots(scope.root);
		let (ttsr, ttsr_diagnostics) = omp_driver::rulebook::ttsr_registry(content.rules.as_ref());
		for error in ttsr_diagnostics {
			tracing::warn!(%error, "TTSR rule condition was rejected");
		}
		let mut agent =
			Agent::new(client.clone(), env.clone(), state.clone(), journal, CHAT_CAPS_BASE);
		parent.bind_host_control(id.clone(), agent.host_control());
		let secrets = SecretSessionSnapshot::build(
			0,
			&data_dir.join("secrets.toml"),
			&scope.root.join(".omp/secrets.toml"),
			iter::empty(),
		)?;
		if omp_driver::settings::current(&data_dir)?.secrets.enabled {
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
		match omp_driver::registry::production_redemption_authority(&data_dir) {
			Ok(Some(authority)) => agent.set_redemption_authority(authority),
			Ok(None) => {},
			Err(error) => {
				tracing::warn!(%error, "codex redemption authority was not constructed");
			},
		}
		parent.bind_parent_jobs(Arc::clone(agent.jobs()));
		let blob_store = BlobStore::open(&data_dir)?;
		let artifact_catalog = Arc::new(Mutex::new(ArtifactCatalog::open(&blob_store)?));
		agent.set_artifact_catalog(Arc::clone(&artifact_catalog));
		agent.set_blob_store(blob_store.clone());
		let credential_factory = if let Some(control) = auth_control.as_ref() {
			Arc::new(omp_driver::secrets::credential_secret_control_factory(
				control.clone(),
				credential_control_grants.clone(),
				&secrets,
			)) as Arc<dyn ControlAuthorityFactory>
		} else {
			let channel = gateway_channel
				.as_ref()
				.ok_or(ChatError::MissingAuthority("credentials"))?;
			Arc::new(omp_driver::auth_backend::gateway_credential_secret_control_factory(
				channel.clone(),
				credential_control_grants.clone(),
				&secrets,
			)) as Arc<dyn ControlAuthorityFactory>
		};
		let prompt_factory = prompt_control_factory(prompt_head.clone());
		let presentation_factory = presentation_control_factory(
			executor.clone(),
			Arc::clone(&presentation_bridge),
			Arc::clone(&extension_callbacks),
		);
		let telemetry_query =
			Arc::new(TelemetryIndexQuery::new(Arc::clone(&telemetry_index), id.clone()));
		let telemetry_factory = telemetry_control_factory(telemetry_query);
		let verdict_factory = verdict_control_factory(
			id.clone(),
			agent.jobs().as_ref().clone(),
			agent.host_control(),
			Arc::clone(&extension_callbacks),
		);
		let provider_factory = gateway_provider_factory.clone().or_else(|| {
			provider_registry
				.as_ref()
				.zip(provider_builtins.as_ref())
				.map(|(registry, builtins)| {
					provider_control_factory(registry.clone(), builtins.clone(), blob_store.clone())
				})
		});
		let capture_rx =
			omp_inference::transport::global_provider_capture().subscribe(Some(id.as_str()));
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
		agent.set_run_activity(PowerActivity::new(power_mode));
		let autolearn_campaign = autolearn.enabled.then(|| AutolearnCampaign::new(autolearn));
		let mut recovered_autolearn = false;
		agent.recover_campaigns(
			|spec_id| {
				if let Some(core) = omp_agent::core_regime(spec_id) {
					return Some(core);
				}
				let Some((spec, machine, _)) = autolearn_campaign.as_ref() else {
					return None;
				};
				if spec_id != omp_driver::autolearn::AUTOLEARN_CAMPAIGN_ID || recovered_autolearn {
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
				.any(|entry| entry.spec_id == omp_driver::autolearn::AUTOLEARN_CAMPAIGN_ID)
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
		let modes = Arc::new(CampaignHandle::new());
		modes.sync_campaigns(agent.arbiter().campaigns());
		bind_goal_todo_context(agent.events().subscribe_lossless(), Arc::downgrade(&modes));
		modes.bind_plan_selection(state.clone(), plan_selection.clone());
		parent.bind_campaigns(Arc::clone(&modes));
		let _goal_binding = goal_control.bind(Arc::clone(&modes), agent.control());
		state.update(|snapshot| {
			snapshot.prompt_source = modes.prompt_source(Arc::clone(&snapshot.prompt_source));
		});
		agent.set_continuation_source(modes.clone());
		let campaign_factory =
			campaign_control_factory(agent.control(), environment.extension_campaign_resolver());
		let provider_factory = provider_factory.ok_or(ChatError::MissingAuthority("provider"))?;
		_external_control_binding = Some(
			SessionControlFactories {
				policy:            host_backends.policy_factory,
				parameters:        host_backends.parameter_factory,
				workers:           host_backends.worker_factory,
				direct_filesystem: host_backends.direct_filesystem_factory,
				credentials:       credential_factory,
				prompts:           prompt_factory,
				ui:                presentation_factory,
				telemetry:         telemetry_factory,
				verdicts:          verdict_factory,
				provider:          provider_factory,
				campaigns:         campaign_factory,
			}
			.bind(environment, AgentsControlAuthority::factory(Arc::clone(&parent))),
		);
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
			.map_err(|error| DriverChatError::EvalBridge(Str::from(error.to_string())))?;
		node.set_status(AgentStatus::Running);
		let broker = parent.broker();
		let inbox = broker
			.register(&node, agent.mailbox())
			.map_err(|error| DriverChatError::EvalBridge(Str::from(error.to_string())))?;
		let inbox = hub_backend::share_inbox(inbox);
		parent.bind_inbox(id.clone(), Arc::clone(&inbox));
		parent.recover_parked_children().await;
		let _hub = hub_backend::attach(Arc::new(hub_backend::ChatHubBackend::new(
			broker,
			inbox,
			Arc::clone(agent.jobs()),
			env.clone(),
			id.clone(),
			id.clone(),
			Some(agent.events().clone()),
			Some(parent.supervisor()),
		)));
		let _vibe = omp_driver::vibe::attach_chat(Arc::clone(&parent), Arc::clone(&modes));
		let initial_draft = if scope.persist_sessions {
			drafts
				.consume(&SessionId(current_id.clone()))?
				.unwrap_or_default()
		} else {
			String::new()
		};
		let (_approval_route, approval_inbox) =
			omp_agent::ApprovalRoute::new(Arc::clone(&approval_book));
		let (replica_pump, replica) =
			GuestRelayPump::new(data_dir.join("collab"), scope.root.to_path_buf(), now_ms());
		let replica_shutdown = replica.clone();
		let mut replica_task = tokio::spawn(replica_pump.run());
		let (collab_authority, collab) = CollabSessionAuthority::with_guest_replica(Some(replica));
		let mut collab_task = session::spawn_session_owner(collab_authority);
		let title = scope
			.session_index
			.subagent_tree(&SessionId(id.clone()))?
			.into_iter()
			.next()
			.map_or_else(SessionTitleState::default, |session| SessionTitleState {
				title:  session.title,
				source: session.title_source,
			});
		let outcome = chat_ui::run(
			executor.clone(),
			agent,
			ChatUiSession { session_id: id, initial_items, context_window, title },
			Arc::clone(&scope.registry),
			parent.tree(),
			Arc::clone(&parent),
			Some(collab),
			modes,
			auth.as_ref().map(|worker| worker.ui().clone()),
			data_dir.clone(),
			Arc::clone(&scope.session_index),
			scope.root.to_path_buf(),
			session_root.join("local"),
			security_enabled,
			title_enabled,
			vec![
				content
					.commands
					.iter()
					.cloned()
					.map(crate::chat_ui::input::CommandContribution::from)
					.collect(),
			],
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
			Some(presentation_bridge.attach()),
		)
		.await;
		replica_shutdown.stop().await;
		if time::timeout(Duration::from_secs(3), &mut replica_task)
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
		if time::timeout(Duration::from_secs(3), &mut collab_task)
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
			host::HostExit::Quit => break,
			host::HostExit::Suspend => {
				#[cfg(unix)]
				if let Err(error) = signal::kill(Pid::from_raw(0), signal::Signal::SIGSTOP) {
					tracing::warn!(%error, "failed to suspend process group");
				}
				eval_control.request_reset();
				let model = state.snapshot().turn.params.model.clone();
				let prompt_props = state.snapshot().props.clone();
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
				next.props = replace_model_props(prompt_props, &model);
				state = AgentState::new(next);
			},
			host::HostExit::Resume(id) => {
				eval_control.request_reset();
				let model = state.snapshot().turn.params.model.clone();
				let prompt_props = state.snapshot().props.clone();
				session = open_session(
					scope.root,
					scope.sessions_dir,
					SessionOpen::Resume(&id),
					scope.registry.as_ref(),
					scope
						.persist_sessions
						.then(|| Arc::clone(&scope.session_index)),
				)?;
				omp_envd::migrate_session_artifacts(
					scope.sessions_dir,
					current_id.as_str(),
					session.id.as_str(),
				)
				.map_err(|source| DriverChatError::ProjectState {
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
				next.props = replace_model_props(prompt_props, &model);
				state = AgentState::new(next);
			},
			host::HostExit::NewSession => {
				eval_control.request_reset();
				let model = state.snapshot().turn.params.model.clone();
				let prompt_props = state.snapshot().props.clone();
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
				omp_envd::migrate_session_artifacts(
					scope.sessions_dir,
					current_id.as_str(),
					session.id.as_str(),
				)
				.map_err(|source| DriverChatError::ProjectState {
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
				next.props = replace_model_props(prompt_props, &model);
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
	catalog: &snapshot::Catalog,
	model: &str,
) -> omp_agent::StreamWatchdog {
	let watchdog = catalog
		.model(omp_catalog::ModelKey::from_ref(model))
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
