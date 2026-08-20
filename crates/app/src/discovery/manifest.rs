//! Static manifest declaration dispatch for discoverable capability content.
//!
//! Declarations are data loaded before any extension process starts. Discovery
//! never registers in-process callbacks; callers dispatch the winning static
//! declaration by kind and priority.

use std::path::PathBuf;

use omp_core::Str;
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
		assert_eq!(entries, vec![("same".into(), Str::from("high"))]);
	}
}
