//! In-memory event model for transcript v4 journals.

use std::path::PathBuf;

use omp_core::{Hash32, Principal, Provenance, Str};
use omp_proto::thread::v1::Item;
use serde_json::{
	Value,
	value::{RawValue, to_raw_value},
};

use super::{
	msg::{Content, Msg},
	patch::Patch,
	raweq::{opt_raw_eq, raw_eq},
	types::{
		AmendPatch, CallId, InvocationTransition, ModelChange, ModelId, ModelRef, Pin, ProviderId,
		RequestAudit, RequestError, SessionId, ThinkingSel, Tier, TitleSource, Usage,
	},
};
use crate::blob::BlobRef;

/// Canonical caller or harness input durably assigned to a logical turn.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TurnInputItem {
	/// Logical turn that must claim this input before opening transport.
	pub turn_id:     Str,
	/// Canonical input item.
	pub item:        Item,
	/// Deterministic prompt identity active when the input was staged.
	pub prompt_hash: Option<Hash32>,
}

impl Eq for TurnInputItem {}

/// One canonical thread item with journal-only turn metadata.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ItemRecord {
	/// Canonical gateway thread item.
	pub item:        Item,
	/// Turn that committed the item, absent for optimistic local input.
	pub turn_id:     Option<Str>,
	/// Deterministic system-prompt hash active when the item was recorded.
	pub prompt_hash: Option<Hash32>,
}

impl Eq for ItemRecord {}

/// Durable, field-exact proof that one gateway turn outcome was fully
/// journaled.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TurnReceipt {
	/// Gateway turn identifier.
	pub turn_id:            Str,
	/// Deterministic prompt identity fixed before the turn opened.
	pub prompt_hash:        Hash32,
	/// Ordered physical item events comprising the canonical system head.
	pub prompt_head_events: Vec<u64>,
	/// Physical event indexes of canonical items emitted by the outcome.
	pub item_events:        Vec<u64>,
	/// Complete authoritative terminal gateway outcome.
	pub outcome:            omp_proto::inference::v1::Outcome,
}

impl Eq for TurnReceipt {}
/// Exact canonical input fixed before an inference turn opens.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum TurnInputRecord {
	/// Complete thread for a stateless turn or context seed.
	Full {
		/// Exact submitted canonical thread.
		thread: omp_proto::thread::v1::Thread,
	},
	/// Atomic delta against one held gateway context revision.
	Delta {
		/// Exact context identity and optimistic-concurrency stamp.
		context: omp_proto::inference::v1::ContextRef,
		/// Exact submitted truncate-and-append delta.
		delta:   omp_proto::inference::v1::ThreadDelta,
	},
}

impl Eq for TurnInputRecord {}

/// Exact frozen options fixed before an inference turn opens.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TurnOptionsRecord {
	/// Context seeded by a full input, absent for stateless turns.
	pub context_id: Option<Str>,
	/// Canonical chat parameters.
	pub params:     omp_proto::inference::v1::ChatParams,
	/// In-turn invocation capability.
	pub executor:   Option<omp_proto::inference::v1::Executor>,
	/// Namespaced turn-level properties.
	pub props:      Option<omp_proto::inference::v1::ValueMap>,
}

impl Eq for TurnOptionsRecord {}

/// Durable proof that a logical turn was fixed before transport opened.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TurnStart {
	/// Gateway turn identifier reused by every retry of this logical turn.
	pub turn_id:            Str,
	/// Ordered physical item events claimed as this submission's input.
	pub item_events:        Vec<u64>,
	/// Deterministic prompt identity used to construct the submission.
	pub prompt_hash:        Hash32,
	/// Ordered physical item events comprising the canonical system head.
	pub prompt_head_events: Vec<u64>,
	/// Deterministic identity of the exact live tool registry used by the turn.
	pub toolset_hash:       Hash32,
	/// Stable ordered allowlist fixed for this exact submission.
	pub enabled_tools:      Vec<Str>,
	/// Ordered item events whose optimistic sequences require outcome patching.
	pub sequence_targets:   Vec<u64>,
	/// Exact canonical opening input used for every retry and crash replay.
	pub input:              TurnInputRecord,
	/// Exact frozen opening options used for every retry and crash replay.
	pub options:            TurnOptionsRecord,
}
/// Durable settlement for a logical turn that failed without a gateway outcome.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TurnAbort {
	/// Gateway turn identifier that must not be resumed.
	pub turn_id:     Str,
	/// Whether crash replay should continue this failed submission.
	pub recoverable: bool,
}

