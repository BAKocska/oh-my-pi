//! Secret-typed command credential resolution with process-local caching.

use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc, time::Duration};

use omp_core::{SecretString, Str};
use parking_lot::Mutex;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// Boxed environment-execution future at the cold command-credential boundary.
pub type CommandExecutionFuture =
	Pin<Box<dyn Future<Output = Result<SecretString, CommandCredentialError>> + Send + 'static>>;

/// Injected command executor. Implementations must cross the Environment
/// boundary rather than spawning a process directly.
pub trait CommandCredentialExecutor: Send + Sync + 'static {
	/// Executes one configured command and returns only its secret stdout value.
	fn execute(&self, command: Str, cancellation: CancellationToken) -> CommandExecutionFuture;
}

/// A redaction-safe command credential failure.
#[derive(Clone, Copy, Debug, thiserror::Error, Eq, PartialEq)]
pub enum CommandCredentialError {
	/// The caller cancelled resolution.
	#[error("command credential resolution was cancelled")]
	Cancelled,
	/// The configured timeout expired.
	#[error("command credential resolution timed out")]
	Timeout,
	/// The environment rejected or failed the command.
	#[error("command credential execution failed")]
	Execution,
	/// Standard output exceeded the secret-value bound.
	#[error("command credential output exceeded its limit")]
	OutputTooLarge,
	/// Standard output was not UTF-8.
	#[error("command credential output was not UTF-8")]
	InvalidUtf8,
	/// Trimmed standard output was empty.
	#[error("command credential output was empty")]
	Empty,
	/// A recent failure is still inside the bounded retry delay.
	#[error("command credential resolution is temporarily unavailable")]
	FailureCached,
}

#[derive(Debug)]
enum CacheEntry {
	Resolving(Arc<Notify>),
	Ready(SecretString),
	FailedUntil(tokio::time::Instant),
}

/// Single-flight, process-lifetime successful command credential cache.
///
/// Successful values never leave their secret wrapper and remain cached for
/// this process. Failures are cached only for `failure_ttl`, preventing tight
/// retry loops without making a transient environment failure permanent.
pub struct CommandCredentialResolver {
	executor:    Arc<dyn CommandCredentialExecutor>,
	failure_ttl: Duration,
	cache:       Mutex<BTreeMap<Str, CacheEntry>>,
}

impl CommandCredentialResolver {
	/// Creates a resolver over an injected Environment executor.
	#[must_use]
	pub fn new(executor: Arc<dyn CommandCredentialExecutor>, failure_ttl: Duration) -> Self {
		Self { executor, failure_ttl, cache: Mutex::new(BTreeMap::new()) }
	}

	/// Resolves a configured command, sharing concurrent work for the same
	/// command.
	pub async fn resolve(
		&self,
		command: &str,
		cancellation: CancellationToken,
	) -> Result<SecretString, CommandCredentialError> {
		let command = command.trim();
		if command.is_empty() {
			return Err(CommandCredentialError::Empty);
		}
		let key = Str::new(command);
		loop {
			let pending = {
				let mut cache = self.cache.lock();
				match cache.get(&key) {
					Some(CacheEntry::Ready(secret)) => return Ok(secret.clone()),
					Some(CacheEntry::FailedUntil(until)) if *until > tokio::time::Instant::now() => {
						return Err(CommandCredentialError::FailureCached);
					},
					Some(CacheEntry::Resolving(notify)) => Some(Arc::clone(notify)),
					Some(CacheEntry::FailedUntil(_)) | None => {
						let notify = Arc::new(Notify::new());
						cache.insert(key.clone(), CacheEntry::Resolving(Arc::clone(&notify)));
						None
					},
				}
			};
			if let Some(notify) = pending {
				tokio::select! {
					() = cancellation.cancelled() => return Err(CommandCredentialError::Cancelled),
					() = notify.notified() => continue,
				}
			}
			let result = self
				.executor
				.execute(key.clone(), cancellation.clone())
				.await;
			let notify = {
				let mut cache = self.cache.lock();
				let notify = match cache.remove(&key) {
					Some(CacheEntry::Resolving(notify)) => notify,
					_ => Arc::new(Notify::new()),
				};
				match &result {
					Ok(secret) => {
						cache.insert(key.clone(), CacheEntry::Ready(secret.clone()));
					},
					Err(_) => {
						cache.insert(
							key.clone(),
							CacheEntry::FailedUntil(tokio::time::Instant::now() + self.failure_ttl),
						);
					},
				}
				notify
			};
			notify.notify_waiters();
			return result;
		}
	}
}

#[cfg(test)]
mod tests {
	use std::sync::atomic::{AtomicUsize, Ordering};

	use omp_core::ExposeSecret as _;

	use super::*;

	struct CountingExecutor {
		calls: AtomicUsize,
		fail:  AtomicUsize,
	}

	impl CommandCredentialExecutor for CountingExecutor {
		fn execute(&self, _: Str, _: CancellationToken) -> CommandExecutionFuture {
			let call = self.calls.fetch_add(1, Ordering::SeqCst);
			let fail = self.fail.load(Ordering::SeqCst);
			Box::pin(async move {
				tokio::task::yield_now().await;
				if call < fail {
					Err(CommandCredentialError::Execution)
				} else {
					Ok(SecretString::from("secret-marker"))
				}
			})
		}
	}

	#[tokio::test]
	async fn concurrent_success_executes_once_and_never_debugs_secret() {
		let executor =
			Arc::new(CountingExecutor { calls: AtomicUsize::new(0), fail: AtomicUsize::new(0) });
		let resolver =
			Arc::new(CommandCredentialResolver::new(executor.clone(), Duration::from_millis(10)));
		let (left, right) = tokio::join!(
			resolver.resolve("credential command", CancellationToken::new()),
			resolver.resolve("credential command", CancellationToken::new())
		);
		assert_eq!(left.unwrap().expose_secret(), "secret-marker");
		assert_eq!(right.unwrap().expose_secret(), "secret-marker");
		assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
		assert!(!format!("{resolver:?}").contains("secret-marker"));
	}

	#[tokio::test]
	async fn transient_failure_retries_after_ttl() {
		let executor =
			Arc::new(CountingExecutor { calls: AtomicUsize::new(0), fail: AtomicUsize::new(1) });
		let resolver = CommandCredentialResolver::new(executor.clone(), Duration::from_millis(1));
		assert!(matches!(
			resolver
				.resolve("credential command", CancellationToken::new())
				.await,
			Err(CommandCredentialError::Execution)
		));
		assert!(matches!(
			resolver
				.resolve("credential command", CancellationToken::new())
				.await,
			Err(CommandCredentialError::FailureCached)
		));
		tokio::time::sleep(Duration::from_millis(2)).await;
		assert_eq!(
			resolver
				.resolve("credential command", CancellationToken::new())
				.await
				.unwrap()
				.expose_secret(),
			"secret-marker"
		);
		assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
	}
}

impl std::fmt::Debug for CommandCredentialResolver {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter
			.debug_struct("CommandCredentialResolver")
			.field("failure_ttl", &self.failure_ttl)
			.finish_non_exhaustive()
	}
}
