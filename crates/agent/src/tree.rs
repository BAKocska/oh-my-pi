//! Session agent roster, recursive budget authority, and concurrency permits.

use std::{
	collections::{BTreeMap, HashMap},
	future::Future,
	sync::{
		Arc,
		atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering},
	},
	time::Instant,
};

use omp_core::{AppendVec, InvocationPhase, Str, sf};
use omp_llm_inference::recovery::tools::{ToolAssemblyLimits, validate_schema};
use parking_lot::{Mutex, RwLock};
use serde_json::Value;
use thiserror::Error;

/// Default tree-wide number of concurrently running agent turns.
pub const DEFAULT_MAX_CONCURRENCY: usize = 32;
/// Default number of whole spawn waves allowed to await admission.
pub const DEFAULT_MAX_ADMISSION_QUEUE: usize = 128;
/// Number of schema-correction attempts before permissive mode accepts the
/// caller-visible override.
pub const MAX_YIELD_SCHEMA_RETRIES: u8 = 2;

/// Validated terminal or incremental subagent yield.
#[derive(Clone, Debug, PartialEq)]
pub struct YieldPayload {
	/// Verbatim structured success payload after lossless string-container
	/// salvage, when present.
	pub data:              Option<Value>,
	/// Caller-reported terminal failure.
	pub error:             Option<Str>,
	/// String terminal label or array incremental section path.
	pub kind:              Option<Value>,
	/// Whether finalization should consume the child's last assistant turn.
	pub use_last_turn:     bool,
	/// Whether this call submitted an incremental section.
	pub incremental:       bool,
	/// Whether permissive mode accepted a payload after exhausting schema
	/// correction attempts.
	pub schema_overridden: bool,
}

/// Retryable malformed-yield reason returned in-band to the child.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum YieldPayloadError {
	/// `type` was neither a string nor a non-empty string array.
	#[error("type must be a string or non-empty array of strings")]
	InvalidType,
	/// No object-shaped result envelope could be recovered.
	#[error("result must be an object containing either data or error")]
	InvalidEnvelope,
	/// A success envelope carried explicit null data.
	#[error("data is required when yield indicates success")]
	MissingData,
	/// A failure envelope did not carry a string error.
	#[error("error must be a string when yield indicates failure")]
	InvalidError,
	/// Structured tasks cannot use prose-only last-turn finalization.
	#[error(
		"this task requires structured output matching the declared schema; submit the full object \
		 as result.data"
	)]
	SchemaBoundLastTurn,
	/// The recovered payload did not match the declared output schema.
	#[error("yield payload violates output schema at {path} ({rule})")]
	SchemaViolation {
		/// JSON Pointer-like failing payload path.
		path: Str,
		/// Stable schema rule identifier.
		rule: &'static str,
	},
}

/// Stateful, verbatim validator for one subagent's yield calls.
///
/// Generic argument coercion must never run before this validator: accepted
/// payloads are the child's deliverable, not tool plumbing. Only reversible
/// wrapper recovery and JSON-container parsing are performed here.
pub struct YieldPayloadValidator {
	schema:                   Option<Value>,
	strict:                   bool,
	has_incremental_sections: bool,
	schema_retries:           u8,
}

impl YieldPayloadValidator {
	/// Creates a validator for an optional declared output schema.
	#[must_use]
	pub fn new(schema: Option<Value>, strict: bool) -> Self {
		Self { schema, strict, has_incremental_sections: false, schema_retries: 0 }
	}

	/// Validates and losslessly salvages one raw yield argument object.
	pub fn validate(&mut self, raw: &Value) -> Result<YieldPayload, YieldPayloadError> {
		let raw = raw.as_object().ok_or(YieldPayloadError::InvalidEnvelope)?;
		let kind = parse_yield_kind(raw.get("type"))?;
		let incremental = kind.as_ref().is_some_and(Value::is_array);
		let result =
			resolve_result_record(raw, kind.is_some()).ok_or(YieldPayloadError::InvalidEnvelope)?;
		let error = match result.get("error") {
			Some(Value::String(error)) => Some(Str::new(error.as_str())),
			Some(_) => return Err(YieldPayloadError::InvalidError),
			None => None,
		};
		let has_data = result.contains_key("data");
		let mut data = result.get("data").cloned();
		let use_last_turn = error.is_none() && !has_data && kind.is_some();
		if error.is_none() && matches!(data, Some(Value::Null)) {
			return Err(YieldPayloadError::MissingData);
		}
		if error.is_none() && !has_data && !use_last_turn {
			return Err(YieldPayloadError::InvalidEnvelope);
		}
		if use_last_turn && self.schema.is_some() && !self.has_incremental_sections && !incremental {
			return Err(YieldPayloadError::SchemaBoundLastTurn);
		}
		let mut schema_overridden = false;
		if error.is_none()
			&& !use_last_turn
			&& !incremental
			&& let Some(schema) = self.schema.as_ref()
			&& let Some(value) = data.as_mut()
		{
			if let Err(issue) =
				validate_schema(schema, value, self.strict, ToolAssemblyLimits::default())
			{
				let mut salvaged = false;
				if let Value::String(encoded) = value
					&& let Some(parsed) = parse_container_string(encoded)
					&& validate_schema(schema, &parsed, self.strict, ToolAssemblyLimits::default())
						.is_ok()
				{
					*value = parsed;
					salvaged = true;
				}
				if !salvaged {
					if self.strict || self.schema_retries < MAX_YIELD_SCHEMA_RETRIES {
						self.schema_retries = self.schema_retries.saturating_add(1);
						return Err(YieldPayloadError::SchemaViolation {
							path: issue.path,
							rule: issue.rule,
						});
					}
					schema_overridden = true;
				}
			}
		}
		if error.is_none() && incremental {
			self.has_incremental_sections = true;
		}
		Ok(YieldPayload { data, error, kind, use_last_turn, incremental, schema_overridden })
	}

