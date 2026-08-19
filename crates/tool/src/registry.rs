//! One-time type erasure, live advertisement, and historical lift composition.

use std::{collections::BTreeMap, pin::Pin, sync::Arc};

use async_stream::stream;
use bytes::Bytes;
use futures::{Stream, StreamExt, pin_mut};
use omp_core::Str;
use omp_llm_catalog::GrammarBits;
use omp_llm_inference::{
	Adjustment, FeatureId, OpaqueJson, ReasonId, ToolDefinition, ToolGrammar, ToolGrammarSyntax,
	ToolInputConstraint,
};
use omp_proto::inference::v1::InvokeInput;
use serde_json::Value;
use smallvec::SmallVec;
use thiserror::Error;

use crate::{
	Abort, CallOutcome, Constraint, GrammarSyntax, IncomingParams, LiftedCall, Part, Presentation,
	PromptCaps, RecordedCall, RecordedCallOwned, Rev, Tool, ToolIdentity,
};

/// Catalog capabilities needed for deterministic tool lowering.
#[derive(Clone, Copy, Debug)]
pub struct LoweringCaps {
	/// Whether per-tool strict JSON Schema is supported.
	pub strict_schema: bool,
	/// Supported freeform grammar languages.
	pub grammar:       GrammarBits,
}

/// Strength retained after capability-aware constraint lowering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConstraintDisposition {
	/// Route can honor the requested constraint.
	Required,
	/// Request remains a preference and is receipted when unavailable.
	Prefer,
}
/// Execution route associated with a live registry entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolRoute {
	/// In-process typed Rust executor erased at registration.
	Native,
	/// Externally supervised worker executor.
	Worker,
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
	/// Long-form documentation, when supplied by the declaration surface.
	pub docs:     Option<&'a str>,
	/// Execution placement, independent of device presentation.
	pub route:    ToolRoute,
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

/// Authoritative model projection and branch metadata decoded from one verdict.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectedVerdict {
	/// Model-facing parts under the supplied current-model capabilities.
	pub parts:    Vec<Part>,
	/// Whether the decoded verdict branch is a fault, argument error, or abort.
	pub is_error: bool,
	/// Durable compaction hint, forced false for argument errors and aborts.
	pub useless:  bool,
}

/// Registry construction, dispatch, serialization, or projection failure.
#[derive(Debug, Error)]
pub enum RegistryError {
	/// `(name, revision)` was registered twice.
	#[error("tool revision already registered: {0}@{1}")]
	Duplicate(Str, Rev),
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
	fn route(&self) -> ToolRoute;
	fn schema(&self) -> &OpaqueJson;
	fn call<'a>(&'a self, params: IncomingParams<'a>) -> ErasedStream<'a>;
	fn project_verdict(
		&self,
		verdict: &[u8],
		recorded_useless: bool,
		caps: PromptCaps,
	) -> Result<ProjectedVerdict, RegistryError>;
	fn invoke_input(
		&self,
		invocation_id: &str,
		json: &[u8],
	) -> Result<Option<InvokeInput>, RegistryError>;
	fn lift(&self, from: &Rev, call: RecordedCall<'_>) -> Option<LiftedCall>;
}

struct Worker {
	spec:   crate::ToolSpec,
	schema: OpaqueJson,
}

impl ErasedTool for Worker {
	fn spec(&self) -> &crate::ToolSpec {
		&self.spec
	}

	fn route(&self) -> ToolRoute {
		ToolRoute::Worker
	}

	fn schema(&self) -> &OpaqueJson {
		&self.schema
	}

	fn call<'a>(&'a self, _params: IncomingParams<'a>) -> ErasedStream<'a> {
		let error = external_error(&self.spec, "invoke");
		Box::pin(futures::stream::once(async move { Err(error) }))
	}

	fn project_verdict(
		&self,
		_verdict: &[u8],
		_recorded_useless: bool,
		_caps: PromptCaps,
	) -> Result<ProjectedVerdict, RegistryError> {
		Err(external_error(&self.spec, "project_verdict"))
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
	tool:   T,
	schema: OpaqueJson,
}

impl<T: Tool> ErasedTool for Registered<T> {
	fn spec(&self) -> &crate::ToolSpec {
		self.tool.spec()
	}

