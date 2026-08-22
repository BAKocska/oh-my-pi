//! Environment-owned canonical workspace-root grant snapshots.

use bytes::Bytes;
use omp_proto::{SCHEMA_REV, env::v1 as pb};

/// Immutable primary grant served by one project Environment generation.
pub(crate) struct WorkspaceRootHost {
	primary: pb::WorkspaceRoot,
}

impl WorkspaceRootHost {
	/// Creates the canonical primary grant from Environment identity facts.
	pub(crate) fn new(canonical_uri: &str, grant_id: Bytes) -> Self {
		Self { primary: pb::WorkspaceRoot { canonical_uri: canonical_uri.to_owned(), grant_id } }
	}

	/// Returns the ordered canonical grants for this Environment generation.
	pub(crate) fn snapshot(&self) -> pb::WorkspaceRootSet {
		pb::WorkspaceRootSet {
			revision:      1,
			primary:       Some(self.primary.clone()),
			granted:       vec![self.primary.clone()],
			wire_revision: SCHEMA_REV,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn primary_is_first_and_revisioned() {
		let host = WorkspaceRootHost::new("file:///workspace", Bytes::from_static(b"grant"));
		let snapshot = host.snapshot();
		assert_eq!(snapshot.revision, 1);
		assert_eq!(snapshot.primary.as_ref(), snapshot.granted.first());
		assert_eq!(snapshot.wire_revision, SCHEMA_REV);
	}
}
