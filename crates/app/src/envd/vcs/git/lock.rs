//! Fair, cancellation-aware repository operation admission.

use std::{
	collections::HashMap,
	path::PathBuf,
	sync::{Arc, LazyLock, Weak},
};

use parking_lot::Mutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use super::repo::Repository;

const READ_PERMITS: u32 = 16;
static LOCKS: LazyLock<Mutex<HashMap<PathBuf, Weak<Semaphore>>>> =
	LazyLock::new(|| Mutex::new(HashMap::new()));

/// Repository lock acquisition failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum LockError {
	/// The caller cancelled while waiting in the fair queue.
	#[error("repository lock acquisition was cancelled")]
	Cancelled,
	/// The repository lock registry was closed unexpectedly.
	#[error("repository lock closed")]
	Closed,
}

/// RAII ownership of a repository read or write admission permit.
///
/// Reads consume one of a bounded number of permits. Writes consume every
/// permit, so Tokio's FIFO semaphore queue serializes writers and prevents new
/// readers from bypassing an already queued writer.
pub struct RepositoryLockGuard {
	_permit: OwnedSemaphorePermit,
}

/// Acquires one bounded read permit for `repository`.
pub async fn read(
	repository: &Repository,
	cancel: &CancellationToken,
) -> Result<RepositoryLockGuard, LockError> {
	acquire(repository, 1, cancel).await
}

/// Acquires exclusive write ownership shared by every linked worktree.
pub async fn write(
	repository: &Repository,
	cancel: &CancellationToken,
) -> Result<RepositoryLockGuard, LockError> {
	acquire(repository, READ_PERMITS, cancel).await
}

async fn acquire(
	repository: &Repository,
	permits: u32,
	cancel: &CancellationToken,
) -> Result<RepositoryLockGuard, LockError> {
	let semaphore = repository_semaphore(repository);
	let acquisition = semaphore.acquire_many_owned(permits);
	tokio::pin!(acquisition);
	let permit = tokio::select! {
		biased;
		() = cancel.cancelled() => return Err(LockError::Cancelled),
		result = &mut acquisition => result.map_err(|_| LockError::Closed)?,
	};
	Ok(RepositoryLockGuard { _permit: permit })
}

fn repository_semaphore(repository: &Repository) -> Arc<Semaphore> {
	let mut locks = LOCKS.lock();
	locks.retain(|_, semaphore| semaphore.strong_count() > 0);
	if let Some(semaphore) = locks.get(&repository.common_dir).and_then(Weak::upgrade) {
		return semaphore;
	}
	let semaphore = Arc::new(Semaphore::new(READ_PERMITS as usize));
	locks.insert(repository.common_dir.clone(), Arc::downgrade(&semaphore));
	semaphore
}