/// Durable intent for an atomic system-prompt head replacement.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PromptRewriteIntent {
	/// Deterministic identity of the replacement prompt.
	pub prompt_hash:    Hash32,
	/// Canonical replacement head items, retained for crash recovery.
	pub head:           Vec<Item>,
	/// Ordered live item-event indexes retained after the new head.
	pub preserved_tail: Vec<u64>,
}

impl Eq for PromptRewriteIntent {}

/// One materialized replacement-head item belonging to a rewrite intent.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PromptRewriteStage {
	/// Physical event index of the owning rewrite intent.
	pub intent:  u64,
	/// Zero-based position in the intent's replacement head.
	pub ordinal: u64,
	/// Canonical replacement-head item.
	pub item:    Item,
}

impl Eq for PromptRewriteStage {}

/// Atomic publication of one fully materialized prompt rewrite.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PromptRewriteCommit {
	/// Physical event index of the owning rewrite intent.
	pub intent:      u64,
	/// Ordered physical event indexes of every staged head item.
	pub head_events: Vec<u64>,
}

/// Durable authorization boundary before committed tool effects may start.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolBatchAuthorized {
	/// Gateway turn whose canonical output contains the calls.
	pub turn_id:  Str,
	/// Model-issued call identifiers authorized as one concurrent batch.
	pub call_ids: Vec<Str>,
}

/// Durable registration of detached work owned by the agent session.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JobRegistered {
	/// Full detached-job reference needed to resume settlement watching.
	pub job: omp_tool::JobRef,
}

/// Durable canonical settlement of one detached job.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct JobSettled {
	/// Stable environment job identifier.
	pub job_id:     Str,
	/// Canonical settlement item posted at the next turn boundary.
	pub settlement: Item,
}

impl Eq for JobSettled {}
/// Core-attributed outcome of one hook subscription or synthetic failure stub.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HookOutcome {
	/// Invocation under decision, when this was an invocation gate.
	pub invocation_id:   Option<Str>,
	/// Stable dense hook event id.
	pub event_id:        u8,
	/// Wire dispatch correlation id.
	pub dispatch_id:     u64,
	/// Subscription that reported this result; absent for a synthetic stub.
	pub subscription_id: Option<u32>,
	/// Wire phase discriminator.
	pub phase:           u8,
	/// Canonical decision arm.
	pub decision:        Str,
	/// Optional terminal reason.
	pub reason:          Option<Str>,
}

/// Durable audit sextet for the effective result of an invocation policy
/// decision.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PolicyDecision {
	/// Environment invocation identity.
	pub invocation_id:       Str,
	/// Requested target fixed at `ARGS_FINALIZED`.
	pub requested_target:    Str,
	/// Canonical requested arguments.
	pub requested_args:      Str,
	/// Ordered canonical transform overwrite records.
	pub transformations:     Vec<Str>,
	/// Effective target after accepted transforms.
	pub effective_target:    Str,
	/// Canonical effective arguments.
	pub effective_args:      Str,
	/// Derived-fact revision used by the accepted result.
	pub derived_ir_revision: u32,
	/// Whether the invocation was admitted.
	pub allowed:             bool,
	/// Optional denial reason.
	pub reason:              Option<Str>,
}

/// One approval reason filed in a Core-owned durable ticket.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ApprovalReason {
	/// User-visible title.
	pub title:         Str,
	/// TML-safe explanatory text.
	pub body:          Str,
	/// Exact command, path, or device subject.
	pub subject:       Str,
	/// Approval kind vocabulary.
	pub kind:          Str,
	/// Offered grant scopes.
	pub scopes:        Vec<Str>,
	/// Optional timeout default.
	pub default:       Option<bool>,
	/// Requested approver route.
	pub route:         Str,
	/// Optional named approver.
	pub approver:      Option<Str>,
	/// Maximum wait in milliseconds.
	pub timeout_ms:    u64,
	/// Unreachable-route behavior.
	pub unreachable:   Str,
	/// Whether only a human may decide.
	pub require_human: bool,
	/// Optional scope-bearing pattern.
	pub pattern:       Option<Str>,
	/// Rule and derived-fact evidence.
	pub evidence:      Vec<Str>,
}
/// Durable filing of a merged Core-owned approval ticket.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ApprovalTicketFiled {
	/// Stable idempotency key for approvers.
	pub ticket_id:     Str,
	/// Invocation blocked by this ticket, if any.
	pub invocation_id: Option<Str>,
	/// Every unresolved reason in filing order.
	pub reasons:       Vec<ApprovalReason>,
	/// Journal-clock filing time.
	pub created_at_ms: u64,
}

