//! Typed, revisioned tool contracts for the agent/environment boundary.
//!
//! Execution is deliberately absent from this crate. A tool keeps concrete
//! parameter and result types until [`Registry::register`], while prompt
//! projection and revision lifting remain deterministic shared code.

mod incoming;
mod registry;
pub mod render;

use std::{collections::BTreeMap, fmt, future::Future, io::Write};

use bytes::Bytes;
use futures::Stream;
pub use incoming::{
	CommitError, IncomingParams, Interrupt, InterruptWaitError, InterruptibleParams,
	InvocationEvent, InvocationFeed, InvocationSendError, ParamError,
};
use omp_core::{InvocationPhase, SparseMap, Str};
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

/// Failure to parse a canonical `family.n` or bare `n` revision stamp.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("invalid tool revision: {value}")]
pub struct RevParseError {
	/// Rejected revision text.
	pub value: Str,
}

impl std::str::FromStr for Rev {
	type Err = RevParseError;

	fn from_str(value: &str) -> Result<Self, Self::Err> {
		let invalid = || RevParseError { value: Str::from(value) };
		let (family, number) = match value.split_once('.') {
			Some((family, number))
				if !family.is_empty() && !number.is_empty() && !number.contains('.') =>
			{
				(family, number)
			},
			Some(_) => return Err(invalid()),
			None => ("", value),
		};
		if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
			return Err(invalid());
		}
		let n = number.parse().map_err(|_| invalid())?;
		Ok(Self { family: Str::from(family), n })
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
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ArgPath {
	/// Object key.
	Key(Str),
	/// Array index.
	Index(u64),
}

/// Declared repair coercion applied after a value is pulled.
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
pub enum Coerce {
	/// Converts common string and numeric boolean spellings.
	LooseBool,
	/// Converts an integral string or integral real to an integer.
	Integer,
	/// Converts a numeric string to a real.
	Number,
	/// Converts a scalar JSON value to its string spelling.
	String,
	/// Wraps one non-array value in a one-element array.
	Singleton,
	/// Parses a string's contents as the target JSON shape.
	JsonString,
	/// Removes leading and trailing string whitespace.
	Strip,
	/// Splits a comma-delimited string into an array.
	Csv,
	/// Treats null-like optional values as an absent field.
	NullElision,
}

/// Immutable declaration for one canonical argument path.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ArgSpec {
	/// Canonical key/index path.
	pub path:     SmallVec<ArgPath, 4>,
	/// Additional accepted spellings of the final object key.
	pub aliases:  SmallVec<Str, 4>,
	/// Coercions applied in declaration order.
	pub coerce:   SmallVec<Coerce, 2>,
	/// Human-readable requested shape used by structured argument faults.
	pub expected: Str,
	/// Optional valid example borrowed into a structured argument fault.
	pub example:  Option<Str>,
}

#[derive(Default)]
struct RevArgSpecs {
	path_ids: BTreeMap<SmallVec<ArgPath, 4>, u32>,
	specs:    SparseMap<u32, ArgSpec>,
}

/// Per-revision argument declarations keyed by interned path identifiers.
///
/// Canonical paths and final-key aliases intern to the same dense identifier.
/// Once sealed, the table serves borrowed lock-free index lookups and rejects
/// every later mutation.
#[derive(Default)]
pub struct ArgSpecRegistry {
	revisions: BTreeMap<Rev, RevArgSpecs>,
	sealed:    bool,
}

/// Deterministic argument declaration registration failure.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ArgSpecRegistryError {
	/// A declaration was attempted after the registry was sealed.
	#[error("argument specification registry is sealed")]
	Sealed,
	/// A canonical path or one of its aliases was already declared.
	#[error("argument path already registered for revision {rev}: {path:?}")]
	Duplicate {
		/// Exact argument dialect revision.
		rev:  Rev,
		/// Conflicting canonical or alias path.
		path: SmallVec<ArgPath, 4>,
	},
	/// Aliases were declared for a path which does not end in an object key.
	#[error("argument aliases require a final object key for revision {rev}: {path:?}")]
	AliasOnIndex {
		/// Exact argument dialect revision.
		rev:  Rev,
		/// Invalid canonical path.
		path: SmallVec<ArgPath, 4>,
	},
	/// One revision exhausted the dense path identifier space.
	#[error("too many argument paths registered for revision {0}")]
	PathLimit(Rev),
}

