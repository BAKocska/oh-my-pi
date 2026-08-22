//! Canonical restricted local security-reviewer registration.

use omp_agent::{AgentDefinition, SECURITY_REVIEW_INSTRUCTION_V1, SpawnPolicy};
use omp_core::Str;

use super::model::strict_result_schema;

/// Stable profile selected by the local review command and ordinary task
/// broker.
pub const PROFILE_ID: &str = "security-reviewer";

const FRONTMATTER: &str = r#"---
name: security-reviewer
description: Read-only local security specialist for evidence-backed exploitable defects
tools: [read, grep, glob, lsp, task]
spawns: [security-reviewer]
model: "@slow"
readSummarize: false
---
"#;

/// Builds the canonical profile. App registration installs this definition
/// after discovery so project, extension, and user declarations cannot widen
/// its authority.
pub fn definition() -> AgentDefinition {
	let role = omp_agent::prompt_assets::prompt_asset(
		omp_agent::prompt_assets::PromptAssetId::AgentSecurityReviewer,
	);
	let mut markdown = String::with_capacity(
		FRONTMATTER.len() + SECURITY_REVIEW_INSTRUCTION_V1.len() + role.content.len() + 2,
	);
	markdown.push_str(FRONTMATTER);
	markdown.push_str(SECURITY_REVIEW_INSTRUCTION_V1);
	markdown.push('\n');
	markdown.push_str(role.content);
	let mut definition = AgentDefinition::parse_markdown(PROFILE_ID, &markdown)
		.expect("the canonical security reviewer profile is a build-time constant");
	definition.output_schema = Some(strict_result_schema());
	definition
}

/// Verifies the immutable authority boundary of a resolved canonical profile.
///
/// This rejects widened or shadowed definitions before they reach an ordinary
/// child spawn. Environment-side tool and LSP admission remain authoritative
/// for each invocation.
pub fn is_canonical(definition: &AgentDefinition) -> bool {
	const TOOLS: &[&str] = &["read", "grep", "glob", "lsp", "task", "yield"];
	definition.name == PROFILE_ID
		&& definition.tools.len() == TOOLS.len()
		&& TOOLS
			.iter()
			.all(|tool| definition.tools.iter().any(|candidate| candidate == tool))
		&& matches!(
			&definition.spawns,
			SpawnPolicy::Only(allowed)
				if allowed.len() == 1 && allowed[0].as_str() == PROFILE_ID
		) && definition.output_schema.as_ref() == Some(&strict_result_schema())
}

/// Returns whether a tool name is explicitly denied by the local reviewer
/// profile. This diagnostic helper never grants a tool; registration and
/// Environment admission own enforcement.
pub fn denied_tool(name: &str) -> bool {
	!matches!(name, "read" | "grep" | "glob" | "lsp" | "task" | "yield")
}

/// Stable profile name as an owned OMP string for spawn requests.
pub fn profile_name() -> Str {
	Str::new_static(PROFILE_ID)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn canonical_profile_is_narrow_and_recursive_children_cannot_widen_it() {
		let definition = definition();
		assert!(is_canonical(&definition));
		assert!(omp_agent::is_read_only_agent(&definition));
		for denied in [
			"bash",
			"write",
			"edit",
			"ast_grep",
			"web_search",
			"mcp",
			"extensions",
			"env",
			"credentials",
		] {
			assert!(denied_tool(denied));
		}
		assert!(definition.spawns.allows(PROFILE_ID));
		assert!(!definition.spawns.allows("task"));
	}
}
