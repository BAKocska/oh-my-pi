//! Goal-directed swarm orchestration exposed as one dynamic device.

use std::{
	collections::BTreeMap,
	fmt,
	sync::{
		Arc, LazyLock, Weak,
		atomic::{AtomicBool, Ordering},
	},
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_stream::stream;
use async_trait::async_trait;
use futures::Stream;
use omp_agent::{AgentStatus, RegistryStatus};
use omp_core::{Str, sf};
use omp_proto::thread::v1::{Item, Message, Part as ThreadPart, Role, item, part};
use omp_tool::{
	Abort, ArgIssue, ArgIssueKind, ArtifactLifetime, CommitError, Constraint, Effects, Ev,
	ExpectedArtifact, IncomingParams, JobKind, JobMetadata, JobOwner, JobRef, ParamError, Part,
	PromptCaps, Rev, Tool, ToolSpec, ToolTerminal,
};
use parking_lot::{Mutex, RwLock};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::{
	chat::ChatParentHost,
	envd::eval::ParentSessionHost as _,
	modes::{ActiveMode, ExecutionModes},
};

/// A worker requested in a vibe-mode spawn wave.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WaveEntry {
	/// Complete delegated brief.
	#[schemars(with = "String")]
	pub brief: Str,
	/// Optional roster label.
	#[schemars(with = "Option<String>")]
	pub label: Option<Str>,
	/// Worker tier: `fast` selects sonic; `good` selects task.
	#[serde(default)]
	pub tier:  WorkerTier,
}

/// Worker tier available to a vibe spawn wave.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerTier {
	/// Mechanical, low-reasoning work.
	Fast,
	/// General-purpose implementation and analysis.
	#[default]
	Good,
}

/// The five operations accepted by the single vibe device.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
	/// Launch one concurrent worker wave.
	Spawn,
	/// Inspect worker lifecycle state.
	Status,
	/// Deliver a steering message to a running worker.
	Steer,
	/// Wait for and return worker results.
	Collect,
	/// Cancel running workers.
	Stop,
}

/// Arguments accepted by the vibe device.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Params {
	/// Operation to execute.
	pub op:         Operation,
	/// Worker briefs for `spawn`.
	#[serde(default)]
	pub wave:       Vec<WaveEntry>,
	/// Worker identifiers for `status`, `collect`, or `stop`; empty means all.
	#[serde(default)]
	#[schemars(with = "Vec<String>")]
	pub ids:        Vec<Str>,
	/// One worker identifier for `steer`.
	#[schemars(with = "Option<String>")]
	pub id:         Option<Str>,
	/// Steering text for `steer`.
	#[schemars(with = "Option<String>")]
	pub message:    Option<Str>,
	/// Maximum wait for `collect`; omitted waits until every selected worker
	/// settles.
	pub timeout_ms: Option<u64>,
}

impl Params {
	fn validate(&self) -> Result<(), Fault> {
		match self.op {
			Operation::Spawn if self.wave.is_empty() => {
				Err(Fault::new("spawn requires a non-empty wave"))
			},
			Operation::Spawn
				if self
					.wave
					.iter()
					.any(|worker| worker.brief.trim().is_empty()) =>
			{
				Err(Fault::new("worker briefs must not be empty"))
			},
			Operation::Steer if self.id.as_ref().is_none_or(|id| id.trim().is_empty()) => {
				Err(Fault::new("steer requires id"))
			},
			Operation::Steer
				if self
					.message
					.as_ref()
					.is_none_or(|text| text.trim().is_empty()) =>
			{
				Err(Fault::new("steer requires a non-empty message"))
			},
			_ => Ok(()),
		}
	}
}

/// JSON result returned by the device.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Payload {
	/// Structured operation result.
	pub result: Value,
}

/// Vibe operations do not stream intermediate updates.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Update {}

/// A rejected or failed vibe operation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Fault {
	message: Str,
}

