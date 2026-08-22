//! Process-level secret policy and composition.

/// Global/project rule loading.
pub mod config;
/// Credential-shaped environment collection.
pub mod env;
pub mod key;
/// Immutable per-session snapshot composition.
use std::{collections::BTreeMap, sync::Arc};

use omp_core::Str;
use omp_inference::auth::AuthControlHandle;
use omp_secrets::SecretMaskingAuthority;

pub mod session;
/// Builds one Core-owned masking authority over the immutable session rules.
pub fn core_secret_masking_authority(
	snapshot: &session::SecretSessionSnapshot,
	extension: impl Into<Str>,
	host_generation: u64,
) -> Result<Arc<SecretMaskingAuthority>, omp_secrets::SecretMaskingError> {
	SecretMaskingAuthority::new(
		extension,
		host_generation,
		snapshot.rules().iter().cloned(),
		key::placeholder_key(),
	)
	.map(Arc::new)
}

/// Composes the live auth handle and Core masking snapshot into the CONTROL
/// domain factory consumed by Environment authority wiring.
pub fn credential_secret_control_factory(
	control: AuthControlHandle,
	grants: BTreeMap<Str, crate::auth_backend::CredentialControlGrant>,
	snapshot: &session::SecretSessionSnapshot,
) -> crate::auth_backend::CredentialSecretControlFactory {
	crate::auth_backend::CredentialSecretControlFactory::new(
		control,
		grants,
		Arc::from(snapshot.rules().to_vec()),
		Arc::<str>::from(key::placeholder_key()),
	)
}
