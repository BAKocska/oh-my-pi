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
use omp_core::{Hash32, Str, sf};
use omp_slopjson::{Object, Value};
use omp_tool::{
	Abort, ArgIssue, ArgIssueKind, ArgPath, Constraint, DeviceIssue, DevicePath, DeviceTarget,
	ErasedEv, ErasedOutcome, IncomingParams, MountedDevice, ParamError, Part, PromptCaps, Registry,
	Rev, Tool, ToolIdentity, ToolRoute, ToolSpec, ToolTerminal, ToolsPolicy, Verdict,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value as SchemaValue;
use smallvec::SmallVec;
/// Aggregate character budget for documentation inlined into a prompt.
pub const DOCS_TOTAL_BUDGET: usize = 48_000;
/// Per-device character cap for documentation inlined into a prompt.
pub const PER_DEVICE_DOCS_CAP: usize = 10_000;
/// UTF-8 byte cap for third-party catalog summaries.
pub const EXTERNAL_SUMMARY_CAP: usize = 200;

/// Stable model-facing guidance for the live dynamic-device transport.
pub const PROMPT_GUIDANCE: &str = "\
`dyn` exposes only the live device catalog. Use `search` to discover, `docs/<path>` for the exact \
                                   schema and guidance, and `invoke/<path>` with that schema to \
                                   call a device. Retry an empty or narrow search with different \
                                   terms; absent devices are unavailable and MUST NOT be \
                                   advertised or guessed.";

/// Conditional model-facing guidance for the mounted AutoQA recorder.
pub const AUTO_QA_PROMPT_GUIDANCE: &str = "\
Automated QA reporting is available through the live `report_issue` device. When a device result \
                                           contradicts its documented behavior for the supplied \
                                           parameters, read `docs/report_issue`, then invoke \
                                           `report_issue` with a concise evidence-backed verdict. \
                                           False positives are acceptable.";

/// How much dynamic-device documentation is inlined into a prompt.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DocsMode {
	/// Render one bounded catalog line per device.
	#[default]
	Catalog,
	/// Inline full documentation for harness-owned devices only.
	Builtins,
	/// Inline full documentation for devices selected by the allowlist.
	Inline,
}

/// Stable search controls for the dynamic-device catalog.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CatalogQuery {
	/// Case-insensitive text matched against path, summary, provenance, and
	/// tags.
	pub text:       Option<Str>,
	/// Tags every result must have.
	pub tags:       SmallVec<Str, 4>,
	/// Case-insensitive claimant/provenance filter.
	pub provenance: Option<Str>,
	/// Number of matched rows to skip.
	pub offset:     usize,
	/// Maximum rows to return.
	pub limit:      Option<usize>,
	/// Maximum path depth relative to the searched subtree.
	pub depth:      Option<usize>,
}

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
		let slot = Str::new(path.as_str().replace('/', "_"));
		if let Some(first) = slots.insert(slot.clone(), claimant.clone()) {
			return Err(FlattenCollision { first, second: claimant, slot });
		}
	}
	Ok(slots)
}

/// Whether the stable `dyn` slot is present under `policy`.
pub const fn dyn_enabled(policy: ToolsPolicy) -> bool {
	!matches!(policy, ToolsPolicy::ToolOnly)
}

/// Returns a parameter forbidden by the stable flat `dyn` envelope, if any.
pub fn reserved_parameter(schema: &SchemaValue) -> Option<Str> {
	let properties = schema.get("properties")?.as_object()?;
	properties
		.keys()
		.find(|name| name.as_str() == "do_" || name.ends_with('_'))
		.map(|name| Str::new(name.as_str()))
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
			"docs" | "invoke" => Err(OperationError::MissingPath { op: Str::new(value) }),
			_ => Err(OperationError::Unknown { op: Str::new(value) }),
		};
	};
	match op {
		"search" => {
			if path.is_empty() {
				Err(OperationError::MissingPath { op: Str::new(op) })
			} else {
				Ok(Operation::Search(Some(Str::new(path))))
			}
		},
		"docs" | "invoke" if path.is_empty() || path.ends_with('/') => {
			Err(OperationError::MissingPath { op: Str::new(op) })
		},
		"docs" => Ok(Operation::Docs(Str::new(path))),
		"invoke" => Ok(Operation::Invoke(Str::new(path))),
		_ => Err(OperationError::Unknown { op: Str::new(op) }),
	}
}

