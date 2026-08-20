//! Core-owned durable approval ticket state.

use std::{
	collections::BTreeMap,
	sync::atomic::{AtomicU64, Ordering},
};

use omp_core::{Str, sf};
use parking_lot::Mutex;

/// One requirement merged into an invocation's single approval ticket.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalSpec {
	/// Short user-visible description.
	pub title:         Str,
	/// TML-safe explanatory text.
	pub body:          Str,
	/// Exact command, path, or device subject.
	pub subject:       Str,
	/// Presentation and configuration vocabulary such as `exec` or `write`.
	pub kind:          Str,
	/// Offered grant scopes in strictness order.
	pub scopes:        Vec<Str>,
	/// Optional timeout default; Core never invents one.
	pub default:       Option<bool>,
	/// Requested approver route.
	pub route:         Str,
	/// Optional named external approver.
	pub approver:      Option<Str>,
	/// Maximum wait in milliseconds.
	pub timeout_ms:    u64,
	/// Unreachable-route behavior.
	pub unreachable:   Str,
	/// Forbids extension-sourced decisions.
	pub require_human: bool,
	/// Scope-bearing approval pattern.
	pub pattern:       Option<Str>,
	/// Rule and derived-fact evidence.
	pub evidence:      Vec<Str>,
}

/// Durable state of an approval ticket.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum TicketState {
	/// Awaiting a single idempotent answer.
	Pending,
	/// Answered exactly once.
	Decided,
	/// Invocation ended before an answer.
	Withdrawn,
}

/// The source that supplied an approval result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ApprovalSource {
	/// A local user answered.
	User,
	/// An authenticated external approver answered.
	External,
	/// A parent agent answered.
	Forwarded,
	/// The frozen turn configuration pre-answered the ticket.
	Config,
	/// An authorized policy extension answered.
	Extension,
	/// An explicit timeout default answered.
	Timeout,
	/// An unreachable-route policy answered.
	Unavailable,
}

/// One idempotent durable approval result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalDecision {
	/// Whether all merged reasons are approved.
	pub approved:   bool,
	/// Granted policy scope.
	pub scope:      Str,
	/// Source of the answer.
	pub source:     ApprovalSource,
	/// Optional authenticated decider.
	pub decided_by: Option<Str>,
	/// Optional user-visible rationale.
	pub reason:     Option<Str>,
	/// Whether a fail-open result was durably audited.
	pub audited:    bool,
}

/// Core-owned ticket, independent of an extension coroutine lifetime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalTicket {
	/// Stable idempotency key for approvers.
	pub ticket_id:     Str,
	/// Invocation this ticket blocks, if any.
	pub invocation_id: Option<Str>,
	/// Every unresolved hook requirement in filing order.
	pub reasons:       Vec<ApprovalSpec>,
	/// Current durable state.
	pub state:         TicketState,
	/// Set only once `state` becomes `Decided`.
	pub decision:      Option<ApprovalDecision>,
	/// Journal-clock epoch milliseconds at filing.
	pub created_at_ms: u64,
}

impl ApprovalTicket {
	/// Converts this ticket to the typed transcript payload filed on creation or
	/// merge.
	#[must_use]
	pub fn filed_record(&self) -> omp_storage::transcript::ApprovalTicketFiled {
		omp_storage::transcript::ApprovalTicketFiled {
			ticket_id:     self.ticket_id.clone(),
			invocation_id: self.invocation_id.clone(),
			reasons:       self
				.reasons
				.iter()
				.map(|reason| omp_storage::transcript::ApprovalReason {
					title:         reason.title.clone(),
					body:          reason.body.clone(),
					subject:       reason.subject.clone(),
					kind:          reason.kind.clone(),
					scopes:        reason.scopes.clone(),
					default:       reason.default,
					route:         reason.route.clone(),
					approver:      reason.approver.clone(),
					timeout_ms:    reason.timeout_ms,
					unreachable:   reason.unreachable.clone(),
					require_human: reason.require_human,
					pattern:       reason.pattern.clone(),
					evidence:      reason.evidence.clone(),
				})
				.collect(),
			created_at_ms: self.created_at_ms,
		}
	}

	/// Converts a terminal decision or withdrawal to its typed transcript
	/// payload.
	#[must_use]
	pub fn decision_record(&self) -> Option<omp_storage::transcript::ApprovalDecided> {
		let state = match self.state {
			TicketState::Pending => return None,
			TicketState::Decided => sf!("decided"),
			TicketState::Withdrawn => sf!("withdrawn"),
		};
		let decision = self.decision.as_ref();
		Some(omp_storage::transcript::ApprovalDecided {
			ticket_id: self.ticket_id.clone(),
			state,
			approved: decision.map(|value| value.approved),
			scope: decision.map(|value| value.scope.clone()),
			source: decision.map(|value| approval_source_name(value.source)),
			decided_by: decision.and_then(|value| value.decided_by.clone()),
			reason: decision.and_then(|value| value.reason.clone()),
			audited: decision.is_some_and(|value| value.audited),
		})
	}
}

