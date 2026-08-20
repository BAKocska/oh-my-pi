//! Speculative environment invocations and ordered concurrent tool batches.

use std::{
	collections::BTreeMap,
	fmt,
	sync::{
		Arc, OnceLock,
		atomic::{AtomicBool, AtomicU8, AtomicU128, Ordering},
	},
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use futures::future::join_all;
use omp_core::{IntoStr, Str, StrMut};
use omp_env::{ClientError, EnvClient, Invocation, InvocationEvent};
use omp_proto::{
	env::v1::{Admission, AdmitInvocation, InvokeTool, Verdict as EnvVerdict},
	inference::v1 as value_pb,
	policy::v1::EffectEnvelope,
	thread::v1::{Item, Part as CanonicalPart},
	toolhost::v1::HookEventId,
};
use omp_tool::{
	Abort, ArgIssue, ArgPath, CallOutcome, CallOutcomeDetails, CapsBase, Effects, JobRef, Part,
	PromptCaps, Registry, ToolIdentity, ToolTerminal,
};
use serde_json::Value;
use tokio::sync::{Notify, watch};

use crate::{
	events::{AgentEvent, EventBus},
	project::{tool_result_item, tool_result_item_canonical_parts},
};

/// Namespaced invocation property carrying the environment-enforced mode.
pub const EXECUTION_MODE_PROP: &str = "omp/execution-mode";
/// Namespaced authorization for the one plan-to-execution transition.
pub const PLAN_YOLO_PROP: &str = "omp/plan-yolo";
/// Namespaced explanation for an automatic prewalk transition.
pub const PREWALK_REASON_PROP: &str = "omp/prewalk-reason";

/// Mutually exclusive application execution mode projected onto each
/// invocation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum ExecutionMode {
	/// Ordinary interactive execution.
	#[default]
	Standard = 0,
	/// Read-only planning enforced by the Environment.
	Plan     = 1,
	/// Planning which may make one env-authorized transition on first mutation.
	PlanYolo = 2,
	/// Goal auto-continuation.
	Goal     = 3,
	/// Director/worker vibe orchestration.
	Vibe     = 4,
	/// Cheap prewalk reasoning until the first mutation.
	Prewalk  = 5,
}

/// Cloneable atomic mode handle shared by the application and loop.
#[derive(Clone, Debug, Default)]
pub struct ExecutionModeHandle(Arc<AtomicU8>);

impl ExecutionModeHandle {
	/// Replaces the current mutually exclusive execution mode.
	pub fn set(&self, mode: ExecutionMode) {
		self.0.store(mode as u8, Ordering::Release);
	}

	/// Returns the current execution mode.
	#[must_use]
	pub fn get(&self) -> ExecutionMode {
		match self.0.load(Ordering::Acquire) {
			1 => ExecutionMode::Plan,
			2 => ExecutionMode::PlanYolo,
			3 => ExecutionMode::Goal,
			4 => ExecutionMode::Vibe,
			5 => ExecutionMode::Prewalk,
			_ => ExecutionMode::Standard,
		}
	}

	/// Builds immutable invocation metadata and performs one-way prewalk/yolo
	/// automation on the first mutating tool.
	#[must_use]
	pub fn invocation_props(&self, effects: &Effects) -> value_pb::ValueMap {
		let mut mode = self.get();
		let mut fields = BTreeMap::new();
		if effects_mutate_environment(effects) {
			match mode {
				ExecutionMode::PlanYolo => {
					if self
						.0
						.compare_exchange(
							ExecutionMode::PlanYolo as u8,
							ExecutionMode::Standard as u8,
							Ordering::AcqRel,
							Ordering::Acquire,
						)
						.is_ok()
					{
						fields.insert(PLAN_YOLO_PROP.to_owned(), bool_value(true));
					} else {
						mode = self.get();
					}
				},
				ExecutionMode::Prewalk => {
					if self
						.0
						.compare_exchange(
							ExecutionMode::Prewalk as u8,
							ExecutionMode::Standard as u8,
							Ordering::AcqRel,
							Ordering::Acquire,
						)
						.is_ok()
					{
						fields.insert(
							PREWALK_REASON_PROP.to_owned(),
							string_value("first mutating environment effect"),
						);
					} else {
						mode = self.get();
					}
				},
				_ => {},
			}
		}
		let label = match mode {
			ExecutionMode::Standard => "standard",
			ExecutionMode::Plan | ExecutionMode::PlanYolo => "plan",
			ExecutionMode::Goal => "goal",
			ExecutionMode::Vibe => "vibe",
			ExecutionMode::Prewalk => "prewalk",
		};
		fields.insert(EXECUTION_MODE_PROP.to_owned(), string_value(label));
		value_pb::ValueMap { fields }
	}
}

/// Returns whether an effect envelope may mutate Environment-owned state.
#[must_use]
pub fn effects_mutate_environment(effects: &Effects) -> bool {
	effects
		.documents
		.as_ref()
		.is_some_and(|documents| !documents.write_globs.is_empty())
		|| effects.exec.as_ref().is_some_and(|exec| !exec.is_empty())
		|| effects.subagents != 0
}

fn string_value(value: &'static str) -> value_pb::Value {
	value_pb::Value { kind: Some(value_pb::value::Kind::String(value.to_owned())) }
}

fn bool_value(value: bool) -> value_pb::Value {
	value_pb::Value { kind: Some(value_pb::value::Kind::Bool(value)) }
}

/// Failure to open, relay, decode, project, or lower a tool invocation.
#[derive(Debug)]
pub enum BatchError {
	/// The environment channel rejected an operation.
	Environment(ClientError),
	/// A terminal environment payload was not a supported structured outcome.
	InvalidOutcome(serde_json::Error),
	/// Canonical result construction failed.
	Projection(Str),
}

impl fmt::Display for BatchError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Environment(error) => write!(formatter, "environment invocation failed: {error}"),
			Self::InvalidOutcome(error) => write!(formatter, "invalid tool outcome: {error}"),
			Self::Projection(error) => write!(formatter, "canonical tool result failed: {error}"),
		}
	}
}

impl std::error::Error for BatchError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Environment(error) => Some(error),
			Self::InvalidOutcome(error) => Some(error),
			Self::Projection(_) => None,
		}
	}
}

impl From<ClientError> for BatchError {
	fn from(error: ClientError) -> Self {
		Self::Environment(error)
	}
}
/// Returns the subscription-mask bit for one stable hook event id.
#[must_use]
pub const fn hook_event_mask(event: HookEventId) -> u128 {
	1_u128 << event as u32
}

/// One hook-composed admission answer and its narrowed authority envelope.
#[derive(Clone, Debug)]
pub struct InvocationAdmission {
	/// Environment admission receipt.
	pub admission: Admission,
	/// Authority no wider than the tool revision's declared maximum.
	pub effects:   Effects,
}

/// One allocation-free-negative-path handoff from an invocation to hook
/// CONTROL.
#[derive(Debug)]
pub enum InvocationHookRequest {
	/// Exact raw provider argument text, emitted before the environment document
	/// feed observes the fragment.
	ArgText {
		/// Transcript-visible invocation identity.
		invocation_id: Str,
		/// The one shared fragment clone made for subscribed hooks.
		fragment:      Str,
	},
	/// Per-invocation admission query, declared authority ceiling, and unique
	/// reply channel.
	Admission {
		/// Boxed because `AdmitInvocation` is a foreign generated prost message;
		/// one allocation is paid per hook-subscribed admission.
		query:           Box<AdmitInvocation>,
		/// Maximum authority declared by the resolved tool revision.
		maximum_effects: Effects,
		/// One-shot response consumed only by this invocation.
		reply:           flume::Sender<InvocationAdmission>,
	},
}

/// Atomic union-mask and hook request sender shared by invocation pumps.
#[derive(Clone, Debug)]
pub struct InvocationHookBus {
	union: Arc<AtomicU128>,
	tx:    flume::Sender<InvocationHookRequest>,
}

impl InvocationHookBus {
	/// Creates a hook bus and its single CONTROL-side request receiver.
	#[must_use]
	pub fn channel() -> (Self, flume::Receiver<InvocationHookRequest>) {
		let (tx, rx) = flume::unbounded();
		(Self { union: Arc::new(AtomicU128::new(0)), tx }, rx)
	}

	/// Replaces the registered union mask in one atomic publication.
	pub fn replace_union_mask(&self, mask: u128) {
		self.union.store(mask, Ordering::Release);
	}

	/// Returns the currently published union mask.
	#[must_use]
	pub fn union_mask(&self) -> u128 {
		self.union.load(Ordering::Acquire)
	}

