//! Persisted application settings and layered extension configuration.

use std::{
	fmt,
	path::{Path, PathBuf},
};

pub mod io;
pub mod manager;
pub mod migrate;
pub mod subscription;

use omp_agent::{CompactionMethodOrder, CompactionTier};
use omp_core::{Duration, DurationError};
use omp_llm_inference::{Difficulty, DifficultyBackend};
pub use omp_memory::config::{AutolearnSettings, MemorySettings, MnemopiSettings};
use omp_tool::DEFAULT_INTERRUPT_GRACE;
use omp_tui::components::ComposerStyle;
use serde::{
	Deserialize, Deserializer, Serialize, Serializer,
	de::{self, Visitor},
};

pub use crate::envd::tool_settings::ToolSettings;
use crate::ext::config::{ExtensionOverlay, Scope, ScopedOverlay};

const PERSISTED_SCOPES: &[omp_settings::SettingScope] = &[
	omp_settings::SettingScope::Global,
	omp_settings::SettingScope::Project,
	omp_settings::SettingScope::Runtime,
];

const CORE_FIELDS: &[omp_settings::FieldDescriptor] = &[
	omp_settings::FieldDescriptor {
		path:        "default_model",
		label:       "Default model",
		description: "Default model selector for interactive chat.",
		kind:        omp_settings::SettingKind::String,
		scopes:      PERSISTED_SCOPES,
		order:       10,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "runtime.interrupt_grace",
		label:       "Interrupt grace",
		description: "Courtesy interval before forced interruption.",
		kind:        omp_settings::SettingKind::Duration,
		scopes:      PERSISTED_SCOPES,
		order:       20,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "compaction.enabled",
		label:       "Automatic compaction",
		description: "Enable automatic context compaction.",
		kind:        omp_settings::SettingKind::Boolean,
		scopes:      PERSISTED_SCOPES,
		order:       40,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "compaction.async_enabled",
		label:       "Speculative compaction",
		description: "Allow latency-bearing methods to run speculatively.",
		kind:        omp_settings::SettingKind::Boolean,
		scopes:      PERSISTED_SCOPES,
		order:       41,
		options:     None,
		condition:   Some(omp_settings::Condition { field: "compaction.enabled", equals: "true" }),
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "compaction.method_order",
		label:       "Compaction methods",
		description: "Ordered automatic compaction fallback ladder.",
		kind:        omp_settings::SettingKind::Array,
		scopes:      PERSISTED_SCOPES,
		order:       42,
		options:     None,
		condition:   Some(omp_settings::Condition { field: "compaction.enabled", equals: "true" }),
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "compaction.threshold_fraction",
		label:       "Compaction threshold",
		description: "Usable-context fraction that triggers compaction.",
		kind:        omp_settings::SettingKind::Number,
		scopes:      PERSISTED_SCOPES,
		order:       43,
		options:     None,
		condition:   Some(omp_settings::Condition { field: "compaction.enabled", equals: "true" }),
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "compaction.keep_recent_tokens",
		label:       "Recent tokens",
		description: "Recent-context growth retained around speculative summaries.",
		kind:        omp_settings::SettingKind::Integer,
		scopes:      PERSISTED_SCOPES,
		order:       44,
		options:     None,
		condition:   Some(omp_settings::Condition { field: "compaction.enabled", equals: "true" }),
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "auto_thinking.backend",
		label:       "Auto-thinking backend",
		description: "Classifier backend.",
		kind:        omp_settings::SettingKind::Enum(&["online", "local"]),
		scopes:      PERSISTED_SCOPES,
		order:       50,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "auto_thinking.provisional",
		label:       "Provisional effort",
		description: "Effort used while classification settles.",
		kind:        omp_settings::SettingKind::Enum(&[
			"off", "minimal", "low", "medium", "high", "max",
		]),
		scopes:      PERSISTED_SCOPES,
		order:       51,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "auto_thinking.ceiling",
		label:       "Effort ceiling",
		description: "Maximum auto-classified effort.",
		kind:        omp_settings::SettingKind::Enum(&[
			"off", "minimal", "low", "medium", "high", "max",
		]),
		scopes:      PERSISTED_SCOPES,
		order:       52,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "auto_thinking.allow_max",
		label:       "Allow maximum effort",
		description: "Allow the online classifier to choose maximum effort.",
		kind:        omp_settings::SettingKind::Boolean,
		scopes:      PERSISTED_SCOPES,
		order:       53,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "memory.backend",
		label:       "Memory backend",
		description: "Default-off durable memory backend.",
		kind:        omp_settings::SettingKind::Enum(&["off", "mnemopi"]),
		scopes:      PERSISTED_SCOPES,
		order:       54,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "mnemopi.scoping",
		label:       "Mnemopi bank scope",
		description: "Canonical-project and shared-bank recall policy.",
		kind:        omp_settings::SettingKind::Enum(&[
			"global",
			"per-project",
			"per-project-tagged",
		]),
		scopes:      PERSISTED_SCOPES,
		order:       55,
		options:     None,
		condition:   Some(omp_settings::Condition { field: "memory.backend", equals: "mnemopi" }),
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "autolearn.enabled",
		label:       "Automatic learning",
		description: "Enable managed-skill guidance and capture eligibility.",
		kind:        omp_settings::SettingKind::Boolean,
		scopes:      PERSISTED_SCOPES,
		order:       56,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "plan.enabled",
		label:       "Plan mode",
		description: "Enable the planning execution mode.",
		kind:        omp_settings::SettingKind::Boolean,
		scopes:      PERSISTED_SCOPES,
		order:       60,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "plan.default_on_startup",
		label:       "Start in plan mode",
		description: "Enter plan mode at the start of a fresh interactive session.",
		kind:        omp_settings::SettingKind::Boolean,
		scopes:      PERSISTED_SCOPES,
		order:       61,
		options:     None,
		condition:   Some(omp_settings::Condition { field: "plan.enabled", equals: "true" }),
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "worktree.base",
		label:       "Worktree base",
		description: "Base directory for Environment-owned isolated worktrees.",
		kind:        omp_settings::SettingKind::Path,
		scopes:      PERSISTED_SCOPES,
		order:       60,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "composer.shape",
		label:       "Composer shape",
		description: "Interactive composer chrome.",
		kind:        omp_settings::SettingKind::Enum(&[
			"box",
			"claude",
			"pi",
			"borderless",
			"rule",
			"field",
			"rail",
		]),
		scopes:      PERSISTED_SCOPES,
		order:       70,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "extensions",
		label:       "Extensions",
		description: "Client-scope extension overlay.",
		kind:        omp_settings::SettingKind::Table,
		scopes:      PERSISTED_SCOPES,
		order:       80,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "images.auto_resize",
		label:       "Auto-resize images",
		description: "Resize large prompt images to 2000x2000 while preserving format.",
		kind:        omp_settings::SettingKind::Boolean,
		scopes:      PERSISTED_SCOPES,
		order:       81,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "secrets.enabled",
		label:       "Provider secret obfuscation",
		description: "Obfuscate configured secrets in provider-bound projections.",
		kind:        omp_settings::SettingKind::Boolean,
		scopes:      PERSISTED_SCOPES,
		order:       82,
		options:     None,
		condition:   None,
		secret:      false,
	},
	omp_settings::FieldDescriptor {
		path:        "export.shareRedactSecrets",
		label:       "Share secret redaction",
		description: "Irreversibly redact configured secrets from share snapshots.",
		kind:        omp_settings::SettingKind::Boolean,
		scopes:      PERSISTED_SCOPES,
		order:       83,
		options:     None,
		condition:   None,
		secret:      false,
	},
];

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

