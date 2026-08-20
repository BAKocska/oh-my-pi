//! Context compaction tiers, usage accounting, and deterministic hook verdicts.

use bytes::{Bytes, BytesMut};
use omp_core::Str;
use omp_proto::{
	prost::Message as _,
	toolhost::v1::{CompactionRequest, CompactionVerdict as WireCompactionVerdict, HookEventId},
};
pub use omp_storage::transcript::SupersededCompaction;
use omp_storage::{
	blob::BlobRef,
	transcript::{Kind, ModelId, ModelRef, ProviderId, capsule::checkpoint_reusable},
};
use smallvec::SmallVec;

use crate::{
	hooks::{DomainReturn, GateError, HookEvent, HookGate, HookPatch, SourceRef},
	journal::Compact,
};

/// Fraction of an auto-compaction trigger below which the ladder is re-armed.
///
/// The band is agent-owned: extensions observe the triggering usage, never the
/// suppression state, so they cannot create a compact-on-every-turn loop.
pub const COMPACTION_RECOVERY_BAND: f64 = 0.8;

/// Ordered context rescue rungs.
///
/// The ladder always attempts `PRUNE` through `HANDOFF` in this order. The
/// snapcompact image-folding slot is intentionally reserved but has no v1 rung.
#[derive(
	Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, strum::Display, strum::EnumString,
)]
#[strum(serialize_all = "SCREAMING_SNAKE_CASE")]
pub enum CompactionTier {
	/// Remove already-useless or superseded tool results without loss.
	Prune,
	/// Remove blob-backed historical content while retaining artifact
	/// references.
	DropMedia,
	/// Replace oversized historical tool results with bounded views.
	Elide,
	/// Summarize a prefix through a local model.
	Local,
	/// Request provider-native context management with replay checkpointing.
	Remote,
	/// End this session and transfer a summary and artifacts to a child.
	Handoff,
}

impl CompactionTier {
	/// The implemented rescue ladder in execution order.
	pub const ALL: [Self; 6] =
		[Self::Prune, Self::DropMedia, Self::Elide, Self::Local, Self::Remote, Self::Handoff];

	/// Returns whether this rung preserves all non-targeted projection items.
	#[must_use]
	pub const fn is_lossless(self) -> bool {
		matches!(self, Self::Prune | Self::DropMedia)
	}
}

/// Reserved textual name for the out-of-scope snapcompact tier.
pub const SNAPCOMPACT_RESERVED_TIER: &str = "SNAPCOMPACT";

/// The current context budget used to decide whether compaction is necessary.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ContextUsage {
	/// All input tokens currently occupying the provider context.
	pub total_tokens:          u64,
	/// Provider-advertised context window.
	pub context_window:        u64,
	/// Tokens reserved for the next completion.
	pub reserve_tokens:        u64,
	/// Context available to the prompt after the reserve.
	pub usable_tokens:         u64,
	/// Prompt-head tokens, accounted independently from message tokens.
	pub prompt_head_tokens:    u64,
	/// Device-catalog tokens included in the prompt head.
	pub device_catalog_tokens: u64,
	/// Message-body token estimate or back-projected provider usage.
	pub message_tokens:        u64,
	/// Media-token estimate kept beside exact byte lengths.
	pub media_tokens:          u64,
	/// Durable compact/reset epoch.
	pub compaction_epoch:      u64,
	/// Configured auto-compaction trigger fraction.
	pub threshold_fraction:    f64,
	/// Whether a streaming turn makes the total an extrapolation.
	pub in_flight:             bool,
}

impl ContextUsage {
	/// Creates usage while deriving the usable window from its reserve.
	#[must_use]
	pub fn new(
		total_tokens: u64,
		context_window: u64,
		reserve_tokens: u64,
		threshold_fraction: f64,
	) -> Self {
		Self {
			total_tokens,
			context_window,
			reserve_tokens,
			usable_tokens: context_window.saturating_sub(reserve_tokens),
			prompt_head_tokens: 0,
			device_catalog_tokens: 0,
			message_tokens: 0,
			media_tokens: 0,
			compaction_epoch: 0,
			threshold_fraction,
			in_flight: false,
		}
	}

	/// Returns occupancy of the usable context window.
	#[must_use]
	pub fn fraction(self) -> f64 {
		if self.usable_tokens == 0 {
			return f64::INFINITY;
		}
		self.total_tokens as f64 / self.usable_tokens as f64
	}

