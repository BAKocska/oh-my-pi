use std::sync::atomic::{AtomicBool, Ordering};

use flume::Sender;
use omp_core::Str;

/// A nonblocking cancellation guard for one invocation or command request.
///
/// Dropping an armed guard queues cancellation for exactly
/// [`Self::request_id`]. The server-owned session containing an exec request is
/// not represented by this guard and therefore cannot be cancelled by it.
/// Detached work must call [`Self::relinquish`] explicitly before the guard is
/// dropped.
#[derive(Debug)]
#[must_use]
pub struct RunGuard {
	state: GuardState,
}

#[derive(Debug)]
struct GuardState {
	request_id: u64,
	armed:      AtomicBool,
	cancel:     Sender<u64>,
}

impl RunGuard {
	pub(crate) const fn new(request_id: u64, cancel: Sender<u64>) -> Self {
		Self { state: GuardState { request_id, armed: AtomicBool::new(true), cancel } }
	}

	/// Returns the request correlation identifier scoped by this guard.
	pub const fn request_id(&self) -> u64 {
		self.state.request_id
	}

	/// Returns whether dropping this guard will request cancellation.
	pub fn is_armed(&self) -> bool {
		self.state.armed.load(Ordering::Acquire)
	}

	/// Queues cancellation now.
	///
	/// This operation never blocks. Repeated calls and a later drop are
	/// idempotent: at most one cancellation is queued.
	pub fn cancel(&self) {
		self.state.cancel();
	}

	/// Explicitly transfers responsibility for detached work to the server.
	///
	/// Consuming the guard without sending cancellation makes the ownership
	/// transition visible at the call site.
	pub fn relinquish(self) {
		self.state.disarm();
	}
}

impl Drop for RunGuard {
	fn drop(&mut self) {
		self.state.cancel();
	}
}

/// Nonblocking termination ownership for one named worker generation.
///
/// Dropping an armed lease queues termination on the supervisor's unbounded
/// control lane. A lease is generation-specific, so a late drop cannot
/// terminate a replacement worker with the same name.
#[derive(Debug)]
#[must_use]
pub struct WorkerLease {
	state: WorkerLeaseState,
}

#[derive(Debug)]
struct WorkerLeaseState {
	name:       Str,
	generation: u64,
	armed:      AtomicBool,
	terminate:  Sender<(Str, u64)>,
}

impl WorkerLease {
	/// Creates an armed lease for one worker generation.
	pub fn new(name: impl Into<Str>, generation: u64, terminate: Sender<(Str, u64)>) -> Self {
		Self {
			state: WorkerLeaseState {
				name: name.into(),
				generation,
				armed: AtomicBool::new(true),
				terminate,
			},
		}
	}

	/// Returns the leased worker generation.
	pub const fn generation(&self) -> u64 {
		self.state.generation
	}

	/// Returns whether dropping this lease will terminate its generation.
	pub fn is_armed(&self) -> bool {
		self.state.armed.load(Ordering::Acquire)
	}

	/// Transfers termination responsibility to the supervisor.
	pub fn relinquish(self) {
		self.state.disarm();
	}
}

impl Drop for WorkerLease {
	fn drop(&mut self) {
		self.state.terminate();
	}
}

impl WorkerLeaseState {
	fn disarm(&self) {
		self.armed.store(false, Ordering::Release);
	}

	fn terminate(&self) {
		if self
			.armed
			.compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
			.is_ok()
		{
			// This sender is always the supervisor's unbounded lifecycle lane.
			let _ = self
				.terminate
				.try_send((self.name.clone(), self.generation));
		}
	}
}

impl GuardState {
	fn disarm(&self) {
		self.armed.store(false, Ordering::Release);
	}

	fn cancel(&self) {
		if self
			.armed
			.compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
			.is_ok()
		{
			// This sender is always the unbounded guard-control queue owned by the
			// client dispatcher, so try_send is nonblocking and cannot be full.
			let _ = self.cancel.try_send(self.request_id);
		}
	}
}
