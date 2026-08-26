//! Ordered live-block allocation and retirement scheduling.
//!
//! Every block receives a monotonic [`BlockOrdinal`] and moves through
//! [`BlockPhase::Queued`], [`BlockPhase::Active`],
//! [`BlockPhase::FinalizedPending`], and [`BlockPhase::Committed`]. The
//! scheduler treats sampled painted heights as authoritative: it first asks
//! existing blocks to contract, then admits queued blocks only after sampled
//! rows are physically free. Collapse requests pass through observed
//! two-row and one-row bridge heights.
//!
//! Finalization immediately hands a block's presentation to its settled
//! semantic snapshot: the block stops sampling, and the scene renders the
//! snapshot in the live viewport until ordered retirement moves it into
//! native history. [`Blocks::retirement_batch`] exposes only the maximal
//! finalized prefix at the monotonic commit frontier. Allocation never changes
//! that frontier. Display replay is owned separately by the scene and never
//! rewinds it.

use std::ops::Range;

use smallvec::SmallVec;

/// Monotonic creation order within one terminal surface.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BlockOrdinal(
	/// Zero-based sequence number, never reused by this scheduler.
	pub u64,
);

/// Lifecycle state of one ordered block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockPhase {
	/// Waiting for at least one physically free live row.
	Queued,
	/// Admitted to the live viewport allocator.
	Active,
	/// Finalized, removed from active sampling, and waiting for ordered
	/// retirement.
	FinalizedPending,
	/// Successfully retired into native history.
	Committed,
}

/// One scheduler-owned live height request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockTarget {
	/// Block receiving the request.
	pub ordinal: BlockOrdinal,
	/// Requested integer row height for this scheduler step.
	pub height:  u16,
}

/// Viewport-only fallback when all active blocks cannot each occupy one row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Overflow {
	/// Most-recent active blocks that fit the caller-supplied row budget.
	pub visible: SmallVec<BlockOrdinal, 8>,
	/// Number of active blocks hidden outside that budget.
	pub hidden:  u32,
}

/// Result of one sampled-height allocation step.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Plan {
	/// Height requests for active blocks.
	///
	/// Queued, finalized, and committed blocks implicitly target zero and are
	/// omitted.
	pub targets:  SmallVec<BlockTarget, 8>,
	/// Blocks admitted from the FIFO queue during this step.
	pub admitted: SmallVec<BlockOrdinal, 4>,
	/// Resize-overflow presentation, when one row per active block is
	/// impossible.
	pub overflow: Option<Overflow>,
}

impl Plan {
	/// Returns this step's explicit target for `ordinal`.
	#[must_use]
	pub fn target(&self, ordinal: BlockOrdinal) -> Option<u16> {
		self
			.targets
			.iter()
			.find(|target| target.ordinal == ordinal)
			.map(|target| target.height)
	}
}

#[derive(Clone, Copy, Debug)]
struct BlockRecord {
	phase:  BlockPhase,
	target: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Admission {
	Allow,
	Defer,
}

/// Ordered block store, sampled-height allocator, and commit frontier.
#[derive(Debug, Default)]
pub struct Blocks {
	records:  Vec<BlockRecord>,
	frontier: u64,
}

impl Blocks {
	/// Creates an empty scheduler whose first ordinal and commit frontier are
	/// zero.
	#[must_use]
	pub const fn new() -> Self {
		Self { records: Vec::new(), frontier: 0 }
	}

	/// Creates one queued block and returns its never-reused ordinal.
	pub fn create(&mut self) -> BlockOrdinal {
		let ordinal = BlockOrdinal(self.records.len() as u64);
		self
			.records
			.push(BlockRecord { phase: BlockPhase::Queued, target: 0 });
		ordinal
	}

	/// Number of blocks not yet committed (queued, active, or settled).
	#[must_use]
	pub fn live_count(&self) -> u64 {
		self.records.len() as u64 - self.frontier
	}

