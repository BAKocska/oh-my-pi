//! The fixed `dyn` device-transport grammar and catalog rendering.
//!
//! This module intentionally owns the envelope vocabulary. A device never
//! interprets `do_`: after finalization [`renest_args`] produces the exact
//! device argument object passed to its resolved route.

use std::{
	collections::BTreeMap,
	sync::{
		Arc, OnceLock, Weak,
		atomic::{AtomicU64, Ordering},
	},
};

use async_stream::stream;
use bytes::Bytes;
use futures::{Stream, StreamExt as _};
use omp_core::Str;
use omp_slopjson::{Object, Value};
use omp_tool::{
	Abort, ArgIssue, ArgIssueKind, ArgPath, Constraint, DeviceIssue, DevicePath, DeviceTarget,
	ErasedEv, ErasedOutcome, IncomingParams, MountedDevice, ParamError, Part, PromptCaps, Registry,
	Rev, Tool, ToolIdentity, ToolRoute, ToolSpec, ToolTerminal, ToolsPolicy, Verdict,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value as SchemaValue;

/// The three operations accepted by the stable `dyn` schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Operation {
	/// Render the device catalog, optionally below a subtree.
	Search(Option<Str>),
	/// Render the documentation for one resolved device path.
	Docs(Str),
	/// Resolve and invoke one device path.
	Invoke(Str),
}

/// A deterministic `tool_only` flattening collision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FlattenCollision {
	/// First claimant to occupy the flattened slot.
	pub first:  Str,
	/// Conflicting claimant.
	pub second: Str,
	/// Model-facing flattened slot spelling.
	pub slot:   Str,
}

/// Flattens device paths for `tool_only`, rejecting collisions fail-closed.
pub fn flatten_slots(
	paths: impl IntoIterator<Item = (Str, Str)>,
) -> Result<BTreeMap<Str, Str>, FlattenCollision> {
	let mut slots = BTreeMap::new();
	for (path, claimant) in paths {
		let slot = Str::from(path.as_str().replace('/', "_"));
		if let Some(first) = slots.insert(slot.clone(), claimant.clone()) {
			return Err(FlattenCollision { first, second: claimant, slot });
		}
	}
	Ok(slots)
}

/// Whether the stable `dyn` slot is present under `policy`.
#[must_use]
pub const fn dyn_enabled(policy: ToolsPolicy) -> bool {
	!matches!(policy, ToolsPolicy::ToolOnly)
}

/// Returns a parameter forbidden by the stable flat `dyn` envelope, if any.
#[must_use]
pub fn reserved_parameter(schema: &SchemaValue) -> Option<Str> {
	let properties = schema.get("properties")?.as_object()?;
	properties
		.keys()
		.find(|name| name.as_str() == "do_" || name.ends_with('_'))
		.map(|name| Str::from(name.as_str()))
}

/// Late-bound immutable registry access for the self-referential `dyn` slot.
///
/// The registry must first register `dyn`, then be frozen in an [`Arc`] and
/// bound exactly once. The catalog retains only a weak reference, so registry
/// assembly does not create an ownership cycle.
#[derive(Clone, Default)]
pub struct DeviceCatalog(Arc<OnceLock<Weak<Registry>>>);

impl DeviceCatalog {
	/// Binds the completed immutable registry once.
	pub fn bind(&self, registry: Arc<Registry>) -> Result<(), Weak<Registry>> {
		self.0.set(Arc::downgrade(&registry))
	}

	fn registry(&self) -> Option<Arc<Registry>> {
		self.0.get()?.upgrade()
	}
}

