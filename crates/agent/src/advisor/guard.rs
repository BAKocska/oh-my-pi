//! Advisor emission deduplication and unsafe-turn quarantine.

use std::{
	collections::{HashSet, VecDeque},
	sync::LazyLock,
};

use omp_core::Str;
use regex::RegexSet;

use super::AdviceSeverity;

/// Maximum accepted normalized notes retained by one advisor session.
pub const ADVICE_DEDUPE_LIMIT: usize = 4096;
/// Default consecutive identical-call threshold for an advisor update.
pub const ADVISOR_TOOL_LOOP_THRESHOLD: u32 = 3;
static HAZARD_PATTERNS: LazyLock<RegexSet> = LazyLock::new(|| {
	RegexSet::new([
		r"(?i)\buser\b.{0,80}\b(?:deleted|erased)\b.{0,80}\baccount\b",
		r"(?i)\bignore\s+(?:all\s+)?(?:prior|previous|earlier)\s+(?:user\s+)?instructions\b",
		r"(?i)\brm\s+(?:(?:-[a-z]*r[a-z]*f[a-z]*|-[a-z]*f[a-z]*r[a-z]*)\s+|(?:-[a-z]*r[a-z]*\s+)(?:-[a-z]+\s+)*-[a-z]*f[a-z]*\s+|(?:-[a-z]*f[a-z]*\s+)(?:-[a-z]+\s+)*-[a-z]*r[a-z]*\s+)",
		r"(?i)\bdeny\s+(?:this|it|the\s+request)\s+if\s+(?:asked|questioned)\b",
	])
	.expect("static advisor hazard expressions are valid")
});
const ACCOUNT_DELETION: usize = 0;
const INSTRUCTION_OVERRIDE: usize = 1;
const DESTRUCTIVE_SHELL: usize = 2;
const DENIAL_INSTRUCTION: usize = 3;
/// Escalation selected after observing one advisor tool-call turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdvisorToolLoopAction {
	/// The update has not reached its repeated-call threshold.
	Continue,
	/// Inject one corrective user-role message, then re-arm repetition
	/// detection.
	Redirect {
		/// Number of consecutive equivalent calls that tripped the threshold.
		count:  u32,
		/// Stable digest of the repeated tool name and arguments.
		digest: Str,
	},
	/// Cleanly terminate an update that ignored its first corrective.
	Abort {
		/// Number of post-corrective equivalent calls that tripped the threshold.
		count:  u32,
		/// Stable digest of the repeated tool name and arguments.
		digest: Str,
	},
}

/// Per-update ladder bounding repeated identical calls in an advisor's private
/// loop.
#[derive(Debug)]
pub struct AdvisorToolLoopGuard {
	threshold:       u32,
	digest:          Option<Str>,
	repeats:         u32,
	redirect_issued: bool,
}

impl AdvisorToolLoopGuard {
	/// Creates a guard, clamping a zero threshold to one completed call.
	pub fn new(threshold: u32) -> Self {
		Self {
			threshold:       threshold.max(1),
			digest:          None,
			repeats:         0,
			redirect_issued: false,
		}
	}

	/// Starts a new externally submitted advisor update.
	pub fn begin_update(&mut self) {
		self.reset();
	}

	/// Clears detector and escalation state after an advisor context
	/// replacement.
	pub fn reset(&mut self) {
		self.digest = None;
		self.repeats = 0;
		self.redirect_issued = false;
	}