	/// Finalizes an active or queued block.
	///
	/// The block's live presentation ends immediately: it stops sampling and
	/// its settled semantic snapshot becomes eligible for viewport rendering
	/// and ordered retirement. Returns `false` when the ordinal is unknown or
	/// was already finalized or committed.
	pub fn finalize(&mut self, ordinal: BlockOrdinal) -> bool {
		let Some(record) = self.record_mut(ordinal) else {
			return false;
		};
		match record.phase {
			BlockPhase::Queued | BlockPhase::Active => {
				record.phase = BlockPhase::FinalizedPending;
				record.target = 0;
				true
			},
			BlockPhase::FinalizedPending | BlockPhase::Committed => false,
		}
	}

	/// Runs one allocation step from authoritative sampled painted heights.
	///
	/// `natural` is consulted only for active blocks. Growth is granted only
	/// from rows not occupied by a sample or an outstanding prior growth grant.
	/// Queued blocks are admitted FIFO, one row at a time, after the same
	/// physical-free-row accounting.
	pub fn tick(
		&mut self,
		h_live: u16,
		sampled: impl Fn(BlockOrdinal) -> u16,
		natural: impl Fn(BlockOrdinal) -> u16,
	) -> Plan {
		self.tick_with(h_live, sampled, natural, Admission::Allow)
	}

	/// Computes contraction targets without admitting queued blocks while a
	/// terminal retirement transaction is still unacknowledged.
	pub(crate) fn tick_without_admission(
		&mut self,
		h_live: u16,
		sampled: impl Fn(BlockOrdinal) -> u16,
		natural: impl Fn(BlockOrdinal) -> u16,
	) -> Plan {
		self.tick_with(h_live, sampled, natural, Admission::Defer)
	}

	fn tick_with(
		&mut self,
		h_live: u16,
		sampled: impl Fn(BlockOrdinal) -> u16,
		natural: impl Fn(BlockOrdinal) -> u16,
		admission: Admission,
	) -> Plan {
		let mut samples = SmallVec::<Sample, 8>::new();
		let mut active_ordinals = SmallVec::<BlockOrdinal, 8>::new();
		for (index, record) in self.records.iter().enumerate() {
			let ordinal = BlockOrdinal(index as u64);
			match record.phase {
				BlockPhase::Active => {
					active_ordinals.push(ordinal);
					samples.push(Sample {
						index,
						ordinal,
						height: sampled(ordinal),
						natural: natural(ordinal).max(1),
					});
				},
				BlockPhase::Queued | BlockPhase::FinalizedPending | BlockPhase::Committed => {},
			}
		}

		if active_ordinals.len() > usize::from(h_live) {
			return self.overflow_plan(h_live, &samples, &active_ordinals);
		}

		let queue_waiting = self
			.records
			.iter()
			.any(|record| record.phase == BlockPhase::Queued);
		let mut plan = Plan::default();
		let mut occupied = 0_u32;

		for sample in &samples {
			let record = &mut self.records[sample.index];
			let target = match record.phase {
				BlockPhase::Active if queue_waiting => collapse_target(sample.height),
				BlockPhase::Active => settled_target(sample.height, record.target, sample.natural),
				BlockPhase::Queued | BlockPhase::FinalizedPending | BlockPhase::Committed => 0,
			};
			record.target = target;
			occupied = occupied.saturating_add(u32::from(sample.height.max(target)));
			plan
				.targets
				.push(BlockTarget { ordinal: sample.ordinal, height: target });
		}

		let mut free = u32::from(h_live).saturating_sub(occupied);
		if queue_waiting && admission == Admission::Allow {
			for (index, record) in self.records.iter_mut().enumerate() {
				if free == 0 {
					break;
				}
				if record.phase != BlockPhase::Queued {
					continue;
				}
				record.phase = BlockPhase::Active;
				record.target = 1;
				let ordinal = BlockOrdinal(index as u64);
				plan.targets.push(BlockTarget { ordinal, height: 1 });
				plan.admitted.push(ordinal);
				free -= 1;
			}
		} else if !queue_waiting {
			for (sample, target) in samples.iter().zip(plan.targets.iter_mut()) {
				if free == 0 {
					break;
				}
				let record = &mut self.records[sample.index];
				if record.phase != BlockPhase::Active || target.height >= sample.natural {
					continue;
				}
				let grant = u32::from(sample.natural - target.height).min(free) as u16;
				target.height += grant;
				record.target = target.height;
				free -= u32::from(grant);
			}
		}

		self.fit_targets(h_live, &samples, &mut plan.targets);
		plan
	}

