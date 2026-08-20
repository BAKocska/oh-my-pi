//! Persisted application settings and layered extension configuration.

use std::{fmt, fs, io, path::Path};

use omp_agent::{CompactionMethodOrder, CompactionTier};
use omp_core::{Duration, DurationError};
use omp_tool::DEFAULT_INTERRUPT_GRACE;
use omp_tui::components::ComposerStyle;
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

/// Tool exposure and timeout policy resolved from the layered settings stack.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolSettings {
	/// Explicit per-tool enablement overrides; absent names remain enabled.
	#[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
	pub enabled:      std::collections::BTreeMap<omp_core::Str, bool>,
	/// Global ceiling for tool deadlines.
	#[serde(
		default,
		alias = "maxTimeout",
		skip_serializing_if = "Option::is_none",
		with = "optional_duration"
	)]
	pub max_timeout:  Option<Duration>,
	/// Optional pinned edit revision (`rep.1` or `hl.1`) for this client.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub edit_dialect: Option<omp_core::Str>,
}

impl ToolSettings {
	/// Whether a named tool is available after applying the default-enabled
	/// rule.
	#[must_use]
	pub fn enabled(&self, name: &str) -> bool {
		self.enabled.get(name).copied().unwrap_or(true)
	}
}

mod optional_duration {
	use super::*;

	pub fn serialize<S>(value: &Option<Duration>, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		match value {
			Some(value) => serializer.serialize_some(&value.to_string()),
			None => serializer.serialize_none(),
		}
	}

	pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
	where
		D: Deserializer<'de>,
	{
		Option::<String>::deserialize(deserializer)?
			.map(|value| value.parse().map_err(de::Error::custom))
			.transpose()
	}
}

const fn default_true() -> bool {
	true
}

const fn default_compaction_threshold() -> f64 {
	0.8
}

const fn default_keep_recent_tokens() -> u64 {
	20_000
}

fn default_compaction_method_order() -> Vec<CompactionTier> {
	CompactionTier::ALL.to_vec()
}

/// Persisted automatic context-maintenance policy.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompactionSettings {
	/// Whether automatic context compaction is enabled.
	#[serde(default = "default_true")]
	pub enabled:            bool,
	/// Whether latency-bearing methods may run speculatively in the background.
	#[serde(default = "default_true")]
	pub async_enabled:      bool,
	/// Ordered enabled ladder methods. Omitted methods are disabled.
	#[serde(default = "default_compaction_method_order")]
	pub method_order:       Vec<CompactionTier>,
	/// Fraction of usable context that triggers automatic compaction.
	#[serde(default = "default_compaction_threshold")]
	pub threshold_fraction: f64,
	/// Recent-context growth allowed before an armed summary is refreshed.
	#[serde(default = "default_keep_recent_tokens")]
	pub keep_recent_tokens: u64,
}

impl Default for CompactionSettings {
	fn default() -> Self {
		Self {
			enabled:            true,
			async_enabled:      true,
			method_order:       default_compaction_method_order(),
			threshold_fraction: default_compaction_threshold(),
			keep_recent_tokens: default_keep_recent_tokens(),
		}
	}
}

impl CompactionSettings {
	/// Resolves duplicates while preserving the user's first-occurrence order.
	/// Disabled automatic compaction resolves to an empty ladder.
	#[must_use]
	pub fn method_order(&self) -> CompactionMethodOrder {
		if self.enabled {
			CompactionMethodOrder::resolve(&self.method_order)
		} else {
			CompactionMethodOrder::resolve(&[])
		}
	}

	/// Returns speculation options consumed by the agent coordinator.
	#[must_use]
	pub const fn speculation_options(&self) -> omp_agent::CompactionSpeculationOptions {
		omp_agent::CompactionSpeculationOptions {
			enabled:            self.async_enabled,
			keep_recent_tokens: self.keep_recent_tokens,
		}
	}
}

/// Persisted appearance options for the interactive composer.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ComposerSettings {
	/// Built-in chrome rendered around the interactive input.
	#[serde(default)]
	pub shape: ComposerStyle,
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
	/// Built-in tool exposure and execution timeout policy.
	#[serde(default)]
	pub tools:         ToolSettings,
	/// Automatic context-maintenance options.
	#[serde(default)]
	pub compaction:    CompactionSettings,
	/// Interactive composer appearance.
	#[serde(default)]
	pub composer:      ComposerSettings,
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
	fn tool_settings_default_enabled_and_parse_timeout() {
		let settings: Settings = toml::from_str(
			"[tools]\nmax_timeout = \"30s\"\nedit_dialect = \"rep.1\"\n[tools.enabled]\nask = false",
		)
		.expect("tool settings parse");
		assert!(!settings.tools.enabled("ask"));
		assert!(settings.tools.enabled("todo"));
		assert_eq!(settings.tools.max_timeout, Some(Duration::new(30, DurationUnit::Seconds)));
		assert_eq!(settings.tools.edit_dialect.as_deref(), Some("rep.1"));
	}

	#[test]
	fn compaction_defaults_to_the_current_ladder() {
		let settings: Settings = toml::from_str("").expect("defaults parse");
		assert_eq!(settings.compaction.method_order().as_slice(), &CompactionTier::ALL);
		assert!(settings.compaction.speculation_options().enabled);
		assert_eq!(settings.compaction.keep_recent_tokens, 20_000);
	}

	#[test]
	fn compaction_method_order_is_user_ordered_and_deduplicated() {
		let settings: Settings = toml::from_str(
			"[compaction]\nmethod_order = [\"remote\", \"local\", \"remote\", \
			 \"handoff\"]\nasync_enabled = false",
		)
		.expect("compaction settings parse");
		assert_eq!(settings.compaction.method_order().as_slice(), &[
			CompactionTier::Remote,
			CompactionTier::Local,
			CompactionTier::Handoff
		],);
		assert!(!settings.compaction.speculation_options().enabled);
		let mut disabled = settings.compaction.clone();
		disabled.enabled = false;
		assert!(disabled.method_order().as_slice().is_empty());
	}

	#[test]
	fn composer_shape_uses_nested_appearance_setting() {
		let settings: Settings =
			toml::from_str("[composer]\nshape = \"rail\"").expect("composer settings parse");
		assert_eq!(settings.composer.shape, ComposerStyle::Rail);
		let encoded = toml::to_string(&settings).expect("composer settings serialize");
		assert!(encoded.contains("[composer]"));
		assert!(encoded.contains("shape = \"rail\""));
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
