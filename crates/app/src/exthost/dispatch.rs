//! Multiplexed, generation-fenced extension-host invocation routing.

use std::{
	collections::{BTreeMap, VecDeque},
	sync::Arc,
	time::Instant,
};

use omp_agent::JournalCustomEntry;
use omp_core::{CowBytes, SparseMap, Str};
use omp_proto::toolhost::v1::{
	Dispatch as HookDispatch, FallbackLifecycleEventV1, HookEventId, HookHostEnvelope,
	LifecycleEventContext, RetryLifecycleEventV1, TodoReminderEventV1, TtsrTriggeredEventV1,
	WorkerFrame, hook_host_envelope, lifecycle_worker_envelope, ui_worker_envelope, worker_frame,
};
use parking_lot::Mutex;
use prost::Message;
use thiserror::Error;

use super::lifecycle::{
	AvailabilityBatch, AvailabilitySink, HeadlessLifecycleSink, HeadlessSinkError,
};
use crate::envd::worker::HostKey;

/// Per-declaration callback overlap policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallbackConcurrency {
	/// The ordinary actor default: exactly one callback enters Python at once.
	Serialized,
	/// An explicit declaration-level overlap limit.
	Concurrent {
		/// Maximum overlapping callback entries.
		limit: usize,
	},
	/// An explicitly thread-safe callback may overlap without a fixed limit.
	Threadsafe,
}

impl CallbackConcurrency {
	fn admits(self, running: usize) -> bool {
		match self {
			Self::Serialized => running == 0,
			Self::Concurrent { limit } => running < limit.max(1),
			Self::Threadsafe => true,
		}
	}
}

/// One host-owned deadline for a dispatched event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EventDeadline {
	/// Monotonic expiration instant.
	pub at: Instant,
}

/// Maximum encoded payload for an observational extension lifecycle event.
pub const MAX_LIFECYCLE_EVENT_BYTES: usize = 8 * 1024;

/// One revisioned observational lifecycle fact ready for hook dispatch.
#[derive(Clone, Debug)]
pub struct LifecycleEvent {
	/// Closed protocol event identifier.
	pub id:       HookEventId,
	/// Payload schema revision.
	pub revision: u32,
	/// Already encoded revision-specific payload.
	pub payload:  CowBytes<'static>,
}

/// Invalid revisioned lifecycle event payload.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum LifecycleEventError {
	/// The event is not one of the sanctioned observational lifecycle facts.
	#[error("hook event is not a sanctioned lifecycle observation")]
	Unsupported,
	/// Only revision 1 is currently admitted.
	#[error("unsupported lifecycle event revision {0}")]
	Revision(u32),
	/// Encoded payload exceeded the extension event ceiling.
	#[error("lifecycle event payload exceeds {MAX_LIFECYCLE_EVENT_BYTES} bytes")]
	PayloadTooLarge,
}

impl LifecycleEvent {
	/// Validates an authoritative event and encodes its hook envelope. The
	/// resulting bytes still travel through the ordinary dispatch router,
	/// deadline, quota, cancellation, and failure-policy path.
	pub fn encode(
		self,
		dispatch_id: u64,
		deadline_ms: u64,
	) -> Result<CowBytes<'static>, LifecycleEventError> {
		if !matches!(
			self.id,
			HookEventId::HookEventTtsrTriggered
				| HookEventId::HookEventTodoReminder
				| HookEventId::HookEventRetryStart
				| HookEventId::HookEventRetryEnd
				| HookEventId::HookEventFallbackApplied
				| HookEventId::HookEventFallbackSucceeded
		) {
			return Err(LifecycleEventError::Unsupported);
		}
		if self.revision != 1 {
			return Err(LifecycleEventError::Revision(self.revision));
		}
		if self.payload.len() > MAX_LIFECYCLE_EVENT_BYTES {
			return Err(LifecycleEventError::PayloadTooLarge);
		}
		let envelope = HookHostEnvelope {
			body:  Some(hook_host_envelope::Body::Dispatch(HookDispatch {
				event_id: self.id as i32,
				event_rev: self.revision,
				dispatch_id,
				phase: omp_proto::toolhost::v1::HookPhase::Observe as i32,
				payload: self.payload.clone().into_bytes(),
				deadline_ms,
				subscription_ids: Vec::new(),
				props: None,
			})),
			props: None,
		};
		Ok(CowBytes::from(envelope.encode_to_vec()))
	}
}

