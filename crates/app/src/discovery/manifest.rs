//! Static manifest declaration dispatch for discoverable capability content.
//!
//! Declarations are data loaded before any extension process starts. Discovery
//! never registers in-process callbacks; callers dispatch the winning static
//! declaration by kind and priority.

use std::path::{Path, PathBuf};

use omp_core::{Str, sf};
use serde::{Deserialize, Serialize};

/// Content capability kinds supported by the static declaration table.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityKind {
	/// Markdown skill content.
	Skills,
	/// Rules and constraints.
	Rules,
	/// Persistent context files.
	ContextFiles,
	/// Reusable prompt templates.
	Prompts,
	/// File-targeted instructions.
	Instructions,
	/// Markdown slash-command templates.
	SlashCommands,
	/// Markdown subagent definitions parsed by `omp-agent`.
	Agents,
	/// Native settings sources.
	Settings,
}

/// A manifest-declared capability root, available without importing code.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CapabilityDeclaration {
	/// Stable declaration identity within its manifest.
	pub id:       Str,
	/// Content type exposed by this root.
	pub kind:     CapabilityKind,
	/// Filesystem root or static manifest payload location.
	pub root:     PathBuf,
	/// Larger values override lower-priority sources for the same item key.
	pub priority: i32,
}

/// One malformed or unreadable manifest-discovered agent definition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentDiscoveryWarning {
	/// Source file skipped during discovery.
	pub path:    PathBuf,
	/// Sanitized parse or I/O failure.
	pub message: Str,
}

/// First-wins agent definitions plus non-fatal source warnings.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AgentDiscovery {
	/// Definitions in manifest precedence order.
	pub definitions: Vec<(Str, omp_agent::AgentDefinition)>,
	/// Malformed definitions skipped without aborting discovery.
	pub warnings:    Vec<AgentDiscoveryWarning>,
}

/// Loads manifest-declared agent directories through the common static
/// capability table. File stems are stable definition keys and malformed
/// extension/project content is skipped with structured warning evidence.
pub fn discover_agents(declarations: &[CapabilityDeclaration]) -> AgentDiscovery {
	let ordered = priority_ordered(declarations.to_vec());
	let warnings = std::cell::RefCell::new(Vec::new());
	let definitions = dispatch_first(&ordered, CapabilityKind::Agents, |declaration| {
		agent_files(&declaration.root)
			.into_iter()
			.filter_map(|path| {
				let key = path
					.file_stem()
					.and_then(std::ffi::OsStr::to_str)
					.map(Str::from)?;
				let markdown = match std::fs::read_to_string(&path) {
					Ok(markdown) => markdown,
					Err(error) => {
						warnings
							.borrow_mut()
							.push(AgentDiscoveryWarning { path, message: Str::from(error.to_string()) });
						return None;
					},
				};
				match omp_agent::AgentDefinition::parse_markdown(key.clone(), &markdown) {
					Ok(definition) => Some((key, definition)),
					Err(error) => {
						warnings
							.borrow_mut()
							.push(AgentDiscoveryWarning { path, message: Str::from(error.to_string()) });
						None
					},
				}
			})
			.collect()
	});
	AgentDiscovery { definitions, warnings: warnings.into_inner() }
}

fn agent_files(root: &Path) -> Vec<PathBuf> {
	if root.is_file() {
		return vec![root.to_path_buf()];
	}
	let mut paths = super::native::scan_capability_dir(root)
		.into_iter()
		.filter(|path| path.extension().and_then(std::ffi::OsStr::to_str) == Some("md"))
		.collect::<Vec<_>>();
	paths.sort();
	paths
}

/// Sorts declarations so dispatch is deterministic without registration order.
pub fn priority_ordered(
	mut declarations: Vec<CapabilityDeclaration>,
) -> Vec<CapabilityDeclaration> {
	declarations.sort_by(|left, right| {
		right
			.priority
			.cmp(&left.priority)
			.then_with(|| left.id.cmp(&right.id))
	});
	declarations
}

/// First-wins dispatch over static declarations of one kind. The caller owns
/// file parsing; its `key` must be the stable capability identity.
pub fn dispatch_first<'a, T>(
	declarations: &'a [CapabilityDeclaration],
	kind: CapabilityKind,
	mut load: impl FnMut(&'a CapabilityDeclaration) -> Vec<(Str, T)>,
) -> Vec<(Str, T)> {
	let mut claimed = std::collections::BTreeSet::new();
	let mut output = Vec::new();
	for declaration in declarations
		.iter()
		.filter(|declaration| declaration.kind == kind)
	{
		for (key, item) in load(declaration) {
			if claimed.insert(key.clone()) {
				output.push((key, item));
			}
		}
	}
	output
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn static_priority_and_first_wins_dispatch() {
		let declarations = priority_ordered(vec![
			CapabilityDeclaration {
				id:       "low".into(),
				kind:     CapabilityKind::Skills,
				root:     PathBuf::new(),
				priority: 1,
			},
			CapabilityDeclaration {
				id:       "high".into(),
				kind:     CapabilityKind::Skills,
				root:     PathBuf::new(),
				priority: 2,
			},
		]);
		let entries = dispatch_first(&declarations, CapabilityKind::Skills, |declaration| {
			vec![("same".into(), declaration.id.clone())]
		});
		assert_eq!(entries, vec![("same".into(), sf!("high"))]);
	}
}