/// Converts a malformed `do_` envelope to the schema-echoing structured issue.
pub fn operation_issue(error: OperationError) -> DeviceIssue {
	let expected = match &error {
		OperationError::Empty => "one of search, docs/<path>, invoke/<path>",
		OperationError::MissingPath { .. } => "a non-empty device path",
		OperationError::Unknown { .. } => "one of search, docs, invoke",
	};
	ArgIssue {
		path:     vec![ArgPath::Key(sf!("do_"))],
		expected: Str::new(expected),
		kind:     ArgIssueKind::Malformed,
		example:  Some(sf!(r#"{{"do_":"search"}}"#)),
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
			return Err(operation_issue(OperationError::MissingPath { op: sf!("search") }));
		},
	};
	let path = DevicePath::parse(raw.as_str())
		.map_err(|_| operation_issue(OperationError::MissingPath { op: sf!("path") }))?;
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

/// Renders the deterministic live catalog used for discovery.
pub fn render_catalog<'a>(devices: impl Iterator<Item = MountedDevice<'a>>) -> Str {
	render_catalog_query(devices, &CatalogQuery::default(), None)
}

/// Searches and paginates mounted devices with deterministic relevance.
///
/// Text search prefers exact leaves, then path prefixes, path containment,
/// summaries, provenance, and finally tags. Without text, registry order is
/// preserved. `tags` are conjunctive and `provenance` matches the authenticated
/// claimant string.
pub fn render_catalog_query<'a>(
	devices: impl Iterator<Item = MountedDevice<'a>>,
	query: &CatalogQuery,
	subtree: Option<&str>,
) -> Str {
	let mut matched = BTreeMap::<(u8, Str), MountedDevice<'a>>::new();
	for device in devices {
		if !catalog_matches(&device, query, subtree) {
			continue;
		}
		let score = query
			.text
			.as_deref()
			.map_or(0, |text| catalog_score(&device, text));
		if score == u8::MAX {
			continue;
		}
		matched.insert((score, device.name.clone()), device);
	}
	let total = matched.len();
	let offset = query.offset.min(total);
	let take = query.limit.unwrap_or(usize::MAX);
	let mut rendered = String::new();
	for (_, device) in matched.into_iter().skip(offset).take(take) {
		append_catalog_row(&mut rendered, &device);
	}
	let shown = total.saturating_sub(offset).min(take);
	if offset.saturating_add(shown) < total {
		rendered.push_str("More: offset=");
		rendered.push_str(&offset.saturating_add(shown).to_string());
		rendered.push_str(" (");
		rendered.push_str(&total.to_string());
		rendered.push_str(" total)\n");
	}
	Str::new(rendered)
}

/// Renders prompt documentation under the selected inlining mode and budgets.
///
/// `allowlist` accepts `*` and `?` globs over canonical device names. Full
/// blocks that exceed the per-device cap, or the remaining aggregate budget,
/// fall back to their catalog line rather than being cut mid-schema.
pub fn render_prompt_docs<'a>(
	devices: impl Iterator<Item = MountedDevice<'a>>,
	mode: DocsMode,
	allowlist: &[Str],
) -> Str {
	let mut rendered = String::new();
	let mut used_chars: usize = 0;
	for device in devices {
		let inline = match mode {
			DocsMode::Catalog => false,
			DocsMode::Builtins => is_builtin(&device),
			DocsMode::Inline => allowlist
				.iter()
				.any(|pattern| glob_matches(pattern.as_str(), device.name.as_str())),
		};
		let block = inline.then(|| render_device_docs(&device, device.name.as_str()));
		let block = block
			.filter(|block| block.chars().count() <= PER_DEVICE_DOCS_CAP)
			.unwrap_or_else(|| {
				let mut line = String::new();
				append_catalog_row(&mut line, &device);
				line
			});
		let block_chars = block.chars().count();
		if used_chars.saturating_add(block_chars) > DOCS_TOTAL_BUDGET {
			break;
		}
		used_chars += block_chars;
		rendered.push_str(&block);
	}
	Str::new(rendered)
}

