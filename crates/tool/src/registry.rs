//! One-time type erasure, live advertisement, and historical lift composition.

use std::{
	collections::BTreeMap,
	future::Future,
	mem::size_of,
	pin::Pin,
	sync::Arc,
	task::{Context, Poll},
};

use async_stream::stream;
use bytes::Bytes;
use futures::{Stream, StreamExt, pin_mut};
use omp_core::{SparseMap, Str, sf};
use omp_llm_catalog::GrammarBits;
use omp_llm_inference::{
	Adjustment, FeatureId, OpaqueJson, ReasonId, ToolDefinition, ToolGrammar, ToolGrammarSyntax,
	ToolInputConstraint,
};
use omp_proto::inference::v1::InvokeInput;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use smallvec::SmallVec;
use thiserror::Error;

use crate::{
	Abort, ArgIssueKind, ArgSpec, ArgSpecRegistry, ArgSpecRegistryError, CallOutcome, Constraint,
	DeviceIssue, DevicePath, GrammarSyntax, IncomingParams, LiftedCall, Part, Presentation,
	PromptCaps, RecordedCall, RecordedCallOwned, Rev, Tool, ToolIdentity,
	render::{Render, RenderEntry, RenderRegistry, RenderRegistryError, ViewState},
};

/// Catalog capabilities needed for deterministic tool lowering.
#[derive(Clone, Copy, Debug)]
pub struct LoweringCaps {
	/// Whether per-tool strict JSON Schema is supported.
	pub strict_schema:  bool,
	/// Supported freeform grammar languages.
	pub grammar:        GrammarBits,
	/// Maximum model-visible tool declarations, when the route declares one.
	pub maximum_tools:  Option<u16>,
	/// Maximum native strict JSON Schema declarations, when the route declares
	/// one.
	pub maximum_strict: Option<u16>,
}

/// Strength retained after capability-aware constraint lowering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConstraintDisposition {
	/// Route can honor the requested constraint.
	Required,
	/// Request remains a preference and is receipted when unavailable.
	Prefer,
}
/// Worker placement site used by a supervised device route.
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
pub enum WorkerSiteKind {
	/// The environment-local worker site.
	Env,
	/// The client-local worker site.
	Local,
	/// A pre-attached external worker site.
	Attached,
}

/// Execution route associated with a live registry entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolRoute {
	/// In-process typed Rust executor erased at registration.
	Native,
	/// Externally supervised worker executor and its resolved placement.
	Worker {
		/// Worker site kind.
		site: WorkerSiteKind,
		/// Named worker target at that site.
		name: Str,
	},
}

/// Declared priority for resolving competing claims on one tool name.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Precedence(pub i32);

impl Precedence {
	/// Harness-owned core tool precedence.
	pub const CORE: Self = Self(1_000);
	/// Ordinary extension precedence.
	pub const DEFAULT: Self = Self(0);
	/// Enhancement of an existing capability.
	pub const ENHANCEMENT: Self = Self(500);
	/// Deliberate last-resort implementation.
	pub const FALLBACK: Self = Self(-500);
	/// First-party or protocol integration precedence.
	pub const INTEGRATION: Self = Self(700);
}

/// Claim metadata supplied with one tool registration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Claims {
	/// Priority used to resolve this name.
	pub precedence: Precedence,
	/// Publisher-qualified implementation identity, such as `ff-labs/fff`.
	pub claimant:   Str,
	/// Name explicitly replaced by this claim, when replacement is intended.
	pub replaces:   Option<Str>,
}

/// Provenance retained for a non-winning claim.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShadowClaim {
	/// Current schema revision supplied by this claimant.
	pub rev:        Rev,
	/// Declared priority.
	pub precedence: Precedence,
	/// Publisher-qualified implementation identity.
	pub claimant:   Str,
	/// Explicit replacement target, when declared.
	pub replaces:   Option<Str>,
}

/// Policy-resolved claim for one stable tool name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Claim {
	/// Current schema revision of the winning claimant.
	pub rev:        Rev,
	/// Winning priority.
	pub precedence: Precedence,
	/// Publisher-qualified winning implementation.
	pub claimant:   Str,
	/// Explicit replacement target, when declared.
	pub replaces:   Option<Str>,
	/// Losing claims retained in deterministic precedence order.
	pub shadowed:   SmallVec<ShadowClaim, 1>,
}

/// Borrowed catalog view of one policy-resolved device.
#[derive(Clone, Copy, Debug)]
pub struct MountedDevice<'a> {
	/// Stable catalog name.
	pub name:     &'a Str,
	/// Current schema revision.
	pub rev:      &'a Rev,
	/// Publisher-qualified implementation identity.
	pub claimant: &'a Str,
	/// Short catalog summary.
	pub summary:  &'a Str,
	/// Complete JSON Schema bytes.
	pub schema:   &'a [u8],
	/// Maximum declared authority before per-invocation narrowing.
	pub effects:  &'a crate::Effects,
	/// Long-form documentation, when supplied by the declaration surface.
	pub docs:     Option<&'a str>,
	/// Execution placement, independent of device presentation.
	pub route:    &'a ToolRoute,
}
/// Resolved device dispatch target.
///
/// This borrows registry provenance while keeping a device's semantic
/// revision separate from its claimant-qualified tool-tree address.
#[derive(Clone, Copy, Debug)]
pub struct DeviceTarget<'a> {
	/// Stable root device token.
	pub name:     &'a Str,
	/// Semantic revision selected for this claimant.
	pub rev:      &'a Rev,
	/// Publisher-qualified implementation identity and worker extension key.
	pub claimant: &'a Str,
	/// Execution placement selected by the declaration.
	pub route:    &'a ToolRoute,
}

impl DeviceTarget<'_> {
	/// Returns the durable identity selected by this device address.
	#[must_use]
	pub fn identity(&self) -> ToolIdentity {
		ToolIdentity { name: self.name.clone(), rev: self.rev.clone() }
	}
}
/// One worker-reported availability transition.
///
/// The registry accepts only unmount transitions from this transport. A later
/// registration or explicit refresh may mount a declaration again; a stale
/// worker can never make an unavailable device reachable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AvailabilityDelta {
	/// Root device name whose live mount state changed.
	pub name:    Str,
	/// Reported reachability state.
	pub mounted: bool,
	/// Human-readable explanation for an unavailable device.
	pub reason:  Option<Str>,
}

/// One live tool declaration ready for inference request construction.
#[derive(Clone, Debug)]
pub struct LoweredTool {
	/// Durable live identity.
	pub identity:    ToolIdentity,
	/// Canonical inference declaration.
	pub definition:  ToolDefinition,
	/// Constraint strength after catalog-aware lowering, if requested.
	pub disposition: Option<ConstraintDisposition>,
	/// Original constraint priority, if requested.
	pub priority:    Option<u8>,
	/// Explicit degradation receipts; unsupported constraints are never silent.
	pub adjustments: Vec<Adjustment>,
}

/// Type-erased event emitted across the environment dispatch boundary.
#[derive(Clone, Debug)]
pub enum ErasedEv {
	/// Serialized typed update.
	Update(Bytes),
	/// Terminal serialized outcome.
	Done(ErasedOutcome),
}

/// Type-erased terminal tool outcome.
#[derive(Clone, Debug)]
pub enum ErasedOutcome {
	/// Structured journal verdict with compaction metadata.
	Done {
		/// Exact serialized [`CallOutcome`] JSON.
		verdict: Bytes,
		/// Whether projected parts may be compacted.
		useless: bool,
	},
	/// Detached work.
	Detached(crate::JobRef),
}

/// Cold dispatch stream allocated once for an erased invocation.
pub type ErasedStream<'a> =
	Pin<Box<dyn Stream<Item = Result<ErasedEv, RegistryError>> + Send + 'a>>;

/// Projection result for a durable historical call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectedCall {
	/// Call is expressed in the live revision and may be emitted as a tool item.
	Live(RecordedCallOwned),
	/// No complete lift path exists; preserve the original call as transcript
	/// data.
	Data(RecordedCallOwned),
}

