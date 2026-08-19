//! Typed, revisioned tool contracts for the agent/environment boundary.
//!
//! Execution is deliberately absent from this crate. A tool keeps concrete
//! parameter and result types until [`Registry::register`], while prompt
//! projection and revision lifting remain deterministic shared code.

mod incoming;
mod registry;

use std::{fmt, future::Future};

use bytes::Bytes;
use futures::Stream;
pub use incoming::{
	CommitError, IncomingParams, Interrupt, InterruptWaitError, InterruptibleParams,
	InvocationEvent, InvocationFeed, InvocationSendError, ParamError,
};
use omp_core::{InvocationPhase, Str};
pub use omp_proto::inference::v1::Fallback;
pub use registry::{
	Claim, Claims, ConstraintDisposition, ErasedEv, ErasedOutcome, ErasedStream, LoweredTool,
	LoweringCaps, MountedDevice, Precedence, ProjectedCall, ProjectedVerdict, Registry,
	RegistryError, ShadowClaim, ToolRoute,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use smallvec::SmallVec;
use thiserror::Error;

/// Generates the compact, deterministic JSON Schema exposed to models for `T`.
///
/// Subschemas are inlined and generator metadata is omitted. Schemas describe
/// deserialization, matching how tool parameters are consumed.
pub fn schema<T: schemars::JsonSchema>() -> Bytes {
	let generator = schemars::generate::SchemaSettings::draft2020_12()
		.with(|settings| {
			settings.inline_subschemas = true;
			settings.meta_schema = None;
		})
		.for_deserialize()
		.into_generator();
	let mut root = generator.into_root_schema_for::<T>();
	root.remove("$schema");
	root.remove("title");
	Bytes::from(
		serde_json::to_vec(root.as_value())
			.expect("schemars-generated JSON Schema must serialize to compact JSON"),
	)
}

/// Namespaced thread-item property carrying a committed tool revision.
pub const TOOL_REV_PROP: &str = "omp/tool-rev";

/// Model-facing registration surface for a tool declaration.
#[derive(
	Clone,
	Copy,
	Debug,
	Deserialize,
	Eq,
	Hash,
	PartialEq,
	Serialize,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Presentation {
	/// A stable schema slot advertised directly to the model.
	Slot,
	/// A catalog entry reached through the dynamic device tool.
	Device,
}

/// One argument-dialect revision within a revision family.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct Rev {
	/// Argument-dialect family, such as `hl` or `rep`.
	pub family: Str,
	/// Monotonic revision within `family`.
	pub n:      u16,
}

impl fmt::Display for Rev {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		if self.family.is_empty() {
			write!(f, "{}", self.n)
		} else {
			write!(f, "{}.{}", self.family, self.n)
		}
	}
}

/// Durable identity of a tool call in a transcript.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ToolIdentity {
	/// Stable model-facing name.
	pub name: Str,
	/// Argument and rendering revision.
	pub rev:  Rev,
}

/// Static description of one tool revision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolSpec {
	/// Stable wire name exposed to models.
	pub name:            Str,
	/// Transcript revision.
	pub rev:             Rev,
	/// Model-facing purpose.
	pub description:     Str,
	/// Complete JSON Schema bytes.
	pub schema:          Bytes,
	/// Requested constrained-sampling behavior.
	pub constraint:      Constraint,
	/// Content identity of the code that produces model-facing projections.
	///
	/// Native registrations use their crate/build identity. Supervised workers
	/// use the frozen module-content hash supplied at registration.
	pub projection_code: [u8; 32],
}

/// Computes a native projection-code identity without allocating.
///
/// `module_source` must contain the source bytes that implement the tool's
/// projection. Package identity separates equal source shipped by unrelated
/// crates, while source bytes move the identity when projection code changes.
#[must_use]
pub fn native_projection_code(
	crate_name: &str,
	crate_version: &str,
	module_source: &[u8],
) -> [u8; 32] {
	let mut hasher = blake3::Hasher::new();
	for field in [crate_name.as_bytes(), crate_version.as_bytes(), module_source] {
		hasher.update(&(field.len() as u64).to_le_bytes());
		hasher.update(field);
	}
	*hasher.finalize().as_bytes()
}

