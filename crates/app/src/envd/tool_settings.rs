//! Typed settings owned by production tool admission and registry composition.

use std::collections::BTreeMap;

use omp_core::{Duration, Str};
use omp_settings::{
	DomainRegistration, FieldDescriptor, SettingKind, SettingScope, SettingsDomain, ValidationError,
};
use omp_tool::Effects;
use omp_tools::edit::FormatPolicy;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use super::admission::{ApprovalMode, ApprovalPolicy, ResolvedApproval, resolve_approval};

const PERSISTED: &[SettingScope] = &[SettingScope::Global, SettingScope::Project];
const APPROVAL_MODES: &[&str] = &["always-ask", "write", "yolo"];

/// Tool exposure, timeout, and approval policy resolved from native settings.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolSettings {
	/// Explicit per-tool enablement overrides; absent names remain enabled.
	#[serde(skip_serializing_if = "BTreeMap::is_empty")]
	pub enabled:              BTreeMap<Str, bool>,
	/// Global ceiling for tool deadlines.
	#[serde(skip_serializing_if = "Option::is_none", with = "optional_duration")]
	pub max_timeout:          Option<Duration>,
	/// Optional pinned edit revision (`rep.1` or `hl.1`) for this client.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub edit_dialect:         Option<Str>,
	/// Permit HTTP(S) URL dispatch from read.
	pub fetch_enabled:        bool,
	/// Convert supported documents to Markdown.
	pub render_markdown:      bool,
	/// Normalize images to model pixel/output bounds.
	pub auto_resize_images:   bool,
	/// Formatter requirement for write/edit transactions.
	pub format_policy:        FormatPolicy,
	/// Capture one final diagnostics batch after write.
	pub diagnostics_on_write: bool,
	/// Capture one final diagnostics batch after edit.
	pub diagnostics_on_edit:  bool,
	/// Collapse identical final diagnostics across server bindings.
	pub diagnostic_dedup:     bool,
	/// Default approval posture, applied after effect-tier resolution.
	pub approval_mode:        ApprovalMode,
	/// Authoritative per-tool approval policy overrides.
	#[serde(skip_serializing_if = "BTreeMap::is_empty")]
	pub approval:             BTreeMap<Str, ApprovalPolicy>,
}

impl Default for ToolSettings {
	fn default() -> Self {
		Self {
			enabled:              BTreeMap::new(),
			max_timeout:          None,
			edit_dialect:         None,
			fetch_enabled:        true,
			render_markdown:      true,
			auto_resize_images:   true,
			format_policy:        FormatPolicy::BestEffort,
			diagnostics_on_write: true,
			diagnostics_on_edit:  true,
			diagnostic_dedup:     true,
			approval_mode:        ApprovalMode::Yolo,
			approval:             BTreeMap::new(),
		}
	}
}

impl ToolSettings {
	/// Whether a named tool is available after applying the default-enabled
	/// rule.
	#[must_use]
	pub fn enabled(&self, name: &str) -> bool {
		self.enabled.get(name).copied().unwrap_or(true)
	}

	/// Resolves and receipts one invocation against its live declared effects.
	#[must_use]
	pub fn approval_for(
		&self,
		invocation_id: impl Into<Str>,
		tool_name: impl Into<Str>,
		effects: &Effects,
	) -> ResolvedApproval {
		let tool_name = tool_name.into();
		resolve_approval(
			invocation_id,
			tool_name.clone(),
			effects,
			self.approval_mode,
			self.approval.get(&tool_name).copied(),
		)
	}
}

impl SettingsDomain for ToolSettings {
	const DOMAIN: &'static str = "tools";
	const FIELDS: &'static [FieldDescriptor] = &[
		FieldDescriptor {
			path:        "tools.enabled",
			label:       "Enabled tools",
			description: "Per-tool availability overrides.",
			kind:        SettingKind::Table,
			scopes:      PERSISTED,
			order:       10,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "tools.max_timeout",
			label:       "Maximum tool timeout",
			description: "Global ceiling for tool execution deadlines.",
			kind:        SettingKind::Duration,
			scopes:      PERSISTED,
			order:       20,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "tools.edit_dialect",
			label:       "Edit dialect",
			description: "Pinned edit tool revision.",
			kind:        SettingKind::String,
			scopes:      PERSISTED,
			order:       30,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "tools.approval_mode",
			label:       "Tool approval",
			description: "Default effect tier approved without confirmation.",
			kind:        SettingKind::Enum(APPROVAL_MODES),
			scopes:      PERSISTED,
			order:       40,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "tools.approval",
			label:       "Tool approval policies",
			description: "Per-tool allow, prompt, or deny overrides.",
			kind:        SettingKind::Table,
			scopes:      PERSISTED,
			order:       50,
			options:     None,
			condition:   None,
			secret:      false,
		},
	];

	fn validate(&self) -> Result<(), ValidationError> {
		if self
			.approval
			.keys()
			.chain(self.enabled.keys())
			.any(|name| name.trim().is_empty())
		{
			return Err(ValidationError::DomainInvariant { domain: Self::DOMAIN });
		}
		Ok(())
	}
}

omp_settings::inventory::submit! {
	DomainRegistration::of::<ToolSettings>()
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

#[cfg(test)]
mod tests {
	use omp_core::sf;
	use omp_settings::{SettingsSnapshot, registered_domains};
	use omp_tool::{Effects, ExecEffects};

	use super::*;
	use crate::envd::admission::{ApprovalPolicy, ApprovalSource, ApprovalTier};

	#[test]
	fn typed_projection_and_registration_share_one_domain() {
		let expected = ToolSettings {
			approval_mode: ApprovalMode::Write,
			approval: BTreeMap::from([(sf!("shell"), ApprovalPolicy::Deny)]),
			..ToolSettings::default()
		};
		let snapshot = SettingsSnapshot::isolated(expected.clone()).expect("isolated settings");
		let projected = snapshot
			.project::<ToolSettings>()
			.expect("typed projection");
		assert_eq!(projected.get(), &expected);
		assert!(
			registered_domains()
				.iter()
				.any(|domain| domain.name == ToolSettings::DOMAIN)
		);
	}

	#[test]
	fn override_is_applied_to_declared_effect_tier() {
		let settings = ToolSettings {
			approval: BTreeMap::from([(sf!("shell"), ApprovalPolicy::Deny)]),
			..ToolSettings::default()
		};
		let effects = Effects {
			exec: Some(ExecEffects { commands: [sf!("*")].into(), network: true }),
			..Effects::empty()
		};
		let decision = settings.approval_for("call-1", "shell", &effects);
		assert_eq!(decision.tier, ApprovalTier::Exec);
		assert_eq!(decision.policy, ApprovalPolicy::Deny);
		assert_eq!(decision.source, ApprovalSource::User);
	}

	#[test]
	fn empty_override_key_is_rejected() {
		let settings = ToolSettings {
			approval: BTreeMap::from([(Str::default(), ApprovalPolicy::Prompt)]),
			..ToolSettings::default()
		};
		assert!(settings.validate().is_err());
	}
}
