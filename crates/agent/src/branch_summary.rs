//! Cancellable, checkpoint-fenced branch-summary coordination.

use omp_core::Str;
use omp_storage::transcript::ModelRef;

/// Configuration captured when branch summarization starts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchSummaryRequest {
	/// Coordinator-local run identity.
	pub run_id:           u64,
	/// Checkpoint whose discarded branch is summarized.
	pub checkpoint:       u64,
	/// Explicit summary model.
	pub model:            ModelRef,
	/// Provider-specific thinking level or budget label.
	pub thinking:         Option<Str>,
	/// Output-token budget reserved for the summary.
	pub token_reserve:    u64,
	/// Immutable branch text presented to the summarizer.
	pub branch_text:      Str,
	/// Durable compaction epoch captured with the checkpoint.
	pub compaction_epoch: u64,
}

/// Detached summarizer completion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchSummaryResult {
	/// Exact request that produced the summary.
	pub request: BranchSummaryRequest,
	/// Structured branch summary text.
	pub summary: Str,
}

/// Fences one detached branch summary against cancellation and checkpoint
/// drift.
#[derive(Clone, Debug, Default)]
pub struct BranchSummaryCoordinator {
	next_run: u64,
	running:  Option<BranchSummaryRequest>,
}

impl BranchSummaryCoordinator {
	/// Starts a detached summary, cancelling and returning any prior run id.
	pub fn start(
		&mut self,
		checkpoint: u64,
		model: ModelRef,
		thinking: Option<Str>,
		token_reserve: u64,
		branch_text: Str,
		compaction_epoch: u64,
	) -> (BranchSummaryRequest, Option<u64>) {
		let cancelled = self.cancel();
		self.next_run = self.next_run.wrapping_add(1).max(1);
		let request = BranchSummaryRequest {
			run_id: self.next_run,
			checkpoint,
			model,
			thinking,
			token_reserve,
			branch_text,
			compaction_epoch,
		};
		self.running = Some(request.clone());
		(request, cancelled)
	}

	/// Cancels the active summarizer, returning its run id to the executor.
	pub fn cancel(&mut self) -> Option<u64> {
		self.running.take().map(|request| request.run_id)
	}

	/// Accepts a completion only while its checkpoint and epoch remain current.
	pub fn finish(
		&mut self,
		result: BranchSummaryResult,
		current_checkpoint: u64,
		current_epoch: u64,
	) -> Option<Str> {
		let running = self.running.as_ref()?;
		if *running != result.request
			|| result.request.checkpoint != current_checkpoint
			|| result.request.compaction_epoch != current_epoch
		{
			return None;
		}
		self.running = None;
		Some(result.summary)
	}

	/// Returns whether detached work is active.
	#[must_use]
	pub const fn is_running(&self) -> bool {
		self.running.is_some()
	}
}
