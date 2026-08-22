//! Bounded advisor history delivery and interruption accounting.

use std::{collections::VecDeque, sync::Arc};

use omp_core::Str;

/// Maximum primary entries retained for advisor catch-up by default.
pub const DEFAULT_HISTORY_ENTRY_LIMIT: usize = 256;
/// Maximum approximate payload bytes retained for advisor catch-up by default.
pub const DEFAULT_HISTORY_BYTE_LIMIT: usize = 512 * 1024;

/// Severity requested by an advisor.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum AdviceSeverity {
	/// Non-interrupting cleanup or optional improvement.
	Nit,
	/// Material risk which should steer when policy permits.
	Concern,
	/// Broken work which may wake an otherwise completed turn.
	Blocker,
}

/// Primary-mailbox delivery selected for one accepted note.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum AdviceDelivery {
	/// Batch into the next primary step boundary.
	Aside,
	/// Interrupt or trigger a primary turn.
	Steer,
	/// Preserve as a visible card without waking the primary.
	Preserve,
}

/// Current primary-loop facts used to evaluate advisor delivery.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DeliveryContext {
	/// A primary turn is currently streaming.
	pub streaming:              bool,
	/// The stopped turn ended with a terminal text answer.
	pub terminal_answer:        bool,
	/// Work remains queued after the current boundary.
	pub queued_work:            bool,
	/// The user or another external authority deliberately stopped the run.
	pub externally_interrupted: bool,
	/// Plan mode forbids advisor-driven turns.
	pub plan_mode:              bool,
	/// The client cannot represent an idle advisor-driven turn.
	pub deferred_client_turns:  bool,
	/// The advisor is reviewing a partial primary update.
	pub update_in_progress:     bool,
}

/// Monotonic immune-window projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImmuneTurnAccount {
	configured:        u32,
	remaining:         u32,
	last_completed_id: Option<u64>,
}

impl ImmuneTurnAccount {
	/// Creates accounting with the configured number of completed primary turns.
	#[must_use]
	pub const fn new(configured: u32) -> Self {
		Self { configured, remaining: 0, last_completed_id: None }
	}

	/// Arms the full immune window after a note actually used the steering
	/// channel.
	pub const fn record_steer(&mut self) {
		self.remaining = self.configured;
	}

	/// Accounts one newly completed primary turn.
	///
	/// Repeated settlement notifications for the same or an older turn id do not
	/// consume the window, which keeps retries and replay idempotent.
	pub fn record_primary_completion(&mut self, turn_id: u64) {
		if self.last_completed_id.is_some_and(|last| turn_id <= last) {
			return;
		}
		self.last_completed_id = Some(turn_id);
		self.remaining = self.remaining.saturating_sub(1);
	}

	/// Remaining primary completions before interrupting advice is enabled.
	#[must_use]
	pub const fn remaining(&self) -> u32 {
		self.remaining
	}

	/// Chooses the pi-parity route without mutating accounting.
	#[must_use]
	pub fn evaluate(&self, severity: AdviceSeverity, context: DeliveryContext) -> AdviceDelivery {
		if severity == AdviceSeverity::Nit {
			return AdviceDelivery::Aside;
		}
		if context.externally_interrupted || context.plan_mode {
			return AdviceDelivery::Preserve;
		}
		if context.update_in_progress && severity != AdviceSeverity::Blocker {
			return AdviceDelivery::Aside;
		}
		if self.remaining > 0 {
			return AdviceDelivery::Aside;
		}
		if context.streaming {
			return AdviceDelivery::Steer;
		}
		if context.deferred_client_turns {
			return AdviceDelivery::Preserve;
		}
		if context.terminal_answer && !context.queued_work && severity == AdviceSeverity::Concern {
			return AdviceDelivery::Preserve;
		}
		AdviceDelivery::Steer
	}
}

impl Default for ImmuneTurnAccount {
	fn default() -> Self {
		Self::new(3)
	}
}

/// One retained primary-history entry with an absolute cursor and bounded size.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdvisorHistoryEntry<T> {
	/// Absolute monotonically increasing source cursor.
	pub cursor: u64,
	/// Approximate rendered bytes charged against the history bound.
	pub bytes:  usize,
	/// Immutable entry payload.
	pub value:  T,
}

/// A bounded advisor delta returned to one runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdvisorHistoryDelta<T> {
	/// Cursor after the last returned entry.
	pub next_cursor: u64,
	/// Whether the requested cursor predates retained history and requires
	/// re-prime.
	pub reset:       bool,
	/// Oldest-to-newest retained entries.
	pub entries:     Arc<[AdvisorHistoryEntry<T>]>,
}