/// Durable idempotent resolution or withdrawal of an approval ticket.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ApprovalDecided {
	/// Stable ticket id.
	pub ticket_id:  Str,
	/// `decided` or `withdrawn`.
	pub state:      Str,
	/// Whether the ticket was approved, absent for withdrawal.
	pub approved:   Option<bool>,
	/// Granted scope, absent for withdrawal.
	pub scope:      Option<Str>,
	/// Answer source vocabulary.
	pub source:     Option<Str>,
	/// Authenticated decider, when present.
	pub decided_by: Option<Str>,
	/// Optional rationale.
	pub reason:     Option<Str>,
	/// Whether a fail-open result was audited.
	pub audited:    bool,
}
/// A deterministic losing custom-summary proposal retained without its summary
/// bytes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SupersededCompaction {
	/// Publisher extension identity ordered after the winning proposal.
	pub extension_id: Str,
	/// Stable rejection or supersession reason.
	pub reason:       Str,
}

/// One canonically encoded extension-authored journal entry.
#[derive(Debug, Clone)]
pub struct Custom {
	/// Extension-defined kind name.
	kind:       Str,
	/// Schema revision at which `data` was encoded.
	rev:        Option<Str>,
	/// Optional source within the authenticated extension.
	source:     Option<Str>,
	/// Authenticated principal stamped by the core.
	principal:  Principal,
	/// Authenticated extension provenance septet stamped by the core.
	provenance: Provenance,
	/// Canonical extension data.
	data:       Option<Box<RawValue>>,
	/// Optional content participating in model context.
	context:    Option<Content>,
	/// Whether clients should display the event.
	display:    bool,
}

impl Custom {
	/// Creates an entry and converts its data to the canonical compact JSON
	/// representation used by the append codec.
	pub fn new(
		kind: Str,
		rev: Option<Str>,
		source: Option<Str>,
		principal: Principal,
		provenance: Provenance,
		data: Option<Box<RawValue>>,
		context: Option<Content>,
		display: bool,
	) -> Result<Self, serde_json::Error> {
		let data = data
			.map(|raw| serde_json::from_str::<Value>(raw.get()).and_then(|value| to_raw_value(&value)))
			.transpose()?;
		Ok(Self { kind, rev, source, principal, provenance, data, context, display })
	}

	/// Returns the declared entry-kind name.
	#[must_use]
	pub fn kind(&self) -> &str {
		self.kind.as_str()
	}

	/// Returns the recorded schema revision.
	#[must_use]
	pub fn rev(&self) -> Option<&str> {
		self.rev.as_deref()
	}

	/// Returns the optional extension-local source.
	#[must_use]
	pub fn source(&self) -> Option<&str> {
		self.source.as_deref()
	}

	/// Returns the authenticated acting principal.
	#[must_use]
	pub const fn principal(&self) -> &Principal {
		&self.principal
	}

	/// Returns the authenticated extension provenance.
	#[must_use]
	pub const fn provenance(&self) -> &Provenance {
		&self.provenance
	}

	/// Returns the canonical data bytes.
	#[must_use]
	pub fn data(&self) -> Option<&RawValue> {
		self.data.as_deref()
	}

	/// Returns the materialized model-context projection.
	#[must_use]
	pub const fn context(&self) -> Option<&Content> {
		self.context.as_ref()
	}

	/// Returns whether clients should display the event.
	#[must_use]
	pub const fn display(&self) -> bool {
		self.display
	}

	/// Returns the same per-revision attribution key and value used by tool
	/// thread items.
	#[must_use]
	pub fn rev_attribution(&self) -> Option<(&'static str, &str)> {
		self
			.rev
			.as_ref()
			.map(|rev| (omp_tool::TOOL_REV_PROP, rev.as_str()))
	}
}