	/// Returns whether at least one incremental section was accepted.
	#[must_use]
	pub const fn has_incremental_sections(&self) -> bool {
		self.has_incremental_sections
	}
}

fn parse_yield_kind(kind: Option<&Value>) -> Result<Option<Value>, YieldPayloadError> {
	match kind {
		None => Ok(None),
		Some(Value::String(kind)) => Ok(Some(Value::String(kind.clone()))),
		Some(Value::Array(kinds)) if !kinds.is_empty() && kinds.iter().all(Value::is_string) => {
			Ok(Some(Value::Array(kinds.clone())))
		},
		Some(_) => Err(YieldPayloadError::InvalidType),
	}
}

fn resolve_result_record(
	raw: &serde_json::Map<String, Value>,
	has_kind: bool,
) -> Option<serde_json::Map<String, Value>> {
	let result = match raw.get("result") {
		Some(Value::String(encoded)) => parse_container_string(encoded),
		Some(value) => Some(value.clone()),
		None => None,
	};
	if let Some(Value::Object(result)) = result {
		return Some(result);
	}
	if raw.get("result").is_some_and(|result| !result.is_null()) {
		return None;
	}
	if raw.contains_key("data") || raw.contains_key("error") {
		let mut result = serde_json::Map::new();
		if let Some(data) = raw.get("data") {
			result.insert("data".to_owned(), data.clone());
		}
		if let Some(error) = raw.get("error") {
			result.insert("error".to_owned(), error.clone());
		}
		return Some(result);
	}
	has_kind.then(serde_json::Map::new)
}

fn parse_container_string(encoded: &str) -> Option<Value> {
	let encoded = encoded.trim();
	if !(encoded.starts_with('{') || encoded.starts_with('[')) {
		return None;
	}
	serde_json::from_str(encoded).ok()
}

/// CONTROL operation whose generated metadata requires effects authorization.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum EffectsOperation {
	/// Starts a foreground or Core-owned child session.
	SpawnAgent,
	/// Creates or replaces a durable standing authorization.
	ScheduleUpsert,
	/// Requests paid constrained inference.
	Completion,
}

/// Enforces the shared `EFFECTS_AUTHORIZED` minimum phase for CONTROL effects.
///
/// Wire responders map [`SpawnRefusal::MinimumPhase`] to
/// `SPAWN_REFUSAL_MINIMUM_PHASE`; all three operations deliberately use the
/// same refusal so hooks cannot spend or spawn speculatively.
pub fn enforce_minimum_phase(
	phase: InvocationPhase,
	_: EffectsOperation,
) -> Result<(), SpawnRefusal> {
	if phase.allows_operation(InvocationPhase::EffectsAuthorized) {
		Ok(())
	} else {
		Err(SpawnRefusal::MinimumPhase)
	}
}

/// Stable classification of a roster node.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display, strum::EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum AgentKind {
	/// The interactive session root.
	Main,
	/// A child admitted through subagent spawning.
	Subagent,
	/// A passive observability transcript hidden from peer rosters.
	Advisor,
}

/// Lifecycle state stored in each roster node without allocating on reads.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display, strum::EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum AgentStatus {
	/// Admitted but not currently submitting a turn.
	Pending   = 0,
	/// A turn is actively consuming a concurrency permit.
	Running   = 1,
	/// Idle and available for steering.
	Settled   = 2,
	/// Successfully terminal.
	Completed = 3,
	/// Terminal with an error.
	Failed    = 4,
	/// Terminal after cancellation.
	Cancelled = 5,
	/// Terminal after a hard budget or deadline ceiling.
	Exhausted = 6,
}

