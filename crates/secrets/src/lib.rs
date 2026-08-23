//! Secret-rule validation and deterministic masking primitives.
use std::sync::Mutex;

use omp_core::Str;
use thiserror::Error;

/// Streaming placeholder-boundary withholding.
/// Built-in provider credential rules.
pub mod builtins;
/// Recursive model-authored JSON transforms.
pub mod json;
/// Author-sensitive message transform policy.
pub mod message;
/// Atomic bidirectional text transform.
pub mod obfuscator;
/// Keyed reversible placeholder grammar and registration.
pub mod placeholder;
/// Dedicated redaction-only projection.
pub mod redact;
/// Deterministic and regex-safe irreversible replacements.
pub mod replacement;
/// Closed secret declaration contract and regex validation.
pub mod rule;
/// Placeholder-boundary buffering for streamed provider output.
pub mod stream;
/// Origin-aware fixed-point text transforms.
pub mod tracked;

use std::mem;

use obfuscator::SecretObfuscator;
use rule::SecretRule;

/// Maximum number of extension-contributed rules retained by one activation.
pub const MAX_CONTROL_SECRET_RULES: usize = 64;

/// Core-owned, generation-fenced secret declaration and masking boundary.
///
/// Declarations are accepted only from the authenticated owner before the
/// first mask operation. Masking seals the rule set and keeps every reversible
/// placeholder mapping inside Core.
pub struct SecretMaskingAuthority {
	owner:      Str,
	generation: u64,
	key:        String,
	state:      Mutex<SecretMaskingState>,
}

struct SecretMaskingState {
	sealed:    bool,
	rules:     Vec<SecretRule>,
	transform: Option<SecretObfuscator>,
	declared:  usize,
}

impl SecretMaskingAuthority {
	/// Creates one activation-local authority over already trusted base rules.
	pub fn new(
		owner: impl Into<Str>,
		generation: u64,
		base_rules: impl IntoIterator<Item = SecretRule>,
		placeholder_key: impl Into<String>,
	) -> Result<Self, SecretMaskingError> {
		let owner = owner.into();
		if owner.is_empty() {
			return Err(SecretMaskingError::MissingOwner);
		}
		Ok(Self {
			owner,
			generation,
			key: placeholder_key.into(),
			state: Mutex::new(SecretMaskingState {
				sealed:    false,
				rules:     base_rules.into_iter().collect(),
				transform: None,
				declared:  0,
			}),
		})
	}

	/// Appends one validated declaration before this activation is sealed.
	pub fn declare(
		&self,
		owner: &str,
		generation: u64,
		rule: SecretRule,
	) -> Result<(), SecretMaskingError> {
		self.validate_owner(owner, generation)?;
		let mut state = self
			.state
			.lock()
			.map_err(|_| SecretMaskingError::Unavailable)?;
		if state.sealed {
			return Err(SecretMaskingError::Sealed);
		}
		if state.declared >= MAX_CONTROL_SECRET_RULES {
			return Err(SecretMaskingError::TooManyRules);
		}
		state.rules.push(rule);
		state.declared += 1;
		Ok(())
	}

	/// Seals declarations and returns Core's masked projection.
	pub fn mask(
		&self,
		owner: &str,
		generation: u64,
		text: &str,
	) -> Result<String, SecretMaskingError> {
		self.validate_owner(owner, generation)?;
		let mut state = self
			.state
			.lock()
			.map_err(|_| SecretMaskingError::Unavailable)?;
		if !state.sealed {
			let rules = mem::take(&mut state.rules);
			state.transform = Some(SecretObfuscator::new(rules, self.key.clone()));
			state.sealed = true;
		}
		Ok(state
			.transform
			.as_mut()
			.expect("sealed secret authority has a transform")
			.obfuscate(text))
	}

	/// Returns whether this activation's declarations are immutable.
	pub fn is_sealed(&self) -> Result<bool, SecretMaskingError> {
		Ok(self
			.state
			.lock()
			.map_err(|_| SecretMaskingError::Unavailable)?
			.sealed)
	}

	fn validate_owner(&self, owner: &str, generation: u64) -> Result<(), SecretMaskingError> {
		if owner != self.owner {
			return Err(SecretMaskingError::OwnerMismatch);
		}
		if generation != self.generation {
			return Err(SecretMaskingError::GenerationMismatch {
				expected: self.generation,
				actual:   generation,
			});
		}
		Ok(())
	}
}

/// Secret declaration and masking refusal without secret-bearing diagnostics.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SecretMaskingError {
	/// Construction omitted the authenticated extension identity.
	#[error("secret masking owner is missing")]
	MissingOwner,
	/// A different extension attempted to use the authority.
	#[error("secret masking owner does not match")]
	OwnerMismatch,
	/// A replaced host generation attempted to use the authority.
	#[error("secret masking generation {actual} does not match {expected}")]
	GenerationMismatch {
		/// Bound activation generation.
		expected: u64,
		/// Presented activation generation.
		actual:   u64,
	},
	/// Declarations were attempted after first use sealed the snapshot.
	#[error("secret declarations are sealed")]
	Sealed,
	/// The activation exceeded its bounded declaration budget.
	#[error("secret declaration limit exceeded")]
	TooManyRules,
	/// The masking state lock was poisoned.
	#[error("secret masking authority is unavailable")]
	Unavailable,
}
