//! Multiplexed, generation-fenced extension-host invocation routing.

use std::{
	collections::{BTreeMap, VecDeque},
	sync::Arc,
	time::Instant,
};

use omp_core::{CowBytes, SparseMap, Str};
use omp_proto::toolhost::v1::{WorkerFrame, lifecycle_worker_envelope, worker_frame};
use parking_lot::Mutex;
use thiserror::Error;

use super::lifecycle::{AvailabilityBatch, AvailabilitySink};
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
	pub fn accept_frame(&self, generation: u64, _frame: &WorkerFrame) -> Result<(), DispatchError> {
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