impl AgentStatus {
	/// Decodes the compact atomic representation, treating corrupt values as
	/// failed.
	#[must_use]
	pub const fn from_atomic(value: u8) -> Self {
		match value {
			0 => Self::Pending,
			1 => Self::Running,
			2 => Self::Settled,
			3 => Self::Completed,
			4 => Self::Failed,
			5 => Self::Cancelled,
			6 => Self::Exhausted,
			_ => Self::Failed,
		}
	}

	/// Reports whether this status cannot receive another turn.
	#[must_use]
	pub const fn terminal(self) -> bool {
		matches!(self, Self::Completed | Self::Failed | Self::Cancelled | Self::Exhausted)
	}
}

/// Frontmatter policy governing which definitions an agent may spawn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SpawnPolicy {
	/// The `task` tool is unavailable.
	Disabled,
	/// Any discovered definition may be spawned.
	Any,
	/// Only the named definitions may be spawned.
	Only(Box<[Str]>),
}

impl SpawnPolicy {
	/// Reports whether `definition` is allowed by this exact policy.
	#[must_use]
	pub fn allows(&self, definition: &str) -> bool {
		match self {
			Self::Disabled => false,
			Self::Any => true,
			Self::Only(allowed) => allowed
				.iter()
				.any(|candidate| candidate.as_str().eq_ignore_ascii_case(definition)),
		}
	}

	/// Returns the inherited default definition for a child spawn.
	#[must_use]
	pub fn default_definition(&self) -> Option<&str> {
		match self {
			Self::Only(allowed) => allowed.first().map(Str::as_str),
			Self::Any => Some("task"),
			Self::Disabled => None,
		}
	}
}

/// Static agent definition loaded through the discovery manifest table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentDefinition {
	/// Stable discovery key, normally the file stem.
	pub name:           Str,
	/// Human-readable description used by dynamic task schemas.
	pub description:    Str,
	/// Exact child tool vocabulary. An empty list inherits the caller toolset.
	pub tools:          Box<[Str]>,
	/// Child-spawn capability and whitelist.
	pub spawns:         SpawnPolicy,
	/// Optional role or exact model selector.
	pub model:          Option<Str>,
	/// Optional typed thinking level name.
	pub thinking_level: Option<Str>,
	/// Whether execution must block the caller.
	pub blocking:       bool,
	/// Markdown body appended to the spawned system prompt.
	pub prompt:         Str,
}

/// Malformed agent discovery frontmatter.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum AgentDefinitionError {
	/// The markdown document lacks a complete frontmatter fence.
	#[error("agent definition frontmatter is missing or unterminated")]
	MissingFrontmatter,
	/// A supported field had an invalid value.
	#[error("invalid agent frontmatter field {0}")]
	InvalidField(Str),
}

impl AgentDefinition {
	/// Parses the portable frontmatter subset used by manifest-discovered
	/// definitions. Unknown keys remain forward-compatible and are ignored.
	pub fn parse_markdown(
		name: impl Into<Str>,
		markdown: &str,
	) -> Result<Self, AgentDefinitionError> {
		let name = name.into();
		let Some(rest) = markdown.strip_prefix("---\n") else {
			return Err(AgentDefinitionError::MissingFrontmatter);
		};
		let Some((frontmatter, prompt)) = rest.split_once("\n---") else {
			return Err(AgentDefinitionError::MissingFrontmatter);
		};
		let prompt = prompt.strip_prefix('\n').unwrap_or(prompt);
		let mut description = Default::default();
		let mut tools = Box::<[Str]>::default();
		let mut spawns = SpawnPolicy::Disabled;
		let mut model = None;
		let mut thinking_level = None;
		let mut blocking = false;
		for raw in frontmatter.lines() {
			let line = raw.trim();
			if line.is_empty() || line.starts_with('#') {
				continue;
			}
			let Some((key, value)) = line.split_once(':') else {
				return Err(AgentDefinitionError::InvalidField(Str::new(line)));
			};
			let value = value.trim();
			match key.trim() {
				"description" => description = Str::new(unquote(value)),
				"tools" => tools = parse_string_list(value)?.into_boxed_slice(),
				"spawns" => spawns = parse_spawn_policy(value)?,
				"model" if !value.is_empty() => model = Some(Str::new(unquote(value))),
				"thinkingLevel" | "thinking_level" if !value.is_empty() => {
					thinking_level = Some(Str::new(unquote(value)));
				},
				"blocking" => {
					blocking = parse_bool(value)
						.ok_or_else(|| AgentDefinitionError::InvalidField(sf!("blocking")))?;
				},
				_ => {},
			}
		}
		Ok(Self {
			name,
			description,
			tools,
			spawns,
			model,
			thinking_level,
			blocking,
			prompt: Str::new(prompt),
		})
	}

	/// Resolves a configured per-agent override ahead of frontmatter.
	#[must_use]
	pub fn effective_model<'a>(&'a self, overrides: &'a BTreeMap<Str, Str>) -> Option<&'a str> {
		overrides
			.iter()
			.find(|(name, _)| name.as_str().eq_ignore_ascii_case(self.name.as_str()))
			.map(|(_, model)| model.as_str())
			.or_else(|| self.model.as_deref())
	}
}

