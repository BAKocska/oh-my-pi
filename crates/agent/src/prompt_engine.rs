//! Shared, immutable scribe engine for agent-owned prompt templates.

use std::sync::LazyLock;

use omp_scribe::{Engine, Template};
macro_rules! system_template {
	($fn_name:ident, $name:literal, $path:literal) => {
		#[doc = concat!("Returns the compiled `", $name, "` prompt template.")]
		pub fn $fn_name() -> &'static Template {
			static TEMPLATE: LazyLock<Template> = LazyLock::new(|| {
				engine()
					.compile($name, include_str!($path))
					.unwrap_or_else(|error| panic!("invalid embedded prompt template: {error}"))
			});
			&TEMPLATE
		}
	};
}

/// Returns the process-wide agent-side prompt engine.
///
/// Agent templates use only scribe builtins. Domain helpers are registered on
/// the independent driver-side engine before its templates are compiled.
pub fn engine() -> &'static Engine {
	static ENGINE: LazyLock<Engine> = LazyLock::new(Engine::new);
	&ENGINE
}
system_template!(conventions, "system/conventions", "../prompts/system/conventions.md");
system_template!(role, "system/role", "../prompts/system/role.md");
system_template!(runtime, "system/runtime", "../prompts/system/runtime.md");
system_template!(tool_policy, "system/tool-policy", "../prompts/system/tool-policy.md");
system_template!(workflow, "system/workflow", "../prompts/system/workflow.md");
system_template!(delivery, "system/delivery", "../prompts/system/delivery.md");
system_template!(computer_safety, "system/computer-safety", "../prompts/system/computer-safety.md");
system_template!(project, "system/project", "../prompts/system/project.md");
system_template!(active_repo, "system/active-repo", "../prompts/system/active-repo.md");
system_template!(
	workspace_fallback,
	"system/workspace-fallback",
	"../prompts/system/workspace-fallback.md"
);

/// Every agent-owned system template.
pub fn system_templates() -> [&'static Template; 10] {
	[
		conventions(),
		role(),
		runtime(),
		tool_policy(),
		workflow(),
		delivery(),
		computer_safety(),
		project(),
		active_repo(),
		workspace_fallback(),
	]
}

#[cfg(test)]
mod tests {

	use std::collections::HashSet;

	use super::{engine, system_templates};
	use crate::prompt_keys::ALL;

	#[test]
	fn process_engine_is_shared() {
		assert!(std::ptr::eq(engine(), engine()));
	}
	#[test]
	fn embedded_templates_parse_and_use_registered_keys() {
		let legal = ALL.iter().copied().collect::<HashSet<_>>();
		for template in system_templates() {
			for key in template.referenced_keys() {
				assert!(
					legal.contains(key),
					"template {} references unregistered key {key}",
					template.name()
				);
			}
		}
	}
}
