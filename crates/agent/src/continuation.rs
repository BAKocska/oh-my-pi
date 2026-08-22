//! Settled-boundary continuation decisions and recursive ledger accounting.

use std::time::Duration;

use bytes::BytesMut;
use omp_core::Str;
use omp_proto::{thread::v1::Item, toolhost::v1::HookEventId};

use crate::{
	hooks::{AgentSettled, GateError, HookEvent, HookPatch},
	mailbox::InterruptSource,
};

/// Consecutive-continuation accounting projected from durable journal facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContinuationLedger {
	/// Accepted continuations since the last real user item.
	pub consecutive: u32,
	/// Total accepted continuations over the agent lifetime.
	pub total:       u64,
	/// Effective cap after policy and ancestor clamping.
	pub cap:         u32,
	/// Epoch milliseconds of the last accepted continuation.
	pub last_ms:     u64,
	/// Count of explicit refusals, which callers must journal rather than drop.
	pub refusals:    u32,
	/// Extension that won the latest continuation decision.
	pub owner:       Option<Str>,
}
/// Per-owner bounds applied before the session-wide continuation ceiling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContinuationPolicy {
	/// Maximum consecutive continuations since real user input.
	pub max_consecutive:  u32,
	/// Optional lifetime continuation ceiling.
	pub max_total:        Option<u64>,
	/// Minimum spacing between accepted continuations.
	pub min_interval:     Duration,
	/// Whether exhaustion should produce a user-visible notification.
	pub notify_exhausted: bool,
}

impl Default for ContinuationPolicy {
	fn default() -> Self {
		Self {
			max_consecutive:  8,
			max_total:        None,
			min_interval:     Duration::ZERO,
			notify_exhausted: true,
		}
	}
}

/// Core-owned repetition and progress evidence consumed by autonomous modes.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LoopSignal {
	/// Consecutive turns with the same committed tool-call digest.
	pub repeats:              u32,
	/// Stable digest of the latest committed tool-call shape.
	pub digest:               Option<Str>,
	/// Consecutive turns without an environment effect.
	pub no_progress_turns:    u32,
	/// Empty-output retries already spent by the core.
	pub empty_output_retries: u8,
	/// Conservative composite used to stop autonomous continuation.
	pub stalled:              bool,
}

impl LoopSignal {
	/// Folds one committed turn into bounded loop evidence.
	pub fn observe(
		&mut self,
		digest: Option<Str>,
		made_environment_effect: bool,
		empty_output_retries: u8,
	) {
		self.repeats = if digest.is_some() && digest == self.digest {
			self.repeats.saturating_add(1)
		} else {
			u32::from(digest.is_some())
		};
		self.digest = digest;
		self.no_progress_turns = if made_environment_effect {
			0
		} else {
			self.no_progress_turns.saturating_add(1)
		};
		self.empty_output_retries = empty_output_retries.min(3);
		self.stalled =
			self.repeats >= 3 || self.no_progress_turns >= 3 || self.empty_output_retries >= 3;
	}
}
/// Application-owned autonomous-mode decision consumed only at the settled
/// boundary.
pub trait ContinuationSource: Send + Sync {
	/// Returns a candidate and its owner policy from Core loop evidence.
	fn decide(&self, signal: &LoopSignal, now_ms: u64) -> (Continuation, ContinuationPolicy);
}

impl ContinuationLedger {
	/// Creates a zeroed ledger with an already-clamped cap.
	#[must_use]
	pub const fn new(cap: u32) -> Self {
		Self { consecutive: 0, total: 0, cap, last_ms: 0, refusals: 0, owner: None }
	}

	/// Resets the consecutive count after a real user item.
	pub const fn reset_for_user(&mut self) {
		self.consecutive = 0;
	}

	/// Applies one candidate decision, returning a refusal that must be
	/// journaled.
	pub fn decide(&mut self, candidate: Continuation, now_ms: u64) -> Continuation {
		match candidate {
			Continuation::Continue { .. } if self.consecutive >= self.cap => {
				self.refusals = self.refusals.saturating_add(1);
				Continuation::Refused { cap: self.cap }
			},
			Continuation::Continue { owner, item, label, collapse_prior } => {
				self.consecutive = self.consecutive.saturating_add(1);
				self.total = self.total.saturating_add(1);
				self.last_ms = now_ms;
				self.owner = Some(owner.clone());
				Continuation::Continue { owner, item, label, collapse_prior }
			},
			other => other,
		}
	}