/// Renders a bounded nearest-match fragment from deterministic catalog rows.
pub fn render_near_miss<'a>(path: &str, devices: impl Iterator<Item = MountedDevice<'a>>) -> Str {
	let needle = path.rsplit('/').next().unwrap_or(path);
	let mut scored = BTreeMap::<(u8, Str), MountedDevice<'a>>::new();
	for device in devices {
		let leaf = device
			.name
			.as_str()
			.rsplit('/')
			.next()
			.unwrap_or(device.name.as_str());
		let distance = levenshtein(needle, leaf).min(u8::MAX as usize) as u8;
		scored.insert((distance, device.name.clone()), device);
	}
	let mut rendered = String::from("Nearest:\n");
	for (_, device) in scored.into_iter().take(5) {
		rendered.push_str("  ");
		append_catalog_row(&mut rendered, &device);
	}
	Str::new(rendered)
}

fn append_catalog_row(rendered: &mut String, device: &MountedDevice<'_>) {
	rendered.push_str(device.name);
	rendered.push_str(" — ");
	rendered.push_str(&catalog_summary(device));
	let mut first_tag = true;
	for tag in DEVICE_TAGS {
		if has_tag(device, tag) {
			if first_tag {
				rendered.push_str(" [");
				first_tag = false;
			} else {
				rendered.push(',');
			}
			rendered.push_str(tag);
		}
	}
	if !first_tag {
		rendered.push(']');
	}
	rendered.push_str(" @ ");
	rendered.push_str(device.claimant);
	rendered.push('\n');
}

fn render_device_docs(device: &MountedDevice<'_>, path: &str) -> String {
	let mut output = String::new();
	output.push_str(path);
	output.push_str(" @ ");
	output.push_str(device.claimant);
	output.push_str(" — ");
	output.push_str(&catalog_summary(device));
	if let Some(docs) = device.docs.filter(|docs| !docs.trim().is_empty()) {
		output.push_str("\n\n");
		output.push_str(docs);
	}
	output.push_str("\n\nEffects:");
	let mut any = false;
	for tag in DEVICE_TAGS {
		if has_tag(device, tag) {
			output.push(' ');
			output.push_str(tag);
			any = true;
		}
	}
	if !any {
		output.push_str(" none");
	}
	output.push_str("\nProvenance: ");
	output.push_str(device.claimant);
	output.push_str("\nRevision: ");
	output.push_str(&device.rev.to_string());
	output.push_str("\n\nSchema:\n");
	output.push_str(std::str::from_utf8(device.schema).unwrap_or("{}"));
	output.push('\n');
	output
}

const DEVICE_TAGS: &[&str] = &[
	"control",
	"effectful",
	"read",
	"write",
	"exec",
	"net",
	"inference",
	"subagent",
	"builtin",
	"external",
	"native",
	"worker",
];

fn has_tag(device: &MountedDevice<'_>, tag: &str) -> bool {
	match tag {
		"control" => device.effects.is_empty(),
		"effectful" => !device.effects.is_empty(),
		"read" => device
			.effects
			.documents
			.as_ref()
			.is_some_and(|effects| effects.read),
		"write" => device
			.effects
			.documents
			.as_ref()
			.is_some_and(|effects| !effects.write_globs.is_empty()),
		"exec" => device
			.effects
			.exec
			.as_ref()
			.is_some_and(|effects| !effects.commands.is_empty()),
		"net" => device
			.effects
			.exec
			.as_ref()
			.is_some_and(|effects| effects.network),
		"inference" => device
			.effects
			.inference
			.as_ref()
			.is_some_and(|effects| !effects.is_empty()),
		"subagent" => device.effects.subagents != 0,
		"builtin" => is_builtin(device),
		"external" => !is_builtin(device),
		"native" => matches!(device.route, ToolRoute::Native),
		"worker" => matches!(device.route, ToolRoute::Worker { .. }),
		_ => false,
	}
}

fn is_builtin(device: &MountedDevice<'_>) -> bool {
	device.claimant.as_str() == "omp/core"
}