/// Stable cache identity for one verdict projection.
///
/// The digest includes every input which may change model-facing parts:
/// verdict bytes, projection caps, semantic revision, and projection-code
/// identity.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ProjectionKey {
	/// Exact tool revision whose typed verdict decoder is selected.
	pub identity:        ToolIdentity,
	/// Digest of the model projection budget.
	pub caps_hash:       [u8; 32],
	/// Registry-wide identity of projection implementations.
	pub projection_hash: [u8; 32],
	cache_hash:          [u8; 32],
}

impl ProjectionKey {
	/// Creates the content-addressed key for one exact verdict and projection
	/// context.
	#[must_use]
	pub fn new(
		identity: &ToolIdentity,
		verdict: &[u8],
		caps: &PromptCaps,
		projection_hash: [u8; 32],
	) -> Self {
		let caps_hash = projection_caps_hash(caps);
		let mut hasher = blake3::Hasher::new();
		hash_field(&mut hasher, verdict);
		hash_field(&mut hasher, &caps.maximum_parts.to_le_bytes());
		hash_field(&mut hasher, &caps.maximum_text_bytes.to_le_bytes());
		hash_field(&mut hasher, &[u8::from(caps.media)]);
		hash_field(&mut hasher, &[caps.dialect as u8]);
		hash_field(&mut hasher, &[caps.model_class as u8]);
		hash_identity(&mut hasher, &identity.name, &identity.rev);
		hash_field(&mut hasher, &projection_hash);
		Self {
			identity: identity.clone(),
			caps_hash,
			projection_hash,
			cache_hash: *hasher.finalize().as_bytes(),
		}
	}

	/// Returns the opaque cache digest.
	#[must_use]
	pub const fn digest(&self) -> [u8; 32] {
		self.cache_hash
	}
}

/// One verdict projection which must be materialized during a turn's warm
/// pre-pass.
#[derive(Clone, Debug)]
pub struct ProjectionRequest<'a> {
	/// Cache identity and target tool revision.
	pub key:              ProjectionKey,
	/// Projection budget represented by [`ProjectionKey::caps_hash`].
	pub caps:             PromptCaps,
	/// Exact canonical structured verdict bytes.
	pub verdict:          &'a [u8],
	/// Durable compaction hint recorded with this call.
	pub recorded_useless: bool,
}

/// Authoritative model projection and branch metadata decoded from one verdict.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectedVerdict {
	/// Model-facing parts under the supplied current-model capabilities.
	///
	/// Shared ownership avoids copying immutable parts into each projected
	/// thread item.
	pub parts:    Arc<[Part]>,
	/// Whether the decoded verdict branch is a fault, argument error, or abort.
	pub is_error: bool,
	/// Durable compaction hint, forced false for argument errors and aborts.
	pub useless:  bool,
}

struct ProjectionCache {
	inner: Mutex<ProjectionCacheInner>,
}

struct ProjectionCacheInner {
	by_device: SparseMap<u32, ProjectionLru>,
	bytes:     usize,
	clock:     u64,
}

struct ProjectionLru {
	entries: SmallVec<ProjectionCacheEntry, 4>,
}

struct ProjectionCacheEntry {
	hash:  [u8; 32],
	value: Arc<ProjectedVerdict>,
	bytes: usize,
	used:  u64,
}

impl Default for ProjectionCache {
	fn default() -> Self {
		Self {
			inner: Mutex::new(ProjectionCacheInner {
				by_device: SparseMap::new(),
				bytes:     0,
				clock:     0,
			}),
		}
	}
}

impl ProjectionCache {
	const MAX_PART_BYTES: usize = 4 * 1024 * 1024;

	fn get(&self, device_id: u32, key: &ProjectionKey) -> Option<Arc<ProjectedVerdict>> {
		let mut inner = self.inner.lock();
		inner.clock = inner.clock.wrapping_add(1);
		let used = inner.clock;
		let entry = inner
			.by_device
			.get_mut(device_id)?
			.entries
			.iter_mut()
			.find(|entry| entry.hash == key.cache_hash)?;
		entry.used = used;
		Some(Arc::clone(&entry.value))
	}

	fn insert(&self, device_id: u32, key: &ProjectionKey, value: ProjectedVerdict) {
		let bytes = projected_part_bytes(&value.parts);
		if bytes > Self::MAX_PART_BYTES {
			return;
		}
		let value = Arc::new(value);
		let mut inner = self.inner.lock();
		inner.clock = inner.clock.wrapping_add(1);
		let used = inner.clock;
		let previous_bytes = {
			let lru = inner
				.by_device
				.get_or_insert_with(device_id, || ProjectionLru { entries: SmallVec::new() });
			if let Some(entry) = lru
				.entries
				.iter_mut()
				.find(|entry| entry.hash == key.cache_hash)
			{
				let previous = entry.bytes;
				*entry = ProjectionCacheEntry { hash: key.cache_hash, value, bytes, used };
				Some(previous)
			} else {
				lru.entries
					.push(ProjectionCacheEntry { hash: key.cache_hash, value, bytes, used });
				None
			}
		};
		let current_bytes = inner.bytes;
		inner.bytes = previous_bytes.map_or_else(
			|| current_bytes.saturating_add(bytes),
			|previous| current_bytes.saturating_sub(previous).saturating_add(bytes),
		);
		while inner.bytes > Self::MAX_PART_BYTES {
			let victim = inner
				.by_device
				.iter()
				.flat_map(|(device_id, lru)| {
					lru.entries
						.iter()
						.enumerate()
						.map(move |(index, entry)| (device_id, index, entry.used))
				})
				.min_by_key(|(_, _, used)| *used);
			let Some((device_id, index, _)) = victim else {
				break;
			};
			let removed = inner
				.by_device
				.get_mut(device_id)
				.expect("selected projection-cache device remains present")
				.entries
				.remove(index);
			inner.bytes = inner.bytes.saturating_sub(removed.bytes);
		}
	}
}

struct ProjectionWarm {
	result: Option<Result<(), RegistryError>>,
}

impl ProjectionWarm {
	const fn ready(result: Result<(), RegistryError>) -> Self {
		Self { result: Some(result) }
	}

	fn into_ready(mut self) -> Result<(), RegistryError> {
		self
			.result
			.take()
			.expect("projection warm future is consumed once")
	}
}

impl Future for ProjectionWarm {
	type Output = Result<(), RegistryError>;

	fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
		Poll::Ready(
			self
				.result
				.take()
				.expect("projection warm future polled after completion"),
		)
	}
}

