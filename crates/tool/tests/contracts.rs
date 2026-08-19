//! Observable contracts for typed tools, lowering, invocation input, and
//! history.

use std::{
	convert::Infallible,
	future::{Future, ready},
	sync::{
		Arc,
		atomic::{AtomicUsize, Ordering},
	},
};

use async_stream::stream;
use bytes::Bytes;
use futures::{FutureExt, Stream, StreamExt, executor::block_on};
use omp_core::Str;
use omp_llm_catalog::GrammarBits;
use omp_llm_inference::{Adjustment, ToolGrammarSyntax};
use omp_tool::{
	Abort, ArgIssue, ArgIssueKind, ArgPath, ArtifactLifetime, BlobRef, CallOutcome,
	CallOutcomeDetails, CallOutcomeSpill, CapsBase, Claims, CommitError, Constraint,
	ConstraintDisposition, ErasedEv, ErasedOutcome, Ev, ExpectedArtifact, Fallback, GrammarSyntax,
	IncomingParams, Interrupt, InterruptWaitError, JobOwner, JobRef, LiftedCall, LoweringCaps,
	ModelClass, ParamError, Part, Precedence, Presentation, ProjectedCall, PromptCaps, RecordedCall,
	RecordedCallOwned, Registry, RegistryError, Rev, Tool, ToolIdentity, ToolSpec, ToolTerminal,
	call_outcome_details,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct FakeParams {
	value: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct FakePayload {
	implementation: Str,
	raw:            Str,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct FakeFault {
	message: Str,
}

struct FakeTool {
	spec:      ToolSpec,
	marker:    Str,
	calls:     Arc<AtomicUsize>,
	lift_from: Option<u16>,
}

impl FakeTool {
	fn new(
		n: u16,
		marker: &str,
		schema: &'static [u8],
		constraint: Constraint,
		calls: Arc<AtomicUsize>,
	) -> Self {
		Self {
			spec: ToolSpec {
				name: Str::from("typed_fake"),
				rev: Rev { family: Str::from("fake"), n },
				description: Str::from(format!("fake revision {n}")),
				schema: Bytes::from_static(schema),
				constraint,
				projection_code: [0; 32],
			},
			marker: Str::from(marker),
			calls,
			lift_from: None,
		}
	}

	const fn lifting_from(mut self, n: u16) -> Self {
		self.lift_from = Some(n);
		self
	}

	fn named(mut self, name: &str) -> Self {
		self.spec.name = Str::from(name);
		self
	}

	fn with_projection_code(mut self, projection_code: [u8; 32]) -> Self {
		self.spec.projection_code = projection_code;
		self
	}
}

impl Tool for FakeTool {
	type Fault = FakeFault;
	type Params = FakeParams;
	type Payload = FakePayload;
	type Update = Str;

	fn spec(&self) -> &ToolSpec {
		&self.spec
	}

	fn call<'c>(
		&'c self,
		mut params: IncomingParams<'c>,
	) -> impl Stream<Item = Ev<Self::Update, Self::Payload, Self::Fault>> + Send + 'c {
		stream! {
			let raw = params.committed().await.expect("test invocation commits its arguments");
			self.calls.fetch_add(1, Ordering::SeqCst);
			yield Ev::Update(self.marker.clone());
			yield Ev::Done(ToolTerminal::Done {
				result: Ok(FakePayload { implementation: self.marker.clone(), raw }),
				useless: false,
			});
		}
	}

	fn prompt(&self, view: Result<&Self::Payload, &Self::Fault>, caps: &PromptCaps) -> Vec<Part> {
		let branch = match view {
			Ok(payload) => format!("ok:{}:{}", payload.implementation, payload.raw),
			Err(fault) => format!("fault:{}", fault.message),
		};
		vec![
			Part::Text {
				text: Str::from(format!(
					"{}|{branch}|{}/{}/{}",
					self.marker, caps.maximum_parts, caps.maximum_text_bytes, caps.media
				)),
			},
			Part::Json { json: Bytes::from(serde_json::to_vec(&branch).expect("string serializes")) },
		]
	}

	fn lift(&self, from: &Rev, call: RecordedCall<'_>) -> Option<LiftedCall> {
		if from.family != self.spec.rev.family || self.lift_from != Some(from.n) {
			return None;
		}
		let suffix = format!(">{}", self.spec.rev.n);
		let mut raw_args = call.raw_args.to_vec();
		raw_args.extend_from_slice(suffix.as_bytes());
		let mut verdict = call.verdict.to_vec();
		verdict.extend_from_slice(suffix.as_bytes());
		Some(LiftedCall { raw_args: Bytes::from(raw_args), verdict: Bytes::from(verdict) })
	}
}

struct PullingTool {
	spec: ToolSpec,
}

impl PullingTool {
	fn new() -> Self {
		Self {
			spec: ToolSpec {
				name:            Str::from("pulling_fake"),
				rev:             Rev { family: Str::from("fake"), n: 1 },
				description:     Str::from("pulls one typed argument"),
				schema:          Bytes::from_static(
					br#"{"type":"object","properties":{"wanted":{"type":"number"}}}"#,
				),
				constraint:      Constraint::None,
				projection_code: [0; 32],
			},
		}
	}
}

impl Tool for PullingTool {
	type Fault = FakeFault;
	type Params = FakeParams;
	type Payload = FakePayload;
	type Update = Str;

	fn spec(&self) -> &ToolSpec {
		&self.spec
	}

	fn call<'c>(
		&'c self,
		mut params: IncomingParams<'c>,
	) -> impl Stream<Item = Ev<Self::Update, Self::Payload, Self::Fault>> + Send + 'c {
		stream! {
			let error = params
				.pull(|mut doc| async move {
					let root = doc.json();
					let mut object = root.object();
					let mut value = object.key("wanted");
					value.number().await
				})
				.await
				.expect_err("test supplies a mistyped pulled value");
			let ParamError::Args(issue) = error else {
				panic!("typed pull must report an argument issue")
			};
			yield Ev::Args(*issue);
			yield Ev::Update(Str::from("post-terminal update"));
			yield Ev::Done(ToolTerminal::Done {
				result: Ok(FakePayload {
					implementation: Str::from("post-terminal"),
					raw: Str::from("must not escape"),
				}),
				useless: false,
			});
		}
	}

	fn prompt(&self, _view: Result<&Self::Payload, &Self::Fault>, _caps: &PromptCaps) -> Vec<Part> {
		Vec::new()
	}
}