/// Fully resolved final device invocation handed to the environment route.
pub struct DeviceInvokeRequest {
	/// Address used by the model.
	pub path:          DevicePath,
	/// Resolved device name.
	pub name:          Str,
	/// Resolved revision.
	pub rev:           Str,
	/// Owning extension claimant when worker-routed.
	pub claimant:      Option<Str>,
	/// Placed worker site when worker-routed.
	pub site:          Option<omp_tool::WorkerSiteKind>,
	/// Placed worker name when worker-routed.
	pub worker:        Option<Str>,
	/// Environment invocation identity.
	pub invocation_id: Str,
	/// Execution deadline.
	pub deadline:      omp_core::Duration,
	/// Final re-nested device arguments.
	pub args_json:     Bytes,
}

/// Environment-owned dispatch bridge for a resolved device route.
///
/// Both native and worker routes yield the registry's existing erased stream;
/// the router only supplies final re-nested arguments and never observes
/// speculative fragments.
pub trait DeviceInvoker: Send + Sync {
	/// Dispatches one resolved device with its final, re-nested JSON bytes.
	fn invoke(
		&self,
		request: DeviceInvokeRequest,
	) -> impl Future<Output = omp_tool::ErasedStream<'static>> + Send;
}

/// A malformed `do_` envelope before a target can be resolved.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperationError {
	/// The envelope did not contain an operation.
	Empty,
	/// A path-taking operation was missing its path.
	MissingPath {
		/// Operation spelling.
		op: Str,
	},
	/// The envelope named an operation outside the stable vocabulary.
	Unknown {
		/// Received operation spelling.
		op: Str,
	},
}

impl OperationError {
	/// The fixed valid-op listing used by the structured fault projection.
	#[must_use]
	pub const fn valid_ops() -> &'static [&'static str] {
		&["search", "docs", "invoke"]
	}
}

/// Parses the path-bearing `do_` grammar without normalizing claimant paths.
///
/// An empty path and a trailing slash are distinct malformed envelopes; they
/// never fall through to registry lookup and therefore cannot be mistaken for
/// an unknown device.
pub fn parse_operation(value: &str) -> Result<Operation, OperationError> {
	let Some((op, path)) = value.split_once('/') else {
		return match value {
			"" => Err(OperationError::Empty),
			"search" => Ok(Operation::Search(None)),
			"docs" | "invoke" => Err(OperationError::MissingPath { op: Str::from(value) }),
			_ => Err(OperationError::Unknown { op: Str::from(value) }),
		};
	};
	match op {
		"search" => {
			if path.is_empty() {
				Err(OperationError::MissingPath { op: Str::from(op) })
			} else {
				Ok(Operation::Search(Some(Str::from(path))))
			}
		},
		"docs" | "invoke" if path.is_empty() || path.ends_with('/') => {
			Err(OperationError::MissingPath { op: Str::from(op) })
		},
		"docs" => Ok(Operation::Docs(Str::from(path))),
		"invoke" => Ok(Operation::Invoke(Str::from(path))),
		_ => Err(OperationError::Unknown { op: Str::from(op) }),
	}
}

