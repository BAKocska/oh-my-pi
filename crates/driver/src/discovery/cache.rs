//! Session-composition-owned parsed capability cache.
//!
//! Cache identity is supplied by Environment, docserver, walker, repository,
//! and package authorities. This module never stats or reads host paths and it
//! deliberately has no process-global singleton.

use std::{
	collections::{BTreeSet, HashMap},
	path::{Path, PathBuf},
	sync::Arc,
};

use omp_core::Str;
use parking_lot::RwLock;

use super::manifest::{CapabilityRevision, DiscoveredCapability};

/// Complete immutable input identity for one parsed source document.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ParsedCacheKey {
	/// Provider whose parser produced the cached declarations.
	pub provider_id:          Str,
	/// Canonical authority-visible source path.
	pub path:                 PathBuf,
	/// Exact file/document/walker revision.
	pub file_revision:        CapabilityRevision,
	/// Repository snapshot revision affecting contextual parsing.
	pub repository_revision:  Option<CapabilityRevision>,
	/// Installed package identity, when the source belongs to one.
	pub installed_package_id: Option<Str>,
	/// Installed package generation, when the source belongs to one.
	pub package_revision:     Option<CapabilityRevision>,
}

/// Point-in-time cache cardinalities for diagnostics.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CacheStats {
	/// Number of parsed revision entries.
	pub entries:            usize,
	/// Number of distinct providers represented.
	pub providers:          usize,
	/// Number of distinct installed packages represented.
	pub installed_packages: usize,
}

/// Revision-keyed cache owned by one application composition.
#[derive(Debug, Default)]
pub struct DiscoveryCache {
	entries: RwLock<HashMap<ParsedCacheKey, Arc<[DiscoveredCapability]>>>,
}

impl DiscoveryCache {
	/// Creates an empty composition-local cache.
	pub fn new() -> Self {
		Self::default()
	}

	/// Returns a shared parsed declaration set only for an exact revision key.
	pub fn get(&self, key: &ParsedCacheKey) -> Option<Arc<[DiscoveredCapability]>> {
		self.entries.read().get(key).cloned()
	}

	/// Stores one parsed declaration set under its exact authority revisions.
	pub fn insert(
		&self,
		key: ParsedCacheKey,
		declarations: Arc<[DiscoveredCapability]>,
	) -> Option<Arc<[DiscoveredCapability]>> {
		self.entries.write().insert(key, declarations)
	}

	/// Invalidates exactly one parsed revision while preserving unrelated files,
	/// providers, repositories, and package generations.
	pub fn invalidate_revision(&self, key: &ParsedCacheKey) -> bool {
		self.entries.write().remove(key).is_some()
	}

	/// Invalidates one canonical path or every cached descendant when `path` is
	/// a directory authority key. No host filesystem lookup is performed.
	pub fn invalidate_path(&self, path: &Path) -> usize {
		self.retain_count(|key| !key.path.starts_with(path))
	}

	/// Invalidates every parsed source produced by one provider.
	pub fn invalidate_provider(&self, provider_id: &str) -> usize {
		self.retain_count(|key| key.provider_id != provider_id)
	}

	/// Invalidates sources contextualized by one repository root revision
	/// identity. Callers pass the exact authority revision being retired.
	pub fn invalidate_repository_revision(&self, revision: &CapabilityRevision) -> usize {
		self.retain_count(|key| key.repository_revision.as_ref() != Some(revision))
	}

	/// Invalidates every source owned by one installed package after a completed
	/// install, upgrade, rollback, or removal transaction.
	pub fn invalidate_installed_package(&self, package_id: &str) -> usize {
		self.retain_count(|key| key.installed_package_id.as_deref() != Some(package_id))
	}

	/// Clears this composition's cache. This does not affect other sessions.
	pub fn clear(&self) -> usize {
		let mut entries = self.entries.write();
		let removed = entries.len();
		entries.clear();
		removed
	}