impl PartialEq for Custom {
	fn eq(&self, other: &Self) -> bool {
		self.kind == other.kind
			&& self.rev == other.rev
			&& self.source == other.source
			&& self.principal == other.principal
			&& self.provenance == other.provenance
			&& opt_raw_eq(self.data.as_deref(), other.data.as_deref())
			&& self.context == other.context
			&& self.display == other.display
	}
}

impl Eq for Custom {}

/// A valid JSON journal object that could not be decoded as recorded.
///
/// `raw` is the exact complete record and `value` is always `None`. Keeping a
/// typed event instead of dropping the record preserves its physical index and
/// makes corruption and forward-version records addressable.
#[derive(Debug, Clone)]
pub struct EntryUndecodable {
	/// Recorded event or extension-kind name, when recoverable.
	pub kind:   Option<Str>,
	/// Recorded schema revision, when recoverable.
	pub rev:    Option<Str>,
	/// Decoded value; always `None` for this event.
	pub value:  Option<Box<RawValue>>,
	/// Exact complete JSON record bytes.
	pub raw:    Box<RawValue>,
	/// Strict-decoding failure description.
	pub reason: Str,
}

impl PartialEq for EntryUndecodable {
	fn eq(&self, other: &Self) -> bool {
		self.kind == other.kind
			&& self.rev == other.rev
			&& opt_raw_eq(self.value.as_deref(), other.value.as_deref())
			&& raw_eq(&self.raw, &other.raw)
			&& self.reason == other.reason
	}
}

impl Eq for EntryUndecodable {}

/// Workspace identity required to reconstruct an equivalent child environment.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChildWorkspaceIdentity {
	/// Canonical Environment URI of the child root.
	pub root_uri:     Str,
	/// Durable isolated-workspace identity, when isolation was requested.
	pub isolation_id: Option<Str>,
	/// Content revision of the workspace snapshot used by this generation.
	pub revision:     Option<Hash32>,
}

/// Secret-free child initialization facts required for cross-process revival.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChildSessionInit {
	/// Resolved agent definition identity.
	pub definition:          Str,
	/// Child depth beneath the main session.
	pub depth:               u16,
	/// Content-addressed composed prompt.
	pub prompt_ref:          BlobRef,
	/// Content-addressed normalized output schema, when configured.
	pub schema_ref:          Option<BlobRef>,
	/// Content-addressed inherited policy snapshot.
	pub policy_snapshot_ref: BlobRef,
	/// Content-addressed inherited grant snapshot without credentials.
	pub grant_snapshot_ref:  BlobRef,
	/// Content-addressed frozen tool snapshot.
	pub tool_snapshot_ref:   BlobRef,
	/// Stable inference role requested for this child.
	pub model_role:          Str,
	/// Durable child workspace identity.
	pub workspace:           ChildWorkspaceIdentity,
	/// Actual serving model most recently attributed to the child.
	pub serving_model:       Option<ModelRef>,
}

/// Durable child lifecycle publication linked to its initialization entry.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChildLifecycleEntry {
	/// Stable child identity.
	pub child_id:        Str,
	/// Monotonic incarnation of the stable identity.
	pub generation:      u64,
	/// Physical event index of the child `Init` entry carrying revival facts.
	pub init_event:      u64,
	/// Lifecycle state encoded with the core vocabulary.
	pub lifecycle:       Str,
	/// Structured terminal classification, when this transition settles.
	pub terminal_status: Option<Str>,
}

/// Durable Snapcompact frames and measured admission accounting.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SnapcompactArchive {
	/// Normalized archive source used for later re-rendering.
	pub source:          BlobRef,
	/// Oldest-to-newest PNG frame blobs.
	pub frames:          Vec<BlobRef>,
	/// Active-tokenizer measurement of the source prefix.
	pub source_tokens:   u64,
	/// Conservative provider input tokens after imaging.
	pub image_tokens:    u64,
	/// Exact retained PNG bytes.
	pub png_bytes:       u64,
	/// Source characters dropped to satisfy hard frame and byte budgets.
	pub truncated_chars: u64,
	/// Stable shape description used to render these frames.
	pub shape:           Str,
}

/// A timestamped transcript event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
	/// Epoch-millisecond timestamp.
	pub ts:   u64,
	/// Event payload.
	pub kind: Kind,
}