	/// Returns the target token count at the configured trigger threshold.
	#[must_use]
	pub fn target_tokens(self) -> u64 {
		(self.usable_tokens as f64 * self.threshold_fraction).floor() as u64
	}

	/// Returns whether occupancy reaches the configured auto-compaction trigger.
	#[must_use]
	pub fn over_threshold(self) -> bool {
		self.fraction() >= self.threshold_fraction
	}
}

/// Agent-owned state that prevents auto-compaction loops near the threshold.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompactionHysteresis {
	armed: bool,
}

impl Default for CompactionHysteresis {
	fn default() -> Self {
		Self { armed: true }
	}
}

impl CompactionHysteresis {
	/// Evaluates auto-compaction and re-arms only below the recovery band edge.
	pub fn evaluate(&mut self, usage: ContextUsage) -> bool {
		if !self.armed {
			if usage.fraction() <= usage.threshold_fraction * COMPACTION_RECOVERY_BAND {
				self.armed = true;
			}
			return false;
		}
		if usage.over_threshold() {
			self.armed = false;
			return true;
		}
		false
	}

	/// Returns whether the next threshold crossing can trigger compaction.
	#[must_use]
	pub const fn armed(self) -> bool {
		self.armed
	}
}

/// Per-item accounting kept beside the exact stored byte length.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ItemUsage {
	/// Exact serialized content length in bytes.
	pub byte_len:        u64,
	/// Provider usage back-projected onto this item.
	pub provider_tokens: u64,
}

/// Back-projects reported provider usage across items proportional to byte
/// size.
///
/// The sum of returned item token counts is exactly `total_tokens`; exact bytes
/// remain untouched for reporting and later tokenizer replacement.
pub fn back_project_provider_usage(total_tokens: u64, items: &mut [ItemUsage]) {
	let total_bytes: u128 = items.iter().map(|item| u128::from(item.byte_len)).sum();
	if items.is_empty() {
		return;
	}
	if total_bytes == 0 {
		let len = u64::try_from(items.len()).expect("slice length fits in u64");
		let each = total_tokens / len;
		let mut remainder = total_tokens % len;
		for item in items {
			item.provider_tokens = each + u64::from(remainder > 0);
			remainder = remainder.saturating_sub(1);
		}
		return;
	}
	let mut assigned = 0_u64;
	for item in items.iter_mut() {
		item.provider_tokens = u64::try_from(
			u128::from(total_tokens).saturating_mul(u128::from(item.byte_len)) / total_bytes,
		)
		.expect("token share fits in u64");
		assigned = assigned.saturating_add(item.provider_tokens);
	}
	let mut remainder = total_tokens.saturating_sub(assigned);
	for item in items {
		if remainder == 0 {
			break;
		}
		item.provider_tokens = item.provider_tokens.saturating_add(1);
		remainder -= 1;
	}
}

/// One body-free projected item considered by lossless compaction planning.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProjectionItem {
	/// Physical event index used by durable amendments.
	pub event:       u64,
	/// Whether the item is a tool result already marked useless.
	pub useless:     bool,
	/// Whether a later result superseded this item.
	pub superseded:  bool,
	/// Number of blob-backed parts in the item.
	pub media_parts: u32,
	/// Token and exact-byte accounting for this item.
	pub usage:       ItemUsage,
}

/// Pure lossless targets selected from one canonical projection.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LosslessPlan {
	/// Tool-result events removed by the `PRUNE` rung.
	pub prune:      Vec<u64>,
	/// Historical events whose blob-backed parts are eligible for `DROP_MEDIA`.
	pub drop_media: Vec<u64>,
}

/// Plans lossless `PRUNE` and `DROP_MEDIA` work without mutating the
/// projection.
#[must_use]
pub fn plan_lossless(items: &[ProjectionItem]) -> LosslessPlan {
	let mut plan = LosslessPlan::default();
	for item in items {
		if item.useless || item.superseded {
			plan.prune.push(item.event);
		}
		if item.media_parts != 0 {
			plan.drop_media.push(item.event);
		}
	}
	plan
}

/// Why a compaction request entered the ladder.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display, strum::EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum CompactionReason {
	/// Context occupancy crossed the configured automatic threshold.
	Threshold,
	/// The session was compacted while idle.
	Idle,
	/// A person explicitly requested compaction.
	Manual,
	/// A streaming turn requires prompt-space recovery.
	MidTurn,
	/// An extension initiated the request.
	Extension,
	/// A provider rejected a request for context length.
	Rescue,
}

