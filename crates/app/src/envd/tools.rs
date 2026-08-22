//! Production built-in tool registry assembly.

use std::{
	collections::BTreeSet,
	future::Future,
	sync::{
		Arc, LazyLock,
		atomic::{AtomicU64, Ordering},
	},
	time::{SystemTime, UNIX_EPOCH},
};

use omp_core::{Duration, Hash32, Str, sf};
use omp_proto::{
	prost::Message as _,
	toolhost::v1::{GrammarSyntax as WorkerGrammarSyntax, ToolDecl, tool_constraint},
};
use omp_tool::{
	Claims, Constraint, GrammarSyntax, Precedence, Presentation, Registry, Rev, Tool, ToolSpec,
	ToolsPolicy,
};
use omp_tools::{
	BuiltinRendererIdentities,
	device::{DeviceCatalog, dyn_enabled, dyn_tool, flatten_slots},
	edit::{EditRevisionCandidates, FormatPolicy, resolve_edit_revision},
	read::conflicts::ConflictRegistry,
	register_builtin_renderers,
};
use parking_lot::RwLock;

use super::{
	EnvdError,
	blobs::BlobHost,
	docs::{DocumentHost, ResourceMutationServices},
	eval::{ProcessEvalExec, SessionBridgeHost},
	exec::ExecHost,
	exec_settings::{AcpRouting, AcpSettings, ShellSettings},
	media_devices,
	search_backend::SearchBridgeHost,
	tool_debug::DocumentDebugControl,
	tool_lsp::DocumentLspControl,
	tool_read_sources::ReadSourceAdapter,
	tool_search::WorkspaceSearchAdapter,
	tool_settings::ToolSettings,
	tool_shell::{AcpExecSlot, ShellExecHost},
	tool_url::production_url_resolvers,
	worker::ExtHostSupervisor,
	workspace::WorkspaceHost,
};

/// Builds the complete registry shared by environment dispatch and the agent.
///
/// Resource adapters are cloned into their typed executors. Worker declarations
/// occupy device presentation entries and explicit worker routes; only the
/// environment's worker supervisor can invoke them.
pub(crate) fn production_registry<I: omp_tools::device::DeviceInvoker + 'static>(
	documents: &DocumentHost,
	blobs: &BlobHost,
	exec: &ExecHost,
	state_dir: &std::path::Path,
	session_id: &str,
	github_cache: Arc<omp_storage::github_cache::GithubCache>,
	mcp: &Arc<super::mcp::McpService>,
	workspace: &WorkspaceHost,
	memory: &Arc<omp_memory::MemoryRuntime>,
	telemetry: &Arc<omp_storage::telemetry_index::TelemetryIndex>,
	root_uri: &Str,
	workers: &ExtHostSupervisor,
	interrupt_grace: Duration,
	tool_settings: &ToolSettings,
	shell_settings: &ShellSettings,
	acp_settings: &AcpSettings,
	acp_exec: AcpExecSlot,
	autolearn_settings: &crate::settings::AutolearnSettings,
	device_invoker: I,
	policy: ToolsPolicy,
	mut registry: Registry,
) -> Result<
	(
		Arc<Registry>,
		Arc<SessionBridgeHost>,
		Arc<crate::memory::ReflectionBridgeHost>,
		omp_tools::eval::EvalSessionControl,
		AgentCheckpointControl,
		omp_tools::staging::PreviewRegistry,
		Arc<omp_tools::read::resolver::ResolverTable<super::tool_url::UrlResolver>>,
		AgentGoalControl,
		Arc<SearchBridgeHost>,
		Arc<super::github_url::GithubCredentialBridge>,
	),
	EnvdError,
