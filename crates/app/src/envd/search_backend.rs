//! Late-bound bridge from the environment tool registry to the one inference
//! facade.

use std::sync::OnceLock;

use futures::StreamExt as _;
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

	/// Routes one image generation/edit through the already-bound inference
	/// facade and returns the final artifact blobs.
	pub(crate) async fn generate_image(
		&self,
		request: pb::GenerateImageRequest,
	) -> Result<Vec<omp_proto::thread::v1::Blob>, BackendError> {
		let inference = self.inference.get().ok_or_else(unbound_media)?;
		match inference {
			SearchFacade::Local(inference) => {
				let response = <InferenceRpc as pb::inference_server::Inference>::generate_image(
					inference,
					tonic::Request::new(request),
				)
				.await
				.map_err(media_status)?;
				collect_images(response.into_inner()).await
			},
			SearchFacade::Remote(client) => {
				let mut client = client.clone();
				let response = client
					.generate_image(tonic::Request::new(request))
					.await
					.map_err(media_status)?;
				collect_images(response.into_inner()).await
			},
		}
	}

	/// Routes speech synthesis and concatenates encoded chunks in wire order.
	pub(crate) async fn speak(&self, request: pb::SpeakRequest) -> Result<Vec<u8>, BackendError> {
		let inference = self.inference.get().ok_or_else(unbound_media)?;
		match inference {
			SearchFacade::Local(inference) => {
				let response = <InferenceRpc as pb::inference_server::Inference>::speak(
					inference,
					tonic::Request::new(request),
				)
				.await
				.map_err(media_status)?;
				collect_audio(response.into_inner()).await
			},
			SearchFacade::Remote(client) => {
				let mut client = client.clone();
				let response = client
					.speak(tonic::Request::new(request))
					.await
					.map_err(media_status)?;
				collect_audio(response.into_inner()).await
			},
		}
	}
}
async fn collect_images<S>(mut events: S) -> Result<Vec<omp_proto::thread::v1::Blob>, BackendError>
where
	S: futures::Stream<Item = Result<pb::ImageEvent, tonic::Status>> + Unpin,
{
	while let Some(event) = events.next().await {
		let event = event.map_err(media_status)?;
		if let Some(pb::image_event::Event::Done(done)) = event.event {
			return Ok(done.images);
		}
	}
	Err(BackendError {
		code:    sf!("media_stream_incomplete"),
		message: sf!("image generation ended without a final artifact"),
	})
}

async fn collect_audio<S>(mut events: S) -> Result<Vec<u8>, BackendError>
where
	S: futures::Stream<Item = Result<pb::SpeakEvent, tonic::Status>> + Unpin,
{
	let mut audio = Vec::new();
	while let Some(event) = events.next().await {
		match event.map_err(media_status)?.event {
			Some(pb::speak_event::Event::Chunk(chunk)) => audio.extend_from_slice(&chunk.audio),
			Some(pb::speak_event::Event::Done(done)) => {
				if let Some(blob) = done.audio {
					audio.extend_from_slice(&blob.inline);
				}
				return Ok(audio);
			},
			None => {},
		}
	}
	Err(BackendError {
		code:    sf!("media_stream_incomplete"),
		message: sf!("speech synthesis ended without a final receipt"),
	})
}
fn unbound_media() -> BackendError {
	BackendError {
		code:    sf!("backend_unbound"),
		message: sf!("media inference is unavailable before inference startup completes"),
	}
}

fn media_status(status: tonic::Status) -> BackendError {
	BackendError {
		code:    Str::new(status.code().to_string()),
		message: sf!("the inference media request failed"),
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