/// Registry construction, dispatch, serialization, or projection failure.
#[derive(Debug, Error)]
pub enum RegistryError {
	/// `(name, revision)` was registered twice.
	#[error("tool revision already registered: {0}@{1}")]
	Duplicate(Str, Rev),
	/// Registered tool revisions exhausted the dense projection-cache id
	/// space.
	#[error("too many registered tool revisions for the projection cache")]
	ProjectionCacheIdLimit,
	/// A synchronous caller requested a projection which failed to warm its
	/// cache entry.
	#[error("projection cache remained cold for {0:?}")]
	ProjectionCacheMiss(ToolIdentity),
	/// Tool name is not registered.
	#[error("unknown tool: {0}")]
	UnknownTool(Str),
	/// Two distinct claimants declared the same precedence for one name.
	#[error("tool precedence tie for {name}: {first} and {second}")]
	PrecedenceTie {
		/// Contested tool name.
		name:   Str,
		/// Lexicographically first claimant.
		first:  Str,
		/// Lexicographically second claimant.
		second: Str,
	},
	/// A declaration attempted to occupy or outrank a reserved core name.
	#[error("claimant {claimant} cannot claim reserved core precedence for {name}: {precedence:?}")]
	CoreNameClaim {
		/// Contested tool name.
		name:       Str,
		/// Rejected device claimant.
		claimant:   Str,
		/// Rejected precedence value.
		precedence: Precedence,
	},
	/// Operation requires a native pure or execution surface unavailable for a
	/// worker declaration.
	#[error("tool {name}@{rev} is worker-routed and cannot perform registry operation {operation}")]
	UnsupportedExternal {
		/// Tool name.
		name:      Str,
		/// Exact registered revision.
		rev:       Rev,
		/// Requested registry operation.
		operation: &'static str,
	},
	/// Registered schema is not one complete JSON value.
	#[error("invalid JSON Schema for {name}@{rev}: {source}")]
	InvalidSchema {
		/// Tool name.
		name:   Str,
		/// Tool revision.
		rev:    Rev,
		/// Parser failure.
		source: serde_json::Error,
	},
	/// Typed event or verdict serialization failed.
	#[error("tool value serialization failed: {0}")]
	Serialize(#[from] serde_json::Error),
	/// Stored verdict does not match its registered typed revision.
	#[error("stored verdict does not match registered tool revision: {0}")]
	VerdictShape(Str),
	/// Serialized update does not match its registered typed revision.
	#[error("tool update does not match registered revision {name}@{rev}: {source}")]
	UpdateShape {
		/// Tool name.
		name:   Str,
		/// Exact registered revision.
		rev:    Rev,
		/// Typed update decoder failure.
		source: serde_json::Error,
	},
	/// Selected route cannot honor a constraint whose fallback is `ERROR`.
	#[error("tool {name}@{rev} requires unsupported constraint: {feature}")]
	UnsupportedConstraint {
		/// Tool name.
		name:    Str,
		/// Exact registered revision.
		rev:     Rev,
		/// Unsupported constraint feature.
		feature: &'static str,
	},
}

trait ErasedTool: Send + Sync {
	fn spec(&self) -> &crate::ToolSpec;
	fn route(&self) -> &ToolRoute;
	fn schema(&self) -> &OpaqueJson;
	fn call<'a>(&'a self, params: IncomingParams<'a>) -> ErasedStream<'a>;
	fn project_cached(&self, key: &ProjectionKey) -> Option<Arc<ProjectedVerdict>>;
	fn cache_projected(&self, key: &ProjectionKey, projected: ProjectedVerdict);
	fn warm(&self, requests: &[ProjectionRequest<'_>]) -> ProjectionWarm;
	fn invoke_input(
		&self,
		invocation_id: &str,
		json: &[u8],
	) -> Result<Option<InvokeInput>, RegistryError>;
	fn lift(&self, from: &Rev, call: RecordedCall<'_>) -> Option<LiftedCall>;
}
static NATIVE_TOOL_ROUTE: ToolRoute = ToolRoute::Native;

struct Worker {
	spec:     crate::ToolSpec,
	schema:   OpaqueJson,
	route:    ToolRoute,
	cache:    Arc<ProjectionCache>,
	cache_id: u32,
}

impl ErasedTool for Worker {
	fn spec(&self) -> &crate::ToolSpec {
		&self.spec
	}

	fn route(&self) -> &ToolRoute {
		&self.route
	}

	fn schema(&self) -> &OpaqueJson {
		&self.schema
	}

	fn call<'a>(&'a self, _params: IncomingParams<'a>) -> ErasedStream<'a> {
		let error = external_error(&self.spec, "invoke");
		Box::pin(futures::stream::once(async move { Err(error) }))
	}

	fn project_cached(&self, key: &ProjectionKey) -> Option<Arc<ProjectedVerdict>> {
		self.cache.get(self.cache_id, key)
	}

	fn cache_projected(&self, key: &ProjectionKey, projected: ProjectedVerdict) {
		self.cache.insert(self.cache_id, key, projected);
	}

	fn warm(&self, _requests: &[ProjectionRequest<'_>]) -> ProjectionWarm {
		ProjectionWarm::ready(Err(external_error(&self.spec, "warm")))
	}

	fn invoke_input(
		&self,
		_invocation_id: &str,
		_json: &[u8],
	) -> Result<Option<InvokeInput>, RegistryError> {
		Err(external_error(&self.spec, "invoke_input"))
	}

	fn lift(&self, _from: &Rev, _call: RecordedCall<'_>) -> Option<LiftedCall> {
		None
	}
}

struct Registered<T> {
	tool:     T,
	schema:   OpaqueJson,
	cache:    Arc<ProjectionCache>,
	cache_id: u32,
}

impl<T: Tool> Registered<T> {
	fn project_fresh(
		&self,
		verdict: &[u8],
		recorded_useless: bool,
		caps: PromptCaps,
	) -> Result<ProjectedVerdict, RegistryError> {
		let verdict: CallOutcome<T::Payload, T::Fault> = serde_json::from_slice(verdict)
			.map_err(|_| RegistryError::VerdictShape(self.tool.spec().name.clone()))?;
		Ok(match &verdict {
			CallOutcome::Ok(payload) => ProjectedVerdict {
				parts:    self.tool.prompt(Ok(payload), &caps).into(),
				is_error: false,
				useless:  recorded_useless,
			},
			CallOutcome::Faulted(fault) => ProjectedVerdict {
				parts:    self.tool.prompt(Err(fault), &caps).into(),
				is_error: true,
				useless:  recorded_useless,
			},
			CallOutcome::ArgsRejected(issue) => ProjectedVerdict {
				parts:    vec![Part::Text { text: render_arg_issue(issue) }].into(),
				is_error: true,
				useless:  false,
			},
			CallOutcome::Aborted { abort, .. } => ProjectedVerdict {
				parts:    vec![Part::Text { text: render_abort(abort) }].into(),
				is_error: true,
				useless:  false,
			},
		})
	}
}

impl<T: Tool> ErasedTool for Registered<T> {
	fn spec(&self) -> &crate::ToolSpec {
		self.tool.spec()
	}

	fn route(&self) -> &ToolRoute {
		&NATIVE_TOOL_ROUTE
	}

	fn schema(&self) -> &OpaqueJson {
		&self.schema
	}

	fn call<'a>(&'a self, params: IncomingParams<'a>) -> ErasedStream<'a> {
		let events = self.tool.call(params);
		Box::pin(stream! {
			pin_mut!(events);
			let mut terminal = false;
			while let Some(event) = events.next().await {
				match event {
					crate::Ev::Update(update) => match serde_json::to_vec(&update) {
						Ok(json) => yield Ok(ErasedEv::Update(Bytes::from(json))),
						Err(error) => {
							terminal = true;
							yield Err(RegistryError::Serialize(error));
							break;
						},
					},
					crate::Ev::Args(issue) => {
						terminal = true;
						let verdict = CallOutcome::<T::Payload, T::Fault>::ArgsRejected(issue);
						match serde_json::to_vec(&verdict) {
							Ok(json) => yield Ok(ErasedEv::Done(ErasedOutcome::Done {
								verdict: Bytes::from(json),
								useless: false,
							})),
							Err(error) => yield Err(RegistryError::Serialize(error)),
						}
						break;
					},
					crate::Ev::Aborted(abort) => {
						terminal = true;
						let verdict = CallOutcome::<T::Payload, T::Fault>::aborted(abort);
						match serde_json::to_vec(&verdict) {
							Ok(json) => yield Ok(ErasedEv::Done(ErasedOutcome::Done {
								verdict: Bytes::from(json),
								useless: false,
							})),
							Err(error) => yield Err(RegistryError::Serialize(error)),
						}
						break;
					},
					crate::Ev::Done(outcome) => {
						terminal = true;
						let erased = match outcome {
							crate::ToolTerminal::Done { result, useless } => {
								let verdict = match result {
									Ok(payload) => CallOutcome::<T::Payload, T::Fault>::Ok(payload),
									Err(fault) => CallOutcome::<T::Payload, T::Fault>::Faulted(fault),
								};
								match serde_json::to_vec(&verdict) {
									Ok(json) => ErasedOutcome::Done {
										verdict: Bytes::from(json),
										useless,
									},
									Err(error) => {
										yield Err(RegistryError::Serialize(error));
										break;
									},
								}
							},
							crate::ToolTerminal::Detached(job) => ErasedOutcome::Detached(job),
						};
						yield Ok(ErasedEv::Done(erased));
						break;
					},
				}
			}
			if !terminal {
				let verdict = CallOutcome::<Value, Value>::aborted(Abort::MissingOutcome);
				match serde_json::to_vec(&verdict) {
					Ok(json) => yield Ok(ErasedEv::Done(ErasedOutcome::Done {
						verdict: Bytes::from(json),
						useless: false,
					})),
					Err(error) => yield Err(RegistryError::Serialize(error)),
				}
			}
		})
	}

	fn project_cached(&self, key: &ProjectionKey) -> Option<Arc<ProjectedVerdict>> {
		self.cache.get(self.cache_id, key)
	}

	fn cache_projected(&self, key: &ProjectionKey, projected: ProjectedVerdict) {
		self.cache.insert(self.cache_id, key, projected);
	}

	fn warm(&self, requests: &[ProjectionRequest<'_>]) -> ProjectionWarm {
		let identity = self.tool.spec().identity();
		let result = requests
			.iter()
			.filter(|request| request.key.identity == identity)
			.filter(|request| self.cache.get(self.cache_id, &request.key).is_none())
			.try_for_each(|request| {
				let value =
					self.project_fresh(request.verdict, request.recorded_useless, request.caps)?;
				self.cache.insert(self.cache_id, &request.key, value);
				Ok(())
			});
		ProjectionWarm::ready(result)
	}

	fn invoke_input(
		&self,
		invocation_id: &str,
		json: &[u8],
	) -> Result<Option<InvokeInput>, RegistryError> {
		let update: T::Update =
			serde_json::from_slice(json).map_err(|source| RegistryError::UpdateShape {
				name: self.tool.spec().name.clone(),
				rev: self.tool.spec().rev.clone(),
				source,
			})?;
		Ok(self.tool.invoke_input(&update, invocation_id))
	}

	fn lift(&self, from: &Rev, call: RecordedCall<'_>) -> Option<LiftedCall> {
		self.tool.lift(from, call)
	}
}

struct RegistryEntry {
	tool:         Arc<dyn ErasedTool>,
	presentation: Presentation,
	claims:       Claims,
}

/// Revision-aware tool registry.
///
/// Concrete associated types are erased exactly once by
/// [`register`](Self::register). Every revision remains available only for pure
/// projection/lift code; dispatch and advertisement always select the one live
/// revision per stable name.
#[derive(Default)]
pub struct Registry {
	versions:         BTreeMap<Str, BTreeMap<Rev, RegistryEntry>>,
	live:             BTreeMap<Str, Claim>,
	unmounted:        RwLock<BTreeMap<Str, Option<Str>>>,
	arg_specs:        ArgSpecRegistry,
	renderers:        RenderRegistry,
	projection_cache: Arc<ProjectionCache>,
}

impl Registry {
	/// Creates an empty registry.
	pub fn new() -> Self {
		Self::default()
	}

	/// Registers one argument declaration for one exact revision.
	pub fn register_arg_spec(
		&mut self,
		rev: Rev,
		spec: ArgSpec,
	) -> Result<(), ArgSpecRegistryError> {
		self.arg_specs.register(rev, spec)
	}

	/// Seals argument declarations against every later mutation.
	pub const fn seal_arg_specs(&mut self) {
		self.arg_specs.seal();
	}

	/// Borrows one exact-revision argument declaration by canonical or alias
	/// path.
	#[must_use]
	pub fn arg_spec(&self, rev: &Rev, path: &[crate::ArgPath]) -> Option<&ArgSpec> {
		self.arg_specs.get(rev, path)
	}

	/// Registers one pure renderer for one exact tool identity.
	pub fn register_renderer<R: Render>(
		&mut self,
		identity: ToolIdentity,
		renderer: R,
	) -> Result<(), RenderRegistryError> {
		self.renderers.register(identity, renderer)
	}

	/// Borrows the exact-revision renderer registry.
	#[must_use]
	pub const fn render_registry(&self) -> &RenderRegistry {
		&self.renderers
	}

	/// Mutably borrows renderer registration storage during composition.
	///
	/// Renderer entries remain excluded from every execution and prompt hash.
	pub const fn render_registry_mut(&mut self) -> &mut RenderRegistry {
		&mut self.renderers
	}

	/// Borrows a cached renderer for one exact identity.
	#[must_use]
	pub fn renderer(&self, identity: &ToolIdentity) -> Option<RenderEntry<'_>> {
		self.renderers.get(identity)
	}

	/// Folds one serialized update through an exact-revision renderer.
	pub fn fold_render(
		&self,
		identity: &ToolIdentity,
		state: &mut ViewState,
		update: Bytes,
	) -> Result<(), RenderRegistryError> {
		self.renderers.fold(identity, state, update)
	}

	/// Renders an exact-revision fold or the generic built-in fallback.
	pub fn render_view(
		&self,
		identity: &ToolIdentity,
		state: &ViewState,
		outcome: Option<&[u8]>,
	) -> Result<Str, RenderRegistryError> {
		self.renderers.view(identity, state, outcome)
	}

	fn next_projection_cache_id(&self) -> Result<u32, RegistryError> {
		let count = self.versions.values().map(BTreeMap::len).sum::<usize>();
		u32::try_from(count).map_err(|_| RegistryError::ProjectionCacheIdLimit)
	}

	/// Registers a typed tool under one presentation and claimant.
	///
	/// Older revisions from the same claimant remain only as pure lift steps.
	/// Competing lower-precedence claimants remain qualified-addressable.
	pub fn register<T: Tool>(
		&mut self,
		tool: T,
		presentation: Presentation,
		claims: Claims,
	) -> Result<(), RegistryError> {
		let spec = tool.spec();
		let name = spec.name.clone();
		let rev = spec.rev.clone();
		let value = serde_json::from_slice(&spec.schema).map_err(|source| {
			RegistryError::InvalidSchema { name: name.clone(), rev: rev.clone(), source }
		})?;
		let cache_id = self.next_projection_cache_id()?;
		let entry = RegistryEntry {
			tool: Arc::new(Registered {
				tool,
				schema: OpaqueJson::new(value),
				cache: Arc::clone(&self.projection_cache),
				cache_id,
			}),
			presentation,
			claims,
		};
		self.insert(name, rev, entry)
	}

	/// Registers an externally supervised declaration under the default
	/// environment worker whose name matches the device token.
	pub fn register_worker(
		&mut self,
		spec: crate::ToolSpec,
		presentation: Presentation,
		claims: Claims,
	) -> Result<(), RegistryError> {
		let worker_name = spec.name.clone();
		self.register_worker_at(spec, presentation, claims, WorkerSiteKind::Env, worker_name)
	}

	/// Registers an externally supervised declaration with its resolved worker
	/// placement.
	pub fn register_worker_at(
		&mut self,
		spec: crate::ToolSpec,
		presentation: Presentation,
		claims: Claims,
		site: WorkerSiteKind,
		worker_name: Str,
	) -> Result<(), RegistryError> {
		let name = spec.name.clone();
		let rev = spec.rev.clone();
		let value = serde_json::from_slice(&spec.schema).map_err(|source| {
			RegistryError::InvalidSchema { name: name.clone(), rev: rev.clone(), source }
		})?;
		let cache_id = self.next_projection_cache_id()?;
		let entry = RegistryEntry {
			tool: Arc::new(Worker {
				spec,
				schema: OpaqueJson::new(value),
				route: ToolRoute::Worker { site, name: worker_name },
				cache: Arc::clone(&self.projection_cache),
				cache_id,
			}),
			presentation,
			claims,
		};
		self.insert(name, rev, entry)
	}

	fn insert(&mut self, name: Str, rev: Rev, entry: RegistryEntry) -> Result<(), RegistryError> {
		if entry.claims.precedence > Precedence::CORE
			|| (entry.presentation == Presentation::Device
				&& entry.claims.precedence >= Precedence::CORE)
		{
			return Err(RegistryError::CoreNameClaim {
				name,
				claimant: entry.claims.claimant,
				precedence: entry.claims.precedence,
			});
		}
		if self
			.versions
			.get(&name)
			.is_some_and(|versions| versions.contains_key(&rev))
		{
			return Err(RegistryError::Duplicate(name, rev));
		}
		let claim = resolve_claim(&name, self.live.get(&name), rev.clone(), &entry.claims)?;
		self
			.versions
			.entry(name.clone())
			.or_default()
			.insert(rev, entry);
		self.live.insert(name, claim);
		Ok(())
	}

	/// Borrows the exact policy-resolved `(name, revision)` identity.
	///
	/// A claimant-qualified name resolves a shadow without promoting it into
	/// catalog iteration.
	#[must_use]
	pub fn live_identity(&self, name: &str) -> Option<(&Str, &Rev)> {
		let (name, claimant) = split_claimant(name);
		let (stored_name, claim) = self.live.get_key_value(name)?;
		Some((stored_name, claim_revision(claim, claimant)?))
	}

	/// Borrows the complete policy-resolved specification.
	///
	/// Claimant-qualified names resolve their shadow without promoting it.
	pub fn live_spec(&self, name: &str) -> Result<&crate::ToolSpec, RegistryError> {
		Ok(self.live_entry(name)?.tool.spec())
	}

	/// Borrows the declared maximum effect envelope of a resolved tool.
	pub fn effects(&self, name: &str) -> Result<&crate::Effects, RegistryError> {
		Ok(&self.live_spec(name)?.effects)
	}

	/// Iterates winning identities in deterministic name order.
	pub fn live_identities(
		&self,
	) -> impl DoubleEndedIterator<Item = (&Str, &Rev)> + ExactSizeIterator + '_ {
		self.live.iter().map(|(name, claim)| (name, &claim.rev))
	}

	/// Borrows the resolved claim and its shadow provenance.
	#[must_use]
	pub fn claim(&self, name: &str) -> Option<&Claim> {
		self.live.get(name)
	}

	/// Returns the execution route of a winning or claimant-qualified entry.
	pub fn route(&self, name: &str) -> Result<ToolRoute, RegistryError> {
		Ok(self.live_entry(name)?.tool.route().clone())
	}

	/// Returns the presentation of a winning or claimant-qualified entry.
	pub fn presentation(&self, name: &str) -> Result<Presentation, RegistryError> {
		Ok(self.live_entry(name)?.presentation)
	}

	/// Resolves a typed device path without admitting it to the model slot
	/// catalog.
	///
	/// The optional sub-tool component remains owned by the `dyn` router; the
	/// registry resolves the root claim and its live semantic revision.
	pub fn resolve_device(&self, path: &DevicePath) -> Result<DeviceTarget<'_>, DeviceIssue> {
		if self.unmounted.read().contains_key(path.root()) {
			return Err(device_issue(path));
		}
		let Some((name, claim)) = self.live.get_key_value(path.root()) else {
			return Err(device_issue(path));
		};
		let Some(selected) = claim_entries(claim).find(|candidate| {
			path
				.claimant
				.as_ref()
				.is_none_or(|claimant| candidate.claimant == claimant)
		}) else {
			return Err(device_issue(path));
		};
		let Some(entry) = self
			.versions
			.get(name)
			.and_then(|versions| versions.get(selected.rev))
		else {
			return Err(device_issue(path));
		};
		if entry.presentation != Presentation::Device {
			return Err(device_issue(path));
		}
		Ok(DeviceTarget {
			name,
			rev: selected.rev,
			claimant: selected.claimant,
			route: entry.tool.route(),
		})
	}

	/// Conservatively applies worker availability reports and returns the
	/// transitions that actually unmounted live devices.
	///
	/// `mounted=true` is deliberately ignored: only a fresh registry
	/// composition may make a device reachable after a worker report.
	pub fn apply_availability(
		&self,
		deltas: &[AvailabilityDelta],
	) -> SmallVec<AvailabilityDelta, 2> {
		let mut applied = SmallVec::new();
		let mut unmounted = self.unmounted.write();
		for delta in deltas {
			if delta.mounted
				|| unmounted.contains_key(&delta.name)
				|| !self.live.get(&delta.name).is_some_and(|claim| {
					self
						.versions
						.get(&delta.name)
						.and_then(|versions| versions.get(&claim.rev))
						.is_some_and(|entry| entry.presentation == Presentation::Device)
				}) {
				continue;
			}
			unmounted.insert(delta.name.clone(), delta.reason.clone());
			applied.push(delta.clone());
		}
		applied
	}

	/// Iterates mounted catalog devices without allocating.
	///
	/// Shadowed and conservatively unmounted devices are intentionally absent.
	pub fn devices(&self) -> impl DoubleEndedIterator<Item = MountedDevice<'_>> + '_ {
		self.live.iter().filter_map(|(name, claim)| {
			let entry = self.versions.get(name)?.get(&claim.rev)?;
			(!self.unmounted.read().contains_key(name) && entry.presentation == Presentation::Device)
				.then(|| MountedDevice {
					name,
					rev: &claim.rev,
					claimant: &claim.claimant,
					summary: &entry.tool.spec().description,
					schema: entry.tool.spec().schema.as_ref(),
					effects: &entry.tool.spec().effects,
					docs: None,
					route: entry.tool.route(),
				})
		})
	}

	/// Hashes only policy-resolved model-visible slots.
	#[must_use]
	pub fn slot_hash(&self) -> [u8; 32] {
		let mut hasher = blake3::Hasher::new();
		hasher.update(b"omp-tool/slots/v1\0");
		for (name, claim) in &self.live {
			let Some(entry) = self
				.versions
				.get(name)
				.and_then(|versions| versions.get(&claim.rev))
			else {
				continue;
			};
			if entry.presentation == Presentation::Slot {
				hash_identity(&mut hasher, name, &claim.rev);
			}
		}
		*hasher.finalize().as_bytes()
	}

	/// Hashes mounted device availability and claimant-qualified reachability.
	#[must_use]
	pub fn device_hash(&self) -> [u8; 32] {
		let mut hasher = blake3::Hasher::new();
		hasher.update(b"omp-tool/devices/v1\0");
		let unmounted = self.unmounted.read();
		for (name, claim) in &self.live {
			if unmounted.contains_key(name) {
				continue;
			}
			for shadow in claim_entries(claim) {
				let Some(entry) = self
					.versions
					.get(name)
					.and_then(|versions| versions.get(shadow.rev))
				else {
					continue;
				};
				if entry.presentation != Presentation::Device {
					continue;
				}
				hash_identity(&mut hasher, name, shadow.rev);
				hash_field(&mut hasher, shadow.claimant.as_bytes());
				hash_field(
					&mut hasher,
					shadow
						.replaces
						.map_or(&[][..], |replacement| replacement.as_bytes()),
				);
				hasher.update(&shadow.precedence.0.to_le_bytes());
				hash_tool_route(&mut hasher, entry.tool.route());
			}
		}
		*hasher.finalize().as_bytes()
	}

	/// Hashes every registered revision with its projection implementation.
	#[must_use]
	pub fn projection_hash(&self) -> [u8; 32] {
		let mut hasher = blake3::Hasher::new();
		hasher.update(b"omp-tool/projections/v1\0");
		for (name, versions) in &self.versions {
			for (rev, entry) in versions {
				hash_identity(&mut hasher, name, rev);
				hash_field(&mut hasher, &entry.tool.spec().projection_code);
			}
		}
		*hasher.finalize().as_bytes()
	}

	/// Dispatches only the policy-resolved or claimant-qualified revision.
	pub fn invoke<'a>(
		&'a self,
		name: &str,
		mut params: IncomingParams<'a>,
	) -> Result<ErasedStream<'a>, RegistryError> {
		let entry = self.live_entry(name)?;
		if matches!(entry.tool.route(), ToolRoute::Worker { .. }) {
			return Err(external_error(entry.tool.spec(), "invoke"));
		}
		params.bind_arg_specs(&entry.tool.spec().rev, &self.arg_specs);
		Ok(entry.tool.call(params))
	}