fn parse_spawn_policy(value: &str) -> Result<SpawnPolicy, AgentDefinitionError> {
	match unquote(value) {
		"*" | "true" => Ok(SpawnPolicy::Any),
		"" | "false" => Ok(SpawnPolicy::Disabled),
		_ => {
			let allowed = parse_string_list(value)?;
			if allowed.is_empty() {
				Ok(SpawnPolicy::Disabled)
			} else {
				Ok(SpawnPolicy::Only(allowed.into_boxed_slice()))
			}
		},
	}
}

fn parse_string_list(value: &str) -> Result<Vec<Str>, AgentDefinitionError> {
	let value = value.trim();
	let value = value
		.strip_prefix('[')
		.and_then(|value| value.strip_suffix(']'))
		.unwrap_or(value);
	if value.trim().is_empty() {
		return Ok(Vec::new());
	}
	let values = value
		.split(',')
		.map(|part| Str::new(unquote(part.trim())))
		.filter(|part| !part.is_empty())
		.collect::<Vec<_>>();
	if values.is_empty() {
		Err(AgentDefinitionError::InvalidField(Str::new(value)))
	} else {
		Ok(values)
	}
}

fn parse_bool(value: &str) -> Option<bool> {
	match unquote(value) {
		"true" => Some(true),
		"false" => Some(false),
		_ => None,
	}
}

fn unquote(value: &str) -> &str {
	value
		.strip_prefix('"')
		.and_then(|value| value.strip_suffix('"'))
		.or_else(|| {
			value
				.strip_prefix('\'')
				.and_then(|value| value.strip_suffix('\''))
		})
		.unwrap_or(value)
}

/// Durable usage totals used for hard subtree budget checks.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Usage {
	/// Submitted provider requests.
	pub requests:      u64,
	/// Metered input tokens, including the inference-owned cache policy.
	pub input_tokens:  u64,
	/// Output and reasoning tokens.
	pub output_tokens: u64,
	/// Cost in micros of USD from durable turn receipts only.
	pub usd_micros:    u64,
}

impl Usage {
	fn saturating_add(self, right: Self) -> Self {
		Self {
			requests:      self.requests.saturating_add(right.requests),
			input_tokens:  self.input_tokens.saturating_add(right.input_tokens),
			output_tokens: self.output_tokens.saturating_add(right.output_tokens),
			usd_micros:    self.usd_micros.saturating_add(right.usd_micros),
		}
	}
}

/// Hard ceilings for an agent and every descendant.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Budget {
	/// Maximum subtree provider requests.
	pub max_requests:      Option<u64>,
	/// Maximum subtree metered input tokens.
	pub max_input_tokens:  Option<u64>,
	/// Maximum subtree output and reasoning tokens.
	pub max_output_tokens: Option<u64>,
	/// Maximum subtree durable receipt spend in micros of USD.
	pub max_usd_micros:    Option<u64>,
	/// Maximum duration from admission to settlement.
	pub max_wall:          Option<std::time::Duration>,
}

impl Budget {
	/// Clamps this budget to the unspent remainder represented by `parent`.
	#[must_use]
	pub fn clamped_to(self, parent: BudgetRemainder) -> Self {
		Self {
			max_requests:      clamp(self.max_requests, parent.requests),
			max_input_tokens:  clamp(self.max_input_tokens, parent.input_tokens),
			max_output_tokens: clamp(self.max_output_tokens, parent.output_tokens),
			max_usd_micros:    clamp(self.max_usd_micros, parent.usd_micros),
			max_wall:          match (self.max_wall, parent.wall) {
				(Some(child), Some(ancestor)) => Some(child.min(ancestor)),
				(None, value) => value,
				(value, None) => value,
			},
		}
	}
}

fn clamp(child: Option<u64>, parent: Option<u64>) -> Option<u64> {
	match (child, parent) {
		(Some(child), Some(parent)) => Some(child.min(parent)),
		(None, value) => value,
		(value, None) => value,
	}
}

/// Remaining capacity at one point in an ancestor chain.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BudgetRemainder {
	/// Remaining requests.
	pub requests:      Option<u64>,
	/// Remaining input tokens.
	pub input_tokens:  Option<u64>,
	/// Remaining output tokens.
	pub output_tokens: Option<u64>,
	/// Remaining durable-receipt spend.
	pub usd_micros:    Option<u64>,
	/// Remaining wall time.
	pub wall:          Option<std::time::Duration>,
}

#[derive(Debug)]
struct BudgetAccount {
	budget:      Budget,
	usage:       Usage,
	admitted_at: Instant,
}