/// An append-only transcript event kind.
#[derive(Debug, Clone)]
pub enum Kind {
	/// Initialize a session's prompt, tools, and spawn options.
	Init {
		/// Content-addressed system prompt.
		system_prompt: BlobRef,
		/// Tool names available to the session.
		tools:         Vec<Str>,
		/// Optional spawning agent identifier.
		agent:         Option<Str>,
		/// Optional response schema, preserved verbatim.
		output_schema: Option<Box<RawValue>>,
		/// Secret-free cold-revival facts for a child session.
		revival:       Option<ChildSessionInit>,
	},
	/// Record one retained child lifecycle transition.
	ChildLifecycle(ChildLifecycleEntry),
	/// Add a conversation message.
	Msg(Msg),
	/// Add one canonical gateway thread item.
	Item(ItemRecord),
	/// Record an inference failure with no conversational content.
	Failed {
		/// Request failure details.
		error: RequestError,
		/// Model selected for the failed request.
		model: ModelRef,
		/// Usage reported despite the failure, when available.
		usage: Option<Usage>,
	},
	/// Change one or more inference selections.
	Infer {
		/// Thinking-mode update.
		thinking: Patch<ThinkingSel>,
		/// Model-selection update.
		model:    Patch<ModelChange>,
		/// Service-tier update.
		tier:     Patch<Tier>,
		/// Credential-pin update.
		cred_pin: Patch<Pin>,
	},
	/// Record one Core-attributed hook result; never an extension `Custom`
	/// entry.
	HookOutcome(HookOutcome),
	/// Record the requested/effective invocation facts and transform audit
	/// trail.
	PolicyDecision(PolicyDecision),
	/// Persist one Core-owned merged approval ticket.
	ApprovalTicketFiled(ApprovalTicketFiled),
	/// Persist an idempotent ticket decision or guard-drop withdrawal.
	ApprovalDecided(ApprovalDecided),
	/// Move the implicit chain point to an earlier event or the root.
	Rewind {
		/// Target event index, or `None` for the root.
		to: Option<u64>,
	},
	/// Replace an old context prefix with a neutral summary.
	Compact {
		/// Full summary used for model context.
		summary:       Str,
		/// Optional shorter display summary.
		short:         Option<Str>,
		/// First pre-compaction event retained after the summary.
		first_kept:    u64,
		/// Token count before compaction.
		tokens_before: u64,
		/// Estimated token count after compaction, when measured.
		tokens_after:  Option<u64>,
		/// Ladder method that produced this compaction.
		method:        Option<Str>,
		/// Optional compaction warning.
		warning:       Option<Str>,
		/// Losing custom-summary proposals in deterministic publisher order.
		superseded:    Vec<SupersededCompaction>,
		/// Durable bitmap archive reattached to rebuilt model contexts.
		snapcompact:   Option<SnapcompactArchive>,
	},
	/// Summarize a branch before returning to another chain point.
	Branch {
		/// Event index from which the summarized branch began.
		from:    u64,
		/// Branch summary.
		summary: Str,
	},
	/// Start a fresh chain boundary, as for `/clear`.
	Reset,
	/// Request that inference discard provider-native session affinity while
	/// preserving the canonical journal and context, as for `/fresh`.
	ProviderReset,
	/// Assign a session title.
	Title {
		/// New title.
		title:  Str,
		/// Source that assigned the title.
		source: TitleSource,
	},
	/// Change the primary working root for future entries without rewriting the
	/// immutable journal header or prior history.
	MoveRoot {
		/// Canonical future primary root.
		root: PathBuf,
	},
	/// Add working directories available to the session.
	AddDirs {
		/// Directories added by this event.
		dirs: Vec<PathBuf>,
	},
	/// Remove secondary working directories from the session.
	///
	/// The session's primary root is fixed by `Init` metadata and is never
	/// removed by this mutation during projection.
	RemoveDirs {
		/// Directories removed by this event.
		dirs: Vec<PathBuf>,
	},
	/// Record lineage from a source session.
	ForkedFrom {
		/// Source session identifier.
		session: SessionId,
		/// Source event index, or the source session head when absent.
		at:      Option<u64>,
	},
	/// Replace accumulated provider-native history with checkpoint items.
	NativeCheckpoint {
		/// Provider whose replay stream the checkpoint replaces.
		provider: ProviderId,
		/// Model whose replay stream the checkpoint replaces.
		model:    ModelId,
		/// Content-addressed checkpoint item payload.
		items:    BlobRef,
	},
	/// Record tool calls aborted by an interrupted turn.
	Aborted {
		/// Bare call identifiers aborted by this event.
		tool_call_ids: Vec<CallId>,
	},
	/// Correct an earlier event without editing it.
	Amend {
		/// Event index receiving the correction.
		target: u64,
		/// Append-only correction.
		patch:  AmendPatch,
	},
	/// Stage one canonical input under its logical turn before transport opens.
	TurnInput(TurnInputItem),
	/// Begin an atomic prompt-head replacement without changing the live chain.
	PromptRewriteIntent(PromptRewriteIntent),
	/// Materialize one hidden item for a pending prompt-head replacement.
	PromptRewriteStage(PromptRewriteStage),
	/// Atomically publish a fully materialized prompt-head replacement.
	PromptRewriteCommit(PromptRewriteCommit),
	/// Register detached work for durable restart ownership.
	JobRegistered(JobRegistered),
	/// Record canonical detached-work settlement.
	JobSettled(JobSettled),
	/// Authorize one committed tool batch before any effects may start.
	ToolBatchAuthorized(ToolBatchAuthorized),
	/// Fix one logical gateway submission before opening its transport.
	TurnStart(TurnStart),
	/// Settle a started turn that failed without an authoritative outcome.
	TurnAbort(TurnAbort),

