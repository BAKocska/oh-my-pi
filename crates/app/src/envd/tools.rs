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
	edit::FormatPolicy,
	register_builtin_renderers,
};

use super::{
	EnvdError,
	blobs::BlobHost,
	docs::DocumentHost,
	eval::{ProcessEvalExec, SessionBridgeHost},
	exec::ExecHost,
	tool_read_sources::ReadSourceAdapter,
	tool_search::WorkspaceSearchAdapter,
	tool_shell::ShellExecHost,
	worker::ExtHostSupervisor,
	workspace::WorkspaceHost,
};

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
	root_uri: &Str,
	workers: &ExtHostSupervisor,
	interrupt_grace: Duration,
	device_invoker: I,
	policy: ToolsPolicy,
	mut registry: Registry,
) -> Result<(Arc<Registry>, Arc<SessionBridgeHost>, omp_tools::eval::EvalSessionControl), EnvdError>
{
	for name in ["read", "edit", "shell", "grep", "glob", "write", "eval", "dyn"] {
		ensure_name_absent(&registry, name)?;
	}
	let read_sources = ReadSourceAdapter::new(documents.clone(), workspace.clone());
	let read = omp_tools::read::tool(read_sources, blobs.clone());
	let read_identity = read.spec().identity();
	registry.register(read, Presentation::Slot, core_claims())?;
	let edit = omp_tools::edit::tool(documents.clone(), FormatPolicy::Configured);
	let edit_identity = edit.spec().identity();
	registry.register(edit, Presentation::Slot, core_claims())?;
	let write = omp_tools::write::tool(documents.clone());
	let write_identity = write.spec().identity();
	registry.register(write, Presentation::Slot, core_claims())?;
	let shell = omp_tools::shell::shell(ShellExecHost::new(exec.clone(), root_uri.clone()));
	let shell_identity = shell.spec().identity();
	registry.register(shell, Presentation::Slot, core_claims())?;
	let search = WorkspaceSearchAdapter::new(workspace.clone(), documents.clone());
	let grep = omp_tools::grep::tool(search.clone(), blobs.clone());
	let grep_identity = grep.spec().identity();
	registry.register(grep, Presentation::Slot, core_claims())?;
	let glob = omp_tools::glob::tool(search, blobs.clone());
	let glob_identity = glob.spec().identity();
	registry.register(glob, Presentation::Slot, core_claims())?;
	let eval_host = Arc::new(SessionBridgeHost::new());
	let eval_exec = ProcessEvalExec::production(Arc::clone(&eval_host), interrupt_grace)
		.map_err(|error| EnvdError::Eval(Str::from(error.to_string())))?;
	let (eval_tool, eval_control) = omp_tools::eval::eval_controlled(eval_exec);
	let eval_identity = eval_tool.spec().identity();
	registry.register(eval_tool, Presentation::Slot, core_claims())?;
	let catalog = DeviceCatalog::default();
	if dyn_enabled(policy) {
		registry.register(
			dyn_tool(device_invoker, catalog.clone(), policy),
			Presentation::Slot,
			core_claims(),
		)?;
	}
	register_builtin_renderers(registry.render_registry_mut(), BuiltinRendererIdentities {
		edit:  edit_identity,
		grep:  grep_identity,
		glob:  glob_identity,
		shell: shell_identity,
		write: write_identity,
		read:  read_identity,
		eval:  eval_identity,
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
	Ok((registry, eval_host, eval_control))
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
