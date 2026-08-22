//! Environment-published extension generations.
//!
//! Materialization needs an installer-only Environment connection, so the
//! composition lives beside the CLI driver rather than in the extension domain
//! crate.

use std::path::{Path, PathBuf};

use omp_env::{ClientError, EnvClient};
use omp_ext::{ExtensionError, upgrade::Generation};
use omp_proto::env::v1::{MaterializeSite, SiteMaterialized};
use thiserror::Error;

/// Failure while publishing a verified generation through the Environment
/// site authority.
#[derive(Debug, Error)]
pub enum MaterializedGenerationError {
	/// The verified wheel/site manifest could not be materialized.
	#[error("Environment site materialization failed")]
	Environment(#[from] ClientError),
	/// The durable lock/install generation could not be committed.
	#[error("extension generation commit failed")]
	Generation(#[from] ExtensionError),
}

/// Materializes verified wheel/blob inputs through the installer-only
/// Environment connection, then atomically publishes the corresponding
/// lock/install generation.
///
/// Site trees are immutable and content-addressed, so a later generation-file
/// failure leaves only an unreachable tree eligible for ordinary GC; it never
/// exposes partially updated active extension state.
///
/// # Errors
///
/// Fails when materialization is rejected or the generation cannot be
/// committed.
pub async fn materialize_and_commit_generation(
	client: &EnvClient,
	request: MaterializeSite,
	lock_path: &Path,
	installed_path: &Path,
	generation_root: &Path,
	generation_id: &str,
	generation: &Generation,
) -> Result<(PathBuf, SiteMaterialized), MaterializedGenerationError> {
	generation.lock.validate_for(generation.lock.layer)?;
	let materialized = client.materialize_site(request).await?;
	let committed = omp_ext::upgrade::commit_generation(
		lock_path,
		installed_path,
		generation_root,
		generation_id,
		generation,
	)?;
	Ok((committed, materialized))
}
