//! Durable project-scoped model-role assignments.

use omp_core::Str;
use omp_llm_catalog::{ModelRole, SelectionError, upsert_role_assignment};
use omp_storage::state::{DurableRequest, Error, StateAuthority, StateScope, StateStore};
use thiserror::Error as ThisError;

const ROLE_KIND: &str = "model-roles";
const ROLE_SCHEMA: &str = "1";
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

/// Loads the latest project role assignment snapshot for the core namespace.
pub fn load_project_roles(
	store: &StateStore,
	authority: &StateAuthority,
) -> Result<Vec<ModelRole>, Error> {
	let Some(entry) =
		store.latest(authority, StateScope::Project, authority.namespace(), ROLE_KIND)?
	else {
		return Ok(Vec::new());
	};
	Ok(serde_json::from_slice(&entry.raw)?)
}

/// Appends a complete replacement snapshot for project-scoped role resolution.
/// `StateStore` supplies durable ordering and idempotency; callers use the
/// returned request's project authority rather than writing workspace files.
pub fn save_project_roles(
	store: &StateStore,
	authority: &StateAuthority,
	roles: &[ModelRole],
	request: &DurableRequest,
) -> Result<(), Error> {
	let data = serde_json::to_vec(roles)?;
	store.append(authority, StateScope::Project, ROLE_KIND, ROLE_SCHEMA, &data, request)?;
	Ok(())
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
