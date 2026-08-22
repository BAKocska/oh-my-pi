//! Application-owned browser escalation adapter for inference search.

use std::{
	future::{Future, poll_fn},
	thread,
};

use bytes::Bytes;
use omp_llm_inference::{
	codec::Cancellation,
	transport::browser::{
		BrowserFetch, BrowserFetchError, BrowserFetchRequest, BrowserFetchResponse,
	},
};
use omp_webview::{
	CloseHandle, Engine, FrameConfig, SurfaceKind, WebViewBuilder, automation::ExtractFormat,
};

/// Stateless entry to supervised, ephemeral browser fetches.
#[derive(Clone, Copy, Debug, Default)]
pub struct BrowserFetchAdapter;

impl BrowserFetch for BrowserFetchAdapter {
	fn fetch(
		&self,
		request: BrowserFetchRequest,
		cancellation: Cancellation,
	) -> impl Future<Output = Result<BrowserFetchResponse, BrowserFetchError>> + Send + '_ {
		async move {
			request.validate()?;
			if cancellation.is_cancelled() {
				return Err(BrowserFetchError::Cancelled);
			}
			let deadline = request.deadline;
			let worker_cancellation = cancellation.clone();
			let (close_tx, close_rx) = flume::bounded::<CloseHandle>(1);
			let (result_tx, result_rx) = flume::bounded(1);
			thread::Builder::new()
				.name("omp-browser-fetch".to_owned())
				.spawn(move || {
					let result = fetch_on_driver(request, &worker_cancellation, &close_tx);
					let _ = result_tx.send(result);
				})
				.map_err(|_| BrowserFetchError::Unavailable)?;

			let cancelled = poll_fn(|context| cancellation.poll_cancelled(context));
			tokio::select! {
				result = result_rx.recv_async() => result.map_err(|_| BrowserFetchError::Unavailable)?,
				() = cancelled => {
					if let Ok(handle) = close_rx.recv_async().await {
						let _ = handle.close();
					}
					let _ = result_rx.recv_async().await;
					Err(BrowserFetchError::Cancelled)
				},
				() = tokio::time::sleep(deadline) => {
					if let Ok(handle) = close_rx.recv_async().await {
						let _ = handle.close();
					}
					let _ = result_rx.recv_async().await;
					Err(BrowserFetchError::TimedOut)
				},
			}
		}
	}
}

fn fetch_on_driver(
	request: BrowserFetchRequest,
	cancellation: &Cancellation,
	close: &flume::Sender<CloseHandle>,
) -> Result<BrowserFetchResponse, BrowserFetchError> {
	let engine = Engine::find(SurfaceKind::Frames).map_err(map_webview_error)?;
	let mut builder = WebViewBuilder::new(engine)
		.url(request.url.clone())
		.incognito(true);
	for header in &request.headers {
		builder = builder.header(header.name.clone(), header.value.clone());
	}
	let view = builder
		.build_frames(FrameConfig { fps_cap: Some(1.0), ..FrameConfig::default() })
		.map_err(map_webview_error)?;
	if let Some(handle) = view.close_handle() {
		let _ = close.send(handle);
	}
	if cancellation.is_cancelled() {
		return Err(BrowserFetchError::Cancelled);
	}
	let tab = view.automation();
	tab.wait_for_navigation(request.deadline)
		.map_err(map_webview_error)?;
	if cancellation.is_cancelled() {
		return Err(BrowserFetchError::Cancelled);
	}
	let body = tab
		.extract(ExtractFormat::Html)
		.map_err(map_webview_error)?;
	if body.len() > request.max_bytes {
		return Err(BrowserFetchError::ResponseTooLarge);
	}
	Ok(BrowserFetchResponse {
		final_url:    view.url(),
		status:       None,
		body:         Bytes::copy_from_slice(body.as_bytes()),
		content_type: Some(omp_core::Str::new_static("text/html")),
	})
}

fn map_webview_error(error: omp_webview::Error) -> BrowserFetchError {
	match error {
		omp_webview::Error::NoEngine(_)
		| omp_webview::Error::Launch { .. }
		| omp_webview::Error::Unsupported(_) => BrowserFetchError::Unavailable,
		omp_webview::Error::Timeout(_) => BrowserFetchError::TimedOut,
		_ => BrowserFetchError::Navigation,
	}
}
