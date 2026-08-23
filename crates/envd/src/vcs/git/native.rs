//! In-process gitoxide execution shared by the read facades.
//!
//! Routine repository reads (refs, status, history, configuration) run
//! entirely in-process on the blocking pool. The hardened system-Git runner
//! remains the compatibility path: callers fall back to it only when gitoxide
//! cannot represent the repository (for example reftable ref storage or a
//! future repository-format extension), never for ordinary reads.

use std::{
	error::Error,
	path::Path,
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
};

use tokio_util::sync::CancellationToken;

/// Object-cache size for peeling and commit parsing, matching a modest
/// per-operation working set without pinning large packs in memory.
const OBJECT_CACHE_BYTES: usize = 16 * 1024 * 1024;

/// One blocking in-process Git operation failure.
///
/// Every variant other than [`NativeError::Cancelled`] is a signal to retry
/// through the system-Git runner; the inner error is kept for diagnostics.
#[derive(Debug, thiserror::Error)]
pub enum NativeError {
	/// The repository could not be discovered or opened by gitoxide.
	// Foreign fat error type; boxed so the Err path stays pointer-sized.
	#[error("gitoxide cannot open the repository")]
	Open(#[source] Box<gix::discover::Error>),
	/// The operation-specific gitoxide step failed.
	///
	/// The concrete gitoxide error is preserved as the source for logging;
	/// callers never branch on it because any operational failure retries
	/// through the system-Git runner.
	#[error("gitoxide operation failed")]
	Operation(#[source] Box<dyn Error + Send + Sync>),
	/// The caller cancelled while the blocking operation was in flight.
	#[error("gitoxide operation was cancelled")]
	Cancelled,
	/// The blocking task panicked before producing a result.
	#[error("gitoxide task failed")]
	Join,
}

impl NativeError {
	/// Returns whether the caller cancelled, which must never fall back to a
	/// system-Git retry.
	pub const fn is_cancelled(&self) -> bool {
		matches!(self, Self::Cancelled)
	}
}

/// Wraps one operation-specific gitoxide failure.
pub fn op_error(error: impl Error + Send + Sync + 'static) -> NativeError {
	NativeError::Operation(Box::new(error))
}

/// Runs one blocking gitoxide operation against the repository discovered
/// from `cwd`.
///
/// Discovery walks ancestors exactly like Git does. The operation receives a
/// stop flag that is raised when `cancel` fires; long iterations must poll it
/// so an abandoned blocking task winds down promptly.
pub async fn with_repository<T, F>(
	cwd: &Path,
	cancel: &CancellationToken,
	operation: F,
) -> Result<T, NativeError>
where
	T: Send + 'static,
	F: FnOnce(&mut gix::Repository, &AtomicBool) -> Result<T, NativeError> + Send + 'static,
{
	let cwd = cwd.to_path_buf();
	let stop = Arc::new(AtomicBool::new(false));
	let flag = Arc::clone(&stop);
	let task = tokio::task::spawn_blocking(move || {
		let mut repository =
			gix::discover(&cwd).map_err(|error| NativeError::Open(Box::new(error)))?;
		configure(&mut repository);
		operation(&mut repository, &flag)
	});
	tokio::select! {
		biased;
		() = cancel.cancelled() => {
			stop.store(true, Ordering::Relaxed);
			Err(NativeError::Cancelled)
		},
		joined = task => joined.map_err(|_| NativeError::Join)?,
	}
}

fn configure(repository: &mut gix::Repository) {
	repository.object_cache_size_if_unset(OBJECT_CACHE_BYTES);
}
