//! Granted-root context-file discovery and immutable prompt projection.

use std::{
	collections::BTreeSet,
	fs,
	path::PathBuf,
	sync::Arc,
};

use omp_agent::{ContextFile, dedupe_context_file_indices};
use omp_core::Str;

use super::{
	at_path::expand_at_paths,
	manifest::{
		CapabilityPayload, ContextPayload, DiscoveredCapability, SourceProvenance, SourceScope,
	},
};

/// Foreign repo-surface context filenames admitted by §8.2.
pub const REPO_SURFACE_CONTEXT_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md", ".cursorrules"];

/// One Environment-granted root and the working directory whose ancestor chain
/// is eligible within that root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrantedContextRoot {
	/// Canonical grant boundary.
	pub root:  PathBuf,
	/// Canonical starting directory, normally the per-root cwd.
	pub start: PathBuf,
}

/// Context discovery configuration frozen with a prompt snapshot.
#[derive(Clone, Debug)]
pub struct ContextDiscoveryOptions {
	/// Manifest-declared native context names. Foreign imports remain restricted
	/// to `REPO_SURFACE_CONTEXT_FILES` regardless of this list.
	pub filenames: Arc<[Str]>,
	/// Maximum ancestor edges per root.
	pub max_depth: usize,
}

impl Default for ContextDiscoveryOptions {
	fn default() -> Self {
		Self {
			filenames: REPO_SURFACE_CONTEXT_FILES
				.iter()
				.copied()
				.map(Str::from)
				.collect::<Vec<_>>()
				.into(),
			max_depth: 64,
		}
	}
}

/// Immutable context item retaining root, depth, source and exact bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextItem {
	/// Root ordinal from the Environment snapshot.
	pub root_index: usize,
	/// Canonical source path.
	pub path:       PathBuf,
	/// Ancestor distance from the root's start directory; zero is closest.
	pub depth:      u16,
	/// Expanded content bytes.
	pub content:    Str,
}

/// Immutable discovered context snapshot.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ContextSnapshot {
	/// Deterministically merged sources ordered from least to most authoritative,
	/// with normalized paragraph-contained sources removed.
	pub items:       Arc<[ContextItem]>,
	/// Bounded non-fatal diagnostics.
	pub diagnostics: Arc<[ContextDiagnostic]>,
}

/// Context discovery diagnostic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContextDiagnostic {
	/// An ungranted start path was refused.
	OutsideGrant(PathBuf),
	/// A source could not be read.
	Unreadable(PathBuf),
	/// Ancestor scanning hit its configured bound.
	Truncated(PathBuf),
}

/// Discovers context only beneath explicit grant boundaries. Paths are
/// ancestor-walked, `@path` expansion reuses the canonical context importer,
/// and normalized paragraph containment favors sources closest to the workspace
/// directory.
pub fn discover(
	roots: &[GrantedContextRoot],
	options: &ContextDiscoveryOptions,
) -> ContextSnapshot {
	let allowed = options
		.filenames
		.iter()
		.map(|name| name.as_str())
		.filter(|name| REPO_SURFACE_CONTEXT_FILES.contains(name))
		.collect::<BTreeSet<_>>();
	let mut diagnostics = Vec::new();
	let mut candidates = Vec::new();
	for (root_index, grant) in roots.iter().enumerate() {
		let root = fs::canonicalize(&grant.root).unwrap_or_else(|_| grant.root.clone());
		let start = fs::canonicalize(&grant.start).unwrap_or_else(|_| grant.start.clone());
		if !start.starts_with(&root) {
			diagnostics.push(ContextDiagnostic::OutsideGrant(start));
			continue;
		}
		let mut current = start.as_path();
		let mut reached_root = false;
		for depth in 0..=options.max_depth {
			for name in &allowed {
				let path = current.join(name);
				if !path.is_file() {
					continue;
				}
				match expand_at_paths(&path) {
					Ok(content) => candidates.push(ContextItem {
						root_index,
						path: fs::canonicalize(&path).unwrap_or(path),
						depth: u16::try_from(depth).unwrap_or(u16::MAX),
						content: Str::from(content),
					}),
					Err(_) => diagnostics.push(ContextDiagnostic::Unreadable(path)),
				}
			}
			if current == root {
				reached_root = true;
				break;
			}
			let Some(parent) = current.parent() else {
				break;
			};
			if parent == current || !parent.starts_with(&root) {
				break;
			}
			current = parent;
		}
		if !reached_root {
			diagnostics.push(ContextDiagnostic::Truncated(grant.start.clone()));
		}
	}

	let comparable = candidates
		.iter()
		.map(|item| {
			ContextFile::new(item.path.clone(), item.content.as_bytes().to_vec())
				.with_depth(item.depth)
		})
		.collect::<Vec<_>>();
	candidates = dedupe_context_file_indices(&comparable)
		.into_iter()
		.map(|index| candidates[index].clone())
		.collect();
	ContextSnapshot { items: candidates.into(), diagnostics: diagnostics.into() }
}

/// Projects exact context bytes into the agent prompt contract without
/// filesystem access.
pub fn prompt_files(snapshot: &ContextSnapshot) -> Arc<[ContextFile]> {
	snapshot
		.items
		.iter()
		.map(|item| {
			ContextFile::new(item.path.clone(), item.content.as_bytes().to_vec())
				.with_origin(Str::from(item.path.to_string_lossy().as_ref()))
				.with_depth(item.depth)
		})
		.collect::<Vec<_>>()
		.into()
}

/// Lowers an immutable context snapshot into registry declarations without
/// re-reading the filesystem.
pub fn declarations(snapshot: &ContextSnapshot) -> Vec<DiscoveredCapability> {
	snapshot
		.items
		.iter()
		.map(|item| {
			let payload = ContextPayload {
				path:    item.path.clone(),
				content: item.content.clone(),
				depth:   Some(item.depth),
			};
			let source = SourceProvenance::native("context", item.path.clone(), SourceScope::Project);
			DiscoveredCapability::keyed(
				item.path.to_string_lossy().as_ref(),
				CapabilityPayload::ContextFiles(payload),
				source,
			)
		})
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn granted_walk_is_depth_sorted_and_dedupes_contained_context() {
		let tree = tempfile::tempdir().unwrap();
		let root = tree.path().join("repo");
		let nested = root.join("a/b");
		fs::create_dir_all(&nested).unwrap();
		fs::write(root.join("AGENTS.md"), "shared").unwrap();
		fs::write(root.join("a/CLAUDE.md"), "shared\n\nnear-only").unwrap();
		fs::write(nested.join(".cursorrules"), "closest").unwrap();
		let snapshot = discover(
			&[GrantedContextRoot { root, start: nested }],
			&ContextDiscoveryOptions::default(),
		);
		assert_eq!(snapshot.items.len(), 2);
		assert!(snapshot.items[0].path.ends_with("CLAUDE.md"));
		assert!(snapshot.items[1].path.ends_with(".cursorrules"));
	}

	#[test]
	fn rejects_starts_outside_grant() {
		let tree = tempfile::tempdir().unwrap();
		let snapshot = discover(
			&[GrantedContextRoot { root: tree.path().join("a"), start: tree.path().join("b") }],
			&ContextDiscoveryOptions::default(),
		);
		assert!(matches!(snapshot.diagnostics.first(), Some(ContextDiagnostic::OutsideGrant(_))));
	}
}