	/// Dispatches one resolved native device while preserving the normal slot
	/// invocation path unchanged.
	///
	/// Worker-routed devices are intentionally rejected here: the environment
	/// router owns their `InvokeTool` transport after inspecting
	/// [`DeviceTarget::route`] from [`Self::resolve_device`].
	pub fn invoke_device<'a>(
		&'a self,
		path: &DevicePath,
		mut params: IncomingParams<'a>,
	) -> Result<ErasedStream<'a>, RegistryError> {
		let target = self
			.resolve_device(path)
			.map_err(|_| RegistryError::UnknownTool(Str::new(path.to_string())))?;
		let entry = self
			.versions
			.get(target.name)
			.and_then(|versions| versions.get(target.rev))
			.expect("resolved device target must retain its registered entry");
		if matches!(entry.tool.route(), ToolRoute::Worker { .. }) {
			return Err(external_error(entry.tool.spec(), "invoke_device"));
		}
		params.bind_arg_specs(&entry.tool.spec().rev, &self.arg_specs);
		Ok(entry.tool.call(params))
	}

	/// Lowers only policy-resolved native model-visible slots in priority order.
	///
	/// Larger priorities win. Core slots occupy the upper priority band, so an
	/// extension declaration can never displace a core intent when a route is
	/// capacity-constrained.
	pub fn advertise(&self, caps: LoweringCaps) -> Result<Vec<LoweredTool>, RegistryError> {
		let mut entries = self
			.live
			.iter()
			.filter_map(|(name, claim)| {
				let entry = self.versions.get(name)?.get(&claim.rev)?;
				(entry.presentation == Presentation::Slot
					&& matches!(entry.tool.route(), ToolRoute::Native))
				.then_some(entry)
			})
			.collect::<Vec<_>>();
		entries.sort_by(|left, right| {
			advertisement_priority(right)
				.cmp(&advertisement_priority(left))
				.then_with(|| left.tool.spec().name.cmp(&right.tool.spec().name))
		});

		let mut lowered = Vec::with_capacity(entries.len());
		let mut strict = 0_usize;
		for entry in entries {
			if caps
				.maximum_tools
				.is_some_and(|limit| lowered.len() >= limit as usize)
			{
				if constraint_requires_capacity(entry.tool.spec()) {
					return Err(budget_constraint_error(entry.tool.spec(), "tool-count-budget"));
				}
				continue;
			}
			let mut tool = lower(entry.tool.as_ref(), caps)?;
			tool.priority = constraint_priority(&entry.tool.spec().constraint)
				.map(|_| advertisement_priority(entry));
			if is_native_strict(&tool)
				&& caps
					.maximum_strict
					.is_some_and(|limit| strict >= limit as usize)
			{
				if constraint_requires_capacity(entry.tool.spec()) {
					return Err(budget_constraint_error(entry.tool.spec(), "strict-schema-budget"));
				}
				downgrade_strict(&mut tool);
			}
			if is_native_strict(&tool) {
				strict = strict.saturating_add(1);
			}
			lowered.push(tool);
		}
		Ok(lowered)
	}

	/// Deterministically projects a structured live verdict through its tool.
	pub fn prompt(
		&self,
		identity: &ToolIdentity,
		verdict: &[u8],
		caps: &PromptCaps,
	) -> Result<Option<Arc<[Part]>>, RegistryError> {
		let projected = self.project_verdict(identity, verdict, false, caps)?;
		Ok(Some(Arc::clone(&projected.parts)))
	}

	/// Builds one cache-addressed projection request for a complete turn
	/// pre-pass.
	pub fn projection_request<'a>(
		&self,
		identity: &ToolIdentity,
		verdict: &'a [u8],
		recorded_useless: bool,
		caps: &PromptCaps,
	) -> Result<ProjectionRequest<'a>, RegistryError> {
		self.projection_entry(identity)?;
		Ok(ProjectionRequest {
			key: ProjectionKey::new(identity, verdict, caps, self.projection_hash()),
			caps: *caps,
			verdict,
			recorded_useless,
		})
	}

	/// Probes the projection cache without allocating or invoking a worker.
	pub fn project_cached(
		&self,
		key: &ProjectionKey,
	) -> Result<Option<Arc<ProjectedVerdict>>, RegistryError> {
		Ok(self
			.projection_entry(&key.identity)?
			.tool
			.project_cached(key))
	}

	/// Stores a projection returned by a worker batch in the matching cache
	/// partition.
	pub fn cache_projected(
		&self,
		key: &ProjectionKey,
		projected: ProjectedVerdict,
	) -> Result<(), RegistryError> {
		self
			.projection_entry(&key.identity)?
			.tool
			.cache_projected(key, projected);
		Ok(())
	}

	/// Warms every cache miss for one turn before prompt assembly.
	pub async fn warm(&self, requests: &[ProjectionRequest<'_>]) -> Result<(), RegistryError> {
		for (index, request) in requests.iter().enumerate() {
			if requests[..index]
				.iter()
				.any(|earlier| earlier.key.identity == request.key.identity)
			{
				continue;
			}
			let entry = self.projection_entry(&request.key.identity)?;
			entry.tool.warm(requests).await?;
		}
		Ok(())
	}

	/// Decodes one recorded verdict into current model parts and branch
	/// metadata.
	///
	/// The durable `recorded_useless` hint is preserved for tool-owned `Ok` and
	/// `Fault` branches. Harness-owned `Args` and `Aborted` branches always
	/// force it false.
	pub fn project_verdict(
		&self,
		identity: &ToolIdentity,
		verdict: &[u8],
		recorded_useless: bool,
		caps: &PromptCaps,
	) -> Result<Arc<ProjectedVerdict>, RegistryError> {
		let request = self.projection_request(identity, verdict, recorded_useless, caps)?;
		let entry = self.projection_entry(identity)?;
		if let Some(projected) = entry.tool.project_cached(&request.key) {
			return Ok(projected);
		}
		entry
			.tool
			.warm(std::slice::from_ref(&request))
			.into_ready()?;
		entry
			.tool
			.project_cached(&request.key)
			.ok_or_else(|| RegistryError::ProjectionCacheMiss(identity.clone()))
	}

	/// Projects one exact serialized update through its registered typed tool.
	pub fn invoke_input(
		&self,
		identity: &ToolIdentity,
		invocation_id: &str,
		json: &[u8],
	) -> Result<Option<InvokeInput>, RegistryError> {
		let entry = self.projection_entry(identity)?;
		entry.tool.invoke_input(invocation_id, json)
	}

	/// Composes registered adjacent lift steps toward the live revision.
	///
	/// Failure of any step returns the exact original bytes as `Data`; partially
	/// migrated history is never exposed or mistaken for a live schema.
	pub fn project(&self, original: RecordedCallOwned) -> ProjectedCall {
		let lifted = self.project_lift_chain(&original);
		#[cfg(debug_assertions)]
		if let Some(first) = &lifted {
			let second = self
				.project_lift_chain(&original)
				.expect("a successful lift chain must remain successful on identical input");
			debug_assert_eq!(
				first.raw_args, second.raw_args,
				"lift chains must re-express arguments byte-identically"
			);
			debug_assert_eq!(
				first.verdict, second.verdict,
				"lift chains must re-express verdicts byte-identically"
			);
		}
		lifted.map_or(ProjectedCall::Data(original), ProjectedCall::Live)
	}

	fn project_lift_chain(&self, original: &RecordedCallOwned) -> Option<RecordedCallOwned> {
		let live_claim = self.live.get(&original.identity.name)?;
		let live_rev = &live_claim.rev;
		if &original.identity.rev == live_rev {
			return Some(original.clone());
		}
		let versions = self.versions.get(&original.identity.name)?;
		let mut current_rev = original.identity.rev.clone();
		let mut current =
			LiftedCall { raw_args: original.raw_args.clone(), verdict: original.verdict.clone() };
		while &current_rev != live_rev {
			let next_rev = if current_rev.family == live_rev.family && current_rev.n < live_rev.n {
				Rev { family: current_rev.family.clone(), n: current_rev.n.saturating_add(1) }
			} else {
				live_rev.clone()
			};
			let step = versions.get(&next_rev)?;
			let lifted = step.tool.lift(&current_rev, RecordedCall {
				raw_args: &current.raw_args,
				verdict:  &current.verdict,
			})?;
			current = lifted;
			current_rev = next_rev;
		}
		Some(RecordedCallOwned {
			identity: ToolIdentity { name: original.identity.name.clone(), rev: current_rev },
			raw_args: current.raw_args,
			verdict:  current.verdict,
		})
	}

	fn projection_entry(&self, identity: &ToolIdentity) -> Result<&RegistryEntry, RegistryError> {
		self
			.versions
			.get(&identity.name)
			.and_then(|versions| versions.get(&identity.rev))
			.ok_or_else(|| RegistryError::UnknownTool(identity.name.clone()))
	}

	fn live_entry(&self, path: &str) -> Result<&RegistryEntry, RegistryError> {
		let (name, claimant) = split_claimant(path);
		let claim = self
			.live
			.get(name)
			.ok_or_else(|| RegistryError::UnknownTool(Str::new(path)))?;
		let rev = claim_revision(claim, claimant)
			.ok_or_else(|| RegistryError::UnknownTool(Str::new(path)))?;
		self
			.versions
			.get(name)
			.and_then(|versions| versions.get(rev))
			.ok_or_else(|| RegistryError::UnknownTool(Str::new(path)))
	}
}