	/// Records one completed tool-call shape and selects the next ladder action.
	///
	/// `digest` must cover the tool name and structured arguments. `None` marks
	/// a non-tool turn and breaks a consecutive sequence. The first threshold
	/// returns [`AdvisorToolLoopAction::Redirect`] and re-arms the counter; the
	/// second threshold in the same update returns
	/// [`AdvisorToolLoopAction::Abort`].
	pub fn observe(&mut self, digest: Option<Str>) -> AdvisorToolLoopAction {
		let Some(digest) = digest else {
			self.digest = None;
			self.repeats = 0;
			return AdvisorToolLoopAction::Continue;
		};
		self.repeats = if self.digest.as_ref() == Some(&digest) {
			self.repeats.saturating_add(1)
		} else {
			1
		};
		self.digest = Some(digest.clone());
		if self.repeats < self.threshold {
			return AdvisorToolLoopAction::Continue;
		}
		let count = self.repeats;
		self.digest = None;
		self.repeats = 0;
		if self.redirect_issued {
			self.redirect_issued = false;
			AdvisorToolLoopAction::Abort { count, digest }
		} else {
			self.redirect_issued = true;
			AdvisorToolLoopAction::Redirect { count, digest }
		}
	}
}

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
pub fn quarantine_advisor_turn(
	requested_tools: &[Str],
	available_tools: &[Str],
	generated: &str,
	quoted_input: &str,
) -> Option<AdvisorQuarantineReason> {
	if requested_tools.iter().any(|requested| {
		requested.as_str() != "advise"
			&& !available_tools
				.iter()
				.any(|available| available == requested)
	}) {
		return Some(AdvisorQuarantineReason::UnavailableTool);
	}

	let generated_matches = HAZARD_PATTERNS.matches(generated);
	let source_matches = HAZARD_PATTERNS.matches(quoted_input);
	let output_only = |label| generated_matches.matched(label) && !source_matches.matched(label);

	if output_only(DESTRUCTIVE_SHELL) {
		return Some(AdvisorQuarantineReason::DestructiveDirective);
	}
	let mut output_only_count = 0;
	for label in [ACCOUNT_DELETION, INSTRUCTION_OVERRIDE, DESTRUCTIVE_SHELL, DENIAL_INSTRUCTION] {
		output_only_count += usize::from(output_only(label));
	}
	if output_only_count >= 3 {
		return Some(AdvisorQuarantineReason::CompoundHazard);
	}
	if output_only(INSTRUCTION_OVERRIDE)
		&& generated_matches.matched(DESTRUCTIVE_SHELL)
		&& source_matches.matched(DESTRUCTIVE_SHELL)
	{
		return Some(AdvisorQuarantineReason::OverrideWithQuotedDestruction);
	}
	None
}

/// Normalizes an advice note for dedupe and content-free filtering.
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
			| "stop here"
			| "stop now"
			| "halt"
			| "abort"
			| "done"
			| "task done"
			| "task complete"
			| "complete"
			| "finished"
			| "ok" | "okay"
			| "ok done"
			| "continue"
			| "lgtm"
			| "looks good"
			| "all good"
			| "agent is on track"
			| "agent on track"
			| "on track"
			| "carry on"
			| "nothing to add"
			| "nothing to flag"
			| "nothing to report"
			| "no issue"
			| "no issues"
			| "no issue continue"
			| "no concern"
			| "no concerns"
			| "no notes"
			| "no further input"
			| "no further input needed"
			| "no further input required"
			| "no further watcher input"
			| "no further watcher input needed"
			| "no further advice"
			| "no further advice needed"
	)
}

#[cfg(test)]
mod tests {
	use super::*;
	fn observe_repeats(
		guard: &mut AdvisorToolLoopGuard,
		digest: &'static str,
		count: u32,
	) -> AdvisorToolLoopAction {
		let mut action = AdvisorToolLoopAction::Continue;
		for _ in 0..count {
			action = guard.observe(Some(Str::new_static(digest)));
		}
		action
	}

	#[test]
	fn advisor_tool_loop_redirects_then_aborts() {
		let mut guard = AdvisorToolLoopGuard::new(3);
		assert_eq!(
			observe_repeats(&mut guard, "read:missing.rs", 3),
			AdvisorToolLoopAction::Redirect { count: 3, digest: Str::new_static("read:missing.rs") }
		);
		assert_eq!(observe_repeats(&mut guard, "read:missing.rs", 3), AdvisorToolLoopAction::Abort {
			count:  3,
			digest: Str::new_static("read:missing.rs"),
		});
	}

	#[test]
	fn advisor_tool_loop_resets_at_update_and_context_boundaries() {
		let mut guard = AdvisorToolLoopGuard::new(3);
		assert_eq!(
			observe_repeats(&mut guard, "read:missing.rs", 2),
			AdvisorToolLoopAction::Continue
		);
		guard.begin_update();
		assert_eq!(
			observe_repeats(&mut guard, "read:missing.rs", 2),
			AdvisorToolLoopAction::Continue
		);
		guard.reset();
		assert_eq!(
			observe_repeats(&mut guard, "read:missing.rs", 2),
			AdvisorToolLoopAction::Continue
		);
		assert_eq!(guard.observe(Some(Str::new_static("bash:pwd"))), AdvisorToolLoopAction::Continue);
		assert_eq!(
			observe_repeats(&mut guard, "read:missing.rs", 2),
			AdvisorToolLoopAction::Continue
		);
	}

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