/// The domain-return payload dispatched once before each ladder rung.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactionEvent {
	/// Stable correlation identifier for this ladder attempt.
	pub preparation_id:       Str,
	/// Rung about to run.
	pub tier:                 CompactionTier,
	/// Why the ladder was entered.
	pub reason:               CompactionReason,
	/// Durable epoch before compaction.
	pub epoch:                u64,
	/// Current total token count.
	pub tokens_before:        u64,
	/// Target token count for this rung.
	pub target_tokens:        u64,
	/// Suggested first retained item id.
	pub suggested_first_kept: Str,
	/// Wire body-free refs selected for summarization.
	pub to_summarize:         Vec<omp_proto::toolhost::v1::MessageRef>,
	/// Wire body-free refs retained verbatim.
	pub to_retain:            Vec<omp_proto::toolhost::v1::MessageRef>,
	/// Whether the suggested cut divides a turn.
	pub split_turn:           bool,
	/// Text of the preceding durable compact summary.
	pub previous_summary:     Option<Str>,
	/// Opaque extension preserve payload from the preceding compaction.
	pub previous_preserve:    Option<bytes::Bytes>,
	/// User-supplied focus text.
	pub custom_instructions:  Option<Str>,
	/// Remaining hook deadline in milliseconds on the frozen context wire.
	pub deadline_ms:          u64,
}

/// Skip one compaction tier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancelCompaction {
	/// Durable and displayable reason for the skip.
	pub reason:             Str,
	/// Number of subsequent turns for which the entire ladder is suppressed.
	pub suppress_for_turns: u64,
}

/// A textual summary supplied by an extension instead of a built-in summarizer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CustomSummary {
	/// Durable `Kind::Compact` payload; its summary is always textual.
	pub compact:  Compact,
	/// Extension-private JSON stored alongside the compaction record.
	pub details:  Option<bytes::Bytes>,
	/// Opaque state returned to the next compaction attempt.
	pub preserve: Option<bytes::Bytes>,
}

/// Adjustments to the built-in behavior of one compaction rung.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DelegateCompaction {
	/// Additional instructions appended to a summarization prompt.
	pub extra_instructions: Str,
	/// Stable item identifiers whose content should survive a summary.
	pub focus_ids:          SmallVec<Str, 2>,
	/// Optional model role override.
	pub role:               Option<Str>,
	/// Optional verbatim recent-history allowance.
	pub keep_recent_tokens: Option<u64>,
}

/// One non-empty domain verdict returned by a compaction handler.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompactionVerdict {
	/// Skip the current rung.
	Cancel(CancelCompaction),
	/// Use an extension-supplied durable textual summary.
	Custom(CustomSummary),
	/// Run the built-in rung with additional direction.
	Delegate(DelegateCompaction),
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WireVerdictDetails {
	Cancel {
		suppress_for_turns: u64,
	},
	Custom {
		short:         Option<Str>,
		tokens_before: u64,
		warning:       Option<Str>,
		details:       Option<Bytes>,
		preserve:      Option<Bytes>,
	},
	Delegate {
		extra_instructions: Str,
		focus_ids:          SmallVec<Str, 2>,
		role:               Option<Str>,
		keep_recent_tokens: Option<u64>,
	},
}

impl HookEvent for CompactionEvent {
	type Return = Option<CompactionVerdict>;

	const ID: HookEventId = HookEventId::HookEventCompaction;
	const REV: u32 = 1;

	fn encode_into(&self, out: &mut BytesMut) {
		let event = CompactionRequest {
			preparation_id:         self.preparation_id.as_str().to_owned(),
			tier:                   self.tier.to_string(),
			reason:                 self.reason.to_string(),
			epoch:                  self.epoch,
			tokens_before:          self.tokens_before,
			target_tokens:          self.target_tokens,
			suggested_first_kept:   self.suggested_first_kept.as_str().to_owned(),
			to_summarize:           self.to_summarize.clone(),
			to_retain:              self.to_retain.clone(),
			split_turn:             self.split_turn,
			previous_summary:       self.previous_summary.as_ref().map(ToString::to_string),
			previous_preserve_json: self.previous_preserve.clone(),
			custom_instructions:    self.custom_instructions.as_ref().map(ToString::to_string),
			deadline_ms:            self.deadline_ms,
			props:                  None,
		};
		event
			.encode(out)
			.expect("bytes buffer cannot fail protobuf encoding");
	}

