//! Read-only repository-surface foreign content imports.
//!
//! This provider deliberately has no code path for foreign commands, agents,
//! plugins, MCP, settings, or user-home roots.

use std::{
	collections::BTreeSet,
	fs,
	path::{Path, PathBuf},
};

use omp_core::Str;
use omp_walker::WalkRequest;

use super::{
	manifest::{
		CapabilityPayload, DiscoveredCapability, InstructionPayload, PromptPayload, SourceProvenance,
		SourceScope,
	},
	rules::{self, RuleSource},
	skills::{self, SkillDiscoverySettings, SkillSource},
};

/// Native settings projection for repo-surface foreign content families.
#[derive(Clone, Debug)]
pub struct ForeignContentSettings {
	/// Master foreign content toggle.
	pub enabled:          bool,
	/// Enabled family IDs. Empty enables every known content-only family.
	pub enabled_families: BTreeSet<Str>,
}

impl Default for ForeignContentSettings {
	fn default() -> Self {
		Self { enabled: true, enabled_families: BTreeSet::new() }
	}
}

/// Allowed read-only foreign content discovered at one repository surface.
#[derive(Clone, Debug, Default)]
pub struct ForeignContentDiscovery {
	/// Skill declarations.
	pub skills:       Vec<DiscoveredCapability>,
	/// Rule declarations.
	pub rules:        Vec<DiscoveredCapability>,
	/// Reusable prompt declarations.
	pub prompts:      Vec<DiscoveredCapability>,
	/// File-targeted instruction declarations.
	pub instructions: Vec<DiscoveredCapability>,
	/// Non-fatal diagnostics.
	pub warnings:     Vec<Str>,
}

struct Family {
	id:               &'static str,
	skill_dirs:       &'static [&'static str],
	rule_paths:       &'static [&'static str],
	prompt_dirs:      &'static [&'static str],
	instruction_dirs: &'static [&'static str],
}

const FAMILIES: &[Family] = &[
	Family {
		id:               "claude",
		skill_dirs:       &[".claude/skills"],
		rule_paths:       &[".claude/rules"],
		prompt_dirs:      &[".claude/prompts"],
		instruction_dirs: &[],
	},
	Family {
		id:               "codex",
		skill_dirs:       &[".codex/skills"],
		rule_paths:       &[".codex/rules"],
		prompt_dirs:      &[".codex/prompts"],
		instruction_dirs: &[],
	},
	Family {
		id:               "gemini",
		skill_dirs:       &[".gemini/skills"],
		rule_paths:       &[".gemini/rules"],
		prompt_dirs:      &[".gemini/prompts"],
		instruction_dirs: &[],
	},
	Family {
		id:               "opencode",
		skill_dirs:       &[".opencode/skills"],
		rule_paths:       &[".opencode/rules"],
		prompt_dirs:      &[".opencode/prompts"],
		instruction_dirs: &[],
	},
	Family {
		id:               "agents",
		skill_dirs:       &[".agent/skills", ".agents/skills"],
		rule_paths:       &[".agent/rules", ".agents/rules"],
		prompt_dirs:      &[".agent/prompts", ".agents/prompts"],
		instruction_dirs: &[".agent/instructions", ".agents/instructions"],
	},
	Family {
		id:               "cursor",
		skill_dirs:       &[".cursor/skills"],
		rule_paths:       &[".cursor/rules"],
		prompt_dirs:      &[".cursor/prompts"],
		instruction_dirs: &[],
	},
	Family {
		id:               "windsurf",
		skill_dirs:       &[".windsurf/skills"],
		rule_paths:       &[".windsurf/rules"],
		prompt_dirs:      &[".windsurf/prompts"],
		instruction_dirs: &[],
	},
	Family {
		id:               "cline",
		skill_dirs:       &[".cline/skills"],
		rule_paths:       &[".clinerules", ".cline/rules"],
		prompt_dirs:      &[".cline/prompts"],
		instruction_dirs: &[],
	},
	Family {
		id:               "copilot",
		skill_dirs:       &[".github/skills"],
		rule_paths:       &[],
		prompt_dirs:      &[".github/prompts"],
		instruction_dirs: &[".github/instructions"],
	},
	Family {
		id:               "vscode",
		skill_dirs:       &[".vscode/skills"],
		rule_paths:       &[".vscode/rules"],
		prompt_dirs:      &[".vscode/prompts"],
		instruction_dirs: &[".vscode/instructions"],
	},
];