/// Converts a malformed `do_` envelope to the schema-echoing structured issue.
#[must_use]
pub fn operation_issue(error: OperationError) -> DeviceIssue {
	let expected = match &error {
		OperationError::Empty => "one of search, docs/<path>, invoke/<path>",
		OperationError::MissingPath { .. } => "a non-empty device path",
		OperationError::Unknown { .. } => "one of search, docs, invoke",
	};
	ArgIssue {
		path:     vec![ArgPath::Key(Str::new_static("do_"))],
		expected: Str::from(expected),
		kind:     ArgIssueKind::Malformed,
		example:  Some(Str::new_static(r#"{"do_":"search"}"#)),
		found:    None,
	}
}

/// Resolves a path-taking operation through the registry's sole device lookup.
///
/// Callers map the returned [`DeviceIssue`] into `Verdict::Device`, retaining
/// the path and resolved revision for schema-echo projection.
pub fn resolve_operation<'a>(
	registry: &'a Registry,
	operation: &Operation,
) -> Result<(DevicePath, DeviceTarget<'a>), DeviceIssue> {
	let raw = match operation {
		Operation::Docs(path) | Operation::Invoke(path) => path,
		Operation::Search(_) => {
			return Err(operation_issue(OperationError::MissingPath {
				op: Str::new_static("search"),
			}));
		},
	};
	let path = DevicePath::parse(raw.as_str())
		.map_err(|_| operation_issue(OperationError::MissingPath { op: Str::new_static("path") }))?;
	let target = registry.resolve_device(&path)?;
	Ok((path, target))
}

/// Re-nests finalized flat `dyn` arguments into one device argument document.
///
/// `do_` is always transport metadata. The habitual core-tool intent field
/// `i` is retained only when the resolved device schema explicitly declares it.
pub fn renest_args(flat: &Object, declares_intent: bool) -> Result<Bytes, serde_json::Error> {
	let mut args = Object::with_capacity(flat.len());
	for (key, value) in flat {
		if key == "do_" || (key == "i" && !declares_intent) {
			continue;
		}
		args.insert(key.clone(), value.clone());
	}
	Ok(Bytes::from(args.to_string()))
}

/// Renders the deterministic live catalog fragment used for discovery and
/// unknown-path repairs.
pub fn render_catalog<'a>(devices: impl Iterator<Item = MountedDevice<'a>>) -> Str {
	let mut rendered = String::new();
	for device in devices {
		rendered.push_str(device.name);
		rendered.push_str(" — ");
		rendered.push_str(device.summary);
		rendered.push_str(" @ ");
		rendered.push_str(device.claimant);
		rendered.push('\n');
	}
	Str::from(rendered)
}

/// Renders a bounded nearest-match fragment from deterministic catalog rows.
#[must_use]
pub fn render_near_miss<'a>(path: &str, devices: impl Iterator<Item = MountedDevice<'a>>) -> Str {
	let needle = path.rsplit('/').next().unwrap_or(path);
	let mut scored = BTreeMap::<(u8, Str), (&Str, &Str)>::new();
	for device in devices {
		let name = device.name.as_str();
		let leaf = name.rsplit('/').next().unwrap_or(name);
		let distance = levenshtein(needle, leaf).min(u8::MAX as usize) as u8;
		scored.insert((distance, device.name.clone()), (device.name, device.summary));
	}
	let mut rendered = String::from("Nearest:\n");
	for (_, (name, summary)) in scored.into_iter().take(5) {
		rendered.push_str("  ");
		rendered.push_str(name);
		rendered.push_str(" — ");
		rendered.push_str(summary);
		rendered.push('\n');
	}
	Str::from(rendered)
}

#[derive(Clone)]
struct CatalogCache {
	hash:     [u8; 32],
	rendered: Str,
}

/// One opaque update forwarded from an invoked device.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DynUpdate {
	/// Exact serialized target update frame.
	pub json: Bytes,
}

/// Durable `dyn` result retaining either catalog text or a target verdict.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum DynPayload {
	/// Rendered catalog or documentation text.
	Text {
		/// Rendered text.
		text: Str,
	},
	/// A terminal result forwarded from a resolved device.
	Invocation {
		/// Resolved target identity used for deterministic projection.
		identity: ToolIdentity,
		/// Exact serialized target verdict.
		verdict:  Bytes,
	},
}

struct DynTool<I> {
	invoker:            I,
	catalog:            DeviceCatalog,
	catalog_cache:      Mutex<Option<CatalogCache>>,
	next_invocation_id: AtomicU64,
	spec:               ToolSpec,
}