/// Bounded append-only primary-history window shared by advisor runtimes.
#[derive(Clone, Debug)]
pub struct BoundedAdvisorHistory<T> {
	entries:        VecDeque<AdvisorHistoryEntry<T>>,
	entry_limit:    usize,
	byte_limit:     usize,
	retained_bytes: usize,
	next_cursor:    u64,
}

impl<T> BoundedAdvisorHistory<T> {
	/// Creates a history window. Zero bounds retain no entries but cursors still
	/// advance.
	#[must_use]
	pub fn new(entry_limit: usize, byte_limit: usize) -> Self {
		Self { entries: VecDeque::new(), entry_limit, byte_limit, retained_bytes: 0, next_cursor: 0 }
	}

	/// Appends one immutable source entry and returns its absolute cursor.
	pub fn push(&mut self, value: T, bytes: usize) -> u64 {
		let cursor = self.next_cursor;
		self.next_cursor = self.next_cursor.saturating_add(1);
		self.retained_bytes = self.retained_bytes.saturating_add(bytes);
		self
			.entries
			.push_back(AdvisorHistoryEntry { cursor, bytes, value });
		while self.entries.len() > self.entry_limit
			|| self.retained_bytes > self.byte_limit
			|| (self.entry_limit == 0 || self.byte_limit == 0) && !self.entries.is_empty()
		{
			if let Some(removed) = self.entries.pop_front() {
				self.retained_bytes = self.retained_bytes.saturating_sub(removed.bytes);
			}
		}
		cursor
	}

	/// Cursor after the newest observed source entry.
	#[must_use]
	pub const fn next_cursor(&self) -> u64 {
		self.next_cursor
	}

	/// Clears retained entries after a primary-history rewrite without rewinding
	/// absolute cursors.
	pub fn reset(&mut self) {
		self.entries.clear();
		self.retained_bytes = 0;
		self.next_cursor = self.next_cursor.saturating_add(1);
	}
}

impl<T: Clone> BoundedAdvisorHistory<T> {
	/// Returns entries at or after `cursor`, signaling re-prime if that cursor
	/// was evicted.
	#[must_use]
	pub fn delta_after(&self, cursor: u64) -> AdvisorHistoryDelta<T> {
		let oldest = self
			.entries
			.front()
			.map_or(self.next_cursor, |entry| entry.cursor);
		let reset = cursor < oldest || cursor > self.next_cursor;
		let start = if reset { oldest } else { cursor };
		let entries = self
			.entries
			.iter()
			.filter(|entry| entry.cursor >= start)
			.cloned()
			.collect::<Vec<_>>()
			.into();
		AdvisorHistoryDelta { next_cursor: self.next_cursor, reset, entries }
	}
}

impl<T> Default for BoundedAdvisorHistory<T> {
	fn default() -> Self {
		Self::new(DEFAULT_HISTORY_ENTRY_LIMIT, DEFAULT_HISTORY_BYTE_LIMIT)
	}
}

/// Durable advisor child identity and private-context cursor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdvisorRuntimeState {
	/// Stable child id.
	pub id:             Str,
	/// Owning primary session id.
	pub parent_id:      Str,
	/// Durable display label.
	pub display_name:   Str,
	/// Next primary history cursor to consume.
	pub history_cursor: u64,
	/// Separate advisor usage totals.
	pub input_tokens:   u64,
	/// Separate advisor usage totals.
	pub output_tokens:  u64,
}
#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn immune_account_counts_each_completed_turn_once() {
		let mut account = ImmuneTurnAccount::new(3);
		account.record_steer();
		account.record_primary_completion(10);
		account.record_primary_completion(10);
		assert_eq!(account.remaining(), 2);
		assert_eq!(
			account.evaluate(AdviceSeverity::Concern, DeliveryContext {
				streaming: true,
				..Default::default()
			}),
			AdviceDelivery::Aside
		);
		account.record_primary_completion(11);
		account.record_primary_completion(12);
		assert_eq!(
			account.evaluate(AdviceSeverity::Concern, DeliveryContext {
				streaming: true,
				..Default::default()
			}),
			AdviceDelivery::Steer
		);
	}

	#[test]
	fn bounded_history_requires_reprime_after_eviction_and_rewrite() {
		let mut history = BoundedAdvisorHistory::new(2, 16);
		history.push("one", 3);
		history.push("two", 3);
		history.push("three", 5);
		let evicted = history.delta_after(0);
		assert!(evicted.reset);
		assert_eq!(evicted.entries.len(), 2);

		let cursor = evicted.next_cursor;
		history.reset();
		history.push("replacement", 11);
		let rewritten = history.delta_after(cursor);
		assert!(rewritten.reset);
		assert_eq!(rewritten.entries[0].value, "replacement");
	}
}
