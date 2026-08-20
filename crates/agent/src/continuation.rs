//! Settled-boundary continuation decisions and recursive ledger accounting.

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

impl ContinuationLedger {
	/// Creates a zeroed ledger with an already-clamped cap.
	#[must_use]
	pub const fn new(cap: u32) -> Self {
		Self { consecutive: 0, total: 0, cap, last_ms: 0, refusals: 0, owner: None }
	}

	/// Resets the consecutive count after a real user item.
	pub fn reset_for_user(&mut self) {
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
	)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn deferable_continuation_source_continues_the_loop() {
		assert!(continues_loop(&InterruptSource::Continuation { owner: Str::from("goal") }));
	}

	#[test]
	fn schedule_source_continues_the_loop() {
		assert!(continues_loop(&InterruptSource::Schedule { id: Str::from("nightly") }));
	}

	#[test]
	fn peer_source_continues_the_loop() {
		assert!(continues_loop(&InterruptSource::Peer { from: Str::from("reviewer") }));
	}
}