/// Constructs the stable `dyn` dynamic-device transport.
///
/// `catalog` is bound after registry assembly with [`DeviceCatalog::bind`].
pub fn dyn_tool<I: DeviceInvoker + 'static>(
	invoker: I,
	catalog: DeviceCatalog,
	_policy: ToolsPolicy,
) -> impl Tool<Params = Value, Update = DynUpdate, Payload = DynPayload, Fault = Verdict> {
	DynTool {
		invoker,
		catalog,
		catalog_cache: Mutex::new(None),
		next_invocation_id: AtomicU64::new(0),
		spec: ToolSpec {
			name:            Str::new_static("dyn"),
			rev:             Rev { family: Str::default(), n: 1 },
			description:     Str::new_static(
				"Discover documented devices and invoke one through the stable dynamic transport.",
			),
			schema:          Bytes::from_static(
				br#"{"type":"object","properties":{"do_":{"type":"string","description":"search, docs/<path>, or invoke/<path>"}},"required":["do_"],"additionalProperties":true}"#,
			),
			constraint:      Constraint::Schema {
				priority:       100,
				on_unsupported: omp_tool::Fallback::Unspecified,
			},
			effects:         Default::default(),
			projection_code: omp_tool::native_projection_code(
				env!("CARGO_PKG_NAME"),
				env!("CARGO_PKG_VERSION"),
				include_bytes!("device.rs"),
			),
		},
	}
}

