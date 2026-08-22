//! Strict `WATCHDOG.yml` parsing and deterministic roster composition.

use std::collections::BTreeMap;

use omp_core::Str;
use serde::Deserialize;
use thiserror::Error;

/// Default investigative tools granted when an advisor omits `tools`.
pub const DEFAULT_ADVISOR_TOOLS: [&str; 3] = ["read", "grep", "glob"];

/// One validated advisor declaration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdvisorRule {
	/// Human-facing advisor name.
	pub name:         Str,
	/// Stable lowercase identifier used for deduplication and persistence.
	pub slug:         Str,
	/// Whether the advisor should be constructed.
	pub enabled:      bool,
	/// Optional model selector; absence resolves through the advisor role.
	pub model:        Option<Str>,
	/// Tool grants. `None` means the default investigative set; an empty slice
	/// means no tools.
	pub tools:        Option<Box<[Str]>>,
	/// Advisor-specific instruction block.
	pub instructions: Option<Str>,
	/// Source containing this declaration.
	pub source:       Str,
}

/// One strictly parsed watchdog file.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WatchdogRuleSet {
	/// Shared instructions applied to every advisor in this file.
	pub instructions: Option<Str>,
	/// Validated advisor declarations in source order.
	pub advisors:     Vec<AdvisorRule>,
}

/// Specificity-merged advisor roster.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AdvisorRoster {
	/// Shared instructions in discovery order.
	pub instructions: Option<Str>,
	/// Advisors ordered by first declaration; later duplicate slugs replace in
	/// place.
	pub advisors:     Vec<AdvisorRule>,
}
/// One unknown tool dropped while evaluating a parsed advisor rule.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdvisorRuleWarning {
	/// Advisor containing the unknown grant.
	pub advisor: Str,
	/// Source file containing the grant.
	pub source:  Str,
	/// Unknown normalized tool name.
	pub tool:    Str,
}

/// Tool grants after evaluation against the tools actually built for a session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvaluatedAdvisorTools {
	/// Effective tool names. Omitted config and unknown-only config use the
	/// default subset.
	pub tools:    Box<[Str]>,
	/// Unknown grants for app-owned diagnostics.
	pub warnings: Vec<AdvisorRuleWarning>,
}