	fn apply(&mut self, _: &HookPatch) -> Result<(), GateError> {
		Ok(())
	}
}
/// Dispatches the domain-return `compaction` hook for one ladder rung.
///
/// Hook failures and malformed replies resolve to
/// [`CompactionResolution::Default`], so the caller runs that rung's built-in
/// behavior rather than leaving the session over budget. A rescue-time
/// `HANDOFF` cancellation is refused because the ladder has no remaining rung
/// that can make the next provider request fit.
pub async fn dispatch_tier(gate: &HookGate, event: &CompactionEvent) -> CompactionResolution {
	let mut outcome = gate.gate_domain(event).await;
	let resolution = if outcome.contributions.is_empty() {
		resolve_one(outcome.winner)
	} else {
		resolve_verdicts(&mut outcome.contributions)
	};
	if matches!(resolution, CompactionResolution::Cancel(_))
		&& event.tier == CompactionTier::Handoff
		&& event.reason == CompactionReason::Rescue
	{
		CompactionResolution::Default
	} else {
		resolution
	}
}

impl DomainReturn for Option<CompactionVerdict> {
	fn decode_domain(bytes: &[u8]) -> Option<Self> {
		let wire = WireCompactionVerdict::decode(bytes).ok()?;
		let details = wire
			.details_json
			.as_deref()
			.map(serde_json::from_slice::<WireVerdictDetails>)
			.transpose()
			.ok()?;
		match (wire.kind.as_str(), details) {
			("cancel", Some(WireVerdictDetails::Cancel { suppress_for_turns })) => {
				Some(Some(CompactionVerdict::Cancel(CancelCompaction {
					reason: Str::from(wire.reason?),
					suppress_for_turns,
				})))
			},
			(
				"custom_summary",
				Some(WireVerdictDetails::Custom { short, tokens_before, warning, details, preserve }),
			) => Some(Some(CompactionVerdict::Custom(CustomSummary {
				compact: Compact {
					summary: Str::from(wire.summary?),
					short,
					first_kept: wire.first_kept_id?.parse().ok()?,
					tokens_before,
					warning,
					superseded: Vec::new(),
				},
				details,
				preserve,
			}))),
			(
				"delegate",
				Some(WireVerdictDetails::Delegate {
					extra_instructions,
					focus_ids,
					role,
					keep_recent_tokens,
				}),
			) => Some(Some(CompactionVerdict::Delegate(DelegateCompaction {
				extra_instructions,
				focus_ids,
				role,
				keep_recent_tokens,
			}))),
			("none", None) => Some(None),
			_ => None,
		}
	}

	fn fail_open() -> Self {
		None
	}

	fn merge_domain(self, next: Self) -> Self {
		match (self, next) {
			(_, Some(CompactionVerdict::Cancel(cancel))) => Some(CompactionVerdict::Cancel(cancel)),
			(Some(CompactionVerdict::Cancel(cancel)), _) => Some(CompactionVerdict::Cancel(cancel)),
			(Some(CompactionVerdict::Custom(summary)), _) => Some(CompactionVerdict::Custom(summary)),
			(None, next) => next,
			(current, None) => current,
			(
				Some(CompactionVerdict::Delegate(mut current)),
				Some(CompactionVerdict::Delegate(next)),
			) => {
				compose_delegate(&mut current, &next);
				Some(CompactionVerdict::Delegate(current))
			},
			(Some(CompactionVerdict::Delegate(_)), Some(CompactionVerdict::Custom(summary))) => {
				Some(CompactionVerdict::Custom(summary))
			},
		}
	}
}

/// Encodes a domain compaction verdict for a `HookGate::gate_domain` reply.
#[must_use]
pub fn encode_domain_verdict(verdict: Option<&CompactionVerdict>) -> Bytes {
	let wire = match verdict {
		None => WireCompactionVerdict { kind: "none".to_owned(), ..Default::default() },
		Some(CompactionVerdict::Cancel(cancel)) => WireCompactionVerdict {
			kind: "cancel".to_owned(),
			reason: Some(cancel.reason.as_str().to_owned()),
			details_json: Some(encode_details(&WireVerdictDetails::Cancel {
				suppress_for_turns: cancel.suppress_for_turns,
			})),
			..Default::default()
		},
		Some(CompactionVerdict::Custom(summary)) => WireCompactionVerdict {
			kind: "custom_summary".to_owned(),
			summary: Some(summary.compact.summary.as_str().to_owned()),
			first_kept_id: Some(summary.compact.first_kept.to_string()),
			details_json: Some(encode_details(&WireVerdictDetails::Custom {
				short:         summary.compact.short.clone(),
				tokens_before: summary.compact.tokens_before,
				warning:       summary.compact.warning.clone(),
				details:       summary.details.clone(),
				preserve:      summary.preserve.clone(),
			})),
			..Default::default()
		},
		Some(CompactionVerdict::Delegate(delegate)) => WireCompactionVerdict {
			kind: "delegate".to_owned(),
			details_json: Some(encode_details(&WireVerdictDetails::Delegate {
				extra_instructions: delegate.extra_instructions.clone(),
				focus_ids:          delegate.focus_ids.clone(),
				role:               delegate.role.clone(),
				keep_recent_tokens: delegate.keep_recent_tokens,
			})),
			..Default::default()
		},
	};
	let mut encoded = BytesMut::new();
	wire
		.encode(&mut encoded)
		.expect("bytes buffer cannot fail protobuf encoding");
	encoded.freeze()
}

