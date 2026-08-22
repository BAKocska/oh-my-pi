//! Fail-closed classification of explicitly read-only agent definitions.

use crate::AgentDefinition;

const READ_ONLY_TOOLS: &[&str] = &[
	"read",
	"grep",
	"glob",
	"web_search",
	"ast_grep",
	"yield",
	"hub",
	"ask",
	"todo",
	"recall",
	"reflect",
	"retain",
	"memory_edit",
	"inspect_image",
	"checkpoint",
	"rewind",
];

/// Reports whether every explicitly declared tool is in the read-only
/// whitelist.
///
/// Empty declarations inherit their parent's tools and are therefore never
/// classified as read-only. Unknown names fail closed and this function never
/// adds tools to the definition.
#[must_use]
pub fn is_read_only_agent(definition: &AgentDefinition) -> bool {
	!definition.tools.is_empty()
		&& definition
			.tools
			.iter()
			.all(|tool| READ_ONLY_TOOLS.contains(&tool.as_str()))
}
