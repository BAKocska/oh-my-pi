//! Leaf value types used by transcript events.

use omp_core::{InvocationPhase, Str};
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use serde_json::{
	Value,
	value::{RawValue, to_raw_value},
};
use smallvec::SmallVec;
use thiserror::Error;

use super::{
	block::Block,
	raweq::{opt_raw_eq, raw_eq},
};

macro_rules! string_id {
	($(#[$meta:meta])* $name:ident, $doc:literal) => {
		$(#[$meta])*
		#[doc = $doc]
		#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
		#[serde(transparent)]
		pub struct $name(
			/// The identifier text.
			pub Str,
		);
	};
}

string_id!(SessionId, "A stable transcript session identifier.");
string_id!(CallId, "A bare provider tool-call identifier.");
string_id!(DialectId, "A replay-capsule dialect identifier such as `oai` or `ant`.");
string_id!(FeatureId, "A feature identifier reported as unavailable for a turn.");
string_id!(ProviderId, "A model-provider identifier.");
string_id!(ModelId, "A provider model identifier.");

/// The fully qualified model selected for an inference request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
	/// Provider serving the request.
	pub provider: ProviderId,
	/// Provider API family used for the request.
	pub api:      Str,
	/// Provider model name.
	pub model:    ModelId,
}

/// Token usage reported for an inference request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
	/// Non-cached input tokens.
	pub input:       u64,
	/// Generated output tokens.
	pub output:      u64,
	/// Input tokens served from a provider cache.
	pub cache_read:  u64,
	/// Input tokens written into a provider cache.
	pub cache_write: u64,
}

/// Why an assistant turn stopped, with reason-specific provider details.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Stop {
	/// The model completed its turn normally.
	EndTurn,
	/// Generation reached its configured token limit.
	MaxTokens,
	/// The model stopped to request one or more tools.
	ToolUse,
	/// The model refused the request.
	Refusal {
		/// Verbatim provider details, when supplied.
		details: Option<Box<RawValue>>,
	},
	/// The turn was aborted after producing partial content.
	Aborted {
		/// Verbatim provider details, when supplied.
		details: Option<Box<RawValue>>,
	},
	/// Provider content filtering stopped the turn.
	ContentFilter {
		/// Verbatim provider details, when supplied.
		details: Option<Box<RawValue>>,
	},
}
/// Equality is byte equality of stored JSON text, preserving verbatim round
/// trips.
impl PartialEq for Stop {
	fn eq(&self, other: &Self) -> bool {
		match (self, other) {
			(Self::EndTurn, Self::EndTurn)
			| (Self::MaxTokens, Self::MaxTokens)
			| (Self::ToolUse, Self::ToolUse) => true,
			(Self::Refusal { details: a }, Self::Refusal { details: b })
			| (Self::Aborted { details: a }, Self::Aborted { details: b })
			| (Self::ContentFilter { details: a }, Self::ContentFilter { details: b }) => {
				opt_raw_eq(a.as_deref(), b.as_deref())
			},
			_ => false,
		}
	}
}

impl Eq for Stop {}

#[derive(Deserialize)]
struct StopProbe {
	reason: Str,
}

#[derive(Deserialize)]
struct StopDetails {
	details: Option<Box<RawValue>>,
}

impl<'de> Deserialize<'de> for Stop {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>,
	{
		let raw = Box::<RawValue>::deserialize(deserializer)?;
		let probe: StopProbe = serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
		match probe.reason.as_str() {
			"end_turn" => Ok(Self::EndTurn),
			"max_tokens" => Ok(Self::MaxTokens),
			"tool_use" => Ok(Self::ToolUse),
			"refusal" => {
				let payload: StopDetails = serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
				Ok(Self::Refusal { details: payload.details })
			},
			"aborted" => {
				let payload: StopDetails = serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
				Ok(Self::Aborted { details: payload.details })
			},
			"content_filter" => {
				let payload: StopDetails = serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
				Ok(Self::ContentFilter { details: payload.details })
			},
			reason => Err(D::Error::custom(format_args!("unknown stop reason `{reason}`"))),
		}
	}
}

/// Wall-clock measurements for an assistant turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timing {
	/// Total request duration in milliseconds.
	pub duration_ms: u64,
	/// Time to the first generated token in milliseconds.
	pub ttft_ms:     u64,
}