fn encode_details(details: &WireVerdictDetails) -> Bytes {
	Bytes::from(serde_json::to_vec(details).expect("compaction verdict details are serializable"))
}

/// Deterministically composed result of one compaction hook dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompactionResolution {
	/// A handler cancelled this rung; later handlers are not consulted.
	Cancel(CancelCompaction),
	/// A custom textual summary won, with durable loser metadata.
	Custom {
		/// Winning extension summary.
		winner: CustomSummary,
		/// Ordered loser records to persist alongside `Kind::Compact`.
		losers: Vec<SupersededCompaction>,
	},
	/// Built-in behavior with all ordered delegate fields composed.
	Delegate(DelegateCompaction),
	/// No handler expressed an opinion.
	Default,
}

/// Composes built-in tier instructions in deterministic handler order.
fn compose_delegate(current: &mut DelegateCompaction, next: &DelegateCompaction) {
	if !next.extra_instructions.is_empty() {
		current.extra_instructions = if current.extra_instructions.is_empty() {
			next.extra_instructions.clone()
		} else {
			Str::from(format!(
				"{}\n{}",
				current.extra_instructions.as_str(),
				next.extra_instructions.as_str()
			))
		};
	}
	for id in &next.focus_ids {
		if !current.focus_ids.iter().any(|known| known == id) {
			current.focus_ids.push(id.clone());
		}
	}
	if current.role.is_none() {
		current.role.clone_from(&next.role);
	}
	if current.keep_recent_tokens.is_none() {
		current.keep_recent_tokens = next.keep_recent_tokens;
	}
}

impl CompactionResolution {
	/// Consumes a winning custom summary into the durable journal payload.
	///
	/// Only this path carries ordered superseded-summary metadata into
	/// `Journal::compact`; cancellation and delegated/default outcomes leave
	/// durable compaction to their respective built-in rungs.
	#[must_use]
	pub fn into_compact(self) -> Option<Compact> {
		let Self::Custom { mut winner, losers } = self else {
			return None;
		};
		winner.compact.superseded = losers;
		Some(winner.compact)
	}
}

/// Resolves one fail-open domain result without source attribution.
fn resolve_one(verdict: Option<CompactionVerdict>) -> CompactionResolution {
	match verdict {
		Some(CompactionVerdict::Cancel(cancel)) => CompactionResolution::Cancel(cancel),
		Some(CompactionVerdict::Custom(winner)) => {
			CompactionResolution::Custom { winner, losers: Vec::new() }
		},
		Some(CompactionVerdict::Delegate(delegate)) => CompactionResolution::Delegate(delegate),
		None => CompactionResolution::Default,
	}
}

/// Resolves handler verdicts in `(layer, publisher, extension_id)` order.
///
/// The first cancellation wins immediately. Otherwise the first custom summary
/// wins and later custom summaries become ordered metadata; delegate fields
/// compose only when no custom summary replaces the rung.
pub fn resolve_verdicts(
	verdicts: &mut [(SourceRef, Option<CompactionVerdict>)],
) -> CompactionResolution {
	verdicts.sort_by(|left, right| left.0.cmp(&right.0));
	let mut winner = None;
	let mut losers = Vec::new();
	let mut delegate = DelegateCompaction::default();
	for (source, returned) in verdicts {
		let Some(verdict) = returned.as_ref() else {
			continue;
		};
		match verdict {
			CompactionVerdict::Cancel(cancel) => return CompactionResolution::Cancel(cancel.clone()),
			CompactionVerdict::Custom(summary) => {
				if winner.is_none() {
					winner = Some(summary.clone());
				} else {
					losers.push(SupersededCompaction {
						extension_id: source.extension_id.clone(),
						reason:       Str::new_static("custom_summary_superseded"),
					});
				}
			},
			CompactionVerdict::Delegate(next) if winner.is_none() => {
				compose_delegate(&mut delegate, next);
			},
			CompactionVerdict::Delegate(_) => {},
		}
	}
	if let Some(winner) = winner {
		CompactionResolution::Custom { winner, losers }
	} else if delegate != DelegateCompaction::default() {
		CompactionResolution::Delegate(delegate)
	} else {
		CompactionResolution::Default
	}
}