	/// Record completion of a gateway turn after all of its items were appended.
	TurnReceipt(TurnReceipt),
	/// Add, replace, or clear a label on an earlier event.
	Label {
		/// Event index receiving the label.
		target: u64,
		/// New label, or `None` to clear it.
		label:  Option<Str>,
	},
	/// Store one canonically encoded, core-attributed extension event.
	///
	/// `TurnStart`, not this payload, determines the enum's current size, so
	/// boxing this variant would add an allocation without shrinking [`Kind`].
	Custom(Custom),
	/// Record the authenticated request identity and indexes assigned to a
	/// durable operation.
	RequestAudit(RequestAudit),
	/// Fix the phase-specific facts of one tool invocation.
	InvocationTransition(InvocationTransition),
	/// Preserve an unrecognized or corrupt machine record verbatim.
	EntryUndecodable(EntryUndecodable),
}
/// Equality is byte equality of stored JSON text, preserving verbatim round
/// trips.
impl PartialEq for Kind {
	fn eq(&self, other: &Self) -> bool {
		match (self, other) {
			(
				Self::Init {
					system_prompt: a_system_prompt,
					tools: a_tools,
					agent: a_agent,
					output_schema: a_output_schema,
					revival: a_revival,
				},
				Self::Init {
					system_prompt: b_system_prompt,
					tools: b_tools,
					agent: b_agent,
					output_schema: b_output_schema,
					revival: b_revival,
				},
			) => {
				a_system_prompt == b_system_prompt
					&& a_tools == b_tools
					&& a_agent == b_agent
					&& opt_raw_eq(a_output_schema.as_deref(), b_output_schema.as_deref())
					&& a_revival == b_revival
			},
			(Self::ChildLifecycle(a), Self::ChildLifecycle(b)) => a == b,
			(Self::Msg(a_message), Self::Msg(b_message)) => a_message == b_message,
			(Self::Item(a), Self::Item(b)) => a == b,
			(
				Self::Failed { error: a_error, model: a_model, usage: a_usage },
				Self::Failed { error: b_error, model: b_model, usage: b_usage },
			) => (a_error, a_model, a_usage) == (b_error, b_model, b_usage),
			(
				Self::Infer {
					thinking: a_thinking,
					model: a_model,
					tier: a_tier,
					cred_pin: a_cred_pin,
				},
				Self::Infer {
					thinking: b_thinking,
					model: b_model,
					tier: b_tier,
					cred_pin: b_cred_pin,
				},
			) => (a_thinking, a_model, a_tier, a_cred_pin) == (b_thinking, b_model, b_tier, b_cred_pin),
			(Self::HookOutcome(a), Self::HookOutcome(b)) => a == b,
			(Self::PolicyDecision(a), Self::PolicyDecision(b)) => a == b,
			(Self::ApprovalTicketFiled(a), Self::ApprovalTicketFiled(b)) => a == b,
			(Self::ApprovalDecided(a), Self::ApprovalDecided(b)) => a == b,
			(Self::Rewind { to: a }, Self::Rewind { to: b }) => a == b,
			(
				Self::Compact {
					summary: a_summary,
					short: a_short,
					first_kept: a_first_kept,
					tokens_before: a_tokens_before,
					tokens_after: a_tokens_after,
					method: a_method,
					warning: a_warning,
					superseded: a_superseded,
					snapcompact: a_snapcompact,
				},
				Self::Compact {
					summary: b_summary,
					short: b_short,
					first_kept: b_first_kept,
					tokens_before: b_tokens_before,
					tokens_after: b_tokens_after,
					method: b_method,
					warning: b_warning,
					superseded: b_superseded,
					snapcompact: b_snapcompact,
				},
			) => {
				(
					a_summary,
					a_short,
					a_first_kept,
					a_tokens_before,
					a_tokens_after,
					a_method,
					a_warning,
					a_superseded,
					a_snapcompact,
				) == (
					b_summary,
					b_short,
					b_first_kept,
					b_tokens_before,
					b_tokens_after,
					b_method,
					b_warning,
					b_superseded,
					b_snapcompact,
				)
			},
			(
				Self::Branch { from: a_from, summary: a_summary },
				Self::Branch { from: b_from, summary: b_summary },
			) => (a_from, a_summary) == (b_from, b_summary),
			(Self::Reset, Self::Reset) | (Self::ProviderReset, Self::ProviderReset) => true,
			(
				Self::Title { title: a_title, source: a_source },
				Self::Title { title: b_title, source: b_source },
			) => (a_title, a_source) == (b_title, b_source),
			(Self::MoveRoot { root: a }, Self::MoveRoot { root: b }) => a == b,
			(Self::AddDirs { dirs: a }, Self::AddDirs { dirs: b }) => a == b,
			(Self::RemoveDirs { dirs: a }, Self::RemoveDirs { dirs: b }) => a == b,
			(
				Self::ForkedFrom { session: a_session, at: a_at },
				Self::ForkedFrom { session: b_session, at: b_at },
			) => (a_session, a_at) == (b_session, b_at),
			(
				Self::NativeCheckpoint { provider: a_provider, model: a_model, items: a_items },
				Self::NativeCheckpoint { provider: b_provider, model: b_model, items: b_items },
			) => (a_provider, a_model, a_items) == (b_provider, b_model, b_items),
			(Self::Aborted { tool_call_ids: a }, Self::Aborted { tool_call_ids: b }) => a == b,
			(Self::TurnInput(a), Self::TurnInput(b)) => a == b,
			(
				Self::Amend { target: a_target, patch: a_patch },
				Self::Amend { target: b_target, patch: b_patch },
			) => (a_target, a_patch) == (b_target, b_patch),
			(Self::PromptRewriteIntent(a), Self::PromptRewriteIntent(b)) => a == b,
			(Self::PromptRewriteStage(a), Self::PromptRewriteStage(b)) => a == b,
			(Self::PromptRewriteCommit(a), Self::PromptRewriteCommit(b)) => a == b,
			(Self::JobRegistered(a), Self::JobRegistered(b)) => a == b,
			(Self::JobSettled(a), Self::JobSettled(b)) => a == b,
			(Self::ToolBatchAuthorized(a), Self::ToolBatchAuthorized(b)) => a == b,
			(Self::TurnStart(a), Self::TurnStart(b)) => a == b,
			(Self::TurnAbort(a), Self::TurnAbort(b)) => a == b,
			(Self::TurnReceipt(a), Self::TurnReceipt(b)) => a == b,
			(
				Self::Label { target: a_target, label: a_label },
				Self::Label { target: b_target, label: b_label },
			) => (a_target, a_label) == (b_target, b_label),
			(Self::InvocationTransition(a), Self::InvocationTransition(b)) => a == b,
			(Self::Custom(a), Self::Custom(b)) => a == b,
			(Self::RequestAudit(a), Self::RequestAudit(b)) => a == b,
			(Self::EntryUndecodable(a), Self::EntryUndecodable(b)) => a == b,
			_ => false,
		}
	}
}

impl Eq for Kind {}
