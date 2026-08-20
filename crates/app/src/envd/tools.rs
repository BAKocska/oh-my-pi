//! Production built-in tool registry assembly.

use std::sync::Arc;
#[cfg(test)]
use std::sync::LazyLock;

use omp_core::{Duration, Str};
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
	docs::DocumentHost,
	eval::{ProcessEvalExec, SessionBridgeHost},
	exec::ExecHost,
	media_devices,
	tool_read_sources::ReadSourceAdapter,
	tool_search::WorkspaceSearchAdapter,
	tool_shell::ShellExecHost,
	tool_url::production_url_resolvers,
	worker::ExtHostSupervisor,
	workspace::WorkspaceHost,
};
use crate::settings::ToolSettings;

/// Builds the complete registry shared by environment dispatch and the agent.
///
/// Resource adapters are cloned into their typed executors. Worker declarations
/// occupy device presentation entries and explicit worker routes; only the
/// environment's worker supervisor can invoke them.
pub fn production_registry<I: omp_tools::device::DeviceInvoker + 'static>(
	documents: &DocumentHost,
	blobs: &BlobHost,
	exec: &ExecHost,
	workspace: &WorkspaceHost,
	telemetry: &Arc<omp_storage::telemetry_index::TelemetryIndex>,
	root_uri: &Str,
	workers: &ExtHostSupervisor,
	interrupt_grace: Duration,
	tool_settings: &ToolSettings,
	device_invoker: I,
	policy: ToolsPolicy,
	mut registry: Registry,
) -> Result<
	(
		Arc<Registry>,
		Arc<SessionBridgeHost>,
		omp_tools::eval::EvalSessionControl,
		AgentCheckpointControl,
	),
	EnvdError,