	/// Returns the maximal retirement-ready contiguous range starting at the
	/// commit frontier.
	#[must_use]
	pub fn retirement_batch(&self) -> Option<Range<u64>> {
		let start = self.frontier;
		let mut end = start;
		while let Some(record) = self.record(BlockOrdinal(end)) {
			if record.phase != BlockPhase::FinalizedPending {
				break;
			}
			end += 1;
		}
		(start != end).then_some(start..end)
	}

	/// Marks a successfully retired prefix committed and advances the frontier.
	///
	/// In debug builds a request that skips, repeats, or crosses an unready
	/// block panics. Release builds saturate the request to the currently
	/// retirement-ready contiguous prefix, preserving ordered exactly-once
	/// commits.
	pub fn mark_committed(&mut self, upto: u64) {
		let start = self.frontier;
		let requested = upto.min(self.records.len() as u64);
		let mut safe_end = start;
		if upto >= start {
			while safe_end < requested {
				let record = &self.records[safe_end as usize];
				if record.phase != BlockPhase::FinalizedPending {
					break;
				}
				safe_end += 1;
			}
		}
		let contiguous = upto >= start && upto <= self.records.len() as u64 && safe_end == upto;
		debug_assert!(contiguous, "commit must advance across one contiguous finalized prefix");
		for ordinal in start..safe_end {
			let record = &mut self.records[ordinal as usize];
			record.phase = BlockPhase::Committed;
			record.target = 0;
		}
		self.frontier = safe_end;
	}

	/// Returns the first ordinal not yet committed.
	#[must_use]
	pub const fn frontier(&self) -> u64 {
		self.frontier
	}

	/// Returns the current phase of a known ordinal.
	#[must_use]
	pub fn phase(&self, ordinal: BlockOrdinal) -> Option<BlockPhase> {
		self.record(ordinal).map(|record| record.phase)
	}

	fn overflow_plan(
		&mut self,
		h_live: u16,
		samples: &[Sample],
		active_ordinals: &[BlockOrdinal],
	) -> Plan {
		let visible_count = usize::from(h_live).min(active_ordinals.len());
		let visible = active_ordinals[active_ordinals.len() - visible_count..]
			.iter()
			.copied()
			.collect();
		let hidden = u32::try_from(active_ordinals.len() - visible_count).unwrap_or(u32::MAX);
		let mut plan = Plan {
			targets:  SmallVec::new(),
			admitted: SmallVec::new(),
			overflow: Some(Overflow { visible, hidden }),
		};
		for sample in samples {
			let record = &mut self.records[sample.index];
			let height = u16::from(
				plan
					.overflow
					.as_ref()
					.is_some_and(|overflow| overflow.visible.contains(&sample.ordinal)),
			);
			record.target = height;
			plan
				.targets
				.push(BlockTarget { ordinal: sample.ordinal, height });
		}
		plan
	}