/// Provider-native remote compaction checkpoint retained only for its origin.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteCheckpoint {
	/// Origin provider.
	pub provider: ProviderId,
	/// Origin model.
	pub model:    ModelId,
	/// Blob containing provider-native replay items.
	pub items:    BlobRef,
}

impl RemoteCheckpoint {
	/// Returns whether this opaque checkpoint is safe for the active model.
	#[must_use]
	pub fn reusable_for(&self, active: &ModelRef) -> bool {
		checkpoint_reusable(&self.provider, &self.model, active)
	}

	/// Converts this verified checkpoint into the existing durable transcript
	/// representation used by the `REMOTE` rung.
	#[must_use]
	pub fn into_event(self) -> Kind {
		Kind::NativeCheckpoint { provider: self.provider, model: self.model, items: self.items }
	}
}

#[cfg(test)]
mod tests {

	use omp_core::Str;

	use super::{
		COMPACTION_RECOVERY_BAND, Compact, CompactionHysteresis, CompactionVerdict, ContextUsage,
		CustomSummary, ItemUsage, ProjectionItem, back_project_provider_usage, plan_lossless,
		resolve_verdicts,
	};
	use crate::hooks::SourceRef;

	#[test]
	fn hysteresis_triggers_at_threshold_and_rearms_at_recovery_edge() {
		let mut hysteresis = CompactionHysteresis::default();
		let mut usage = ContextUsage::new(80, 100, 0, 0.8);
		assert!(hysteresis.evaluate(usage));
		assert!(!hysteresis.armed());
		assert!(!hysteresis.evaluate(usage));
		usage.total_tokens = (80.0 * COMPACTION_RECOVERY_BAND) as u64;
		assert!(!hysteresis.evaluate(usage));
		assert!(hysteresis.armed());
	}

	#[test]
	fn lossless_prune_leaves_non_targets_identical() {
		let retained = ProjectionItem {
			event:       1,
			useless:     false,
			superseded:  false,
			media_parts: 0,
			usage:       ItemUsage { byte_len: 10, provider_tokens: 2 },
		};
		let pruned = ProjectionItem { event: 2, useless: true, ..retained };
		let projection = [retained, pruned];
		let plan = plan_lossless(&projection);
		assert_eq!(plan.prune, vec![2]);
		assert_eq!(projection[0], retained);
	}

	#[test]
	fn provider_usage_back_projection_preserves_exact_total() {
		let mut usage = [ItemUsage { byte_len: 1, provider_tokens: 0 }, ItemUsage {
			byte_len:        3,
			provider_tokens: 0,
		}];
		back_project_provider_usage(7, &mut usage);
		assert_eq!(usage.iter().map(|item| item.provider_tokens).sum::<u64>(), 7);
	}

	#[test]
	fn custom_summary_winner_and_loser_metadata_follow_publisher_order() {
		let summary = |text: &'static str, first_kept| {
			CompactionVerdict::Custom(CustomSummary {
				compact:  Compact {
					summary: Str::new_static(text),
					short: None,
					first_kept,
					tokens_before: 100,
					warning: None,
					superseded: Vec::new(),
				},
				details:  None,
				preserve: None,
			})
		};
		let mut verdicts = [
			(
				SourceRef {
					layer:        1,
					publisher:    Str::new_static("z"),
					extension_id: Str::new_static("late"),
				},
				Some(summary("late", 8)),
			),
			(
				SourceRef {
					layer:        1,
					publisher:    Str::new_static("a"),
					extension_id: Str::new_static("early"),
				},
				Some(summary("early", 4)),
			),
		];
		let resolution = resolve_verdicts(&mut verdicts);
		let compact = resolution.into_compact().expect("custom summary resolves");
		assert_eq!(compact.summary.as_str(), "early");
		assert_eq!(compact.superseded.len(), 1);
		assert_eq!(compact.superseded[0].extension_id.as_str(), "late");
	}
}