	fn subscribed(&self, event: HookEventId) -> bool {
		self.union.load(Ordering::Relaxed) & hook_event_mask(event) != 0
	}

	fn arg_text(&self, invocation_id: &Str, fragment: &Str) {
		if self.subscribed(HookEventId::HookEventToolCall) {
			let _ = self.tx.send(InvocationHookRequest::ArgText {
				invocation_id: invocation_id.clone(),
				fragment:      fragment.clone(),
			});
		}
	}

	async fn admit(&self, query: AdmitInvocation, maximum_effects: Effects) -> InvocationAdmission {
		let (reply, receive) = flume::bounded(1);
		let decision = if self.subscribed(HookEventId::HookEventToolCall) {
			if self
				.tx
				.send(InvocationHookRequest::Admission {
					query: Box::new(query.clone()),
					maximum_effects: maximum_effects.clone(),
					reply,
				})
				.is_ok()
			{
				receive.recv_async().await.ok()
			} else {
				None
			}
		} else {
			Some(InvocationAdmission {
				admission: allowed_admission(&query),
				effects:   maximum_effects.clone(),
			})
		};
		match decision {
			Some(mut decision) if decision.effects.is_subset_of(&maximum_effects) => {
				if !decision.admission.allow {
					decision.effects = Effects::empty();
				}
				decision
			},
			_ => {
				InvocationAdmission { admission: denied_admission(&query), effects: Effects::empty() }
			},
		}
	}
}

fn allowed_admission(query: &AdmitInvocation) -> Admission {
	Admission { invocation_id: query.invocation_id.clone(), allow: true, ..Admission::default() }
}

fn denied_admission(query: &AdmitInvocation) -> Admission {
	Admission { invocation_id: query.invocation_id.clone(), allow: false, ..Admission::default() }
}
#[derive(Clone, Debug)]
pub(crate) struct InvocationAdmissionFact {
	pub(crate) invocation_id: Str,
	pub(crate) raw:           Str,
	pub(crate) admission:     Admission,
}

enum PumpCommand {
	ArgText {
		fragment: Str,
		ack:      flume::Sender<Result<(), ClientError>>,
	},
	Authorize {
		raw:              Bytes,
		effect_token:     Bytes,
		authorized_at_ms: u64,
		effects:          Effects,
		ack:              flume::Sender<Result<AuthorizationState, ClientError>>,
	},
	Interrupt {
		reason: Str,
		ack:    flume::Sender<Result<(), ClientError>>,
	},
	Cancel {
		ack: flume::Sender<()>,
	},
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum AuthorizationState {
	Sent,
	DeliveryIndeterminate,
}

struct AuthorizationReceipt(flume::Receiver<Result<AuthorizationState, ClientError>>);

impl AuthorizationReceipt {
	async fn wait(&self) -> Result<AuthorizationState, BatchError> {
		Ok(self
			.0
			.recv_async()
			.await
			.map_err(|_| InvocationPump::closed())??)
	}
}

struct CommandReceipt(flume::Receiver<Result<(), ClientError>>);

impl CommandReceipt {
	async fn wait(&self) -> Result<(), BatchError> {
		self
			.0
			.recv_async()
			.await
			.map_err(|_| InvocationPump::closed())??;
		Ok(())
	}
}

enum PumpTerminal {
	Verdict(EnvVerdict),
	ClientError(ClientError),
	Closed,
	CancelUnobserved,
}

enum PumpOutput {
	Update(Bytes),
	Terminal(PumpTerminal),
}
struct InterruptRequest {
	reason:       Str,
	acknowledged: flume::Sender<()>,
}

struct InvocationPump {
	commands:        flume::Sender<PumpCommand>,
	outputs:         flume::Receiver<PumpOutput>,
	hooks:           Arc<OnceLock<InvocationHookBus>>,
	maximum_effects: Arc<OnceLock<Effects>>,
	maximum_ready:   Arc<Notify>,
	admission:       Arc<OnceLock<Admission>>,
	effects:         Arc<OnceLock<Effects>>,
	facts:           Arc<OnceLock<flume::Sender<InvocationAdmissionFact>>>,
	cancelled:       Arc<AtomicBool>,
}

impl InvocationPump {
	async fn arg_text(&self, fragment: Str) -> Result<(), BatchError> {
		let (ack, reply) = flume::bounded(1);
		self.send(PumpCommand::ArgText { fragment, ack })?;
		reply.recv_async().await.map_err(|_| Self::closed())??;
		Ok(())
	}

	fn begin_authorization(
		&self,
		raw: Bytes,
		effect_token: Bytes,
		authorized_at_ms: u64,
		effects: Effects,
	) -> Result<AuthorizationReceipt, BatchError> {
		let (ack, reply) = flume::bounded(1);
		self.send(PumpCommand::Authorize { raw, effect_token, authorized_at_ms, effects, ack })?;
		Ok(AuthorizationReceipt(reply))
	}

	fn begin_interrupt(&self, reason: Str) -> Result<CommandReceipt, BatchError> {
		let (ack, reply) = flume::bounded(1);
		self.send(PumpCommand::Interrupt { reason, ack })?;
		Ok(CommandReceipt(reply))
	}

	async fn cancel(&self) -> Result<(), BatchError> {
		let (ack, reply) = flume::bounded(1);
		self.send(PumpCommand::Cancel { ack })?;
		reply.recv_async().await.map_err(|_| Self::closed())
	}

	fn send(&self, command: PumpCommand) -> Result<(), BatchError> {
		self.commands.send(command).map_err(|_| Self::closed())
	}

	const fn closed() -> BatchError {
		BatchError::Projection(Str::new_static("environment invocation pump closed"))
	}