fn approval_source_name(source: ApprovalSource) -> Str {
	match source {
		ApprovalSource::User => sf!("user"),
		ApprovalSource::External => sf!("external"),
		ApprovalSource::Forwarded => sf!("forwarded"),
		ApprovalSource::Config => sf!("config"),
		ApprovalSource::Extension => sf!("extension"),
		ApprovalSource::Timeout => sf!("timeout"),
		ApprovalSource::Unavailable => sf!("unavailable"),
	}
}

fn approval_source_from_name(source: &str) -> Option<ApprovalSource> {
	Some(match source {
		"user" => ApprovalSource::User,
		"external" => ApprovalSource::External,
		"forwarded" => ApprovalSource::Forwarded,
		"config" => ApprovalSource::Config,
		"extension" => ApprovalSource::Extension,
		"timeout" => ApprovalSource::Timeout,
		"unavailable" => ApprovalSource::Unavailable,
		_ => return None,
	})
}

/// In-memory index reconstructed from `ApprovalTicketFiled` and
/// `ApprovalDecided` journal entries.
pub struct ApprovalBook {
	next_id:       AtomicU64,
	tickets:       Mutex<BTreeMap<Str, ApprovalTicket>>,
	by_invocation: Mutex<BTreeMap<Str, Str>>,
}
/// Invocation-owned guard that withdraws an unanswered ticket on drop.
pub struct ApprovalGuard<'a> {
	book:      &'a ApprovalBook,
	ticket_id: Str,
}

impl Drop for ApprovalGuard<'_> {
	fn drop(&mut self) {
		let _ = self.book.withdraw(self.ticket_id.as_str());
	}
}

impl ApprovalBook {
	/// Creates an empty Core ticket index.
	#[must_use]
	pub fn new() -> Self {
		Self {
			next_id:       AtomicU64::new(1),
			tickets:       Mutex::new(BTreeMap::new()),
			by_invocation: Mutex::new(BTreeMap::new()),
		}
	}