struct AbortingTool {
	spec: ToolSpec,
}

impl AbortingTool {
	fn new() -> Self {
		Self {
			spec: ToolSpec {
				name:            Str::from("aborting_fake"),
				rev:             Rev { family: Str::from("fake"), n: 1 },
				description:     Str::from("aborts before completion"),
				schema:          Bytes::from_static(br#"{"type":"object"}"#),
				constraint:      Constraint::None,
				projection_code: [0; 32],
			},
		}
	}
}

impl Tool for AbortingTool {
	type Fault = FakeFault;
	type Params = FakeParams;
	type Payload = FakePayload;
	type Update = Str;

	fn spec(&self) -> &ToolSpec {
		&self.spec
	}

	fn call<'c>(
		&'c self,
		params: IncomingParams<'c>,
	) -> impl Stream<Item = Ev<Self::Update, Self::Payload, Self::Fault>> + Send + 'c {
		drop(params);
		stream! {
			yield Ev::Aborted(Abort::Skipped { reason: Str::from("policy denied") });
			yield Ev::Update(Str::from("post-terminal update"));
			yield Ev::Done(ToolTerminal::Done {
				result: Err(FakeFault { message: Str::from("must not escape") }),
				useless: false,
			});
		}
	}

	fn prompt(&self, _view: Result<&Self::Payload, &Self::Fault>, _caps: &PromptCaps) -> Vec<Part> {
		Vec::new()
	}
}

fn fake_tool(n: u16, marker: &str, calls: Arc<AtomicUsize>) -> FakeTool {
	FakeTool::new(
		n,
		marker,
		br#"{"type":"object","properties":{"value":{"type":"integer"}},"required":["value"]}"#,
		Constraint::None,
		calls,
	)
}

fn claims(claimant: &str, precedence: Precedence) -> Claims {
	Claims { precedence, claimant: Str::from(claimant), replaces: None }
}

fn identity(n: u16) -> ToolIdentity {
	ToolIdentity { name: Str::from("typed_fake"), rev: Rev { family: Str::from("fake"), n } }
}