impl ToolSpec {
	/// Returns the durable `(name, family/n)` identity.
	pub fn identity(&self) -> ToolIdentity {
		ToolIdentity { name: self.name.clone(), rev: self.rev.clone() }
	}
}

/// Requested argument-sampling constraint.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Constraint {
	/// Ordinary lenient JSON arguments.
	None,
	/// Strict JSON Schema sampling when supported.
	Schema {
		/// Relative request priority retained for upstream negotiation.
		priority:       u8,
		/// Required behavior when the selected route lacks strict sampling.
		#[serde(default)]
		on_unsupported: Fallback,
	},
	/// Freeform input constrained by a grammar.
	Grammar {
		/// Grammar language.
		syntax:         GrammarSyntax,
		/// Complete grammar definition.
		definition:     Str,
		/// Relative request priority retained for upstream negotiation.
		priority:       u8,
		/// Required behavior when the selected route lacks this grammar.
		#[serde(default)]
		on_unsupported: Fallback,
	},
}

/// Grammar languages represented in the model catalog.
#[derive(
	Clone,
	Copy,
	Debug,
	Deserialize,
	Eq,
	PartialEq,
	Serialize,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum GrammarSyntax {
	/// Lark grammar.
	Lark,
	/// Regular expression.
	Regex,
	/// Extended Backus-Naur form.
	Ebnf,
}

/// Argument dialect used by the live tool revision.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Deserialize,
	Eq,
	Hash,
	PartialEq,
	Serialize,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
#[repr(u8)]
pub enum Dialect {
	/// Hashline's snapshot-anchored edit language.
	#[serde(rename = "hl")]
	#[strum(serialize = "hl")]
	Hashline,
	/// Old-text/new-text replacement.
	#[serde(rename = "rep")]
	#[strum(serialize = "rep")]
	Replace,
	/// Patch-envelope or unified-diff input.
	#[serde(rename = "patch")]
	#[strum(serialize = "patch")]
	Patch,
	/// A vendor-trained or otherwise unclassified native dialect.
	#[default]
	#[serde(rename = "native")]
	#[strum(serialize = "native")]
	Native,
}

impl Dialect {
	/// Classifies a revision family without consulting model names.
	#[must_use]
	pub fn for_rev(rev: &Rev) -> Self {
		rev.family.parse().unwrap_or_default()
	}
}

/// Coarse, ordered model capability band used only for projection verbosity.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Deserialize,
	Eq,
	Hash,
	Ord,
	PartialEq,
	PartialOrd,
	Serialize,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
#[repr(u8)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ModelClass {
	/// Embedded classification or titling model.
	Tiny     = 0,
	/// Small local model.
	Small    = 1,
	/// Mainstream hosted model and the conservative default.
	#[default]
	Standard = 2,
	/// Long-context flagship model.
	Frontier = 3,
}

/// Model-wide projection inputs shared by every tool in one request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CapsBase {
	/// Maximum number of parts a tool may emit.
	pub maximum_parts:      u16,
	/// Maximum aggregate UTF-8 text bytes.
	pub maximum_text_bytes: u32,
	/// Whether blob-backed media parts may be exposed to the model.
	pub media:              bool,
	/// Catalog-derived model capability band.
	pub model_class:        ModelClass,
}

/// Deterministic model-facing projection budget for one live tool revision.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PromptCaps {
	/// Maximum number of parts a tool may emit.
	pub maximum_parts:      u16,
	/// Maximum aggregate UTF-8 text bytes.
	pub maximum_text_bytes: u32,
	/// Whether blob-backed media parts may be exposed to the model.
	pub media:              bool,
	/// Argument dialect derived from the live revision family.
	#[serde(default)]
	pub dialect:            Dialect,
	/// Catalog-derived model capability band.
	#[serde(default)]
	pub model_class:        ModelClass,
}

impl PromptCaps {
	/// Combines model-wide limits with the dialect of `live_rev`.
	#[must_use]
	pub fn for_tool(base: CapsBase, live_rev: &Rev) -> Self {
		Self {
			maximum_parts:      base.maximum_parts,
			maximum_text_bytes: base.maximum_text_bytes,
			media:              base.media,
			dialect:            Dialect::for_rev(live_rev),
			model_class:        base.model_class,
		}
	}