impl Fault {
	fn new(message: impl Into<Str>) -> Self {
		Self { message: message.into() }
	}
}

impl fmt::Display for Fault {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.message)
	}
}

impl std::error::Error for Fault {}

#[async_trait]
trait VibeBackend: Send + Sync {
	async fn execute(&self, params: Params) -> Result<Value, Fault>;
}

static BACKEND: LazyLock<RwLock<Option<Arc<dyn VibeBackend>>>> =
	LazyLock::new(|| RwLock::new(None));

/// Restores the preceding chat-scoped vibe backend when dropped.
pub(crate) struct Attachment {
	previous: Option<Arc<dyn VibeBackend>>,
}

impl Drop for Attachment {
	fn drop(&mut self) {
		*BACKEND.write() = self.previous.take();
	}
}

fn attach(backend: Arc<dyn VibeBackend>) -> Attachment {
	let previous = BACKEND.write().replace(backend);
	Attachment { previous }
}

/// The native implementation mounted under the dynamic-device catalog.
pub struct Vibe {
	spec: ToolSpec,
}

/// Creates the single five-verb vibe device.
#[must_use]
pub fn tool() -> Vibe {
	Vibe {
		spec: ToolSpec {
			name:            sf!("vibe"),
			rev:             Rev { family: Default::default(), n: 1 },
			description:     sf!(
				"Runs a goal-directed worker swarm through one device. Use op=spawn with a wave of \
				 briefs, op=status to inspect workers, op=steer with id/message, op=collect to return \
				 settled results, and op=stop to cancel workers. ids omitted means all workers in \
				 this wave.",
			),
			schema:          omp_tool::schema::<Params>(),
			constraint:      Constraint::Schema {
				priority:       100,
				on_unsupported: omp_tool::Fallback::Unspecified,
			},
			effects:         Effects { subagents: u32::MAX, ..Effects::empty() },
			projection_code: omp_tool::native_projection_code(
				env!("CARGO_PKG_NAME"),
				env!("CARGO_PKG_VERSION"),
				include_bytes!("vibe.rs"),
			)
			.into_bytes(),
		},
	}
}

impl Tool for Vibe {
	type Fault = Fault;
	type Params = Params;
	type Payload = Payload;
	type Update = Update;

	fn spec(&self) -> &ToolSpec {
		&self.spec
	}

	fn call<'c>(
		&'c self,
		mut incoming: IncomingParams<'c>,
	) -> impl Stream<Item = Ev<Update, Payload, Fault>> + Send + 'c {
		stream! {
			let params = match incoming.whole::<Params>().await {
				Ok(value) => value,
				Err(error) => { yield param_event(error); return; }
			};
			if let Err(error) = params.validate() {
				yield Ev::Done(ToolTerminal::Done { result: Err(error), useless: true });
				return;
			}
			if let Err(error) = incoming.interruptable().committed().await {
				yield commit_event(error);
				return;
			}
			let Some(backend) = BACKEND.read().clone() else {
				yield Ev::Done(ToolTerminal::Done {
					result: Err(Fault::new("vibe is unavailable outside an attached chat session")),
					useless: true,
				});
				return;
			};
			match backend.execute(params).await {
				Ok(result) => yield Ev::Done(ToolTerminal::Done { result: Ok(Payload { result }), useless: false }),
				Err(error) => yield Ev::Done(ToolTerminal::Done { result: Err(error), useless: false }),
			}
		}
	}

	fn prompt(&self, view: Result<&Payload, &Fault>, _: &PromptCaps) -> Vec<Part> {
		let text = match view {
			Ok(payload) => serde_json::to_string_pretty(&payload.result)
				.unwrap_or_else(|_| "{\"error\":\"vibe result serialization failed\"}".to_owned()),
			Err(fault) => fault.to_string(),
		};
		vec![Part::Text { text: Str::from(text) }]
	}
}

