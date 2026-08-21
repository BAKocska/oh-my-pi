//! Durable N-turn agent policy loop.

use std::{
	collections::{BTreeMap, VecDeque},
	sync::Arc,
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures::StreamExt;
use omp_core::{Hash32, IntoStr, InvocationPhase, Str, sf};
use omp_env::EnvClient;
use omp_llm_inference::TurnId;
use omp_proto::{
	inference::v1::{self as pb, ContextRef, Outcome, ThreadDelta},
	thread::v1::{self as thread, Item},
};
use omp_storage::transcript::{CallId, InvocationTransition};
use omp_telemetry::firehose::{
	Branch, BranchOp, Envelope, Event as FirehoseEvent, Firehose, ModelAttempt, ModelRequest,
	ProviderError, ToolCall as FirehoseToolCall, TurnEnd as FirehoseTurnEnd,
	TurnStart as FirehoseTurnStart,
};
use omp_tool::{CapsBase, Registry as ToolRegistry, ToolIdentity};
use serde_json::{Value, value::RawValue};
use thiserror::Error;

use crate::{
	BatchError, Journal, JournalError, Mailbox, MailboxSender, ProjectionError, PromptError,
	TurnClient, TurnInput, TurnSession, YieldPayload, YieldPayloadError, YieldPayloadValidator,
	batch::{
		ExecutionModeHandle, InvocationAdmissionFact, InvocationHookBus, InvocationHookRequest,
		SpeculativeCall, ToolBatch,
	},
	context::{ContextProjection, project_context},
	continuation::{
		AgentSettledEvent, Continuation, ContinuationLedger, ContinuationPolicy, ContinuationSource,
		LoopSignal, continues_loop, from_hook,
	},
	control::{ControlMailbox, ControlMailboxEvent, ControlSender, ScheduledRewind},
	duplex::{DuplexError, DuplexManager},
	events::{AgentEvent, AgentPhase, EventBus},
	hooks::HookGate,
	jobs::JobBoard,
	journal::{AbortDisposition, TurnInputRecord, TurnOptionsRecord, TurnStart},
	mailbox::DrainPoint,
	project::project_journal,
	state::AgentState,
	turn::{Error as TurnError, empty_stop},
};

const INTERRUPT_GRACE: omp_core::Duration =
	omp_core::Duration::new(500, omp_core::DurationUnit::Milliseconds);
const TOOL_DEADLINE: omp_core::Duration =
	omp_core::Duration::new(300, omp_core::DurationUnit::Seconds);
const EMPTY_OUTPUT_RETRY_CAP: u8 = 3;
const EMPTY_OUTPUT_RETRY_DETAIL: &str =
	"Assistant returned no final output after retry cap; try switching models";
const CONTROL_DRAIN_LIMIT: usize = 32;

/// Terminal result of one complete caller submission, including tool
/// follow-ups.
#[derive(Clone, Debug)]
pub struct AgentRunSummary {
	/// Authoritative terminal gateway outcome of the last committed turn, if
	/// any.
	pub outcome:         Option<Outcome>,
	/// Committed turn count for this submission.
	pub committed_turns: u32,
	/// Whether the submission stopped on a caller abort.
	pub interrupted:     bool,
}

impl AgentRunSummary {
	/// Extracts and verbatim-validates the terminal `yield` call from the last
	/// subagent turn.
	///
	/// The raw argument bytes are decoded directly here, bypassing generic tool
	/// coercion so the structured deliverable cannot be stringified, wrapped,
	/// or stripped before its own retryable validation path sees it.
	pub fn yield_payload(
		&self,
		validator: &mut YieldPayloadValidator,
	) -> Result<Option<YieldPayload>, YieldPayloadError> {
		let Some(outcome) = self.outcome.as_ref() else {
			return Ok(None);
		};
		let mut payload = None;
		for item in &outcome.output {
			let Some(thread::item::Kind::ToolCall(call)) = item.kind.as_ref() else {
				continue;
			};
			if call.name != "yield" {
				continue;
			}
			let raw = serde_json::from_slice::<Value>(&call.args_json)
				.map_err(|_| YieldPayloadError::InvalidEnvelope)?;
			payload = Some(validator.validate(&raw)?);
		}
		Ok(payload)
	}
}
/// A live user message that can be rewound and edited.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewindTarget {
	/// Physical event index of the user message.
	pub event: u64,
	/// Previous live item event to retain, or the transcript root.
	pub keep:  Option<u64>,
	/// Concatenated text content of the user message.
	pub text:  Str,
}