fn catalog_summary(device: &MountedDevice<'_>) -> String {
	let mut summary = String::with_capacity(device.summary.len().min(EXTERNAL_SUMMARY_CAP));
	let mut spacing = false;
	for character in device.summary.chars() {
		if character.is_control() || character.is_whitespace() {
			spacing = !summary.is_empty();
			continue;
		}
		if spacing {
			summary.push(' ');
			spacing = false;
		}
		summary.push(character);
	}
	if is_builtin(device) || summary.len() <= EXTERNAL_SUMMARY_CAP {
		return summary;
	}
	let mut end = EXTERNAL_SUMMARY_CAP.saturating_sub(3).min(summary.len());
	while !summary.is_char_boundary(end) {
		end -= 1;
	}
	summary.truncate(end);
	summary.push_str("...");
	summary
}

fn catalog_matches(
	device: &MountedDevice<'_>,
	query: &CatalogQuery,
	subtree: Option<&str>,
) -> bool {
	if let Some(subtree) = subtree {
		if device.name.as_str() != subtree
			&& !device
				.name
				.as_str()
				.strip_prefix(subtree)
				.is_some_and(|tail| tail.starts_with('/'))
		{
			return false;
		}
		if let Some(depth) = query.depth {
			let relative = device
				.name
				.as_str()
				.strip_prefix(subtree)
				.unwrap_or(device.name.as_str())
				.trim_start_matches('/');
			if !relative.is_empty() && relative.split('/').count() > depth {
				return false;
			}
		}
	} else if let Some(depth) = query.depth
		&& device.name.as_str().split('/').count() > depth
	{
		return false;
	}
	if query
		.tags
		.iter()
		.any(|tag| !has_tag(device, tag.to_ascii_lowercase().as_str()))
	{
		return false;
	}
	if let Some(provenance) = query.provenance.as_deref()
		&& !device
			.claimant
			.to_ascii_lowercase()
			.contains(&provenance.to_ascii_lowercase())
	{
		return false;
	}
	true
}

fn catalog_score(device: &MountedDevice<'_>, text: &str) -> u8 {
	let needle = text.trim().to_ascii_lowercase();
	if needle.is_empty() {
		return 0;
	}
	let name = device.name.to_ascii_lowercase();
	let leaf = name.rsplit('/').next().unwrap_or(&name);
	if leaf == needle || name == needle {
		0
	} else if leaf.starts_with(&needle) || name.starts_with(&needle) {
		1
	} else if name.contains(&needle) {
		2
	} else if device.summary.to_ascii_lowercase().contains(&needle) {
		3
	} else if device.claimant.to_ascii_lowercase().contains(&needle) {
		4
	} else if DEVICE_TAGS
		.iter()
		.any(|tag| has_tag(device, tag) && tag.contains(&needle))
	{
		5
	} else {
		u8::MAX
	}
}

const fn glob_matches(pattern: &str, value: &str) -> bool {
	let pattern = pattern.as_bytes();
	let value = value.as_bytes();
	let (mut pattern_at, mut value_at, mut star, mut retry) = (0, 0, None, 0);
	while value_at < value.len() {
		if pattern_at < pattern.len()
			&& (pattern[pattern_at] == b'?' || pattern[pattern_at] == value[value_at])
		{
			pattern_at += 1;
			value_at += 1;
		} else if pattern_at < pattern.len() && pattern[pattern_at] == b'*' {
			star = Some(pattern_at);
			pattern_at += 1;
			retry = value_at;
		} else if let Some(star_at) = star {
			pattern_at = star_at + 1;
			retry += 1;
			value_at = retry;
		} else {
			return false;
		}
	}
	while pattern_at < pattern.len() && pattern[pattern_at] == b'*' {
		pattern_at += 1;
	}
	pattern_at == pattern.len()
}

#[derive(Clone)]
struct CatalogCache {
	hash:     Hash32,
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
			name:            sf!("dyn"),
			rev:             Rev { family: Str::default(), n: 1 },
			description:     sf!(
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
			)
			.into(),
		},
	}
}