impl BudgetAccount {
	fn remainder(&self) -> BudgetRemainder {
		BudgetRemainder {
			requests:      self
				.budget
				.max_requests
				.map(|cap| cap.saturating_sub(self.usage.requests)),
			input_tokens:  self
				.budget
				.max_input_tokens
				.map(|cap| cap.saturating_sub(self.usage.input_tokens)),
			output_tokens: self
				.budget
				.max_output_tokens
				.map(|cap| cap.saturating_sub(self.usage.output_tokens)),
			usd_micros:    self
				.budget
				.max_usd_micros
				.map(|cap| cap.saturating_sub(self.usage.usd_micros)),
			wall:          self
				.budget
				.max_wall
				.map(|cap| cap.saturating_sub(self.admitted_at.elapsed())),
		}
	}

	fn permits(&self, next: Usage) -> Result<(), BudgetCeiling> {
		let total = self.usage.saturating_add(next);
		if self
			.budget
			.max_requests
			.is_some_and(|cap| total.requests > cap)
		{
			return Err(BudgetCeiling::Requests);
		}
		if self
			.budget
			.max_input_tokens
			.is_some_and(|cap| total.input_tokens > cap)
		{
			return Err(BudgetCeiling::InputTokens);
		}
		if self
			.budget
			.max_output_tokens
			.is_some_and(|cap| total.output_tokens > cap)
		{
			return Err(BudgetCeiling::OutputTokens);
		}
		if self
			.budget
			.max_usd_micros
			.is_some_and(|cap| total.usd_micros > cap)
		{
			return Err(BudgetCeiling::Usd);
		}
		if self
			.budget
			.max_wall
			.is_some_and(|cap| self.admitted_at.elapsed() > cap)
		{
			return Err(BudgetCeiling::Wall);
		}
		Ok(())
	}
}

/// One roster node retained for the life of its session.
pub struct AgentNode {
	/// Stable agent identity.
	pub id:      Str,
	/// Session-unique display and routing name.
	pub name:    Str,
	/// Whether this is the root or a spawned child.
	pub kind:    AgentKind,
	/// Parent identity, absent only for the root.
	pub parent:  Option<Str>,
	/// Tree depth, with root at zero.
	pub depth:   u16,
	/// Session identity owning this journal.
	pub session: Str,
	status:      AtomicU8,
	activity:    Mutex<Str>,
	budget:      Mutex<BudgetAccount>,
}

impl AgentNode {
	/// Returns this node's allocation-free lifecycle state.
	#[must_use]
	pub fn status(&self) -> AgentStatus {
		AgentStatus::from_atomic(self.status.load(Ordering::Acquire))
	}

	/// Publishes a lifecycle state.
	pub fn set_status(&self, status: AgentStatus) {
		self.status.store(status as u8, Ordering::Release);
	}

	/// Replaces the short roster activity text.
	pub fn set_activity(&self, activity: Str) {
		*self.activity.lock() = activity;
	}

	/// Returns a clone of the latest roster activity text.
	#[must_use]
	pub fn activity(&self) -> Str {
		self.activity.lock().clone()
	}

	/// Returns direct durable-receipt usage for this node.
	#[must_use]
	pub fn usage(&self) -> Usage {
		self.budget.lock().usage
	}
}

/// Reason a spawn wave could not be admitted.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum SpawnRefusal {
	/// The requested parent was absent or terminal.
	#[error("parent agent is unavailable")]
	ParentGone,
	/// The requested child would exceed the tree depth ceiling.
	#[error("agent depth ceiling exceeded")]
	DepthExceeded,
	/// CONTROL effects were invoked before `EFFECTS_AUTHORIZED`.
	#[error("SPAWN_REFUSAL_MINIMUM_PHASE")]
	MinimumPhase,
	/// The whole spawn wave cannot fit in the bounded admission queue.
	#[error(
		"agent concurrency exhausted (running={running}, queued={queued}, max={max_concurrency})"
	)]
	ConcurrencyExhausted {
		/// Turns holding concurrency permits.
		running:         usize,
		/// Spawn-wave slots already awaiting permits.
		queued:          usize,
		/// Tree-wide concurrency ceiling.
		max_concurrency: usize,
	},
}

/// Ceiling which rejected a request before it reached a provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum BudgetCeiling {
	/// Request count would exceed its cap.
	Requests,
	/// Input tokens would exceed their cap.
	InputTokens,
	/// Output tokens would exceed their cap.
	OutputTokens,
	/// Durable receipt spend would exceed its cap.
	Usd,
	/// Admission-to-settlement duration exceeded its cap.
	Wall,
}

/// Budget pre-dispatch rejection for a node or any ancestor.
#[derive(Debug, Error, Eq, PartialEq)]
#[error("agent budget exhausted: {ceiling}")]
pub struct BudgetExceeded {
	/// The first ancestor ceiling crossed by the proposed request.
	pub ceiling: BudgetCeiling,
}

