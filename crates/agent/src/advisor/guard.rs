//! Advisor emission deduplication and unsafe-turn quarantine.

use std::collections::{HashSet, VecDeque};

use omp_core::Str;

use super::AdviceSeverity;

/// Maximum accepted normalized notes retained by one advisor session.
pub const ADVICE_DEDUPE_LIMIT: usize = 4096;

/// Why an advisor emission was not admitted to the primary mailbox.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum AdvisorSuppression {
	/// The note contains no concrete guidance.
	ContentFree,
	/// This normalized note was accepted earlier in the session.
	Duplicate,
	/// One note was already accepted during this model update.
	UpdateLimit,
}

/// Durable quarantine classification for an unsafe advisor turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum AdvisorQuarantineReason {
	/// The model requested a non-bridge tool unavailable to this advisor.
	UnavailableTool,
	/// Generated output contains a destructive command directive.
	DestructiveDirective,
	/// Several independent output-only hazard classes matched.
	CompoundHazard,
	/// A new instruction override combined with a destructive command quoted
	/// from input.
	OverrideWithQuotedDestruction,
}

/// Accepted advisor note after guard normalization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuardedAdvice {
	/// Original trimmed note retained for delivery.
	pub note:       Str,
	/// Requested severity.
	pub severity:   AdviceSeverity,
	/// Stable dedupe key.
	pub normalized: Str,
}

/// Per-advisor guard state. Reset it whenever the advisor context is re-primed.
#[derive(Debug, Default)]
pub struct AdvisorEmissionGuard {
	accepted:        HashSet<Str>,
	accepted_order:  VecDeque<Str>,
	accepted_update: bool,
	quarantines:     u8,
}

impl AdvisorEmissionGuard {
	/// Opens a new model update and resets only the per-update gate.
	pub fn begin_update(&mut self) {
		self.accepted_update = false;
	}

	/// Admits at most one concrete, never-before-accepted note per update.
	pub fn admit(
		&mut self,
		note: &str,
		severity: AdviceSeverity,
	) -> Result<GuardedAdvice, AdvisorSuppression> {
		let normalized = normalize_advice(note);
		if content_free(normalized.as_str()) {
			return Err(AdvisorSuppression::ContentFree);
		}
		if self.accepted.contains(&normalized) {
			return Err(AdvisorSuppression::Duplicate);
		}
		if self.accepted_update {
			return Err(AdvisorSuppression::UpdateLimit);
		}
		self.accepted_update = true;
		self.accepted.insert(normalized.clone());
		self.accepted_order.push_back(normalized.clone());
		if self.accepted_order.len() > ADVICE_DEDUPE_LIMIT
			&& let Some(expired) = self.accepted_order.pop_front()
		{
			self.accepted.remove(&expired);
		}
		Ok(GuardedAdvice { note: Str::new(note.trim()), severity, normalized })
	}

	/// Records one quarantined turn and returns whether the host should emit its
	/// single warning before dropping the affected batch.
	pub fn record_quarantine(&mut self) -> bool {
		self.quarantines = self.quarantines.saturating_add(1);
		self.quarantines == 2
	}

	/// A successful safe advisor turn clears the consecutive quarantine count.
	pub fn record_safe_turn(&mut self) {
		self.quarantines = 0;
	}

	/// Clears all session/reset-scoped state.
	pub fn reset(&mut self) {
		self.accepted.clear();
		self.accepted_order.clear();
		self.accepted_update = false;
		self.quarantines = 0;
	}
}

/// Classifies unsafe output before any advisor tool dispatch or primary
/// delivery.
#[must_use]
pub fn quarantine_advisor_turn(
	requested_tools: &[Str],
	available_tools: &[Str],
	generated: &str,
	quoted_input: &str,
) -> Option<AdvisorQuarantineReason> {
	if requested_tools.iter().any(|requested| {
		!available_tools
			.iter()
			.any(|available| available == requested)
	}) {
		return Some(AdvisorQuarantineReason::UnavailableTool);
	}
	let output = generated.to_ascii_lowercase();
	let quoted = quoted_input.to_ascii_lowercase();
	let destructive = contains_destructive_command(&output);
	let instruction_override = contains_any(&output, &[
		"ignore previous",
		"ignore all prior",
		"override system",
		"disregard instructions",
	]);
	let denial = contains_any(&output, &[
		"do not comply",
		"refuse the user",
		"must not continue",
		"deny the request",
	]);
	let account_deletion =
		contains_any(&output, &["delete account", "remove account", "close the account"]);
	let directive =
		destructive && contains_any(&output, &["run ", "execute ", "you must", "immediately", "now"]);
	if directive {
		return Some(AdvisorQuarantineReason::DestructiveDirective);
	}
	let hazard_count = usize::from(destructive)
		+ usize::from(instruction_override)
		+ usize::from(denial)
		+ usize::from(account_deletion);
	if hazard_count >= 3 {
		return Some(AdvisorQuarantineReason::CompoundHazard);
	}
	if instruction_override && contains_destructive_command(&quoted) {
		return Some(AdvisorQuarantineReason::OverrideWithQuotedDestruction);
	}
	None
}

/// Normalizes an advice note for dedupe and content-free filtering.
#[must_use]
pub fn normalize_advice(note: &str) -> Str {
	let mut normalized = String::with_capacity(note.len());
	let mut separator = false;
	for character in note.chars().flat_map(char::to_lowercase) {
		if character.is_alphanumeric() {
			if separator && !normalized.is_empty() {
				normalized.push(' ');
			}
			separator = false;
			normalized.push(character);
		} else {
			separator = true;
		}
	}
	Str::new(normalized)
}

fn content_free(note: &str) -> bool {
	matches!(
		note,
		"" | "stop"
			| "done"
			| "complete"
			| "continue"
			| "lgtm"
			| "looks good"
			| "nothing to add"
			| "no issue"
			| "no issues"
			| "no issue continue"
	)
}

fn contains_destructive_command(text: &str) -> bool {
	contains_any(text, &[
		"rm -rf",
		"git reset --hard",
		"git clean -f",
		"drop database",
		"truncate table",
		"delete from",
		"format disk",
		"mkfs.",
	])
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
	needles.iter().any(|needle| text.contains(needle))
}
#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn one_concrete_note_per_update_and_session_dedupe() {
		let mut guard = AdvisorEmissionGuard::default();
		guard.begin_update();
		assert!(
			guard
				.admit("Missing a rollback path.", AdviceSeverity::Concern)
				.is_ok()
		);
		assert_eq!(
			guard.admit("A second issue.", AdviceSeverity::Blocker),
			Err(AdvisorSuppression::UpdateLimit)
		);
		guard.begin_update();
		assert_eq!(
			guard.admit("*missing a rollback path*", AdviceSeverity::Blocker),
			Err(AdvisorSuppression::Duplicate)
		);
	}

	#[test]
	fn unsafe_turn_is_quarantined_before_delivery() {
		assert_eq!(
			quarantine_advisor_turn(&[], &[], "You must run rm -rf / now", ""),
			Some(AdvisorQuarantineReason::DestructiveDirective)
		);
	}
}