/// Failure while projecting, submitting, recovering, journaling, or executing
/// tools.
#[derive(Debug, Error)]
pub enum AgentError {
	/// Durable journal operation failed.
	#[error(transparent)]
	Journal(#[from] JournalError),
	/// Canonical thread projection failed.
	#[error(transparent)]
	Projection(#[from] ProjectionError),
	/// Deterministic prompt rendering failed.
	#[error(transparent)]
	Prompt(#[from] PromptError),
	/// Gateway turn failed.
	#[error(transparent)]
	Turn(#[from] TurnError),
	/// Tool execution or lowering failed.
	#[error(transparent)]
	Batch(#[from] BatchError),
	/// Gateway stream or outcome violated the canonical turn contract.
	#[error("gateway turn protocol violation: {0}")]
	Protocol(&'static str),
	/// A crash replay cannot reconstruct the exact frozen tool registry.
	#[error("durable turn toolset differs from the authoritative registry")]
	ToolsetMismatch {
		/// Registry identity fixed by the durable turn start.
		durable: Hash32,
		/// Registry identity published when replay was attempted.
		current: Hash32,
	},
	/// An in-turn duplex invocation failed.
	#[error("in-turn invocation failed: {0}")]
	Duplex(Str),
	/// The configured absolute deadline elapsed.
	#[error("agent turn deadline elapsed")]
	Deadline,
	/// The caller aborted the active submission.
	#[error("submission interrupted by caller")]
	Interrupted,
}

const _: () = assert!(std::mem::size_of::<AgentError>() <= 128, "AgentError must stay compact");

/// Cloneable out-of-band stop signal for the active submission.
#[derive(Clone, Debug)]
pub struct AbortHandle {
	tx: Arc<tokio::sync::watch::Sender<u64>>,
}

impl AbortHandle {
	/// Aborts the active submission, if any.
	pub fn abort(&self) {
		self
			.tx
			.send_modify(|generation| *generation = generation.wrapping_add(1));
	}
}

/// Durable agent loop composed from transport-neutral Phase 1 foundations.
pub struct Agent<C: TurnClient> {
	client: C,
	env: EnvClient,
	state: AgentState,
	journal: Journal,
	caps: CapsBase,
	events: EventBus,
	hook_bus: InvocationHookBus,
	hook_requests: flume::Receiver<InvocationHookRequest>,
	invocation_fact_tx: flume::Sender<InvocationAdmissionFact>,
	invocation_fact_rx: flume::Receiver<InvocationAdmissionFact>,
	control_tx: ControlSender,
	control_mailbox: ControlMailbox,
	pending_rewinds: VecDeque<ScheduledRewind>,
	mailbox: Mailbox,
	jobs: Arc<JobBoard>,
	jobs_restored: bool,
	abort_tx: Arc<tokio::sync::watch::Sender<u64>>,
	abort_rx: tokio::sync::watch::Receiver<u64>,
	phase: AgentPhase,
	control_serviced_during_turn: bool,
	context: Option<ContextRef>,
	prompt_hash: Option<crate::PromptHash>,
	prompt_head_events: Vec<u64>,
	settled_gate: Option<Arc<HookGate>>,
	continuations: ContinuationLedger,
	execution_mode: ExecutionModeHandle,
	continuation_source: Option<Arc<dyn ContinuationSource>>,
	loop_signal: LoopSignal,
	last_toolset_hash: Option<Hash32>,
	firehose: Arc<Firehose>,
}

impl<C: TurnClient> Agent<C> {
	/// Constructs an agent with stable state, event, mailbox, and job handles.
	pub fn new(
		client: C,
		env: EnvClient,
		state: AgentState,
		journal: Journal,
		caps: CapsBase,
	) -> Self {
		let mailbox = Mailbox::new();
		let jobs = Arc::new(JobBoard::new(env.clone(), mailbox.sender()));
		let events = EventBus::new();
		let (abort_tx, abort_rx) = tokio::sync::watch::channel(0_u64);
		let (hook_bus, hook_requests) = InvocationHookBus::channel();
		let (invocation_fact_tx, invocation_fact_rx) = flume::unbounded();
		let (control_tx, control_mailbox) = crate::control::channel();
		let mut context = None;
		let mut prompt_hash = None;
		let mut prompt_head_events = Vec::new();
		let mut last_toolset_hash = None;
		if let Some(start) = journal.latest_turn_start() {
			prompt_hash = Some(start.prompt_hash.into());
			prompt_head_events.clone_from(&start.prompt_head_events);
			last_toolset_hash = Some(start.toolset_hash);
			if !journal.is_turn_aborted(start.turn_id.as_str()) {
				let context_id = match &start.input {
					TurnInputRecord::Delta { context, .. } => Some(context.context_id.clone()),
					TurnInputRecord::Full { .. } => {
						start.options.context_id.as_ref().map(ToString::to_string)
					},
				};
				let expected = journal
					.latest_receipt()
					.and_then(|receipt| receipt.outcome.revision.clone())
					.or_else(|| match &start.input {
						TurnInputRecord::Delta { context, .. } => context.expected.clone(),
						TurnInputRecord::Full { .. } => None,
					});
				if let (Some(context_id), Some(expected)) = (context_id, expected) {
					context = Some(ContextRef { context_id, expected: Some(expected) });
				}
			}
		} else if let Some(receipt) = journal.latest_receipt() {
			prompt_hash = Some(receipt.prompt_hash.into());
			prompt_head_events.clone_from(&receipt.prompt_head_events);
		}
		if let Some((hash, head_events)) = journal.active_prompt() {
			prompt_hash = Some(hash.into());
			prompt_head_events = head_events.to_vec();
		}
		Self {
			client,
			env,
			state,
			journal,
			caps,
			events,
			hook_bus,
			hook_requests,
			invocation_fact_tx,
			invocation_fact_rx,
			control_tx,
			control_mailbox,
			pending_rewinds: VecDeque::new(),
			mailbox,
			jobs,
			jobs_restored: false,
			abort_tx: Arc::new(abort_tx),
			abort_rx,
			phase: AgentPhase::Idle,
			control_serviced_during_turn: false,
			context,
			prompt_hash,
			prompt_head_events,
			settled_gate: None,
			continuations: ContinuationLedger::new(8),
			execution_mode: ExecutionModeHandle::default(),
			continuation_source: None,
			loop_signal: LoopSignal::default(),
			firehose: Arc::new(Firehose::new()),
			last_toolset_hash,
		}
	}

	/// Returns the authoritative configuration handle.
	pub const fn state(&self) -> &AgentState {
		&self.state
	}

	/// Returns the ordered event feed handle.
	pub const fn events(&self) -> &EventBus {
		&self.events
	}

	/// Returns a producer for asynchronous steering and settlement items.
	pub fn mailbox(&self) -> MailboxSender {
		self.mailbox.sender()
	}

	/// Returns the CONTROL-side receiver for invocation hook handoffs.
	///
	/// Clones compete for messages; one supervisor should own the receiver.
	pub fn hook_requests(&self) -> flume::Receiver<InvocationHookRequest> {
		self.hook_requests.clone()
	}

	/// Replaces the registered hook union mask in one atomic publication.
	pub fn replace_hook_mask(&self, mask: u128) {
		self.hook_bus.replace_union_mask(mask);
	}

	/// Returns a sender for authenticated extension CONTROL operations.
	pub fn control(&self) -> ControlSender {
		self.control_tx.clone()
	}

	/// Installs the fail-open `agent_settled` hook gate for this durable loop.
	pub fn set_agent_settled_gate(&mut self, gate: Arc<HookGate>, cap: u32) {
		self.settled_gate = Some(gate);
		self.continuations = ContinuationLedger::new(cap);
	}

	/// Returns the shared mode handle whose invocation metadata is enforced by
	/// the Environment dispatch boundary.
	pub fn execution_mode(&self) -> ExecutionModeHandle {
		self.execution_mode.clone()
	}

	/// Installs one application-owned autonomous-mode continuation source.
	pub fn set_continuation_source(&mut self, source: Arc<dyn ContinuationSource>) {
		self.continuation_source = Some(source);
	}

	/// Returns Core's latest loop-repetition and progress evidence.
	#[must_use]
	pub const fn loop_signal(&self) -> &LoopSignal {
		&self.loop_signal
	}

	/// Returns the latest recursive continuation ledger projection.
	#[must_use]
	pub const fn continuations(&self) -> &ContinuationLedger {
		&self.continuations
	}

	/// Replaces the non-blocking telemetry fan-out used by this loop.
	pub fn set_firehose(&mut self, firehose: Arc<Firehose>) {
		self.firehose = firehose;
	}

	/// Returns the shared non-blocking telemetry fan-out handle.
	#[must_use]
	pub fn firehose(&self) -> Arc<Firehose> {
		Arc::clone(&self.firehose)
	}

	/// Returns a cloneable out-of-band stop signal.
	pub fn abort_handle(&self) -> AbortHandle {
		AbortHandle { tx: Arc::clone(&self.abort_tx) }
	}

	/// Returns detached-job settlement state.
	pub const fn jobs(&self) -> &Arc<JobBoard> {
		&self.jobs
	}

	/// Returns the durable journal owner.
	pub const fn journal(&self) -> &Journal {
		&self.journal
	}

	/// Rewinds the durable session to a live prefix and returns the fresh
	/// projection.
	pub fn rewind(&mut self, to: Option<u64>) -> Result<Vec<Item>, AgentError> {
		let event = self.journal.rewind(now_ms(), to)?;
		self.firehose.publish(FirehoseEvent::Branch(Branch {
			envelope:   telemetry_envelope(),
			op:         Some(BranchOp::Switch),
			from_entry: to,
			to_entry:   Some(event),
		}));
		self.mailbox.discard_producer_interrupts();
		self.context = None;
		self.prompt_hash = None;
		self.prompt_head_events.clear();
		self.last_toolset_hash = None;
		let journal = self.journal.load()?;
		let projected = project_journal(
			&journal,
			journal.as_ref(),
			self.state.snapshot().registry.as_ref(),
			&self.caps,
		)?;
		Ok(projected.items)
	}

	/// Lists live user messages from oldest to newest for rewind selection.
	pub fn rewind_targets(&self) -> Result<Vec<RewindTarget>, AgentError> {
		let events = self.journal.live_item_events()?;
		let items = self.journal.items_at(&events)?;
		let mut targets = Vec::new();
		let mut previous = None;
		for (event, item) in events.into_iter().zip(items) {
			let Some(thread::item::Kind::Message(message)) = item.kind.as_ref() else {
				previous = Some(event);
				continue;
			};
			if message.role != thread::Role::User as i32 {
				previous = Some(event);
				continue;
			}
			let synthetic = item
				.props
				.as_ref()
				.is_some_and(|props| props.fields.contains_key(omp_tool::TOOL_REV_PROP));
			let mut text = String::new();
			for part in &message.parts {
				if let Some(thread::part::Kind::Text(part)) = part.kind.as_ref() {
					text.push_str(part);
				}
			}
			if !synthetic && !text.starts_with("<system-injection>") {
				targets.push(RewindTarget { event, keep: previous, text: Str::new(text) });
			}
			previous = Some(event);
		}
		Ok(targets)
	}

	/// Submits caller-authored canonical items and runs every tool follow-up.
	pub async fn submit(
		&mut self,
		items: impl IntoIterator<Item = Item>,
		root_turn_id: TurnId,
	) -> Result<AgentRunSummary, AgentError> {
		let result = self.submit_inner(items, root_turn_id).await;
		if result.is_err() {
			self.transition(AgentPhase::Idle);
		}
		result
	}

	async fn submit_inner(
		&mut self,
		items: impl IntoIterator<Item = Item>,
		root_turn_id: TurnId,
	) -> Result<AgentRunSummary, AgentError> {
		self.abort_rx.mark_unchanged();
		if !self.jobs_restored {
			for job in self.journal.pending_jobs() {
				self.jobs.register(job.clone());
			}
			self.jobs_restored = true;
		}
		let now = now_ms();
		let resumed = self.journal.pending_turn().cloned();
		let staged = self
			.journal
			.pending_input_submission()
			.map(|(turn_id, events)| {
				(
					turn_id.clone(),
					events.to_vec(),
					self.journal.is_released_submission(turn_id.as_str()),
				)
			});
		let continuing_recovery = resumed.is_some() || staged.is_some();
		let mut supplied = items.into_iter();
		let (mut pending_indexes, mut turn_id) = if let Some(start) = resumed {
			if supplied.next().is_some() {
				return Err(AgentError::Protocol(
					"cannot append caller items while resuming a durable turn",
				));
			}
			(start.item_events, TurnId::new(start.turn_id))
		} else if let Some((turn_id, events, released)) = staged {
			if supplied.next().is_some() {
				return Err(AgentError::Protocol(
					"cannot append caller items while resuming durable staged input",
				));
			}
			let mut pending_indexes = self.journal.released_input_events().to_vec();
			pending_indexes.extend(events);
			if released {
				let attempt = u8::try_from(self.journal.trailing_aborts())
					.unwrap_or(u8::MAX)
					.clamp(1, EMPTY_OUTPUT_RETRY_CAP);
				pending_indexes.push(self.journal.append_turn_input(
					now,
					turn_id.as_str(),
					empty_output_retry_item(attempt),
					self.prompt_hash,
				)?);
			}
			pending_indexes.sort_unstable();
			pending_indexes.dedup();
			(pending_indexes, TurnId::new(turn_id))
		} else {
			self.drain_control();
			self.execute_scheduled_rewinds()?;
			let snapshot = self.state.snapshot();
			let queued = self
				.mailbox
				.drain(DrainPoint::Idle, snapshot.defer_interrupts);
			let mut pending_indexes = self.journal.recoverable_input_events().to_vec();
			pending_indexes.extend_from_slice(self.journal.recoverable_settlement_events());
			pending_indexes.sort_unstable();
			pending_indexes.extend(self.stage_interrupts(&root_turn_id, queued)?);
			for item in supplied {
				pending_indexes.push(self.journal.append_turn_input(
					now,
					root_turn_id.as_str(),
					item,
					self.prompt_hash,
				)?);
			}
			(pending_indexes, root_turn_id)
		};
		let mut committed_turns = 0_u32;
		let mut last_outcome = None;
		let mut empty_output_retries = if continuing_recovery {
			u8::try_from(self.journal.trailing_aborts()).unwrap_or(u8::MAX)
		} else {
			0
		};

		loop {
			self
				.firehose
				.publish(FirehoseEvent::TurnStart(FirehoseTurnStart {
					envelope: telemetry_envelope(),
					turn:     u64::from(committed_turns).saturating_add(1),
				}));
			let turn = self.run_turn(turn_id.clone(), pending_indexes).await;
			let (outcome, mut speculative, submitted_context_id, snapshot, enabled_tools) = match turn
			{
				Ok(turn) => {
					empty_output_retries = 0;
					turn
				},
				Err(AgentError::Interrupted) => {
					self
						.journal
						.abort_turn(now_ms(), turn_id.as_str(), AbortDisposition::Exhausted)?;
					self.context = None;
					self.abort_rx.mark_unchanged();
					self.drain_control();
					self.execute_scheduled_rewinds()?;
					let snapshot = self.state.snapshot();
					let drained = self
						.mailbox
						.drain(DrainPoint::Idle, snapshot.defer_interrupts);
					let has_producer = drained
						.iter()
						.any(|interrupt| continues_loop(&interrupt.source));
					let next_turn_id = follow_up_id(&turn_id, committed_turns);
					pending_indexes = self.stage_interrupts(&next_turn_id, drained)?;
					if has_producer {
						turn_id = next_turn_id;
						continue;
					}
					self.transition(AgentPhase::Idle);
					if self.control_serviced_during_turn {
						return Err(AgentError::Interrupted);
					}
					return Ok(AgentRunSummary {
						outcome: last_outcome,
						committed_turns,
						interrupted: true,
					});
				},
				Err(AgentError::Turn(TurnError::Terminal(mut error)))
					if pb::turn_error::Kind::try_from(error.kind)
						== Ok(pb::turn_error::Kind::EmptyOutput) =>
				{
					let disposition = if empty_output_retries < EMPTY_OUTPUT_RETRY_CAP {
						AbortDisposition::Continue
					} else {
						AbortDisposition::Exhausted
					};
					self
						.journal
						.abort_turn(now_ms(), turn_id.as_str(), disposition)?;
					self.context = None;
					if disposition == AbortDisposition::Exhausted {
						error.detail = empty_output_cap_detail(&error);
						return Err(AgentError::Turn(TurnError::Terminal(error)));
					}
					empty_output_retries = empty_output_retries.saturating_add(1);
					let next_turn_id = follow_up_id(&turn_id, u32::from(empty_output_retries));
					pending_indexes = self
						.append_pending(&next_turn_id, [empty_output_retry_item(empty_output_retries)])?;
					turn_id = next_turn_id;
					continue;
				},
				Err(AgentError::Turn(error @ TurnError::Terminal(_))) => {
					self
						.journal
						.abort_turn(now_ms(), turn_id.as_str(), AbortDisposition::Exhausted)?;
					self.context = None;
					return Err(AgentError::Turn(error));
				},
				Err(error) => {
					self.publish_provider_error("turn_failed", Some(Str::new(error.to_string())));
					return Err(error);
				},
			};
			self.publish_model_request(&outcome);
			committed_turns = committed_turns.saturating_add(1);
			let stop = outcome.stop();
			self.context = outcome.revision.clone().and_then(|expected| {
				submitted_context_id
					.map(|context_id| ContextRef { context_id, expected: Some(expected) })
			});

			self.events.publish(AgentEvent::Snapshot(snapshot.clone()));
			self.drain_control();
			let mut immediate = self
				.mailbox
				.drain(DrainPoint::Immediate, snapshot.defer_interrupts);
			let mut boundary = self
				.mailbox
				.drain(DrainPoint::TurnBoundary, snapshot.defer_interrupts);
			if stop == pb::StopReason::StopToolUse {
				self.transition(AgentPhase::ToolBatch);
				if let Err(error) = self
					.complete_missing_speculation(
						&outcome.output,
						&mut speculative,
						snapshot.registry.as_ref(),
						enabled_tools.as_ref(),
					)
					.await
				{
					immediate.append(&mut boundary);
					self.mailbox.requeue_front(immediate);
					return Err(error);
				}
				tokio::task::yield_now().await;
				self.drain_invocation_facts()?;
				let calls = match committed_calls(&outcome.output, &mut speculative) {
					Ok(calls) => calls,
					Err(error) => {
						immediate.append(&mut boundary);
						self.mailbox.requeue_front(immediate);
						return Err(error);
					},
				};
				let made_environment_effect = calls
					.iter()
					.any(|call| crate::effects_mutate_environment(call.effects()));
				let call_digest = tool_call_digest(&outcome.output);
				let call_ids: Vec<Str> = outcome
					.output
					.iter()
					.filter_map(|item| match item.kind.as_ref() {
						Some(thread::item::Kind::ToolCall(call)) => Some(call.id.as_str().to_str()),
						_ => None,
					})
					.collect();
				if let Err(error) =
					self
						.journal
						.authorize_tool_batch(now_ms(), turn_id.as_str(), &call_ids)
				{
					immediate.append(&mut boundary);
					self.mailbox.requeue_front(immediate);
					return Err(error.into());
				}
				for call in &calls {
					self.journal.record_invocation_transition(
						call.authorized_at_ms(),
						InvocationTransition {
							effect_token: Some(call.effect_token().clone()),
							authorized_at: Some(call.authorized_at_ms()),
							effects: Some(call.effects().clone()),
							..empty_invocation_transition(
								call.call_id().clone(),
								CallId(call.call_id().clone()),
								InvocationPhase::EffectsAuthorized,
							)
						},
					)?;
				}
				let (interrupt_tx, interrupt_rx) = tokio::sync::watch::channel(None);
				let mut aborted = self.abort_rx.has_changed().unwrap_or(false);
				if aborted {
					interrupt_tx.send_replace(Some(sf!("user interrupt")));
				}
				let mut deadline_elapsed = false;
				let mut abort_rx = self.abort_rx.clone();
				let results = {
					let caps = self.caps;
					let drive = ToolBatch::new(calls).drive_interruptible(
						snapshot.registry.as_ref(),
						&caps,
						interrupt_rx,
						runtime_duration(INTERRUPT_GRACE),
					);
					tokio::pin!(drive);
					loop {
						tokio::select! {
							results = &mut drive => break results,
							() = wait_deadline(snapshot.deadline), if !deadline_elapsed => {
								deadline_elapsed = true;
								interrupt_tx.send_replace(Some(sf!("agent deadline elapsed")));
							},
							_ = abort_rx.changed(), if !aborted => {
								aborted = true;
								interrupt_tx.send_replace(Some(sf!("user interrupt")));
							},
							event = self.control_mailbox.handle_next(&mut self.journal) => {
								match event {
									ControlMailboxEvent::Closed => std::future::pending::<()>().await,
									ControlMailboxEvent::JournalHandled => {},
									ControlMailboxEvent::Rewind(rewind) => self.pending_rewinds.push_back(rewind),
								}
							},
							received = self.mailbox.wait() => {
								if received.is_err() { continue; }
								self.drain_control();
								for interrupt in self.mailbox.drain(DrainPoint::Immediate, snapshot.defer_interrupts) {
									interrupt_tx.send_replace(Some(interrupt_reason(&interrupt.source)));
									boundary.push(interrupt);
								}
							},
						}
					}
				};
				let mut next = Vec::with_capacity(results.len() + boundary.len());
				for result in results {
					self
						.firehose
						.publish(FirehoseEvent::ToolCall(Box::new(FirehoseToolCall {
							envelope: telemetry_envelope(),
							tool: result.call_id().clone(),
							..FirehoseToolCall::default()
						})));
					if let Some(outcome) = result.outcome().cloned() {
						let call_id = result.call_id().clone();
						self
							.journal
							.record_invocation_transition(now_ms(), InvocationTransition {
								outcome: Some(outcome),
								..empty_invocation_transition(
									call_id.clone(),
									CallId(call_id),
									InvocationPhase::Settled,
								)
							})?;
					}
					next.push(result.item().clone());
					if let Some(job) = result.into_job() {
						let id = job.id.clone();
						self.journal.register_job(now_ms(), job.clone())?;
						if self.jobs.register(job) {
							self
								.events
								.publish(AgentEvent::JobRegistered { job_id: id });
						}
					}
				}
				self
					.loop_signal
					.observe(call_digest, made_environment_effect, empty_output_retries);
				if self.execute_scheduled_rewinds()? {
					self.transition(AgentPhase::Idle);
					return Ok(AgentRunSummary {
						outcome: Some(outcome),
						committed_turns,
						interrupted: false,
					});
				}
				let next_turn_id = follow_up_id(&turn_id, committed_turns);
				pending_indexes = self.append_pending(&next_turn_id, next)?;
				let has_producer = boundary
					.iter()
					.any(|interrupt| continues_loop(&interrupt.source));
				pending_indexes
					.extend(self.stage_interrupts(&next_turn_id, std::mem::take(&mut boundary))?);
				if deadline_elapsed {
					return Err(AgentError::Deadline);
				}
				if aborted {
					self.abort_rx.mark_unchanged();
					if has_producer {
						last_outcome = Some(outcome);
						turn_id = next_turn_id;
						continue;
					}
					self.transition(AgentPhase::Idle);
					return Ok(AgentRunSummary {
						outcome: Some(outcome),
						committed_turns,
						interrupted: true,
					});
				}
				last_outcome = Some(outcome);
				turn_id = next_turn_id;
				continue;
			}
			immediate.append(&mut boundary);
			boundary = immediate;

			self.drain_control();
			if self.execute_scheduled_rewinds()? {
				self.transition(AgentPhase::Idle);
				return Ok(AgentRunSummary {
					outcome: Some(outcome),
					committed_turns,
					interrupted: false,
				});
			}
			let mut idle = self
				.mailbox
				.drain(DrainPoint::Idle, snapshot.defer_interrupts);
			boundary.append(&mut idle);
			if !boundary.is_empty() {
				let next_turn_id = follow_up_id(&turn_id, committed_turns);
				pending_indexes = self.stage_interrupts(&next_turn_id, boundary)?;
				last_outcome = Some(outcome);
				turn_id = next_turn_id;
				continue;
			}
			self.loop_signal.observe(None, false, empty_output_retries);
			if let Some(interrupt) = self.settled_continuation(&turn_id).await {
				let _ = self.mailbox.sender().try_enqueue(interrupt);
				boundary = self
					.mailbox
					.drain(DrainPoint::Idle, snapshot.defer_interrupts);
				if !boundary.is_empty() {
					let next_turn_id = follow_up_id(&turn_id, committed_turns);
					pending_indexes = self.stage_interrupts(&next_turn_id, boundary)?;
					last_outcome = Some(outcome);
					turn_id = next_turn_id;
					continue;
				}
			}
			if let Some((queued_turn, events)) = self.journal.pending_input_submission() {
				pending_indexes = events.to_vec();
				last_outcome = Some(outcome);
				turn_id = TurnId::new(queued_turn.clone());
				continue;
			}
			self.transition(AgentPhase::Idle);
			self
				.firehose
				.publish(FirehoseEvent::TurnEnd(Box::new(FirehoseTurnEnd {
					envelope: telemetry_envelope(),
					turn:     u64::from(committed_turns),
					outcome:  None,
				})));
			return Ok(AgentRunSummary {
				outcome: Some(outcome),
				committed_turns,
				interrupted: false,
			});
		}
	}

	/// Publishes post-hoc inference facts without participating in durable
	/// billing.
	fn publish_model_request(&self, outcome: &Outcome) {
		self
			.firehose
			.publish(FirehoseEvent::ModelRequest(Box::new(ModelRequest {
				envelope: telemetry_envelope(),
				served_model: Str::new(outcome.model.as_str()),
				provider: Str::new(outcome.provider.as_str()),
				usage: outcome.usage.clone().unwrap_or_default(),
				cost: outcome.cost,
				..ModelRequest::default()
			})));
		for diagnostic in &outcome.diagnostics {
			if diagnostic.retryability != pb::Retryability::Never as i32 {
				self
					.firehose
					.publish(FirehoseEvent::ModelAttempt(ModelAttempt {
						envelope: telemetry_envelope(),
						attempt:  diagnostic.attempt,
						code:     Str::new(diagnostic.code.as_str()),
					}));
			}
		}
	}

	/// Publishes a classified provider failure after the durable abort path ran.
	fn publish_provider_error(&self, code: &'static str, detail: Option<Str>) {
		self
			.firehose
			.publish(FirehoseEvent::ProviderError(Box::new(ProviderError {
				envelope: telemetry_envelope(),
				code: sf!(code),
				detail,
			})));
	}

	fn append_pending(
		&mut self,
		turn_id: &TurnId,
		items: impl IntoIterator<Item = Item>,
	) -> Result<Vec<u64>, AgentError> {
		let ts = now_ms();
		items
			.into_iter()
			.map(|item| {
				self
					.journal
					.append_turn_input(ts, turn_id.as_str(), item, self.prompt_hash)
					.map_err(Into::into)
			})
			.collect()
	}

	/// Runs the settled-boundary domain hook and converts an accepted decision
	/// into a normal mailbox interrupt so `defer_interrupts` remains
	/// authoritative.
	async fn settled_continuation(&mut self, turn_id: &TurnId) -> Option<crate::Interrupt> {
		let now = now_ms();
		let (mut candidate, mut policy) = self
			.continuation_source
			.as_ref()
			.map_or((Continuation::Settle, ContinuationPolicy::default()), |source| {
				source.decide(&self.loop_signal, now)
			});
		if matches!(candidate, Continuation::Settle)
			&& let Some(gate) = self.settled_gate.clone()
		{
			let event =
				AgentSettledEvent { agent_id: sf!("agent"), turn_id: Str::new(turn_id.as_str()) };
			let outcome = gate.gate_domain(&event).await;
			candidate = from_hook(outcome.winner, sf!("agent_settled"), continuation_item());
			policy = ContinuationPolicy::default();
		}
		match self
			.continuations
			.decide_with_policy(candidate, now, policy)
		{
			Continuation::Continue { owner, item, .. } => Some(crate::Interrupt {
				class: crate::InterruptClass::Immediate,
				item,
				source: crate::InterruptSource::Continuation { owner },
			}),
			Continuation::Settle | Continuation::Refused { .. } => None,
		}
	}

	fn stage_interrupts(
		&mut self,
		turn_id: &TurnId,
		interrupts: impl IntoIterator<Item = crate::Interrupt>,
	) -> Result<Vec<u64>, AgentError> {
		let ts = now_ms();
		let mut indexes = Vec::new();
		for interrupt in interrupts {
			if let crate::InterruptSource::Job { id } = &interrupt.source {
				indexes.push(self.journal.settle_job(ts, id.as_str(), interrupt.item)?);
				self
					.events
					.publish(AgentEvent::JobSettled { job_id: id.clone() });
			} else {
				indexes.push(self.journal.append_turn_input(
					ts,
					turn_id.as_str(),
					interrupt.item,
					self.prompt_hash,
				)?);
			}
		}
		Ok(indexes)
	}

	async fn run_turn(
		&mut self,
		turn_id: TurnId,
		pending: Vec<u64>,
	) -> Result<
		(
			Outcome,
			BTreeMap<Str, SpeculativeCall>,
			Option<String>,
			Arc<crate::AgentSnapshot>,
			Arc<[Str]>,
		),
		AgentError,
	> {
		self.control_serviced_during_turn = false;
		let snapshot = self.state.snapshot();
		let durable = self
			.journal
			.pending_turn()
			.filter(|start| start.turn_id.as_str() == turn_id.as_str())
			.cloned();
		if let Some(start) = durable.as_ref() {
			let current = snapshot.registry.slot_hash();
			if current != start.toolset_hash
				|| start
					.enabled_tools
					.iter()
					.any(|name| snapshot.registry.live_identity(name.as_str()).is_none())
			{
				return Err(AgentError::ToolsetMismatch { durable: start.toolset_hash, current });
			}
		}
		let rendered = if durable.is_none() {
			Some(snapshot.render_prompt()?)
		} else {
			None
		};
		let changed_prompt = rendered
			.as_ref()
			.is_some_and(|rendered| self.prompt_hash.is_some_and(|hash| hash != rendered.hash));
		let mut input_events = durable
			.as_ref()
			.map_or(pending, |start| start.item_events.clone());
		let toolset_hash = durable
			.as_ref()
			.map_or_else(|| snapshot.registry.slot_hash(), |start| start.toolset_hash);
		let changed_toolset = durable.is_none()
			&& self
				.last_toolset_hash
				.is_some_and(|hash| hash != toolset_hash);
		if let Some(rendered) = rendered.as_ref()
			&& (self.prompt_hash.is_none() || changed_prompt)
		{
			let old_head = std::mem::take(&mut self.prompt_head_events);
			let live = self.journal.live_item_events()?;
			let preserved_tail: Vec<_> = live
				.into_iter()
				.filter(|index| !old_head.contains(index))
				.collect();
			self.prompt_head_events = self.journal.rewrite_prompt_head(
				now_ms(),
				rendered.hash,
				rendered.items.as_ref(),
				&preserved_tail,
			)?;
			if changed_prompt {
				input_events = preserved_tail;
			}
			self.prompt_hash = Some(rendered.hash);
		}
		let frozen_enabled_tools: Arc<[Str]> = durable.as_ref().map_or_else(
			|| Arc::clone(&snapshot.enabled_tools),
			|start| Arc::from(start.enabled_tools.clone()),
		);
		let mut resume_input = durable.as_ref().map(|start| match &start.input {
			TurnInputRecord::Full { thread } => TurnInput::Full(thread.clone()),
			TurnInputRecord::Delta { context, delta } => {
				TurnInput::Delta(context.clone(), delta.clone())
			},
		});
		let all_live = self.journal.live_item_events()?;
		let mut full = resume_input
			.as_ref()
			.map_or_else(|| self.context.is_none(), |input| matches!(input, TurnInput::Full(_)));
		let mut context = match resume_input.as_ref() {
			Some(TurnInput::Delta(context, _)) => Some(context.clone()),
			_ => self.context.clone(),
		};
		let truncate_to = (changed_prompt || changed_toolset).then_some(0);
		let append_events = if let Some(start) = &durable {
			start.sequence_targets.clone()
		} else if changed_prompt {
			self
				.prompt_head_events
				.iter()
				.chain(&input_events)
				.copied()
				.collect()
		} else if changed_toolset || full {
			all_live.clone()
		} else {
			input_events.clone()
		};
		let sequence_targets = durable.as_ref().map_or_else(
			|| {
				if changed_prompt || changed_toolset || self.context.is_none() {
					append_events.clone()
				} else {
					input_events.clone()
				}
			},
			|start| start.sequence_targets.clone(),
		);
		let mut attempts = 0_u32;
		let mut backoff = snapshot.retry.initial_backoff();
		let frozen_options = durable.as_ref().map_or_else(
			|| snapshot.turn.clone(),
			|start| crate::TurnOptions {
				context_id: start.options.context_id.clone(),
				params:     start.options.params.clone(),
				executor:   start.options.executor.clone(),
				props:      start.options.props.clone(),
			},
		);
		let lifted_reseed = if changed_toolset {
			self.transition(AgentPhase::Projecting);
			let journal = self.journal.load()?;
			Some(project_journal(&journal, journal.as_ref(), snapshot.registry.as_ref(), &self.caps)?)
		} else {
			None
		};

		loop {
			let latest = self.state.snapshot();
			if latest
				.deadline
				.is_some_and(|deadline| std::time::Instant::now() >= deadline)
			{
				return Err(AgentError::Deadline);
			}
			let input = if let Some(input) = resume_input.as_ref() {
				input.clone()
			} else if full {
				let journal = self.journal.load()?;
				let projected =
					project_journal(&journal, journal.as_ref(), snapshot.registry.as_ref(), &self.caps)?;
				let context_handlers = self.hook_bus.union_mask()
					& crate::hook_event_mask(
						omp_proto::toolhost::v1::HookEventId::HookEventThreadProjection,
					) != 0;
				match project_context(projected, &all_live, context_handlers) {
					ContextProjection::Unchanged(thread) | ContextProjection::View { thread, .. } => {
						TurnInput::Full(thread)
					},
				}
			} else {
				let held = context
					.clone()
					.ok_or(AgentError::Protocol("delta missing context"))?;
				let append = match &lifted_reseed {
					Some(thread) => thread.items.clone(),
					None => self.journal.items_at(&append_events)?,
				};
				TurnInput::Delta(held, ThreadDelta { truncate_to, append })
			};
			let start = TurnStart {
				turn_id: turn_id.as_str().to_str(),
				item_events: input_events.clone(),
				prompt_hash: self.prompt_hash.expect("prompt rendered").digest(),
				prompt_head_events: self.prompt_head_events.clone(),
				toolset_hash,
				enabled_tools: frozen_enabled_tools.to_vec(),
				sequence_targets: sequence_targets.clone(),
				input: match &input {
					TurnInput::Full(thread) => TurnInputRecord::Full { thread: thread.clone() },
					TurnInput::Delta(context, delta) => {
						TurnInputRecord::Delta { context: context.clone(), delta: delta.clone() }
					},
				},
				options: TurnOptionsRecord {
					context_id: frozen_options.context_id.clone(),
					params:     frozen_options.params.clone(),
					executor:   frozen_options.executor.clone(),
					props:      frozen_options.props.clone(),
				},
			};
			let expected_head = match &input {
				TurnInput::Delta(context, delta) => {
					let expected = context
						.expected
						.as_ref()
						.ok_or(AgentError::Protocol("delta context missing revision"))?;
					let retained = delta.truncate_to.unwrap_or(expected.head);
					if retained > expected.head {
						return Err(AgentError::Protocol("delta truncation exceeds expected head"));
					}
					Some(
						retained
							.checked_add(
								u64::try_from(delta.append.len())
									.map_err(|_| AgentError::Protocol("delta too large"))?,
							)
							.ok_or(AgentError::Protocol("delta head overflow"))?,
					)
				},
				TurnInput::Full(thread) if frozen_options.context_id.is_some() => Some(
					u64::try_from(thread.items.len())
						.map_err(|_| AgentError::Protocol("full thread too large"))?,
				),
				TurnInput::Full(_) => None,
			};
			self.journal.start_turn(now_ms(), start)?;
			self.transition(AgentPhase::Turning);
			attempts = attempts.saturating_add(1);
			let submitted_context_id = match &input {
				TurnInput::Full(_) => frozen_options.context_id.as_ref().map(ToString::to_string),
				TurnInput::Delta(context, _) => Some(context.context_id.clone()),
			};
			let stateful = matches!(&input, TurnInput::Delta(..))
				|| matches!(&input, TurnInput::Full(_) if frozen_options.context_id.is_some());

			let selected = {
				let mut abort_rx = self.abort_rx.clone();
				let session = self.drive_session(
					turn_id.clone(),
					input,
					&frozen_options,
					Arc::clone(&snapshot.registry),
					Arc::clone(&frozen_enabled_tools),
				);
				tokio::pin!(session);
				tokio::select! {
					result = &mut session => Ok(result),
					() = wait_deadline(latest.deadline) => Err(AgentError::Deadline),
					_ = abort_rx.changed() => Err(AgentError::Interrupted),
				}
			};
			self.drain_invocation_facts()?;
			let session_result = selected?;
			match session_result {
				Ok((outcome, speculative)) => {
					validate_outcome(&outcome)?;
					if stateful && outcome.revision.is_none() {
						return Err(AgentError::Protocol("stateful outcome missing revision"));
					}
					if let (Some(base), Some(revision)) = (expected_head, outcome.revision.as_ref()) {
						let expected = base
							.checked_add(
								u64::try_from(outcome.output.len())
									.map_err(|_| AgentError::Protocol("outcome too large"))?,
							)
							.ok_or(AgentError::Protocol("outcome head overflow"))?;
						if revision.head != expected {
							return Err(AgentError::Protocol(
								"outcome revision head does not match committed append",
							));
						}
					}
					let (receipt, _) = self.journal.append_gateway_outcome(
						now_ms(),
						turn_id.as_str(),
						outcome.clone(),
					)?;
					self.record_committed_invocations(&outcome, &speculative, &receipt)?;
					self.patch_input_sequences(&sequence_targets, &outcome)?;
					self.last_toolset_hash = Some(toolset_hash);
					return Ok((
						outcome,
						speculative,
						submitted_context_id,
						snapshot.clone(),
						Arc::clone(&frozen_enabled_tools),
					));
				},
				Err(TurnError::Conflict(error)) => {
					if attempts >= latest.retry.max_attempts().get() {
						return Err(TurnError::Conflict(error).into());
					}
					let actual = error
						.actual
						.ok_or(AgentError::Protocol("conflict missing actual revision"))?;
					let held = context
						.as_mut()
						.ok_or(AgentError::Protocol("conflict on full turn"))?;
					held.expected = Some(actual);
					resume_input = None;
				},
				Err(TurnError::NeedFull(error)) => {
					if attempts >= latest.retry.max_attempts().get() {
						return Err(TurnError::NeedFull(error).into());
					}
					full = true;
					resume_input = None;
				},
				Err(TurnError::Terminal(error))
					if matches!(
						pb::turn_error::Kind::try_from(error.kind),
						Ok(pb::turn_error::Kind::RateLimited)
					) && attempts < latest.retry.max_attempts().get() =>
				{
					sleep_with_deadline(Duration::from_millis(error.retry_after_ms), latest.deadline)
						.await?;
				},
				Err(TurnError::Terminal(error))
					if matches!(
						pb::turn_error::Kind::try_from(error.kind),
						Ok(pb::turn_error::Kind::Overloaded | pb::turn_error::Kind::Upstream)
					) && attempts < latest.retry.max_attempts().get() =>
				{
					sleep_with_deadline(backoff, latest.deadline).await?;
					backoff = backoff.saturating_mul(2).min(latest.retry.max_backoff());
				},
				Err(TurnError::Terminal(error)) => {
					return Err(TurnError::Terminal(error).into());
				},
				Err(TurnError::Rpc(_)) if attempts < latest.retry.max_attempts().get() => {
					sleep_with_deadline(backoff, latest.deadline).await?;
					backoff = backoff.saturating_mul(2).min(latest.retry.max_backoff());
				},
				Err(error) => return Err(error.into()),
			}
		}
	}

	async fn drive_session(
		&mut self,
		turn_id: TurnId,
		input: TurnInput,
		options: &crate::TurnOptions,
		registry: Arc<ToolRegistry>,
		enabled_tools: Arc<[Str]>,
	) -> Result<(Outcome, BTreeMap<Str, SpeculativeCall>), TurnError> {
		let opening = self.client.turn(turn_id.clone(), input, options);
		tokio::pin!(opening);
		let mut session = loop {
			tokio::select! {
				session = &mut opening => break session?,
				event = self.control_mailbox.handle_next(&mut self.journal) => {
					match event {
						ControlMailboxEvent::Closed => std::future::pending::<()>().await,
						ControlMailboxEvent::JournalHandled => {},
						ControlMailboxEvent::Rewind(rewind) => self.pending_rewinds.push_back(rewind),
					}
				},
			}
		};
		let mut duplex = DuplexManager::new(
			self.env.clone(),
			Arc::clone(&registry),
			self.events.clone(),
			self.caps,
			runtime_duration(INTERRUPT_GRACE),
		);
		let mut speculative = BTreeMap::new();
		let mut part_calls: BTreeMap<u32, Str> = BTreeMap::new();
		loop {
			let event = if duplex.is_empty() {
				let mut events = session.events();
				tokio::select! {
					event = events.next() => event,
					event = self.control_mailbox.handle_next(&mut self.journal) => {
						match event {
							ControlMailboxEvent::Closed => std::future::pending().await,
							ControlMailboxEvent::JournalHandled => {
								self.control_serviced_during_turn = true;
								continue;
							},
							ControlMailboxEvent::Rewind(rewind) => {
								self.pending_rewinds.push_back(rewind);
								self.control_serviced_during_turn = true;
								continue;
							},
						}
					},
				}
			} else {
				let completion = {
					let mut events = session.events();
					tokio::select! {
						event = events.next() => Ok(event),
						completion = duplex.next() => Err(completion),
						event = self.control_mailbox.handle_next(&mut self.journal) => {
							match event {
								ControlMailboxEvent::Closed => std::future::pending().await,
								ControlMailboxEvent::JournalHandled => {
									self.control_serviced_during_turn = true;
									continue;
								},
								ControlMailboxEvent::Rewind(rewind) => {
									self.pending_rewinds.push_back(rewind);
									self.control_serviced_during_turn = true;
									continue;
								},
							}
						},
					}
				};
				match completion {
					Ok(event) => event,
					Err(Some((_id, result))) => {
						let frame = result.map_err(duplex_turn_error)?;
						session.submit(frame).await?;
						continue;
					},
					Err(None) => continue,
				}
			};
			let event = event.ok_or_else(|| tonic::Status::unavailable("turn stream lost"))??;
			self.events.publish(AgentEvent::Turn {
				turn_id: turn_id.clone(),
				event:   Box::new(event.clone()),
			});
			match event.event {
				Some(pb::turn_event::Event::Outcome(outcome)) => {
					return Ok((outcome, speculative));
				},
				Some(pb::turn_event::Event::PartStart(part))
					if part.kind() == pb::part_start::Kind::ToolCall =>
				{
					if !enabled_tools
						.iter()
						.any(|name| name.as_str() == part.tool_name)
					{
						return Err(TurnError::Protocol("stream named disabled tool"));
					}
					let Some((name, rev)) = registry.live_identity(&part.tool_name) else {
						return Err(TurnError::Protocol("stream named unknown tool"));
					};
					let maximum_effects = registry
						.effects(&part.tool_name)
						.map_err(|_| TurnError::Protocol("stream named unknown tool"))?
						.clone();
					let call_id = part.tool_call_id.as_str().to_str();
					let opened = SpeculativeCall::open_with_props(
						&self.env,
						&self.events,
						call_id.clone(),
						ToolIdentity { name: name.clone(), rev: rev.clone() },
						runtime_duration(TOOL_DEADLINE),
						self.execution_mode.invocation_props(&maximum_effects),
					)
					.await
					.map_err(|_| TurnError::Protocol("failed to open speculative tool"))?;
					opened
						.attach_runtime(
							self.hook_bus.clone(),
							self.invocation_fact_tx.clone(),
							maximum_effects,
						)
						.map_err(|_| TurnError::Protocol("failed to attach invocation runtime"))?;
					self
						.journal
						.record_invocation_transition(
							now_ms(),
							empty_invocation_transition(
								call_id.clone(),
								CallId(call_id.clone()),
								InvocationPhase::Open,
							),
						)
						.map_err(|_| TurnError::Protocol("failed to journal invocation open"))?;
					speculative.insert(call_id.clone(), opened);
					part_calls.insert(part.index, call_id);
				},
				Some(pb::turn_event::Event::PartDelta(part)) => {
					if let Some(call_id) = part_calls.get(&part.index) {
						let fragment = std::str::from_utf8(&part.chunk)
							.map_err(|_| TurnError::Protocol("tool argument fragment is not UTF-8"))?;
						speculative
							.get_mut(call_id)
							.expect("part call owns speculation")
							.relay_fragment(fragment.to_str())
							.await
							.map_err(|_| TurnError::Protocol("failed to relay speculative arguments"))?;
					}
				},
				Some(pb::turn_event::Event::PartEnd(part)) => {
					part_calls.remove(&part.index);
				},
				Some(pb::turn_event::Event::Invoke(invoke)) => duplex.start(invoke),
				Some(pb::turn_event::Event::InvokeCancel(cancel)) => {
					duplex.cancel(&cancel.invocation_id);
				},
				_ => {},
			}
		}
	}

	async fn complete_missing_speculation(
		&self,
		output: &[Item],
		speculative: &mut BTreeMap<Str, SpeculativeCall>,
		registry: &ToolRegistry,
		enabled_tools: &[Str],
	) -> Result<(), AgentError> {
		for item in output {
			let Some(thread::item::Kind::ToolCall(call)) = &item.kind else {
				continue;
			};
			if speculative.contains_key(call.id.as_str()) {
				continue;
			}
			if !enabled_tools.iter().any(|name| name.as_str() == call.name) {
				return Err(AgentError::Protocol("outcome names disabled tool"));
			}
			let Some((name, rev)) = registry.live_identity(&call.name) else {
				return Err(AgentError::Protocol("outcome names unknown tool"));
			};
			let maximum_effects = registry
				.effects(&call.name)
				.map_err(|_| AgentError::Protocol("committed tool effects missing"))?
				.clone();
			let mut opened = SpeculativeCall::open_with_props(
				&self.env,
				&self.events,
				call.id.as_str().to_str(),
				ToolIdentity { name: name.clone(), rev: rev.clone() },
				runtime_duration(TOOL_DEADLINE),
				self.execution_mode.invocation_props(&maximum_effects),
			)
			.await?;
			opened.attach_runtime(
				self.hook_bus.clone(),
				self.invocation_fact_tx.clone(),
				maximum_effects,
			)?;
			let fragment = std::str::from_utf8(&call.args_json)
				.map_err(|_| AgentError::Protocol("tool arguments are not UTF-8"))?;
			opened.relay_fragment(fragment.to_str()).await?;
			speculative.insert(call.id.as_str().to_str(), opened);
		}
		Ok(())
	}

	fn drain_control(&mut self) {
		self.control_mailbox.drain_ready(
			&mut self.journal,
			CONTROL_DRAIN_LIMIT,
			&mut self.pending_rewinds,
		);
	}

	fn execute_scheduled_rewinds(&mut self) -> Result<bool, AgentError> {
		let mut executed = false;
		while let Some(ScheduledRewind { target, scope }) = self.pending_rewinds.pop_front() {
			drop(scope);
			self.rewind(Some(target))?;
			if !self.jobs.is_empty() {
				self.journal.append_optimistic(
					now_ms(),
					rewind_background_warning(self.jobs.len()),
					self.prompt_hash,
				)?;
			}
			executed = true;
		}
		Ok(executed)
	}

	fn patch_input_sequences(
		&mut self,
		inputs: &[u64],
		outcome: &Outcome,
	) -> Result<(), AgentError> {
		let Some(revision) = outcome.revision.as_ref() else {
			return Ok(());
		};
		let output_len = u64::try_from(outcome.output.len())
			.map_err(|_| AgentError::Protocol("outcome too large"))?;
		let first_output = revision
			.head
			.checked_sub(output_len)
			.ok_or(AgentError::Protocol("outcome exceeds revision"))?
			+ 1;
		let first_input = first_output
			.checked_sub(
				u64::try_from(inputs.len()).map_err(|_| AgentError::Protocol("input too large"))?,
			)
			.ok_or(AgentError::Protocol("input exceeds revision"))?;
		for (offset, target) in inputs.iter().enumerate() {
			self.journal.amend_seq(
				now_ms(),
				*target,
				first_input + u64::try_from(offset).unwrap_or(u64::MAX),
			)?;
		}
		Ok(())
	}

	fn drain_invocation_facts(&mut self) -> Result<(), AgentError> {
		while let Ok(fact) = self.invocation_fact_rx.try_recv() {
			let requested = canonical_raw(fact.raw.as_bytes())?;
			let patch = (!fact.admission.args_patch.is_empty())
				.then(|| canonical_raw(&fact.admission.args_patch))
				.transpose()?;
			let effective = effective_args(&requested, patch.as_deref())?;
			let admission_receipt = serde_json::value::to_raw_value(&serde_json::json!({
				"allow": fact.admission.allow,
			}))
			.map_err(|_| AgentError::Protocol("admission receipt is not canonical JSON"))?;
			let call_id = CallId(fact.invocation_id.clone());
			for transition in [
				empty_invocation_transition(
					fact.invocation_id.clone(),
					call_id.clone(),
					InvocationPhase::Open,
				),
				InvocationTransition {
					requested_args: Some(requested),
					..empty_invocation_transition(
						fact.invocation_id.clone(),
						call_id.clone(),
						InvocationPhase::ArgsFinalized,
					)
				},
				empty_invocation_transition(
					fact.invocation_id.clone(),
					call_id.clone(),
					InvocationPhase::Admission,
				),
				InvocationTransition {
					transformations: Some(patch.into_iter().collect()),
					effective_args: Some(effective),
					admission_receipt: Some(admission_receipt),
					..empty_invocation_transition(fact.invocation_id, call_id, InvocationPhase::Admitted)
				},
			] {
				// A fact drained after later phases were journaled must not
				// replay earlier steps; the journal's richer record wins.
				if self
					.journal
					.invocation_phase(transition.invocation_id.as_str())
					.is_some_and(|current| current > transition.phase)
				{
					continue;
				}
				self
					.journal
					.record_invocation_transition(now_ms(), transition)?;
			}
		}
		Ok(())
	}

	fn record_committed_invocations(
		&mut self,
		outcome: &Outcome,
		speculative: &BTreeMap<Str, SpeculativeCall>,
		receipt: &crate::TurnReceipt,
	) -> Result<(), AgentError> {
		for (position, item) in outcome.output.iter().enumerate() {
			let Some(thread::item::Kind::ToolCall(call)) = item.kind.as_ref() else {
				continue;
			};
			let opened = speculative
				.get(call.id.as_str())
				.ok_or(AgentError::Protocol("committed tool lacked speculation"))?;
			let requested = canonical_raw(&call.args_json)?;
			let admission = opened.admission();
			let patch = admission
				.filter(|value| !value.args_patch.is_empty())
				.map(|value| canonical_raw(&value.args_patch))
				.transpose()?;
			let effective = effective_args(&requested, patch.as_deref())?;
			let admission_receipt = serde_json::value::to_raw_value(&serde_json::json!({
				"allow": admission.is_none_or(|value| value.allow),
			}))
			.map_err(|_| AgentError::Protocol("admission receipt is not canonical JSON"))?;
			let invocation_id = call.id.as_str().to_str();
			let call_id = CallId(invocation_id.clone());
			for transition in [
				empty_invocation_transition(
					invocation_id.clone(),
					call_id.clone(),
					InvocationPhase::Open,
				),
				InvocationTransition {
					requested_args: Some(requested.clone()),
					..empty_invocation_transition(
						invocation_id.clone(),
						call_id.clone(),
						InvocationPhase::ArgsFinalized,
					)
				},
				empty_invocation_transition(
					invocation_id.clone(),
					call_id.clone(),
					InvocationPhase::Admission,
				),
				InvocationTransition {
					transformations: Some(patch.into_iter().collect()),
					effective_args: Some(effective),
					admission_receipt: Some(admission_receipt),
					..empty_invocation_transition(
						invocation_id.clone(),
						call_id.clone(),
						InvocationPhase::Admitted,
					)
				},
				InvocationTransition {
					assistant_item_event: receipt.item_events.get(position).copied(),
					..empty_invocation_transition(
						invocation_id,
						call_id,
						InvocationPhase::AssistantItemCommitted,
					)
				},
			] {
				// Live admission facts may already have advanced this invocation
				// past the replayed step; the journal's richer record wins.
				if self
					.journal
					.invocation_phase(transition.invocation_id.as_str())
					.is_some_and(|current| current > transition.phase)
				{
					continue;
				}
				self
					.journal
					.record_invocation_transition(now_ms(), transition)?;
			}
		}
		Ok(())
	}

	fn transition(&mut self, to: AgentPhase) {
		if self.phase != to {
			self.events.transition(self.phase, to);
			self.phase = to;
		}
	}
}

const fn empty_invocation_transition(
	invocation_id: Str,
	call_id: CallId,
	phase: InvocationPhase,
) -> InvocationTransition {
	InvocationTransition {
		invocation_id,
		call_id,
		phase,
		requested_args: None,
		transformations: None,
		effective_args: None,
		admission_receipt: None,
		assistant_item_event: None,
		effect_token: None,
		effects: None,
		authorized_at: None,
		outcome: None,
	}
}

fn canonical_raw(bytes: &[u8]) -> Result<Box<RawValue>, AgentError> {
	let value = serde_json::from_slice::<Value>(bytes)
		.map_err(|_| AgentError::Protocol("invocation arguments are not one JSON document"))?;
	serde_json::value::to_raw_value(&value)
		.map_err(|_| AgentError::Protocol("invocation arguments cannot be canonicalized"))
}

fn effective_args(
	requested: &RawValue,
	patch: Option<&RawValue>,
) -> Result<Box<RawValue>, AgentError> {
	let mut value = serde_json::from_str::<Value>(requested.get())
		.map_err(|_| AgentError::Protocol("canonical requested arguments became invalid"))?;
	if let Some(patch) = patch {
		let patch = serde_json::from_str::<Value>(patch.get())
			.map_err(|_| AgentError::Protocol("admission patch is not valid JSON"))?;
		apply_merge_patch(&mut value, patch);
	}
	serde_json::value::to_raw_value(&value)
		.map_err(|_| AgentError::Protocol("effective arguments cannot be canonicalized"))
}

fn apply_merge_patch(target: &mut Value, patch: Value) {
	let Value::Object(patch) = patch else {
		*target = patch;
		return;
	};
	if !target.is_object() {
		*target = Value::Object(serde_json::Map::new());
	}
	let target = target
		.as_object_mut()
		.expect("target was normalized to an object");
	for (key, value) in patch {
		if value.is_null() {
			target.remove(&key);
		} else {
			apply_merge_patch(target.entry(key).or_insert(Value::Null), value);
		}
	}
}

fn committed_calls(
	output: &[Item],
	speculative: &mut BTreeMap<Str, SpeculativeCall>,
) -> Result<Vec<crate::CommittedCall>, AgentError> {
	let mut committed = Vec::new();
	for item in output {
		let Some(thread::item::Kind::ToolCall(call)) = &item.kind else {
			continue;
		};
		let opened = speculative
			.remove(call.id.as_str())
			.ok_or(AgentError::Protocol("committed tool lacked speculation"))?;
		if opened.identity().name.as_str() != call.name {
			return Err(AgentError::Protocol("committed tool identity changed"));
		}
		let committed_rev = item
			.props
			.as_ref()
			.and_then(|props| props.fields.get(omp_tool::TOOL_REV_PROP))
			.and_then(|value| value.kind.as_ref())
			.and_then(|kind| match kind {
				pb::value::Kind::String(value) => Some(value.as_str()),
				_ => None,
			})
			.ok_or(AgentError::Protocol("committed tool revision missing"))?;
		if committed_rev != opened.identity().rev.to_string() {
			return Err(AgentError::Protocol("committed tool revision changed"));
		}
		committed.push(opened.commit(call.args_json.clone()));
	}
	Ok(committed)
}

fn validate_outcome(outcome: &Outcome) -> Result<(), AgentError> {
	let tool_calls = outcome
		.output
		.iter()
		.filter(|item| matches!(item.kind, Some(thread::item::Kind::ToolCall(_))))
		.count();
	match outcome.stop() {
		pb::StopReason::StopToolUse if tool_calls == 0 => {
			return Err(AgentError::Protocol("tool-use outcome has no tool calls"));
		},
		pb::StopReason::StopEndTurn if tool_calls != 0 => {
			return Err(AgentError::Protocol("end-turn outcome contains unresolved tool calls"));
		},
		_ => {},
	}
	if let Some(revision) = outcome.revision.as_ref() {
		let count = u64::try_from(outcome.output.len())
			.map_err(|_| AgentError::Protocol("outcome too large"))?;
		let first = revision
			.head
			.checked_sub(count)
			.ok_or(AgentError::Protocol("outcome exceeds revision"))?
			+ 1;
		for (offset, item) in outcome.output.iter().enumerate() {
			if item.seq != first + u64::try_from(offset).unwrap_or(u64::MAX) {
				return Err(AgentError::Protocol("outcome sequences are not a consecutive suffix"));
			}
		}
	}
	Ok(())
}
fn telemetry_envelope() -> Envelope {
	Envelope { occurred_at_ms: now_ms(), ..Envelope::default() }
}

async fn wait_deadline(deadline: Option<std::time::Instant>) {
	match deadline {
		Some(deadline) => tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await,
		None => std::future::pending().await,
	}
}

async fn sleep_with_deadline(
	duration: Duration,
	deadline: Option<std::time::Instant>,
) -> Result<(), AgentError> {
	tokio::select! {
		() = tokio::time::sleep(duration) => Ok(()),
		() = wait_deadline(deadline) => Err(AgentError::Deadline),
	}
}

fn interrupt_reason(source: &crate::mailbox::InterruptSource) -> Str {
	match source {
		crate::mailbox::InterruptSource::Job { id } => {
			format!("job {} settled", id.as_str()).to_str()
		},
		crate::mailbox::InterruptSource::Continuation { owner } => {
			format!("continuation from {}", owner.as_str()).to_str()
		},
		crate::mailbox::InterruptSource::Schedule { id } => {
			format!("schedule {} fired", id.as_str()).to_str()
		},
		crate::mailbox::InterruptSource::Peer { from } => {
			format!("peer {} steered", from.as_str()).to_str()
		},
		crate::mailbox::InterruptSource::Producer(name) => name.clone(),
	}
}

fn tool_call_digest(items: &[Item]) -> Option<Str> {
	let mut hasher = Hash32::hasher();
	let mut calls = 0_u32;
	for item in items {
		let Some(thread::item::Kind::ToolCall(call)) = &item.kind else {
			continue;
		};
		calls = calls.saturating_add(1);
		hasher.update((call.name.len() as u64).to_le_bytes());
		hasher.update(call.name.as_bytes());
		hasher.update((call.args_json.len() as u64).to_le_bytes());
		hasher.update(&call.args_json);
	}
	(calls != 0).then(|| Str::new(hasher.finalize().to_string()))
}

fn continuation_item() -> Item {
	Item {
		seq:           0,
		created_at_ms: now_ms(),
		kind:          Some(thread::item::Kind::Message(thread::Message {
			role:  thread::Role::User as i32,
			parts: vec![thread::Part {
				kind: Some(thread::part::Kind::Text(
					"<system-injection>\nA settled-boundary continuation was accepted. Continue the \
					 task.\n</system-injection>"
						.to_owned(),
				)),
			}],
		})),
		props:         None,
	}
}

fn rewind_background_warning(count: usize) -> Item {
	let text = format!(
		"<system-injection>\nRewind left {count} background job(s) running; their settlements may \
		 still arrive. Cancel them explicitly if they are no longer wanted.\n</system-injection>"
	);
	Item {
		seq:           0,
		created_at_ms: now_ms(),
		kind:          Some(thread::item::Kind::Message(thread::Message {
			role:  thread::Role::System as i32,
			parts: vec![thread::Part { kind: Some(thread::part::Kind::Text(text)) }],
		})),
		props:         None,
	}
}

fn duplex_turn_error(error: DuplexError) -> TurnError {
	TurnError::Protocol(match error {
		DuplexError::Batch(_) => "duplex tool batch failed",
		DuplexError::Registry(_) => "duplex tool registry failed",
		DuplexError::MissingToolResult => "duplex completion missing tool result",
	})
}
fn empty_output_retry_item(attempt: u8) -> Item {
	let text = format!(
		"<system-injection>\nStopped without actionable output; task incomplete. Continue with a \
		 user-visible final answer or the next required tool call.\nAttempt \
		 #{attempt}/{EMPTY_OUTPUT_RETRY_CAP}\n</system-injection>"
	);
	Item {
		seq:           0,
		created_at_ms: now_ms(),
		kind:          Some(thread::item::Kind::Message(thread::Message {
			role:  thread::Role::User as i32,
			parts: vec![thread::Part { kind: Some(thread::part::Kind::Text(text)) }],
		})),
		props:         None,
	}
}

/// Selects the terminal message for a capped empty-output chain from the
/// gateway's empty-stop diagnostics on the final failed turn.
///
/// Billed non-reasoning output on a zero-block stop means content was
/// generated and then dropped downstream — a filter or refusal flattened to a
/// clean stop by a proxy, or a lossy API translation — so the generic
/// switch-models hint would misdiagnose a provider-side delivery problem.
/// Known reasoning-only usage is not evidence that deliverable content was
/// dropped and keeps the context hint.
fn empty_output_cap_detail(error: &pb::TurnError) -> String {
	let diagnostic = error
		.diagnostics
		.iter()
		.rev()
		.find(|diagnostic| diagnostic.code.starts_with("empty_stop."));
	match diagnostic.map(|diagnostic| (diagnostic.code.as_str(), diagnostic.detail.as_str())) {
		Some((empty_stop::BILLED_OUTPUT, billed)) => {
			let tokens: u64 = billed.parse().unwrap_or(0);
			let plural = if tokens == 1 { "" } else { "s" };
			format!(
				"Assistant returned an empty stop after retry cap, but the provider billed {tokens} \
				 output token{plural} for it; content was generated and then dropped before delivery, \
				 which usually points to a provider-side content filter or a lossy API translation \
				 rather than a context problem"
			)
		},
		Some((empty_stop::EMPTY, _)) => "Assistant returned an empty stop after retry cap; try \
		                                 switching models or removing large attachments from recent \
		                                 context"
			.to_owned(),
		_ => EMPTY_OUTPUT_RETRY_DETAIL.to_owned(),
	}
}

fn follow_up_id(_root: &TurnId, _ordinal: u32) -> TurnId {
	TurnId::new(omp_core::Ulid::generate().to_string())
}

fn runtime_duration(duration: omp_core::Duration) -> Duration {
	duration
		.to_std()
		.expect("agent runtime duration constants fit std::time::Duration")
}

pub fn now_ms() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or(Duration::ZERO)
		.as_millis()
		.try_into()
		.unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
	use std::{
		collections::{BTreeMap, VecDeque},
		sync::Arc,
	};

	use bytes::Bytes;
	use futures::stream;
	use omp_storage::transcript::{Entry, Header, Kind, SessionId};
	use omp_tool::{
		Claims, Constraint, Effects, ModelClass, Precedence, Presentation, Rev, ToolSpec,
	};
	use parking_lot::Mutex;

	use super::*;

	type Script = Vec<Result<pb::TurnEvent, TurnError>>;
	type OpenedTurn = (TurnId, TurnInput, crate::TurnOptions);
	type OpenedTurns = Vec<OpenedTurn>;

	#[derive(Clone)]
	struct ScriptedClient {
		scripts: Arc<Mutex<VecDeque<Script>>>,
		opened:  Arc<Mutex<OpenedTurns>>,
	}

	struct ScriptedSession {
		events: VecDeque<Result<pb::TurnEvent, TurnError>>,
	}

	impl TurnSession for ScriptedSession {
		fn events(
			&mut self,
		) -> impl futures::Stream<Item = Result<pb::TurnEvent, TurnError>> + Send + Unpin + '_ {
			stream::poll_fn(move |_| match self.events.pop_front() {
				Some(event) => std::task::Poll::Ready(Some(event)),
				None => std::task::Poll::Pending,
			})
		}

		fn submit(
			&mut self,
			_frame: crate::InvokeFrame,
		) -> impl Future<Output = Result<(), TurnError>> + Send + '_ {
			std::future::ready(Ok(()))
		}
	}

	impl TurnClient for ScriptedClient {
		type Session<'client> = ScriptedSession;

		fn turn<'client>(
			&'client self,
			turn_id: TurnId,
			input: TurnInput,
			options: &'client crate::TurnOptions,
		) -> impl Future<Output = Result<Self::Session<'client>, TurnError>> + Send + 'client {
			self.opened.lock().push((turn_id, input, options.clone()));
			let events = self
				.scripts
				.lock()
				.pop_front()
				.expect("one script per turn");
			std::future::ready(Ok(ScriptedSession { events: events.into() }))
		}
	}

	fn outcome_script(outcome: Outcome) -> Vec<Result<pb::TurnEvent, TurnError>> {
		vec![Ok(pb::TurnEvent { event: Some(pb::turn_event::Event::Outcome(outcome)) })]
	}

	fn pending_text_script() -> Vec<Result<pb::TurnEvent, TurnError>> {
		vec![
			Ok(pb::TurnEvent {
				event: Some(pb::turn_event::Event::PartStart(pb::PartStart {
					index:        0,
					kind:         pb::part_start::Kind::Text as i32,
					tool_call_id: String::new(),
					tool_name:    String::new(),
				})),
			}),
			Ok(pb::TurnEvent {
				event: Some(pb::turn_event::Event::PartDelta(pb::PartDelta {
					index: 0,
					chunk: Bytes::from_static(b"partial"),
				})),
			}),
		]
	}
	fn pending_tool_script(identity: &ToolIdentity) -> Vec<Result<pb::TurnEvent, TurnError>> {
		let call_id = "pending-call";
		let call = thread::ToolCall {
			id: call_id.to_owned(),
			name: identity.name.to_string(),
			args_json: Bytes::from_static(b"{}"),
			..thread::ToolCall::default()
		};
		let item = Item {
			kind: Some(thread::item::Kind::ToolCall(call)),
			props: Some(pb::ValueMap {
				fields: BTreeMap::from([(omp_tool::TOOL_REV_PROP.to_owned(), pb::Value {
					kind: Some(pb::value::Kind::String(identity.rev.to_string())),
				})]),
			}),
			..Item::default()
		};
		vec![
			Ok(pb::TurnEvent {
				event: Some(pb::turn_event::Event::PartStart(pb::PartStart {
					index:        0,
					kind:         pb::part_start::Kind::ToolCall as i32,
					tool_call_id: call_id.to_owned(),
					tool_name:    identity.name.to_string(),
				})),
			}),
			Ok(pb::TurnEvent {
				event: Some(pb::turn_event::Event::PartDelta(pb::PartDelta {
					index: 0,
					chunk: Bytes::from_static(b"{}"),
				})),
			}),
			Ok(pb::TurnEvent {
				event: Some(pb::turn_event::Event::PartEnd(pb::PartEnd {
					index:     0,
					signature: Bytes::new(),
				})),
			}),
			Ok(pb::TurnEvent {
				event: Some(pb::turn_event::Event::Outcome(Outcome {
					output: vec![item],
					stop: pb::StopReason::StopToolUse as i32,
					..Outcome::default()
				})),
			}),
		]
	}

	fn message(role: thread::Role, text: &str) -> Item {
		Item {
			kind: Some(thread::item::Kind::Message(thread::Message {
				role:  i32::from(role),
				parts: vec![thread::Part { kind: Some(thread::part::Kind::Text(text.to_owned())) }],
			})),
			..Item::default()
		}
	}

	fn end_outcome(text: &str) -> Outcome {
		Outcome {
			output: vec![message(thread::Role::Assistant, text)],
			stop: pb::StopReason::StopEndTurn as i32,
			..Outcome::default()
		}
	}

	fn test_journal(name: &str) -> (Journal, std::path::PathBuf) {
		let path = std::env::temp_dir().join(format!(
			"omp-agent-loop-{name}-{}-{}.jsonl",
			std::process::id(),
			omp_core::Ulid::generate()
		));
		let journal = Journal::create(&path, &Header {
			v:       4,
			id:      SessionId(Str::new(name)),
			created: 1,
			cwd:     std::env::temp_dir(),
		})
		.expect("create test journal");
		(journal, path)
	}

	fn test_caps() -> CapsBase {
		CapsBase {
			maximum_parts:      16,
			maximum_text_bytes: 16_384,
			media:              false,
			model_class:        ModelClass::Standard,
		}
	}

	async fn wait_for_opened(opened: &Arc<Mutex<OpenedTurns>>, count: usize) {
		for _ in 0..100 {
			if opened.lock().len() >= count {
				return;
			}
			tokio::task::yield_now().await;
		}
		panic!("scripted turn did not open");
	}

	fn input_contains_text(input: &TurnInput, expected: &str) -> bool {
		let items = match input {
			TurnInput::Full(thread) => thread.items.as_slice(),
			TurnInput::Delta(_, delta) => delta.append.as_slice(),
		};
		items.iter().any(|item| {
			matches!(
				item.kind.as_ref(),
				Some(thread::item::Kind::Message(message))
					if message.parts.iter().any(|part| {
						matches!(
							part.kind.as_ref(),
							Some(thread::part::Kind::Text(text)) if text == expected
						)
					})
			)
		})
	}

	fn worker(name: &str) -> ToolSpec {
		ToolSpec {
			name:            Str::new(name),
			rev:             Rev { family: sf!("test"), n: 1 },
			description:     sf!("test worker"),
			schema:          Bytes::from_static(br#"{"type":"object"}"#),
			constraint:      Constraint::None,
			effects:         Effects::empty(),
			projection_code: [0; 32],
		}
	}

	fn worker_claims() -> Claims {
		Claims { precedence: Precedence::DEFAULT, claimant: sf!("test/worker"), replaces: None }
	}

	#[tokio::test]
	async fn resumed_turn_freezes_durable_allowlist_then_fresh_turn_uses_snapshot() {
		let mut registry = ToolRegistry::new();
		registry
			.register_worker(worker("old"), Presentation::Device, worker_claims())
			.expect("register old");
		registry
			.register_worker(worker("new"), Presentation::Device, worker_claims())
			.expect("register new");
		let registry = Arc::new(registry);

		let mut old_options = crate::TurnOptions::default();
		old_options.params.model = "durable-model".to_owned();
		let mut new_options = crate::TurnOptions::default();
		new_options.params.model = "fresh-model".to_owned();
		let state = AgentState::new(crate::AgentSnapshot {
			turn: new_options.clone(),
			enabled_tools: Arc::from([sf!("new")]),
			registry: Arc::clone(&registry),
			..crate::AgentSnapshot::default()
		});

		let path = std::env::temp_dir().join(format!(
			"omp-agent-loop-allowlist-{}-{}.jsonl",
			std::process::id(),
			omp_core::Ulid::generate()
		));
		let mut journal = Journal::create(&path, &Header {
			v:       4,
			id:      SessionId(sf!("allowlist-test")),
			created: 1,
			cwd:     std::env::temp_dir(),
		})
		.expect("create journal");
		let durable_input = thread::Thread::default();
		journal
			.start_turn(1, TurnStart {
				turn_id:            sf!("durable-turn"),
				item_events:        Vec::new(),
				prompt_hash:        Hash32::new([7; 32]),
				prompt_head_events: Vec::new(),
				toolset_hash:       registry.slot_hash(),
				enabled_tools:      vec![sf!("old")],
				sequence_targets:   Vec::new(),
				input:              TurnInputRecord::Full { thread: durable_input.clone() },
				options:            TurnOptionsRecord {
					context_id: old_options.context_id.clone(),
					params:     old_options.params.clone(),
					executor:   old_options.executor.clone(),
					props:      old_options.props.clone(),
				},
			})
			.expect("persist durable start");

		let opened = Arc::new(Mutex::new(Vec::new()));
		let client = ScriptedClient {
			scripts: Arc::new(Mutex::new(VecDeque::from([
				outcome_script(Outcome {
					stop: pb::StopReason::StopEndTurn as i32,
					..Outcome::default()
				}),
				outcome_script(Outcome {
					stop: pb::StopReason::StopEndTurn as i32,
					..Outcome::default()
				}),
			]))),
			opened:  Arc::clone(&opened),
		};
		let (env, _transport) = EnvClient::in_process(1);
		let mut agent = Agent::new(client, env, state, journal, test_caps());

		let (_, _, _, _, resumed_tools) = agent
			.run_turn(TurnId::new("durable-turn"), Vec::new())
			.await
			.expect("resume durable turn");
		let (_, _, _, _, fresh_tools) = agent
			.run_turn(TurnId::new("fresh-turn"), Vec::new())
			.await
			.expect("run fresh turn");

		assert_eq!(resumed_tools.as_ref(), &[sf!("old")]);
		assert_eq!(fresh_tools.as_ref(), &[sf!("new")]);
		let opened = opened.lock();
		assert_eq!(opened.len(), 2);
		assert_eq!(opened[0].0.as_str(), "durable-turn");
		assert!(matches!(&opened[0].1, TurnInput::Full(thread) if thread == &durable_input));
		assert_eq!(opened[0].2.params, old_options.params);
		assert_eq!(opened[1].0.as_str(), "fresh-turn");
		assert_eq!(opened[1].2.params, new_options.params);
		assert_eq!(
			agent
				.journal()
				.latest_turn_start()
				.expect("fresh durable start")
				.enabled_tools,
			vec![sf!("new")]
		);
		drop(opened);
		drop(agent);
		std::fs::remove_file(path).expect("remove journal");
	}

	#[tokio::test]
	async fn caller_abort_settles_pending_stream_and_allows_follow_up() {
		let (journal, path) = test_journal("stream-abort");
		let opened = Arc::new(Mutex::new(Vec::new()));
		let client = ScriptedClient {
			scripts: Arc::new(Mutex::new(VecDeque::from([
				pending_text_script(),
				outcome_script(end_outcome("after abort")),
			]))),
			opened:  Arc::clone(&opened),
		};
		let (env, _transport) = EnvClient::in_process(1);
		let mut agent = Agent::new(
			client,
			env,
			AgentState::new(crate::AgentSnapshot::default()),
			journal,
			test_caps(),
		);
		let abort = agent.abort_handle();
		let aborting = async {
			wait_for_opened(&opened, 1).await;
			abort.abort();
		};
		let (summary, ()) = tokio::join!(
			agent.submit([message(thread::Role::User, "before abort")], TurnId::new("abort-turn"),),
			aborting,
		);
		let summary = summary.expect("abort returns a summary");
		assert!(summary.interrupted);
		assert!(summary.outcome.is_none());
		assert_eq!(summary.committed_turns, 0);
		assert!(agent.journal().pending_turn().is_none());
		let log = agent.journal().load().expect("load aborted journal");
		assert!((0..u64::try_from(log.len()).expect("log length fits")).any(|index| {
			matches!(
				log.get(index),
				Some(Entry::Ok(event))
					if matches!(&event.kind, Kind::TurnAbort(abort) if !abort.recoverable)
			)
		}));
		drop(log);

		let follow_up = agent
			.submit([message(thread::Role::User, "after abort")], TurnId::new("post-abort-turn"))
			.await
			.expect("follow-up submission succeeds");
		assert!(!follow_up.interrupted);
		assert!(follow_up.outcome.is_some());
		drop(agent);

		let reopened = Journal::open(&path).expect("reopen exhausted abort");
		assert!(reopened.pending_turn().is_none());
		assert!(reopened.pending_input_submission().is_none());
		assert!(reopened.recoverable_input_events().is_empty());
		drop(reopened);
		std::fs::remove_file(path).expect("remove journal");
	}

	#[tokio::test]
	async fn caller_abort_continues_into_queued_producer_input() {
		let (journal, path) = test_journal("abort-and-send");
		let opened = Arc::new(Mutex::new(Vec::new()));
		let client = ScriptedClient {
			scripts: Arc::new(Mutex::new(VecDeque::from([
				pending_text_script(),
				outcome_script(end_outcome("queued answer")),
			]))),
			opened:  Arc::clone(&opened),
		};
		let (env, _transport) = EnvClient::in_process(1);
		let mut agent = Agent::new(
			client,
			env,
			AgentState::new(crate::AgentSnapshot::default()),
			journal,
			test_caps(),
		);
		let abort = agent.abort_handle();
		let mailbox = agent.mailbox();
		let interrupting = async {
			wait_for_opened(&opened, 1).await;
			mailbox
				.try_enqueue(crate::Interrupt {
					class:  crate::InterruptClass::Immediate,
					item:   message(thread::Role::User, "queued user input"),
					source: crate::InterruptSource::Producer(sf!("user")),
				})
				.expect("enqueue producer input");
			abort.abort();
		};
		let (summary, ()) = tokio::join!(
			agent.submit(
				[message(thread::Role::User, "initial user input")],
				TurnId::new("interrupt-and-send"),
			),
			interrupting,
		);

		let summary = summary.expect("continued submission succeeds");
		assert!(!summary.interrupted);
		assert_eq!(summary.committed_turns, 1);
		assert!(summary.outcome.is_some());
		let opened = opened.lock();
		assert_eq!(opened.len(), 2);
		assert!(input_contains_text(&opened[1].1, "queued user input"));
		drop(opened);
		drop(agent);
		std::fs::remove_file(path).expect("remove journal");
	}
	#[tokio::test]
	async fn caller_abort_interrupts_tool_batch_and_stages_results() {
		let (journal, path) = test_journal("batch-abort");
		let identity = ToolIdentity { name: sf!("pending"), rev: Rev { family: sf!("test"), n: 1 } };
		let mut registry = ToolRegistry::new();
		registry
			.register_worker(worker(identity.name.as_str()), Presentation::Device, worker_claims())
			.expect("register pending tool");
		let registry = Arc::new(registry);
		let state = AgentState::new(crate::AgentSnapshot {
			enabled_tools: Arc::from([identity.name.clone()]),
			registry,
			..crate::AgentSnapshot::default()
		});
		let opened = Arc::new(Mutex::new(Vec::new()));
		let client = ScriptedClient {
			scripts: Arc::new(Mutex::new(VecDeque::from([pending_tool_script(&identity)]))),
			opened,
		};
		let (env, transport) = EnvClient::in_process(1);
		let (requests, responses) = transport.into_parts();
		let env_task = tokio::spawn(async move {
			let _responses = responses;
			while requests.recv_async().await.is_ok() {}
		});
		let mut agent = Agent::new(client, env, state, journal, test_caps());
		let abort = agent.abort_handle();
		let events = agent.events().subscribe_lossless();
		let aborting = async {
			loop {
				let event = events.recv().await.expect("agent event");
				if matches!(event.as_ref(), AgentEvent::PhaseChanged { to: AgentPhase::ToolBatch, .. })
				{
					abort.abort();
					break;
				}
			}
		};
		let (summary, ()) = tokio::join!(
			agent.submit(
				[message(thread::Role::User, "run pending tool")],
				TurnId::new("batch-abort-turn"),
			),
			aborting,
		);
		let summary = summary.expect("batch abort returns summary");
		assert!(summary.interrupted);
		assert_eq!(summary.committed_turns, 1);
		assert!(summary.outcome.is_some());
		assert!(agent.journal().pending_turn().is_none());
		assert!(
			agent.journal().pending_input_submission().is_some(),
			"interrupted tool results remain staged"
		);
		drop(agent);
		env_task.abort();
		std::fs::remove_file(path).expect("remove journal");
	}

	#[tokio::test]
	async fn rewind_truncates_projection_and_forces_full_post_rewind_turn() {
		let (journal, path) = test_journal("rewind");
		let opened = Arc::new(Mutex::new(Vec::new()));
		let client = ScriptedClient {
			scripts: Arc::new(Mutex::new(VecDeque::from([
				outcome_script(end_outcome("answer one")),
				outcome_script(end_outcome("answer two")),
				outcome_script(end_outcome("replacement answer")),
			]))),
			opened:  Arc::clone(&opened),
		};
		let (env, _transport) = EnvClient::in_process(1);
		let mut agent = Agent::new(
			client,
			env,
			AgentState::new(crate::AgentSnapshot::default()),
			journal,
			test_caps(),
		);
		agent
			.submit([message(thread::Role::User, "turn one")], TurnId::new("rewind-one"))
			.await
			.expect("first turn");
		agent
			.submit([message(thread::Role::User, "turn two")], TurnId::new("rewind-two"))
			.await
			.expect("second turn");
		let targets = agent.rewind_targets().expect("list rewind targets");
		assert_eq!(
			targets
				.iter()
				.map(|target| target.text.as_str())
				.collect::<Vec<_>>(),
			vec!["turn one", "turn two"]
		);
		let second = targets.last().expect("second rewind target").clone();
		let projected = agent.rewind(second.keep).expect("rewind second turn");
		assert!(projected.iter().any(|item| {
			matches!(
				item.kind.as_ref(),
				Some(thread::item::Kind::Message(message))
					if message.parts.iter().any(|part| {
						matches!(
							part.kind.as_ref(),
							Some(thread::part::Kind::Text(text)) if text == "turn one"
						)
					})
			)
		}));
		assert!(!projected.iter().any(|item| {
			matches!(
				item.kind.as_ref(),
				Some(thread::item::Kind::Message(message))
					if message.parts.iter().any(|part| {
						matches!(
							part.kind.as_ref(),
							Some(thread::part::Kind::Text(text)) if text == "turn two"
						)
					})
			)
		}));
		agent
			.submit([message(thread::Role::User, "replacement")], TurnId::new("rewind-replacement"))
			.await
			.expect("post-rewind turn");
		assert!(agent.prompt_hash.is_some(), "post-rewind turn re-rendered prompt head");
		let opened = opened.lock();
		assert_eq!(opened.len(), 3);
		assert!(matches!(&opened[2].1, TurnInput::Full(_)));
		assert!(input_contains_text(&opened[2].1, "turn one"));
		assert!(input_contains_text(&opened[2].1, "replacement"));
		drop(opened);

		let cleared = agent.rewind(None).expect("rewind to root");
		assert!(cleared.is_empty());
		drop(agent);
		std::fs::remove_file(path).expect("remove journal");
	}

	#[tokio::test]
	async fn rewind_discards_queued_user_steering_before_next_submission() {
		let (journal, path) = test_journal("rewind-steering");
		let opened = Arc::new(Mutex::new(Vec::new()));
		let client = ScriptedClient {
			scripts: Arc::new(Mutex::new(VecDeque::from([outcome_script(end_outcome(
				"replacement answer",
			))]))),
			opened:  Arc::clone(&opened),
		};
		let (env, _transport) = EnvClient::in_process(1);
		let mut agent = Agent::new(
			client,
			env,
			AgentState::new(crate::AgentSnapshot::default()),
			journal,
			test_caps(),
		);
		agent
			.mailbox()
			.try_enqueue(crate::Interrupt {
				class:  crate::InterruptClass::Immediate,
				item:   message(thread::Role::User, "stale steering"),
				source: crate::InterruptSource::Producer(sf!("user")),
			})
			.expect("enqueue stale steering");

		agent.rewind(None).expect("rewind to root");
		agent
			.submit([message(thread::Role::User, "replacement")], TurnId::new("replacement"))
			.await
			.expect("replacement turn");

		let opened = opened.lock();
		assert_eq!(opened.len(), 1);
		assert!(input_contains_text(&opened[0].1, "replacement"));
		assert!(!input_contains_text(&opened[0].1, "stale steering"));
		drop(opened);
		drop(agent);
		std::fs::remove_file(path).expect("remove journal");
	}

	#[tokio::test]
	async fn control_requests_complete_at_idle_and_active_turn_points() {
		let (journal, path) = test_journal("control-mailbox");
		let opened = Arc::new(Mutex::new(Vec::new()));
		let client = ScriptedClient {
			scripts: Arc::new(Mutex::new(VecDeque::from([
				outcome_script(end_outcome("idle drained")),
				pending_text_script(),
			]))),
			opened:  Arc::clone(&opened),
		};
		let (env, _transport) = EnvClient::in_process(1);
		let mut agent = Agent::new(
			client,
			env,
			AgentState::new(crate::AgentSnapshot::default()),
			journal,
			test_caps(),
		);
		let control = agent.control();
		let idle_request = tokio::spawn({
			let control = control.clone();
			async move { control.query(Vec::new()).await }
		});
		tokio::task::yield_now().await;
		agent
			.submit([message(thread::Role::User, "idle")], TurnId::new("idle"))
			.await
			.expect("idle turn");
		assert!(
			idle_request
				.await
				.expect("idle CONTROL task")
				.expect("idle CONTROL request")
				.is_empty()
		);

		let abort = agent.abort_handle();
		let active = tokio::spawn(async move {
			let result = agent
				.submit([message(thread::Role::User, "active")], TurnId::new("active"))
				.await;
			(agent, result)
		});
		wait_for_opened(&opened, 2).await;
		let rows = tokio::time::timeout(Duration::from_secs(1), control.query(Vec::new()))
			.await
			.expect("active CONTROL timeout")
			.expect("active CONTROL request");
		assert!(rows.is_empty());
		abort.abort();
		let (agent, result) = active.await.expect("active turn task");
		assert!(matches!(result, Err(AgentError::Interrupted)));
		drop(agent);
		std::fs::remove_file(path).expect("remove journal");
	}

	#[tokio::test]
	async fn scheduled_rewind_waits_for_active_tool_batch_boundary() {
		let (journal, path) = test_journal("scheduled-rewind-boundary");
		let identity = ToolIdentity { name: sf!("pending"), rev: Rev { family: sf!("test"), n: 1 } };
		let mut registry = ToolRegistry::new();
		registry
			.register_worker(worker(identity.name.as_str()), Presentation::Device, worker_claims())
			.expect("register pending tool");
		let state = AgentState::new(crate::AgentSnapshot {
			enabled_tools: Arc::from([identity.name.clone()]),
			registry: Arc::new(registry),
			..crate::AgentSnapshot::default()
		});
		let client = ScriptedClient {
			scripts: Arc::new(Mutex::new(VecDeque::from([pending_tool_script(&identity)]))),
			opened:  Arc::new(Mutex::new(Vec::new())),
		};
		let (env, transport) = EnvClient::in_process(1);
		let (requests, responses) = transport.into_parts();
		let env_task = tokio::spawn(async move {
			let _responses = responses;
			while requests.recv_async().await.is_ok() {}
		});
		let mut agent = Agent::new(client, env, state, journal, test_caps());
		let control = agent.control();
		let checkpoint = tokio::spawn({
			let control = control.clone();
			async move { control.checkpoint(sf!("before batch")).await }
		});
		tokio::task::yield_now().await;
		agent.drain_control();
		let checkpoint = checkpoint
			.await
			.expect("checkpoint task")
			.expect("checkpoint command");

		let events = agent.events().subscribe_lossless();
		let abort = agent.abort_handle();
		let scheduling = async {
			loop {
				let event = events.recv().await.expect("agent event");
				if matches!(event.as_ref(), AgentEvent::PhaseChanged { to: AgentPhase::ToolBatch, .. })
				{
					let ack = control
						.schedule_rewind(checkpoint, sf!("thread"))
						.await
						.expect("schedule rewind");
					assert_eq!(ack.target, checkpoint);
					abort.abort();
					break ack;
				}
			}
		};
		let (summary, ack) = tokio::join!(
			agent.submit(
				[message(thread::Role::User, "run pending tool")],
				TurnId::new("scheduled-rewind"),
			),
			scheduling,
		);
		let summary = summary.expect("rewind boundary summary");
		assert_eq!(ack.target, checkpoint);
		assert_eq!(summary.committed_turns, 1);

		let log = agent.journal.load().expect("load rewind journal");
		let mut settled = None;
		let mut rewinds = Vec::new();
		for index in 0..u64::try_from(log.len()).expect("journal length") {
			let Some(Entry::Ok(event)) = log.get(index) else {
				continue;
			};
			match &event.kind {
				Kind::InvocationTransition(transition)
					if transition.phase == InvocationPhase::Settled =>
				{
					settled = Some(index);
				},
				Kind::Rewind { to } => rewinds.push((index, *to)),
				_ => {},
			}
		}
		assert_eq!(rewinds.len(), 1, "rewind outcome is journaled exactly once");
		assert_eq!(rewinds[0].1, Some(checkpoint));
		assert!(
			settled.is_some_and(|settled| settled < rewinds[0].0),
			"rewind executes only after tool settlement is journaled"
		);
		drop(log);
		drop(agent);
		env_task.abort();
		std::fs::remove_file(path).expect("remove journal");
	}

	#[tokio::test]
	async fn deadline_wait_wins_over_long_backoff() {
		let deadline = std::time::Instant::now() + Duration::from_millis(1);
		let result = sleep_with_deadline(Duration::from_secs(60), Some(deadline)).await;
		assert!(matches!(result, Err(AgentError::Deadline)));
	}

	#[test]
	fn run_summary_extracts_yield_arguments_verbatim() {
		let call = thread::ToolCall {
			id: sf!("yield-call").to_string(),
			name: "yield".to_owned(),
			args_json: Bytes::from_static(
				br#"{"result":{"data":{"summary":{"purge":13,"keep":20}}}}"#,
			),
			..thread::ToolCall::default()
		};
		let summary = AgentRunSummary {
			outcome:         Some(Outcome {
				output: vec![Item {
					kind: Some(thread::item::Kind::ToolCall(call)),
					..Item::default()
				}],
				..Outcome::default()
			}),
			committed_turns: 1,
			interrupted:     false,
		};
		let schema = serde_json::json!({
			"type": "object",
			"properties": {"summary": {"type": "string"}},
			"required": ["summary"],
			"additionalProperties": false
		});
		let mut validator = YieldPayloadValidator::new(Some(schema), true);
		assert!(matches!(
			summary.yield_payload(&mut validator),
			Err(YieldPayloadError::SchemaViolation { path, rule: "type" })
				if path.as_str() == "/summary"
		));
	}
}
