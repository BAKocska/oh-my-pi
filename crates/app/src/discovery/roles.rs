//! Durable project-scoped model-role assignments.

use omp_core::Str;
use omp_llm_catalog::{
	ModelKey, ModelRole, SelectionError, select_model, snapshot::Catalog, upsert_role_assignment,
};
use omp_storage::state::{DurableRequest, Error, StateAuthority, StateScope, StateStore};
use thiserror::Error as ThisError;

const ROLE_KIND: &str = "model-roles";
const ROLE_SCHEMA: &str = "2";
/// Invocation-local resolved model roles after CLI-over-environment precedence.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LaunchRoles {
	/// Primary model when explicitly overridden.
	pub primary:       Option<ModelKey>,
	/// Fast/low-cost model.
	pub smol:          Option<ModelKey>,
	/// Deep-reasoning model.
	pub slow:          Option<ModelKey>,
	/// Planning model.
	pub plan:          Option<ModelKey>,
	/// Planning selector's explicit thinking annotation.
	pub plan_thinking: Option<Str>,
}

/// Resolves role selectors through the catalog authority. CLI values override
/// `OMP_*_MODEL`; unsupported thinking annotations are rejected by catalog
/// selection rather than clamped client-side.
pub fn resolve_launch_roles(
	catalog: &Catalog,
	primary: Option<&str>,
	smol: Option<&str>,
	slow: Option<&str>,
	plan: Option<&str>,
) -> Result<LaunchRoles, SelectionError> {
	let resolve_selected = |cli: Option<&str>, variable: &str| {
		let environment = std::env::var(variable).ok();
		let Some(selector) = cli.or(environment.as_deref()) else {
			return Ok(None);
		};
		select_model(
			catalog.models(),
			catalog.routes(),
			catalog.aliases(),
			&[],
			&Default::default(),
			selector,
		)
		.map(Some)
	};
	let primary = resolve_selected(primary, "OMP_DEFAULT_MODEL")?;
	let smol = resolve_selected(smol, "OMP_SMOL_MODEL")?;
	let slow = resolve_selected(slow, "OMP_SLOW_MODEL")?;
	let plan = resolve_selected(plan, "OMP_PLAN_MODEL")?;
	Ok(LaunchRoles {
		primary:       primary.map(|selected| selected.model),
		smol:          smol.map(|selected| selected.model),
		slow:          slow.map(|selected| selected.model),
		plan_thinking: plan.as_ref().and_then(|selected| selected.thinking.clone()),
		plan:          plan.map(|selected| selected.model),
	})
}
/// Failure while validating or durably saving a role assignment.
#[derive(Debug, ThisError)]
pub enum RolePersistenceError {
	/// The model selector or role id is invalid.
	#[error(transparent)]
	Selection(#[from] SelectionError),
	/// Durable project state could not be loaded or appended.
	#[error(transparent)]
	Storage(#[from] Error),
}

fn load_roles(
	store: &StateStore,
	authority: &StateAuthority,
	scope: StateScope,
) -> Result<Vec<ModelRole>, Error> {
	let Some(entry) = store.latest(authority, scope, authority.namespace(), ROLE_KIND)? else {
		return Ok(Vec::new());
	};
	Ok(serde_json::from_slice(&entry.raw)?)
}

/// Loads the latest project role assignment snapshot for the core namespace.
pub fn load_project_roles(
	store: &StateStore,
	authority: &StateAuthority,
) -> Result<Vec<ModelRole>, Error> {
	load_roles(store, authority, StateScope::Project)
}

/// Loads the latest global/user role assignment snapshot.
pub fn load_global_roles(
	store: &StateStore,
	authority: &StateAuthority,
) -> Result<Vec<ModelRole>, Error> {
	load_roles(store, authority, StateScope::User)
}

/// Merges global and project roles with project records winning per stable role
/// id.
pub fn load_effective_roles(
	store: &StateStore,
	authority: &StateAuthority,
) -> Result<Vec<ModelRole>, Error> {
	let mut roles = load_global_roles(store, authority)?;
	for project in load_project_roles(store, authority)? {
		if let Some(existing) = roles.iter_mut().find(|role| role.id == project.id) {
			*existing = project;
		} else {
			roles.push(project);
		}
	}
	Ok(omp_llm_catalog::known_roles(&roles))
}

/// Appends a complete replacement snapshot for project-scoped role resolution.
///
/// `StateStore` supplies durable ordering and idempotency; callers use the
/// returned request's project authority rather than writing workspace files.
fn save_roles(
	store: &StateStore,
	authority: &StateAuthority,
	scope: StateScope,
	roles: &[ModelRole],
	request: &DurableRequest,
) -> Result<(), Error> {
	let data = serde_json::to_vec(roles)?;
	store.append(authority, scope, ROLE_KIND, ROLE_SCHEMA, &data, request)?;
	Ok(())
}

/// Saves project-scoped roles.
pub fn save_project_roles(
	store: &StateStore,
	authority: &StateAuthority,
	roles: &[ModelRole],
	request: &DurableRequest,
) -> Result<(), Error> {
	save_roles(store, authority, StateScope::Project, roles, request)
}

/// Saves global/user-scoped roles.
pub fn save_global_roles(
	store: &StateStore,
	authority: &StateAuthority,
	roles: &[ModelRole],
	request: &DurableRequest,
) -> Result<(), Error> {
	save_roles(store, authority, StateScope::User, roles, request)
}

/// Validates and durably upserts one project-scoped role assignment.
///
/// The thinking annotation is stored in the role selector itself, including
/// explicit `auto` for non-default roles. This persistence-only boundary has no
/// access to the active session and therefore cannot switch its model.
pub fn save_project_role_assignment(
	store: &StateStore,
	authority: &StateAuthority,
	role: impl Into<Str>,
	selector: &str,
	thinking: Option<&str>,
	request: &DurableRequest,
) -> Result<Vec<ModelRole>, RolePersistenceError> {
	let mut roles = load_project_roles(store, authority)?;
	if upsert_role_assignment(&mut roles, role, selector, thinking)? {
		save_project_roles(store, authority, &roles, request)?;
	}
	Ok(roles)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn explicit_auto_survives_role_snapshot_codec() {
		let roles = vec![
			ModelRole::assignment("default", "openai/primary", Some("high")).expect("default role"),
			ModelRole::assignment("task", "openai-codex/worker", Some("auto")).expect("task role"),
		];
		let encoded = serde_json::to_vec(&roles).expect("encode role snapshot");
		let decoded: Vec<ModelRole> = serde_json::from_slice(&encoded).expect("decode role snapshot");
		assert_eq!(decoded, roles);
		assert_eq!(decoded[1].selectors[0].as_str(), "openai-codex/worker:auto");
	}
}