impl<I: DeviceInvoker + 'static> DynTool<I> {
	fn catalog(&self, registry: &Registry, subtree: Option<&Str>, flat: &Object) -> Str {
		let query = catalog_query(flat);
		let bare = subtree.is_none() && query == CatalogQuery::default();
		if !bare {
			return render_catalog_query(
				registry.devices(),
				&query,
				subtree.map(|value| value.as_str()),
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

	const fn fault(&self, path: DevicePath, rev: Rev, issue: DeviceIssue) -> Verdict {
		Verdict::Device { path, rev, issue }
	}

	fn dyn_path() -> DevicePath {
		DevicePath::parse("dyn").expect("the fixed dyn path is valid")
	}

	fn unknown_path_fault(&self, raw: &str, registry: &Registry) -> Verdict {
		let path = DevicePath::parse(raw).unwrap_or_else(|_| Self::dyn_path());
		let issue = ArgIssue {
			path:     vec![ArgPath::Key(sf!("do_"))],
			expected: render_near_miss(raw, registry.devices()),
			kind:     ArgIssueKind::Malformed,
			example:  Some(sf!(r#"{{"do_":"search"}}"#)),
			found:    None,
		};
		self.fault(path, self.spec.rev.clone(), issue)
	}

	fn docs(&self, registry: &Registry, path: &DevicePath, flat: &Object) -> Str {
		let rendered = registry
			.devices()
			.find(|device| device.name.as_str() == path.root().as_str())
			.map_or_else(
				|| sf!("The resolved device is no longer mounted."),
				|device| Str::new(render_device_docs(&device, path.to_string().as_str())),
			);
		slice_rendered(&rendered, flat_offset(flat), flat_limit(flat))
	}

	fn schema_echo(&self) -> Str {
		let mut output = String::from(
			"dyn\n\nDiscover documented devices and invoke one through the stable dynamic \
			 transport.\n\nOperations: search, docs/<path>, invoke/<path>\nSearch parameters: q, \
			 tags, provenance, offset, limit, depth.\n\nSchema:\n",
		);
		output.push_str(std::str::from_utf8(&self.spec.schema).unwrap_or("{}"));
		Str::new(output)
	}

	fn next_invocation_id(&self) -> Str {
		let sequence = self.next_invocation_id.fetch_add(1, Ordering::Relaxed);
		sf!("dyn-{sequence}")
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
					path: vec![], expected: sf!("an argument object"), kind: ArgIssueKind::Malformed,
					example: None, found: None,
				});
				return;
			};
			let operation = match flat.get("do_").and_then(Value::as_str) {
				Some(value) if is_help_token(value) => {
					yield done(Ok(DynPayload::Text { text: self.schema_echo() }));
					return;
				},
				Some(value) => match parse_operation(value) {
					Ok(operation) => operation,
					Err(error) => {
						yield done(Err(self.fault(Self::dyn_path(), self.spec.rev.clone(), operation_issue(error))));
						return;
					},
				},
				None if is_device_help(flat) => {
					yield done(Ok(DynPayload::Text { text: self.schema_echo() }));
					return;
				},
				None => {
					yield done(Err(self.fault(Self::dyn_path(), self.spec.rev.clone(), operation_issue(OperationError::Empty))));
					return;
				},
			};
			let Some(registry) = self.catalog.registry() else {
				yield done(Err(self.fault(Self::dyn_path(), self.spec.rev.clone(), operation_issue(OperationError::Unknown { op: sf!("catalog") }))));
				return;
			};
			match operation {
				Operation::Search(subtree) => {
					yield done(Ok(DynPayload::Text { text: self.catalog(&registry, subtree.as_ref(), flat) }));
				},
				Operation::Docs(raw) => {
					let (path, _target) = if let Ok(resolved) = resolve_operation(&registry, &Operation::Docs(raw.clone())) { resolved } else {
								  yield done(Err(self.unknown_path_fault(raw.as_str(), &registry)));
								  return;
							  };
					yield done(Ok(DynPayload::Text { text: self.docs(&registry, &path, flat) }));
				},
				Operation::Invoke(raw) => {
					let (path, target) = if let Ok(resolved) = resolve_operation(&registry, &Operation::Invoke(raw.clone())) { resolved } else {
								  yield done(Err(self.unknown_path_fault(raw.as_str(), &registry)));
								  return;
							  };
					if is_device_help(flat) {
						yield done(Ok(DynPayload::Text { text: self.docs(&registry, &path, flat) }));
						return;
					}
					let declares_intent = registry
						.live_spec(target.name.as_str())
						.ok()
						.and_then(|spec| serde_json::from_slice::<SchemaValue>(&spec.schema).ok())
						.and_then(|schema| schema.get("properties").and_then(SchemaValue::as_object).cloned())
						.is_some_and(|properties| properties.contains_key("i"));
					let args_json = if let Ok(args) = renest_args(flat, declares_intent) { args } else {
								  yield done(Err(self.fault(path, target.rev.clone(), operation_issue(OperationError::MissingPath { op: sf!("arguments") }))));
								  return;
							  };
					let identity = target.identity();
					let mut events = match target.route {
						ToolRoute::Native => {
							let (feed, nested) = IncomingParams::channel();
							let raw = Str::new(std::str::from_utf8(&args_json).expect("serialized JSON is UTF-8"));
							if feed.args_committed(raw).is_err() {
								yield done(Err(self.unknown_path_fault(path.to_string().as_str(), &registry)));
								return;
							}
							if let Ok(events) = registry.invoke_device(&path, nested) { events } else {
											 yield done(Err(self.unknown_path_fault(path.to_string().as_str(), &registry)));
											 return;
										 }
						},
						ToolRoute::Worker { site, name } => self.invoker.invoke(DeviceInvokeRequest {
							path: path.clone(),
							name: target.name.clone(),
							rev: Str::new(target.rev.to_string()),
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
					|| vec![Part::Text { text: sf!("Device result unavailable.") }],
					|parts| parts.to_vec(),
				),
			Err(Verdict::Device { issue, .. }) => vec![Part::Text { text: issue.expected.clone() }],
		}
	}
}

const fn done(result: Result<DynPayload, Verdict>) -> omp_tool::Ev<DynUpdate, DynPayload, Verdict> {
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

fn is_help_token(value: &str) -> bool {
	matches!(value.trim().to_ascii_lowercase().as_str(), "" | "?" | "help")
}

fn is_device_help(flat: &Object) -> bool {
	let mut payload = flat.iter().filter(|(key, _)| key.as_str() != "do_");
	match (payload.next(), payload.next()) {
		(None, None) => true,
		(Some((_, value)), None) => value.as_str().is_some_and(is_help_token),
		_ => false,
	}
}

fn catalog_query(flat: &Object) -> CatalogQuery {
	let text = flat
		.get("q")
		.and_then(Value::as_str)
		.map(str::trim)
		.filter(|value| !value.is_empty())
		.map(Str::new);
	let provenance = ["provenance", "claimant", "source"]
		.into_iter()
		.find_map(|key| flat.get(key).and_then(Value::as_str))
		.map(str::trim)
		.filter(|value| !value.is_empty())
		.map(Str::new);
	let mut tags = SmallVec::new();
	if let Some(value) = flat.get("tags") {
		if let Some(authored) = value.as_str() {
			tags.extend(
				authored
					.split([',', ' '])
					.map(str::trim)
					.filter(|tag| !tag.is_empty())
					.map(Str::new),
			);
		} else if let Some(authored) = value.as_array() {
			tags.extend(
				authored
					.iter()
					.filter_map(Value::as_str)
					.map(str::trim)
					.filter(|tag| !tag.is_empty())
					.map(Str::new),
			);
		}
	}
	CatalogQuery {
		text,
		tags,
		provenance,
		offset: flat_offset(flat),
		limit: flat_limit(flat),
		depth: flat
			.get("depth")
			.and_then(Value::as_u64)
			.and_then(|depth| usize::try_from(depth).ok()),
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
	Str::new(&text[start..end])
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
	use std::sync::Arc;

	use async_stream::stream;
	use bytes::Bytes;
	use futures::StreamExt as _;
	use omp_core::{Str, sf};
	use omp_slopjson::Value;
	use omp_tool::{
		Claims, Constraint, Effects, ErasedEv, ErasedOutcome, Ev, IncomingParams, MountedDevice,
		Precedence, Presentation, Registry, Rev, Tool, ToolRoute, ToolSpec, ToolTerminal,
		ToolsPolicy,
	};
	use parking_lot::Mutex;
	use serde_json::json;

	use super::{
		AUTO_QA_PROMPT_GUIDANCE, CatalogQuery, DOCS_TOTAL_BUDGET, DeviceCatalog, DeviceInvokeRequest,
		DeviceInvoker, DocsMode, DynPayload, EXTERNAL_SUMMARY_CAP, Operation, OperationError,
		PER_DEVICE_DOCS_CAP, PROMPT_GUIDANCE, dyn_enabled, dyn_tool, flatten_slots, parse_operation,
		render_catalog, render_catalog_query, render_prompt_docs, renest_args, reserved_parameter,
	};

	#[derive(Clone, Default)]
	struct StubInvoker(Arc<Mutex<Vec<DeviceInvokeRequest>>>);

	impl DeviceInvoker for StubInvoker {
		async fn invoke(&self, request: DeviceInvokeRequest) -> omp_tool::ErasedStream<'static> {
			self.0.lock().push(request);
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
					name:            sf!("fixture"),
					rev:             Rev { family: Str::default(), n: 1 },
					description:     sf!("Fixture device."),
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
					claimant:   sf!("test/fixture"),
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
			.args_committed(Str::new(args))
			.expect("arguments commit");
		tool.call(params).collect().await
	}

	fn mounted<'a>(
		name: &'a Str,
		claimant: &'a Str,
		summary: &'a Str,
		docs: Option<&'a str>,
		rev: &'a Rev,
		effects: &'a Effects,
		route: &'a ToolRoute,
	) -> MountedDevice<'a> {
		MountedDevice {
			name,
			rev,
			claimant,
			summary,
			schema: br#"{"type":"object"}"#,
			effects,
			docs,
			route,
		}
	}
	#[test]
	fn prompt_guidance_uses_only_live_dyn_operations_and_never_xd_urls() {
		for operation in ["search", "docs/<path>", "invoke/<path>"] {
			assert!(PROMPT_GUIDANCE.contains(operation));
		}
		assert!(AUTO_QA_PROMPT_GUIDANCE.contains("report_issue"));
		assert!(!PROMPT_GUIDANCE.contains("xd://"));
		assert!(!AUTO_QA_PROMPT_GUIDANCE.contains("xd://"));
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
		let calls = invoker.0.lock();
		assert_eq!(calls.len(), 1);
		assert_eq!(calls[0].args_json.as_ref(), br#"{"title":"Fix"}"#);
	}

	#[tokio::test]
	async fn dyn_help_echoes_schema_without_dispatch() {
		for args in [r"{}", r#"{"content":"?"}"#, r#"{"do_":" ? "}"#, r#"{"do_":"HELP"}"#] {
			let (catalog, _registry) = catalog();
			let invoker = StubInvoker::default();
			let tool = dyn_tool(invoker.clone(), catalog, ToolsPolicy::Auto);
			let events = invoke(&tool, args).await;
			match events.last().expect("terminal event") {
				Ev::Done(ToolTerminal::Done { result: Ok(DynPayload::Text { text }), .. }) => {
					assert!(text.contains(r#""do_""#));
					assert!(text.contains("docs/<path>"));
				},
				event => panic!("unexpected event: {event:?}"),
			}
			assert!(invoker.0.lock().is_empty());
		}
	}

	#[tokio::test]
	async fn invoke_help_returns_target_schema_without_dispatch() {
		let invoker = StubInvoker::default();
		let (catalog, _registry) = catalog();
		let tool = dyn_tool(invoker.clone(), catalog, ToolsPolicy::Auto);
		let events = invoke(&tool, r#"{"do_":"invoke/fixture/resolve","content":" help "}"#).await;
		match events.last().expect("terminal event") {
			Ev::Done(ToolTerminal::Done { result: Ok(DynPayload::Text { text }), .. }) => {
				assert!(text.contains("Fixture device."));
				assert!(text.contains("fixture/resolve @ test/fixture"));
				assert!(text.contains(r#""title""#));
			},
			event => panic!("unexpected event: {event:?}"),
		}
		assert!(invoker.0.lock().is_empty());
	}

	#[tokio::test]
	async fn staged_action_subtool_path_is_preserved_for_device_owner() {
		let invoker = StubInvoker::default();
		let (catalog, _registry) = catalog();
		let tool = dyn_tool(invoker.clone(), catalog, ToolsPolicy::Auto);
		let events =
			invoke(&tool, r#"{"do_":"invoke/fixture/resolve","reason":"Apply proposal"}"#).await;
		assert!(matches!(
			events.last(),
			Some(Ev::Done(ToolTerminal::Done { result: Ok(DynPayload::Invocation { .. }), .. }))
		));
		let calls = invoker.0.lock();
		assert_eq!(calls[0].path.to_string(), "fixture/resolve");
		assert_eq!(calls[0].args_json.as_ref(), br#"{"reason":"Apply proposal"}"#);
	}

	#[test]
	fn catalog_search_ranks_filters_and_paginates() {
		let first_name = sf!("lint");
		let second_name = sf!("format");
		let first_claimant = sf!("acme/lint");
		let second_claimant = sf!("other/format");
		let first_summary = sf!("Pending proposal: resolve or reject the lint rewrite.");
		let second_summary = sf!("Format files and lint imports.");
		let rev = Rev { family: Str::default(), n: 1 };
		let effects = Effects::default();
		let route = ToolRoute::Native;
		let devices = [
			mounted(&first_name, &first_claimant, &first_summary, None, &rev, &effects, &route),
			mounted(&second_name, &second_claimant, &second_summary, None, &rev, &effects, &route),
		];
		let first_page = render_catalog_query(
			devices.into_iter(),
			&CatalogQuery {
				text:       Some("lint".into()),
				tags:       smallvec::smallvec!["external".into()],
				provenance: None,
				offset:     0,
				limit:      Some(1),
				depth:      None,
			},
			None,
		);
		assert!(first_page.starts_with("lint — Pending proposal:"));
		assert!(first_page.contains("More: offset=1 (2 total)"));
		let filtered = render_catalog_query(
			devices.into_iter(),
			&CatalogQuery { provenance: Some("other".into()), ..CatalogQuery::default() },
			None,
		);
		assert!(!filtered.contains("acme/lint"));
		assert!(filtered.contains("other/format"));
	}

	#[test]
	fn docs_modes_honor_allowlist_and_external_summary_budget() {
		let builtin_name = sf!("builtin");
		let external_name = sf!("external");
		let builtin_claimant = sf!("omp/core");
		let external_claimant = sf!("acme/tools");
		let builtin_summary = sf!("Built-in summary.");
		let external_summary = sf!("{}\nignored", "é".repeat(150));
		let rev = Rev { family: Str::default(), n: 1 };
		let effects = Effects::default();
		let route = ToolRoute::Native;
		let builtin = mounted(
			&builtin_name,
			&builtin_claimant,
			&builtin_summary,
			Some("BUILTIN FULL DOCS"),
			&rev,
			&effects,
			&route,
		);
		let external = mounted(
			&external_name,
			&external_claimant,
			&external_summary,
			Some("EXTERNAL FULL DOCS"),
			&rev,
			&effects,
			&route,
		);
		let builtins = render_prompt_docs([builtin, external].into_iter(), DocsMode::Builtins, &[]);
		assert!(builtins.contains("BUILTIN FULL DOCS"));
		assert!(!builtins.contains("EXTERNAL FULL DOCS"));
		let inline =
			render_prompt_docs([builtin, external].into_iter(), DocsMode::Inline, &["ext*".into()]);
		assert!(!inline.contains("BUILTIN FULL DOCS"));
		assert!(inline.contains("EXTERNAL FULL DOCS"));
		let oversized_docs = "x".repeat(PER_DEVICE_DOCS_CAP + 1);
		let oversized = mounted(
			&external_name,
			&external_claimant,
			&external_summary,
			Some(&oversized_docs),
			&rev,
			&effects,
			&route,
		);
		let bounded = render_prompt_docs([oversized].into_iter(), DocsMode::Inline, &["*".into()]);
		assert!(!bounded.contains(&"x".repeat(PER_DEVICE_DOCS_CAP)));
		assert!(bounded.chars().count() <= DOCS_TOTAL_BUDGET);
		let catalog = render_catalog([external].into_iter());
		let summary = catalog
			.split_once(" — ")
			.expect("catalog separator")
			.1
			.split_once(" [")
			.expect("tag separator")
			.0;
		assert!(summary.len() <= EXTERNAL_SUMMARY_CAP);
		assert!(!summary.contains('\n'));
		assert!(summary.ends_with("..."));
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