/// Persisted automatic per-turn reasoning classifier policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AutoThinkingSettings {
	/// Backend used for the constrained-output classifier.
	#[serde(default = "default_difficulty_backend")]
	pub backend:     DifficultyBackend,
	/// Provisional `auto` level while classification settles.
	#[serde(default)]
	pub provisional: Difficulty,
	/// Session-wide effort ceiling applied after classification.
	#[serde(default = "default_difficulty_ceiling")]
	pub ceiling:     Difficulty,
	/// Whether the online five-rung ladder may choose `max`.
	#[serde(default)]
	pub allow_max:   bool,
}

const fn default_difficulty_backend() -> DifficultyBackend {
	DifficultyBackend::Online
}

const fn default_difficulty_ceiling() -> Difficulty {
	Difficulty::Max
}

impl Default for AutoThinkingSettings {
	fn default() -> Self {
		Self {
			backend:     default_difficulty_backend(),
			provisional: Difficulty::default(),
			ceiling:     default_difficulty_ceiling(),
			allow_max:   false,
		}
	}
}

impl AutoThinkingSettings {
	/// Builds immutable classifier inputs for an ordinary turn.
	#[must_use]
	pub const fn for_turn(self) -> omp_llm_inference::AutoDifficulty {
		omp_llm_inference::AutoDifficulty {
			provisional:  self.provisional,
			ceiling:      self.ceiling,
			allow_max:    self.allow_max,
			prewalk_noop: false,
		}
	}

