use omp_core::{SecretString, Str, base64_url};
use ring::rand::{SecureRandom as _, SystemRandom};
use sha2::{Digest as _, Sha256};
use zeroize::Zeroizing;

/// PKCE S256 verifier, challenge, and CSRF state for one authorization attempt.
pub struct PkceMaterial {
	verifier:  SecretString,
	challenge: Str,
	state:     Str,
}

impl PkceMaterial {
	/// Borrows the public S256 challenge.
	#[must_use]
	pub fn challenge(&self) -> &str {
		self.challenge.as_str()
	}

	/// Borrows the public authorization state.
	#[must_use]
	pub fn state(&self) -> &str {
		self.state.as_str()
	}

	/// Consumes the material, retaining the verifier as a secret.
	#[must_use]
	pub fn into_parts(self) -> (SecretString, Str, Str) {
		(self.verifier, self.challenge, self.state)
	}
}

impl std::fmt::Debug for PkceMaterial {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter
			.debug_struct("PkceMaterial")
			.field("verifier", &"[REDACTED]")
			.field("challenge", &self.challenge)
			.field("state", &"[REDACTED]")
			.finish()
	}
}

/// Operating-system cryptographic entropy source.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemEntropy;

impl SystemEntropy {
	/// Fills a destination with operating-system cryptographic randomness.
	pub fn fill(self, destination: &mut [u8]) -> Result<(), EntropyError> {
		SystemRandom::new()
			.fill(destination)
			.map_err(|_| EntropyError)
	}
}

/// Cryptographic entropy was unavailable.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("cryptographic entropy is unavailable")]
pub struct EntropyError;

/// Generates RFC 7636 S256 material using an injected entropy function.
///
/// Injection keeps provider clients deterministic in unit tests without moving
/// credential ownership into this crate.
pub fn generate_pkce<E>(
	mut fill: impl FnMut(&mut [u8]) -> Result<(), E>,
) -> Result<PkceMaterial, E> {
	let mut verifier_bytes = Zeroizing::new([0_u8; 32]);
	let mut state_bytes = Zeroizing::new([0_u8; 24]);
	fill(&mut verifier_bytes[..])?;
	fill(&mut state_bytes[..])?;
	let verifier = SecretString::from(base64_url::encode_raw(&verifier_bytes[..]).into_string());
	let challenge = Str::from(
		base64_url::encode_raw(&Sha256::digest(verifier.expose_secret().as_bytes())).into_string(),
	);
	let state = Str::from(base64_url::encode_raw(&state_bytes[..]).into_string());
	Ok(PkceMaterial { verifier, challenge, state })
}

use omp_core::ExposeSecret as _;

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn pkce_is_deterministic_with_injected_entropy() {
		let mut next = 0_u8;
		let material = generate_pkce::<std::convert::Infallible>(|bytes| {
			for byte in bytes {
				*byte = next;
				next = next.wrapping_add(1);
			}
			Ok(())
		})
		.expect("infallible entropy");
		assert_eq!(material.state().len(), 32);
		assert_eq!(material.challenge().len(), 43);
		let (verifier, ..) = material.into_parts();
		assert_eq!(verifier.expose_secret().len(), 43);
	}
}