	fn fit_targets(&mut self, h_live: u16, samples: &[Sample], targets: &mut [BlockTarget]) {
		let sampled_total = samples
			.iter()
			.map(|sample| u32::from(sample.height))
			.sum::<u32>();
		let growth_budget = u32::from(h_live).saturating_sub(sampled_total);
		let mut excess_growth = targets
			.iter()
			.enumerate()
			.map(|(index, target)| {
				let sampled = samples.get(index).map_or(0, |sample| {
					debug_assert_eq!(sample.ordinal, target.ordinal);
					sample.height
				});
				u32::from(target.height.saturating_sub(sampled))
			})
			.sum::<u32>()
			.saturating_sub(growth_budget);
		for (index, target) in targets.iter_mut().enumerate().rev() {
			if excess_growth == 0 {
				break;
			}
			let sampled = samples.get(index).map_or(0, |sample| sample.height);
			let minimum = if self.phase(target.ordinal) == Some(BlockPhase::Active) {
				1
			} else {
				0
			};
			let reduction =
				u32::from(target.height.saturating_sub(sampled.max(minimum))).min(excess_growth) as u16;
			target.height -= reduction;
			excess_growth -= u32::from(reduction);
		}

		let mut excess = targets
			.iter()
			.map(|target| u32::from(target.height))
			.sum::<u32>()
			.saturating_sub(u32::from(h_live));
		for target in targets.iter_mut().rev() {
			if excess == 0 {
				break;
			}
			let minimum = match self.phase(target.ordinal) {
				Some(BlockPhase::Active) => 1,
				Some(BlockPhase::Queued)
				| Some(BlockPhase::FinalizedPending)
				| Some(BlockPhase::Committed)
				| None => 0,
			};
			let reduction = u32::from(target.height.saturating_sub(minimum)).min(excess) as u16;
			target.height -= reduction;
			excess -= u32::from(reduction);
		}
		for target in targets {
			if let Some(record) = self.record_mut(target.ordinal) {
				record.target = target.height;
			}
		}
	}

	fn record(&self, ordinal: BlockOrdinal) -> Option<&BlockRecord> {
		usize::try_from(ordinal.0)
			.ok()
			.and_then(|index| self.records.get(index))
	}

	fn record_mut(&mut self, ordinal: BlockOrdinal) -> Option<&mut BlockRecord> {
		usize::try_from(ordinal.0)
			.ok()
			.and_then(|index| self.records.get_mut(index))
	}
}

#[derive(Clone, Copy, Debug)]
struct Sample {
	index:   usize,
	ordinal: BlockOrdinal,
	height:  u16,
	natural: u16,
}

fn collapse_target(sampled: u16) -> u16 {
	if sampled > 2 { 2 } else { 1 }
}

fn settled_target(sampled: u16, previous: u16, natural: u16) -> u16 {
	if natural < sampled {
		if natural == 1 {
			collapse_target(sampled)
		} else {
			natural
		}
	} else {
		previous.max(sampled).max(1).min(natural)
	}
}

#[cfg(test)]
mod tests {
	use std::collections::BTreeSet;

	use proptest::prelude::*;

	use super::*;

	fn sample_from(values: &[u16], ordinal: BlockOrdinal) -> u16 {
		values.get(ordinal.0 as usize).copied().unwrap_or(0)
	}

	#[test]
	fn full_one_row_live_region_backpressures_next_create() {
		let mut blocks = Blocks::new();
		let first = blocks.create();
		let second = blocks.create();
		let waiting = blocks.create();
		let plan = blocks.tick(2, |_| 0, |_| 4);
		assert_eq!(plan.admitted.as_slice(), &[first, second]);

		let sampled = [1, 1, 0];
		let plan = blocks.tick(2, |ordinal| sample_from(&sampled, ordinal), |_| 4);
		assert!(plan.admitted.is_empty());
		assert_eq!(blocks.phase(waiting), Some(BlockPhase::Queued));
		assert_eq!(plan.targets.iter().map(|target| target.height).sum::<u16>(), 2);
	}

	#[test]
	fn finalization_releases_the_live_row_to_the_next_waiter() {
		let mut blocks = Blocks::new();
		let first = blocks.create();
		let second = blocks.create();
		let waiting = blocks.create();
		blocks.tick(2, |_| 0, |_| 3);
		assert!(blocks.finalize(first));

		// The finalized block stops sampling immediately: its settled snapshot
		// now occupies scene-owned rows outside this allocator's budget, so the
		// queued waiter is admitted into the released allocator row.
		let sampled = [1, 1, 0];
		let released = blocks.tick(2, |ordinal| sample_from(&sampled, ordinal), |_| 3);
		assert!(released.target(first).is_none());
		assert_eq!(released.admitted.as_slice(), &[waiting]);
		assert_eq!(blocks.phase(second), Some(BlockPhase::Active));
		assert_eq!(blocks.phase(waiting), Some(BlockPhase::Active));
	}

