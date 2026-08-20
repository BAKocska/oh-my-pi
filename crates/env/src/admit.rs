//! Reversed-direction invocation admission handling.

use omp_proto::env::v1::{Admission, AdmitInvocation};

/// Answers server-initiated admission queries for one environment client.
///
/// Implementations run outside the frame dispatcher. The associated future
/// keeps the per-query path unboxed while allowing the client to retain one
/// installed handler for its lifetime.
pub trait Admitter: Send + Sync + 'static {
	/// Future returned by [`Self::admit`].
	type Future<'client>: Future<Output = Admission> + Send + 'client
	where
		Self: 'client;

	/// Decides one invocation without blocking the frame dispatcher.
	fn admit<'client>(&'client self, query: AdmitInvocation) -> Self::Future<'client>;
}
