use std::{
	cmp::Ordering,
	collections::{BinaryHeap, VecDeque},
	future::Future,
	pin::Pin,
	sync::Arc,
	task::{Context, Poll, Waker},
	time::Duration,
};

use async_task::{Runnable, Task};
use parking_lot::Mutex;
use rand::{RngExt as _, SeedableRng as _, rngs::SmallRng};

#[derive(Clone)]
pub(crate) struct Scheduler {
	state: Arc<Mutex<State>>,
}

struct State {
	runnable: VecDeque<Runnable>,
	timers:   BinaryHeap<TimerEntry>,
	now:      Duration,
	next_timer: u64,
	rng:      SmallRng,
	seed:     u64,
}

struct TimerEntry {
	deadline: Duration,
	sequence: u64,
	state:    Arc<Mutex<TimerState>>,
}

impl PartialEq for TimerEntry {
	fn eq(&self, other: &Self) -> bool {
		self.deadline == other.deadline && self.sequence == other.sequence
	}
}

impl Eq for TimerEntry {}

impl PartialOrd for TimerEntry {
	fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
		Some(self.cmp(other))
	}
}

impl Ord for TimerEntry {
	fn cmp(&self, other: &Self) -> Ordering {
		other
			.deadline
			.cmp(&self.deadline)
			.then_with(|| other.sequence.cmp(&self.sequence))
	}
}

#[derive(Default)]
struct TimerState {
	fired:     bool,
	cancelled: bool,
	waker:     Option<Waker>,
}

impl Scheduler {
	pub(crate) fn new(seed: u64) -> Self {
		Self {
			state: Arc::new(Mutex::new(State {
				runnable: VecDeque::new(),
				timers: BinaryHeap::new(),
				now: Duration::ZERO,
				next_timer: 0,
				rng: SmallRng::seed_from_u64(seed),
				seed,
			})),
		}
	}

	pub(crate) fn spawn<T: Send + 'static>(
		&self,
		future: impl Future<Output = T> + Send + 'static,
	) -> Task<T> {
		let scheduler = self.clone();
		let (runnable, task) = async_task::spawn(future, move |runnable| {
			scheduler.enqueue(runnable);
		});
		runnable.schedule();
		task
	}

	fn enqueue(&self, runnable: Runnable) {
		self.state.lock().runnable.push_back(runnable);
	}

	fn run_one(&self) -> bool {
		let runnable = {
			let mut state = self.state.lock();
			if state.runnable.is_empty() {
				return false;
			}
			let runnable_count = state.runnable.len();
			let index = state.rng.random_range(0..runnable_count);
			state.runnable.remove(index).expect("runnable index is in bounds")
		};
		runnable.run();
		true
	}

	pub(crate) fn run_until_parked(&self) {
		while self.run_one() {}
	}

	pub(crate) fn advance_clock(&self, duration: Duration) {
		let wakers = {
			let mut state = self.state.lock();
			state.now = state.now.saturating_add(duration);
			let now = state.now;
			let mut wakers = Vec::new();
			while state.timers.peek().is_some_and(|timer| timer.deadline <= now) {
				let timer = state.timers.pop().expect("peeked timer exists");
				let mut timer_state = timer.state.lock();
				if !timer_state.cancelled {
					timer_state.fired = true;
					if let Some(waker) = timer_state.waker.take() {
						wakers.push(waker);
					}
				}
			}
			wakers
		};
		for waker in wakers {
			waker.wake();
		}
		self.run_until_parked();
	}

	pub(crate) fn timer(&self, after: Duration) -> Timer {
		let deadline = self.state.lock().now.saturating_add(after);
		Timer {
			scheduler: self.clone(),
			deadline,
			state: Arc::new(Mutex::new(TimerState::default())),
			registered: false,
		}
	}

	pub(crate) fn seed(&self) -> u64 {
		self.state.lock().seed
	}
}

pub(crate) struct Timer {
	scheduler:  Scheduler,
	deadline:   Duration,
	state:      Arc<Mutex<TimerState>>,
	registered: bool,
}

impl Future for Timer {
	type Output = ();

	fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
		let register = !self.registered;
		self.registered = true;
		let deadline = self.deadline;
		let timer_state = Arc::clone(&self.state);
		let scheduler_state = Arc::clone(&self.scheduler.state);
		let mut scheduler = scheduler_state.lock();
		let mut timer = timer_state.lock();
		if timer.fired || deadline <= scheduler.now {
			timer.fired = true;
			return Poll::Ready(());
		}
		timer.waker = Some(context.waker().clone());
		drop(timer);
		if register {
			let sequence = scheduler.next_timer;
			scheduler.next_timer = scheduler.next_timer.wrapping_add(1);
			scheduler.timers.push(TimerEntry { deadline, sequence, state: timer_state });
		}
		Poll::Pending
	}
}

impl Drop for Timer {
	fn drop(&mut self) {
		self.state.lock().cancelled = true;
	}
}

/// Control handle for a seeded deterministic executor and its virtual clock.
#[derive(Clone)]
pub struct DeterministicHandle {
	pub(crate) scheduler: Scheduler,
}

impl DeterministicHandle {
	/// Runs scheduled tasks until no runnable work remains.
	pub fn run_until_parked(&self) {
		self.scheduler.run_until_parked();
	}

	/// Advances virtual time, wakes every due timer, and runs until parked.
	pub fn advance_clock(&self, duration: Duration) {
		self.scheduler.advance_clock(duration);
	}

	/// Returns the seed controlling runnable selection.
	#[must_use]
	pub fn seed(&self) -> u64 {
		self.scheduler.seed()
	}
}
