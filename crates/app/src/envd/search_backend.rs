//! Late-bound bridge from the environment tool registry to the one inference
//! facade.

use std::sync::OnceLock;

use omp_core::{Str, sf};
use omp_proto::inference::v1 as pb;
use omp_tools::web_search::{BackendError, SearchBackend};
use thiserror::Error;

use crate::rpc_adapter::InferenceRpc;

/// Failure to bind the one production inference facade.
#[derive(Clone, Copy, Debug, Error)]
pub(crate) enum SearchBindingError {
	/// A facade was already installed for this environment generation.
	#[error("web search inference facade is already bound")]
	AlreadyBound,
}

/// Late-bound application DI seam used by `web_search@1`.
///
/// Environment tools are assembled before the inference facade. The bridge is
/// stable inside the immutable tool registry and receives the already-built
/// facade exactly once; it never constructs providers or credential state.
pub(crate) struct SearchBridgeHost {
	inference: OnceLock<SearchFacade>,
}

enum SearchFacade {
	Local(InferenceRpc),
	Remote(pb::inference_client::InferenceClient<tonic::transport::Channel>),
}

impl SearchBridgeHost {
	/// Creates an unbound host for registry construction.
	#[must_use]
	pub(crate) const fn new() -> Self {
		Self { inference: OnceLock::new() }
	}

	/// Installs the one application-owned inference facade.
	pub(crate) fn bind(&self, inference: InferenceRpc) -> Result<(), SearchBindingError> {
		self
			.inference
			.set(SearchFacade::Local(inference))
			.map_err(|_| SearchBindingError::AlreadyBound)
	}

	/// Installs a client for an already-running inference daemon.
	pub(crate) fn bind_remote(
		&self,
		channel: tonic::transport::Channel,
	) -> Result<(), SearchBindingError> {
		self
			.inference
			.set(SearchFacade::Remote(pb::inference_client::InferenceClient::new(channel)))
			.map_err(|_| SearchBindingError::AlreadyBound)
	}
}

impl SearchBackend for SearchBridgeHost {
	fn search(
		&self,
		request: pb::SearchRequest,
	) -> impl Future<Output = Result<pb::SearchResponse, BackendError>> + Send + '_ {
		async move {
			let inference = self.inference.get().ok_or_else(|| BackendError {
				code:    sf!("backend_unbound"),
				message: sf!("web search is unavailable before inference startup completes"),
			})?;
			let response = match inference {
				SearchFacade::Local(inference) => {
					<InferenceRpc as pb::inference_server::Inference>::search(
						inference,
						tonic::Request::new(request),
					)
					.await
				},
				SearchFacade::Remote(client) => {
					let mut client = client.clone();
					client.search(tonic::Request::new(request)).await
				},
			}
			.map_err(|status| BackendError {
				code:    Str::new(status.code().to_string()),
				message: sf!("the inference search request failed"),
			})?;
			Ok(response.into_inner())
		}
	}
}

impl std::fmt::Debug for SearchBridgeHost {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter
			.debug_struct("SearchBridgeHost")
			.field("bound", &self.inference.get().is_some())
			.finish()
	}
}