#[derive(Clone)]
enum WorkerOutcome {
	Done(Value),
	Failed(Str),
}

struct Worker {
	label:      Str,
	tier:       WorkerTier,
	generation: u64,
	running:    bool,
	stopped:    bool,
	outcome:    Option<WorkerOutcome>,
	notify:     Arc<Notify>,
}

/// Chat-scoped wave runner backed by durable registered agent loops.
pub(crate) struct ChatVibeBackend<C: omp_agent::TurnClient + Clone + Send + 'static> {
	parent:      Arc<ChatParentHost<C>>,
	modes:       Arc<ExecutionModes>,
	workers:     Arc<Mutex<BTreeMap<Str, Worker>>>,
	seen_active: AtomicBool,
}

impl<C: omp_agent::TurnClient + Clone + Send + 'static> ChatVibeBackend<C> {
	/// Creates a wave runner and its app-owned TTL/mode-exit scheduler.
	#[must_use]
	pub(crate) fn new(parent: Arc<ChatParentHost<C>>, modes: Arc<ExecutionModes>) -> Arc<Self> {
		let backend = Arc::new(Self {
			parent,
			modes,
			workers: Arc::new(Mutex::new(BTreeMap::new())),
			seen_active: AtomicBool::new(false),
		});
		Self::start_scheduler(Arc::downgrade(&backend));
		backend
	}

	fn start_scheduler(backend: Weak<Self>) {
		drop(tokio::spawn(async move {
			let mut tick = tokio::time::interval(Duration::from_secs(1));
			tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
			loop {
				tick.tick().await;
				let Some(backend) = backend.upgrade() else {
					break;
				};
				if backend.modes.active() == ActiveMode::Vibe {
					backend.seen_active.store(true, Ordering::Release);
					let ttl_ms = backend.parent.task_settings().agent_idle_ttl_ms;
					if ttl_ms != 0 {
						backend
							.parent
							.park_expired_children(Duration::from_millis(ttl_ms))
							.await;
					}
				} else if backend.seen_active.swap(false, Ordering::AcqRel) {
					backend.release_scope().await;
				}
			}
		}));
	}

	fn selected_ids(&self, ids: &[Str]) -> Vec<Str> {
		if ids.is_empty() {
			self.workers.lock().keys().cloned().collect()
		} else {
			ids.to_vec()
		}
	}

	async fn spawn(&self, wave: Vec<WaveEntry>) -> Result<Value, Fault> {
		if self.parent.job_board().is_none() {
			return Err(Fault::new("vibe requires the session async job manager"));
		}
		let mut launched = Vec::with_capacity(wave.len());
		for entry in wave {
			let id = Str::from(omp_core::Ulid::generate().to_string());
			let label = entry.label.unwrap_or_else(|| id.clone());
			self.workers.lock().insert(id.clone(), Worker {
				label:      label.clone(),
				tier:       entry.tier,
				generation: 1,
				running:    true,
				stopped:    false,
				outcome:    None,
				notify:     Arc::new(Notify::new()),
			});
			if let Err(error) = self.launch_turn(id.clone(), label.clone(), entry.tier, entry.brief, 1)
			{
				self.workers.lock().remove(&id);
				return Err(error);
			}
			launched.push(json!({ "id": id, "label": label, "status": "running" }));
		}
		Ok(json!({ "wave": launched }))
	}

	fn launch_turn(
		&self,
		id: Str,
		label: Str,
		tier: WorkerTier,
		prompt: Str,
		generation: u64,
	) -> Result<(), Fault> {
		let board = self
			.parent
			.job_board()
			.ok_or_else(|| Fault::new("vibe requires the session async job manager"))?;
		let job_id = board.next_id();
		let job = JobRef {
			id:       job_id.clone(),
			owner:    JobOwner::AgentLoop { agent_id: id.clone() },
			metadata: Arc::new(JobMetadata::running(JobKind::Task, sf!("vibe:{}", label), now_ms())),
			artifact: ExpectedArtifact {
				description: sf!("durable vibe worker result"),
				media_type:  Some(sf!("application/vnd.omp.vibe-result+json")),
				lifetime:    ArtifactLifetime::Durable,
			},
		};
		if !board
			.try_register(job)
			.map_err(|error| Fault::new(format!("vibe job admission failed: {error}")))?
		{
			return Err(Fault::new("vibe job identifier collision"));
		}
		let parent = Arc::clone(&self.parent);
		let workers = Arc::clone(&self.workers);
		drop(tokio::spawn(async move {
			let kind = match tier {
				WorkerTier::Fast => "sonic",
				WorkerTier::Good => "task",
			};
			let mut args = json!({
				"prompt": prompt,
				"agent": kind,
				"stableId": id,
				"enableLsp": true,
			});
			if valid_worker_name(label.as_str()) {
				args["name"] = json!(label);
			}
			let result = parent
				.agent(args, &crate::envd::eval::NoopBridgeProgress)
				.await;
			let outcome = match result {
				Ok(value) => WorkerOutcome::Done(value),
				Err(error) => WorkerOutcome::Failed(Str::from(error.to_string())),
			};
			let delivery = delivery_text(&id, &outcome);
			let _ = board.settle(job_id.as_str(), system_item(delivery));
			let mut workers = workers.lock();
			let Some(worker) = workers.get_mut(&id) else {
				return;
			};
			if worker.generation != generation {
				return;
			}
			worker.running = false;
			worker.stopped = false;
			worker.outcome = Some(outcome);
			worker.notify.notify_waiters();
		}));
		Ok(())
	}

	fn status(&self, ids: &[Str]) -> Result<Value, Fault> {
		let ids = self.selected_ids(ids);
		let workers = self.workers.lock();
		let mut rows = Vec::with_capacity(ids.len());
		for id in ids {
			let worker = workers
				.get(&id)
				.ok_or_else(|| Fault::new(format!("unknown vibe worker: {id}")))?;
			let status = if worker.stopped {
				"stopped"
			} else if worker.running {
				"running"
			} else {
				match worker.outcome.as_ref() {
					Some(WorkerOutcome::Done(_)) => "idle",
					Some(WorkerOutcome::Failed(_)) => "failed",
					None => "parked",
				}
			};
			rows.push(json!({
				"id": id,
				"label": worker.label,
				"tier": worker.tier,
				"status": status,
				"generation": worker.generation,
			}));
		}
		Ok(json!({ "workers": rows }))
	}

	async fn steer(&self, id: Str, message: Str) -> Result<Value, Fault> {
		let running = self
			.workers
			.lock()
			.get(&id)
			.map(|worker| worker.running)
			.ok_or_else(|| Fault::new(format!("unknown vibe worker: {id}")))?;
		if running && self.parent.child_registry_status(id.as_str()) == Some(RegistryStatus::Running)
		{
			let session_id = self.parent.session_id();
			let receipts = self
				.parent
				.broker()
				.send(omp_agent::PeerMessage {
					id: Str::from(omp_core::Ulid::generate().to_string()),
					from: session_id.clone(),
					to: id.clone(),
					text: message,
					mode: omp_agent::DeliveryMode::Steer,
					reply_to: None,
					sent_ms: now_ms(),
					session_id,
					expects_reply: false,
				})
				.map_err(|error| Fault::new(error.to_string()))?;
			return Ok(json!({
				"id": id,
				"receipts": receipts.iter().map(ToString::to_string).collect::<Vec<_>>(),
			}));
		}

		let (label, tier, generation) = {
			let mut workers = self.workers.lock();
			let worker = workers
				.get_mut(&id)
				.ok_or_else(|| Fault::new(format!("unknown vibe worker: {id}")))?;
			worker.generation = worker.generation.saturating_add(1);
			worker.running = true;
			worker.stopped = false;
			worker.outcome = None;
			(worker.label.clone(), worker.tier, worker.generation)
		};
		if let Err(error) = self.launch_turn(id.clone(), label, tier, message, generation) {
			if let Some(worker) = self.workers.lock().get_mut(&id) {
				worker.running = false;
				worker.outcome = Some(WorkerOutcome::Failed(error.message.clone()));
				worker.notify.notify_waiters();
			}
			return Err(error);
		}
		Ok(json!({ "id": id, "status": "running", "generation": generation }))
	}

	async fn collect(&self, ids: &[Str], timeout_ms: Option<u64>) -> Result<Value, Fault> {
		let ids = self.selected_ids(ids);
		let mut rows = Vec::with_capacity(ids.len());
		for id in ids {
			let notify = {
				let workers = self.workers.lock();
				let worker = workers
					.get(&id)
					.ok_or_else(|| Fault::new(format!("unknown vibe worker: {id}")))?;
				worker.notify.clone()
			};
			let notified = notify.notified();
			let waiting = self
				.workers
				.lock()
				.get(&id)
				.is_some_and(|worker| worker.running);
			if waiting {
				if let Some(limit) = timeout_ms {
					if tokio::time::timeout(Duration::from_millis(limit), notified)
						.await
						.is_err()
					{
						rows.push(json!({ "id": id, "status": "running" }));
						continue;
					}
				} else {
					notified.await;
				}
			}
			let workers = self.workers.lock();
			let worker = workers
				.get(&id)
				.ok_or_else(|| Fault::new(format!("unknown vibe worker: {id}")))?;
			match worker.outcome.as_ref() {
				Some(WorkerOutcome::Done(value)) => {
					rows.push(json!({ "id": id, "result": value }));
				},
				Some(WorkerOutcome::Failed(error)) => {
					rows.push(json!({ "id": id, "error": error }));
				},
				None => rows.push(json!({
					"id": id,
					"status": if worker.stopped { "stopped" } else { "parked" },
				})),
			}
		}
		Ok(json!({ "workers": rows }))
	}

	async fn stop(&self, ids: &[Str]) -> Result<Value, Fault> {
		let ids = self.selected_ids(ids);
		let tree = self.parent.tree();
		for id in &ids {
			let mut workers = self.workers.lock();
			let worker = workers
				.get_mut(id)
				.ok_or_else(|| Fault::new(format!("unknown vibe worker: {id}")))?;
			worker.generation = worker.generation.saturating_add(1);
			worker.running = false;
			worker.stopped = true;
			worker.outcome = None;
			worker.notify.notify_waiters();
			drop(workers);
			self.parent.cancel_child(id.as_str());
			if let Some(node) = tree.node(id.as_str()) {
				node.set_status(AgentStatus::Cancelled);
			}
		}
		let release = async {
			for id in &ids {
				self.parent.release_child(id.as_str()).await;
			}
		};
		let _ = tokio::time::timeout(Duration::from_secs(5), release).await;
		Ok(json!({ "stopped": ids }))
	}

	async fn release_scope(&self) {
		let ids = self.selected_ids(&[]);
		let _ = self.stop(&ids).await;
	}
}