> {
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
		"think",
		"yield",
		"checkpoint",
		"rewind",
		"hub",
		"image_gen",
		"tts",
		"report_issue",
		"vibe",
		"dyn",
	] {
		ensure_name_absent(&registry, name)?;
	}
	for device in [media_devices::image_gen(), media_devices::tts()] {
		registry.register(device, Presentation::Device, builtin_device_claims())?;
	}
	registry.register(
		media_devices::report_issue(Arc::clone(telemetry)),
		Presentation::Device,
		builtin_device_claims(),
	)?;
	registry.register(crate::vibe::tool(), Presentation::Device, builtin_device_claims())?;
	let checkpoint_control = AgentCheckpointControl::default();
	let read_sources = ReadSourceAdapter::new(documents.clone(), workspace.clone());
	let conflicts = Arc::new(ConflictRegistry::default());
	let resolvers = production_url_resolvers(Arc::clone(&conflicts));
	let read = omp_tools::read::tool_with_resolvers_and_conflicts(
		read_sources.clone(),
		blobs.clone(),
		resolvers,
		Arc::clone(&conflicts),
	);
	let read_identity = read.spec().identity();
	if tool_settings.enabled("read") {
		registry.register(read, Presentation::Slot, core_claims())?;
	}
	let fetch = omp_tools::fetch::tool(read_sources);
	if tool_settings.enabled("fetch") {
		registry.register(fetch, Presentation::Slot, core_claims())?;
	}
	let edit_pin = tool_settings
		.edit_dialect
		.as_deref()
		.map(str::parse::<Rev>)
		.transpose()
		.map_err(|error| EnvdError::EditDialect(error.to_string().into()))?;
	let environment_edit_dialect = std::env::var("OMP_EDIT_DIALECT").ok();
	let selected_edit = resolve_edit_revision(EditRevisionCandidates {
		environment: environment_edit_dialect.as_deref(),
		pin: edit_pin.as_ref(),
		..EditRevisionCandidates::default()
	})
	.map_err(EnvdError::EditDialect)?
	.revision;
	let replace_edit = omp_tools::edit::replace_tool(documents.clone(), FormatPolicy::Configured);
	let replace_identity = replace_edit.spec().identity();
	let edit = omp_tools::edit::tool(documents.clone(), FormatPolicy::Configured);
	let hashline_identity = edit.spec().identity();
	let edit_identity = if selected_edit == replace_identity.rev {
		replace_identity.clone()
	} else {
		hashline_identity.clone()
	};
	if tool_settings.enabled("edit") {
		if selected_edit == replace_identity.rev {
			registry.register(edit, Presentation::Slot, core_claims())?;
			registry.register(replace_edit, Presentation::Slot, core_claims())?;
		} else {
			registry.register(replace_edit, Presentation::Slot, core_claims())?;
			registry.register(edit, Presentation::Slot, core_claims())?;
		}
	}
	let write = omp_tools::write::tool_with_conflicts(documents.clone(), conflicts);
	let write_identity = write.spec().identity();
	if tool_settings.enabled("write") {
		registry.register(write, Presentation::Slot, core_claims())?;
	}
	let shell = omp_tools::shell::shell_with_timeout_bounds(
		ShellExecHost::new(exec.clone(), root_uri.clone()),
		shell_timeout_bounds(tool_settings),
	);
	let shell_identity = shell.spec().identity();
	if tool_settings.enabled("shell") {
		registry.register(shell, Presentation::Slot, core_claims())?;
	}
	let search = WorkspaceSearchAdapter::new(workspace.clone(), documents.clone());
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
	let eval_host = Arc::new(SessionBridgeHost::new());
	let eval_exec =
		ProcessEvalExec::production(Arc::clone(&eval_host), interrupt_grace, blobs.clone())
			.map_err(|error| EnvdError::Eval(Str::from(error.to_string())))?;
	let (eval_tool, eval_control) = omp_tools::eval::eval_controlled(eval_exec);
	let eval_identity = eval_tool.spec().identity();
	if tool_settings.enabled("eval") {
		registry.register(eval_tool, Presentation::Slot, core_claims())?;
	}
	if tool_settings.enabled("todo") {
		registry.register(omp_tools::todo::tool(), Presentation::Slot, core_claims())?;
	}
	if tool_settings.enabled("ask") {
		registry.register(
			omp_tools::ask::tool(omp_chat_ui::ask::presenter()),
			Presentation::Slot,
			core_claims(),
		)?;
	}
	if tool_settings.enabled("think") {
		registry.register(omp_tools::think::tool(), Presentation::Slot, core_claims())?;
	}
	if tool_settings.enabled("hub") {
		registry.register(crate::chat::chat_hub_tool(), Presentation::Slot, core_claims())?;
	}
	if tool_settings.enabled("yield") {
		registry.register(omp_tools::yield_tool::tool(), Presentation::Slot, core_claims())?;
	}
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
	register_builtin_renderers(registry.render_registry_mut(), BuiltinRendererIdentities {
		edit:  tool_settings.enabled("edit").then_some(edit_identity),
		grep:  tool_settings.enabled("grep").then_some(grep_identity),
		glob:  tool_settings.enabled("glob").then_some(glob_identity),
		shell: tool_settings.enabled("shell").then_some(shell_identity),
		write: tool_settings.enabled("write").then_some(write_identity),
		read:  tool_settings.enabled("read").then_some(read_identity),
		eval:  tool_settings.enabled("eval").then_some(eval_identity),
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
	catalog.bind(Arc::clone(&registry)).map_err(|_| {
		EnvdError::WorkerDeclaration(Str::new_static("dynamic device catalog bound twice"))
	})?;
	eval_host
		.bind_registry(Arc::clone(&registry))
		.map_err(|error| EnvdError::Eval(Str::from(error.to_string())))?;
	Ok((registry, eval_host, eval_control, checkpoint_control))
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

	fn sender(&self) -> Result<omp_agent::ControlSender, Str> {
		self
			.sender
			.read()
			.as_ref()
			.map(|binding| binding.sender.clone())
			.ok_or_else(|| Str::new_static("active Agent CONTROL is not bound"))
	}
}

impl omp_tools::checkpoint::CheckpointControl for AgentCheckpointControl {
	async fn checkpoint(&self, label: Str) -> Result<u64, Str> {
		self
			.sender()?
			.checkpoint(label)
			.await
			.map_err(|error| Str::from(error.to_string()))
	}

	async fn schedule_rewind(
		&self,
		target: u64,
		scope: Str,
	) -> Result<omp_tools::checkpoint::RewindAck, Str> {
		let ack = self
			.sender()?
			.schedule_rewind(target, scope)
			.await
			.map_err(|error| Str::from(error.to_string()))?;
		Ok(omp_tools::checkpoint::RewindAck { target: ack.target, receipt: ack.receipt })
	}
}

#[cfg(test)]
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

fn ensure_name_absent(registry: &Registry, name: &str) -> Result<(), EnvdError> {
	if registry.live_identity(name).is_some() {
		return Err(EnvdError::DuplicateToolName(Str::from(name)));
	}
	Ok(())
}

const fn core_claims() -> Claims {
	Claims {
		precedence: Precedence::CORE,
		claimant:   Str::new_static("omp/core"),
		replaces:   None,
	}
}

const fn builtin_device_claims() -> Claims {
	Claims {
		precedence: Precedence::ENHANCEMENT,
		claimant:   Str::new_static("omp/core"),
		replaces:   None,
	}
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
		EnvdError::WorkerDeclaration(Str::new_static("worker tool declaration has no definition"))
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
	let mut hasher = blake3::Hasher::new();
	hasher.update(b"omp/frozen-worker-registration/v1");
	hasher.update(&declaration.encode_to_vec());
	*hasher.finalize().as_bytes()
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
	EnvdError::WorkerDeclaration(Str::new_static(message))
}
