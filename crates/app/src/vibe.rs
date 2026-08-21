//! Goal-directed swarm orchestration exposed as one dynamic device.

use std::{
	collections::BTreeMap,
	fmt,
	sync::{Arc, LazyLock},
	time::{SystemTime, UNIX_EPOCH},
};

use async_stream::stream;
use async_trait::async_trait;
use futures::Stream;
use omp_agent::AgentStatus;
use omp_core::{Str, sf};
use omp_tool::{
	Abort, ArgIssue, ArgIssueKind, CommitError, Constraint, Effects, Ev, IncomingParams, ParamError,
	Part, PromptCaps, Rev, Tool, ToolSpec, ToolTerminal,
};
use parking_lot::{Mutex, RwLock};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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

struct Worker {
	label:   Str,
	handle:  Option<tokio::task::JoinHandle<Result<Value, Fault>>>,
	result:  Option<Value>,
	stopped: bool,
}

/// Chat-scoped wave runner backed by the existing `omp.agents` parent-session
/// seam.
pub(crate) struct ChatVibeBackend<C: omp_agent::TurnClient + Clone + 'static> {
	parent:  Arc<ChatParentHost<C>>,
	modes:   Arc<ExecutionModes>,
	workers: Mutex<BTreeMap<Str, Worker>>,
}

impl<C: omp_agent::TurnClient + Clone + 'static> ChatVibeBackend<C> {
	/// Creates a wave runner for one interactive chat.
	#[must_use]
	pub(crate) const fn new(parent: Arc<ChatParentHost<C>>, modes: Arc<ExecutionModes>) -> Self {
		Self { parent, modes, workers: Mutex::new(BTreeMap::new()) }
	}

	fn selected_ids(&self, ids: &[Str]) -> Vec<Str> {
		if ids.is_empty() {
			self.workers.lock().keys().cloned().collect()
		} else {
			ids.to_vec()
		}
	}

	async fn spawn(&self, wave: Vec<WaveEntry>) -> Result<Value, Fault> {
		let mut launched = Vec::with_capacity(wave.len());
		for entry in wave {
			let id = Str::from(ulid::Ulid::generate().to_string());
			let label = entry.label.unwrap_or_else(|| id.clone());
			let parent = Arc::clone(&self.parent);
			let prompt = entry.brief;
			let kind = match entry.tier {
				WorkerTier::Fast => "sonic",
				WorkerTier::Good => "task",
			};
			let child_id = id.clone();
			let child_label = label.clone();
			let handle = tokio::spawn(async move {
				parent
					.agent(json!({
						"prompt": prompt,
						"agent": kind,
						"label": child_label,
						"_id": child_id,
					}))
					.await
					.map_err(|error| Fault::new(error.to_string()))
			});
			self.workers.lock().insert(id.clone(), Worker {
				label:   label.clone(),
				handle:  Some(handle),
				result:  None,
				stopped: false,
			});
			launched.push(json!({ "id": id, "label": label, "status": "running" }));
		}
		Ok(json!({ "wave": launched }))
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
			} else if worker.result.is_some() {
				"collected"
			} else if worker
				.handle
				.as_ref()
				.is_some_and(tokio::task::JoinHandle::is_finished)
			{
				"settled"
			} else {
				"running"
			};
			rows.push(json!({ "id": id, "label": worker.label, "status": status }));
		}
		Ok(json!({ "workers": rows }))
	}

	async fn steer(&self, id: Str, message: Str) -> Result<Value, Fault> {
		if !self.workers.lock().contains_key(&id) {
			return Err(Fault::new(format!("unknown vibe worker: {id}")));
		}
		let session_id = self.parent.session_id();
		let receipts = self
			.parent
			.broker()
			.send(omp_agent::PeerMessage {
				id: Str::from(ulid::Ulid::generate().to_string()),
				from: session_id.clone(),
				to: id.clone(),
				text: message,
				mode: omp_agent::DeliveryMode::Steer,
				reply_to: None,
				sent_ms: SystemTime::now()
					.duration_since(UNIX_EPOCH)
					.map_or(0, |duration| duration.as_millis() as u64),
				session_id,
			})
			.map_err(|error| Fault::new(error.to_string()))?;
		Ok(json!({
			"id": id,
			"receipts": receipts.iter().map(ToString::to_string).collect::<Vec<_>>(),
		}))
	}

	async fn collect(&self, ids: &[Str], timeout_ms: Option<u64>) -> Result<Value, Fault> {
		let ids = self.selected_ids(ids);
		let mut rows = Vec::with_capacity(ids.len());
		for id in ids {
			let (cached, mut handle) = {
				let mut workers = self.workers.lock();
				let worker = workers
					.get_mut(&id)
					.ok_or_else(|| Fault::new(format!("unknown vibe worker: {id}")))?;
				(worker.result.clone(), worker.handle.take())
			};
			if let Some(result) = cached {
				rows.push(json!({ "id": id, "result": result }));
				continue;
			}
			let Some(mut task) = handle.take() else {
				return Err(Fault::new(format!("vibe worker {id} has no collectable result")));
			};
			let joined = if let Some(limit) = timeout_ms {
				if let Ok(result) =
					tokio::time::timeout(std::time::Duration::from_millis(limit), &mut task).await
				{
					result
				} else {
					self
						.workers
						.lock()
						.get_mut(&id)
						.expect("selected worker remains registered")
						.handle = Some(task);
					rows.push(json!({ "id": id, "status": "running" }));
					continue;
				}
			} else {
				task.await
			};
			let result = joined
				.map_err(|error| Fault::new(format!("vibe worker {id} failed to join: {error}")))??;
			if let Some(worker) = self.workers.lock().get_mut(&id) {
				worker.result = Some(result.clone());
			}
			rows.push(json!({ "id": id, "result": result }));
		}
		Ok(json!({ "workers": rows }))
	}

	fn stop(&self, ids: &[Str]) -> Result<Value, Fault> {
		let ids = self.selected_ids(ids);
		let tree = self.parent.tree();
		let mut stopped = Vec::with_capacity(ids.len());
		let mut workers = self.workers.lock();
		for id in ids {
			let worker = workers
				.get_mut(&id)
				.ok_or_else(|| Fault::new(format!("unknown vibe worker: {id}")))?;
			if let Some(task) = worker.handle.take() {
				task.abort();
			}
			worker.stopped = true;
			if let Some(node) = tree.node(id.as_str()) {
				node.set_status(AgentStatus::Cancelled);
			}
			stopped.push(id);
		}
		Ok(json!({ "stopped": stopped }))
	}
}

#[async_trait]
impl<C: omp_agent::TurnClient + Clone + 'static> VibeBackend for ChatVibeBackend<C> {
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
			Operation::Stop => self.stop(&params.ids),
		}
	}
}

/// Attaches the vibe device to one chat session.
pub(crate) fn attach_chat<C: omp_agent::TurnClient + Clone + 'static>(
	parent: Arc<ChatParentHost<C>>,
	modes: Arc<ExecutionModes>,
) -> Attachment {
	attach(Arc::new(ChatVibeBackend::new(parent, modes)))
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