impl<I: DeviceInvoker + 'static> DynTool<I> {
	fn catalog(&self, registry: &Registry, subtree: Option<&Str>) -> Str {
		if let Some(subtree) = subtree {
			return render_catalog(
				registry
					.devices()
					.filter(|device| device.name.as_str().starts_with(subtree.as_str())),
			);
		}
		let hash = registry.device_hash();
		let mut cache = self.catalog_cache.lock();
		if let Some(cached) = cache.as_ref()
			&& cached.hash == hash
		{
			return cached.rendered.clone();
		}
		let rendered = render_catalog(registry.devices());
		*cache = Some(CatalogCache { hash, rendered: rendered.clone() });
		rendered
	}

	fn fault(&self, path: DevicePath, rev: Rev, issue: DeviceIssue) -> Verdict {
		Verdict::Device { path, rev, issue }
	}

	fn dyn_path() -> DevicePath {
		DevicePath::parse("dyn").expect("the fixed dyn path is valid")
	}

	fn unknown_path_fault(&self, raw: &str, registry: &Registry) -> Verdict {
		let path = DevicePath::parse(raw).unwrap_or_else(|_| Self::dyn_path());
		let issue = ArgIssue {
			path:     vec![ArgPath::Key(Str::new_static("do_"))],
			expected: render_near_miss(raw, registry.devices()),
			kind:     ArgIssueKind::Malformed,
			example:  Some(Str::new_static(r#"{"do_":"search"}"#)),
			found:    None,
		};
		self.fault(path, self.spec.rev.clone(), issue)
	}

	fn docs(&self, registry: &Registry, path: &DevicePath, flat: &Object) -> Str {
		let rendered = registry.live_spec(&path.to_string()).map_or_else(
			|_| Str::new_static("The resolved device is no longer mounted."),
			|spec| {
				let mut output = format!("{}\n\n{}\n\nSchema:\n", path, spec.description);
				output.push_str(std::str::from_utf8(&spec.schema).unwrap_or("{}"));
				Str::from(output)
			},
		);
		slice_rendered(&rendered, flat_offset(flat), flat_limit(flat))
	}

	fn next_invocation_id(&self) -> Str {
		let sequence = self.next_invocation_id.fetch_add(1, Ordering::Relaxed);
		Str::from(format!("dyn-{sequence}"))
	}
}

impl<I: DeviceInvoker + 'static> Tool for DynTool<I> {
	type Fault = Verdict;
	type Params = Value;
	type Payload = DynPayload;
	type Update = DynUpdate;

	fn spec(&self) -> &ToolSpec {
		&self.spec
	}

	fn call<'c>(
		&'c self,
		mut params: IncomingParams<'c>,
	) -> impl Stream<Item = omp_tool::Ev<Self::Update, Self::Payload, Self::Fault>> + Send + 'c {
		stream! {
			let finalized = match params.finalize().await {
				Ok(finalized) => finalized,
				Err(error) => {
					yield param_event(error);
					return;
				},
			};
			let Some(flat) = finalized.effective().as_object() else {
				yield omp_tool::Ev::Args(ArgIssue {
					path: vec![], expected: Str::new_static("an argument object"), kind: ArgIssueKind::Malformed,
					example: None, found: None,
				});
				return;
			};
			let operation = match flat.get("do_").and_then(Value::as_str) {
				Some(value) => match parse_operation(value) {
					Ok(operation) => operation,
					Err(error) => {
						yield done(Err(self.fault(Self::dyn_path(), self.spec.rev.clone(), operation_issue(error))));
						return;
					},
				},
				None => {
					yield done(Err(self.fault(Self::dyn_path(), self.spec.rev.clone(), operation_issue(OperationError::Empty))));
					return;
				},
			};
			let Some(registry) = self.catalog.registry() else {
				yield done(Err(self.fault(Self::dyn_path(), self.spec.rev.clone(), operation_issue(OperationError::Unknown { op: Str::new_static("catalog") }))));
				return;
			};
			match operation {
				Operation::Search(subtree) => {
					yield done(Ok(DynPayload::Text { text: self.catalog(&registry, subtree.as_ref()) }));
				},
				Operation::Docs(raw) => {
					let (path, _target) = match resolve_operation(&registry, &Operation::Docs(raw.clone())) {
						Ok(resolved) => resolved,
						Err(_) => {
							yield done(Err(self.unknown_path_fault(raw.as_str(), &registry)));
							return;
						},
					};
					yield done(Ok(DynPayload::Text { text: self.docs(&registry, &path, flat) }));
				},
				Operation::Invoke(raw) => {
					let (path, target) = match resolve_operation(&registry, &Operation::Invoke(raw.clone())) {
						Ok(resolved) => resolved,
						Err(_) => {
							yield done(Err(self.unknown_path_fault(raw.as_str(), &registry)));
							return;
						},
					};
					let declares_intent = registry
						.live_spec(&path.to_string())
						.ok()
						.and_then(|spec| serde_json::from_slice::<SchemaValue>(&spec.schema).ok())
						.and_then(|schema| schema.get("properties").and_then(SchemaValue::as_object).cloned())
						.is_some_and(|properties| properties.contains_key("i"));
					let args_json = match renest_args(flat, declares_intent) {
						Ok(args) => args,
						Err(_) => {
							yield done(Err(self.fault(path, target.rev.clone(), operation_issue(OperationError::MissingPath { op: Str::new_static("arguments") }))));
							return;
						},
					};
					let identity = target.identity();
					let mut events = match target.route {
						ToolRoute::Native => {
							let (feed, nested) = IncomingParams::channel();
							let raw = Str::from(std::str::from_utf8(&args_json).expect("serialized JSON is UTF-8"));
							if feed.args_committed(raw).is_err() {
								yield done(Err(self.unknown_path_fault(path.to_string().as_str(), &registry)));
								return;
							}
							match registry.invoke_device(&path, nested) {
								Ok(events) => events,
								Err(_) => {
									yield done(Err(self.unknown_path_fault(path.to_string().as_str(), &registry)));
									return;
								},
							}
						},
						ToolRoute::Worker { site, name } => self.invoker.invoke(DeviceInvokeRequest {
							path: path.clone(),
							name: target.name.clone(),
							rev: Str::from(target.rev.to_string()),
							claimant: Some(target.claimant.clone()),
							site: Some(*site),
							worker: Some(name.clone()),
							invocation_id: self.next_invocation_id(),
							deadline: omp_core::Duration::new(5, omp_core::DurationUnit::Minutes),
							args_json,
						}).await,
					};
					while let Some(event) = events.next().await {
						match event {
							Ok(ErasedEv::Update(json)) => yield omp_tool::Ev::Update(DynUpdate { json }),
							Ok(ErasedEv::Done(ErasedOutcome::Done { verdict, useless })) => {
								yield omp_tool::Ev::Done(ToolTerminal::Done {
									result: Ok(DynPayload::Invocation { identity, verdict }), useless,
								});
								return;
							},
							Ok(ErasedEv::Done(ErasedOutcome::Detached(job))) => {
								yield omp_tool::Ev::Done(ToolTerminal::Detached(job));
								return;
							},
							Err(_) => {
								yield done(Err(self.unknown_path_fault(path.to_string().as_str(), &registry)));
								return;
							},
						}
					}
					yield omp_tool::Ev::Aborted(Abort::MissingOutcome);
				},
			}
		}
	}

	fn prompt(&self, view: Result<&Self::Payload, &Self::Fault>, caps: &PromptCaps) -> Vec<Part> {
		match view {
			Ok(DynPayload::Text { text }) => vec![Part::Text {
				text: slice_rendered(text, 0, Some(caps.maximum_text_bytes as usize)),
			}],
			Ok(DynPayload::Invocation { identity, verdict }) => self
				.catalog
				.registry()
				.and_then(|registry| registry.prompt(identity, verdict, caps).ok().flatten())
				.map_or_else(
					|| vec![Part::Text { text: Str::new_static("Device result unavailable.") }],
					|parts| parts.to_vec(),
				),
			Err(Verdict::Device { issue, .. }) => vec![Part::Text { text: issue.expected.clone() }],
		}
	}
}