	#[test]
	fn queue_pressure_collapses_across_observed_bridge_rows() {
		let mut blocks = Blocks::new();
		let donor = blocks.create();
		blocks.tick(5, |_| 0, |_| 5);
		let expanded = blocks.tick(5, |_| 1, |_| 5);
		assert_eq!(expanded.target(donor), Some(5));
		let waiting = blocks.create();

		let to_two = blocks.tick(5, |_| 5, |_| 5);
		assert_eq!(to_two.target(donor), Some(2));
		assert!(to_two.admitted.is_empty());
		let still_two = blocks.tick(5, |_| 3, |_| 5);
		assert_eq!(still_two.target(donor), Some(2));
		assert_eq!(still_two.admitted.as_slice(), &[waiting]);

		// Finalization needs no exit bridge: the settled snapshot is
		// immediately eligible at the retirement frontier.
		assert!(blocks.finalize(donor));
		assert_eq!(blocks.retirement_batch(), Some(0..1));
	}

	#[test]
	fn later_finalization_hides_behind_unfinished_head() {
		let mut blocks = Blocks::new();
		let head = blocks.create();
		let later = blocks.create();
		blocks.tick(2, |_| 0, |_| 1);
		assert!(blocks.finalize(later));
		let sampled = [1, 1];
		blocks.tick(2, |ordinal| sample_from(&sampled, ordinal), |_| 1);
		let sampled = [1, 0];
		blocks.tick(2, |ordinal| sample_from(&sampled, ordinal), |_| 1);
		assert_eq!(blocks.phase(head), Some(BlockPhase::Active));
		assert_eq!(blocks.phase(later), Some(BlockPhase::FinalizedPending));
		assert!(blocks.retirement_batch().is_none());
		assert_eq!(blocks.frontier(), 0);
	}

	#[test]
	fn head_completion_exposes_one_contiguous_ordered_batch() {
		let mut blocks = Blocks::new();
		let head = blocks.create();
		let later = blocks.create();
		blocks.tick(2, |_| 0, |_| 1);
		assert!(blocks.finalize(later));
		assert!(blocks.retirement_batch().is_none());
		assert!(blocks.finalize(head));

		assert_eq!(blocks.retirement_batch(), Some(0..2));
		blocks.mark_committed(2);
		assert_eq!(blocks.frontier(), 2);
		assert_eq!(blocks.phase(head), Some(BlockPhase::Committed));
		assert_eq!(blocks.phase(later), Some(BlockPhase::Committed));
		assert!(blocks.retirement_batch().is_none());
	}

	#[test]
	fn concurrent_lagging_shrink_and_expand_never_overgrant_rows() {
		let mut budget = Blocks::new();
		let growing = budget.create();
		let occupying = budget.create();
		budget.tick(5, |_| 0, |_| 4);
		let prior_grant = budget.tick(5, |_| 1, |_| 4);
		assert_eq!(prior_grant.target(growing), Some(4));
		assert_eq!(prior_grant.target(occupying), Some(1));
		let sampled = [1, 4];
		let revoked = budget.tick(5, |ordinal| sample_from(&sampled, ordinal), |_| 4);
		assert_eq!(revoked.target(growing), Some(1));
		assert_eq!(revoked.target(occupying), Some(4));

		let mut blocks = Blocks::new();
		let a = blocks.create();
		let b = blocks.create();
		let c = blocks.create();
		blocks.tick(6, |_| 0, |_| 4);
		let expanded = blocks.tick(6, |_| 1, |_| 4);
		assert_eq!(
			expanded
				.targets
				.iter()
				.map(|target| target.height)
				.sum::<u16>(),
			6
		);

		let waiting = blocks.create();
		let sampled = [3, 1, 1, 0];
		let shrinking = blocks.tick(6, |ordinal| sample_from(&sampled, ordinal), |_| 4);
		assert_eq!(shrinking.admitted.as_slice(), &[waiting]);
		assert!(
			shrinking
				.targets
				.iter()
				.map(|target| target.height)
				.sum::<u16>()
				<= 6
		);

		let lagged = [3, 1, 1, 0];
		let held = blocks.tick(6, |ordinal| sample_from(&lagged, ordinal), |_| 4);
		assert!(held.targets.iter().map(|target| target.height).sum::<u16>() <= 6);
		assert_eq!(blocks.phase(a), Some(BlockPhase::Active));
		assert_eq!(blocks.phase(b), Some(BlockPhase::Active));
		assert_eq!(blocks.phase(c), Some(BlockPhase::Active));
	}