#[derive(Clone, Copy)]
struct ClaimRef<'a> {
	rev:        &'a Rev,
	precedence: Precedence,
	claimant:   &'a Str,
	replaces:   Option<&'a Str>,
}

fn resolve_claim(
	name: &Str,
	existing: Option<&Claim>,
	rev: Rev,
	claims: &Claims,
) -> Result<Claim, RegistryError> {
	let mut contenders = SmallVec::<ShadowClaim, 2>::new();
	if let Some(existing) = existing {
		contenders.push(ShadowClaim {
			rev:        existing.rev.clone(),
			precedence: existing.precedence,
			claimant:   existing.claimant.clone(),
			replaces:   existing.replaces.clone(),
		});
		contenders.extend(existing.shadowed.iter().cloned());
	}
	if let Some(position) = contenders
		.iter()
		.position(|candidate| candidate.claimant == claims.claimant)
	{
		contenders.remove(position);
	}
	contenders.push(ShadowClaim {
		rev,
		precedence: claims.precedence,
		claimant: claims.claimant.clone(),
		replaces: claims.replaces.clone(),
	});
	contenders.sort_by(|left, right| {
		right
			.precedence
			.cmp(&left.precedence)
			.then_with(|| left.claimant.cmp(&right.claimant))
	});
	for pair in contenders.windows(2) {
		if pair[0].precedence == pair[1].precedence {
			let (first, second) = if pair[0].claimant <= pair[1].claimant {
				(pair[0].claimant.clone(), pair[1].claimant.clone())
			} else {
				(pair[1].claimant.clone(), pair[0].claimant.clone())
			};
			return Err(RegistryError::PrecedenceTie { name: name.clone(), first, second });
		}
	}
	let winner = contenders.remove(0);
	Ok(Claim {
		rev:        winner.rev,
		precedence: winner.precedence,
		claimant:   winner.claimant,
		replaces:   winner.replaces,
		shadowed:   contenders.into_iter().collect(),
	})
}