/// Context-window state observed when a request was sent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CtxSnapshot {
	/// Tokens occupying the model context window.
	pub tokens: u64,
	/// Maximum tokens available in the context window.
	pub limit:  u64,
}

/// Origin information attached to user or developer content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attribution {
	/// Stable source kind, such as a user, hook, or imported session.
	pub source: Str,
	/// Optional source-specific identifier.
	pub id:     Option<Str>,
}

/// A failed inference request that produced no conversational content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestError {
	/// Human-readable error message.
	pub message: Str,
	/// Provider or protocol error code.
	pub code:    Option<Str>,
	/// HTTP or provider-equivalent status code.
	pub status:  Option<u16>,
	/// Verbatim structured error details.
	pub details: Option<Box<RawValue>>,
}
/// Equality is byte equality of stored JSON text, preserving verbatim round
/// trips.
impl PartialEq for RequestError {
	fn eq(&self, other: &Self) -> bool {
		self.message == other.message
			&& self.code == other.code
			&& self.status == other.status
			&& opt_raw_eq(self.details.as_deref(), other.details.as_deref())
	}
}

impl Eq for RequestError {}

/// The source that assigned a transcript title.
#[derive(
	Debug,
	Clone,
	Copy,
	PartialEq,
	Eq,
	Serialize,
	Deserialize,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum TitleSource {
	/// A person explicitly chose the title.
	User,
	/// An assistant generated the title.
	Assistant,
	/// The runtime assigned the title.
	System,
	/// Migration imported the title from an older journal.
	Imported,
}

/// An append-only correction to an earlier transcript event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AmendPatch {
	/// Prune an earlier assistant message to a prefix of its blocks.
	Prune {
		/// Number of leading blocks that remain live.
		keep_blocks: u64,
	},
	/// Restore the original assistant turn after a failed retry attempt.
	RetryRecovery {
		/// Original assistant blocks replaced by the retry.
		content:     Vec<Block>,
		/// Original stop reason.
		stop:        Stop,
		/// Original token usage.
		usage:       Usage,
		/// Original provider response identifier, when present.
		response_id: Option<Str>,
	},
	/// Assign the gateway sequence to an optimistically recorded canonical item.
	Seq {
		/// Gateway-assigned dense thread sequence.
		seq: u64,
	},
}

#[derive(Deserialize)]
struct AmendProbe {
	op: Str,
}

#[derive(Deserialize)]
struct PrunePayload {
	keep_blocks: u64,
}

#[derive(Deserialize)]
struct RetryRecoveryPayload {
	content:     Vec<Block>,
	stop:        Stop,
	usage:       Usage,
	response_id: Option<Str>,
}

#[derive(Deserialize)]
struct SeqPayload {
	seq: u64,
}

impl<'de> Deserialize<'de> for AmendPatch {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>,
	{
		let raw = Box::<RawValue>::deserialize(deserializer)?;
		let probe: AmendProbe = serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
		match probe.op.as_str() {
			"prune" => {
				let payload: PrunePayload =
					serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
				Ok(Self::Prune { keep_blocks: payload.keep_blocks })
			},
			"retry_recovery" => {
				let payload: RetryRecoveryPayload =
					serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
				Ok(Self::RetryRecovery {
					content:     payload.content,
					stop:        payload.stop,
					usage:       payload.usage,
					response_id: payload.response_id,
				})
			},
			"seq" => {
				let payload: SeqPayload = serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
				Ok(Self::Seq { seq: payload.seq })
			},
			op => Err(D::Error::custom(format_args!("unknown amendment operation `{op}`"))),
		}
	}
}

/// Effective and user-configured thinking selections for an inference request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingSel {
	/// Selection actually sent to the provider.
	pub effective:  Str,
	/// Selection configured by the user, including automatic modes.
	pub configured: Str,
}

/// A role-specific model selection change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelChange {
	/// Model role affected by the change.
	pub role:     Str,
	/// New model selection.
	pub model:    ModelRef,
	/// Whether this selection is a fallback rather than the primary choice.
	pub fallback: bool,
}

/// A provider service-tier selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Tier(
	/// The provider tier name.
	pub Str,
);