impl ArgSpecRegistry {
	/// Creates an empty mutable declaration table.
	#[must_use]
	pub fn new() -> Self {
		Self::default()
	}

	/// Registers one canonical declaration and interns its alias paths.
	pub fn register(&mut self, rev: Rev, spec: ArgSpec) -> Result<(), ArgSpecRegistryError> {
		if self.sealed {
			return Err(ArgSpecRegistryError::Sealed);
		}
		let mut paths = SmallVec::<SmallVec<ArgPath, 4>, 5>::new();
		paths.push(spec.path.clone());
		if !spec.aliases.is_empty() {
			if !matches!(spec.path.last(), Some(ArgPath::Key(_))) {
				return Err(ArgSpecRegistryError::AliasOnIndex { rev, path: spec.path });
			}
			for alias in &spec.aliases {
				let mut path = spec.path.clone();
				let Some(ArgPath::Key(key)) = path.last_mut() else {
					unreachable!("final path segment was checked as a key")
				};
				*key = alias.clone();
				if paths.contains(&path) {
					return Err(ArgSpecRegistryError::Duplicate { rev, path });
				}
				paths.push(path);
			}
		}
		let revision = self.revisions.entry(rev.clone()).or_default();
		if let Some(path) = paths
			.iter()
			.find(|path| revision.path_ids.contains_key(path.as_slice()))
		{
			return Err(ArgSpecRegistryError::Duplicate { rev, path: (*path).clone() });
		}
		let path_id = u32::try_from(revision.specs.len())
			.map_err(|_| ArgSpecRegistryError::PathLimit(rev.clone()))?;
		for path in paths {
			let previous = revision.path_ids.insert(path, path_id);
			debug_assert!(previous.is_none(), "argument paths were checked before insertion");
		}
		let previous = revision.specs.insert(path_id, spec);
		debug_assert!(previous.is_none(), "path identifiers are dense and never reused");
		Ok(())
	}

	/// Seals the table against every later registration.
	pub fn seal(&mut self) {
		self.sealed = true;
	}

	/// Reports whether the declaration table is immutable.
	#[must_use]
	pub const fn is_sealed(&self) -> bool {
		self.sealed
	}

	/// Borrows the declaration for one exact revision and canonical or alias
	/// path.
	#[must_use]
	pub fn get(&self, rev: &Rev, path: &[ArgPath]) -> Option<&ArgSpec> {
		let revision = self.revisions.get(rev)?;
		revision.specs.get(*revision.path_ids.get(path)?)
	}
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

/// Environment-provided staged writer for durable large-outcome storage.
pub trait CallOutcomeSpill: Send + Sync {
	/// Storage error returned while opening or finalizing a stage.
	type Error;
	/// Environment-owned synchronous stage receiving exact JSON bytes.
	type Stage<'a>: Write + Send
	where
		Self: 'a;

	/// Opens one spill stage after serialization first exceeds the inline limit.
	fn open(&self) -> Result<Self::Stage<'_>, Self::Error>;

	/// Finalizes one completed stage and returns its durable blob reference.
	fn finish<'a>(
		&'a self,
		stage: Self::Stage<'a>,
	) -> impl Future<Output = Result<BlobRef, Self::Error>> + Send + 'a;
}

/// Failure while serializing or spilling a structured call outcome.
#[derive(Debug, Error)]
pub enum CallOutcomeDetailsError<E> {
	/// Structured outcome serialization failed before a spill writer failed.
	#[error("call-outcome serialization failed: {0}")]
	Serialize(serde_json::Error),
	/// The environment could not open a spill stage.
	#[error("call-outcome spill open failed")]
	SpillOpen(E),
	/// The environment-owned spill writer rejected serialized bytes.
	#[error("call-outcome spill write failed: {0}")]
	SpillWrite(serde_json::Error),
	/// The environment could not finalize the completed spill stage.
	#[error("call-outcome spill finalize failed")]
	SpillFinalize(E),
}