fn done(result: Result<DynPayload, Verdict>) -> omp_tool::Ev<DynUpdate, DynPayload, Verdict> {
	omp_tool::Ev::Done(ToolTerminal::Done { result, useless: false })
}

fn param_event(error: ParamError) -> omp_tool::Ev<DynUpdate, DynPayload, Verdict> {
	match error {
		ParamError::Args(issue) => omp_tool::Ev::Args(*issue),
		ParamError::Interrupted(interrupt) => {
			omp_tool::Ev::Aborted(Abort::Interrupted { reason: interrupt.reason })
		},
		ParamError::Protocol(reason) => omp_tool::Ev::Args(ArgIssue {
			path:     vec![],
			expected: reason,
			kind:     ArgIssueKind::Malformed,
			example:  None,
			found:    None,
		}),
	}
}

fn flat_offset(flat: &Object) -> usize {
	flat
		.get("offset")
		.and_then(Value::as_u64)
		.and_then(|offset| usize::try_from(offset).ok())
		.unwrap_or(0)
}

fn flat_limit(flat: &Object) -> Option<usize> {
	flat
		.get("limit")
		.and_then(Value::as_u64)
		.and_then(|limit| usize::try_from(limit).ok())
}

fn slice_rendered(text: &str, offset: usize, limit: Option<usize>) -> Str {
	let mut start = offset.min(text.len());
	while start != 0 && !text.is_char_boundary(start) {
		start -= 1;
	}
	let mut end = limit.map_or(text.len(), |limit| start.saturating_add(limit).min(text.len()));
	while end != start && !text.is_char_boundary(end) {
		end -= 1;
	}
	Str::from(&text[start..end])
}

fn levenshtein(left: &str, right: &str) -> usize {
	let mut row: Vec<usize> = (0..=right.chars().count()).collect();
	for (left_index, left_char) in left.chars().enumerate() {
		let mut diagonal = row[0];
		row[0] = left_index + 1;
		for (right_index, right_char) in right.chars().enumerate() {
			let above = row[right_index + 1];
			row[right_index + 1] = (row[right_index + 1] + 1)
				.min(row[right_index] + 1)
				.min(diagonal + usize::from(left_char != right_char));
			diagonal = above;
		}
	}
	row[right.chars().count()]
}

#[cfg(test)]
mod tests {
	use std::sync::{Arc, Mutex};