	#[test]
	fn resize_below_active_count_uses_recent_rows_without_committing() {
		let mut blocks = Blocks::new();
		let first = blocks.create();
		let second = blocks.create();
		let third = blocks.create();
		blocks.tick(3, |_| 0, |_| 1);
		let plan = blocks.tick(2, |_| 1, |_| 1);
		let overflow = plan.overflow.as_ref().expect("active overflow");
		assert_eq!(overflow.visible.as_slice(), &[second, third]);
		assert_eq!(overflow.hidden, 1);
		assert_eq!(plan.target(first), Some(0));
		assert_eq!(plan.target(second), Some(1));
		assert_eq!(plan.target(third), Some(1));
		assert_eq!(blocks.frontier(), 0);
		assert!(blocks.retirement_batch().is_none());
	}

	#[test]
	fn failure_timeout_and_cancel_finalizations_unblock_frontier() {
		let mut blocks = Blocks::new();
		let outcomes = [blocks.create(), blocks.create(), blocks.create()];
		blocks.tick(3, |_| 0, |_| 1);
		for ordinal in outcomes.into_iter().rev() {
			assert!(blocks.finalize(ordinal));
		}
		assert_eq!(blocks.retirement_batch(), Some(0..3));
	}

	#[test]
	#[should_panic(expected = "commit must advance across one contiguous finalized prefix")]
	fn non_contiguous_commit_panics_in_debug_builds() {
		let mut blocks = Blocks::new();
		blocks.create();
		blocks.create();
		assert!(blocks.finalize(BlockOrdinal(1)));
		blocks.mark_committed(2);
	}
	#[derive(Clone, Copy)]
	struct ReferenceRecord {
		phase: BlockPhase,
	}

	#[derive(Default)]
	struct Reference {
		records:  Vec<ReferenceRecord>,
		frontier: u64,
	}

	impl Reference {
		fn create(&mut self) -> BlockOrdinal {
			let ordinal = BlockOrdinal(self.records.len() as u64);
			self
				.records
				.push(ReferenceRecord { phase: BlockPhase::Queued });
			ordinal
		}

		fn finalize(&mut self, ordinal: BlockOrdinal) -> bool {
			let record = &mut self.records[ordinal.0 as usize];
			match record.phase {
				BlockPhase::Queued | BlockPhase::Active => {
					record.phase = BlockPhase::FinalizedPending;
					true
				},
				BlockPhase::FinalizedPending | BlockPhase::Committed => false,
			}
		}

		fn apply_tick(&mut self, plan: &Plan) {
			for ordinal in &plan.admitted {
				self.records[ordinal.0 as usize].phase = BlockPhase::Active;
			}
		}

		fn retirement_batch(&self) -> Option<Range<u64>> {
			let mut end = self.frontier;
			while let Some(record) = self.records.get(end as usize) {
				if record.phase != BlockPhase::FinalizedPending {
					break;
				}
				end += 1;
			}
			(self.frontier != end).then_some(self.frontier..end)
		}

		fn mark_committed(&mut self, upto: u64) {
			for ordinal in self.frontier..upto {
				self.records[ordinal as usize].phase = BlockPhase::Committed;
			}
			self.frontier = upto;
		}
	}