enum ThresholdState<W> {
	Inline(Vec<u8>),
	Spilled(W),
}

struct ThresholdWriter<'a, S: CallOutcomeSpill> {
	spill:              &'a S,
	inline_limit:       usize,
	state:              ThresholdState<S::Stage<'a>>,
	byte_len:           u64,
	open_error:         Option<S::Error>,
	spill_write_failed: bool,
}

impl<'a, S: CallOutcomeSpill> ThresholdWriter<'a, S> {
	fn new(spill: &'a S, inline_limit: usize) -> Self {
		Self {
			spill,
			inline_limit,
			state: ThresholdState::Inline(Vec::new()),
			byte_len: 0,
			open_error: None,
			spill_write_failed: false,
		}
	}
}

impl<S: CallOutcomeSpill> Write for ThresholdWriter<'_, S> {
	fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
		if self.open_error.is_some() || self.spill_write_failed {
			return Err(std::io::Error::other("call-outcome spill writer previously failed"));
		}
		if let ThresholdState::Inline(inline) = &mut self.state
			&& bytes.len() <= self.inline_limit.saturating_sub(inline.len())
		{
			inline.extend_from_slice(bytes);
			self.byte_len = self.byte_len.saturating_add(bytes.len() as u64);
			return Ok(bytes.len());
		}
		if matches!(self.state, ThresholdState::Inline(_)) {
			let stage = match self.spill.open() {
				Ok(stage) => stage,
				Err(error) => {
					self.open_error = Some(error);
					return Err(std::io::Error::other("call-outcome spill open failed"));
				},
			};
			let ThresholdState::Inline(inline) =
				std::mem::replace(&mut self.state, ThresholdState::Spilled(stage))
			else {
				unreachable!("inline spill transition changed state")
			};
			let ThresholdState::Spilled(opened) = &mut self.state else {
				unreachable!("spill transition did not retain its stage")
			};
			if let Err(error) = opened.write_all(&inline) {
				self.spill_write_failed = true;
				return Err(error);
			}
		}
		let ThresholdState::Spilled(stage) = &mut self.state else {
			unreachable!("threshold writer was neither inline nor spilled")
		};
		if let Err(error) = stage.write_all(bytes) {
			self.spill_write_failed = true;
			return Err(error);
		}
		self.byte_len = self.byte_len.saturating_add(bytes.len() as u64);
		Ok(bytes.len())
	}

	fn flush(&mut self) -> std::io::Result<()> {
		match &mut self.state {
			ThresholdState::Inline(_) => Ok(()),
			ThresholdState::Spilled(stage) => stage.flush(),
		}
	}
}

/// Serializes an outcome once and spills on the first byte above
/// `inline_limit`.
///
/// The inline buffer never grows beyond the limit. After overflow, buffered
/// bytes and every later serializer write go directly to one environment-owned
/// stage in their original order, and that stage is finalized exactly once.
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
	let mut writer = ThresholdWriter::new(spill, inline_limit);
	let serialized = outcome.serialize(&mut serde_json::Serializer::new(&mut writer));
	if let Err(source) = serialized {
		if let Some(error) = writer.open_error {
			return Err(CallOutcomeDetailsError::SpillOpen(error));
		}
		if writer.spill_write_failed {
			return Err(CallOutcomeDetailsError::SpillWrite(source));
		}
		return Err(CallOutcomeDetailsError::Serialize(source));
	}
	match writer.state {
		ThresholdState::Inline(json) => Ok(CallOutcomeDetails::Inline { json: Bytes::from(json) }),
		ThresholdState::Spilled(stage) => {
			let blob = spill
				.finish(stage)
				.await
				.map_err(CallOutcomeDetailsError::SpillFinalize)?;
			Ok(CallOutcomeDetails::Spilled { blob, byte_len: writer.byte_len })
		},
	}
}
