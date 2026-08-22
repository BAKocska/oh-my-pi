//! Durable handoff-child planning and stale-result fencing.

use omp_core::{Str, sf};
use omp_storage::blob::BlobRef;

use crate::journal::Compact;
/// Instruction used when a model prepares a successor-facing handoff document.
pub const HANDOFF_DOCUMENT_PROMPT: &str = include_str!("../prompts/compaction/handoff-document.md");

/// Structured context transferred to a handoff child.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HandoffSummary {
	/// Concise state of completed work.
	pub completed: Str,
	/// Explicit work remaining for the child.
	pub remaining: Str,
	/// Decisions and invariants the child must preserve.
	pub decisions: Str,
}

/// Immutable parent state captured before detached handoff summarization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HandoffRequest {
	/// Parent session id.
	pub parent_session_id: Str,
	/// Parent checkpoint covered by the summary.
	pub parent_checkpoint: u64,
	/// Parent compaction epoch used as a stale-result fence.
	pub compaction_epoch:  u64,
	/// Workspace roots inherited by the child.
	pub workspace_roots:   Vec<Str>,
	/// Shared `local://` roots available to the child.
	pub local_roots:       Vec<Str>,
	/// Shared blob roots transferred by reference.
	pub blob_roots:        Vec<BlobRef>,
	/// Whether policy requires materializing the child journal on disk.
	pub save_to_disk:      bool,
}

/// Journal-owner inputs for creating the durable child.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HandoffCommit {
	/// New child session identity.
	pub child_session_id: Str,
	/// Exact lineage request captured from the parent.
	pub request:          HandoffRequest,
	/// Structured summary stored in the child's initial projection.
	pub summary:          HandoffSummary,
}

impl HandoffSummary {
	/// Renders the structured fields into the portable child context summary.
	pub fn render(&self) -> Str {
		sf!(
			"## Completed\n{}\n\n## Remaining\n{}\n\n## Decisions\n{}",
			self.completed,
			self.remaining,
			self.decisions
		)
	}
}

impl HandoffCommit {
	/// Builds the sole compact event seeded into the child journal.
	pub fn compact(&self, tokens_before: u64, tokens_after: Option<u64>) -> Compact {
		Compact {
			summary: self.summary.render(),
			short: Some(sf!("Handoff from {}", self.request.parent_session_id)),
			first_kept: 0,
			tokens_before,
			tokens_after,
			method: Some(sf!("handoff")),
			warning: None,
			superseded: Vec::new(),
			snapcompact: None,
		}
	}
}

impl HandoffRequest {
	/// Fences and assembles a child commit only while the parent leaf is
	/// current.
	pub fn commit_if_current(
		self,
		current_checkpoint: u64,
		current_epoch: u64,
		child_session_id: Str,
		summary: HandoffSummary,
	) -> Option<HandoffCommit> {
		(self.parent_checkpoint == current_checkpoint && self.compaction_epoch == current_epoch)
			.then_some(HandoffCommit { child_session_id, request: self, summary })
	}
}
