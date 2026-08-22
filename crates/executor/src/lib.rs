//! OMP-owned scheduling primitives for the runtime-neutral application core.

mod bridge;
mod deterministic;
mod pool;
#[cfg(unix)]
pub mod signal;

use std::{
	future::Future,
	pin::Pin,
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
	task::{Context, Poll, Wake, Waker},
	time::Duration,
};

pub use bridge::{BridgeTask, TokioBridge};
pub use deterministic::DeterministicHandle;
use futures_lite::Stream;

/// A cloneable handle to the OMP core executor.
#[derive(Clone)]
pub struct Executor {
	inner: Arc<Inner>,
}

enum Inner {
	Pool(pool::Pool),
	Deterministic(deterministic::Scheduler),
}

impl Executor {
	/// Creates a production executor pool.
	///
	/// When `workers` is `None`, available parallelism is clamped to two through
	/// eight workers. Worker threads are named `omp-core-{i}`.
	#[must_use]
	pub fn new(workers: Option<usize>) -> Self {
		Self { inner: Arc::new(Inner::Pool(pool::Pool::new(workers))) }
	}

	/// Creates a seeded, single-thread deterministic scheduler with virtual
	/// time.
	#[must_use]
	pub fn deterministic(seed: u64) -> (Self, DeterministicHandle) {
		let scheduler = deterministic::Scheduler::new(seed);
		let handle = DeterministicHandle { scheduler: scheduler.clone() };
		(Self { inner: Arc::new(Inner::Deterministic(scheduler)) }, handle)
	}

	/// Spawns a task that is cancelled when its handle is dropped.
	pub fn spawn<T: Send + 'static>(
		&self,
		future: impl Future<Output = T> + Send + 'static,
	) -> Task<T> {
		let task = match self.inner.as_ref() {
			Inner::Pool(pool) => pool.spawn(future),
			Inner::Deterministic(scheduler) => scheduler.spawn(future),
		};
		Task { inner: task }
	}

	/// Runs a blocking closure on the blocking pool.
	///
	/// Deterministic executors run the closure inline when the task is polled.
	pub fn unblock<T: Send + 'static>(
		&self,
		function: impl FnOnce() -> T + Send + 'static,
	) -> Task<T> {
		match self.inner.as_ref() {
			Inner::Pool(_) => self.spawn(blocking::unblock(function)),
			Inner::Deterministic(_) => self.spawn(async move { function() }),
		}
	}

	/// Returns a future that completes after the specified duration.
	#[must_use]
	pub fn timer(&self, after: Duration) -> Timer {
		let inner = match self.inner.as_ref() {
			Inner::Pool(_) => TimerInner::Pool(pool::Pool::timer(after)),
			Inner::Deterministic(scheduler) => TimerInner::Deterministic(scheduler.timer(after)),
		};
		Timer { inner }
	}

	/// Returns a stream that yields at the specified period.
	///
	/// # Panics
	///
	/// Panics when `period` is zero.
	#[must_use]
	pub fn interval(&self, period: Duration) -> Interval {
		assert!(!period.is_zero(), "interval period must be non-zero");
		let inner = match self.inner.as_ref() {
			Inner::Pool(_) => IntervalInner::Pool(pool::Pool::interval(period)),
			Inner::Deterministic(scheduler) => IntervalInner::Deterministic {
				timer: scheduler.timer(period),
				scheduler: scheduler.clone(),
				period,
			},
		};
		Interval { inner }
	}

	/// Runs `future` until completion or until its deadline elapses.
	pub fn timeout<F: Future>(
		&self,
		after: Duration,
		future: F,
	) -> impl Future<Output = Result<F::Output, Elapsed>> {
		let timer = self.timer(after);
		futures_lite::future::or(async move { Ok(future.await) }, async move {
			timer.await;
			Err(Elapsed)
		})
	}

	/// Runs a main-thread future to completion.
	///
	/// A deterministic executor panics if the root future parks without any
	/// runnable work; virtual time must be advanced through its handle.
	pub fn block_on<F: Future>(&self, future: F) -> F::Output {
		match self.inner.as_ref() {
			Inner::Pool(pool) => pool.block_on(future),
			Inner::Deterministic(scheduler) => deterministic_block_on(scheduler, future),
		}
	}
}

/// A spawned task that is cancelled when dropped.
#[must_use = "dropping a task cancels it; await it or call detach"]
pub struct Task<T> {
	inner: async_task::Task<T>,
}

impl<T> Task<T> {
	/// Detaches the task so it continues running after this handle is consumed.
	pub fn detach(self) {
		self.inner.detach();
	}
}

impl<T> Future for Task<T> {
	type Output = T;

	fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
		Pin::new(&mut self.inner).poll(context)
	}
}

/// A future that completes after an executor-relative duration.
#[must_use = "timers do nothing unless awaited or polled"]
pub struct Timer {
	inner: TimerInner,
}

enum TimerInner {
	Pool(async_io::Timer),
	Deterministic(deterministic::Timer),
}

impl Future for Timer {
	type Output = ();

	fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
		match &mut self.inner {
			TimerInner::Pool(timer) => Pin::new(timer).poll(context).map(|_| ()),
			TimerInner::Deterministic(timer) => Pin::new(timer).poll(context),
		}
	}
}

