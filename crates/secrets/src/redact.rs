//! Irreversible secret projection for export and other leakage boundaries.

use crate::{obfuscator::SecretObfuscator, rule::SecretRule};

/// A one-way secret transform with no placeholder key or restoration registry.
///
/// Reversible declarations are lowered to replacement declarations at
/// construction. The wrapped transform therefore never resolves a placeholder
/// key and cannot restore any emitted value.
pub struct SecretRedactor {
	transform: SecretObfuscator,
}

impl std::fmt::Debug for SecretRedactor {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter
			.debug_struct("SecretRedactor")
			.finish_non_exhaustive()
	}
}

impl SecretRedactor {
	/// Builds a redaction-only projection from one sealed rule snapshot.
	pub fn new(rules: impl IntoIterator<Item = SecretRule>) -> Self {
		let rules = rules
			.into_iter()
			.map(SecretRule::into_irreversible)
			.collect();
		Self { transform: SecretObfuscator::build_irreversible(rules) }
	}

	/// Irreversibly replaces every declared secret match.
	pub fn redact(&mut self, text: &str) -> String {
		self.transform.obfuscate(text)
	}
}
