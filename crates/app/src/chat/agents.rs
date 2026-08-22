//! Native OMP subagent catalog and discovery composition.

use std::{
	collections::BTreeMap,
	path::{Path, PathBuf},
	sync::{Arc, LazyLock},
};

use omp_agent::{
	AgentDefinition,
	prompt_assets::{PromptAssetId, prompt_asset},
};
use omp_core::{Str, sf};

static BUNDLED: LazyLock<Arc<BTreeMap<Str, AgentDefinition>>> = LazyLock::new(|| {
	let definitions = [
		("task", TASK, PromptAssetId::AgentTask),
		("scout", SCOUT, PromptAssetId::AgentScout),
		("sonic", SONIC, PromptAssetId::AgentTask),
		("designer", DESIGNER, PromptAssetId::AgentDesigner),
		("reviewer", REVIEWER, PromptAssetId::AgentReviewer),
		("security-reviewer", SECURITY_REVIEWER, PromptAssetId::AgentSecurityReviewer),
		("librarian", LIBRARIAN, PromptAssetId::AgentLibrarian),
	];
	Arc::new(
		definitions
			.into_iter()
			.map(|(name, frontmatter, asset)| {
				let markdown = format!("{frontmatter}{}", prompt_asset(asset).content);
				let definition = AgentDefinition::parse_markdown(name, &markdown)
					.expect("bundled agent definitions are build-time constants");
				(sf!(name), definition)
			})
			.collect(),
	)
});

/// Returns the complete native catalog using project → user → extension →
/// bundled precedence.
#[must_use]
pub fn discover(root: &Path) -> Arc<BTreeMap<Str, AgentDefinition>> {
	let home = std::env::var_os("HOME").map(PathBuf::from);
	let extensions = extension_roots(root, home.as_deref());
	let declarations =
		crate::discovery::manifest::agent_declarations(root, home.as_deref(), &extensions);
	let discovery = crate::discovery::manifest::discover_agents(&declarations);
	for warning in &discovery.warnings {
		tracing::warn!(path = %warning.path.display(), error = %warning.kind, "skipping malformed agent definition");
	}
	let mut definitions = BUNDLED.as_ref().clone();
	for (name, definition) in discovery.definitions.into_iter().rev() {
		definitions.insert(name, definition);
	}
	Arc::new(definitions)
}

fn extension_roots(root: &Path, home: Option<&Path>) -> Vec<PathBuf> {
	let mut roots = Vec::new();
	for extensions in
		[Some(root.join(".omp/extensions")), home.map(|home| home.join(".omp/extensions"))]
			.into_iter()
			.flatten()
	{
		let Ok(entries) = std::fs::read_dir(extensions) else {
			continue;
		};
		roots.extend(entries.filter_map(Result::ok).filter_map(|entry| {
			entry
				.file_type()
				.ok()
				.filter(|kind| kind.is_dir())
				.map(|_| entry.path())
		}));
	}
	roots.sort();
	roots.dedup();
	roots
}

const TASK: &str = r#"---
name: task
description: General-purpose subagent with full capabilities for delegated multi-step tasks
spawns: "*"
model: "@task"
thinkingLevel: medium
---
"#;

const SCOUT: &str = r#"---
name: scout
description: MUST be used for exploratory codebase research, rapid code analysis, and broad pattern searches. Fast read-only scout returning compressed context for handoff.
tools: [read, grep, glob, web_search]
model: "@smol"
thinkingLevel: medium
readSummarize: false
output:
  type: object
  required: [summary, files, architecture]
  properties:
    summary: { type: string }
    files: { type: array, items: { type: object } }
    architecture: { type: string }
---
"#;

const SONIC: &str = r#"---
name: sonic
description: Low-reasoning agent for strictly mechanical updates or data collection only
model: "@smol"
thinkingLevel: medium
---
"#;

const DESIGNER: &str = r#"---
name: designer
description: UI/UX specialist for design implementation, review, visual refinement
model: "@designer"
---
"#;

const REVIEWER: &str = r#"---
name: reviewer
description: Code review specialist for quality/security analysis
tools: [read, grep, glob, bash, lsp, web_search, ast_grep]
spawns: [scout]
model: "@slow"
output:
  type: object
  required: [overall_correctness, explanation, confidence]
  properties:
    overall_correctness: { enum: [correct, incorrect] }
    explanation: { type: string }
    confidence: { type: number }
    findings: { type: array, items: { type: object } }
---
"#;

const SECURITY_REVIEWER: &str = r#"---
name: security-reviewer
description: Read-only security specialist for evidence-backed repository vulnerability discovery
tools: [read, grep, glob, lsp, ast_grep]
output:
  type: object
  required: [coverage_summary]
  properties:
    coverage_summary: { type: string }
    findings: { type: array, items: { type: object } }
    reviewed_paths: { type: array, items: { type: string } }
---
"#;

const LIBRARIAN: &str = r#"---
name: librarian
description: Researches external libraries and APIs by reading source code. Returns definitive, source-verified answers.
tools: [read, grep, glob, bash, lsp, web_search, ast_grep]
model: "@smol"
thinkingLevel: minimal
readSummarize: false
output:
  type: object
  required: [answer, sources, api, version]
  properties:
    answer: { type: string }
    sources: { type: array, items: { type: object } }
    api: { type: array, items: { type: object } }
    version: { type: string }
    breaking_changes: { type: array, items: { type: string } }
    caveats: { type: array, items: { type: string } }
---
"#;