	async fn output(&self) -> PumpOutput {
		self
			.outputs
			.recv_async()
			.await
			.unwrap_or(PumpOutput::Terminal(PumpTerminal::Closed))
	}
}

enum InterruptAction {
	Sent(Result<(), ClientError>),
	Cancel(flume::Sender<()>),
	Unsupported,
	Closed,
}

async fn handle_interrupt(
	invocation: &Invocation,
	reason: Str,
	ack: flume::Sender<Result<(), ClientError>>,
	command_rx: &flume::Receiver<PumpCommand>,
) -> bool {
	let action = {
		let sent = invocation.interrupt(reason);
		tokio::pin!(sent);
		tokio::select! {
			result = &mut sent => InterruptAction::Sent(result),
			control = command_rx.recv_async() => match control {
				Ok(PumpCommand::Cancel { ack }) => InterruptAction::Cancel(ack),
				Ok(_) => InterruptAction::Unsupported,
				Err(_) => InterruptAction::Closed,
			},
		}
	};
	match action {
		InterruptAction::Sent(result) => {
			let failed = result.is_err();
			let _ = ack.send(result);
			failed
		},
		InterruptAction::Cancel(cancel_ack) => {
			invocation.guard().cancel();
			let _ = cancel_ack.send(());
			false
		},
		InterruptAction::Unsupported | InterruptAction::Closed => true,
	}
}

enum AuthorizationAction {
	Sent(Result<(), ClientError>),
	Control(PumpCommand),
	Closed,
}

fn spawn_invocation_pump(
	mut invocation: Invocation,
	call_id: Str,
	events: EventBus,
) -> InvocationPump {
	let (commands, command_rx) = flume::unbounded();
	let (output_tx, outputs) = flume::unbounded();
	let hooks: Arc<OnceLock<InvocationHookBus>> = Arc::new(OnceLock::new());
	let task_hooks = Arc::clone(&hooks);
	let maximum_effects: Arc<OnceLock<Effects>> = Arc::new(OnceLock::new());
	let task_maximum_effects = Arc::clone(&maximum_effects);
	let maximum_ready = Arc::new(Notify::new());
	let task_maximum_ready = Arc::clone(&maximum_ready);
	let admission: Arc<OnceLock<Admission>> = Arc::new(OnceLock::new());
	let task_admission = Arc::clone(&admission);
	let effects: Arc<OnceLock<Effects>> = Arc::new(OnceLock::new());
	let task_effects = Arc::clone(&effects);
	let facts: Arc<OnceLock<flume::Sender<InvocationAdmissionFact>>> = Arc::new(OnceLock::new());
	let task_facts = Arc::clone(&facts);
	let cancelled = Arc::new(AtomicBool::new(false));
	let task_cancelled = Arc::clone(&cancelled);
	tokio::spawn(async move {
		let mut args_text = StrMut::default();
		loop {
			tokio::select! {
				command = command_rx.recv_async() => {
					let Ok(command) = command else { break };
					match command {
						PumpCommand::ArgText { fragment, ack } => {
							let fragment_start = args_text.len();
							args_text.push_str(&fragment);
							let result = invocation.arg_text(fragment).await;
							if result.is_ok() {
								let view = omp_slopjson::parse_streaming(args_text.as_str());
								events.publish(AgentEvent::ToolArgs {
									call_id: call_id.clone(),
									fragment: Bytes::copy_from_slice(
										&args_text.as_str().as_bytes()[fragment_start..],
									),
									view,
								});
							} else {
								args_text.truncate(fragment_start);
							}
							let failed = result.is_err();
							let _ = ack.send(result);
							if failed {
								break;
							}
						},
						PumpCommand::Authorize {
							raw,
							effect_token,
							authorized_at_ms,
							effects,
							ack,
						} => {
							let action = {
								let sent = invocation.commit_args(
									raw,
									effect_token,
									authorized_at_ms,
									Some(EffectEnvelope::from(&effects)),
								);
								tokio::pin!(sent);
								tokio::select! {
									result = &mut sent => AuthorizationAction::Sent(result),
									control = command_rx.recv_async() => match control {
										Ok(control) => AuthorizationAction::Control(control),
										Err(_) => AuthorizationAction::Closed,
									},
								}
							};
							match action {
								AuthorizationAction::Sent(result) => {
									let result = result.map(|()| AuthorizationState::Sent);
									let failed = result.is_err();
									let _ = ack.send(result);
									if failed {
										break;
									}
								},
								AuthorizationAction::Control(command) => match command {
									PumpCommand::Interrupt { reason, ack: interrupt_ack } => {
										let _ = ack.send(Ok(AuthorizationState::DeliveryIndeterminate));
										if handle_interrupt(
											&invocation,
											reason,
											interrupt_ack,
											&command_rx,
										)
										.await
										{
											break;
										}
									},
									PumpCommand::Cancel { ack: cancel_ack } => {
										let _ = ack.send(Ok(AuthorizationState::DeliveryIndeterminate));
										invocation.guard().cancel();
										let _ = cancel_ack.send(());
									},
									command => {
										drop(command);
										drop(ack);
										break;
									},
								},
								AuthorizationAction::Closed => break,
							}
						},
						PumpCommand::Interrupt { reason, ack } => {
							if handle_interrupt(&invocation, reason, ack, &command_rx).await {
								break;
							}
						},
						PumpCommand::Cancel { ack } => {
							invocation.guard().cancel();
							let _ = ack.send(());
						},
					}
				},
				event = invocation.next_event() => match event {
					Ok(Some(InvocationEvent::Accepted(_))) => {},
					Ok(Some(InvocationEvent::Admission(query))) => {
						let maximum = loop {
							if let Some(maximum) = task_maximum_effects.get() {
								break maximum.clone();
							}
							task_maximum_ready.notified().await;
						};
						let decision = match task_hooks.get() {
							Some(hooks) => hooks.admit(query.clone(), maximum.clone()).await,
							None => InvocationAdmission {
								admission: allowed_admission(&query),
								effects: maximum,
							},
						};
						let _ = task_admission.set(decision.admission.clone());
						let _ = task_effects.set(decision.effects.clone());
						if let Some(facts) = task_facts.get() {
							let _ = facts.send(InvocationAdmissionFact {
								invocation_id: call_id.clone(),
								raw:           args_text.as_str().to_str(),
								admission:     decision.admission.clone(),
							});
						}
						tokio::task::yield_now().await;
						if task_cancelled.load(Ordering::Acquire) {
							invocation.guard().cancel();
							break;
						}
						if let Err(error) = invocation.admit(decision.admission).await {
							let _ = output_tx.send(PumpOutput::Terminal(
								PumpTerminal::ClientError(error),
							));
							break;
						}
					},
					Ok(Some(InvocationEvent::Update(update))) => {
						let json = update.json;
						events.publish(AgentEvent::ToolUpdate {
							call_id: call_id.clone(),
							json: json.clone(),
						});
						let _ = output_tx.send(PumpOutput::Update(json));
					},
					Ok(Some(InvocationEvent::Verdict(verdict))) => {
						let _ = output_tx.send(PumpOutput::Terminal(
							PumpTerminal::Verdict(verdict),
						));
						break;
					},
					Ok(None) => {
						let _ = output_tx.send(PumpOutput::Terminal(PumpTerminal::Closed));
						break;
					},
					Err(error) => {
						let _ = output_tx.send(PumpOutput::Terminal(
							PumpTerminal::ClientError(error),
						));
						break;
					},
				},
			}
		}
	});
	InvocationPump {
		commands,
		outputs,
		hooks,
		maximum_effects,
		maximum_ready,
		admission,
		effects,
		facts,
		cancelled,
	}
}

/// An environment invocation opened before its model arguments are committed.
///
/// Relaying fragments may prepare environment-owned resources, but only
/// [`commit`](Self::commit) creates a call eligible to send `ArgsCommitted`.
/// Dropping this handle structurally cancels the uncommitted invocation.
pub struct SpeculativeCall {
	inner: Option<SpeculativeCallInner>,
}

struct SpeculativeCallInner {
	call_id:  Str,
	identity: ToolIdentity,
	pump:     InvocationPump,
	events:   EventBus,
}

impl SpeculativeCall {
	/// Opens an environment invocation without mode metadata.
	pub async fn open(
		env: &EnvClient,
		events: &EventBus,
		call_id: Str,
		identity: ToolIdentity,
		deadline: Duration,
	) -> Result<Self, BatchError> {
		Self::open_with_props(env, events, call_id, identity, deadline, Default::default()).await
	}

	/// Opens an invocation carrying immutable environment policy metadata.
	pub async fn open_with_props(
		env: &EnvClient,
		events: &EventBus,
		call_id: Str,
		identity: ToolIdentity,
		deadline: Duration,
		props: value_pb::ValueMap,
	) -> Result<Self, BatchError> {
		let invocation = env
			.invoke(InvokeTool {
				invocation_id: call_id.to_string(),
				name:          identity.name.to_string(),
				rev:           identity.rev.to_string(),
				deadline_ms:   u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX),
				props:         Some(props),
			})
			.await?;
		events.publish(AgentEvent::ToolOpened {
			call_id: call_id.clone(),
			name:    identity.name.clone(),
			rev:     identity.rev.clone(),
		});
		let pump = spawn_invocation_pump(invocation, call_id.clone(), events.clone());
		Ok(Self {
			inner: Some(SpeculativeCallInner { call_id, identity, pump, events: events.clone() }),
		})
	}

	/// Returns the stable model-authored call identifier.
	pub fn call_id(&self) -> &Str {
		&self.inner.as_ref().expect("live speculative call").call_id
	}

	/// Returns the exact live tool identity selected when speculation opened.
	pub fn identity(&self) -> &ToolIdentity {
		&self.inner.as_ref().expect("live speculative call").identity
	}

	/// Installs the loop-owned hook, authority ceiling, and durable fact bus.
	pub(crate) fn attach_runtime(
		&self,
		hooks: InvocationHookBus,
		facts: flume::Sender<InvocationAdmissionFact>,
		maximum_effects: Effects,
	) -> Result<(), BatchError> {
		let pump = &self.inner.as_ref().expect("live speculative call").pump;
		pump
			.hooks
			.set(hooks)
			.map_err(|_| BatchError::Projection(Str::new_static("invocation hook bus already set")))?;
		pump.maximum_effects.set(maximum_effects).map_err(|_| {
			BatchError::Projection(Str::new_static("invocation effect maximum already set"))
		})?;
		pump
			.facts
			.set(facts)
			.map_err(|_| BatchError::Projection(Str::new_static("invocation fact bus already set")))?;
		pump.maximum_ready.notify_one();
		Ok(())
	}

	/// Queues one provider argument fragment verbatim for the invocation owner.
	///
	/// Subscribed hooks observe the raw fragment before the environment document
	/// feed. The negative path performs one atomic load and no clone.
	pub async fn relay_fragment(&mut self, fragment: Str) -> Result<(), BatchError> {
		let inner = self.inner.as_ref().expect("live speculative call");
		if let Some(hooks) = inner.pump.hooks.get() {
			hooks.arg_text(&inner.call_id, &fragment);
		}
		inner.pump.arg_text(fragment).await
	}

	/// Returns the admission receipt fixed by the environment, when available.
	pub(crate) fn admission(&self) -> Option<&Admission> {
		self
			.inner
			.as_ref()
			.expect("live speculative call")
			.pump
			.admission
			.get()
	}