/// Rejected `WATCHDOG.yml` surface.
#[derive(Debug, Error)]
pub enum AdvisorRuleError {
	/// YAML syntax or shape did not match the closed schema.
	#[error("invalid WATCHDOG.yml document")]
	Yaml(#[source] serde_yaml::Error),
	/// The advisor name is empty after trimming.
	#[error("advisor at index {index} has an empty name")]
	EmptyName {
		/// Zero-based roster index.
		index: usize,
	},
	/// A model selector is present but empty.
	#[error("advisor `{advisor}` has an empty model selector")]
	EmptyModel {
		/// Advisor label.
		advisor: Str,
	},
	/// A tool name is not a canonical built-in identifier.
	#[error("advisor `{advisor}` has invalid tool name `{tool}`")]
	InvalidTool {
		/// Advisor label.
		advisor: Str,
		/// Rejected tool name.
		tool:    Str,
	},
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WatchdogWire {
	instructions: Option<String>,
	#[serde(default)]
	advisors:     Vec<AdvisorWire>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AdvisorWire {
	name:         String,
	model:        Option<String>,
	tools:        Option<Vec<String>>,
	instructions: Option<String>,
	#[serde(default = "enabled_by_default")]
	enabled:      bool,
}

const fn enabled_by_default() -> bool {
	true
}

/// Parses and validates one `WATCHDOG.yml` or `WATCHDOG.yaml` document.
///
/// Discovery and `@` import expansion remain app-owned. This parser rejects the
/// entire file on unknown fields or invalid entries so partially understood
/// policy never reaches an advisor runtime.
pub fn parse_watchdog_yaml(source: &str, yaml: &str) -> Result<WatchdogRuleSet, AdvisorRuleError> {
	let wire: WatchdogWire = serde_yaml::from_str(yaml).map_err(AdvisorRuleError::Yaml)?;
	let mut advisors = Vec::with_capacity(wire.advisors.len());
	for (index, entry) in wire.advisors.into_iter().enumerate() {
		let name = entry.name.trim();
		if name.is_empty() {
			return Err(AdvisorRuleError::EmptyName { index });
		}
		let model = match entry.model {
			Some(model) if model.trim().is_empty() => {
				return Err(AdvisorRuleError::EmptyModel { advisor: Str::new(name) });
			},
			Some(model) => Some(Str::new(model.trim())),
			None => None,
		};
		let tools = entry
			.tools
			.map(|tools| validate_tools(name, tools))
			.transpose()?;
		advisors.push(AdvisorRule {
			name: Str::new(name),
			slug: slugify_advisor_name(name),
			enabled: entry.enabled,
			model,
			tools,
			instructions: trimmed(entry.instructions),
			source: Str::new(source),
		});
	}
	Ok(WatchdogRuleSet { instructions: trimmed(wire.instructions), advisors })
}

/// Merges files from least to most specific.
///
/// Shared instructions concatenate in discovery order. A later advisor with an
/// existing slug replaces that declaration without changing roster position.
pub fn merge_watchdog_rules(rule_sets: impl IntoIterator<Item = WatchdogRuleSet>) -> AdvisorRoster {
	let mut shared = String::new();
	let mut advisors = Vec::<AdvisorRule>::new();
	let mut positions = BTreeMap::<Str, usize>::new();
	for rule_set in rule_sets {
		if let Some(instructions) = rule_set.instructions {
			if !shared.is_empty() {
				shared.push_str("\n\n");
			}
			shared.push_str(instructions.as_str());
		}
		for advisor in rule_set.advisors {
			if let Some(position) = positions.get(&advisor.slug).copied() {
				advisors[position] = advisor;
			} else {
				positions.insert(advisor.slug.clone(), advisors.len());
				advisors.push(advisor);
			}
		}
	}
	AdvisorRoster { instructions: (!shared.is_empty()).then(|| Str::new(shared)), advisors }
}

/// Produces the durable slug used by advisor child identities.
pub fn slugify_advisor_name(name: &str) -> Str {
	let mut slug = String::with_capacity(name.len());
	let mut separator = false;
	for character in name.chars() {
		if character.is_ascii_alphanumeric() {
			if separator && !slug.is_empty() {
				slug.push('-');
			}
			separator = false;
			slug.push(character.to_ascii_lowercase());
		} else {
			separator = true;
		}
	}
	if slug.is_empty() {
		Str::new_static("advisor")
	} else {
		Str::new(slug)
	}
}

/// Evaluates one parsed tool grant against the tools built for this
/// session.
///
/// Unknown tools are dropped. An explicit empty list remains empty; an
/// omitted or unknown-only nonempty list falls back to the available
/// members of [`DEFAULT_ADVISOR_TOOLS`].
pub fn evaluate_advisor_tools(
	rule: &AdvisorRule,
	available: impl IntoIterator<Item = impl AsRef<str>>,
) -> EvaluatedAdvisorTools {
	let available = available
		.into_iter()
		.map(|name| Str::new(name.as_ref()))
		.collect::<Vec<_>>();
	let configured = rule.tools.as_deref();
	let mut warnings = Vec::new();
	let mut tools = configured
		.unwrap_or_default()
		.iter()
		.filter_map(|tool| {
			if available.iter().any(|name| name == tool) {
				Some(tool.clone())
			} else {
				warnings.push(AdvisorRuleWarning {
					advisor: rule.name.clone(),
					source:  rule.source.clone(),
					tool:    tool.clone(),
				});
				None
			}
		})
		.collect::<Vec<_>>();
	if configured.is_none() || configured.is_some_and(|names| !names.is_empty() && tools.is_empty())
	{
		tools = DEFAULT_ADVISOR_TOOLS
			.into_iter()
			.filter(|default| available.iter().any(|name| name.as_str() == *default))
			.map(Str::new_static)
			.collect();
	}
	EvaluatedAdvisorTools { tools: tools.into_boxed_slice(), warnings }
}

fn validate_tools(advisor: &str, tools: Vec<String>) -> Result<Box<[Str]>, AdvisorRuleError> {
	let mut normalized = Vec::with_capacity(tools.len());
	for tool in tools {
		let tool = tool.trim();
		let tool = match tool {
			"search" => "grep",
			"find" => "glob",
			other => other,
		};
		if !valid_tool_name(tool) {
			return Err(AdvisorRuleError::InvalidTool {
				advisor: Str::new(advisor),
				tool:    Str::new(tool),
			});
		}
		if !normalized.iter().any(|known: &Str| known.as_str() == tool) {
			normalized.push(Str::new(tool));
		}
	}
	Ok(normalized.into_boxed_slice())
}

fn valid_tool_name(tool: &str) -> bool {
	let mut bytes = tool.bytes();
	bytes.next().is_some_and(|first| first.is_ascii_lowercase())
		&& bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn trimmed(value: Option<String>) -> Option<Str> {
	value.and_then(|value| {
		let value = value.trim();
		(!value.is_empty()).then(|| Str::new(value))
	})
}
#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn strict_parse_merge_and_tool_evaluation() {
		let base = parse_watchdog_yaml(
			"user/WATCHDOG.yml",
			"instructions: shared\nadvisors:\n  - name: Architecture\n    tools: [search, unknown]\n",
		)
		.expect("base watchdog");
		let leaf = parse_watchdog_yaml(
			"project/WATCHDOG.yml",
			"advisors:\n  - name: Architecture\n    model: provider/model:high\n",
		)
		.expect("leaf watchdog");
		let roster = merge_watchdog_rules([base, leaf]);
		assert_eq!(roster.instructions.as_deref(), Some("shared"));
		assert_eq!(roster.advisors.len(), 1);
		assert_eq!(roster.advisors[0].model.as_deref(), Some("provider/model:high"));

		let unknown_only =
			parse_watchdog_yaml("WATCHDOG.yml", "advisors:\n  - name: Review\n    tools: [unknown]\n")
				.expect("unknown tools remain evaluable");
		let evaluated = evaluate_advisor_tools(&unknown_only.advisors[0], ["read", "grep", "glob"]);
		assert_eq!(evaluated.tools.as_ref(), &[
			Str::new_static("read"),
			Str::new_static("grep"),
			Str::new_static("glob")
		]);
		assert_eq!(evaluated.warnings.len(), 1);
	}

	#[test]
	fn unknown_fields_and_invalid_tool_surfaces_are_rejected() {
		assert!(parse_watchdog_yaml("WATCHDOG.yml", "unexpected: true").is_err());
		assert!(
			parse_watchdog_yaml(
				"WATCHDOG.yml",
				"advisors:\n  - name: Review\n    tools: [bad-tool]\n"
			)
			.is_err()
		);
	}
}