	use async_stream::stream;
	use bytes::Bytes;
	use futures::StreamExt as _;
	use omp_core::Str;
	use omp_slopjson::Value;
	use omp_tool::{
		Claims, Constraint, ErasedEv, ErasedOutcome, Ev, IncomingParams, Precedence, Presentation,
		Registry, Rev, Tool, ToolSpec, ToolTerminal, ToolsPolicy,
	};
	use serde_json::json;

	use super::{
		DeviceCatalog, DeviceInvokeRequest, DeviceInvoker, DynPayload, Operation, OperationError,
		dyn_enabled, dyn_tool, flatten_slots, parse_operation, renest_args, reserved_parameter,
	};

	#[derive(Clone, Default)]
	struct StubInvoker(Arc<Mutex<Vec<DeviceInvokeRequest>>>);

	impl DeviceInvoker for StubInvoker {
		async fn invoke(&self, request: DeviceInvokeRequest) -> omp_tool::ErasedStream<'static> {
			self.0.lock().expect("stub lock").push(request);
			Box::pin(stream! {
				yield Ok(ErasedEv::Done(ErasedOutcome::Done {
					verdict: Bytes::from_static(br#"{"kind":"ok","value":{}}"#),
					useless: false,
				}));
			})
		}
	}

	fn catalog() -> (DeviceCatalog, Arc<Registry>) {
		let catalog = DeviceCatalog::default();
		let mut registry = Registry::default();
		registry
			.register_worker(
				ToolSpec {
					name:            Str::new_static("fixture"),
					rev:             Rev { family: Str::default(), n: 1 },
					description:     Str::new_static("Fixture device."),
					schema:          Bytes::from_static(
						br#"{"type":"object","properties":{"title":{"type":"string"}},"additionalProperties":false}"#,
					),
					constraint:      Constraint::None,
					effects:         Default::default(),
					projection_code: [0; 32],
				},
				Presentation::Device,
				Claims {
					precedence: Precedence::DEFAULT,
					claimant:   Str::new_static("test/fixture"),
					replaces:   None,
				},
			)
			.expect("fixture registers");
		let registry = Arc::new(registry);
		catalog.bind(Arc::clone(&registry)).expect("catalog binds");
		(catalog, registry)
	}

	async fn invoke<T>(
		tool: &T,
		args: &str,
	) -> Vec<Ev<super::DynUpdate, DynPayload, omp_tool::Verdict>>
	where
		T: Tool<
				Params = Value,
				Update = super::DynUpdate,
				Payload = DynPayload,
				Fault = omp_tool::Verdict,
			>,
	{
		let (feed, params) = IncomingParams::channel();
		feed
			.args_committed(Str::from(args))
			.expect("arguments commit");
		tool.call(params).collect().await
	}
	#[test]
	fn do_grammar_refuses_empty_and_trailing_paths() {
		assert_eq!(parse_operation(""), Err(OperationError::Empty));
		assert!(matches!(parse_operation("docs/"), Err(OperationError::MissingPath { .. })));
		assert!(matches!(parse_operation("invoke/jira/"), Err(OperationError::MissingPath { .. })));
	}