> {
	let previews = omp_tools::staging::PreviewRegistry::new();
	registry.protect_core_claims([
		"read",
		"write",
		"shell",
		"edit",
		"glob",
		"eval",
		"task",
		"hub",
		"browser",
		"learn",
		"manage_skill",
		"computer",
		"lsp",
		"debug",
	]);
	for name in [
		"read",
		"edit",
		"shell",
		"grep",
		"glob",
		"write",
		"eval",
		"todo",
		"ask",
		"fetch",
		"web_search",
		"think",
		"goal",
		"yield",
		"checkpoint",
		"rewind",
		"hub",
		"browser",
		"github",
		"image_gen",
		"tts",
		"report_issue",
		"vibe",
		"retain",
		"recall",
		"reflect",
		"memory_edit",
		"learn",
		"manage_skill",
		"dyn",
		"lsp",
		"debug",
		"computer",
	] {
		ensure_name_absent(&registry, name)?;
	}
	let search_bridge = Arc::new(SearchBridgeHost::new());
	let browser_daemon = crate::browser_daemon::BrowserDaemon::start(blobs.clone());
	registry.register(
		omp_tools::browser::tool(browser_daemon),
		Presentation::Device,
		builtin_device_claims(),
	)?;
	let computer = super::computer::ComputerSessionHost::new(blobs.clone());
	registry.register(
		omp_tools::computer::tool(computer),
		Presentation::Device,
		builtin_device_claims(),
	)?;
	for device in [
		media_devices::image_gen(
			Arc::clone(&search_bridge),
			blobs.clone(),
			workspace.root().to_path_buf(),
		),
		media_devices::tts(Arc::clone(&search_bridge), blobs.clone(), workspace.root().to_path_buf()),
	] {
		registry.register(device, Presentation::Device, builtin_device_claims())?;
	}
	registry.register(
		media_devices::report_issue(Arc::clone(telemetry)),
		Presentation::Device,
		builtin_device_claims(),
	)?;
	registry.register(crate::vibe::tool(), Presentation::Device, builtin_device_claims())?;
	let active = crate::discovery::active_content_snapshots(workspace.root());
	let reflection_bridge = Arc::new(crate::memory::ReflectionBridgeHost::new());
	let memory_capabilities = memory.capabilities();
	if memory_capabilities.writable {
		registry.register(
			omp_tools::memory::retain_tool(Arc::clone(memory)),
			Presentation::Device,
			builtin_device_claims(),
		)?;
	}
	if memory_capabilities.searchable {
		registry.register(
			omp_tools::memory::recall_tool(Arc::clone(memory)),
			Presentation::Device,
			builtin_device_claims(),
		)?;
		registry.register(
			omp_tools::memory::reflect_tool(Arc::clone(memory), Arc::clone(&reflection_bridge)),
			Presentation::Device,
			builtin_device_claims(),
		)?;
	}
	if memory_capabilities.editable {
		registry.register(
			omp_tools::memory_edit::tool(Arc::clone(memory)),
			Presentation::Device,
			builtin_device_claims(),
		)?;
	}
	if autolearn_settings.enabled {
		let home = std::env::var_os("HOME")
			.map_or_else(|| workspace.root().to_path_buf(), std::path::PathBuf::from);
		let authored_names = active
			.skills
			.all()
			.iter()
			.filter(|skill| skill.source.as_str() != crate::skills::managed::PROVIDER_ID)
			.map(|skill| skill.name.clone())
			.collect::<BTreeSet<_>>();
		let authority = Arc::new(super::managed_skills::ManagedSkills::new(
			crate::discovery::managed_skills::root(&crate::discovery::native::user_config_root(&home)),
			authored_names,
		));
		registry.register(
			omp_tools::manage_skill::tool(Arc::clone(&authority)),
			Presentation::Device,
			builtin_device_claims(),
		)?;
		if memory_capabilities.writable {
			registry.register(
				omp_tools::learn::tool(Arc::clone(memory), authority),
				Presentation::Device,
				builtin_device_claims(),
			)?;
		}
	}
	let github_credentials = Arc::new(super::github_url::GithubCredentialBridge::new());
	let github = super::github::GithubService::new(
		workspace.root().to_path_buf(),
		state_dir,
		Arc::clone(&github_credentials),
	);
	crate::telemetry_upload::start(Arc::clone(telemetry), Arc::clone(&github_credentials));
	registry.register(
		omp_tools::github::tool(github),
		Presentation::Device,
		builtin_device_claims(),
	)?;
	let ssh = super::ssh::SshService::new(
		super::ssh::HostStore::load(&state_dir.join("ssh/hosts.toml"))
			.map_err(|error| EnvdError::State(Str::new(error.to_string())))?,
	);
	let vault = super::vault::VaultService::load(&state_dir.join("vaults.toml"))
		.map_err(|error| EnvdError::State(Str::new(error.to_string())))?;
	documents.set_resource_mutations(ResourceMutationServices {
		ssh:   ssh.clone(),
		vault: vault.clone(),
	});
	let read_sources = ReadSourceAdapter::new(
		documents.clone(),
		workspace.clone(),
		super::document_cache::project_document_cache(state_dir),
	);
	let conflicts = Arc::new(ConflictRegistry::default());
	let resolvers = production_url_resolvers(
		Arc::clone(&conflicts),
		blobs.store().clone(),
		session_id,
		state_dir.join("local"),
		workspace.root().to_path_buf(),
		github_cache,
		Arc::clone(&github_credentials),
		crate::skills::SkillResolver::new(active.skills),
		crate::rulebook::RuleResolver::new(active.rules),
		Arc::clone(mcp),
		ssh,
		vault,
	);
	let edit_pin = tool_settings
		.edit_dialect
		.as_deref()
		.map(str::parse::<Rev>)
		.transpose()
		.map_err(|error| EnvdError::EditDialect(error.to_string().into()))?;
	let environment_edit_dialect = std::env::var("OMP_EDIT_DIALECT").ok();
	let force_hashline = std::env::var_os("OMP_STRICT_EDIT_MODE").is_some();
	let selected_edit = resolve_edit_revision(EditRevisionCandidates {
		environment: environment_edit_dialect.as_deref(),
		pin: edit_pin.as_ref(),
		force_hashline,
		..EditRevisionCandidates::default()
	})
	.map_err(EnvdError::EditDialect)?
	.revision;
	let read = omp_tools::read::tool_with_policy(
		read_sources.clone(),
		blobs.clone(),
		Arc::clone(&resolvers),
		Arc::clone(&conflicts),
		omp_tools::read::ReadPolicy {
			fetch_enabled:      tool_settings.fetch_enabled,
			render_markdown:    tool_settings.render_markdown,
			auto_resize_images: tool_settings.auto_resize_images,
			hashline_headers:   tool_settings.enabled("edit") && selected_edit.family.as_str() == "hl",
		},
	);
	let read_identity = read.spec().identity();
	if tool_settings.enabled("read") {
		registry.register(read, Presentation::Slot, core_claims())?;
	}
	let fetch = omp_tools::fetch::tool(read_sources.clone());
	if tool_settings.enabled("fetch") && tool_settings.fetch_enabled {
		registry.register(fetch, Presentation::Slot, core_claims())?;
	}
	let web_search_identity = if tool_settings.enabled("web_search") {
		let web_search = omp_tools::web_search::tool(Arc::clone(&search_bridge));
		let identity = web_search.spec().identity();
		registry.register(web_search, Presentation::Slot, core_claims())?;
		Some(identity)
	} else {
		None
	};

	let mut hashline_edit = Some(omp_tools::edit::tool_with_snapshots(
		documents.clone(),
		blobs.clone(),
		tool_settings.format_policy,
	));
	let mut replace_edit =
		Some(omp_tools::edit::replace_tool(documents.clone(), tool_settings.format_policy));
	let mut patch_edit =
		Some(omp_tools::edit::patch_tool(documents.clone(), tool_settings.format_policy));
	let mut apply_patch_edit =
		Some(omp_tools::edit::apply_patch_tool(documents.clone(), tool_settings.format_policy));
	let mut sloppy_edit =
		Some(omp_tools::edit::sloppy_tool(documents.clone(), tool_settings.format_policy));
	let edit_identity = [
		hashline_edit
			.as_ref()
			.expect("constructed")
			.spec()
			.identity(),
		replace_edit
			.as_ref()
			.expect("constructed")
			.spec()
			.identity(),
		patch_edit.as_ref().expect("constructed").spec().identity(),
		apply_patch_edit
			.as_ref()
			.expect("constructed")
			.spec()
			.identity(),
		sloppy_edit.as_ref().expect("constructed").spec().identity(),
	]
	.into_iter()
	.find(|identity| identity.rev == selected_edit)
	.ok_or_else(|| EnvdError::EditDialect(sf!("selected edit revision is not registered")))?
	.clone();
	if tool_settings.enabled("edit") {
		let mut edits = [
			(
				hashline_edit
					.as_ref()
					.expect("constructed")
					.spec()
					.identity(),
				0_u8,
			),
			(
				replace_edit
					.as_ref()
					.expect("constructed")
					.spec()
					.identity(),
				1,
			),
			(patch_edit.as_ref().expect("constructed").spec().identity(), 2),
			(
				apply_patch_edit
					.as_ref()
					.expect("constructed")
					.spec()
					.identity(),
				3,
			),
			(sloppy_edit.as_ref().expect("constructed").spec().identity(), 4),
		];
		edits.sort_by_key(|(identity, _)| identity.rev == selected_edit);
		for (_, index) in edits {
			match index {
				0 => registry.register(
					hashline_edit.take().expect("once"),
					Presentation::Slot,
					core_claims(),
				)?,
				1 => registry.register(
					replace_edit.take().expect("once"),
					Presentation::Slot,
					core_claims(),
				)?,
				2 => registry.register(
					patch_edit.take().expect("once"),
					Presentation::Slot,
					core_claims(),
				)?,
				3 => registry.register(
					apply_patch_edit.take().expect("once"),
					Presentation::Slot,
					core_claims(),
				)?,
				4 => registry.register(
					sloppy_edit.take().expect("once"),
					Presentation::Slot,
					core_claims(),
				)?,
				_ => unreachable!(),
			}
		}
	}
	let write = omp_tools::write::tool_with_policy_and_conflicts(
		documents.clone(),
		conflicts,
		tool_settings.format_policy,
	);
	let write_identity = write.spec().identity();
	if tool_settings.enabled("write") {
		registry.register(write, Presentation::Slot, core_claims())?;
	}
	if tool_settings.enabled("lsp") {
		let maximum = tool_settings
			.max_timeout
			.and_then(|duration| duration.to_std().ok())
			.unwrap_or_else(|| std::time::Duration::from_secs(300));
		registry.register(
			omp_tools::lsp::tool(DocumentLspControl::new(documents.clone(), exec.clone()), maximum),
			Presentation::Slot,
			core_claims(),
		)?;
	}
	if tool_settings.enabled("debug") {
		let maximum = tool_settings
			.max_timeout
			.and_then(|duration| duration.to_std().ok())
			.unwrap_or_else(|| std::time::Duration::from_secs(300));
		registry.register(
			omp_tools::debug::tool(DocumentDebugControl::new(documents.clone()), maximum),
			Presentation::Slot,
			core_claims(),
		)?;
	}
	let search = WorkspaceSearchAdapter::new(
		workspace.clone(),
		documents.clone(),
		read_sources.clone(),
		Arc::clone(&resolvers),
	);
	let grep = omp_tools::grep::tool(search.clone(), blobs.clone());
	let grep_identity = grep.spec().identity();
	if tool_settings.enabled("grep") {
		registry.register(grep, Presentation::Slot, core_claims())?;
	}
	let glob = omp_tools::glob::tool(search, blobs.clone());
	let glob_identity = glob.spec().identity();
	if tool_settings.enabled("glob") {
		registry.register(glob, Presentation::Slot, core_claims())?;
	}
	if tool_settings.enabled("ast_grep") {
		registry.register(
			omp_tools::ast_grep::tool(workspace.root().to_path_buf()),
			Presentation::Slot,
			core_claims(),
		)?;
	}
	if tool_settings.enabled("ast_edit") {
		registry.register(
			omp_tools::ast_edit::tool(workspace.root().to_path_buf(), previews.clone()),
			Presentation::Slot,
			core_claims(),
		)?;
	}
	let eval_host = Arc::new(SessionBridgeHost::new());
	let mut eval_control = omp_tools::eval::EvalSessionControl::default();
	let eval_identity = if tool_settings.enabled("eval") {
		match preflight_python_eval(Arc::clone(&eval_host), interrupt_grace, blobs.clone()) {
			Ok(eval_exec) => {
				let (eval_tool, control) = omp_tools::eval::eval_controlled(eval_exec);
				let identity = eval_tool.spec().identity();
				registry.register(eval_tool, Presentation::Slot, core_claims())?;
				eval_control = control;
				Some(identity)
			},
			Err(error) => {
				tracing::warn!(
					error = %error,
					"eval omitted because CPython is unreachable; run `just setup-python` and restart OMP"
				);
				None
			},
		}
	} else {
		None
	};
	if tool_settings.enabled("todo") {
		registry.register(omp_tools::todo::tool(), Presentation::Slot, core_claims())?;
	}
	if tool_settings.enabled("ask") {
		registry.register(
			omp_tools::ask::tool_with_vocalizer(
				omp_chat_ui::ask::presenter(),
				media_devices::ask_vocalizer(Arc::clone(&search_bridge)),
			),
			Presentation::Slot,
			core_claims(),
		)?;
	}
	if tool_settings.enabled("think") {
		registry.register(omp_tools::think::tool(), Presentation::Slot, core_claims())?;
	}
	let goal_control = AgentGoalControl::default();
	if tool_settings.enabled("goal") {
		registry.register(
			omp_tools::goal::tool(goal_control.clone()),
			Presentation::Hidden,
			core_claims(),
		)?;
	}
	let hub_identity = if tool_settings.enabled("hub") {
		let hub = crate::chat::chat_hub_tool();
		let identity = hub.spec().identity();
		registry.register(hub, Presentation::Slot, core_claims())?;
		Some(identity)
	} else {
		None
	};
	if tool_settings.enabled("yield") {
		registry.register(omp_tools::yield_tool::tool(), Presentation::Slot, core_claims())?;
	}
	let checkpoint_control = AgentCheckpointControl::default();
	let (checkpoint, rewind) = omp_tools::checkpoint::tools(checkpoint_control.clone());
	if tool_settings.enabled("checkpoint") {
		registry.register(checkpoint, Presentation::Slot, core_claims())?;
	}
	if tool_settings.enabled("rewind") {
		registry.register(rewind, Presentation::Slot, core_claims())?;
	}
	let catalog = DeviceCatalog::default();
	if tool_settings.enabled("dyn") && dyn_enabled(policy) {
		registry.register(
			dyn_tool(device_invoker, catalog.clone(), policy),
			Presentation::Slot,
			core_claims(),
		)?;
	}
	let shell_identity = if tool_settings.enabled("shell") && shell_settings.enabled {
		let sibling_tools = registry
			.live_identities()
			.filter_map(|(name, _)| {
				(name != "shell" && registry.presentation(name).ok() == Some(Presentation::Slot))
					.then(|| name.clone())
			})
			.collect::<Arc<[_]>>();
		let snapshot = omp_tools::shell::ShellPromptSnapshot {
			sibling_tools,
			platform: Str::new(std::env::consts::OS),
			embedded_builtins: shell_settings.embedded_builtins,
			interceptor_enabled: shell_settings.interceptor.enabled,
			interceptor_rules: shell_settings
				.interceptor
				.patterns
				.iter()
				.map(|rule| omp_tools::shell_intercept::Rule {
					pattern: rule.pattern.clone(),
					tool:    rule.tool.clone(),
					message: rule.message.clone(),
				})
				.collect(),
			acp_routing: acp_settings.routing != AcpRouting::Never,
			profile: Str::new(<&'static str>::from(shell_settings.profile)),
			command_prefix: shell_settings.command_prefix.is_some(),
			minimizer_enabled: shell_settings.minimizer.enabled,
		};
		let shell = omp_tools::shell::shell_with_snapshot_and_timeout_bounds(
			ShellExecHost::new(
				exec.clone(),
				root_uri.clone(),
				Arc::clone(&resolvers),
				shell_settings.clone(),
				acp_exec,
				acp_settings.routing != AcpRouting::Never,
			),
			shell_timeout_bounds(tool_settings),
			&snapshot,
		)
		.with_auto_background(
			shell_settings.auto_background.enabled,
			std::time::Duration::from_millis(shell_settings.auto_background.threshold_ms),
		);
		let identity = shell.spec().identity();
		registry.register(shell, Presentation::Slot, core_claims())?;
		Some(identity)
	} else {
		None
	};
	register_builtin_renderers(registry.render_registry_mut(), BuiltinRendererIdentities {
		edit:       tool_settings.enabled("edit").then_some(edit_identity),
		grep:       tool_settings.enabled("grep").then_some(grep_identity),
		web_search: web_search_identity,
		glob:       tool_settings.enabled("glob").then_some(glob_identity),
		shell:      shell_identity,
		hub:        hub_identity,
		write:      tool_settings.enabled("write").then_some(write_identity),
		read:       tool_settings.enabled("read").then_some(read_identity),
		eval:       eval_identity,
	})
	.map_err(|error| EnvdError::WorkerDeclaration(Str::from(error.to_string())))?;
	let flattened_slots = if policy == ToolsPolicy::ToolOnly {
		Some(
			flatten_slots(
				workers
					.registrations()
					.iter()
					.map(|registration| {
						let definition =
							registration
								.declaration
								.definition
								.as_ref()
								.ok_or_else(|| {
									worker_declaration_error("worker tool declaration has no definition")
								})?;
						Ok((Str::from(definition.name.as_str()), registration.owner.extension().clone()))
					})
					.collect::<Result<Vec<_>, EnvdError>>()?,
			)
			.map_err(|collision| {
				EnvdError::WorkerDeclaration(Str::from(format!(
					"tool_only slot {} is claimed by both {} and {}",
					collision.slot, collision.first, collision.second
				)))
			})?,
		)
	} else {
		None
	};
	for registration in workers.registrations() {
		let declaration = &registration.declaration;
		let mut spec = worker_spec(declaration)?;
		if flattened_slots.is_some() {
			spec.name = Str::from(spec.name.as_str().replace('/', "_"));
		}
		ensure_name_absent(&registry, &spec.name)?;
		registry.register_worker(
			spec,
			if flattened_slots.is_some() {
				Presentation::Slot
			} else {
				Presentation::Device
			},
			Claims {
				precedence: Precedence::DEFAULT,
				claimant:   registration.owner.extension().clone(),
				replaces:   None,
			},
		)?;
	}
	let registry = Arc::new(registry);
	catalog
		.bind(Arc::clone(&registry))
		.map_err(|_| EnvdError::WorkerDeclaration(sf!("dynamic device catalog bound twice")))?;
	eval_host
		.bind_registry(Arc::clone(&registry))
		.map_err(|error| EnvdError::Eval(Str::from(error.to_string())))?;
	Ok((
		registry,
		eval_host,
		reflection_bridge,
		eval_control,
		checkpoint_control,
		previews,
		resolvers,
		goal_control,
		search_bridge,
		github_credentials,
	))
}
#[derive(Clone)]
struct GoalBinding {
	id:     u64,
	modes:  Arc<crate::modes::CampaignHandle>,
	sender: omp_agent::ControlSender,
}

/// Late-bound durable goal-mode authority for the active chat session.
#[derive(Clone, Default)]
pub struct AgentGoalControl {
	binding: Arc<RwLock<Option<GoalBinding>>>,
	next_id: Arc<AtomicU64>,
}

impl AgentGoalControl {
	/// Binds the active session goal projection and Agent campaign authority
	/// until the returned lease is dropped.
	pub fn bind(
		&self,
		modes: Arc<crate::modes::CampaignHandle>,
		sender: omp_agent::ControlSender,
	) -> AgentGoalBinding {
		let id = self
			.next_id
			.fetch_add(1, Ordering::Relaxed)
			.saturating_add(1);
		*self.binding.write() = Some(GoalBinding { id, modes, sender });
		AgentGoalBinding { control: self.clone(), id }
	}

	fn binding(&self) -> Result<GoalBinding, omp_tools::goal::Fault> {
		self.binding.read().clone().ok_or(omp_tools::goal::Fault::Unavailable)
	}

	fn unbind(&self, id: u64) {
		let mut binding = self.binding.write();
		if binding.as_ref().is_some_and(|binding| binding.id == id) {
			*binding = None;
		}
	}
}

/// Sole-owner lease for one active goal-mode binding.
#[must_use]
pub struct AgentGoalBinding {
	control: AgentGoalControl,
	id:      u64,
}

impl Drop for AgentGoalBinding {
	fn drop(&mut self) {
		self.control.unbind(self.id);
	}
}

impl omp_tools::goal::GoalControl for AgentGoalControl {
	fn apply(
		&self,
		params: omp_tools::goal::Params,
	) -> impl Future<Output = Result<Option<omp_tools::goal::Goal>, omp_tools::goal::Fault>> + Send + '_
	{
		let binding = self.binding();
		async move {
			let GoalBinding { modes, sender, .. } = binding?;
			let now = SystemTime::now()
				.duration_since(UNIX_EPOCH)
				.unwrap_or_default()
				.as_millis()
				.try_into()
				.unwrap_or(u64::MAX);
			let outcome = match params.op {
				omp_tools::goal::Operation::Create => {
					let objective = params
						.objective
						.ok_or(omp_tools::goal::Fault::ObjectiveRequired)?;
					if objective.trim().is_empty() {
						return Err(omp_tools::goal::Fault::ObjectiveRequired);
					}
					if params.token_budget == Some(0) {
						return Err(omp_tools::goal::Fault::InvalidBudget);
					}
					let (engagement, newly_engaged) = ensure_goal_campaign(&sender).await?;
					let goal = match modes.set_goal(objective, params.token_budget, now) {
						Ok(goal) => goal,
						Err(error) => {
							if newly_engaged {
								let _ = sender.disengage_campaign(engagement).await;
							}
							return Err(map_goal_error(error));
						},
					};
					if let Err(error) =
						update_goal_campaign_state(&sender, &engagement, &goal).await
					{
						let _ = modes.drop_goal(now);
						let _ = sender.disengage_campaign(engagement).await;
						return Err(error);
					}
					crate::goal::GoalOutcome {
						lifecycle: crate::goal::GoalLifecycle::Created,
						goal:      Some(goal),
					}
				},
				omp_tools::goal::Operation::Get => crate::goal::GoalOutcome {
					lifecycle: crate::goal::GoalLifecycle::Current,
					goal:      modes.goal(),
				},
				omp_tools::goal::Operation::Complete => {
					let engagement = active_goal_engagement(&sender)
						.await?
						.ok_or(omp_tools::goal::Fault::Unavailable)?;
					let goal = modes.complete_goal(now).map_err(map_goal_error)?;
					sender
						.disengage_campaign(engagement)
						.await
						.map_err(map_goal_campaign_error)?;
					crate::goal::GoalOutcome {
						lifecycle: crate::goal::GoalLifecycle::Completed,
						goal:      Some(goal),
					}
				},
				omp_tools::goal::Operation::Resume => {
					let (engagement, newly_engaged) = ensure_goal_campaign(&sender).await?;
					let goal = match modes.resume_goal(now) {
						Ok(goal) => goal,
						Err(error) => {
							if newly_engaged {
								let _ = sender.disengage_campaign(engagement).await;
							}
							return Err(map_goal_error(error));
						},
					};
					if let Err(error) =
						update_goal_campaign_state(&sender, &engagement, &goal).await
					{
						let _ = sender.disengage_campaign(engagement).await;
						return Err(error);
					}
					crate::goal::GoalOutcome {
						lifecycle: crate::goal::GoalLifecycle::Resumed,
						goal:      Some(goal),
					}
				},
				omp_tools::goal::Operation::Drop => {
					let engagement = active_goal_engagement(&sender)
						.await?
						.ok_or(omp_tools::goal::Fault::Unavailable)?;
					let goal = modes.drop_goal(now).map_err(map_goal_error)?;
					sender
						.disengage_campaign(engagement)
						.await
						.map_err(map_goal_campaign_error)?;
					crate::goal::GoalOutcome {
						lifecycle: crate::goal::GoalLifecycle::Dropped,
						goal:      Some(goal),
					}
				},
			};
			Ok(outcome.goal.map(project_goal))
		}
	}
}

async fn update_goal_campaign_state(
	sender: &omp_agent::ControlSender,
	engagement: &Str,
	goal: &crate::modes::Goal,
) -> Result<(), omp_tools::goal::Fault> {
	let state = omp_agent::GoalCampaignState {
		objective:          goal.objective.clone(),
		budget_tokens:      goal.token_budget,
		spent_tokens:       goal.tokens_used,
		thresholds_crossed: 0,
	};
	let payload = bytes::Bytes::from(
		serde_json::to_vec(&state).expect("goal campaign state has infallible JSON serialization"),
	);
	sender
		.update_campaign_state(engagement.clone(), payload)
		.await
		.map_err(map_goal_campaign_error)?;
	Ok(())
}

async fn active_goal_engagement(
	sender: &omp_agent::ControlSender,
) -> Result<Option<Str>, omp_tools::goal::Fault> {
	Ok(sender
		.active_campaigns()
		.await
		.map_err(map_goal_campaign_error)?
		.into_iter()
		.find(|entry| {
			entry.spec_id.as_str() == "goal"
				&& entry.status == omp_agent::CampaignEntryStatus::Engaged
		})
		.map(|entry| entry.engagement))
}

async fn ensure_goal_campaign(
	sender: &omp_agent::ControlSender,
) -> Result<(Str, bool), omp_tools::goal::Fault> {
	if let Some(engagement) = active_goal_engagement(sender).await? {
		return Ok((engagement, false));
	}
	let receipt = sender
		.engage_regime("goal", false)
		.await
		.map_err(map_goal_campaign_error)?;
	Ok((receipt.engagement, true))
}

fn map_goal_campaign_error(
	error: omp_agent::control::ControlError,
) -> omp_tools::goal::Fault {
	match error {
		omp_agent::control::ControlError::CampaignEngage(omp_agent::EngageError::Claim {
			outcome: omp_agent::ClaimOutcome::Denied { holder, since },
			..
		}) => omp_tools::goal::Fault::ClaimDenied { holder, since },
		_ => omp_tools::goal::Fault::Unavailable,
	}
}

fn map_goal_error(error: crate::modes::RegimeError) -> omp_tools::goal::Fault {
	match error {
		crate::modes::RegimeError::NoGoal => omp_tools::goal::Fault::NoGoal,
		crate::modes::RegimeError::EmptyObjective => omp_tools::goal::Fault::ObjectiveRequired,
		crate::modes::RegimeError::InvalidBudget => omp_tools::goal::Fault::InvalidBudget,
		crate::modes::RegimeError::CampaignInactive { .. }
		| crate::modes::RegimeError::InvalidPlanArtifact => omp_tools::goal::Fault::ModeConflict,
		crate::modes::RegimeError::InvalidGoalTransition { .. }
		| crate::modes::RegimeError::GoalExists => omp_tools::goal::Fault::InvalidTransition,
	}
}

fn project_goal(goal: crate::modes::Goal) -> omp_tools::goal::Goal {
	let status = match goal.status {
		crate::modes::GoalStatus::Active => omp_tools::goal::Status::Active,
		crate::modes::GoalStatus::Paused => omp_tools::goal::Status::Paused,
		crate::modes::GoalStatus::BudgetLimited => omp_tools::goal::Status::BudgetLimited,
		crate::modes::GoalStatus::Complete => omp_tools::goal::Status::Complete,
		crate::modes::GoalStatus::Dropped => omp_tools::goal::Status::Dropped,
	};
	omp_tools::goal::Goal {
		id: goal.id,
		objective: goal.objective,
		status,
		token_budget: goal.token_budget,
		tokens_used: goal.tokens_used,
		time_used_secs: goal.time_used_seconds,
	}
}

#[derive(Clone)]
struct CheckpointBinding {
	id:     u64,
	sender: omp_agent::ControlSender,
}

/// Late-bound bridge from environment-owned checkpoint tools to the active
/// Agent CONTROL mailbox.
#[derive(Clone, Default)]
pub struct AgentCheckpointControl {
	sender: Arc<RwLock<Option<CheckpointBinding>>>,
}

impl AgentCheckpointControl {
	/// Replaces the active session binding.
	pub fn bind(&self, id: u64, sender: omp_agent::ControlSender) {
		*self.sender.write() = Some(CheckpointBinding { id, sender });
	}

	/// Releases the binding only when it is still owned by `id`.
	pub fn unbind(&self, id: u64) {
		let mut binding = self.sender.write();
		if binding.as_ref().is_some_and(|binding| binding.id == id) {
			*binding = None;
		}
	}

	fn sender(&self) -> Result<omp_agent::ControlSender, omp_tools::checkpoint::CheckpointFault> {
		self
			.sender
			.read()
			.as_ref()
			.map(|binding| binding.sender.clone())
			.ok_or_else(|| omp_tools::checkpoint::CheckpointFault {
				code:    omp_tools::checkpoint::FaultCode::Control,
				message: sf!("active Agent CONTROL is not bound"),
			})
	}
}

impl omp_tools::checkpoint::CheckpointControl for AgentCheckpointControl {
	async fn checkpoint(
		&self,
		goal: Str,
	) -> Result<omp_tools::checkpoint::CheckpointAck, omp_tools::checkpoint::CheckpointFault> {
		let ack = self
			.sender()?
			.checkpoint(goal)
			.await
			.map_err(checkpoint_fault)?;
		Ok(omp_tools::checkpoint::CheckpointAck { token: ack.token, started_at: ack.started_at })
	}

	async fn schedule_rewind(
		&self,
		token: Str,
		report: Str,
	) -> Result<omp_tools::checkpoint::RewindAck, omp_tools::checkpoint::CheckpointFault> {
		let ack = self
			.sender()?
			.schedule_rewind(token, report)
			.await
			.map_err(checkpoint_fault)?;
		Ok(omp_tools::checkpoint::RewindAck { token: ack.token, receipt: ack.receipt })
	}
}

fn checkpoint_fault(
	error: omp_agent::control::ControlError,
) -> omp_tools::checkpoint::CheckpointFault {
	let (code, message) = match error {
		omp_agent::control::ControlError::CheckpointAlreadyActive => {
			(omp_tools::checkpoint::FaultCode::AlreadyActive, sf!("checkpoint already active"))
		},
		omp_agent::control::ControlError::NoActiveCheckpoint => (
			omp_tools::checkpoint::FaultCode::NoActive,
			sf!("no active checkpoint; create a checkpoint before calling rewind"),
		),
		omp_agent::control::ControlError::CheckpointAlreadyCompleted => (
			omp_tools::checkpoint::FaultCode::AlreadyCompleted,
			sf!("checkpoint already completed; continue from the retained rewind report"),
		),
		omp_agent::control::ControlError::WrongCheckpointToken => (
			omp_tools::checkpoint::FaultCode::WrongToken,
			sf!("checkpoint token does not belong to the active session"),
		),
		omp_agent::control::ControlError::EmptyRewindReport => {
			(omp_tools::checkpoint::FaultCode::EmptyReport, sf!("rewind report must not be empty"))
		},
		omp_agent::control::ControlError::RewindAlreadyScheduled => (
			omp_tools::checkpoint::FaultCode::AlreadyScheduled,
			sf!("rewind already scheduled for the active checkpoint"),
		),
		omp_agent::control::ControlError::Closed
		| omp_agent::control::ControlError::Journal(_)
		| omp_agent::control::ControlError::CampaignEngage(_)
		| omp_agent::control::ControlError::CampaignDisengage(_)
		| omp_agent::control::ControlError::CampaignArbiter(_)
		| omp_agent::control::ControlError::UnknownCoreCampaign { .. } => (
			omp_tools::checkpoint::FaultCode::Control,
			sf!("active Agent CONTROL checkpoint operation failed"),
		),
	};
	omp_tools::checkpoint::CheckpointFault { code, message }
}

pub(super) fn python_engine() -> Result<Arc<omp_py::Engine>, EnvdError> {
	static ENGINE: LazyLock<Result<Arc<omp_py::Engine>, Str>> = LazyLock::new(|| {
		omp_py::Engine::builder()
			.init()
			.map(Arc::new)
			.map_err(|error| Str::from(error.to_string()))
	});
	ENGINE
		.as_ref()
		.map(Arc::clone)
		.map_err(|error| EnvdError::Eval(error.clone()))
}

fn preflight_python_eval(
	host: Arc<SessionBridgeHost>,
	interrupt_grace: Duration,
	blobs: BlobHost,
) -> Result<ProcessEvalExec, EnvdError> {
	python_engine()?;
	ProcessEvalExec::production(host, interrupt_grace, blobs)
		.map_err(|error| EnvdError::Eval(Str::from(error.to_string())))
}

fn ensure_name_absent(registry: &Registry, name: &str) -> Result<(), EnvdError> {
	if registry.live_identity(name).is_some() {
		return Err(EnvdError::DuplicateToolName(Str::from(name)));
	}
	Ok(())
}

const fn core_claims() -> Claims {
	Claims { precedence: Precedence::CORE, claimant: sf!("omp/core"), replaces: None }
}

const fn builtin_device_claims() -> Claims {
	Claims { precedence: Precedence::ENHANCEMENT, claimant: sf!("omp/core"), replaces: None }
}

fn shell_timeout_bounds(settings: &ToolSettings) -> omp_tools::shell::TimeoutBounds {
	let mut bounds = omp_tools::shell::TimeoutBounds::default();
	let Some(maximum) = settings.max_timeout else {
		return bounds;
	};
	let milliseconds = maximum
		.to_std()
		.ok()
		.and_then(|duration| u64::try_from(duration.as_millis()).ok())
		.unwrap_or(bounds.ceiling_ms);
	bounds.ceiling_ms = milliseconds.max(bounds.floor_ms).min(bounds.ceiling_ms);
	bounds.default_ms = bounds
		.default_ms
		.min(bounds.ceiling_ms)
		.max(bounds.floor_ms);
	bounds
}

fn worker_spec(declaration: &ToolDecl) -> Result<ToolSpec, EnvdError> {
	let definition = declaration.definition.as_ref().ok_or_else(|| {
		EnvdError::WorkerDeclaration(sf!("worker tool declaration has no definition"))
	})?;
	if declaration.extension_id.is_empty() {
		return Err(worker_declaration_error("worker tool declaration has no extension id"));
	}
	Ok(ToolSpec {
		name:            Str::from(definition.name.as_str()),
		rev:             declaration
			.rev
			.parse::<Rev>()
			.map_err(|error| EnvdError::WorkerDeclaration(Str::from(error.to_string())))?,
		description:     Str::from(definition.description.as_str()),
		schema:          definition.schema_json.clone(),
		constraint:      worker_constraint(declaration)?,
		projection_code: worker_projection_code(declaration),
		effects:         declaration
			.effects
			.as_ref()
			.map(omp_tool::Effects::try_from)
			.transpose()
			.map_err(|error| EnvdError::WorkerDeclaration(Str::from(error.to_string())))?
			.unwrap_or_default(),
	})
}

fn worker_projection_code(declaration: &ToolDecl) -> [u8; 32] {
	let mut hasher = Hash32::hasher();
	hasher.update(b"omp/frozen-worker-registration/v1");
	hasher.update(declaration.encode_to_vec());
	hasher.finalize().into_bytes()
}

fn worker_constraint(declaration: &ToolDecl) -> Result<Constraint, EnvdError> {
	let Some(kind) = declaration
		.constraint
		.as_ref()
		.and_then(|value| value.kind.as_ref())
	else {
		let strict = declaration
			.definition
			.as_ref()
			.and_then(|definition| definition.strict)
			.unwrap_or(false);
		return Ok(if strict {
			Constraint::Schema {
				priority:       100,
				on_unsupported: omp_proto::inference::v1::Fallback::Unspecified,
			}
		} else {
			Constraint::None
		});
	};
	match kind {
		tool_constraint::Kind::Schema(schema) => Ok(Constraint::Schema {
			priority:       constraint_priority(schema.priority)?,
			on_unsupported: omp_proto::inference::v1::Fallback::Unspecified,
		}),
		tool_constraint::Kind::Grammar(grammar) => {
			let syntax = match WorkerGrammarSyntax::try_from(grammar.syntax) {
				Ok(WorkerGrammarSyntax::Lark) => GrammarSyntax::Lark,
				Ok(WorkerGrammarSyntax::Regex) => GrammarSyntax::Regex,
				_ => {
					return Err(worker_declaration_error(
						"worker grammar constraint has an unsupported syntax",
					));
				},
			};
			Ok(Constraint::Grammar {
				syntax,
				definition: Str::from(grammar.definition.as_str()),
				priority: constraint_priority(grammar.priority)?,
				on_unsupported: omp_proto::inference::v1::Fallback::Unspecified,
			})
		},
		tool_constraint::Kind::Textual(_) => {
			Err(worker_declaration_error("worker textual constraints are not supported"))
		},
		tool_constraint::Kind::Json(_) => {
			Err(worker_declaration_error("worker JSON constraints are not supported"))
		},
	}
}

fn constraint_priority(priority: u32) -> Result<u8, EnvdError> {
	u8::try_from(priority)
		.map_err(|_| worker_declaration_error("worker constraint priority exceeds u8"))
}

const fn worker_declaration_error(message: &'static str) -> EnvdError {
	EnvdError::WorkerDeclaration(sf!(message))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn goal_create_denial_surfaces_campaign_holder_and_since() {
		let mut campaigns = omp_agent::CampaignStack::new();
		let (plan_spec, plan_machine) = omp_agent::core_regime("plan").expect("plan regime");
		let plan = campaigns
			.engage(
				plan_spec,
				plan_machine,
				omp_agent::EngageOptions { now_ms: 137, queue: false },
			)
			.expect("plan engagement");
		let (goal_spec, goal_machine) = omp_agent::core_regime("goal").expect("goal regime");
		let error = campaigns
			.engage(
				goal_spec,
				goal_machine,
				omp_agent::EngageOptions { now_ms: 211, queue: false },
			)
			.expect_err("plan owns the mode slot");

		let fault = map_goal_campaign_error(error.into());

		assert_eq!(
			fault,
			omp_tools::goal::Fault::ClaimDenied {
				holder: plan.engagement,
				since:  137,
			}
		);
	}
}