/// RAII reservation for a complete spawn wave or an active agent turn.
///
/// Dropping it releases every held permit. A waiter must call
/// [`Self::release_for_wait`] before awaiting a child and [`Self::reacquire`]
/// afterwards; this is the release-while-waiting accounting rule.
pub struct SpawnPermit {
	semaphore: Arc<tokio::sync::Semaphore>,
	held:      Option<tokio::sync::OwnedSemaphorePermit>,
	units:     u32,
}

impl SpawnPermit {
	/// Releases this agent's active-turn capacity before waiting on a child.
	pub fn release_for_wait(&mut self) {
		let _ = self.held.take();
	}

	/// Re-acquires the same capacity after a child wait completes.
	///
	/// # Panics
	/// Panics only when an internal semaphore is closed, which this tree never
	/// does.
	pub async fn reacquire(&mut self) {
		if self.held.is_none() {
			self.held = Some(
				Arc::clone(&self.semaphore)
					.acquire_many_owned(self.units)
					.await
					.expect("agent tree semaphore is never closed"),
			);
		}
	}

	/// Runs `future` without holding this agent's turn permit, then restores it.
	pub async fn wait<F: Future>(&mut self, future: F) -> F::Output {
		self.release_for_wait();
		let output = future.await;
		self.reacquire().await;
		output
	}

	/// Returns how many concurrency units this reservation represents.
	#[must_use]
	pub const fn units(&self) -> u32 {
		self.units
	}
}

/// Session-scoped append-only roster and resource authority.
pub struct AgentTree {
	nodes:             AppendVec<Arc<AgentNode>>,
	by_id:             RwLock<HashMap<Str, usize>>,
	by_name:           RwLock<HashMap<Str, usize>>,
	permits:           Arc<tokio::sync::Semaphore>,
	max_depth:         u16,
	max_concurrency:   usize,
	max_queue:         usize,
	queued:            AtomicUsize,
	roster_generation: AtomicU64,
	roster_watch:      tokio::sync::watch::Sender<u64>,
}

impl AgentTree {
	/// Creates an empty tree with explicit depth, concurrency, and queue
	/// ceilings.
	#[must_use]
	pub fn new(max_depth: u16, max_concurrency: usize, max_queue: usize) -> Self {
		let max_concurrency = max_concurrency.max(1);
		let (roster_watch, _) = tokio::sync::watch::channel(0_u64);
		Self {
			nodes: AppendVec::new(),
			by_id: RwLock::new(HashMap::new()),
			by_name: RwLock::new(HashMap::new()),
			permits: Arc::new(tokio::sync::Semaphore::new(max_concurrency)),
			max_depth,
			max_concurrency,
			max_queue,
			queued: AtomicUsize::new(0),
			roster_generation: AtomicU64::new(0),
			roster_watch,
		}
	}

	/// Creates a tree with the standard session ceilings.
	#[must_use]
	pub fn standard(max_depth: u16) -> Self {
		Self::new(max_depth, DEFAULT_MAX_CONCURRENCY, DEFAULT_MAX_ADMISSION_QUEUE)
	}

	/// Adds a root or admitted child to the append-only roster.
	pub fn register(
		&self,
		id: Str,
		name: Str,
		kind: AgentKind,
		parent: Option<Str>,
		session: Str,
		budget: Budget,
	) -> Result<Arc<AgentNode>, SpawnRefusal> {
		let depth = match parent.as_ref() {
			Some(parent) => self
				.node(parent)
				.ok_or(SpawnRefusal::ParentGone)?
				.depth
				.saturating_add(1),
			None => 0,
		};
		if depth > self.max_depth {
			return Err(SpawnRefusal::DepthExceeded);
		}
		let node = Arc::new(AgentNode {
			id: id.clone(),
			name: name.clone(),
			kind,
			parent,
			depth,
			session,
			status: AtomicU8::new(AgentStatus::Pending as u8),
			activity: Mutex::new(Default::default()),
			budget: Mutex::new(BudgetAccount {
				budget,
				usage: Usage::default(),
				admitted_at: Instant::now(),
			}),
		});
		let index = self.nodes.push(Arc::clone(&node));
		self.by_id.write().insert(id, index);
		self.by_name.write().insert(name, index);
		self.publish_roster_change();
		Ok(node)
	}

	/// Returns a node by stable identity without scanning the roster.
	#[must_use]
	pub fn node(&self, id: &str) -> Option<Arc<AgentNode>> {
		let index = *self.by_id.read().get(id)?;
		self.nodes.get(index).cloned()
	}

	/// Returns a node by session-local name without scanning the roster.
	#[must_use]
	pub fn named(&self, name: &str) -> Option<Arc<AgentNode>> {
		let index = *self.by_name.read().get(name)?;
		self.nodes.get(index).cloned()
	}

	/// Iterates the append-only roster in admission order.
	pub fn roster(&self) -> impl Iterator<Item = &Arc<AgentNode>> {
		self.nodes.iter()
	}