	#[test]
	fn do_grammar_preserves_claimant_qualified_path() {
		assert_eq!(
			parse_operation("docs/jira/create@acme/tools"),
			Ok(Operation::Docs("jira/create@acme/tools".into()))
		);
		assert!(matches!(parse_operation("list"), Err(OperationError::Unknown { .. })));
	}
	#[tokio::test]
	async fn dyn_search_renders_the_bound_catalog() {
		let (catalog, _registry) = catalog();
		let tool = dyn_tool(StubInvoker::default(), catalog, ToolsPolicy::Auto);
		let events = invoke(&tool, r#"{"do_":"search"}"#).await;
		match events.last().expect("terminal event") {
			Ev::Done(ToolTerminal::Done { result: Ok(DynPayload::Text { text }), .. }) => {
				assert!(text.contains("fixture"));
			},
			event => panic!("unexpected event: {event:?}"),
		}
	}

	#[tokio::test]
	async fn dyn_docs_honors_flat_pagination() {
		let (catalog, _registry) = catalog();
		let tool = dyn_tool(StubInvoker::default(), catalog, ToolsPolicy::Auto);
		let events = invoke(&tool, r#"{"do_":"docs/fixture","offset":0,"limit":7}"#).await;
		match events.last().expect("terminal event") {
			Ev::Done(ToolTerminal::Done { result: Ok(DynPayload::Text { text }), .. }) => {
				assert_eq!(text, "fixture");
			},
			event => panic!("unexpected event: {event:?}"),
		}
	}

	#[tokio::test]
	async fn dyn_invocation_renests_final_arguments_for_worker() {
		let invoker = StubInvoker::default();
		let (catalog, _registry) = catalog();
		let tool = dyn_tool(invoker.clone(), catalog, ToolsPolicy::Auto);
		let events =
			invoke(&tool, r#"{"do_":"invoke/fixture","i":"Calling fixture","title":"Fix"}"#).await;
		assert!(matches!(
			events.last(),
			Some(Ev::Done(ToolTerminal::Done { result: Ok(DynPayload::Invocation { .. }), .. }))
		));
		let calls = invoker.0.lock().expect("stub lock");
		assert_eq!(calls.len(), 1);
		assert_eq!(calls[0].args_json.as_ref(), br#"{"title":"Fix"}"#);
	}

	#[tokio::test]
	async fn dyn_unknown_operation_returns_a_device_fault() {
		let (catalog, _registry) = catalog();
		let tool = dyn_tool(StubInvoker::default(), catalog, ToolsPolicy::Auto);
		let events = invoke(&tool, r#"{"do_":"list"}"#).await;
		match events.last().expect("terminal event") {
			Ev::Done(ToolTerminal::Done {
				result: Err(omp_tool::Verdict::Device { issue, .. }),
				..
			}) => assert_eq!(issue.expected, "one of search, docs, invoke"),
			event => panic!("unexpected event: {event:?}"),
		}
	}

	#[test]
	fn near_miss_distance_prefers_the_leaf_typo() {
		assert!(
			super::levenshtein("hose_lint", "house_lint") < super::levenshtein("hose_lint", "jira")
		);
	}
	#[test]
	fn renesting_removes_transport_fields() {
		let flat =
			omp_slopjson::parse(r#"{"do_":"invoke/jira/create","i":"Creating issue","title":"Fix"}"#)
				.expect("flat arguments parse");
		let flat = flat.as_object().expect("flat arguments are an object");
		assert_eq!(renest_args(flat, false).unwrap().as_ref(), br#"{"title":"Fix"}"#);
		assert_eq!(
			renest_args(flat, true).unwrap().as_ref(),
			br#"{"i":"Creating issue","title":"Fix"}"#
		);
	}

	#[test]
	fn tool_only_flattening_refuses_collisions() {
		let collision = flatten_slots([
			("jira/create".into(), "acme/jira".into()),
			("jira_create".into(), "other/tools".into()),
		])
		.unwrap_err();
		assert_eq!(collision.slot, "jira_create");
		assert_eq!(collision.first, "acme/jira");
		assert_eq!(collision.second, "other/tools");
		assert!(!dyn_enabled(ToolsPolicy::ToolOnly));
	}

	#[test]
	fn reserved_envelope_parameters_are_refused() {
		assert_eq!(reserved_parameter(&json!({"properties": {"do_": {}}})), Some("do_".into()));
		assert_eq!(
			reserved_parameter(&json!({"properties": {"future_": {}}})),
			Some("future_".into())
		);
		assert_eq!(reserved_parameter(&json!({"properties": {"title": {}}})), None);
	}
}