const TTSR_INJECTION_KIND: &str = "dev.omp.core.ttsr-injection";
const MAX_EVENT_ID_BYTES: usize = 128;
const MAX_EVENT_TEXT_BYTES: usize = 4096;

/// Projection failure for one durable authoritative lifecycle journal fact.
#[derive(Debug, Error)]
pub enum JournalLifecycleEventError {
	/// The core-authored TTSR payload is absent.
	#[error("TTSR journal entry has no data payload")]
	MissingPayload,
	/// The core-authored TTSR payload did not match its fixed revision.
	#[error("TTSR journal entry payload is malformed")]
	InvalidPayload(#[source] serde_json::Error),
	/// A required provenance identifier exceeded the event protocol bound.
	#[error("TTSR lifecycle event provenance exceeds protocol bounds")]
	ProvenanceTooLarge,
}

#[derive(serde::Deserialize)]
struct TtsrInjection<'a> {
	turn_id: &'a str,
	rules:   Vec<&'a str>,
	content: &'a str,
}

/// Projects the authoritative durable TTSR custom entry into the revisioned
/// extension event. Raw streamed deltas are deliberately not accepted by this
/// seam; the physical journal index supplies the exactly-once sequence.
pub fn ttsr_event_from_journal(
	session_id: &str,
	entry: &JournalCustomEntry,
) -> Result<Option<LifecycleEvent>, JournalLifecycleEventError> {
	if entry.entry.kind() != TTSR_INJECTION_KIND {
		return Ok(None);
	}
	let raw = entry
		.entry
		.data()
		.ok_or(JournalLifecycleEventError::MissingPayload)?;
	let payload: TtsrInjection<'_> =
		serde_json::from_str(raw.get()).map_err(JournalLifecycleEventError::InvalidPayload)?;
	if session_id.len() > MAX_EVENT_ID_BYTES || payload.turn_id.len() > MAX_EVENT_ID_BYTES {
		return Err(JournalLifecycleEventError::ProvenanceTooLarge);
	}
	let mut rules = payload.rules.join(",");
	rules.truncate(rules.floor_char_boundary(MAX_EVENT_TEXT_BYTES));
	let mut matched = payload.content.to_owned();
	matched.truncate(matched.floor_char_boundary(MAX_EVENT_TEXT_BYTES));
	let event = TtsrTriggeredEventV1 {
		context: Some(LifecycleEventContext {
			session_id: session_id.to_owned(),
			turn_id:    payload.turn_id.to_owned(),
			sequence:   entry.index,
		}),
		rule: rules,
		matched,
		interrupted: true,
	};
	Ok(Some(LifecycleEvent {
		id:       HookEventId::HookEventTtsrTriggered,
		revision: 1,
		payload:  CowBytes::from(event.encode_to_vec()),
	}))
}

/// Emits the revision-1 todo reminder contract from the authoritative todo
/// projection.
pub fn todo_reminder_event(
	context: LifecycleEventContext,
	pending: u32,
	reminder: Str,
) -> Result<LifecycleEvent, LifecycleEventError> {
	let event = TodoReminderEventV1 {
		context: Some(context),
		pending,
		reminder: bounded_event_text(reminder, MAX_EVENT_TEXT_BYTES),
	};
	lifecycle_event(HookEventId::HookEventTodoReminder, event)
}

/// Emits one revision-1 inference retry transition.
pub fn retry_event(
	context: LifecycleEventContext,
	started: bool,
	attempt: u32,
	maximum: u32,
	delay_ms: u64,
	reason: Str,
	outcome: Option<Str>,
) -> Result<LifecycleEvent, LifecycleEventError> {
	let event = RetryLifecycleEventV1 {
		context: Some(context),
		attempt,
		maximum,
		delay_ms,
		reason: bounded_event_text(reason, 512),
		outcome: outcome.map(|value| bounded_event_text(value, 512)),
	};
	lifecycle_event(
		if started {
			HookEventId::HookEventRetryStart
		} else {
			HookEventId::HookEventRetryEnd
		},
		event,
	)
}

/// Emits one revision-1 inference fallback transition.
pub fn fallback_event(
	context: LifecycleEventContext,
	succeeded: bool,
	source_model: Str,
	target_model: Str,
	reason: Str,
) -> Result<LifecycleEvent, LifecycleEventError> {
	let event = FallbackLifecycleEventV1 {
		context:      Some(context),
		source_model: bounded_event_text(source_model, 512),
		target_model: bounded_event_text(target_model, 512),
		reason:       bounded_event_text(reason, 512),
	};
	lifecycle_event(
		if succeeded {
			HookEventId::HookEventFallbackSucceeded
		} else {
			HookEventId::HookEventFallbackApplied
		},
		event,
	)
}