	/// Records the durable assistant-item commitment for this invocation.
	///
	/// This local transition performs no I/O. Effect authorization is sent only
	/// by [`ToolBatch::drive`] after the loop journals the token and timestamp.
	pub fn commit(mut self, raw_args: Bytes) -> CommittedCall {
		let effect_token = ulid::Ulid::generate().to_string().to_str();
		let authorized_at_ms = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.unwrap_or_default()
			.as_millis()
			.try_into()
			.unwrap_or(u64::MAX);
		let SpeculativeCallInner { call_id, identity, pump, events } =
			self.inner.take().expect("live speculative call");
		let effects = pump.effects.get().cloned().unwrap_or_default();
		CommittedCall {
			call_id,
			identity,
			raw_args,
			effect_token,
			authorized_at_ms,
			effects,
			pump,
			events,
		}
	}
}

impl Drop for SpeculativeCall {
	fn drop(&mut self) {
		let Some(inner) = self.inner.as_ref() else {
			return;
		};
		inner.pump.cancelled.store(true, Ordering::Release);
		let (acknowledged, _) = flume::bounded(1);
		let _ = inner.pump.send(PumpCommand::Cancel { ack: acknowledged });
	}
}

/// An assistant-item-committed call waiting for effect authorization.
pub struct CommittedCall {
	call_id:          Str,
	identity:         ToolIdentity,
	raw_args:         Bytes,
	effect_token:     Str,
	authorized_at_ms: u64,
	effects:          Effects,
	pump:             InvocationPump,
	events:           EventBus,
}

impl CommittedCall {
	/// Returns the stable model-authored call identifier.
	pub const fn call_id(&self) -> &Str {
		&self.call_id
	}

	/// Returns the exact committed model argument bytes.
	pub const fn raw_args(&self) -> &Bytes {
		&self.raw_args
	}

	/// Returns the tool identity fixed when speculation opened.
	pub const fn identity(&self) -> &ToolIdentity {
		&self.identity
	}

	/// Returns the unforgeable token issued for this invocation's effect scope.
	pub const fn effect_token(&self) -> &Str {
		&self.effect_token
	}

	/// Returns the epoch-millisecond effect-authorization timestamp.
	pub const fn authorized_at_ms(&self) -> u64 {
		self.authorized_at_ms
	}

	/// Returns the exact Core-narrowed authority envelope.
	pub const fn effects(&self) -> &Effects {
		&self.effects
	}
}

/// One exact serialized tool update emitted while a batch call is live.
#[derive(Clone, Debug)]
pub struct BatchUpdate {
	pub(crate) call_id:  Str,
	pub(crate) identity: ToolIdentity,
	pub(crate) json:     Bytes,
}

/// One ordered batch completion shared with the event feed.
#[derive(Clone)]
pub struct BatchResult {
	event:   Arc<AgentEvent>,
	job:     Option<JobRef>,
	outcome: Option<CallOutcome<CallOutcomeDetails, CallOutcomeDetails>>,
}

impl BatchResult {
	/// Borrows the canonical result item carried by this completion's event.
	pub fn item(&self) -> &Item {
		match self.event.as_ref() {
			AgentEvent::ToolFinished { item, .. } => item,
			_ => unreachable!("batch results only retain ToolFinished events"),
		}
	}

	/// Returns the transcript-visible invocation identity.
	pub fn call_id(&self) -> &Str {
		match self.event.as_ref() {
			AgentEvent::ToolFinished { call_id, .. } => call_id,
			_ => unreachable!("batch results only retain ToolFinished events"),
		}
	}

	/// Borrows the already-published immutable result event.
	pub const fn event(&self) -> &Arc<AgentEvent> {
		&self.event
	}

	/// Returns detached job ownership when work outlives the turn.
	pub const fn job(&self) -> Option<&JobRef> {
		self.job.as_ref()
	}

	/// Borrows the canonical four-arm durable outcome fixed at settlement.
	pub const fn outcome(&self) -> Option<&CallOutcome<CallOutcomeDetails, CallOutcomeDetails>> {
		self.outcome.as_ref()
	}

	/// Takes detached job ownership for registration with the job board.
	pub fn into_job(self) -> Option<JobRef> {
		self.job
	}

	/// Returns whether this completion transferred work to the job board.
	pub const fn is_detached(&self) -> bool {
		self.job.is_some()
	}
}

/// A set of committed calls driven concurrently and returned in issued order.
pub struct ToolBatch {
	calls: Vec<CommittedCall>,
}

impl ToolBatch {
	/// Creates a batch in model-issued order.
	pub const fn new(calls: Vec<CommittedCall>) -> Self {
		Self { calls }
	}

	/// Returns the number of calls in the batch.
	pub const fn len(&self) -> usize {
		self.calls.len()
	}

	/// Returns whether the batch contains no calls.
	pub const fn is_empty(&self) -> bool {
		self.calls.is_empty()
	}

	/// Sends every effect authorization and drives all calls concurrently.
	///
	/// Results remain in issued order. Once a call is authorized, environment
	/// or lowering failures become canonical `EffectsUnknown` results so every
	/// committed call remains journalable and peer truth is never discarded.
	pub async fn drive(self, registry: &Registry, caps: &CapsBase) -> Vec<BatchResult> {
		self
			.drive_inner(registry, caps, None, Duration::ZERO, None)
			.await
	}

	/// Drives the batch with one watch-broadcast cooperative interrupt source.
	pub async fn drive_interruptible(
		self,
		registry: &Registry,
		caps: &CapsBase,
		interrupt: watch::Receiver<Option<Str>>,
		grace: Duration,
	) -> Vec<BatchResult> {
		self
			.drive_inner(registry, caps, Some(interrupt), grace, None)
			.await
	}

	/// Drives an interruptible batch while forwarding each queued update once.
	pub(crate) async fn drive_streaming(
		self,
		registry: &Registry,
		caps: &CapsBase,
		interrupt: watch::Receiver<Option<Str>>,
		grace: Duration,
		updates: flume::Sender<BatchUpdate>,
	) -> Vec<BatchResult> {
		self
			.drive_inner(registry, caps, Some(interrupt), grace, Some(updates))
			.await
	}

	async fn drive_inner(
		self,
		registry: &Registry,
		caps: &CapsBase,
		mut interrupt: Option<watch::Receiver<Option<Str>>>,
		grace: Duration,
		updates: Option<flume::Sender<BatchUpdate>>,
	) -> Vec<BatchResult> {
		if let Some(reason) = interrupt
			.as_mut()
			.and_then(|receiver| receiver.borrow_and_update().clone())
		{
			let reason = format!("interrupted before execution: {reason}").to_str();
			return self
				.calls
				.iter()
				.map(|call| lower_abort_total(call, Abort::Skipped { reason: reason.clone() }))
				.collect();
		}

		let mut interrupt_senders = Vec::with_capacity(self.calls.len());
		let mut calls = Vec::with_capacity(self.calls.len());
		for (index, call) in self.calls.into_iter().enumerate() {
			let (interrupt_tx, interrupt_rx) = flume::bounded(1);
			interrupt_senders.push(interrupt_tx);
			calls.push(run_call(index, call, registry, caps, interrupt_rx, grace, updates.clone()));
		}

		let drive = join_all(calls);
		let results = if let Some(mut interrupt) = interrupt {
			let coordinate = coordinate_interrupts(&mut interrupt, &interrupt_senders, grace);
			tokio::pin!(drive, coordinate);
			tokio::select! {
				results = &mut drive => results,
				() = &mut coordinate => drive.await,
			}
		} else {
			drive.await
		};
		results.into_iter().map(|(_, result)| result).collect()
	}
}

