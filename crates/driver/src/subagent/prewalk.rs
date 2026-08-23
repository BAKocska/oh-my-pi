//! Prewalk gating and definition features retained across revival.

use std::collections::BTreeMap;

use omp_agent::{AgentAuxiliary, AgentDefinition};
use omp_core::Str;

use super::settings::TaskSettings;

/// Definition features that must survive parking and cold revival.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevivalAgentFeatures {
	/// Model selector for the prewalk handoff, or empty for the default role.
	pub prewalk:         Option<Str>,
	/// Model selector for the advisor, or empty for the default role.
	pub advisor:         Option<Str>,
	/// Skills injected before the first child prompt.
	pub autoload_skills: Box<[Str]>,
	/// Structural read-summary override.
	pub read_summarize:  Option<bool>,
}

/// One-shot prewalk gate which alone may retain `todo`.
#[derive(Clone, Debug)]
pub struct PrewalkGate {
	features: RevivalAgentFeatures,
	armed:    bool,
}

impl PrewalkGate {
	/// Resolves settings overrides ahead of definition frontmatter.
	pub fn resolve(definition: &AgentDefinition, settings: &TaskSettings) -> Self {
		let prewalk = setting_override(&settings.agent_prewalk, definition.name.as_str())
			.or_else(|| auxiliary_selector(definition.prewalk.as_ref()));
		let advisor = setting_override(&settings.agent_advisor, definition.name.as_str())
			.or_else(|| auxiliary_selector(definition.advisor.as_ref()));
		let armed = prewalk.is_some();
		Self {
			features: RevivalAgentFeatures {
				prewalk,
				advisor,
				autoload_skills: definition.autoload_skills.clone(),
				read_summarize: definition.read_summarize,
			},
			armed,
		}
	}

	/// Whether the prewalk pass is pending and `todo` may remain enabled.
	pub const fn armed(&self) -> bool {
		self.armed
	}

	/// Completes the gate before delegating the real assignment.
	pub const fn complete(&mut self) {
		self.armed = false;
	}

	/// Returns the durable features copied into revival metadata.
	pub const fn features(&self) -> &RevivalAgentFeatures {
		&self.features
	}
}

fn setting_override(overrides: &BTreeMap<Str, Str>, name: &str) -> Option<Str> {
	overrides
		.iter()
		.find(|(candidate, _)| candidate.as_str().eq_ignore_ascii_case(name))
		.and_then(|(_, value)| {
			let value = value.trim();
			(!matches!(value.to_ascii_lowercase().as_str(), "off" | "false" | "none")).then(|| {
				if matches!(value.as_str(), "on" | "true") {
					Str::default()
				} else {
					value
				}
			})
		})
}

fn auxiliary_selector(auxiliary: Option<&AgentAuxiliary>) -> Option<Str> {
	match auxiliary {
		Some(AgentAuxiliary::Default) => Some(Str::default()),
		Some(AgentAuxiliary::Model(model)) => Some(model.clone()),
		None => None,
	}
}