	/// Returns the model-wide inputs independent of a tool revision.
	#[must_use]
	pub const fn base(self) -> CapsBase {
		CapsBase {
			maximum_parts:      self.maximum_parts,
			maximum_text_bytes: self.maximum_text_bytes,
			media:              self.media,
			model_class:        self.model_class,
		}
	}
}

/// Whether an operation leaves durable state.
#[derive(
	Clone,
	Copy,
	Debug,
	Deserialize,
	Eq,
	Hash,
	PartialEq,
	Serialize,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Durability {
	/// No durable state is promised.
	Ephemeral,
	/// The operation acknowledges a durable state transition.
	Durable,
}

/// Cost class charged by an operation.
#[derive(
	Clone,
	Copy,
	Debug,
	Deserialize,
	Eq,
	Hash,
	PartialEq,
	Serialize,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum CostClass {
	/// No separately metered resource is consumed.
	None,
	/// A bounded local or quota-metered resource is consumed.
	Metered,
	/// A paid upstream resource may be consumed.
	Paid,
}

/// Runtime authority responsible for enforcing an operation specification.
#[derive(
	Clone,
	Copy,
	Debug,
	Deserialize,
	Eq,
	Hash,
	PartialEq,
	Serialize,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Authority {
	/// The core control-plane boundary enforces the operation.
	Core,
	/// The environment data-plane boundary enforces the operation.
	Environment,
}

/// Generated phase, durability, cost, and authority metadata for one operation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct OperationSpec {
	/// Earliest invocation phase in which the operation is legal.
	pub minimum_phase: InvocationPhase,
	/// Whether the operation leaves durable state.
	pub durability:    Durability,
	/// Resource cost class.
	pub cost:          CostClass,
	/// Boundary that authoritatively enforces `minimum_phase`.
	pub authority:     Authority,
}

/// A content-addressed blob reference suitable for durable projection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BlobRef {
	/// Content hash in the environment blob namespace.
	pub hash:       Str,
	/// MIME type of the stored bytes.
	pub media_type: Str,
	/// Exact stored byte length.
	pub byte_len:   u64,
}

/// One model-facing tool-result part.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Part {
	/// UTF-8 model-visible text.
	Text {
		/// Model-visible text payload.
		text: Str,
	},
	/// Structured JSON retained as exact bytes.
	Json {
		/// Raw JSON byte payload.
		json: Bytes,
	},
	/// Blob-backed media; never inline base64.
	Blob {
		/// Durable blob reference.
		blob: BlobRef,
		/// Optional deterministic accessibility/model fallback.
		alt:  Option<Str>,
	},
}

/// One typed tool implementation.
pub trait Tool: Send + Sync + 'static {
	/// Declared whole-argument shape for tools which opt into whole validation.
	type Params: DeserializeOwned;
	/// Ephemeral progress payload.
	type Update: Serialize + DeserializeOwned + Send;
	/// Durable successful result.
	type Payload: Serialize + DeserializeOwned + Send;
	/// Durable typed failure.
	type Fault: Serialize + DeserializeOwned + Send;

	/// Returns this implementation's immutable specification.
	fn spec(&self) -> &ToolSpec;

	/// Executes one invocation from its single linear argument/event stream.
	fn call<'c>(
		&'c self,
		params: IncomingParams<'c>,
	) -> impl Stream<Item = Ev<Self::Update, Self::Payload, Self::Fault>> + Send + 'c;

	/// Deterministically projects either durable tool branch for one model.
	fn prompt(&self, view: Result<&Self::Payload, &Self::Fault>, caps: &PromptCaps) -> Vec<Part>;

	/// Projects one typed ephemeral update into an optional live invocation
	/// frame.
	///
	/// The default keeps ordinary tool progress on the agent event feed only.
	fn invoke_input(
		&self,
		_update: &Self::Update,
		_invocation_id: &str,
	) -> Option<omp_proto::inference::v1::InvokeInput> {
		None
	}

	/// Deterministically migrates one historical call toward this revision.
	fn lift(&self, _from: &Rev, _call: RecordedCall<'_>) -> Option<LiftedCall> {
		None
	}
}