	/// Builds classifier inputs for a prewalk turn and applies its no-op hook.
	#[must_use]
	pub fn for_prewalk_turn(
		self,
		reason_to_execute: Option<&str>,
	) -> omp_llm_inference::AutoDifficulty {
		self.for_turn().with_prewalk_reason(reason_to_execute)
	}
}

/// Persisted planning-mode defaults.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlanSettings {
	/// Whether plan mode is available.
	#[serde(default = "default_plan_enabled")]
	pub enabled:            bool,
	/// Whether fresh interactive sessions begin in plan mode.
	#[serde(default)]
	pub default_on_startup: bool,
}

const fn default_plan_enabled() -> bool {
	true
}

impl Default for PlanSettings {
	fn default() -> Self {
		Self { enabled: true, default_on_startup: false }
	}
}

/// Placement policy for Environment-owned isolated worktrees.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorktreeSettings {
	/// Optional base directory. `OMP_WORKTREE_DIR` takes precedence.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub base: Option<PathBuf>,
}

/// Persisted appearance options for the interactive composer.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ComposerSettings {
	/// Built-in chrome rendered around the interactive input.
	#[serde(default)]
	pub shape: ComposerStyle,
}

/// Prompt image attachment policy.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ImageSettings {
	/// Resize dimensions above the provider-compatible ceiling.
	#[serde(default = "default_true")]
	pub auto_resize: bool,
}

impl Default for ImageSettings {
	fn default() -> Self {
		Self { auto_resize: true }
	}
}

/// Reversible provider-bound secret policy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SecretsSettings {
	/// Whether complete bidirectional provider obfuscation is enabled.
	#[serde(default)]
	pub enabled: bool,
}

/// Irreversible export-boundary policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExportSettings {
	/// Whether share snapshots are irreversibly redacted before leaving Core.
	#[serde(default = "default_true", rename = "shareRedactSecrets")]
	pub share_redact_secrets: bool,
}

impl Default for ExportSettings {
	fn default() -> Self {
		Self { share_redact_secrets: true }
	}
}

/// Persisted client-scope preferences under `<data_dir>/config.toml`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
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
	/// Automatic per-turn reasoning classification.
	#[serde(default)]
	pub auto_thinking: AutoThinkingSettings,
	/// Planning-mode availability and startup policy.
	#[serde(default)]
	pub plan:          PlanSettings,
	/// Default-off memory backend selector.
	#[serde(default)]
	pub memory:        MemorySettings,
	/// Mnemopi-specific durable bank and lifecycle settings.
	#[serde(default)]
	pub mnemopi:       MnemopiSettings,
	/// Automatic-learning capture settings.
	#[serde(default)]
	pub autolearn:     AutolearnSettings,
	/// Isolated worktree placement policy.
	#[serde(default)]
	pub worktree:      WorktreeSettings,
	/// Interactive composer appearance.
	#[serde(default)]
	pub composer:      ComposerSettings,
	/// Client-scope extension overlay.
	#[serde(default)]
	pub extensions:    ExtensionOverlay,
	/// Prompt image attachment policy.
	#[serde(default)]
	pub images:        ImageSettings,
	/// Reversible provider-bound secret policy.
	#[serde(default)]
	pub secrets:       SecretsSettings,
	/// Irreversible export-boundary policy.
	#[serde(default)]
	pub export:        ExportSettings,
}

impl omp_settings::SettingsDomain for Settings {
	const DOMAIN: &'static str = "app-core";
	const FIELDS: &'static [omp_settings::FieldDescriptor] = CORE_FIELDS;
	const PREFIX: Option<&'static str> = None;

