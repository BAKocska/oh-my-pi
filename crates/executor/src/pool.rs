use std::{future::Future, sync::Arc, time::Duration};

#[derive(Clone)]
pub(crate) struct Pool {
	executor: Arc<async_executor::Executor<'static>>,
}

impl Pool {
	pub(crate) fn new(workers: Option<usize>) -> Self {
		let executor = Arc::new(async_executor::Executor::new());
		let workers = workers.unwrap_or_else(|| {
			std::thread::available_parallelism()
				.map_or(2, usize::from)
				.clamp(2, 8)
		});
		for index in 0..workers.max(1) {
			let executor = Arc::clone(&executor);
			std::thread::Builder::new()
				.name(format!("omp-core-{index}"))
				.spawn(move || async_io::block_on(executor.run(std::future::pending::<()>())))
				.expect("failed to spawn omp core executor worker");
		}
		Self { executor }
	}

	pub(crate) fn spawn<T: Send + 'static>(
		&self,
		future: impl Future<Output = T> + Send + 'static,
	) -> async_task::Task<T> {
		self.executor.spawn(future)
	}

	pub(crate) fn block_on<F: Future>(&self, future: F) -> F::Output {
		async_io::block_on(self.executor.run(future))
	}

	pub(crate) fn timer(after: Duration) -> async_io::Timer {
		async_io::Timer::after(after)
	}

	pub(crate) fn interval(period: Duration) -> async_io::Timer {
		async_io::Timer::interval(period)
	}
}