	/// Files or merges requirements into the one ticket for an invocation.
	pub fn file(
		&self,
		invocation_id: Option<Str>,
		reasons: Vec<ApprovalSpec>,
		created_at_ms: u64,
	) -> ApprovalTicket {
		if let Some(invocation_id) = &invocation_id
			&& let Some(ticket_id) = self.by_invocation.lock().get(invocation_id).cloned()
		{
			let mut tickets = self.tickets.lock();
			let ticket = tickets
				.get_mut(&ticket_id)
				.expect("invocation ticket index stays coherent");
			if ticket.state == TicketState::Pending {
				ticket.reasons.extend(reasons);
			}
			return ticket.clone();
		}
		let ticket_id = sf!("approval-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
		let ticket = ApprovalTicket {
			ticket_id: ticket_id.clone(),
			invocation_id: invocation_id.clone(),
			reasons,
			state: TicketState::Pending,
			decision: None,
			created_at_ms,
		};
		if let Some(invocation_id) = invocation_id {
			self
				.by_invocation
				.lock()
				.insert(invocation_id, ticket_id.clone());
		}
		self.tickets.lock().insert(ticket_id, ticket.clone());
		ticket
	}

	/// Applies an idempotent answer. The first answer wins permanently.
	pub fn decide(&self, ticket_id: &str, decision: ApprovalDecision) -> Option<ApprovalTicket> {
		let mut tickets = self.tickets.lock();
		let ticket = tickets.get_mut(ticket_id)?;
		if ticket.state == TicketState::Pending {
			ticket.state = TicketState::Decided;
			ticket.decision = Some(decision);
		}
		Some(ticket.clone())
	}

	/// Marks an unanswered ticket withdrawn when its invocation guard drops.
	pub fn withdraw(&self, ticket_id: &str) -> Option<ApprovalTicket> {
		let mut tickets = self.tickets.lock();
		let ticket = tickets.get_mut(ticket_id)?;
		if ticket.state == TicketState::Pending {
			ticket.state = TicketState::Withdrawn;
		}
		Some(ticket.clone())
	}

	/// Returns pending tickets in filing order.
	#[must_use]
	pub fn pending(&self) -> Vec<ApprovalTicket> {
		self
			.tickets
			.lock()
			.values()
			.filter(|ticket| ticket.state == TicketState::Pending)
			.cloned()
			.collect()
	}

	/// Restores a filed ticket from its typed durable record during session
	/// replay.
	pub fn restore_filed(&self, filed: omp_storage::transcript::ApprovalTicketFiled) {
		let ticket = ApprovalTicket {
			ticket_id:     filed.ticket_id.clone(),
			invocation_id: filed.invocation_id.clone(),
			reasons:       filed
				.reasons
				.into_iter()
				.map(|reason| ApprovalSpec {
					title:         reason.title,
					body:          reason.body,
					subject:       reason.subject,
					kind:          reason.kind,
					scopes:        reason.scopes,
					default:       reason.default,
					route:         reason.route,
					approver:      reason.approver,
					timeout_ms:    reason.timeout_ms,
					unreachable:   reason.unreachable,
					require_human: reason.require_human,
					pattern:       reason.pattern,
					evidence:      reason.evidence,
				})
				.collect(),
			state:         TicketState::Pending,
			decision:      None,
			created_at_ms: filed.created_at_ms,
		};
		if let Some(invocation_id) = ticket.invocation_id.clone() {
			self
				.by_invocation
				.lock()
				.insert(invocation_id, ticket.ticket_id.clone());
		}
		if let Some(sequence) = ticket
			.ticket_id
			.as_str()
			.strip_prefix("approval-")
			.and_then(|value| value.parse::<u64>().ok())
		{
			self
				.next_id
				.fetch_max(sequence.saturating_add(1), Ordering::Relaxed);
		}
		self.tickets.lock().insert(ticket.ticket_id.clone(), ticket);
	}

	/// Restores a terminal ticket decision or withdrawal during session replay.
	pub fn restore_decision(&self, decided: omp_storage::transcript::ApprovalDecided) {
		let mut tickets = self.tickets.lock();
		let Some(ticket) = tickets.get_mut(decided.ticket_id.as_str()) else {
			return;
		};
		if decided.state.as_str() == "withdrawn" {
			ticket.state = TicketState::Withdrawn;
			return;
		}
		let Some((approved, scope, source)) = decided
			.approved
			.zip(decided.scope)
			.zip(decided.source)
			.and_then(|((approved, scope), source)| {
				approval_source_from_name(source.as_str()).map(|source| (approved, scope, source))
			})
		else {
			return;
		};
		ticket.state = TicketState::Decided;
		ticket.decision = Some(ApprovalDecision {
			approved,
			scope,
			source,
			decided_by: decided.decided_by,
			reason: decided.reason,
			audited: decided.audited,
		});
	}

	/// Returns a guard which withdraws this ticket unless it is decided first.
	#[must_use]
	pub fn guard(&self, ticket_id: &str) -> Option<ApprovalGuard<'_>> {
		self
			.tickets
			.lock()
			.contains_key(ticket_id)
			.then(|| ApprovalGuard { book: self, ticket_id: Str::new(ticket_id) })
	}
}

impl Default for ApprovalBook {
	fn default() -> Self {
		Self::new()
	}
}

#[cfg(test)]
mod tests {
	use omp_core::Str;

	use super::{ApprovalBook, ApprovalDecision, ApprovalSource, ApprovalSpec, TicketState};
	fn spec() -> ApprovalSpec {
		ApprovalSpec {
			title:         sf!("Run"),
			body:          sf!("run"),
			subject:       sf!("cmd"),
			kind:          sf!("exec"),
			scopes:        vec![sf!("once")],
			default:       None,
			route:         sf!("local"),
			approver:      None,
			timeout_ms:    1,
			unreachable:   sf!("fail_closed"),
			require_human: false,
			pattern:       None,
			evidence:      Vec::new(),
		}
	}
	#[test]
	fn tickets_merge_answer_idempotently_and_withdraw() {
		let book = ApprovalBook::new();
		let ticket = book.file(Some(sf!("i")), vec![spec()], 1);
		assert_eq!(book.file(Some(sf!("i")), vec![spec()], 2).reasons.len(), 2);
		let decision = ApprovalDecision {
			approved:   true,
			scope:      sf!("once"),
			source:     ApprovalSource::User,
			decided_by: None,
			reason:     None,
			audited:    false,
		};
		assert_eq!(
			book
				.decide(ticket.ticket_id.as_str(), decision.clone())
				.unwrap()
				.decision,
			Some(decision)
		);
		assert_eq!(book.withdraw(ticket.ticket_id.as_str()).unwrap().state, TicketState::Decided);
		let withdrawn = book.file(Some(sf!("j")), vec![spec()], 3);
		assert_eq!(
			book.withdraw(withdrawn.ticket_id.as_str()).unwrap().state,
			TicketState::Withdrawn
		);
	}
	#[test]
	fn guard_withdraws_unanswered_ticket() {
		let book = ApprovalBook::new();
		let ticket = book.file(Some(sf!("guarded")), vec![spec()], 1);
		{
			let _guard = book.guard(ticket.ticket_id.as_str()).unwrap();
		}
		assert_eq!(book.pending(), Vec::new());
	}
}