fn split_claimant(path: &str) -> (&str, Option<&str>) {
	path
		.rsplit_once('@')
		.map_or((path, None), |(name, claimant)| {
			if name.is_empty() || claimant.is_empty() {
				(path, None)
			} else {
				(name, Some(claimant))
			}
		})
}

fn claim_revision<'a>(claim: &'a Claim, claimant: Option<&str>) -> Option<&'a Rev> {
	let Some(claimant) = claimant else {
		return Some(&claim.rev);
	};
	if claim.claimant == claimant {
		return Some(&claim.rev);
	}
	claim
		.shadowed
		.iter()
		.find(|shadow| shadow.claimant == claimant)
		.map(|shadow| &shadow.rev)
}

fn claim_entries(claim: &Claim) -> impl Iterator<Item = ClaimRef<'_>> {
	std::iter::once(ClaimRef {
		rev:        &claim.rev,
		precedence: claim.precedence,
		claimant:   &claim.claimant,
		replaces:   claim.replaces.as_ref(),
	})
	.chain(claim.shadowed.iter().map(|shadow| ClaimRef {
		rev:        &shadow.rev,
		precedence: shadow.precedence,
		claimant:   &shadow.claimant,
		replaces:   shadow.replaces.as_ref(),
	}))
}

fn device_issue(path: &DevicePath) -> DeviceIssue {
	DeviceIssue {
		path:     Vec::new(),
		expected: sf!("a mounted device path"),
		kind:     ArgIssueKind::Missing,
		example:  None,
		found:    Some(Str::new(path.to_string())),
	}
}