	/// Returns bounded cache cardinality diagnostics without exposing payloads.
	pub fn stats(&self) -> CacheStats {
		let entries = self.entries.read();
		let providers = entries
			.keys()
			.map(|key| &key.provider_id)
			.collect::<BTreeSet<_>>()
			.len();
		let installed_packages = entries
			.keys()
			.filter_map(|key| key.installed_package_id.as_ref())
			.collect::<BTreeSet<_>>()
			.len();
		CacheStats { entries: entries.len(), providers, installed_packages }
	}

	fn retain_count(&self, mut retain: impl FnMut(&ParsedCacheKey) -> bool) -> usize {
		let mut entries = self.entries.write();
		let before = entries.len();
		entries.retain(|key, _| retain(key));
		before - entries.len()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::discovery::manifest::{
		CapabilityPayload, CapabilityRevision, PromptPayload, RevisionAuthority, SourceProvenance,
		SourceScope,
	};

	fn revision(authority: RevisionAuthority, sequence: u64) -> CapabilityRevision {
		CapabilityRevision {
			authority,
			authority_id: Arc::from([1_u8, 2, 3]),
			sequence,
			digest: [sequence as u8; 32],
		}
	}

	fn declaration(path: &Path) -> DiscoveredCapability {
		DiscoveredCapability::keyed(
			"review",
			CapabilityPayload::Prompts(PromptPayload {
				name:    "review".into(),
				path:    path.to_path_buf(),
				content: "Review carefully".into(),
			}),
			SourceProvenance::native("project-prompts", path.to_path_buf(), SourceScope::Project),
		)
	}

	fn key(path: &Path, file_sequence: u64, repository_sequence: u64) -> ParsedCacheKey {
		ParsedCacheKey {
			provider_id:          "native".into(),
			path:                 path.to_path_buf(),
			file_revision:        revision(RevisionAuthority::Environment, file_sequence),
			repository_revision:  Some(revision(RevisionAuthority::Repository, repository_sequence)),
			installed_package_id: None,
			package_revision:     None,
		}
	}

	#[test]
	fn discovery_cache_invalidation_is_revision_scoped() {
		let cache = DiscoveryCache::new();
		let first_path = Path::new("/env/repo/.omp/prompts/review.md");
		let second_path = Path::new("/env/repo/.omp/prompts/explain.md");
		let first = key(first_path, 1, 7);
		let changed_file = key(first_path, 2, 7);
		let changed_repository = key(first_path, 1, 8);
		let second = key(second_path, 1, 7);
		cache.insert(first.clone(), Arc::from([declaration(first_path)]));
		cache.insert(second.clone(), Arc::from([declaration(second_path)]));

		assert!(cache.get(&first).is_some());
		assert!(cache.get(&changed_file).is_none());
		assert!(cache.get(&changed_repository).is_none());
		assert!(cache.invalidate_revision(&first));
		assert!(cache.get(&first).is_none());
		assert!(cache.get(&second).is_some());
		assert_eq!(cache.stats().entries, 1);
	}

	#[test]
	fn package_invalidator_preserves_unrelated_sources() {
		let cache = DiscoveryCache::new();
		let one_path = Path::new("/env/packages/one/SKILL.md");
		let two_path = Path::new("/env/packages/two/SKILL.md");
		let mut one = key(one_path, 1, 1);
		one.installed_package_id = Some("one".into());
		one.package_revision = Some(revision(RevisionAuthority::InstalledPackage, 3));
		let mut two = key(two_path, 1, 1);
		two.installed_package_id = Some("two".into());
		two.package_revision = Some(revision(RevisionAuthority::InstalledPackage, 4));
		cache.insert(one.clone(), Arc::from([declaration(one_path)]));
		cache.insert(two.clone(), Arc::from([declaration(two_path)]));

		assert_eq!(cache.invalidate_installed_package("one"), 1);
		assert!(cache.get(&one).is_none());
		assert!(cache.get(&two).is_some());
	}
}