async fn run_call(
	index: usize,
	call: CommittedCall,
	registry: &Registry,
	caps: &CapsBase,
	interrupt: flume::Receiver<InterruptRequest>,
	grace: Duration,
	updates: Option<flume::Sender<BatchUpdate>>,
) -> (usize, BatchResult) {
	let receipt = match call.pump.begin_authorization(
		call.raw_args.clone(),
		Bytes::copy_from_slice(call.effect_token.as_bytes()),
		call.authorized_at_ms,
		call.effects.clone(),
	) {
		Ok(receipt) => receipt,
		Err(error) => {
			let reason = format!("effect authorization delivery failed: {error}").to_str();
			return (index, lower_abort_total(&call, Abort::EffectsUnknown { reason }));
		},
	};
	let mut pending_interrupt = None;
	let mut terminal_during_authorization = None;
	let mut authorization_failure = None;
	let authorization = tokio::select! {
		biased;
		request = wait_for_ordered_interrupt(&interrupt) => {
			match call.pump.begin_interrupt(request.reason) {
				Ok(interrupt_receipt) => {
					pending_interrupt = Some((interrupt_receipt, request.acknowledged));
					receipt.wait().await
				},
				Err(error) => {
					drop(request.acknowledged);
					authorization_failure =
						Some(format!("failed to interrupt pending authorization: {error}").to_str());
					terminal_during_authorization =
						Some(drain_pump(&call, updates.as_ref()).await);
					Ok(AuthorizationState::DeliveryIndeterminate)
				},
			}
		},
		result = receipt.wait() => result,
	};
	let authorization_indeterminate = match authorization {
		Ok(AuthorizationState::Sent) => false,
		Ok(AuthorizationState::DeliveryIndeterminate) => true,
		Err(error) => {
			authorization_failure =
				Some(format!("effect authorization delivery failed: {error}").to_str());
			terminal_during_authorization = Some(drain_pump(&call, updates.as_ref()).await);
			true
		},
	};

	let terminal = if let Some(terminal) = terminal_during_authorization {
		terminal
	} else if let Some((receipt, acknowledged)) = pending_interrupt {
		finish_interrupt_with_grace(&call, updates.as_ref(), receipt, acknowledged, grace).await
	} else {
		tokio::select! {
			biased;
			request = wait_for_ordered_interrupt(&interrupt) => {
				interrupt_pump_with_grace(&call, updates.as_ref(), request, grace).await
			},
			terminal = drain_pump(&call, updates.as_ref()) => terminal,
		}
	};
	let result = match terminal {
		PumpTerminal::Verdict(verdict) => lower_verdict(&call, registry, *caps, verdict)
			.unwrap_or_else(|error| {
				lower_abort_total(&call, Abort::EffectsUnknown {
					reason: format!("failed to lower environment verdict: {error}").to_str(),
				})
			}),
		PumpTerminal::Closed => {
			if let Some(reason) = authorization_failure {
				lower_abort_total(&call, Abort::EffectsUnknown { reason })
			} else if authorization_indeterminate {
				lower_abort_total(&call, Abort::EffectsUnknown {
					reason: Str::new_static(
						"effect authorization delivery became indeterminate during interruption",
					),
				})
			} else {
				lower_abort_total(&call, Abort::MissingOutcome)
			}
		},
		PumpTerminal::CancelUnobserved => lower_abort_total(&call, Abort::EffectsUnknown {
			reason: Str::new_static(
				"environment owner did not report terminal truth after cancellation",
			),
		}),
		PumpTerminal::ClientError(error) => lower_abort_total(&call, Abort::EffectsUnknown {
			reason: format!("environment invocation failed: {error}").to_str(),
		}),
	};
	(index, result)
}

async fn drain_pump(
	call: &CommittedCall,
	updates: Option<&flume::Sender<BatchUpdate>>,
) -> PumpTerminal {
	loop {
		match call.pump.output().await {
			PumpOutput::Update(json) => {
				if let Some(updates) = updates {
					let _ = updates.send(BatchUpdate {
						call_id: call.call_id.clone(),
						identity: call.identity.clone(),
						json,
					});
				}
			},
			PumpOutput::Terminal(terminal) => return terminal,
		}
	}
}

async fn interrupt_pump_with_grace(
	call: &CommittedCall,
	updates: Option<&flume::Sender<BatchUpdate>>,
	request: InterruptRequest,
	grace: Duration,
) -> PumpTerminal {
	let Ok(receipt) = call.pump.begin_interrupt(request.reason) else {
		drop(request.acknowledged);
		return force_cancel_with_grace(call, updates, grace).await;
	};
	finish_interrupt_with_grace(call, updates, receipt, request.acknowledged, grace).await
}

async fn finish_interrupt_with_grace(
	call: &CommittedCall,
	updates: Option<&flume::Sender<BatchUpdate>>,
	receipt: CommandReceipt,
	acknowledged: flume::Sender<()>,
	grace: Duration,
) -> PumpTerminal {
	let cooperative = async {
		let result = receipt.wait().await;
		let _ = acknowledged.send(());
		result?;
		Ok::<_, BatchError>(drain_pump(call, updates).await)
	};
	match tokio::time::timeout(grace, cooperative).await {
		Ok(Ok(terminal)) => terminal,
		Ok(Err(_)) | Err(_) => force_cancel_with_grace(call, updates, grace).await,
	}
}

async fn force_cancel_with_grace(
	call: &CommittedCall,
	updates: Option<&flume::Sender<BatchUpdate>>,
	grace: Duration,
) -> PumpTerminal {
	let forced = async {
		let _ = call.pump.cancel().await;
		drain_pump(call, updates).await
	};
	match tokio::time::timeout(grace, forced).await {
		Ok(PumpTerminal::Verdict(verdict)) => PumpTerminal::Verdict(verdict),
		Ok(PumpTerminal::ClientError(error)) => PumpTerminal::ClientError(error),
		Ok(PumpTerminal::Closed | PumpTerminal::CancelUnobserved) | Err(_) => {
			PumpTerminal::CancelUnobserved
		},
	}
}

async fn coordinate_interrupts(
	source: &mut watch::Receiver<Option<Str>>,
	targets: &[flume::Sender<InterruptRequest>],
	grace: Duration,
) {
	let reason = wait_for_interrupt(source).await;
	for target in targets.iter().rev() {
		let (acknowledged, acknowledgement) = flume::bounded(1);
		if target
			.send_async(InterruptRequest { reason: reason.clone(), acknowledged })
			.await
			.is_err()
		{
			continue;
		}
		if grace.is_zero() {
			tokio::task::yield_now().await;
		} else {
			let _ = tokio::time::timeout(grace, acknowledgement.recv_async()).await;
		}
	}
}

async fn wait_for_ordered_interrupt(
	receiver: &flume::Receiver<InterruptRequest>,
) -> InterruptRequest {
	match receiver.recv_async().await {
		Ok(request) => request,
		Err(_) => std::future::pending().await,
	}
}

async fn wait_for_interrupt(receiver: &mut watch::Receiver<Option<Str>>) -> Str {
	loop {
		let reason = receiver.borrow_and_update().clone();
		if let Some(reason) = reason {
			return reason;
		}
		if receiver.changed().await.is_err() {
			std::future::pending::<()>().await;
		}
	}
}

fn lower_verdict(
	call: &CommittedCall,
	registry: &Registry,
	caps: CapsBase,
	wire: omp_proto::env::v1::Verdict,
) -> Result<BatchResult, BatchError> {
	if let Ok(ToolTerminal::Detached(job)) =
		serde_json::from_slice::<ToolTerminal<Value, Value>>(&wire.json)
	{
		return lower_detached(call, wire.json, job);
	}

	let outcome = serde_json::from_slice::<CallOutcome<Value, Value>>(&wire.json)
		.map_err(BatchError::InvalidOutcome)?;
	let durable = durable_outcome(&wire.json, &outcome);
	let is_error = !matches!(outcome, CallOutcome::Ok(_));
	let mut result = if let Some(parts) = harness_parts(&outcome) {
		lower_tool_parts(call, &wire.json, is_error, wire.useless, &parts)?
	} else {
		let caps = PromptCaps::for_tool(caps, &call.identity.rev);
		match registry.prompt(&call.identity, &wire.json, &caps) {
			Ok(Some(parts)) => lower_tool_parts(call, &wire.json, is_error, wire.useless, &parts)?,
			Ok(None) => {
				unreachable!("harness outcome branches were handled before registry projection")
			},
			Err(_) => lower_canonical_parts(call, &wire.json, is_error, wire.useless, wire.parts)?,
		}
	};
	result.outcome = Some(durable);
	Ok(result)
}

fn lower_detached(
	call: &CommittedCall,
	raw: Bytes,
	job: JobRef,
) -> Result<BatchResult, BatchError> {
	let text =
		format!("job started; artifact will land at job://{} ({})", job.id, job.artifact.description)
			.to_str();
	let parts = [Part::Text { text }];
	let item = tool_result_item(0, &call.call_id, &call.identity, &raw, false, false, &parts)
		.map_err(|error| BatchError::Projection(error.to_string().to_str()))?;
	let event = finish_event(call, item);
	Ok(BatchResult { event, job: Some(job), outcome: None })
}

fn lower_abort(call: &CommittedCall, abort: Abort) -> Result<BatchResult, BatchError> {
	let outcome = CallOutcome::<Value, Value>::aborted(abort);
	let raw = Bytes::from(serde_json::to_vec(&outcome).map_err(BatchError::InvalidOutcome)?);
	let parts = harness_parts(&outcome).expect("aborted outcome always uses the harness renderer");
	let mut result = lower_tool_parts(call, &raw, true, false, &parts)?;
	result.outcome = Some(durable_outcome(&raw, &outcome));
	Ok(result)
}

