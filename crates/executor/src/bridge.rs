use std::{
	future::Future,
	pin::Pin,
	task::{Context, Poll},
};

/// The single embedded Tokio runtime hosting Tokio-bound edge subsystems.
pub struct TokioBridge {
	runtime: tokio::runtime::Runtime,
}

impl TokioBridge {
	/// Creates a multi-thread Tokio bridge.
	///
	/// When `workers` is `None`, available parallelism is clamped to two through
	/// eight worker threads. Worker threads are named `omp-io-{i}`.
	#[must_use]
	pub fn new(workers: Option<usize>) -> Self {
		let workers = workers.unwrap_or_else(|| {
			std::thread::available_parallelism()
				.map_or(2, usize::from)
				.clamp(2, 8)
		});
		let next_thread = std::sync::atomic::AtomicUsize::new(0);
		let runtime = tokio::runtime::Builder::new_multi_thread()
			.worker_threads(workers.max(1))
			.thread_name_fn(move || {
				let index = next_thread.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
				format!("omp-io-{index}")
			})
			.enable_all()
			.build()
			.expect("failed to build Tokio edge bridge");
		Self { runtime }
	}

	/// Returns a handle for explicitly composing Tokio-bound edge services.
	#[must_use]
	pub fn handle(&self) -> tokio::runtime::Handle {
		self.runtime.handle().clone()
	}

	/// Polls `future` on the bridge runtime.
	///
	/// Awaiting the returned task is runtime-neutral. The task aborts when
	/// dropped unless [`BridgeTask::detach`] is called.
	pub fn spawn<T: Send + 'static>(
		&self,
		future: impl Future<Output = T> + Send + 'static,
	) -> BridgeTask<T> {
		BridgeTask { handle: Some(self.runtime.spawn(future)) }
	}

	/// Runs a future to completion on the bridge runtime.
	pub fn block_on<F: Future>(&self, future: F) -> F::Output {
		self.runtime.block_on(future)
	}
}

/// A Tokio bridge task that aborts when dropped.
///
/// Awaiting propagates task panics and panics if the task was cancelled,
/// matching an unwrapped Tokio join handle.
#[must_use = "dropping a bridge task cancels it; await it or call detach"]
pub struct BridgeTask<T> {
	handle: Option<tokio::task::JoinHandle<T>>,
}

impl<T> BridgeTask<T> {
	/// Detaches the task so it continues running after this handle is consumed.
	pub fn detach(mut self) {
		self.handle.take();
	}
}

impl<T> Future for BridgeTask<T> {
	type Output = T;

	fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
		let handle = self.handle.as_mut().expect("polled BridgeTask after completion");
		match Pin::new(handle).poll(context) {
			Poll::Ready(Ok(output)) => {
				self.handle.take();
				Poll::Ready(output)
			},
			Poll::Ready(Err(error)) if error.is_panic() => {
				self.handle.take();
				std::panic::resume_unwind(error.into_panic());
			},
			Poll::Ready(Err(error)) => {
				self.handle.take();
				panic!("Tokio bridge task was cancelled: {error}");
			},
			Poll::Pending => Poll::Pending,
		}
	}
}

impl<T> Drop for BridgeTask<T> {
	fn drop(&mut self) {
		if let Some(handle) = self.handle.take() {
			handle.abort();
		}
	}
}