	fn route(&self) -> ToolRoute {
		ToolRoute::Native
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

	fn project_verdict(
		&self,
		verdict: &[u8],
		recorded_useless: bool,
		caps: PromptCaps,
	) -> Result<ProjectedVerdict, RegistryError> {
		let verdict: CallOutcome<T::Payload, T::Fault> = serde_json::from_slice(verdict)
			.map_err(|_| RegistryError::VerdictShape(self.tool.spec().name.clone()))?;
		Ok(match &verdict {
			CallOutcome::Ok(payload) => ProjectedVerdict {
				parts:    self.tool.prompt(Ok(payload), &caps),
				is_error: false,
				useless:  recorded_useless,
			},
			CallOutcome::Faulted(fault) => ProjectedVerdict {
				parts:    self.tool.prompt(Err(fault), &caps),
				is_error: true,
				useless:  recorded_useless,
			},
			CallOutcome::ArgsRejected(issue) => ProjectedVerdict {
				parts:    vec![Part::Text { text: render_arg_issue(issue) }],
				is_error: true,
				useless:  false,
			},
			CallOutcome::Aborted { abort, .. } => ProjectedVerdict {
				parts:    vec![Part::Text { text: render_abort(abort) }],
				is_error: true,
				useless:  false,
			},
		})
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
	versions: BTreeMap<Str, BTreeMap<Rev, RegistryEntry>>,
	live:     BTreeMap<Str, Claim>,
}

impl Registry {
	/// Creates an empty registry.
	pub fn new() -> Self {
		Self::default()
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
		let entry = RegistryEntry {
			tool: Arc::new(Registered { tool, schema: OpaqueJson::new(value) }),
			presentation,
			claims,
		};
		self.insert(name, rev, entry)
	}

	/// Registers an externally supervised declaration under one presentation
	/// and claimant.
	///
	/// Worker execution and typed projection remain owned by the worker route.
	pub fn register_worker(
		&mut self,
		spec: crate::ToolSpec,
		presentation: Presentation,
		claims: Claims,
	) -> Result<(), RegistryError> {
		let name = spec.name.clone();
		let rev = spec.rev.clone();
		let value = serde_json::from_slice(&spec.schema).map_err(|source| {
			RegistryError::InvalidSchema { name: name.clone(), rev: rev.clone(), source }
		})?;
		let entry = RegistryEntry {
			tool: Arc::new(Worker { spec, schema: OpaqueJson::new(value) }),
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
		Ok(self.live_entry(name)?.tool.route())
	}

	/// Returns the presentation of a winning or claimant-qualified entry.
	pub fn presentation(&self, name: &str) -> Result<Presentation, RegistryError> {
		Ok(self.live_entry(name)?.presentation)
	}

	/// Iterates mounted catalog devices without allocating.
	///
	/// Shadowed devices remain claimant-qualified but are intentionally absent.
	pub fn devices(&self) -> impl DoubleEndedIterator<Item = MountedDevice<'_>> + '_ {
		self.live.iter().filter_map(|(name, claim)| {
			let entry = self.versions.get(name)?.get(&claim.rev)?;
			(entry.presentation == Presentation::Device).then(|| MountedDevice {
				name,
				rev: &claim.rev,
				claimant: &claim.claimant,
				summary: &entry.tool.spec().description,
				schema: entry.tool.spec().schema.as_ref(),
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
		for (name, claim) in &self.live {
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
				hasher.update(&[entry.tool.route() as u8]);
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
		params: IncomingParams<'a>,
	) -> Result<ErasedStream<'a>, RegistryError> {
		let entry = self.live_entry(name)?;
		if entry.tool.route() == ToolRoute::Worker {
			return Err(external_error(entry.tool.spec(), "invoke"));
		}
		Ok(entry.tool.call(params))
	}

	/// Lowers only policy-resolved model-visible slots.
	pub fn advertise(&self, caps: LoweringCaps) -> Result<Vec<LoweredTool>, RegistryError> {
		self
			.live
			.iter()
			.filter_map(|(name, claim)| {
				let entry = self.versions.get(name)?.get(&claim.rev)?;
				(entry.presentation == Presentation::Slot).then(|| lower(entry.tool.as_ref(), caps))
			})
			.collect()
	}

	/// Deterministically projects a structured live verdict through its tool.
	pub fn prompt(
		&self,
		identity: &ToolIdentity,
		verdict: &[u8],
		caps: &PromptCaps,
	) -> Result<Option<Vec<Part>>, RegistryError> {
		Ok(Some(self.project_verdict(identity, verdict, false, caps)?.parts))
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
	) -> Result<ProjectedVerdict, RegistryError> {
		let entry = self
			.versions
			.get(&identity.name)
			.and_then(|versions| versions.get(&identity.rev))
			.ok_or_else(|| RegistryError::UnknownTool(identity.name.clone()))?;
		entry.tool.project_verdict(verdict, recorded_useless, *caps)
	}

	/// Projects one exact serialized update through its registered typed tool.
	pub fn invoke_input(
		&self,
		identity: &ToolIdentity,
		invocation_id: &str,
		json: &[u8],
	) -> Result<Option<InvokeInput>, RegistryError> {
		let entry = self
			.versions
			.get(&identity.name)
			.and_then(|versions| versions.get(&identity.rev))
			.ok_or_else(|| RegistryError::UnknownTool(identity.name.clone()))?;
		entry.tool.invoke_input(invocation_id, json)
	}

	/// Composes registered adjacent lift steps toward the live revision.
	///
	/// Failure of any step returns the exact original bytes as `Data`; partially
	/// migrated history is never exposed or mistaken for a live schema.
	pub fn project(&self, original: RecordedCallOwned) -> ProjectedCall {
		let Some(live_claim) = self.live.get(&original.identity.name) else {
			return ProjectedCall::Data(original);
		};
		let live_rev = &live_claim.rev;
		if &original.identity.rev == live_rev {
			return ProjectedCall::Live(original);
		}
		let Some(versions) = self.versions.get(&original.identity.name) else {
			return ProjectedCall::Data(original);
		};

		let mut current_rev = original.identity.rev.clone();
		let mut current =
			LiftedCall { raw_args: original.raw_args.clone(), verdict: original.verdict.clone() };
		while &current_rev != live_rev {
			let next_rev = if current_rev.family == live_rev.family && current_rev.n < live_rev.n {
				Rev { family: current_rev.family.clone(), n: current_rev.n.saturating_add(1) }
			} else {
				live_rev.clone()
			};
			let Some(step) = versions.get(&next_rev) else {
				return ProjectedCall::Data(original);
			};
			let Some(lifted) = step.tool.lift(&current_rev, RecordedCall {
				raw_args: &current.raw_args,
				verdict:  &current.verdict,
			}) else {
				return ProjectedCall::Data(original);
			};
			current = lifted;
			current_rev = next_rev;
		}
		ProjectedCall::Live(RecordedCallOwned {
			identity: ToolIdentity { name: original.identity.name, rev: current_rev },
			raw_args: current.raw_args,
			verdict:  current.verdict,
		})
	}

	fn live_entry(&self, path: &str) -> Result<&RegistryEntry, RegistryError> {
		let (name, claimant) = split_claimant(path);
		let claim = self
			.live
			.get(name)
			.ok_or_else(|| RegistryError::UnknownTool(Str::from(path)))?;
		let rev = claim_revision(claim, claimant)
			.ok_or_else(|| RegistryError::UnknownTool(Str::from(path)))?;
		self
			.versions
			.get(name)
			.and_then(|versions| versions.get(rev))
			.ok_or_else(|| RegistryError::UnknownTool(Str::from(path)))
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

fn hash_identity(hasher: &mut blake3::Hasher, name: &Str, rev: &Rev) {
	hash_field(hasher, name.as_bytes());
	hash_field(hasher, rev.family.as_bytes());
	hash_field(hasher, &rev.n.to_le_bytes());
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
	Str::from(text)
}

fn render_abort(abort: &Abort) -> Str {
	match abort {
		Abort::Skipped { reason } => Str::from(format!("skipped: {reason}")),
		Abort::Interrupted { reason } => Str::from(format!("interrupted: {reason}")),
		Abort::EffectsUnknown { reason } => {
			Str::from(format!("aborted with effects unknown: {reason}"))
		},
		Abort::InputDropped => Str::new_static("aborted: invocation input dropped before commit"),
		Abort::MissingOutcome => {
			Str::new_static("aborted: executor ended without a terminal outcome")
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
		feature: FeatureId(Str::from(format!("tool.{name}.{feature}"))),
		reason:  ReasonId(Str::from(reason)),
	}
}