/// A credential pin used to keep a session on a stable provider account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pin {
	/// Provider whose credential is pinned.
	pub provider:   ProviderId,
	/// Provider-local credential identifier.
	pub credential: Str,
}

/// Core-authenticated identity and replay result of one durable request.
///
/// The generation and idempotency fields are copied from the authenticated
/// request envelope after the core accepts its generation fence. `indexes`
/// records the exact journal events assigned to the logical operation so a
/// retry can return the original result without applying it twice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestAudit {
	/// Unique identifier for this attempt.
	pub request_id:         Str,
	/// Stable identity shared by retries of the logical operation.
	pub idempotency_key:    Str,
	/// Authenticated extension namespace owning this idempotency key.
	pub extension_id:       Str,
	/// Authenticated extension-host incarnation.
	pub host_generation:    u64,
	/// Session incarnation against which the operation was accepted.
	pub session_generation: u64,
	/// Stable operation vocabulary supplied by the core request router.
	pub operation:          Str,
	/// Exact journal indexes returned by the first successful application.
	pub indexes:            SmallVec<u64, 8>,
}

/// Phase-specific durable facts fixed by one invocation transition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvocationTransition {
	/// Stable invocation identity shared by all seven transitions.
	pub invocation_id:        Str,
	/// Provider call identity for the invocation.
	pub call_id:              CallId,
	/// Canonical phase whose facts this record fixes.
	pub phase:                InvocationPhase,
	/// Canonical requested arguments, present only at `ARGS_FINALIZED`.
	pub requested_args:       Option<Box<RawValue>>,
	/// Ordered argument transformation trail, present only at `ADMITTED`.
	pub transformations:      Option<SmallVec<Box<RawValue>, 4>>,
	/// Frozen effective arguments, present only at `ADMITTED`.
	pub effective_args:       Option<Box<RawValue>>,
	/// Structured admission receipt, present only at `ADMITTED`.
	pub admission_receipt:    Option<Box<RawValue>>,
	/// Durable assistant-item journal index, present only when committed.
	pub assistant_item_event: Option<u64>,
	/// Core-issued scoped effect token, present only at `EFFECTS_AUTHORIZED`.
	pub effect_token:         Option<Str>,
	/// Exact core-narrowed effect envelope paired with the scoped token.
	pub effects:              Option<omp_tool::Effects>,
	/// Epoch-millisecond effect authorization time, paired with the token.
	pub authorized_at:        Option<u64>,
	/// Single durable call-outcome reference, present only at `SETTLED`.
	pub outcome:
		Option<omp_tool::CallOutcome<omp_tool::CallOutcomeDetails, omp_tool::CallOutcomeDetails>>,
}