	/// Applies one candidate under both an owner policy and the session cap.
	pub fn decide_with_policy(
		&mut self,
		candidate: Continuation,
		now_ms: u64,
		policy: ContinuationPolicy,
	) -> Continuation {
		let effective_cap = self.cap.min(policy.max_consecutive);
		let exhausted = self.consecutive >= effective_cap
			|| policy
				.max_total
				.is_some_and(|maximum| self.total >= maximum)
			|| (self.last_ms != 0
				&& now_ms.saturating_sub(self.last_ms)
					< u64::try_from(policy.min_interval.as_millis()).unwrap_or(u64::MAX));
		if matches!(candidate, Continuation::Continue { .. }) && exhausted {
			self.refusals = self.refusals.saturating_add(1);
			return Continuation::Refused { cap: effective_cap };
		}
		self.decide(candidate, now_ms)
	}
}

/// What the settled boundary decided for the next loop action.
#[derive(Clone, Debug, PartialEq)]
pub enum Continuation {
	/// Leave the agent settled.
	Settle,
	/// Start another turn with a canonical item after deferred-interrupt
	/// handling.
	Continue {
		/// Extension that requested the continuation.
		owner:          Str,
		/// Canonical item appended through the normal mailbox path.
		item:           Item,
		/// Optional telemetry and journal label.
		label:          Option<Str>,
		/// Whether an earlier continuation item is replaced.
		collapse_prior: bool,
	},
	/// A cap refusal that is retained as a durable ledger fact.
	Refused {
		/// Effective cap that rejected the candidate.
		cap: u32,
	},
}

/// Hook payload emitted at the settled boundary.
#[derive(Clone, Debug)]
pub struct AgentSettledEvent {
	/// Stable agent identity.
	pub agent_id: Str,
	/// The terminal turn id that reached the boundary.
	pub turn_id:  Str,
}

impl HookEvent for AgentSettledEvent {
	type Return = AgentSettled;

	const ID: HookEventId = HookEventId::HookEventAgentSettled;
	const REV: u32 = 1;

	fn encode_into(&self, out: &mut BytesMut) {
		out.extend_from_slice(self.agent_id.as_bytes());
		out.extend_from_slice(b"\n");
		out.extend_from_slice(self.turn_id.as_bytes());
	}

	fn apply(&mut self, _: &HookPatch) -> Result<(), GateError> {
		// Domain events never accept transforms.
		Ok(())
	}
}

/// Converts a hook's fail-open settled result into a loop continuation.
#[must_use]
pub fn from_hook(result: AgentSettled, owner: Str, item: Item) -> Continuation {
	match result {
		AgentSettled::Continue => {
			Continuation::Continue { owner, item, label: None, collapse_prior: false }
		},
		AgentSettled::Settle => Continuation::Settle,
	}
}

/// Returns whether an interrupt source is permitted to start another loop turn.
///
/// Detached job settlement is deliberately excluded: job facts are next-turn
/// data, not an autonomous-loop signal.
#[must_use]
pub const fn continues_loop(source: &InterruptSource) -> bool {
	matches!(
		source,
		InterruptSource::Producer(_)
			| InterruptSource::Continuation { .. }
			| InterruptSource::Schedule { .. }
			| InterruptSource::Peer { .. }
			| InterruptSource::DeferredDiagnostics { .. }
	)
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn owner_policy_clamps_and_spaces_continuations() {
		let mut ledger = ContinuationLedger::new(8);
		let policy = ContinuationPolicy {
			max_consecutive:  2,
			max_total:        Some(4),
			min_interval:     Duration::from_millis(10),
			notify_exhausted: true,
		};
		let candidate = || Continuation::Continue {
			owner:          sf!("goal"),
			item:           Item::default(),
			label:          None,
			collapse_prior: true,
		};
		assert!(matches!(
			ledger.decide_with_policy(candidate(), 100, policy),
			Continuation::Continue { .. }
		));
		assert_eq!(ledger.decide_with_policy(candidate(), 105, policy), Continuation::Refused {
			cap: 2,
		});
		assert!(matches!(
			ledger.decide_with_policy(candidate(), 110, policy),
			Continuation::Continue { .. }
		));
		assert_eq!(ledger.decide_with_policy(candidate(), 120, policy), Continuation::Refused {
			cap: 2,
		});
	}

	#[test]
	fn loop_signal_detects_repetition_and_no_progress() {
		let mut signal = LoopSignal::default();
		for _ in 0..3 {
			signal.observe(Some(sf!("same")), false, 0);
		}
		assert_eq!(signal.repeats, 3);
		assert_eq!(signal.no_progress_turns, 3);
		assert!(signal.stalled);
	}

	#[test]
	fn deferable_continuation_source_continues_the_loop() {
		assert!(continues_loop(&InterruptSource::Continuation { owner: sf!("goal") }));
	}

	#[test]
	fn schedule_source_continues_the_loop() {
		assert!(continues_loop(&InterruptSource::Schedule { id: sf!("nightly") }));
	}

	#[test]
	fn peer_source_continues_the_loop() {
		assert!(continues_loop(&InterruptSource::Peer { from: sf!("reviewer") }));
	}
}