fn lifecycle_event(
	id: HookEventId,
	payload: impl Message,
) -> Result<LifecycleEvent, LifecycleEventError> {
	let payload = CowBytes::from(payload.encode_to_vec());
	if payload.len() > MAX_LIFECYCLE_EVENT_BYTES {
		return Err(LifecycleEventError::PayloadTooLarge);
	}
	Ok(LifecycleEvent { id, revision: 1, payload })
}

fn bounded_event_text(value: Str, limit: usize) -> String {
	let mut value = value.to_string();
	value.truncate(value.floor_char_boundary(limit));
	value
}

/// Invocation bytes awaiting host dispatch.
#[derive(Clone, Debug)]
pub struct DispatchRequest {
	/// Nonzero host-local correlation id.
	pub id:       u64,
	/// Registered callback overlap policy.
	pub policy:   CallbackConcurrency,
	/// Deadline applied by the host frame pump.
	pub deadline: EventDeadline,
	/// Already encoded request payload.
	pub payload:  CowBytes<'static>,
}

/// Correlated completion receiver returned to the caller.
pub struct DispatchPending {
	response: flume::Receiver<Result<CowBytes<'static>, DispatchError>>,
}

impl DispatchPending {
	/// Waits for the terminal worker response.
	pub async fn response(self) -> Result<CowBytes<'static>, DispatchError> {
		self
			.response
			.recv_async()
			.await
			.map_err(|_| DispatchError::HostGone)?
	}
}

struct Pending {
	generation: u64,
	deadline:   EventDeadline,
	response:   flume::Sender<Result<CowBytes<'static>, DispatchError>>,
}

struct ExtensionActor {
	running: usize,
	queued:  VecDeque<DispatchRequest>,
}

/// Failure while projecting a verified worker frame into a headless sink.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum HeadlessDispatchError {
	/// Worker dispatch generation or correlation was stale.
	#[error(transparent)]
	Dispatch(#[from] DispatchError),
	/// The owning headless lifecycle sink rejected the frame.
	#[error(transparent)]
	Sink(#[from] HeadlessSinkError),
}

/// One generation-fenced host router.
///
/// Frame multiplexing only correlates concurrent CONTROL traffic. Callback
/// entry remains serialized unless the declaration explicitly opts out.
pub struct DispatchRouter {
	host:       HostKey,
	generation: u64,
	pending:    Arc<Mutex<SparseMap<u64, Pending>>>,
	actors:     BTreeMap<Str, ExtensionActor>,
}

/// Router rejection or terminal failure.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum DispatchError {
	/// Zero cannot identify an invocation.
	#[error("dispatch correlation id must be nonzero")]
	ZeroId,
	/// A duplicate live correlation was supplied.
	#[error("dispatch correlation {0} is already live")]
	Duplicate(u64),
	/// A frame arrived from an old child generation.
	#[error("stale worker frame generation: expected {expected}, got {actual}")]
	StaleGeneration {
		/// Current host generation.
		expected: u64,
		/// Generation authenticated at the transport boundary.
		actual:   u64,
	},
	/// A terminal frame named no live invocation.
	#[error("stale worker frame correlation {0}")]
	StaleCorrelation(u64),
	/// The child disconnected before a terminal response.
	#[error("extension host disconnected")]
	HostGone,
	/// A per-event deadline elapsed.
	#[error("extension event deadline elapsed")]
	Deadline,
}

impl DispatchRouter {
	/// Creates a router for one authenticated child generation.
	#[must_use]
	pub fn new(host: HostKey, generation: u64) -> Self {
		Self {
			host,
			generation,
			pending: Arc::new(Mutex::new(SparseMap::new())),
			actors: BTreeMap::new(),
		}
	}