/// One event emitted by a typed tool invocation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Ev<U, P, F> {
	/// Ephemeral progress, never transcript history.
	Update(U),
	/// Terminal structured failure of a parameter the tool pulled.
	Args(ArgIssue),
	/// Terminal structured cancellation or effect-uncertainty report.
	Aborted(Abort),
	/// Terminal event; supervisors fuse the stream after this event.
	Done(ToolTerminal<P, F>),
}

/// Terminal executor result before durable call-outcome lowering.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolTerminal<P, F> {
	/// A synchronous success or typed fault.
	Done {
		/// Tool-owned durable branch.
		result:  Result<P, F>,
		/// Whether model-facing parts may be compacted while truth survives.
		useless: bool,
	},
	/// Work continues outside the turn and will settle through the job board.
	Detached(JobRef),
}

/// Journaled truth for exactly one of a settled call's four branches.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum CallOutcome<P, F> {
	/// Successful durable payload.
	Ok(P),
	/// Tool-owned durable fault.
	Faulted(F),
	/// Structured failure of a parameter the tool actually pulled.
	ArgsRejected(ArgIssue),
	/// Structured cancellation, skip, or policy denial.
	Aborted {
		/// Fine-grained owner-reported abort reason.
		abort:  Abort,
		/// Coarse machine-readable abort class.
		kind:   AbortKind,
		/// Structured denial when `kind` is [`AbortKind::PolicyDenied`].
		#[serde(default, skip_serializing_if = "Option::is_none")]
		policy: Option<PolicyDenied>,
	},
}

impl<P, F> CallOutcome<P, F> {
	/// Creates a non-policy abort, deriving its coarse class from `abort`.
	#[must_use]
	pub fn aborted(abort: Abort) -> Self {
		let kind = abort.kind();
		Self::Aborted { abort, kind, policy: None }
	}

	/// Creates a structured policy denial.
	#[must_use]
	pub fn policy_denied(abort: Abort, policy: PolicyDenied) -> Self {
		Self::Aborted { abort, kind: AbortKind::PolicyDenied, policy: Some(policy) }
	}
}

#[derive(Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum CallOutcomeRepr<P, F> {
	Ok(P),
	#[serde(alias = "fault")]
	Faulted(F),
	#[serde(alias = "args")]
	ArgsRejected(ArgIssue),
	Aborted(AbortedRepr),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum AbortedRepr {
	Current {
		abort:  Abort,
		#[serde(default)]
		kind:   Option<AbortKind>,
		#[serde(default)]
		policy: Option<PolicyDenied>,
	},
	Legacy(Abort),
}

impl<'de, P, F> Deserialize<'de> for CallOutcome<P, F>
where
	P: Deserialize<'de>,
	F: Deserialize<'de>,
{
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		Ok(match CallOutcomeRepr::<P, F>::deserialize(deserializer)? {
			CallOutcomeRepr::Ok(payload) => Self::Ok(payload),
			CallOutcomeRepr::Faulted(fault) => Self::Faulted(fault),
			CallOutcomeRepr::ArgsRejected(issue) => Self::ArgsRejected(issue),
			CallOutcomeRepr::Aborted(AbortedRepr::Legacy(abort)) => Self::aborted(abort),
			CallOutcomeRepr::Aborted(AbortedRepr::Current { abort, kind, policy }) => {
				let kind = kind.unwrap_or_else(|| {
					if policy.is_some() {
						AbortKind::PolicyDenied
					} else {
						abort.kind()
					}
				});
				let carries_policy = policy.is_some();
				if (kind == AbortKind::PolicyDenied) != carries_policy {
					return Err(serde::de::Error::custom(
						"policy is present if and only if abort kind is policy_denied",
					));
				}
				Self::Aborted { abort, kind, policy }
			},
		})
	}
}

/// One segment in a pulled JSON path.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ArgPath {
	/// Object key.
	Key(Str),
	/// Array index.
	Index(u64),
}