	/// Returns a watch receiver that advances whenever a node is admitted.
	///
	/// Consumers obtain the allocation-free roster after `changed()`; this
	/// avoids UI polling while keeping node storage append-only.
	#[must_use]
	pub fn watch_roster(&self) -> tokio::sync::watch::Receiver<u64> {
		self.roster_watch.subscribe()
	}

	/// Returns the current monotonic roster generation.
	#[must_use]
	pub fn roster_generation(&self) -> u64 {
		self.roster_generation.load(Ordering::Acquire)
	}

	/// Reserves an entire spawn wave, queuing it as one unit when saturated.
	///
	/// A queue overflow refuses the whole wave before any member can start.
	pub async fn admit(&self, count: usize) -> Result<SpawnPermit, SpawnRefusal> {
		let count = u32::try_from(count).unwrap_or(u32::MAX);
		let slots = usize::try_from(count).unwrap_or(usize::MAX);
		if count == 0 || slots > self.max_concurrency {
			return Err(self.concurrency_refusal());
		}
		let queued = self.queued.fetch_add(slots, Ordering::AcqRel);
		if queued.saturating_add(slots) > self.max_queue {
			self.queued.fetch_sub(slots, Ordering::AcqRel);
			return Err(self.concurrency_refusal());
		}
		let permit = Arc::clone(&self.permits)
			.acquire_many_owned(count)
			.await
			.expect("agent tree semaphore is never closed");
		self.queued.fetch_sub(slots, Ordering::AcqRel);
		Ok(SpawnPermit {
			semaphore: Arc::clone(&self.permits),
			held:      Some(permit),
			units:     count,
		})
	}

	/// Checks all ancestor ceilings before dispatch and records receipt-backed
	/// usage.
	///
	/// Callers must pass only usage committed by a durable receipt; telemetry is
	/// intentionally not an input to this method.
	pub fn debit_receipt(&self, node_id: &str, usage: Usage) -> Result<(), BudgetExceeded> {
		let mut lineage = Vec::new();
		let mut current = self
			.node(node_id)
			.ok_or(BudgetExceeded { ceiling: BudgetCeiling::Requests })?;
		loop {
			lineage.push(Arc::clone(&current));
			let Some(parent) = current.parent.as_ref() else {
				break;
			};
			current = self
				.node(parent)
				.ok_or(BudgetExceeded { ceiling: BudgetCeiling::Requests })?;
		}
		lineage.reverse();
		let mut accounts = lineage
			.iter()
			.map(|node| node.budget.lock())
			.collect::<Vec<_>>();
		for account in &accounts {
			account
				.permits(usage)
				.map_err(|ceiling| BudgetExceeded { ceiling })?;
		}
		for account in &mut accounts {
			account.usage = account.usage.saturating_add(usage);
		}
		Ok(())
	}

	/// Clamps a child's requested budget against every ancestor's unspent
	/// remainder.
	pub fn clamp_budget(&self, parent_id: &str, requested: Budget) -> Result<Budget, SpawnRefusal> {
		let mut effective = requested;
		let mut current = self.node(parent_id).ok_or(SpawnRefusal::ParentGone)?;
		loop {
			effective = effective.clamped_to(current.budget.lock().remainder());
			let Some(parent) = current.parent.as_ref() else {
				break;
			};
			current = self.node(parent).ok_or(SpawnRefusal::ParentGone)?;
		}
		Ok(effective)
	}

	/// Returns the tree-wide concurrency ceiling.
	#[must_use]
	pub const fn max_concurrency(&self) -> usize {
		self.max_concurrency
	}

	fn concurrency_refusal(&self) -> SpawnRefusal {
		SpawnRefusal::ConcurrencyExhausted {
			running:         self
				.max_concurrency
				.saturating_sub(self.permits.available_permits()),
			queued:          self.queued.load(Ordering::Acquire),
			max_concurrency: self.max_concurrency,
		}
	}

	fn publish_roster_change(&self) {
		let generation = self
			.roster_generation
			.fetch_add(1, Ordering::AcqRel)
			.wrapping_add(1);
		self.roster_watch.send_replace(generation);
	}
}

#[cfg(test)]
mod tests {
	use serde_json::json;

	use super::*;

	#[tokio::test]
	async fn permit_is_released_while_waiting() {
		let tree = AgentTree::new(2, 1, 2);
		let mut parent = tree.admit(1).await.unwrap();
		parent
			.wait(async {
				drop(tree.admit(1).await.unwrap());
			})
			.await;
	}

	#[test]
	fn child_budget_clamps_to_ancestor_remainder() {
		let tree = AgentTree::standard(2);
		tree
			.register(sf!("root"), sf!("Main"), AgentKind::Main, None, sf!("s"), Budget {
				max_requests: Some(4),
				..Budget::default()
			})
			.unwrap();
		tree
			.debit_receipt("root", Usage { requests: 3, ..Usage::default() })
			.unwrap();
		assert_eq!(
			tree
				.clamp_budget("root", Budget { max_requests: Some(9), ..Budget::default() })
				.unwrap()
				.max_requests,
			Some(1)
		);
	}