fn delivery_text(id: &str, outcome: &WorkerOutcome) -> Str {
	let (status, text) = match outcome {
		WorkerOutcome::Done(value) => (
			"settled",
			value
				.get("text")
				.and_then(Value::as_str)
				.map_or_else(|| value.to_string(), str::to_owned),
		),
		WorkerOutcome::Failed(error) => ("failed", error.to_string()),
	};
	let mut preview = text.chars().take(6_000).collect::<String>();
	if text.chars().count() > 6_000 {
		preview.push_str("\n[preview truncated]");
	}
	Str::from(format!("Vibe worker {id} {status}:\n{preview}\n\nFull output: agent://{id}"))
}

fn valid_worker_name(name: &str) -> bool {
	if name.len() > 32 {
		return false;
	}
	let mut bytes = name.bytes();
	bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic())
		&& bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn system_item(text: Str) -> Item {
	Item {
		seq:           0,
		created_at_ms: now_ms(),
		kind:          Some(item::Kind::Message(Message {
			role:  i32::from(Role::System),
			parts: vec![ThreadPart { kind: Some(part::Kind::Text(text.to_string())) }],
		})),
		props:         None,
	}
}

fn now_ms() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map_or(0, |duration| duration.as_millis() as u64)
}

#[async_trait]
impl<C: omp_agent::TurnClient + Clone + Send + 'static> VibeBackend for ChatVibeBackend<C> {
	async fn execute(&self, params: Params) -> Result<Value, Fault> {
		if self.modes.active() != ActiveMode::Vibe {
			return Err(Fault::new("vibe device requires /vibe on"));
		}
		match params.op {
			Operation::Spawn => self.spawn(params.wave).await,
			Operation::Status => self.status(&params.ids),
			Operation::Steer => {
				self
					.steer(
						params.id.expect("validated steer id"),
						params.message.expect("validated steer message"),
					)
					.await
			},
			Operation::Collect => self.collect(&params.ids, params.timeout_ms).await,
			Operation::Stop => self.stop(&params.ids).await,
		}
	}
}

