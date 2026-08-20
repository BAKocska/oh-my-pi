//! Persisted application settings and layered extension configuration.

use std::{fmt, fs, io, path::Path};

use omp_core::{Duration, DurationError};
use omp_tool::DEFAULT_INTERRUPT_GRACE;
use serde::{
	Deserialize, Deserializer, Serialize, Serializer,
	de::{self, Visitor},
};

use crate::ext::config::{ExtensionOverlay, Scope, ScopedOverlay};

const SETTINGS_FILE: &str = "config.toml";
const SETTINGS_TEMP_FILE: &str = "config.toml.tmp";

/// Runtime durations shared by the agent, eval, and extension-host control
/// planes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RuntimeDurations {
	/// Courtesy interval between cooperative cancellation and forced
	/// interruption.
	#[serde(with = "nonzero_duration")]
	pub interrupt_grace: Duration,
}

impl Default for RuntimeDurations {
	fn default() -> Self {
		Self { interrupt_grace: DEFAULT_INTERRUPT_GRACE }
	}
}

/// Persisted client-scope preferences under `<data_dir>/config.toml`.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Settings {
	/// Model key selected as the default for interactive chat.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub default_model: Option<String>,
	/// Runtime timeout and cancellation settings.
	#[serde(default)]
	pub runtime:       RuntimeDurations,
	/// Client-scope extension overlay.
	#[serde(default)]
	pub extensions:    ExtensionOverlay,
}

impl Settings {
	/// Loads the client settings from `data_dir`, falling back to defaults when
	/// absent or corrupt. Invalid extension overlays are refused by
	/// [`Self::extension_scopes`] rather than silently admitted.
	#[must_use]
	pub fn load(data_dir: &Path) -> Self {
		fs::read_to_string(data_dir.join(SETTINGS_FILE))
			.ok()
			.and_then(|data| toml::from_str(&data).ok())
			.unwrap_or_default()
	}

	/// Loads and validates client extension configuration without the
	/// compatibility fallback used by the chat preferences path.
	///
	/// Extension admission must use this entry point so a secret in
	/// `[extensions.settings]` is refused as `E-SETTING-SECRET`.
	pub fn load_checked(data_dir: &Path) -> Result<Self, crate::ext::ExtensionError> {
		let path = data_dir.join(SETTINGS_FILE);
		if !path.exists() {
			return Ok(Self::default());
		}
		let data = fs::read_to_string(path).map_err(|error| {
			crate::ext::ExtensionError::new(
				crate::ext::ExtensionCode::EManifestParse,
				error.to_string(),
			)
		})?;
		let settings: Self = toml::from_str(&data).map_err(|error| {
			crate::ext::ExtensionError::new(
				crate::ext::ExtensionCode::EManifestParse,
				error.to_string(),
			)
		})?;
		settings.extensions.validate(Scope::Client)?;
		Ok(settings)
	}

	/// Returns the resolved runtime durations.
	#[must_use]
	pub const fn runtime_durations(&self) -> RuntimeDurations {
		self.runtime
	}

	/// Constructs the ordered P1 client → workspace overlay list and validates
	/// each scope's security invariants.
	pub fn extension_scopes(
		&self,
		workspace: Option<ExtensionOverlay>,
	) -> Result<Vec<ScopedOverlay>, crate::ext::ExtensionError> {
		self.extensions.validate(Scope::Client)?;
		let mut scopes =
			vec![ScopedOverlay { scope: Scope::Client, overlay: self.extensions.clone() }];
		if let Some(workspace) = workspace {
			workspace.validate(Scope::Workspace)?;
			scopes.push(ScopedOverlay { scope: Scope::Workspace, overlay: workspace });
		}
		Ok(scopes)
	}

	/// Atomically saves client-scope settings to `<data_dir>/config.toml`.
	pub fn save(&self, data_dir: &Path) -> io::Result<()> {
		fs::create_dir_all(data_dir)?;
		let data = toml::to_string_pretty(self).map_err(io::Error::other)?;
		let temporary = data_dir.join(SETTINGS_TEMP_FILE);
		fs::write(&temporary, data)?;
		fs::rename(temporary, data_dir.join(SETTINGS_FILE))
	}
}

#[cfg(test)]
mod tests {
	use omp_core::DurationUnit;

	use super::*;
	#[test]
	fn settings_round_trip() {
		let data_dir = tempfile::tempdir().expect("create temporary data directory");
		let settings = Settings {
			default_model: Some("anthropic/claude-sonnet-4".to_owned()),
			..Settings::default()
		};

		settings.save(data_dir.path()).expect("save settings");

		let loaded = Settings::load(data_dir.path());
		assert_eq!(loaded.default_model, settings.default_model);
		assert_eq!(
			loaded.runtime_durations().interrupt_grace,
			settings.runtime_durations().interrupt_grace,
		);
		assert_eq!(loaded.runtime_durations().interrupt_grace.unit(), DurationUnit::Milliseconds,);
	}

	#[test]
	fn configured_runtime_duration_precedes_default() {
		let settings: Settings = toml::from_str("[runtime]\ninterrupt_grace = \"375ms\"")
			.expect("configured duration parses");

		assert_eq!(
			settings.runtime_durations().interrupt_grace,
			Duration::new(375, DurationUnit::Milliseconds),
		);
		assert_eq!(settings.runtime_durations().interrupt_grace.to_string(), "375ms");
	}

	#[test]
	fn missing_runtime_duration_uses_explicit_unit_default() {
		let settings: Settings = toml::from_str("").expect("defaults parse");

		assert_eq!(settings.runtime_durations().interrupt_grace, omp_tool::DEFAULT_INTERRUPT_GRACE,);
		assert_eq!(settings.runtime_durations().interrupt_grace.to_string(), "150ms");
	}

	#[test]
	fn corrupt_settings_fall_back_to_default() {
		let data_dir = tempfile::tempdir().expect("create temporary data directory");
		fs::write(data_dir.path().join(SETTINGS_FILE), "not valid toml")
			.expect("write corrupt settings");

		let loaded = Settings::load(data_dir.path());
		assert!(loaded.default_model.is_none());
	}
}

mod nonzero_duration {
	use super::*;

	pub fn serialize<S>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		serializer.collect_str(value)
	}

	pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
	where
		D: Deserializer<'de>,
	{
		deserializer.deserialize_str(DurationVisitor)
	}

	struct DurationVisitor;

	impl Visitor<'_> for DurationVisitor {
		type Value = Duration;

		fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
			formatter.write_str("a positive integer duration with an explicit ns/us/ms/s/m/h unit")
		}

		fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
		where
			E: de::Error,
		{
			let duration = value.parse::<Duration>().map_err(E::custom)?;
			if duration.value() == 0 {
				return Err(E::custom("duration must be greater than zero"));
			}
			let standard = duration.to_std().map_err(|error| match error {
				DurationError::Overflow => E::custom("duration is too large"),
				other => E::custom(other),
			})?;
			i64::try_from(standard.as_nanos())
				.map_err(|_| E::custom("duration is too large for telemetry serialization"))?;
			Ok(duration)
		}
	}
}