	/// Queues an invocation and installs its correlation before any frame is
	/// written. Returns the request immediately only when actor policy admits
	/// it.
	pub fn dispatch(
		&mut self,
		extension: impl Into<Str>,
		request: DispatchRequest,
	) -> Result<(Option<DispatchRequest>, DispatchPending), DispatchError> {
		if request.id == 0 {
			return Err(DispatchError::ZeroId);
		}
		let (tx, rx) = flume::bounded(1);
		if self.pending.lock().get(request.id).is_some() {
			return Err(DispatchError::Duplicate(request.id));
		}
		self.pending.lock().insert(request.id, Pending {
			generation: self.generation,
			deadline:   request.deadline,
			response:   tx,
		});
		let actor = self
			.actors
			.entry(extension.into())
			.or_insert_with(|| ExtensionActor { running: 0, queued: VecDeque::new() });
		if actor.policy_admits(request.policy) {
			actor.running += 1;
			Ok((Some(request), DispatchPending { response: rx }))
		} else {
			actor.queued.push_back(request);
			Ok((None, DispatchPending { response: rx }))
		}
	}

	/// Validates every inbound frame against the transport-authenticated child
	/// generation before domain-specific dispatch examines the frame body.
	pub const fn accept_frame(
		&self,
		generation: u64,
		_frame: &WorkerFrame,
	) -> Result<(), DispatchError> {
		if generation == self.generation {
			Ok(())
		} else {
			Err(DispatchError::StaleGeneration { expected: self.generation, actual: generation })
		}
	}

	/// Consumes a generation-fenced `SetAvailability` lifecycle frame.
	///
	/// The caller supplies the generation authenticated by the CONTROL
	/// transport. A stale frame therefore fails before it reaches the shared
	/// registry or emits a turn-boundary notification.
	///
	/// Returns `true` only when the worker frame contained this lifecycle arm.
	pub fn dispatch_availability(
		&self,
		generation: u64,
		frame: WorkerFrame,
		sink: &dyn AvailabilitySink,
	) -> Result<bool, DispatchError> {
		self.accept_frame(generation, &frame)?;
		let Some(worker_frame::Body::Lifecycle(lifecycle)) = frame.body else {
			return Ok(false);
		};
		let Some(lifecycle_worker_envelope::Body::SetAvailability(availability)) = lifecycle.body
		else {
			return Ok(false);
		};
		sink.set_availability(AvailabilityBatch::from_wire(availability));
		Ok(true)
	}

	/// Consumes typed UI effects and requests into the shared headless sink.
	///
	/// Returns `true` only for a retained UI payload. Registration and dispatch
	/// result frames remain owned by their dedicated registries.
	pub fn dispatch_headless_ui(
		&self,
		generation: u64,
		frame: WorkerFrame,
		sink: &HeadlessLifecycleSink,
	) -> Result<bool, HeadlessDispatchError> {
		self.accept_frame(generation, &frame)?;
		let Some(worker_frame::Body::Ui(ui)) = frame.body else {
			return Ok(false);
		};
		match ui.body {
			Some(ui_worker_envelope::Body::Effect(effect)) => {
				sink.ui_effect(generation, effect)?;
				Ok(true)
			},
			Some(ui_worker_envelope::Body::Request(request)) => {
				sink.ui_request(generation, request)?;
				Ok(true)
			},
			_ => Ok(false),
		}
	}

	/// Completes a correlation and releases one serialized callback slot.
	pub fn complete(
		&mut self,
		extension: &str,
		id: u64,
		generation: u64,
		result: Result<CowBytes<'static>, DispatchError>,
	) -> Result<Option<DispatchRequest>, DispatchError> {
		if generation != self.generation {
			return Err(DispatchError::StaleGeneration {
				expected: self.generation,
				actual:   generation,
			});
		}
		let record = self
			.pending
			.lock()
			.remove(id)
			.ok_or(DispatchError::StaleCorrelation(id))?;
		if record.generation != generation {
			return Err(DispatchError::StaleGeneration {
				expected: record.generation,
				actual:   generation,
			});
		}
		let _ = record.response.send(result);
		let Some(actor) = self.actors.get_mut(extension) else {
			return Ok(None);
		};
		actor.running = actor.running.saturating_sub(1);
		let next = actor.queued.pop_front();
		if next.is_some() {
			actor.running += 1;
		}
		Ok(next)
	}

	/// Expires outstanding per-host event deadlines without waiting for another
	/// frame.
	pub fn expire(&self, now: Instant) {
		self.pending.lock().retain(|_, record| {
			if record.deadline.at > now {
				return true;
			}
			let _ = record.response.send(Err(DispatchError::Deadline));
			false
		});
	}

	/// Returns the authenticated host identity.
	#[must_use]
	pub const fn host(&self) -> &HostKey {
		&self.host
	}
}

impl ExtensionActor {
	fn policy_admits(&self, policy: CallbackConcurrency) -> bool {
		policy.admits(self.running)
	}
}