	proptest! {
		#[test]
		fn randomized_traces_preserve_all_scheduler_invariants(
			steps in prop::collection::vec((any::<u8>(), any::<u16>(), any::<u16>()), 1..256),
		) {
			let mut blocks = Blocks::new();
			let mut reference = Reference::default();
			let mut sampled = Vec::<u16>::new();
			let mut last_targets = Vec::<u16>::new();
			let mut committed = BTreeSet::<u64>::new();
			let mut last_frontier = 0_u64;
			let mut finalized = BTreeSet::<u64>::new();

			for (operation, choice, lag) in steps {
				match operation % 5 {
					0 => {
						let ordinal = blocks.create();
						prop_assert_eq!(ordinal, reference.create());
						prop_assert_eq!(ordinal.0 as usize, sampled.len());
						sampled.push(0);
						last_targets.push(0);
					}
					1 | 2 => {
						if !sampled.is_empty() {
							let ordinal = BlockOrdinal(usize::from(choice) as u64 % sampled.len() as u64);
							let before = blocks.phase(ordinal);
							let changed = blocks.finalize(ordinal);
							prop_assert_eq!(changed, reference.finalize(ordinal));
							if changed {
								prop_assert!(matches!(before, Some(BlockPhase::Queued | BlockPhase::Active)));
								prop_assert!(finalized.insert(ordinal.0));
							}
						}
					}
					3 => {
						let batch = blocks.retirement_batch();
						let expected_batch = reference.retirement_batch();
						prop_assert_eq!(&batch, &expected_batch);
						if let Some(batch) = batch {
							prop_assert_eq!(batch.start, blocks.frontier());
							prop_assert!(batch.start < batch.end);
							for ordinal in batch.clone() {
								prop_assert_eq!(
									blocks.phase(BlockOrdinal(ordinal)),
									Some(BlockPhase::FinalizedPending)
								);
								prop_assert!(!committed.contains(&ordinal));
							}
							blocks.mark_committed(batch.end);
							reference.mark_committed(batch.end);
							for ordinal in batch {
								prop_assert!(committed.insert(ordinal));
							}
						}
					}
					_ => {}
				}

				let h_live = choice % 9;
				let plan = blocks.tick(
					h_live,
					|ordinal| sampled[ordinal.0 as usize],
					|ordinal| 1 + ((ordinal.0 as u16).wrapping_add(lag) % 6),
				);
				reference.apply_tick(&plan);
				let target_sum = plan.targets.iter().map(|target| u32::from(target.height)).sum::<u32>();
				prop_assert!(target_sum <= u32::from(h_live));
				if let Some(overflow) = &plan.overflow {
					prop_assert!(overflow.visible.len() <= usize::from(h_live));
					prop_assert_eq!(
						overflow.visible.len() as u32 + overflow.hidden,
						blocks.records.iter().filter(|record| record.phase == BlockPhase::Active).count() as u32,
					);
				}
				for target in &plan.targets {
					let phase = blocks.phase(target.ordinal).expect("planned ordinal");
					prop_assert!(phase == BlockPhase::Active);
					if plan.overflow.is_none() {
						prop_assert!(target.height >= 1);
					}
					last_targets[target.ordinal.0 as usize] = target.height;
				}

				for (index, height) in sampled.iter_mut().enumerate() {
					let target = last_targets[index];
					if *height < target && lag & 1 != 0 {
						*height += 1;
					} else if *height > target && lag & 2 != 0 {
						*height -= 1;
					}
				}

				prop_assert!(blocks.frontier() >= last_frontier);
				prop_assert_eq!(blocks.frontier(), reference.frontier);
				for (index, expected) in reference.records.iter().enumerate() {
					prop_assert_eq!(
						blocks.phase(BlockOrdinal(index as u64)),
						Some(expected.phase)
					);
				}
				last_frontier = blocks.frontier();
				prop_assert_eq!(committed.len() as u64, blocks.frontier());
				for ordinal in 0..blocks.frontier() {
					prop_assert!(committed.contains(&ordinal));
					prop_assert_eq!(blocks.phase(BlockOrdinal(ordinal)), Some(BlockPhase::Committed));
				}
				if let Some(batch) = blocks.retirement_batch() {
					prop_assert_eq!(batch.start, blocks.frontier());
					for ordinal in batch {
						prop_assert_eq!(blocks.phase(BlockOrdinal(ordinal)), Some(BlockPhase::FinalizedPending));
					}
				}
			}
		}
	}
}