	fn validate(&self) -> Result<(), omp_settings::ValidationError> {
		if self
			.default_model
			.as_deref()
			.is_some_and(|model| model.trim().is_empty())
			|| self.extensions.validate(Scope::Client).is_err()
			|| !(0.0..=1.0).contains(&self.compaction.threshold_fraction)
			|| self.compaction.threshold_fraction == 0.0
		{
			return Err(omp_settings::ValidationError::DomainInvariant { domain: Self::DOMAIN });
		}
		Ok(())
	}
}

omp_settings::inventory::submit! {
	omp_settings::DomainRegistration::of::<Settings>()
}

/// Loads the current typed projection through the single settings authority.
pub fn current(data_dir: &Path) -> Result<Settings, manager::SettingsManagerError> {
	current_with_overlays(data_dir, &[])
}

/// Loads settings with ordered invocation-local native TOML overlays.
pub fn current_with_overlays(
	data_dir: &Path,
	overlays: &[PathBuf],
) -> Result<Settings, manager::SettingsManagerError> {
	let project = std::env::current_dir().ok();
	let mut paths = manager::SettingsPaths::discover(data_dir, project.as_deref());
	paths.overlays.extend_from_slice(overlays);
	let manager = manager::SettingsManager::open(paths)?;
	let projection = manager
		.snapshot()
		.project::<Settings>()
		.map_err(|error| manager::SettingsManagerError::Projection { source: error })?;
	let mut settings = projection.get().clone();
	settings.mnemopi = settings.mnemopi.normalize();
	Ok(settings)
}

impl Settings {
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
}

#[cfg(test)]
mod tests {
	use omp_core::DurationUnit;

	use super::*;
	#[test]
	fn isolated_snapshot_round_trip() {
		let settings = Settings {
			default_model: Some("anthropic/claude-sonnet-4".to_owned()),
			..Settings::default()
		};
		let snapshot = omp_settings::SettingsSnapshot::isolated(settings.clone()).expect("snapshot");
		let loaded = snapshot.project::<Settings>().expect("projection");
		assert_eq!(loaded.get().default_model, settings.default_model);
		assert_eq!(
			loaded.get().runtime_durations().interrupt_grace,
			settings.runtime_durations().interrupt_grace,
		);
		assert_eq!(
			loaded.get().runtime_durations().interrupt_grace.unit(),
			DurationUnit::Milliseconds,
		);
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
	fn auto_thinking_backend_and_ceiling_are_configurable() {
		let settings: Settings = toml::from_str(
			"[auto_thinking]\nbackend = \"local\"\nprovisional = \"high\"\nceiling = \
			 \"medium\"\nallow_max = true",
		)
		.expect("auto thinking settings parse");
		assert_eq!(settings.auto_thinking.backend, DifficultyBackend::Local);
		assert_eq!(settings.auto_thinking.provisional, Difficulty::High);
		assert_eq!(settings.auto_thinking.ceiling, Difficulty::Medium);
		assert!(settings.auto_thinking.allow_max);
		assert!(!settings.auto_thinking.for_turn().prewalk_noop);
		assert!(settings.auto_thinking.for_prewalk_turn(None).prewalk_noop);
	}

	#[test]
	fn plan_settings_use_owned_nested_keys() {
		let settings: Settings = toml::from_str("[plan]\nenabled = true\ndefault_on_startup = true")
			.expect("plan settings parse");
		assert!(settings.plan.enabled);
		assert!(settings.plan.default_on_startup);
		let encoded = toml::to_string(&settings).expect("plan settings serialize");
		assert!(encoded.contains("[plan]"));
		assert!(encoded.contains("default_on_startup = true"));
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
	fn corrupt_settings_are_quarantined_with_diagnostics() {
		let data_dir = tempfile::tempdir().expect("create temporary data directory");
		let path = data_dir.path().join("config.toml");
		std::fs::write(&path, "not valid toml").expect("write corrupt settings");
		let manager = manager::SettingsManager::open(manager::SettingsPaths {
			global:   path.clone(),
			project:  None,
			overlays: Vec::new(),
		})
		.expect("manager");
		let diagnostics = manager.diagnostics();
		assert_eq!(diagnostics.len(), 1);
		assert_eq!(diagnostics[0].path, path);
		assert!(diagnostics[0].backup_path.exists());
		assert!(!path.exists());
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
