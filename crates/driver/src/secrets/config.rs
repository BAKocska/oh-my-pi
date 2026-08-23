use std::{
	fs, io,
	path::{Path, PathBuf},
	str::FromStr as _,
};

use omp_core::Str;
use omp_secrets::rule::{SecretKind, SecretMode, SecretRule, SecretRuleError};
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawRule {
	#[serde(rename = "type")]
	kind:          String,
	content:       String,
	#[serde(default)]
	mode:          Option<String>,
	#[serde(default)]
	replacement:   Option<String>,
	#[serde(default)]
	flags:         Option<String>,
	#[serde(default)]
	friendly_name: Option<String>,
}

/// Failure to load or validate a secret configuration file.
#[derive(Debug, Error)]
pub enum SecretConfigError {
	/// Reading a present configuration file failed.
	#[error("failed to read secret configuration `{path}`")]
	Read {
		/// Configuration path.
		path:   PathBuf,
		/// Filesystem failure.
		#[source]
		source: io::Error,
	},
	/// YAML syntax or shape is invalid.
	#[error("invalid secret configuration YAML in `{path}`")]
	Yaml {
		/// Configuration path.
		path:   PathBuf,
		/// YAML decoding failure.
		#[source]
		source: serde_yaml::Error,
	},
	/// An entry contains an unknown kind.
	#[error("secret configuration `{path}` entry {index} has unknown kind `{value}`")]
	Kind {
		/// Configuration file containing the invalid declaration.
		path:  PathBuf,
		/// Zero-based position of the declaration in the YAML sequence.
		index: usize,
		/// Unsupported rule-kind token, not the configured secret content.
		value: Str,
	},
	/// An entry contains an unknown mode.
	#[error("secret configuration `{path}` entry {index} has unknown mode `{value}`")]
	Mode {
		/// Configuration file containing the invalid declaration.
		path:  PathBuf,
		/// Zero-based position of the declaration in the YAML sequence.
		index: usize,
		/// Unsupported masking-mode token, not the configured secret content.
		value: Str,
	},
	/// Core rule validation failed.
	#[error("secret configuration `{path}` entry {index} is invalid")]
	Rule {
		/// Configuration file containing the rejected rule.
		path:   PathBuf,
		/// Zero-based position of the rule in the YAML sequence.
		index:  usize,
		/// Validation failure whose public display omits configured secret
		/// content.
		#[source]
		source: SecretRuleError,
	},
}

/// Loads global rules first and project rules second, with project declarations
/// overriding global declarations that have identical content.
/// Loads the native global and project-local `secrets.yml` files.
pub fn load_for_project(
	project_root: &Path,
	agent_dir: &Path,
) -> Result<Vec<SecretRule>, SecretConfigError> {
	load_secret_rules(&agent_dir.join("secrets.yml"), &project_root.join(".omp").join("secrets.yml"))
}

/// Loads explicit global and project files, with project declarations
/// overriding global declarations that have identical content.
pub fn load_secret_rules(
	global: &Path,
	project: &Path,
) -> Result<Vec<SecretRule>, SecretConfigError> {
	let mut global_rules = load_file(global)?;
	let project_rules = load_file(project)?;
	if project_rules.is_empty() {
		return Ok(global_rules);
	}
	global_rules.retain(|rule| {
		!project_rules
			.iter()
			.any(|project| project.content() == rule.content())
	});
	global_rules.extend(project_rules);
	Ok(global_rules)
}

fn load_file(path: &Path) -> Result<Vec<SecretRule>, SecretConfigError> {
	let text = match fs::read_to_string(path) {
		Ok(text) => text,
		Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
		Err(source) => return Err(SecretConfigError::Read { path: path.to_owned(), source }),
	};
	let raw: Vec<RawRule> = serde_yaml::from_str(&text)
		.map_err(|source| SecretConfigError::Yaml { path: path.to_owned(), source })?;
	raw.into_iter()
		.enumerate()
		.map(|(index, raw)| {
			let kind = SecretKind::from_str(&raw.kind).map_err(|_| SecretConfigError::Kind {
				path: path.to_owned(),
				index,
				value: Str::new(raw.kind),
			})?;
			let mode_value = raw.mode.as_deref().unwrap_or("obfuscate");
			let mode = SecretMode::from_str(mode_value).map_err(|_| SecretConfigError::Mode {
				path: path.to_owned(),
				index,
				value: Str::new(mode_value),
			})?;
			SecretRule::new(
				kind,
				mode,
				raw.content,
				raw.replacement.map(Str::new),
				raw.flags.as_deref(),
				raw.friendly_name.map(Str::new),
			)
			.map_err(|source| SecretConfigError::Rule { path: path.to_owned(), index, source })
		})
		.collect()
}

#[cfg(test)]
mod tests {
	use std::fs;

	use super::*;

	#[test]
	fn project_content_overrides_global() {
		let root = tempfile::tempdir().expect("tempdir");
		let global = root.path().join("global.yml");
		let project = root.path().join("project.yml");
		fs::write(
			&global,
			"- type: plain\n  content: global-secret\n- type: plain\n  content: shared-secret\n",
		)
		.expect("global");
		fs::write(&project, "- type: plain\n  content: shared-secret\n  friendlyName: project\n")
			.expect("project");
		let rules = load_secret_rules(&global, &project).expect("rules");
		assert_eq!(rules.iter().map(SecretRule::content).collect::<Vec<_>>(), [
			"global-secret",
			"shared-secret"
		]);
		assert_eq!(rules[1].friendly_name(), Some("project"));
	}
}
