use std::{collections::HashSet, env};

use omp_secrets::rule::{SecretKind, SecretMode, SecretRule};

const MIN_ENV_SECRET_LEN: usize = 8;
const CREDENTIAL_TERMS: &[&str] =
	&["KEY", "SECRET", "TOKEN", "PASSWORD", "PASS", "AUTH", "CREDENTIAL", "PRIVATE", "OAUTH"];

/// Collects credential-shaped process environment values as reversible rules.
///
/// Names and values stay in Core memory and are never included in extension
/// declaration frames.
pub fn collect_env_secret_rules() -> Vec<SecretRule> {
	collect_env_secret_rules_from(env::vars())
}

fn collect_env_secret_rules_from(
	values: impl IntoIterator<Item = (String, String)>,
) -> Vec<SecretRule> {
	let mut seen = HashSet::new();
	values
		.into_iter()
		.filter_map(|(name, value)| {
			(value.len() >= MIN_ENV_SECRET_LEN && credential_name(&name) && seen.insert(value.clone()))
				.then(|| {
					SecretRule::new(SecretKind::Plain, SecretMode::Obfuscate, value, None, None, None)
						.ok()
				})
				.flatten()
		})
		.collect()
}

fn credential_name(name: &str) -> bool {
	let uppercase = name.to_ascii_uppercase();
	CREDENTIAL_TERMS.iter().any(|term| {
		uppercase.match_indices(term).any(|(start, _)| {
			start + term.len() == uppercase.len() || uppercase.as_bytes()[start + term.len()] == b'_'
		})
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn accepts_only_pi_credential_names_and_long_values() {
		let rules = collect_env_secret_rules_from([
			("API_TOKEN".into(), "abcdefgh".into()),
			("NORMAL_NAME".into(), "long-enough-but-not-secret".into()),
			("PASSWORD_FILE".into(), "12345678".into()),
			("AUTH".into(), "short".into()),
			("OTHER".into(), "abcdefgh".into()),
		]);
		assert_eq!(rules.iter().map(SecretRule::content).collect::<Vec<_>>(), [
			"abcdefgh", "12345678"
		]);
	}
}
