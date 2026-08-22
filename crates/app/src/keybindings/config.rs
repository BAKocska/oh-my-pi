//! Canonical TOML keybinding decoding and one-way legacy import.

use std::{
	collections::BTreeMap,
	fs, io,
	path::{Path, PathBuf},
};

use omp_core::Str;
use serde::{Deserialize, Serialize};

use crate::settings::io::atomic_replace;

/// A named keybinding profile with action-to-chord mappings.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct KeybindingProfile {
	/// Optional parent profile.
	pub extends:  Option<Str>,
	/// Canonical action ids and their ordered chords.
	#[serde(default)]
	pub bindings: BTreeMap<Str, Vec<Str>>,
}

/// Canonical `keybindings.toml` document.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct KeybindingsConfig {
	/// Selected named profile.
	pub active:   Option<Str>,
	/// Named profiles.
	#[serde(default)]
	pub profiles: BTreeMap<Str, KeybindingProfile>,
}

/// Origin of a decoded keybinding document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KeybindingsSource {
	/// Canonical native TOML.
	NativeToml(PathBuf),
	/// One-time imported JSON source.
	LegacyJson(PathBuf),
	/// One-time imported YAML source.
	LegacyYaml(PathBuf),
}

/// Decoded config and its explicit source label.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedKeybindings {
	/// Typed config.
	pub config: KeybindingsConfig,
	/// Source used for this load/import.
	pub source: KeybindingsSource,
}

/// Decodes the only live format, native TOML.
pub fn load(path: &Path) -> Result<LoadedKeybindings, KeybindingsConfigError> {
	let source = fs::read_to_string(path)
		.map_err(|source| KeybindingsConfigError::Read { path: path.to_owned(), source })?;
	let config = toml::from_str(&source)
		.map_err(|source| KeybindingsConfigError::Toml { path: path.to_owned(), source })?;
	Ok(LoadedKeybindings { config, source: KeybindingsSource::NativeToml(path.to_owned()) })
}

/// Imports the first existing legacy JSON/YAML source exactly once. This is not
/// a fallback decoder: after import, only `keybindings.toml` is read.
pub fn import_legacy(
	directory: &Path,
) -> Result<Option<LoadedKeybindings>, KeybindingsConfigError> {
	let native = directory.join("keybindings.toml");
	let marker = directory.join(".keybindings-migration-v1");
	if native.exists() || marker.exists() {
		return Ok(None);
	}
	let candidates = [
		("keybindings.json", LegacyKind::Json),
		("keybindings.yml", LegacyKind::Yaml),
		("keybindings.yaml", LegacyKind::Yaml),
	];
	let Some((path, kind)) = candidates
		.into_iter()
		.map(|(name, kind)| (directory.join(name), kind))
		.find(|(path, _)| path.exists())
	else {
		atomic_replace(&marker, "revision = 1\n")?;
		return Ok(None);
	};
	let source = fs::read_to_string(&path)
		.map_err(|source| KeybindingsConfigError::Read { path: path.clone(), source })?;
	let config = match kind {
		LegacyKind::Json => omp_slopjson::from_str::<KeybindingsConfig>(&source)?,
		LegacyKind::Yaml => serde_yaml::from_str::<KeybindingsConfig>(&source)
			.map_err(|source| KeybindingsConfigError::Yaml { path: path.clone(), source })?,
	};
	atomic_replace(&native, &toml::to_string_pretty(&config)?)?;
	let backup = path.with_file_name(format!(
		"{}.pre-omp-migration.bak",
		path
			.file_name()
			.and_then(|name| name.to_str())
			.unwrap_or("keybindings")
	));
	fs::copy(&path, &backup).map_err(|source| KeybindingsConfigError::Backup {
		path: path.clone(),
		backup,
		source,
	})?;
	let label = match kind {
		LegacyKind::Json => "legacy-json",
		LegacyKind::Yaml => "legacy-yaml",
	};
	atomic_replace(&marker, &format!("revision = 1\nsource = {label:?}\n"))?;
	Ok(Some(LoadedKeybindings {
		config,
		source: match kind {
			LegacyKind::Json => KeybindingsSource::LegacyJson(path),
			LegacyKind::Yaml => KeybindingsSource::LegacyYaml(path),
		},
	}))
}

#[derive(Clone, Copy)]
enum LegacyKind {
	Json,
	Yaml,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn legacy_json_import_has_source_backup_and_native_cutover() {
		let directory = tempfile::tempdir().expect("directory");
		let legacy = directory.path().join("keybindings.json");
		fs::write(
			&legacy,
			"{ active: 'default', profiles: { default: { bindings: { submit: ['enter'], }, }, }, }",
		)
		.expect("legacy");
		let imported = import_legacy(directory.path())
			.expect("import")
			.expect("config");
		assert!(matches!(imported.source, KeybindingsSource::LegacyJson(_)));
		assert_eq!(imported.config.active.as_deref(), Some("default"));
		assert!(
			directory
				.path()
				.join("keybindings.json.pre-omp-migration.bak")
				.exists()
		);
		assert!(import_legacy(directory.path()).expect("second").is_none());
		let native = load(&directory.path().join("keybindings.toml")).expect("native");
		assert!(matches!(native.source, KeybindingsSource::NativeToml(_)));
	}
}

/// Native keybinding configuration failure.
#[derive(Debug, thiserror::Error)]
pub enum KeybindingsConfigError {
	/// Reading a source failed.
	#[error("failed to read keybindings source {path}")]
	Read {
		path:   PathBuf,
		#[source]
		source: io::Error,
	},
	/// Canonical TOML was malformed.
	#[error("failed to parse native keybindings TOML {path}")]
	Toml {
		path:   PathBuf,
		#[source]
		source: toml::de::Error,
	},
	/// Legacy YAML was malformed.
	#[error("failed to parse legacy keybindings YAML {path}")]
	Yaml {
		path:   PathBuf,
		#[source]
		source: serde_yaml::Error,
	},
	/// Legacy JSON/JSONC was malformed.
	#[error(transparent)]
	Json(#[from] omp_slopjson::ParseError),
	/// Native TOML encoding failed.
	#[error(transparent)]
	Encode(#[from] toml::ser::Error),
	/// Atomic persistence failed.
	#[error(transparent)]
	Persist(#[from] crate::settings::io::SettingsIoError),
	/// A legacy source backup failed.
	#[error("failed to back up keybindings source {path} to {backup}")]
	Backup {
		path:   PathBuf,
		backup: PathBuf,
		#[source]
		source: io::Error,
	},
}