fn hash_identity(hasher: &mut blake3::Hasher, name: &Str, rev: &Rev) {
	hash_field(hasher, name.as_bytes());
	hash_field(hasher, rev.family.as_bytes());
	hash_field(hasher, &rev.n.to_le_bytes());
}

fn projection_caps_hash(caps: &PromptCaps) -> [u8; 32] {
	let mut hasher = blake3::Hasher::new();
	hash_field(&mut hasher, &caps.maximum_parts.to_le_bytes());
	hash_field(&mut hasher, &caps.maximum_text_bytes.to_le_bytes());
	hash_field(&mut hasher, &[u8::from(caps.media)]);
	hash_field(&mut hasher, &[caps.dialect as u8]);
	hash_field(&mut hasher, &[caps.model_class as u8]);
	*hasher.finalize().as_bytes()
}

fn hash_tool_route(hasher: &mut blake3::Hasher, route: &ToolRoute) {
	match route {
		ToolRoute::Native => hash_field(hasher, &[0]),
		ToolRoute::Worker { site, name } => {
			hash_field(hasher, &[1]);
			hash_field(hasher, &[*site as u8]);
			hash_field(hasher, name.as_bytes());
		},
	}
}

fn projected_part_bytes(parts: &[Part]) -> usize {
	parts.iter().fold(0, |bytes, part| {
		let part_bytes = match part {
			Part::Text { text } => text.len(),
			Part::Json { json } => json.len(),
			Part::Blob { blob, alt } => blob
				.hash
				.len()
				.saturating_add(blob.media_type.len())
				.saturating_add(alt.as_ref().map_or(0, Str::len))
				.saturating_add(size_of::<u64>()),
		};
		bytes.saturating_add(part_bytes)
	})
}

fn hash_field(hasher: &mut blake3::Hasher, field: &[u8]) {
	let len = u64::try_from(field.len()).expect("tool identity length fits in u64");
	hasher.update(&len.to_le_bytes());
	hasher.update(field);
}

fn render_arg_issue(issue: &crate::ArgIssue) -> Str {
	let mut path = String::from("$");
	for segment in &issue.path {
		match segment {
			crate::ArgPath::Key(key) => {
				path.push('[');
				path.push_str(&serde_json::to_string(key.as_str()).unwrap_or_else(|_| "\"?\"".into()));
				path.push(']');
			},
			crate::ArgPath::Index(index) => {
				path.push('[');
				path.push_str(&index.to_string());
				path.push(']');
			},
		}
	}
	let kind_json = serde_json::to_string(&issue.kind)
		.expect("serializing a fieldless argument issue kind cannot fail");
	let kind = kind_json.trim_matches('"');
	let mut text = format!("invalid arguments at {path}: expected {} ({kind})", issue.expected);
	if let Some(found) = &issue.found {
		text.push_str("; found ");
		text.push_str(found);
	}
	if let Some(example) = &issue.example {
		text.push_str("; example ");
		text.push_str(example);
	}
	Str::new(text)
}

fn render_abort(abort: &Abort) -> Str {
	match abort {
		Abort::Skipped { reason } => sf!("skipped: {reason}"),
		Abort::Interrupted { reason } => sf!("interrupted: {reason}"),
		Abort::EffectsUnknown { reason } => {
			sf!("aborted with effects unknown: {reason}")
		},
		Abort::InputDropped => sf!("aborted: invocation input dropped before commit"),
		Abort::MissingOutcome => {
			sf!("aborted: executor ended without a terminal outcome")
		},
	}
}