/// Attaches the vibe device to one chat session.
pub(crate) fn attach_chat<C: omp_agent::TurnClient + Clone + Send + 'static>(
	parent: Arc<ChatParentHost<C>>,
	modes: Arc<ExecutionModes>,
) -> Attachment {
	attach(ChatVibeBackend::new(parent, modes))
}

fn param_event(error: ParamError) -> Ev<Update, Payload, Fault> {
	match error {
		ParamError::Args(issue) => Ev::Args(*issue),
		ParamError::Interrupted(interrupt) => {
			Ev::Aborted(Abort::Interrupted { reason: interrupt.reason })
		},
		ParamError::Protocol(message) => Ev::Args(protocol_issue(message)),
	}
}

fn commit_event(error: CommitError) -> Ev<Update, Payload, Fault> {
	match error {
		CommitError::Aborted => Ev::Aborted(Abort::InputDropped),
		CommitError::Interrupted(interrupt) => {
			Ev::Aborted(Abort::Interrupted { reason: interrupt.reason })
		},
		CommitError::Protocol(message) => Ev::Args(protocol_issue(message)),
	}
}

fn protocol_issue(message: Str) -> ArgIssue {
	ArgIssue {
		path:     Vec::new(),
		expected: sf!("one committed vibe operation object"),
		kind:     ArgIssueKind::Protocol,
		example:  Some(sf!(r#"{{"op":"status"}}"#)),
		found:    Some(message),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn one_closed_schema_covers_all_five_verbs() {
		for op in ["spawn", "status", "steer", "collect", "stop"] {
			let mut value = json!({ "op": op });
			if op == "spawn" {
				value["wave"] = json!([{ "brief": "inspect", "tier": "fast" }]);
			}
			if op == "steer" {
				value["id"] = json!("worker");
				value["message"] = json!("focus on errors");
			}
			let params: Params = serde_json::from_value(value).expect("valid verb shape");
			params.validate().expect("valid operation");
		}
		assert!(serde_json::from_value::<Params>(json!({ "op": "status", "extra": true })).is_err());
	}

	#[test]
	fn spawn_and_steer_reject_incomplete_shapes() {
		let spawn: Params = serde_json::from_value(json!({ "op": "spawn" })).expect("shape");
		assert!(spawn.validate().is_err());
		let steer: Params =
			serde_json::from_value(json!({ "op": "steer", "id": "worker" })).expect("shape");
		assert!(steer.validate().is_err());
	}
}