fn lower_abort_total(call: &CommittedCall, abort: Abort) -> BatchResult {
	lower_abort(call, abort)
		.expect("harness-owned Aborted verdict serialization and canonical lowering are infallible")
}

fn lower_tool_parts(
	call: &CommittedCall,
	verdict: &[u8],
	is_error: bool,
	useless: bool,
	parts: &[Part],
) -> Result<BatchResult, BatchError> {
	let item = tool_result_item(0, &call.call_id, &call.identity, verdict, is_error, useless, parts)
		.map_err(|error| BatchError::Projection(error.to_string().to_str()))?;
	Ok(BatchResult { event: finish_event(call, item), job: None, outcome: None })
}

fn lower_canonical_parts(
	call: &CommittedCall,
	verdict: &[u8],
	is_error: bool,
	useless: bool,
	parts: Vec<CanonicalPart>,
) -> Result<BatchResult, BatchError> {
	let item = tool_result_item_canonical_parts(
		0,
		&call.call_id,
		&call.identity,
		verdict,
		is_error,
		useless,
		parts,
	)
	.map_err(|error| BatchError::Projection(error.to_string().to_str()))?;
	Ok(BatchResult { event: finish_event(call, item), job: None, outcome: None })
}

fn durable_outcome(
	raw: &Bytes,
	outcome: &CallOutcome<Value, Value>,
) -> CallOutcome<CallOutcomeDetails, CallOutcomeDetails> {
	let details = || CallOutcomeDetails::Inline { json: raw.clone() };
	match outcome {
		CallOutcome::Ok(_) => CallOutcome::Ok(details()),
		CallOutcome::Faulted(_) => CallOutcome::Faulted(details()),
		CallOutcome::ArgsRejected(issue) => CallOutcome::ArgsRejected(issue.clone()),
		CallOutcome::Aborted { abort, kind, policy } => {
			CallOutcome::Aborted { abort: abort.clone(), kind: *kind, policy: policy.clone() }
		},
	}
}

fn finish_event(call: &CommittedCall, item: Item) -> Arc<AgentEvent> {
	call
		.events
		.publish(AgentEvent::ToolFinished { call_id: call.call_id.clone(), item })
}

fn harness_parts(outcome: &CallOutcome<Value, Value>) -> Option<Vec<Part>> {
	let text = match outcome {
		CallOutcome::ArgsRejected(issue) => render_arg_issue(issue),
		CallOutcome::Aborted { abort, .. } => render_abort(abort),
		CallOutcome::Ok(_) | CallOutcome::Faulted(_) => return None,
	};
	Some(vec![Part::Text { text }])
}