/// Imports only static content below the supplied repository root. The API has
/// no home-directory parameter by design, preventing accidental user-profile
/// probing.
#[must_use]
pub fn discover(
	repository_root: &Path,
	settings: &ForeignContentSettings,
) -> ForeignContentDiscovery {
	if !settings.enabled {
		return ForeignContentDiscovery::default();
	}
	let mut output = ForeignContentDiscovery::default();
	let skill_settings = SkillDiscoverySettings::default();
	for family in FAMILIES {
		if !settings.enabled_families.is_empty() && !settings.enabled_families.contains(family.id) {
			continue;
		}
		let source_id = Str::from(format!("foreign-{}", family.id));
		let skill_sources = family
			.skill_dirs
			.iter()
			.map(|relative| SkillSource {
				id:                  source_id.clone(),
				root:                repository_root.join(relative),
				scope:               SourceScope::Project,
				include_root:        true,
				require_description: true,
				contain_root:        Some(repository_root.to_path_buf()),
				read_only:           true,
			})
			.collect::<Vec<_>>();
		let discovered_skills = skills::discover(&skill_sources, &skill_settings);
		output.skills.extend(discovered_skills.declarations);
		output.warnings.extend(
			discovered_skills
				.warnings
				.into_iter()
				.map(|warning| warning.message),
		);

		let rule_sources = family
			.rule_paths
			.iter()
			.map(|relative| RuleSource {
				id:        source_id.clone(),
				root:      repository_root.join(relative),
				scope:     SourceScope::Project,
				read_only: true,
			})
			.collect::<Vec<_>>();
		let discovered_rules = rules::discover(&rule_sources);
		output.rules.extend(discovered_rules.declarations);
		output.warnings.extend(
			discovered_rules
				.warnings
				.into_iter()
				.map(|warning| warning.message),
		);

		for relative in family.prompt_dirs {
			output
				.prompts
				.extend(load_markdown(repository_root, relative, family.id, true));
		}
		for relative in family.instruction_dirs {
			output
				.instructions
				.extend(load_markdown(repository_root, relative, family.id, false));
		}
	}
	output
}

fn load_markdown(
	root: &Path,
	relative: &str,
	family: &str,
	prompt: bool,
) -> Vec<DiscoveredCapability> {
	let path = root.join(relative);
	let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
	let mut files = if path.is_file() {
		vec![path]
	} else {
		WalkRequest::new(&path)
			.hidden(false)
			.gitignore(true)
			.skip_git(true)
			.depth(1, 8)
			.collect_files()
			.unwrap_or_default()
			.into_iter()
			.map(|entry| entry.absolute_path(&path))
			.collect()
	};
	files.retain(|file| {
		file
			.extension()
			.is_some_and(|ext| ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("mdc"))
	});
	files.sort();
	files
		.into_iter()
		.filter_map(|path| {
			let canonical = fs::canonicalize(&path).ok()?;
			if !canonical.starts_with(&canonical_root) {
				return None;
			}
			let content = fs::read_to_string(&canonical).ok()?;
			let name = canonical.file_stem()?.to_str()?;
			let mut source = SourceProvenance::native(
				format!("foreign-{family}"),
				canonical.clone(),
				SourceScope::Project,
			);
			source.read_only = true;
			let payload = if prompt {
				CapabilityPayload::Prompts(PromptPayload {
					name:    Str::from(name),
					path:    canonical.clone(),
					content: Str::from(content),
				})
			} else {
				CapabilityPayload::Instructions(InstructionPayload {
					name:     Str::from(name),
					path:     canonical.clone(),
					content:  Str::from(content),
					apply_to: None,
				})
			};
			Some(DiscoveredCapability::keyed(name, payload, source))
		})
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn imports_content_but_never_foreign_executable_surfaces() {
		let tree = tempfile::tempdir().unwrap();
		fs::create_dir_all(tree.path().join(".claude/skills/review")).unwrap();
		fs::create_dir_all(tree.path().join(".claude/commands")).unwrap();
		fs::write(
			tree.path().join(".claude/skills/review/SKILL.md"),
			"---\ndescription: review\n---\nbody",
		)
		.unwrap();
		fs::write(tree.path().join(".claude/commands/danger.md"), "run").unwrap();
		fs::write(tree.path().join(".claude/mcp.json"), "{}").unwrap();
		let result = discover(tree.path(), &ForeignContentSettings::default());
		assert_eq!(result.skills.len(), 1);
		assert!(result.prompts.is_empty());
		assert!(result.instructions.is_empty());
		assert!(result.skills[0].source.read_only);
	}
}