/// A stream that yields at a fixed executor-relative period.
#[must_use = "intervals do nothing unless polled"]
pub struct Interval {
	inner: IntervalInner,
}

enum IntervalInner {
	Pool(async_io::Timer),
	Deterministic {
		scheduler: deterministic::Scheduler,
		period:    Duration,
		timer:     deterministic::Timer,
	},
}

impl Stream for Interval {
	type Item = ();

	fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
		match &mut self.inner {
			IntervalInner::Pool(timer) => Pin::new(timer)
				.poll_next(context)
				.map(|item| item.map(|_| ())),
			IntervalInner::Deterministic { scheduler, period, timer } => {
				if Pin::new(&mut *timer).poll(context).is_pending() {
					return Poll::Pending;
				}
				*timer = scheduler.timer(*period);
				Poll::Ready(Some(()))
			},
		}
	}
}

/// Error returned when an executor deadline wins a timeout race.
#[derive(Debug, thiserror::Error)]
#[error("deadline elapsed")]
pub struct Elapsed;

struct RootWake(AtomicBool);

impl Wake for RootWake {
	fn wake(self: Arc<Self>) {
		self.0.store(true, Ordering::Release);
	}

	fn wake_by_ref(self: &Arc<Self>) {
		self.0.store(true, Ordering::Release);
	}
}

fn deterministic_block_on<F: Future>(scheduler: &deterministic::Scheduler, future: F) -> F::Output {
	let wake = Arc::new(RootWake(AtomicBool::new(true)));
	let waker = Waker::from(Arc::clone(&wake));
	let mut context = Context::from_waker(&waker);
	let mut future = std::pin::pin!(future);
	loop {
		wake.0.store(false, Ordering::Release);
		if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
			return output;
		}
		scheduler.run_until_parked();
		if !wake.0.load(Ordering::Acquire) {
			panic!("deterministic executor parked before the root future completed");
		}
	}
}

#[cfg(test)]
mod tests {
	use futures_lite::future::yield_now;
	use parking_lot::Mutex;

	use super::*;

	fn deterministic_order(seed: u64) -> Vec<(usize, usize)> {
		let (executor, handle) = Executor::deterministic(seed);
		let log = Arc::new(Mutex::new(Vec::new()));
		for task_id in 0..3 {
			let executor = executor.clone();
			let log = Arc::clone(&log);
			executor
				.clone()
				.spawn(async move {
					log.lock().push((task_id, 0));
					yield_now().await;
					log.lock().push((task_id, 1));
					executor.timer(Duration::from_millis(1)).await;
					log.lock().push((task_id, 2));
				})
				.detach();
		}
		handle.run_until_parked();
		handle.advance_clock(Duration::from_millis(1));
		Arc::try_unwrap(log)
			.expect("tasks released log")
			.into_inner()
	}

	#[test]
	fn seeded_scheduling_is_repeatable_and_seed_sensitive() {
		let first = deterministic_order(42);
		assert_eq!(first, deterministic_order(42));
		assert_ne!(first, deterministic_order(7));
	}

	#[test]
	fn task_drop_cancels_and_detach_keeps_running() {
		let (executor, handle) = Executor::deterministic(1);
		let cancelled = Arc::new(AtomicBool::new(false));
		let flag = Arc::clone(&cancelled);
		let timer_executor = executor.clone();
		let task = executor.spawn(async move {
			timer_executor.timer(Duration::from_millis(10)).await;
			flag.store(true, Ordering::Release);
		});
		handle.run_until_parked();
		drop(task);
		handle.advance_clock(Duration::from_millis(20));
		assert!(!cancelled.load(Ordering::Acquire));

		let detached = Arc::new(AtomicBool::new(false));
		let flag = Arc::clone(&detached);
		let timer_executor = executor.clone();
		executor
			.spawn(async move {
				timer_executor.timer(Duration::from_millis(10)).await;
				flag.store(true, Ordering::Release);
			})
			.detach();
		handle.run_until_parked();
		handle.advance_clock(Duration::from_millis(10));
		assert!(detached.load(Ordering::Acquire));
	}

	#[test]
	fn unblock_returns_value() {
		let (executor, _) = Executor::deterministic(3);
		assert_eq!(executor.block_on(executor.unblock(|| 42)), 42);
	}

	#[test]
	fn production_pool_runs_timer_and_cross_thread_task() {
		let executor = Executor::new(Some(2));
		let task_executor = executor.clone();
		let task = std::thread::spawn(move || task_executor.spawn(async { 7 }))
			.join()
			.expect("cross-thread spawner succeeds");
		let value = executor.block_on(async {
			executor.timer(Duration::from_millis(1)).await;
			task.await
		});
		assert_eq!(value, 7);
	}

	#[test]
	fn bridge_roundtrip_polls_tokio_future_on_edge_runtime() {
		let executor = Executor::new(None);
		let bridge = TokioBridge::new(Some(2));
		let value = executor.block_on(async {
			bridge
				.spawn(async {
					tokio::time::sleep(Duration::from_millis(10)).await;
					7
				})
				.await
		});
		assert_eq!(value, 7);
	}
}