fn lower(entry: &dyn ErasedTool, caps: LoweringCaps) -> Result<LoweredTool, RegistryError> {
	let spec = entry.spec();
	let mut adjustments = Vec::new();
	let (input, disposition, priority) = match &spec.constraint {
		Constraint::None => (
			ToolInputConstraint::JsonSchema { parameters: entry.schema().clone(), strict: false },
			None,
			None,
		),
		Constraint::Schema { priority, .. } if caps.strict_schema => (
			ToolInputConstraint::JsonSchema { parameters: entry.schema().clone(), strict: true },
			Some(ConstraintDisposition::Required),
			Some(*priority),
		),
		Constraint::Schema { priority, on_unsupported } => {
			if *on_unsupported == omp_proto::inference::v1::Fallback::Error {
				return Err(RegistryError::UnsupportedConstraint {
					name:    spec.name.clone(),
					rev:     spec.rev.clone(),
					feature: "schema",
				});
			}
			adjustments.push(dropped(&spec.name, "schema", "catalog.strict-schema-unsupported"));
			(
				ToolInputConstraint::JsonSchema {
					parameters: entry.schema().clone(),
					strict:     false,
				},
				Some(ConstraintDisposition::Prefer),
				Some(*priority),
			)
		},
		Constraint::Grammar { syntax, definition, priority, .. }
			if caps.grammar.contains(grammar_bit(*syntax)) =>
		{
			(
				ToolInputConstraint::Grammar(ToolGrammar {
					syntax:     grammar_syntax(*syntax),
					definition: definition.clone(),
				}),
				Some(ConstraintDisposition::Required),
				Some(*priority),
			)
		},
		Constraint::Grammar { syntax, priority, on_unsupported, .. } => {
			if *on_unsupported == omp_proto::inference::v1::Fallback::Error {
				return Err(RegistryError::UnsupportedConstraint {
					name:    spec.name.clone(),
					rev:     spec.rev.clone(),
					feature: grammar_name(*syntax),
				});
			}
			adjustments.push(dropped(
				&spec.name,
				grammar_name(*syntax),
				"catalog.grammar-unsupported",
			));
			(
				ToolInputConstraint::JsonSchema {
					parameters: entry.schema().clone(),
					strict:     false,
				},
				Some(ConstraintDisposition::Prefer),
				Some(*priority),
			)
		},
	};
	Ok(LoweredTool {
		identity: spec.identity(),
		definition: ToolDefinition {
			name: spec.name.clone(),
			description: Some(spec.description.clone()),
			input,
		},
		disposition,
		priority,
		adjustments,
	})
}
const EXTENSION_PRIORITY_MAX: u8 = 127;
const CORE_PRIORITY_MIN: u8 = 128;

const fn constraint_priority(constraint: &Constraint) -> Option<u8> {
	match constraint {
		Constraint::None => None,
		Constraint::Schema { priority, .. } | Constraint::Grammar { priority, .. } => Some(*priority),
	}
}

fn advertisement_priority(entry: &RegistryEntry) -> u8 {
	let requested = constraint_priority(&entry.tool.spec().constraint).unwrap_or_default();
	if entry.claims.precedence == Precedence::CORE {
		CORE_PRIORITY_MIN.saturating_add(requested / 2)
	} else {
		requested.min(EXTENSION_PRIORITY_MAX)
	}
}

const fn constraint_requires_capacity(spec: &crate::ToolSpec) -> bool {
	matches!(
		&spec.constraint,
		Constraint::Schema { on_unsupported: omp_proto::inference::v1::Fallback::Error, .. }
			| Constraint::Grammar { on_unsupported: omp_proto::inference::v1::Fallback::Error, .. }
	)
}

fn budget_constraint_error(spec: &crate::ToolSpec, feature: &'static str) -> RegistryError {
	RegistryError::UnsupportedConstraint { name: spec.name.clone(), rev: spec.rev.clone(), feature }
}

const fn is_native_strict(tool: &LoweredTool) -> bool {
	matches!(&tool.definition.input, ToolInputConstraint::JsonSchema { strict: true, .. })
}

fn downgrade_strict(tool: &mut LoweredTool) {
	if let ToolInputConstraint::JsonSchema { strict, .. } = &mut tool.definition.input {
		*strict = false;
	}
	tool.disposition = Some(ConstraintDisposition::Prefer);
	tool.adjustments.push(dropped(
		&tool.definition.name,
		"schema",
		"catalog.strict-schema-budget-exhausted",
	));
}

fn external_error(spec: &crate::ToolSpec, operation: &'static str) -> RegistryError {
	RegistryError::UnsupportedExternal { name: spec.name.clone(), rev: spec.rev.clone(), operation }
}

const fn grammar_syntax(syntax: GrammarSyntax) -> ToolGrammarSyntax {
	match syntax {
		GrammarSyntax::Lark => ToolGrammarSyntax::Lark,
		GrammarSyntax::Regex => ToolGrammarSyntax::Regex,
		GrammarSyntax::Ebnf => ToolGrammarSyntax::Ebnf,
	}
}

const fn grammar_bit(syntax: GrammarSyntax) -> GrammarBits {
	match syntax {
		GrammarSyntax::Lark => GrammarBits::LARK,
		GrammarSyntax::Regex => GrammarBits::REGEX,
		GrammarSyntax::Ebnf => GrammarBits::EBNF,
	}
}

const fn grammar_name(syntax: GrammarSyntax) -> &'static str {
	match syntax {
		GrammarSyntax::Lark => "lark",
		GrammarSyntax::Regex => "regex",
		GrammarSyntax::Ebnf => "ebnf",
	}
}

fn dropped(name: &Str, feature: &str, reason: &'static str) -> Adjustment {
	Adjustment::Dropped {
		feature: FeatureId(sf!("tool.{name}.{feature}")),
		reason:  ReasonId(Str::new(reason)),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{Effects, Ev, ToolSpec};

	struct LiftTool {
		spec: ToolSpec,
	}

	impl Tool for LiftTool {
		type Fault = Value;
		type Params = Value;
		type Payload = Value;
		type Update = Value;

		fn spec(&self) -> &ToolSpec {
			&self.spec
		}

		fn call<'c>(
			&'c self,
			_params: IncomingParams<'c>,
		) -> impl Stream<Item = Ev<Self::Update, Self::Payload, Self::Fault>> + Send + 'c {
			futures::stream::empty()
		}

		fn prompt(
			&self,
			_view: Result<&Self::Payload, &Self::Fault>,
			_caps: &PromptCaps,
		) -> Vec<Part> {
			Vec::new()
		}

		fn lift(&self, _from: &Rev, call: RecordedCall<'_>) -> Option<LiftedCall> {
			Some(LiftedCall {
				raw_args: Bytes::copy_from_slice(call.raw_args),
				verdict:  Bytes::copy_from_slice(call.verdict),
			})
		}
	}

	fn identity(n: u16) -> ToolIdentity {
		ToolIdentity { name: sf!("lift"), rev: Rev { family: sf!("x"), n } }
	}

	fn caps() -> PromptCaps {
		PromptCaps {
			maximum_parts:      1,
			maximum_text_bytes: 1,
			media:              false,
			dialect:            crate::Dialect::Native,
			model_class:        crate::ModelClass::Standard,
		}
	}

	fn tool(n: u16) -> LiftTool {
		LiftTool {
			spec: ToolSpec {
				name:            sf!("lift"),
				rev:             identity(n).rev,
				description:     sf!("lift test"),
				schema:          Bytes::from_static(b"{}"),
				constraint:      Constraint::None,
				effects:         Effects::empty(),
				projection_code: [n as u8; 32],
			},
		}
	}

	#[test]
	fn projection_cache_returns_the_same_arc_only_for_the_same_key() {
		let cache = ProjectionCache::default();
		let key = ProjectionKey::new(&identity(1), b"{\"kind\":\"ok\"}", &caps(), [1; 32]);
		let different = ProjectionKey::new(&identity(1), b"{\"kind\":\"ok\"}", &caps(), [2; 32]);
		assert!(cache.get(0, &key).is_none());
		cache.insert(0, &key, ProjectedVerdict {
			parts:    Arc::<[Part]>::from([]),
			is_error: false,
			useless:  false,
		});
		let hit = cache.get(0, &key).expect("matching key hits");
		assert!(Arc::ptr_eq(&hit, &cache.get(0, &key).expect("second matching key hits")));
		assert!(cache.get(0, &different).is_none());
	}

	#[test]
	fn registered_lift_runs_byte_stably() {
		let mut registry = Registry::new();
		let claims =
			Claims { precedence: Precedence::DEFAULT, claimant: sf!("test/lift"), replaces: None };
		registry
			.register(tool(1), Presentation::Device, claims.clone())
			.expect("first revision registers");
		registry
			.register(tool(2), Presentation::Device, claims)
			.expect("live revision registers");
		let original = RecordedCallOwned {
			identity: identity(1),
			raw_args: Bytes::from_static(br#"{"old":true}"#),
			verdict:  Bytes::from_static(br#"{"kind":"ok","value":null}"#),
		};
		let ProjectedCall::Live(lifted) = registry.project(original.clone()) else {
			panic!("registered lift must dispatch");
		};
		assert_eq!(lifted.identity, identity(2));
		assert_eq!(lifted.raw_args, original.raw_args);
		assert_eq!(lifted.verdict, original.verdict);
	}
}