fn render_arg_issue(issue: &ArgIssue) -> Str {
	let mut path = String::from("$");
	for segment in &issue.path {
		match segment {
			ArgPath::Key(key) => {
				path.push('[');
				path.push_str(&serde_json::to_string(key.as_str()).unwrap_or_else(|_| "\"?\"".into()));
				path.push(']');
			},
			ArgPath::Index(index) => {
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
	text.to_str()
}

fn render_abort(abort: &Abort) -> Str {
	match abort {
		Abort::Skipped { reason } => format!("skipped: {reason}").to_str(),
		Abort::Interrupted { reason } => format!("interrupted: {reason}").to_str(),
		Abort::EffectsUnknown { reason } => {
			format!("aborted with effects unknown: {reason}").to_str()
		},
		Abort::InputDropped => Str::new_static("aborted: invocation input dropped before commit"),
		Abort::MissingOutcome => {
			Str::new_static("aborted: executor ended without a terminal outcome")
		},
	}
}

#[cfg(test)]
mod tests {
	use std::collections::HashMap;

	use omp_env::frame::{self, client_frame, server_frame};
	use omp_proto::thread::v1::{Part as ThreadPart, part};
	use omp_tool::{ArgIssueKind, ModelClass, Rev};

	use super::*;

	fn identity(name: &'static str) -> ToolIdentity {
		ToolIdentity {
			name: Str::new_static(name),
			rev:  Rev { family: Str::new_static("test"), n: 1 },
		}
	}

	fn caps() -> CapsBase {
		CapsBase {
			maximum_parts:      8,
			maximum_text_bytes: 4096,
			media:              false,
			model_class:        ModelClass::Standard,
		}
	}

	fn terminal_text(result: &BatchResult) -> &str {
		let Some(omp_proto::thread::v1::item::Kind::ToolResult(result)) = result.item().kind.as_ref()
		else {
			panic!("batch completion was not a ToolResult");
		};
		let Some(ThreadPart { kind: Some(part::Kind::Text(text)) }) = result.parts.first() else {
			panic!("tool result did not contain text");
		};
		text
	}

	#[test]
	fn hook_mask_zero_path_does_not_clone_or_enqueue_argument_text() {
		let (bus, requests) = InvocationHookBus::channel();
		let invocation_id = Str::from("call");
		let fragment = Str::from("{\"value\":");
		bus.arg_text(&invocation_id, &fragment);
		assert!(requests.try_recv().is_err());

		bus.replace_union_mask(hook_event_mask(HookEventId::HookEventToolCall));
		bus.arg_text(&invocation_id, &fragment);
		assert!(matches!(
			requests.try_recv(),
			Ok(InvocationHookRequest::ArgText {
				invocation_id: actual_id,
				fragment: actual_fragment,
			}) if actual_id == invocation_id && actual_fragment == fragment
		));
	}

	#[tokio::test]
	async fn admission_hooks_cannot_widen_declared_effects() {
		let (bus, requests) = InvocationHookBus::channel();
		bus.replace_union_mask(hook_event_mask(HookEventId::HookEventToolCall));
		let maximum = Effects { subagents: 1, ..Effects::empty() };
		let query = AdmitInvocation { invocation_id: "effects".into(), ..AdmitInvocation::default() };
		let answer = bus.admit(query, maximum.clone());
		let responder = async {
			let InvocationHookRequest::Admission { maximum_effects, reply, .. } =
				requests.recv_async().await.expect("admission request")
			else {
				panic!("expected admission request");
			};
			assert_eq!(maximum_effects, maximum);
			reply
				.send(InvocationAdmission {
					admission: Admission {
						invocation_id: "effects".into(),
						allow: true,
						..Admission::default()
					},
					effects:   Effects { subagents: 2, ..Effects::empty() },
				})
				.expect("admission reply");
		};
		let (decision, ()) = tokio::join!(answer, responder);
		assert!(!decision.admission.allow);
		assert!(decision.effects.is_empty());
	}

	#[test]
	fn durable_outcome_preserves_all_four_terminal_arms() {
		let issue = ArgIssue {
			path:     Vec::new(),
			expected: Str::new_static("object"),
			kind:     ArgIssueKind::Malformed,
			example:  None,
			found:    None,
		};
		let outcomes = [
			CallOutcome::Ok(Value::Null),
			CallOutcome::Faulted(Value::Null),
			CallOutcome::ArgsRejected(issue),
			CallOutcome::aborted(Abort::InputDropped),
		];
		for outcome in outcomes {
			let raw = Bytes::from(serde_json::to_vec(&outcome).expect("serialize outcome"));
			let durable = durable_outcome(&raw, &outcome);
			assert!(matches!(
				(&outcome, durable),
				(CallOutcome::Ok(_), CallOutcome::Ok(_))
					| (CallOutcome::Faulted(_), CallOutcome::Faulted(_))
					| (CallOutcome::ArgsRejected(_), CallOutcome::ArgsRejected(_))
					| (CallOutcome::Aborted { .. }, CallOutcome::Aborted { .. })
			));
		}
	}

	#[tokio::test]
	async fn two_calls_preserve_order_and_malformed_terminal_becomes_effects_unknown() {
		let (client, transport) = EnvClient::in_process(0);
		let (requests, responses) = transport.into_parts();
		let server = tokio::spawn(async move {
			let mut opened = HashMap::new();
			while opened.len() < 2 {
				let frame = requests.recv_async().await.expect("invoke frame");
				let Some(client_frame::Body::InvokeTool(invoke)) = frame.body else {
					continue;
				};
				opened.insert(invoke.invocation_id, frame.request_id);
			}
			let mut committed = HashMap::new();
			while committed.len() < 2 {
				let frame = requests.recv_async().await.expect("commit frame");
				let Some(client_frame::Body::ArgsCommitted(commit)) = frame.body else {
					continue;
				};
				assert!(commit.effects.is_some(), "authorization must carry an explicit envelope");
				committed.insert(commit.invocation_id, frame.request_id);
			}
			let second = committed["second"];
			responses
				.send_async(frame::ServerFrame {
					request_id: second,
					body: Some(server_frame::Body::Verdict(frame::Verdict {
						invocation_id: "second".into(),
						json: Bytes::from_static(b"not-json"),
						..Default::default()
					})),
					..Default::default()
				})
				.await
				.expect("malformed verdict");
			let first = committed["first"];
			responses
				.send_async(frame::ServerFrame {
					request_id: first,
					body: Some(server_frame::Body::Verdict(frame::Verdict {
						invocation_id: "first".into(),
						json: Bytes::from_static(br#"{"kind":"ok","value":{"answer":1}}"#),
						parts: vec![ThreadPart { kind: Some(part::Kind::Text("one".into())) }],
						..Default::default()
					})),
					..Default::default()
				})
				.await
				.expect("valid verdict");
		});
		let events = EventBus::new();
		let observed = events.subscribe_lossless();
		let first = SpeculativeCall::open(
			&client,
			&events,
			Str::new_static("first"),
			identity("first_tool"),
			Duration::from_secs(1),
		)
		.await
		.expect("open first");
		let second = SpeculativeCall::open(
			&client,
			&events,
			Str::new_static("second"),
			identity("second_tool"),
			Duration::from_secs(1),
		)
		.await
		.expect("open second");
		let results = ToolBatch::new(vec![
			first.commit(Bytes::from_static(b"{}")),
			second.commit(Bytes::from_static(b"{}")),
		])
		.drive(&Registry::new(), &caps())
		.await;
		server.await.expect("scripted env task");

		assert_eq!(results.len(), 2);
		assert_eq!(terminal_text(&results[0]), "one");
		assert!(terminal_text(&results[1]).contains("failed to lower environment verdict"));
		let mut finished = 0;
		while let Ok(event) = observed.try_recv() {
			if matches!(event.as_ref(), AgentEvent::ToolFinished { .. }) {
				finished += 1;
			}
		}
		assert_eq!(finished, 2, "every committed call emits exactly one result");
	}

	#[tokio::test]
	async fn interrupt_before_commit_yields_skipped_without_args_committed() {
		let (client, transport) = EnvClient::in_process(0);
		let (requests, _responses) = transport.into_parts();
		let events = EventBus::new();
		let call = SpeculativeCall::open(
			&client,
			&events,
			Str::new_static("skipped"),
			identity("skipped_tool"),
			Duration::from_secs(1),
		)
		.await
		.expect("open call");
		let opened = requests.recv_async().await.expect("invoke frame");
		assert!(matches!(opened.body, Some(client_frame::Body::InvokeTool(_))));

		let (_interrupt_tx, interrupt_rx) = watch::channel(Some(Str::new_static("user interrupted")));
		let results = ToolBatch::new(vec![call.commit(Bytes::from_static(b"{}"))])
			.drive_interruptible(&Registry::new(), &caps(), interrupt_rx, Duration::from_millis(10))
			.await;
		assert_eq!(results.len(), 1);
		assert!(terminal_text(&results[0]).starts_with("skipped:"));
		while let Ok(frame) = requests.try_recv() {
			assert!(
				!matches!(frame.body, Some(client_frame::Body::ArgsCommitted(_))),
				"interrupted unstarted call sent ArgsCommitted"
			);
		}
	}

	#[tokio::test]
	async fn abandonment_after_admission_never_authorizes_effects() {
		let (client, transport) = EnvClient::in_process(0);
		let (requests, responses) = transport.into_parts();
		let events = EventBus::new();
		let call = SpeculativeCall::open(
			&client,
			&events,
			Str::new_static("abandoned"),
			identity("abandoned_tool"),
			Duration::from_secs(1),
		)
		.await
		.expect("open call");
		let (hooks, _hook_requests) = InvocationHookBus::channel();
		let (facts, _fact_receiver) = flume::unbounded();
		call
			.attach_runtime(hooks, facts, Effects::empty())
			.expect("attach runtime");
		let opened = requests.recv_async().await.expect("invoke frame");
		responses
			.send_async(frame::ServerFrame {
				request_id: opened.request_id,
				body: Some(server_frame::Body::AdmitInvocation(AdmitInvocation {
					invocation_id: "abandoned".into(),
					..AdmitInvocation::default()
				})),
				..frame::ServerFrame::default()
			})
			.await
			.expect("admit invocation");
		tokio::time::timeout(Duration::from_secs(1), async {
			while call.admission().is_none() {
				tokio::task::yield_now().await;
			}
		})
		.await
		.expect("admission observed");
		drop(call);

		let cancelled = tokio::time::timeout(Duration::from_secs(1), requests.recv_async())
			.await
			.expect("cancel timeout")
			.expect("cancel frame");
		assert!(matches!(cancelled.body, Some(client_frame::Body::Cancel(_))));
		while let Ok(frame) = requests.try_recv() {
			assert!(
				!matches!(frame.body, Some(client_frame::Body::ArgsCommitted(_))),
				"abandoned admitted invocation authorized effects"
			);
		}
	}
	#[tokio::test]
	async fn tool_args_events_accumulate_exact_fragments_and_partial_view() {
		let (client, transport) = EnvClient::in_process(0);
		let (requests, _responses) = transport.into_parts();
		let events = EventBus::new();
		let observed = events.subscribe_lossless();
		let mut call = SpeculativeCall::open(
			&client,
			&events,
			Str::new_static("partial"),
			identity("partial_tool"),
			Duration::from_secs(1),
		)
		.await
		.expect("open call");
		let opened = requests.recv_async().await.expect("invoke frame");
		assert!(matches!(opened.body, Some(client_frame::Body::InvokeTool(_))));

		call
			.relay_fragment(Str::new_static(r#"{"path":"src/main.rs","#))
			.await
			.expect("relay path fragment");
		let first_wire = requests.recv_async().await.expect("first ArgText");
		assert!(matches!(
			&first_wire.body,
			Some(client_frame::Body::ArgText(args))
				if args.fragment == r#"{"path":"src/main.rs","#
		));
		call
			.relay_fragment(Str::new_static(r#""command":"cargo ch"#))
			.await
			.expect("relay command fragment");
		let second_wire = requests.recv_async().await.expect("second ArgText");
		assert!(matches!(
			&second_wire.body,
			Some(client_frame::Body::ArgText(args))
				if args.fragment == r#""command":"cargo ch"#
		));

		let mut args_events = Vec::new();
		while let Ok(event) = observed.try_recv() {
			if let AgentEvent::ToolArgs { fragment, view, .. } = event.as_ref() {
				args_events.push((fragment.clone(), view.clone()));
			}
		}
		assert_eq!(args_events.len(), 2);
		assert_eq!(args_events[0].0, Bytes::from_static(br#"{"path":"src/main.rs","#));
		assert_eq!(args_events[0].1["path"].as_str(), Some("src/main.rs"));
		assert_eq!(args_events[1].0, Bytes::from_static(br#""command":"cargo ch"#));
		assert_eq!(args_events[1].1["path"].as_str(), Some("src/main.rs"));
		assert_eq!(args_events[1].1["command"].as_str(), Some("cargo ch"));
	}

	#[tokio::test]
	async fn speculative_update_publishes_before_commit_then_completes_once() {
		let (client, transport) = EnvClient::in_process(0);
		let (requests, responses) = transport.into_parts();
		let events = EventBus::new();
		let observed = events.subscribe_lossless();
		let mut call = SpeculativeCall::open(
			&client,
			&events,
			Str::new_static("preview"),
			identity("preview_tool"),
			Duration::from_secs(1),
		)
		.await
		.expect("open speculative call");
		let opened = requests.recv_async().await.expect("InvokeTool frame");
		let request_id = opened.request_id;
		assert!(matches!(opened.body, Some(client_frame::Body::InvokeTool(_))));

		call
			.relay_fragment(Str::new_static(r#"{"path":"src/lib.rs"}"#))
			.await
			.expect("relay speculative arguments");
		let fragment = requests.recv_async().await.expect("ArgText frame");
		assert!(matches!(fragment.body, Some(client_frame::Body::ArgText(_))));
		responses
			.send_async(frame::ServerFrame {
				request_id,
				body: Some(server_frame::Body::Update(frame::Update {
					invocation_id: "preview".into(),
					json: Bytes::from_static(br#"{"diff":"+preview"}"#),
					..Default::default()
				})),
				..Default::default()
			})
			.await
			.expect("speculative update");

		let mut saw_args = false;
		let mut update_count = 0;
		let mut saw_update = false;
		while !saw_update {
			let event = tokio::time::timeout(Duration::from_secs(1), observed.recv())
				.await
				.expect("speculative event timeout")
				.expect("event subscriber");
			match event.as_ref() {
				AgentEvent::ToolArgs { .. } => saw_args = true,
				AgentEvent::ToolUpdate { json, .. } => {
					assert!(saw_args, "ToolArgs must precede its speculative ToolUpdate");
					assert_eq!(json, &Bytes::from_static(br#"{"diff":"+preview"}"#));
					update_count += 1;
					saw_update = true;
				},
				_ => {},
			}
		}
		assert!(requests.try_recv().is_err(), "speculative update authorized effects before commit");

		let drive = tokio::spawn(async move {
			ToolBatch::new(vec![call.commit(Bytes::from_static(br#"{"path":"src/lib.rs"}"#))])
				.drive(&Registry::new(), &caps())
				.await
		});
		let commit = requests.recv_async().await.expect("ArgsCommitted frame");
		assert!(matches!(
			&commit.body,
			Some(client_frame::Body::ArgsCommitted(committed))
				if committed.raw == Bytes::from_static(br#"{"path":"src/lib.rs"}"#)
		));
		responses
			.send_async(frame::ServerFrame {
				request_id,
				body: Some(server_frame::Body::Verdict(frame::Verdict {
					invocation_id: "preview".into(),
					json: Bytes::from_static(br#"{"kind":"ok","value":{"applied":true}}"#),
					parts: vec![ThreadPart { kind: Some(part::Kind::Text("applied".into())) }],
					..Default::default()
				})),
				..Default::default()
			})
			.await
			.expect("terminal verdict");
		let results = drive.await.expect("batch task");
		assert_eq!(results.len(), 1);
		assert_eq!(terminal_text(&results[0]), "applied");
		let mut finished = 0;
		while let Ok(event) = observed.try_recv() {
			match event.as_ref() {
				AgentEvent::ToolFinished { .. } => finished += 1,
				AgentEvent::ToolUpdate { .. } => update_count += 1,
				_ => {},
			}
		}
		assert_eq!(finished, 1, "committed call must complete exactly once");
		assert_eq!(update_count, 1, "speculative update must publish exactly once");
	}

	async fn run_backpressured_commit_race(send_verdict: bool) -> Vec<BatchResult> {
		let (client, transport) = EnvClient::in_process(1);
		let (requests, responses) = transport.into_parts();
		let events = EventBus::new();
		let call = SpeculativeCall::open(
			&client,
			&events,
			Str::new_static("raced-commit"),
			identity("raced_tool"),
			Duration::from_secs(1),
		)
		.await
		.expect("open call");
		let opened = requests.recv_async().await.expect("first InvokeTool");
		let request_id = opened.request_id;
		assert!(matches!(opened.body, Some(client_frame::Body::InvokeTool(_))));

		// Occupy the one-slot channel, then let the pump enqueue ArgsCommitted
		// behind it. Receiving the blocker synchronously promotes that queued
		// frame before the current-thread pump can observe send completion.
		let blocker = SpeculativeCall::open(
			&client,
			&events,
			Str::new_static("channel-blocker"),
			identity("blocker_tool"),
			Duration::from_secs(1),
		)
		.await
		.expect("open channel blocker");
		let (interrupt_tx, interrupt_rx) = watch::channel(None);
		let drive = tokio::spawn(async move {
			ToolBatch::new(vec![call.commit(Bytes::from_static(b"{}"))])
				.drive_interruptible(&Registry::new(), &caps(), interrupt_rx, Duration::from_millis(25))
				.await
		});
		tokio::task::yield_now().await;
		tokio::task::yield_now().await;
		let blocker_frame = requests.recv().expect("queued blocker InvokeTool");
		assert!(matches!(blocker_frame.body, Some(client_frame::Body::InvokeTool(_))));
		let committed_frame = requests
			.try_recv()
			.expect("receiver promoted the backpressured ArgsCommitted frame");
		assert!(matches!(
			&committed_frame.body,
			Some(client_frame::Body::ArgsCommitted(committed))
				if committed.invocation_id == "raced-commit"
		));
		interrupt_tx
			.send(Some(Str::new_static("interrupt after receiver took commit")))
			.expect("interrupt batch");
		if send_verdict {
			responses
				.send(frame::ServerFrame {
					request_id,
					body: Some(server_frame::Body::Verdict(frame::Verdict {
						invocation_id: "raced-commit".into(),
						json: Bytes::from_static(br#"{"kind":"ok","value":{"committed":true}}"#),
						parts: vec![ThreadPart { kind: Some(part::Kind::Text("committed".into())) }],
						..Default::default()
					})),
					..Default::default()
				})
				.expect("authoritative verdict");
		}
		let results = tokio::time::timeout(Duration::from_secs(1), drive)
			.await
			.expect("commit race timeout")
			.expect("batch task");
		drop(blocker);
		results
	}

	#[tokio::test]
	async fn interrupt_coordinator_orders_queued_calls_before_active_call() {
		let (source_tx, mut source_rx) = watch::channel::<Option<Str>>(None);
		let mut targets = Vec::new();
		let mut receivers = Vec::new();
		for _ in 0..3 {
			let (target, receiver) = flume::bounded(1);
			targets.push(target);
			receivers.push(receiver);
		}
		let coordinator = tokio::spawn(async move {
			coordinate_interrupts(&mut source_rx, &targets, Duration::from_secs(1)).await;
		});
		source_tx
			.send(Some(Str::new_static("stop every call")))
			.expect("interrupt coordinator");

		let third = receivers[2].recv_async().await.expect("third interrupt");
		assert!(receivers[1].try_recv().is_err());
		assert!(receivers[0].try_recv().is_err());
		third.acknowledged.send(()).expect("acknowledge third");

		let second = receivers[1].recv_async().await.expect("second interrupt");
		assert!(receivers[0].try_recv().is_err());
		second.acknowledged.send(()).expect("acknowledge second");

		let first = receivers[0].recv_async().await.expect("first interrupt");
		first.acknowledged.send(()).expect("acknowledge first");
		coordinator.await.expect("coordinator task");
	}

	#[tokio::test(flavor = "current_thread")]
	async fn interrupt_after_receiver_takes_backpressured_commit_is_effects_unknown() {
		let results = run_backpressured_commit_race(false).await;
		assert_eq!(results.len(), 1);
		assert!(terminal_text(&results[0]).starts_with("aborted with effects unknown:"));
		assert!(!terminal_text(&results[0]).starts_with("skipped:"));
	}

	#[tokio::test(flavor = "current_thread")]
	async fn authoritative_verdict_wins_after_pending_commit_interrupt() {
		let results = run_backpressured_commit_race(true).await;
		assert_eq!(results.len(), 1);
		assert_eq!(terminal_text(&results[0]), "committed");
	}
	#[test]
	fn prewalk_transitions_once_on_first_mutating_effect() {
		let mode = ExecutionModeHandle::default();
		mode.set(ExecutionMode::Prewalk);
		let read_only = Effects {
			documents: Some(omp_tool::DocEffects { read: true, write_globs: Arc::default() }),
			..Effects::empty()
		};
		let mutating = Effects {
			documents: Some(omp_tool::DocEffects {
				read:        true,
				write_globs: Arc::from([Str::new_static("**")]),
			}),
			..Effects::empty()
		};
		let read_props = mode.invocation_props(&read_only);
		assert_eq!(mode.get(), ExecutionMode::Prewalk);
		assert!(!read_props.fields.contains_key(PREWALK_REASON_PROP));
		let write_props = mode.invocation_props(&mutating);
		assert_eq!(mode.get(), ExecutionMode::Standard);
		assert!(write_props.fields.contains_key(PREWALK_REASON_PROP));
		assert!(
			!mode
				.invocation_props(&mutating)
				.fields
				.contains_key(PREWALK_REASON_PROP)
		);
	}

	#[test]
	fn plan_yolo_is_a_single_env_authorized_transition() {
		let mode = ExecutionModeHandle::default();
		mode.set(ExecutionMode::PlanYolo);
		let mutating = Effects {
			exec: Some(omp_tool::ExecEffects {
				commands: Arc::from([Str::new_static("*")]),
				network:  false,
			}),
			..Effects::empty()
		};
		let props = mode.invocation_props(&mutating);
		assert!(props.fields.contains_key(PLAN_YOLO_PROP));
		assert_eq!(mode.get(), ExecutionMode::Standard);
		assert!(
			!mode
				.invocation_props(&mutating)
				.fields
				.contains_key(PLAN_YOLO_PROP)
		);
	}
}