	#[test]
	fn wrapperless_terminal_yield_uses_last_turn_without_schema() {
		let mut validator = YieldPayloadValidator::new(None, true);
		let payload = validator.validate(&json!({"type": "result"})).unwrap();
		assert!(payload.use_last_turn);
		assert_eq!(payload.kind, Some(json!("result")));
		assert!(payload.data.is_none());
	}

	#[test]
	fn schema_bound_last_turn_is_retryable_until_a_section_exists() {
		let schema = json!({
			"type": "object",
			"properties": {"summary": {"type": "string"}},
			"required": ["summary"]
		});
		let mut validator = YieldPayloadValidator::new(Some(schema), true);
		assert_eq!(
			validator.validate(&json!({"type": "result"})),
			Err(YieldPayloadError::SchemaBoundLastTurn)
		);
		validator
			.validate(&json!({
				"type": ["summary"],
				"result": {"data": "done"}
			}))
			.unwrap();
		assert!(validator.has_incremental_sections());
		assert!(
			validator
				.validate(&json!({"type": "result"}))
				.unwrap()
				.use_last_turn
		);
	}

	#[test]
	fn weak_yield_envelopes_are_salvaged_losslessly() {
		let mut validator = YieldPayloadValidator::new(None, true);
		assert_eq!(
			validator
				.validate(&json!({"data": {"ok": true}}))
				.unwrap()
				.data,
			Some(json!({"ok": true}))
		);
		assert_eq!(
			validator
				.validate(&json!({"error": "blocked"}))
				.unwrap()
				.error,
			Some(sf!("blocked"))
		);
		assert_eq!(
			validator
				.validate(&json!({"result": "{\"data\":{\"ok\":true}}"}))
				.unwrap()
				.data,
			Some(json!({"ok": true}))
		);
	}

	#[test]
	fn schema_payload_parses_container_string_but_never_stringifies_objects() {
		let object_schema = json!({
			"type": "object",
			"properties": {"n": {"type": "number"}},
			"required": ["n"],
			"additionalProperties": false
		});
		let mut validator = YieldPayloadValidator::new(Some(object_schema), true);
		assert_eq!(
			validator
				.validate(&json!({"result": {"data": "{\"n\":4}"}}))
				.unwrap()
				.data,
			Some(json!({"n": 4}))
		);

		let string_field_schema = json!({
			"type": "object",
			"properties": {"summary": {"type": "string"}},
			"required": ["summary"],
			"additionalProperties": false
		});
		let mut validator = YieldPayloadValidator::new(Some(string_field_schema), true);
		assert!(matches!(
			validator.validate(&json!({
				"result": {"data": {"summary": {"purge": 13, "keep": 20}}}
			})),
			Err(YieldPayloadError::SchemaViolation { path, rule: "type" })
				if path.as_str() == "/summary"
		));
	}

	#[test]
	fn permissive_yield_overrides_only_after_retry_budget() {
		let schema = json!({"type":"string"});
		let raw = json!({"result":{"data":7}});
		let mut permissive = YieldPayloadValidator::new(Some(schema.clone()), false);
		for _ in 0..MAX_YIELD_SCHEMA_RETRIES {
			assert!(matches!(
				permissive.validate(&raw),
				Err(YieldPayloadError::SchemaViolation { .. })
			));
		}
		assert!(permissive.validate(&raw).unwrap().schema_overridden);

		let mut strict = YieldPayloadValidator::new(Some(schema), true);
		for _ in 0..=MAX_YIELD_SCHEMA_RETRIES {
			assert!(matches!(strict.validate(&raw), Err(YieldPayloadError::SchemaViolation { .. })));
		}
	}

	#[test]
	fn discovered_agent_frontmatter_enforces_spawn_and_model_policy() {
		let definition = AgentDefinition::parse_markdown(
			"reviewer",
			"---\ndescription: Review code\ntools: [read, grep, hub]\nspawns: [scout, \
			 librarian]\nmodel: '@task'\nthinkingLevel: high\nblocking: true\n---\nReview carefully.",
		)
		.expect("definition");
		assert_eq!(definition.tools.as_ref(), ["read", "grep", "hub"]);
		assert!(definition.spawns.allows("SCOUT"));
		assert!(!definition.spawns.allows("task"));
		assert_eq!(definition.spawns.default_definition(), Some("scout"));
		assert_eq!(definition.thinking_level.as_deref(), Some("high"));
		assert!(definition.blocking);
		let overrides = BTreeMap::from([("Reviewer".into(), "@slow".into())]);
		assert_eq!(definition.effective_model(&overrides), Some("@slow"));
	}
}