/// Stable class of parameter pull failure.
#[derive(
	Clone,
	Copy,
	Debug,
	Deserialize,
	Eq,
	PartialEq,
	Serialize,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ArgIssueKind {
	/// Required pulled value was absent.
	Missing,
	/// Input ended before the pulled value completed.
	Incomplete,
	/// Input was explicitly or implicitly abandoned.
	Aborted,
	/// Complete input was malformed.
	Malformed,
	/// Pulled value had another JSON shape.
	TypeMismatch,
	/// Invocation framing violated the linear stream contract.
	Protocol,
}

/// Structured issue for one parameter the tool pulled.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ArgIssue {
	/// Full pulled key/index path.
	pub path:     Vec<ArgPath>,
	/// Requested shape.
	pub expected: Str,
	/// Stable failure class.
	pub kind:     ArgIssueKind,
	/// Optional valid example for model repair.
	pub example:  Option<Str>,
	/// Observed shape for [`ArgIssueKind::TypeMismatch`].
	pub found:    Option<Str>,
}

/// Structured reason an invocation did not produce a normal outcome.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Abort {
	/// Call was deliberately not started.
	Skipped {
		/// Explanation of why invocation execution was bypassed.
		reason: Str,
	},
	/// Owner observed interruption before effects could land.
	Interrupted {
		/// Explanation of the interruption event or signal.
		reason: Str,
	},
	/// Cancellation raced an effect and only the owner can report uncertainty.
	EffectsUnknown {
		/// Explanation of why side-effect state cannot be confirmed.
		reason: Str,
	},
	/// Invocation feed disappeared before explicit commitment.
	InputDropped,
	/// Executor stream ended without a terminal event.
	MissingOutcome,
}

impl Abort {
	/// Returns the coarse class implied by this owner-reported reason.
	#[must_use]
	pub const fn kind(&self) -> AbortKind {
		match self {
			Self::Skipped { .. } | Self::InputDropped => AbortKind::Skipped,
			Self::Interrupted { .. } | Self::EffectsUnknown { .. } | Self::MissingOutcome => {
				AbortKind::Cancelled
			},
		}
	}
}

/// Coarse machine-readable class of an aborted invocation.
#[derive(
	Clone,
	Copy,
	Debug,
	Deserialize,
	Eq,
	Hash,
	PartialEq,
	Serialize,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum AbortKind {
	/// A dispatched call failed to settle normally.
	Cancelled,
	/// A call was never dispatched.
	Skipped,
	/// Core admission policy denied the call.
	PolicyDenied,
}

/// Structured durable evidence for a policy denial.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PolicyDenied {
	/// Human-readable explanation.
	pub reason:      Str,
	/// Stable machine-readable denial code, when one exists.
	pub code:        Option<Str>,
	/// Durable admission decision identifier.
	pub decision_id: Str,
	/// Stable identifiers of every policy rule that fired.
	pub rules:       SmallVec<Str, 4>,
}

/// Result of a post-settlement review that cannot rewrite the call outcome.
#[derive(
	Clone,
	Copy,
	Debug,
	Deserialize,
	Eq,
	Hash,
	PartialEq,
	Serialize,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum PostconditionStatus {
	/// Downstream review accepted the settled outcome.
	Passed,
	/// Downstream review found a durable problem after settlement.
	Rejected,
}

/// Durable finding attached beside, and never inside, a settled call outcome.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Postcondition {
	/// Review result.
	pub status:      PostconditionStatus,
	/// Human-readable finding.
	pub reason:      Str,
	/// Stable machine-readable finding code, when one exists.
	pub code:        Option<Str>,
	/// Durable decision identifier.
	pub decision_id: Str,
	/// Stable identifiers of policy rules supporting the finding.
	#[serde(default)]
	pub rules:       SmallVec<Str, 4>,
}

/// Retention promise for an artifact produced by detached work.
///
/// This is a lifetime hint for artifact storage, not ownership of an
/// environment resource. Producers may retain an artifact longer than promised.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Deserialize,
	Eq,
	PartialEq,
	Serialize,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ArtifactLifetime {
	/// Retain only long enough to consume the settlement.
	Ephemeral,
	/// Retain for the current agent session.
	#[default]
	Session,
	/// Retain independently of the current agent session.
	Durable,
}