impl InvocationTransition {
	/// Validates that exactly the facts fixed by `phase` are present and that
	/// structured fact bytes use canonical compact JSON.
	pub fn validate(&self) -> Result<(), InvocationTransitionError> {
		let canonical = self.requested_args.as_deref().is_none_or(raw_is_canonical)
			&& self.effective_args.as_deref().is_none_or(raw_is_canonical)
			&& self
				.admission_receipt
				.as_deref()
				.is_none_or(raw_is_canonical)
			&& self
				.transformations
				.as_ref()
				.is_none_or(|trail| trail.iter().all(|raw| raw_is_canonical(raw)))
			&& self.outcome.as_ref().is_none_or(outcome_is_canonical);
		let phase_fields = match self.phase {
			InvocationPhase::Open | InvocationPhase::Admission => {
				self.requested_args.is_none()
					&& self.transformations.is_none()
					&& self.effective_args.is_none()
					&& self.admission_receipt.is_none()
					&& self.assistant_item_event.is_none()
					&& self.effect_token.is_none()
					&& self.effects.is_none()
					&& self.authorized_at.is_none()
					&& self.outcome.is_none()
			},
			InvocationPhase::ArgsFinalized => {
				self.requested_args.is_some()
					&& self.transformations.is_none()
					&& self.effective_args.is_none()
					&& self.admission_receipt.is_none()
					&& self.assistant_item_event.is_none()
					&& self.effect_token.is_none()
					&& self.effects.is_none()
					&& self.authorized_at.is_none()
					&& self.outcome.is_none()
			},
			InvocationPhase::Admitted => {
				self.requested_args.is_none()
					&& self.transformations.is_some()
					&& self.effective_args.is_some()
					&& self.admission_receipt.is_some()
					&& self.assistant_item_event.is_none()
					&& self.effect_token.is_none()
					&& self.effects.is_none()
					&& self.authorized_at.is_none()
					&& self.outcome.is_none()
			},
			InvocationPhase::AssistantItemCommitted => {
				self.requested_args.is_none()
					&& self.transformations.is_none()
					&& self.effective_args.is_none()
					&& self.admission_receipt.is_none()
					&& self.assistant_item_event.is_some()
					&& self.effect_token.is_none()
					&& self.effects.is_none()
					&& self.authorized_at.is_none()
					&& self.outcome.is_none()
			},
			InvocationPhase::EffectsAuthorized => {
				self.requested_args.is_none()
					&& self.transformations.is_none()
					&& self.effective_args.is_none()
					&& self.admission_receipt.is_none()
					&& self.assistant_item_event.is_none()
					&& self.effect_token.is_some()
					&& self.effects.is_some()
					&& self.authorized_at.is_some()
					&& self.outcome.is_none()
			},
			InvocationPhase::Settled => {
				self.requested_args.is_none()
					&& self.transformations.is_none()
					&& self.effective_args.is_none()
					&& self.admission_receipt.is_none()
					&& self.assistant_item_event.is_none()
					&& self.effect_token.is_none()
					&& self.effects.is_none()
					&& self.authorized_at.is_none()
					&& self.outcome.is_some()
			},
		};
		if canonical && phase_fields {
			Ok(())
		} else {
			Err(InvocationTransitionError { phase: self.phase })
		}
	}
}

fn raw_is_canonical(raw: &RawValue) -> bool {
	serde_json::from_str::<Value>(raw.get())
		.and_then(|value| to_raw_value(&value))
		.is_ok_and(|canonical| canonical.get() == raw.get())
}

fn outcome_is_canonical(
	outcome: &omp_tool::CallOutcome<omp_tool::CallOutcomeDetails, omp_tool::CallOutcomeDetails>,
) -> bool {
	let details = match outcome {
		omp_tool::CallOutcome::Ok(details) | omp_tool::CallOutcome::Faulted(details) => details,
		omp_tool::CallOutcome::ArgsRejected(_) | omp_tool::CallOutcome::Aborted { .. } => {
			return true;
		},
	};
	match details {
		omp_tool::CallOutcomeDetails::Inline { json } => serde_json::from_slice::<Value>(json)
			.and_then(|value| serde_json::to_vec(&value))
			.is_ok_and(|canonical| canonical.as_slice() == json.as_ref()),
		omp_tool::CallOutcomeDetails::Spilled { .. } => true,
	}
}

impl PartialEq for InvocationTransition {
	fn eq(&self, other: &Self) -> bool {
		self.invocation_id == other.invocation_id
			&& self.call_id == other.call_id
			&& self.phase == other.phase
			&& opt_raw_eq(self.requested_args.as_deref(), other.requested_args.as_deref())
			&& match (&self.transformations, &other.transformations) {
				(Some(a), Some(b)) => a.len() == b.len() && a.iter().zip(b).all(|(a, b)| raw_eq(a, b)),
				(None, None) => true,
				_ => false,
			} && opt_raw_eq(self.effective_args.as_deref(), other.effective_args.as_deref())
			&& opt_raw_eq(self.admission_receipt.as_deref(), other.admission_receipt.as_deref())
			&& self.assistant_item_event == other.assistant_item_event
			&& self.effect_token == other.effect_token
			&& self.effects == other.effects
			&& self.authorized_at == other.authorized_at
			&& self.outcome == other.outcome
	}
}

impl Eq for InvocationTransition {}

/// A transition carried facts that do not belong to its recorded phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("invocation transition has fields inconsistent with phase {phase}")]
pub struct InvocationTransitionError {
	phase: InvocationPhase,
}

impl InvocationTransitionError {
	/// Returns the phase whose fact invariant was violated.
	#[must_use]
	pub const fn phase(self) -> InvocationPhase {
		self.phase
	}
}