fn worker_spec(name: &str, projection_code: [u8; 32]) -> ToolSpec {
	ToolSpec {
		name: Str::from(name),
		rev: Rev { family: Str::from("worker"), n: 1 },
		description: Str::from(format!("{name} device")),
		schema: Bytes::from_static(br#"{"type":"object"}"#),
		constraint: Constraint::None,
		projection_code,
	}
}

#[test]
fn duplicate_registration_never_replaces_the_erased_implementation() {
	let original_calls = Arc::new(AtomicUsize::new(0));
	let rejected_calls = Arc::new(AtomicUsize::new(0));
	let mut registry = Registry::new();
	registry
		.register(
			fake_tool(1, "original", Arc::clone(&original_calls)),
			Presentation::Slot,
			claims("omp/tests", Precedence::CORE),
		)
		.expect("first typed registration succeeds");
	let error = registry
		.register(
			fake_tool(1, "replacement", Arc::clone(&rejected_calls)),
			Presentation::Slot,
			claims("omp/tests", Precedence::CORE),
		)
		.expect_err("the same durable revision is erased only once");
	assert!(
		matches!(error, RegistryError::Duplicate(name, rev) if name == "typed_fake" && rev == identity(1).rev)
	);

	let (feed, params) = IncomingParams::channel();
	feed
		.args_committed(Str::from("{value:1}"))
		.expect("consumer remains live");
	let events = block_on(
		registry
			.invoke("typed_fake", params)
			.expect("live tool is invokable")
			.collect::<Vec<_>>(),
	);
	assert_eq!(original_calls.load(Ordering::SeqCst), 1);
	assert_eq!(rejected_calls.load(Ordering::SeqCst), 0);
	let [
		Ok(ErasedEv::Update(update)),
		Ok(ErasedEv::Done(ErasedOutcome::Done { verdict, useless: false })),
	] = events.as_slice()
	else {
		panic!("expected an erased update and terminal outcome: {events:?}")
	};
	assert_eq!(
		serde_json::from_slice::<Str>(update)
			.expect("typed update remains recoverable after erasure"),
		"original"
	);
	let verdict: CallOutcome<FakePayload, FakeFault> =
		serde_json::from_slice(verdict).expect("typed verdict remains recoverable after erasure");
	assert_eq!(
		verdict,
		CallOutcome::Ok(FakePayload {
			implementation: Str::from("original"),
			raw:            Str::from("{value:1}"),
		})
	);
}

#[test]
fn hashes_are_registration_order_independent() {
	let calls = Arc::new(AtomicUsize::new(0));
	let mut first = Registry::new();
	first
		.register(
			fake_tool(1, "slot", Arc::clone(&calls)).named("slot_fake"),
			Presentation::Slot,
			claims("omp/core", Precedence::CORE),
		)
		.unwrap();
	first
		.register_worker(
			worker_spec("device_fake", [9; 32]),
			Presentation::Device,
			claims("publisher/device", Precedence::DEFAULT),
		)
		.unwrap();

	let mut second = Registry::new();
	second
		.register_worker(
			worker_spec("device_fake", [9; 32]),
			Presentation::Device,
			claims("publisher/device", Precedence::DEFAULT),
		)
		.unwrap();
	second
		.register(
			fake_tool(1, "slot", calls).named("slot_fake"),
			Presentation::Slot,
			claims("omp/core", Precedence::CORE),
		)
		.unwrap();

	assert_eq!(first.slot_hash(), second.slot_hash());
	assert_eq!(first.device_hash(), second.device_hash());
	assert_eq!(first.projection_hash(), second.projection_hash());
}

#[test]
fn worker_device_is_catalogued_without_consuming_a_model_slot() {
	let mut registry = Registry::new();
	let empty_slots = registry.slot_hash();
	let empty_devices = registry.device_hash();
	registry
		.register_worker(
			worker_spec("catalogued", [3; 32]),
			Presentation::Device,
			claims("publisher/catalogue", Precedence::DEFAULT),
		)
		.unwrap();

	assert_eq!(registry.slot_hash(), empty_slots);
	assert_ne!(registry.device_hash(), empty_devices);
	assert!(
		registry
			.advertise(LoweringCaps { strict_schema: false, grammar: GrammarBits::empty() })
			.unwrap()
			.is_empty()
	);
	assert_eq!(registry.route("catalogued").unwrap(), omp_tool::ToolRoute::Worker);
	assert_eq!(registry.presentation("catalogued").unwrap(), Presentation::Device);
	let mut devices = registry.devices();
	let device = devices.next().expect("worker device is mounted");
	assert_eq!(device.name, "catalogued");
	assert_eq!(device.claimant, "publisher/catalogue");
	assert_eq!(device.route, omp_tool::ToolRoute::Worker);
	assert_eq!(device.summary, "catalogued device");
	assert_eq!(device.schema, br#"{"type":"object"}"#);
	assert_eq!(device.docs, None);
	assert!(devices.next().is_none());
}

#[test]
fn route_and_presentation_filter_independently() {
	let mut registry = Registry::new();
	registry
		.register(
			fake_tool(1, "native-device", Arc::new(AtomicUsize::new(0))).named("native_device"),
			Presentation::Device,
			claims("publisher/native", Precedence::DEFAULT),
		)
		.unwrap();
	registry
		.register_worker(
			worker_spec("worker_slot", [5; 32]),
			Presentation::Slot,
			claims("publisher/hard", Precedence::INTEGRATION),
		)
		.unwrap();

	let advertised = registry
		.advertise(LoweringCaps { strict_schema: false, grammar: GrammarBits::empty() })
		.unwrap();
	assert_eq!(advertised.len(), 1);
	assert_eq!(advertised[0].identity.name, "worker_slot");
	assert_eq!(registry.route("worker_slot").unwrap(), omp_tool::ToolRoute::Worker);
	assert_eq!(registry.presentation("worker_slot").unwrap(), Presentation::Slot);

	let mut devices = registry.devices();
	let device = devices.next().expect("native soft tool is catalogued");
	assert_eq!(device.name, "native_device");
	assert_eq!(device.route, omp_tool::ToolRoute::Native);
	assert!(devices.next().is_none());
}

#[test]
fn projection_code_moves_only_projection_identity() {
	let calls = Arc::new(AtomicUsize::new(0));
	let mut first = Registry::new();
	first
		.register(
			fake_tool(1, "same", Arc::clone(&calls)).with_projection_code([1; 32]),
			Presentation::Slot,
			claims("omp/tests", Precedence::CORE),
		)
		.unwrap();
	let mut second = Registry::new();
	second
		.register(
			fake_tool(1, "same", calls).with_projection_code([2; 32]),
			Presentation::Slot,
			claims("omp/tests", Precedence::CORE),
		)
		.unwrap();

	assert_eq!(first.slot_hash(), second.slot_hash());
	assert_eq!(first.device_hash(), second.device_hash());
	assert_ne!(first.projection_hash(), second.projection_hash());
}

#[test]
fn precedence_ties_fail_closed_with_both_claimants() {
	let mut registry = Registry::new();
	registry
		.register(
			fake_tool(1, "first", Arc::new(AtomicUsize::new(0))).named("search"),
			Presentation::Device,
			claims("alpha/search", Precedence::ENHANCEMENT),
		)
		.unwrap();
	let error = registry
		.register(
			fake_tool(2, "second", Arc::new(AtomicUsize::new(0))).named("search"),
			Presentation::Device,
			claims("beta/search", Precedence::ENHANCEMENT),
		)
		.expect_err("equal precedence must not resolve by registration order");
	assert!(matches!(
		error,
		RegistryError::PrecedenceTie { name, first, second }
			if name == "search" && first == "alpha/search" && second == "beta/search"
	));
}

#[test]
fn shadowed_claims_are_only_claimant_qualified_reachable() {
	let mut registry = Registry::new();
	registry
		.register(
			fake_tool(1, "lower", Arc::new(AtomicUsize::new(0))).named("search"),
			Presentation::Device,
			claims("low/search", Precedence::DEFAULT),
		)
		.unwrap();
	registry
		.register(
			fake_tool(2, "higher", Arc::new(AtomicUsize::new(0))).named("search"),
			Presentation::Device,
			Claims {
				precedence: Precedence::ENHANCEMENT,
				claimant:   Str::from("high/search"),
				replaces:   Some(Str::from("search")),
			},
		)
		.unwrap();

	let claim = registry.claim("search").unwrap();
	assert_eq!(claim.claimant, "high/search");
	assert_eq!(claim.replaces.as_deref(), Some("search"));
	assert_eq!(claim.shadowed.len(), 1);
	assert_eq!(claim.shadowed[0].claimant, "low/search");
	assert_eq!(registry.devices().count(), 1);
	assert_eq!(registry.live_identity("search@low/search").unwrap().1.n, 1);

	let (feed, params) = IncomingParams::channel();
	feed.args_committed(Str::from("{value:1}")).unwrap();
	let events = block_on(
		registry
			.invoke("search@low/search", params)
			.expect("shadow remains explicitly reachable")
			.collect::<Vec<_>>(),
	);
	let Some(Ok(ErasedEv::Update(update))) = events.first() else {
		panic!("qualified dispatch must reach the lower implementation: {events:?}")
	};
	assert_eq!(serde_json::from_slice::<Str>(update).unwrap(), "lower");
}

#[test]
fn core_precedence_band_rejects_devices_and_overrides() {
	let mut registry = Registry::new();
	let error = registry
		.register_worker(
			worker_spec("reserved", [4; 32]),
			Presentation::Device,
			claims("publisher/reserved", Precedence::CORE),
		)
		.expect_err("core precedence is reserved from devices");
	assert!(matches!(
		error,
		RegistryError::CoreNameClaim { name, claimant, precedence }
			if name == "reserved"
				&& claimant == "publisher/reserved"
				&& precedence == Precedence::CORE
	));

	registry
		.register(
			fake_tool(1, "core", Arc::new(AtomicUsize::new(0))).named("core_name"),
			Presentation::Slot,
			claims("omp/core", Precedence::CORE),
		)
		.unwrap();
	let error = registry
		.register(
			fake_tool(2, "override", Arc::new(AtomicUsize::new(0))).named("core_name"),
			Presentation::Slot,
			claims("publisher/override", Precedence(Precedence::CORE.0 + 1)),
		)
		.expect_err("no declaration may outrank a core name");
	assert!(matches!(
		error,
		RegistryError::CoreNameClaim { name, claimant, precedence }
			if name == "core_name"
				&& claimant == "publisher/override"
				&& precedence == Precedence(1_001)
	));
}

#[test]
fn erased_tool_does_not_run_before_explicit_argument_commitment() {
	let calls = Arc::new(AtomicUsize::new(0));
	let mut registry = Registry::new();
	registry
		.register(
			fake_tool(1, "gated", Arc::clone(&calls)),
			Presentation::Slot,
			claims("omp/tests", Precedence::CORE),
		)
		.unwrap();
	let (feed, params) = IncomingParams::channel();
	let mut events = registry.invoke("typed_fake", params).unwrap();

	assert!(events.next().now_or_never().is_none());
	assert_eq!(calls.load(Ordering::SeqCst), 0);

	feed.args_committed(Str::from("{value:1}")).unwrap();
	assert!(matches!(block_on(events.next()), Some(Ok(ErasedEv::Update(_)))));
	assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn pulled_mismatch_erases_to_args_outcome_and_fuses_every_later_event() {
	let mut registry = Registry::new();
	registry
		.register(PullingTool::new(), Presentation::Slot, claims("omp/tests", Precedence::CORE))
		.unwrap();
	let raw = r#"{"wanted":"seven","ignored":true}"#;
	let (feed, params) = IncomingParams::channel();
	feed.args_committed(Str::from(raw)).unwrap();

	let events = block_on(
		registry
			.invoke("pulling_fake", params)
			.unwrap()
			.collect::<Vec<_>>(),
	);
	let [Ok(ErasedEv::Done(ErasedOutcome::Done { verdict, useless: false }))] = events.as_slice()
	else {
		panic!("Args must be the sole erased terminal event: {events:?}")
	};
	let verdict: CallOutcome<FakePayload, FakeFault> = serde_json::from_slice(verdict).unwrap();
	assert_eq!(
		verdict,
		CallOutcome::ArgsRejected(ArgIssue {
			path:     vec![ArgPath::Key(Str::from("wanted"))],
			expected: Str::from("number"),
			kind:     ArgIssueKind::TypeMismatch,
			example:  None,
			found:    Some(Str::from("string")),
		})
	);
}

#[test]
fn aborted_outcome_is_terminal_and_fuses_every_later_event() {
	let mut registry = Registry::new();
	registry
		.register(AbortingTool::new(), Presentation::Slot, claims("omp/tests", Precedence::CORE))
		.unwrap();
	let (_feed, params) = IncomingParams::channel();

	let events = block_on(
		registry
			.invoke("aborting_fake", params)
			.unwrap()
			.collect::<Vec<_>>(),
	);
	let [Ok(ErasedEv::Done(ErasedOutcome::Done { verdict, useless: false }))] = events.as_slice()
	else {
		panic!("Aborted must be the sole erased terminal event: {events:?}")
	};
	let verdict: CallOutcome<FakePayload, FakeFault> = serde_json::from_slice(verdict).unwrap();
	assert_eq!(verdict, CallOutcome::aborted(Abort::Skipped { reason: Str::from("policy denied") }));
}

#[test]
fn advertisement_contains_only_the_live_schema_and_preserves_supported_grammar() {
	let calls = Arc::new(AtomicUsize::new(0));
	let mut registry = Registry::new();
	registry
		.register(
			FakeTool::new(
				1,
				"old",
				br#"{"type":"object","properties":{"old":{"type":"boolean"}}}"#,
				Constraint::None,
				Arc::clone(&calls),
			),
			Presentation::Slot,
			claims("omp/tests", Precedence::CORE),
		)
		.unwrap();
	registry
		.register(
			FakeTool::new(
				2,
				"live",
				br#"{"type":"object","properties":{"live":{"const":true}},"required":["live"]}"#,
				Constraint::Grammar {
					syntax:         GrammarSyntax::Regex,
					definition:     Str::from(r"live=(true|false)"),
					priority:       7,
					on_unsupported: Fallback::Unspecified,
				},
				calls,
			),
			Presentation::Slot,
			claims("omp/tests", Precedence::CORE),
		)
		.unwrap();

	let advertised = registry
		.advertise(LoweringCaps { strict_schema: false, grammar: GrammarBits::REGEX })
		.unwrap();
	let [tool] = advertised.as_slice() else {
		panic!("historical revisions must not be advertised")
	};
	assert_eq!(tool.identity, identity(2));
	assert_eq!(tool.definition.name, "typed_fake");
	assert_eq!(tool.definition.description.as_deref(), Some("fake revision 2"));
	let grammar = tool
		.definition
		.input
		.grammar()
		.expect("supported grammar remains native");
	assert_eq!(grammar.syntax, ToolGrammarSyntax::Regex);
	assert_eq!(grammar.definition, r"live=(true|false)");
	assert_eq!(tool.disposition, Some(ConstraintDisposition::Required));
	assert_eq!(tool.priority, Some(7));
	assert_eq!(tool.adjustments, [] as [omp_llm_inference::Adjustment; 0]);
}

#[test]
fn live_identity_and_advertisement_are_the_same_exact_revision() {
	let calls = Arc::new(AtomicUsize::new(0));
	let mut registry = Registry::new();
	registry
		.register(
			FakeTool::new(
				1,
				"historical",
				br#"{"type":"object","properties":{"hl1_only":{"const":true}}}"#,
				Constraint::None,
				Arc::clone(&calls),
			),
			Presentation::Slot,
			claims("omp/tests", Precedence::CORE),
		)
		.unwrap();
	registry
		.register(
			FakeTool::new(
				2,
				"live",
				br#"{"type":"object","properties":{"hl2_only":{"const":true}}}"#,
				Constraint::None,
				calls,
			),
			Presentation::Slot,
			claims("omp/tests", Precedence::CORE),
		)
		.unwrap();

	let (name, revision) = registry
		.live_identity("typed_fake")
		.expect("registered live identity");
	let [advertised] = registry
		.advertise(LoweringCaps { strict_schema: false, grammar: GrammarBits::empty() })
		.unwrap()
		.try_into()
		.expect("only one live definition");
	assert_eq!(name, &advertised.identity.name);
	assert_eq!(revision, &advertised.identity.rev);
	assert_eq!(revision.to_string(), "fake.2");
	let (schema, _) = advertised
		.definition
		.input
		.json_schema()
		.expect("unconstrained tool lowers to JSON Schema");
	let schema_bytes = serde_json::to_vec(schema.as_value()).expect("schema serializes");
	assert!(
		schema_bytes
			.windows(b"hl2_only".len())
			.any(|window| window == b"hl2_only")
	);
	assert!(
		!schema_bytes
			.windows(b"hl1_only".len())
			.any(|window| window == b"hl1_only")
	);
}

#[test]
fn unsupported_grammar_degrades_to_live_lenient_schema_with_a_receipt() {
	let live_schema = json!({
		"type": "object",
		"properties": {"live": {"const": true}},
		"required": ["live"]
	});
	let mut registry = Registry::new();
	registry
		.register(
			FakeTool::new(
				1,
				"old",
				br#"{"type":"object","properties":{"obsolete":{"type":"string"}}}"#,
				Constraint::None,
				Arc::new(AtomicUsize::new(0)),
			),
			Presentation::Slot,
			claims("omp/tests", Precedence::CORE),
		)
		.unwrap();
	registry
		.register(
			FakeTool::new(
				2,
				"live",
				br#"{"type":"object","properties":{"live":{"const":true}},"required":["live"]}"#,
				Constraint::Grammar {
					syntax:         GrammarSyntax::Ebnf,
					definition:     Str::from("root = 'live';"),
					priority:       11,
					on_unsupported: Fallback::Unspecified,
				},
				Arc::new(AtomicUsize::new(0)),
			),
			Presentation::Slot,
			claims("omp/tests", Precedence::CORE),
		)
		.unwrap();

	let [tool] = registry
		.advertise(LoweringCaps { strict_schema: true, grammar: GrammarBits::empty() })
		.unwrap()
		.try_into()
		.expect("one live tool");
	assert_eq!(tool.identity, identity(2));
	let (schema, strict) = tool
		.definition
		.input
		.json_schema()
		.expect("unsupported grammar falls back to JSON Schema");
	assert_eq!(schema.as_value(), &live_schema);
	assert!(!strict, "grammar fallback must remain non-strict even when strict schema is available");
	assert_eq!(tool.disposition, Some(ConstraintDisposition::Prefer));
	assert_eq!(tool.priority, Some(11));
	assert_eq!(tool.adjustments.len(), 1);
	assert!(matches!(
		&tool.adjustments[0],
		Adjustment::Dropped { feature, reason }
			if feature.0 == "tool.typed_fake.ebnf" && reason.0 == "catalog.grammar-unsupported"
	));
}

#[test]
fn pull_validates_only_the_requested_value_and_ignores_unknown_malformed_json() {
	let raw = r#"{"wanted":7,"unknown":[}"#;
	let (feed, mut params) = IncomingParams::channel();
	feed.args_committed(Str::from(raw)).unwrap();

	let wanted = block_on(params.pull(|mut doc| async move {
		let root = doc.json();
		let mut object = root.object();
		let mut value = object.key("wanted");
		value.number().await
	}))
	.expect("an unknown unpulled sibling cannot fail the requested pull");
	assert_eq!(wanted.as_f64(), 7.0);
	assert_eq!(block_on(params.committed()).unwrap(), raw);
}

#[test]
fn pulled_type_failure_is_a_structured_argument_issue() {
	let raw = r#"{"wanted":"seven","unknown":[}"#;
	let (feed, mut params) = IncomingParams::channel();
	feed.args_committed(Str::from(raw)).unwrap();

	let error = block_on(params.pull(|mut doc| async move {
		let root = doc.json();
		let mut object = root.object();
		let mut value = object.key("wanted");
		value.number().await
	}))
	.expect_err("the requested number has the wrong shape");
	let ParamError::Args(issue) = error else {
		panic!("pull failures must retain their structured argument issue")
	};
	assert_eq!(issue.path, vec![ArgPath::Key(Str::from("wanted"))]);
	assert_eq!(issue.kind, ArgIssueKind::TypeMismatch);
	assert_eq!(issue.expected, "number");
	assert_eq!(issue.found.as_deref(), Some("string"));
}

#[test]
fn commitment_is_explicit_and_feed_guard_drop_aborts() {
	let (feed, mut committed) = IncomingParams::channel();
	feed.arg_text(Str::from("{value:1}")).unwrap();
	feed.args_committed(Str::from("{value:1}")).unwrap();
	assert_eq!(block_on(committed.committed()).unwrap(), "{value:1}");

	let (guard, mut abandoned) = IncomingParams::channel();
	guard.arg_text(Str::from("{value:")).unwrap();
	drop(guard);
	assert!(matches!(block_on(abandoned.committed()), Err(CommitError::Aborted)));
}
#[test]
fn post_commit_interrupt_wait_preserves_reason_and_reports_owner_drop() {
	let (feed, mut params) = IncomingParams::channel();
	feed.args_committed(Str::from("{}")).unwrap();
	assert_eq!(block_on(params.committed()).unwrap(), "{}");
	let expected =
		Interrupt { class: Str::from("immediate"), reason: Str::from("steering changed") };
	feed.interrupt(expected.clone()).unwrap();
	assert_eq!(block_on(params.next_interrupt()).unwrap(), expected);

	drop(feed);
	assert!(matches!(block_on(params.next_interrupt()), Err(InterruptWaitError::Closed)));
}

#[test]
fn prompt_projection_is_exact_and_deterministic_for_the_same_input() {
	let mut registry = Registry::new();
	registry
		.register(
			fake_tool(1, "renderer", Arc::new(AtomicUsize::new(0))),
			Presentation::Slot,
			claims("omp/tests", Precedence::CORE),
		)
		.unwrap();
	let verdict = serde_json::to_vec(&CallOutcome::<FakePayload, FakeFault>::Ok(FakePayload {
		implementation: Str::from("engine"),
		raw:            Str::from("{value:9}"),
	}))
	.unwrap();
	let live = identity(1);
	let caps = PromptCaps::for_tool(
		CapsBase {
			maximum_parts:      3,
			maximum_text_bytes: 256,
			media:              true,
			model_class:        ModelClass::Standard,
		},
		&live.rev,
	);

	let first = registry
		.prompt(&identity(1), &verdict, &caps)
		.unwrap()
		.unwrap();
	let second = registry
		.prompt(&identity(1), &verdict, &caps)
		.unwrap()
		.unwrap();
	assert_eq!(first, second);
	assert_eq!(first, vec![
		Part::Text { text: Str::from(format!("renderer|ok:engine:{}|3/256/true", "{value:9}")) },
		Part::Json { json: Bytes::from_static(br#""ok:engine:{value:9}""#) },
	]);
}

#[test]
fn all_adjacent_lifts_compose_to_the_live_revision_byte_identically() {
	let mut registry = Registry::new();
	registry
		.register(
			fake_tool(1, "one", Arc::new(AtomicUsize::new(0))),
			Presentation::Slot,
			claims("omp/tests", Precedence::CORE),
		)
		.unwrap();
	registry
		.register(
			fake_tool(2, "two", Arc::new(AtomicUsize::new(0))).lifting_from(1),
			Presentation::Slot,
			claims("omp/tests", Precedence::CORE),
		)
		.unwrap();
	registry
		.register(
			fake_tool(3, "three", Arc::new(AtomicUsize::new(0))).lifting_from(2),
			Presentation::Slot,
			claims("omp/tests", Precedence::CORE),
		)
		.unwrap();
	let original = RecordedCallOwned {
		identity: identity(1),
		raw_args: Bytes::from_static(b"raw"),
		verdict:  Bytes::from_static(b"verdict"),
	};

	let first = registry.project(original.clone());
	let second = registry.project(original);
	assert_eq!(first, second, "same projection inputs must produce identical bytes");
	assert_eq!(
		first,
		ProjectedCall::Live(RecordedCallOwned {
			identity: identity(3),
			raw_args: Bytes::from_static(b"raw>2>3"),
			verdict:  Bytes::from_static(b"verdict>2>3"),
		})
	);
}

#[test]
fn incomplete_lift_chain_preserves_the_exact_original_as_data() {
	let mut registry = Registry::new();
	registry
		.register(
			fake_tool(1, "one", Arc::new(AtomicUsize::new(0))),
			Presentation::Slot,
			claims("omp/tests", Precedence::CORE),
		)
		.unwrap();
	registry
		.register(
			fake_tool(3, "three", Arc::new(AtomicUsize::new(0))).lifting_from(2),
			Presentation::Slot,
			claims("omp/tests", Precedence::CORE),
		)
		.unwrap();
	let original = RecordedCallOwned {
		identity: identity(1),
		raw_args: Bytes::from_static(b"{ not rewritten "),
		verdict:  Bytes::from_static(b"opaque verdict bytes\0\xff"),
	};

	assert_eq!(registry.project(original.clone()), ProjectedCall::Data(original));
}

struct RecordingSpill {
	tx: flume::Sender<Bytes>,
	rx: flume::Receiver<Bytes>,
}

impl RecordingSpill {
	fn new() -> Self {
		let (tx, rx) = flume::unbounded();
		Self { tx, rx }
	}
}

impl CallOutcomeSpill for RecordingSpill {
	type Error = Infallible;

	fn spill(&self, json: Bytes) -> impl Future<Output = Result<BlobRef, Self::Error>> + Send + '_ {
		self
			.tx
			.send(json.clone())
			.expect("test receiver remains live");
		ready(Ok(BlobRef {
			hash:       Str::from("sha256:fake"),
			media_type: Str::from("application/json"),
			byte_len:   json.len() as u64,
		}))
	}
}

#[test]
fn call_outcome_spill_hook_runs_only_beyond_the_inline_boundary_with_exact_bytes() {
	let verdict = CallOutcome::<FakePayload, FakeFault>::Ok(FakePayload {
		implementation: Str::from("engine"),
		raw:            Str::from("{value:5}"),
	});
	let expected = Bytes::from(serde_json::to_vec(&verdict).unwrap());
	let spill = RecordingSpill::new();

	let inline = block_on(call_outcome_details(&verdict, expected.len(), &spill)).unwrap();
	assert_eq!(inline, CallOutcomeDetails::Inline { json: expected.clone() });
	assert!(spill.rx.try_recv().is_err());

	let spilled = block_on(call_outcome_details(&verdict, expected.len() - 1, &spill)).unwrap();
	assert_eq!(spilled, CallOutcomeDetails::Spilled {
		blob:     BlobRef {
			hash:       Str::from("sha256:fake"),
			media_type: Str::from("application/json"),
			byte_len:   expected.len() as u64,
		},
		byte_len: expected.len() as u64,
	});
	assert_eq!(spill.rx.try_recv().unwrap(), expected);
	assert!(spill.rx.try_recv().is_err());
}

#[test]
fn detached_artifact_lifetime_is_explicit_and_session_is_the_conservative_default() {
	assert_eq!(ArtifactLifetime::default(), ArtifactLifetime::Session);

	for (lifetime, encoded) in [
		(ArtifactLifetime::Ephemeral, "ephemeral"),
		(ArtifactLifetime::Session, "session"),
		(ArtifactLifetime::Durable, "durable"),
	] {
		let job = JobRef {
			id:       Str::from("job-7"),
			owner:    JobOwner::NamedProcess { name: Str::from("render"), generation: 3 },
			artifact: ExpectedArtifact {
				description: Str::from("rendered video"),
				media_type: Some(Str::from("video/mp4")),
				lifetime,
			},
		};
		let value = serde_json::to_value(&job).expect("job reference serializes");
		assert_eq!(value["artifact"]["lifetime"], encoded);
		assert_eq!(
			serde_json::from_value::<JobRef>(value).expect("explicit lifetime deserializes"),
			job
		);
	}

	assert!(
		serde_json::from_value::<JobRef>(json!({
			"id": "job-7",
			"owner": {
				"kind": "named_process",
				"name": "render",
				"generation": 3
			},
			"artifact": {
				"description": "rendered video",
				"media_type": "video/mp4"
			}
		}))
		.is_err(),
		"wire descriptors must carry an explicit lifetime"
	);
	assert!(
		serde_json::from_value::<JobRef>(json!({
			"id": "job-7",
			"artifact": {
				"description": "rendered video",
				"media_type": "video/mp4",
				"lifetime": "session"
			}
		}))
		.is_err(),
		"wire job references must carry an explicit resource owner"
	);
}