/// Environment resource that authoritatively owns detached work.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobOwner {
	/// One generation of a named environment process.
	NamedProcess {
		/// Stable process name.
		name:       Str,
		/// Exact process generation observed when detaching.
		generation: u64,
	},
}

/// Detached work and its expected artifact.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct JobRef {
	/// Stable environment job identifier.
	pub id:       Str,
	/// Environment resource that authoritatively reports settlement.
	pub owner:    JobOwner,
	/// Artifact expected when the job settles.
	pub artifact: ExpectedArtifact,
}

/// Expected output of a detached job.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExpectedArtifact {
	/// Human-readable artifact role.
	pub description: Str,
	/// Expected MIME type, when known.
	pub media_type:  Option<Str>,
	/// Minimum retention promised by the artifact producer.
	pub lifetime:    ArtifactLifetime,
}

/// Borrowed durable call supplied to a pure revision lift.
#[derive(Clone, Copy, Debug)]
pub struct RecordedCall<'a> {
	/// Exact original model-emitted argument bytes.
	pub raw_args: &'a [u8],
	/// Exact structured verdict JSON bytes.
	pub verdict:  &'a [u8],
}

/// Owned result of one successful pure revision lift.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LiftedCall {
	/// Arguments expressed in the target revision.
	pub raw_args: Bytes,
	/// Verdict expressed in the target revision.
	pub verdict:  Bytes,
}

/// Owned historical call retained when projecting a transcript.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RecordedCallOwned {
	/// Durable tool identity at recording time.
	pub identity: ToolIdentity,
	/// Exact original arguments.
	pub raw_args: Bytes,
	/// Exact original structured verdict.
	pub verdict:  Bytes,
}

impl RecordedCallOwned {
	/// Borrows the byte-stable lift input.
	pub fn as_recorded(&self) -> RecordedCall<'_> {
		RecordedCall { raw_args: &self.raw_args, verdict: &self.verdict }
	}
}

/// Serialized call-outcome details before or after blob spill.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "storage", rename_all = "snake_case")]
pub enum CallOutcomeDetails {
	/// Small outcome retained inline as structured JSON bytes.
	Inline {
		/// Complete serialized call-outcome JSON bytes.
		json: Bytes,
	},
	/// Large outcome retained by content-addressed blob reference.
	Spilled {
		/// Durable blob reference.
		blob:     BlobRef,
		/// Original serialized byte length.
		byte_len: u64,
	},
}

/// Environment-provided hook for durable large-outcome storage.
pub trait CallOutcomeSpill: Send + Sync {
	/// Storage error.
	type Error;

	/// Stores exact JSON bytes and returns their durable blob reference.
	fn spill(&self, json: Bytes) -> impl Future<Output = Result<BlobRef, Self::Error>> + Send + '_;
}

/// Failure while serializing or spilling a structured call outcome.
#[derive(Debug, Error)]
pub enum CallOutcomeDetailsError<E> {
	/// Structured outcome serialization failed.
	#[error("call-outcome serialization failed: {0}")]
	Serialize(#[from] serde_json::Error),
	/// Blob storage failed.
	#[error("call-outcome spill failed")]
	Spill(E),
}

/// Serializes an outcome deterministically and spills it above `inline_limit`.
pub async fn call_outcome_details<P, F, S>(
	outcome: &CallOutcome<P, F>,
	inline_limit: usize,
	spill: &S,
) -> Result<CallOutcomeDetails, CallOutcomeDetailsError<S::Error>>
where
	P: Serialize + Sync,
	F: Serialize + Sync,
	S: CallOutcomeSpill,
{
	let json = Bytes::from(serde_json::to_vec(outcome)?);
	if json.len() <= inline_limit {
		return Ok(CallOutcomeDetails::Inline { json });
	}
	let byte_len = json.len() as u64;
	let blob = spill
		.spill(json)
		.await
		.map_err(CallOutcomeDetailsError::Spill)?;
	Ok(CallOutcomeDetails::Spilled { blob, byte_len })
}
