//! Capability-gated, bounded HTTP egress owned by the Environment.

use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures::StreamExt as _;
use http::{HeaderMap, HeaderName, HeaderValue, Method};
use omp_proto::env::v1 as pb;
use thiserror::Error;

use super::worker_pool::MAX_TUNNEL_BUFFER_BYTES;

// Reuse the existing retained wire-buffer ceiling rather than defining a
// second Environment response-size policy.
#[derive(Clone)]
pub(crate) struct HttpEgressHost {
	client: wreq::Client,
}

impl HttpEgressHost {
	pub(crate) fn new() -> Self {
		let client = wreq::Client::builder()
			.build()
			.expect("build Environment HTTP egress client");
		Self { client }
	}

	pub(crate) async fn request(
		&self,
		request: pb::HttpRequest,
	) -> Result<pb::HttpResponse, HttpEgressError> {
		let timeout_ms = request.timeout_ms;
		let request = self.request_once(request);
		if timeout_ms == 0 {
			request.await
		} else {
			tokio::time::timeout(Duration::from_millis(timeout_ms), request)
				.await
				.map_err(|_| HttpEgressError::TimedOut)?
		}
	}

	async fn request_once(
		&self,
		request: pb::HttpRequest,
	) -> Result<pb::HttpResponse, HttpEgressError> {
		let method = parse_method(&request.method)?;
		let url = url::Url::parse(&request.url)
			.map_err(|error| HttpEgressError::InvalidArgument(error.to_string()))?;
		if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
			return Err(HttpEgressError::InvalidArgument(
				"HTTP egress URL must use http or https and include a host".to_owned(),
			));
		}

		let headers = parse_headers(&request.headers)?;
		let response = self
			.client
			.request(method, url.as_str())
			.headers(headers)
			.body(request.body)
			.send()
			.await
			.map_err(HttpEgressError::transport)?;
		let status = u32::from(response.status().as_u16());
		let headers = response_headers(response.headers());
		let body = read_bounded(response).await?;
		Ok(pb::HttpResponse { status, headers, body, props: None })
	}
}

#[derive(Debug, Error)]
pub(crate) enum HttpEgressError {
	#[error("invalid HTTP egress request: {0}")]
	InvalidArgument(String),
	#[error("HTTP egress request timed out")]
	TimedOut,
	#[error("HTTP egress response exceeds the bounded frame limit")]
	ResponseTooLarge,
	#[error("HTTP egress transport failed: {0}")]
	Transport(String),
}

impl HttpEgressError {
	fn transport(error: wreq::Error) -> Self {
		Self::Transport(error.to_string())
	}
}

fn parse_method(method: &str) -> Result<Method, HttpEgressError> {
	match method {
		"GET" => Ok(Method::GET),
		"POST" => Ok(Method::POST),
		"PUT" => Ok(Method::PUT),
		_ => {
			Err(HttpEgressError::InvalidArgument(format!("unsupported HTTP egress method {method:?}")))
		},
	}
}

fn parse_headers(headers: &[pb::HttpHeader]) -> Result<HeaderMap, HttpEgressError> {
	let mut parsed = HeaderMap::with_capacity(headers.len());
	for header in headers {
		let name = HeaderName::from_bytes(header.name.as_bytes()).map_err(|error| {
			HttpEgressError::InvalidArgument(format!(
				"invalid HTTP header name {:?}: {error}",
				header.name
			))
		})?;
		let value = HeaderValue::from_str(&header.value).map_err(|error| {
			HttpEgressError::InvalidArgument(format!(
				"invalid HTTP header value for {:?}: {error}",
				header.name
			))
		})?;
		parsed.append(name, value);
	}
	Ok(parsed)
}

fn response_headers(headers: &HeaderMap) -> Vec<pb::HttpHeader> {
	headers
		.iter()
		.map(|(name, value)| pb::HttpHeader {
			name:  name.as_str().to_owned(),
			value: String::from_utf8_lossy(value.as_bytes()).into_owned(),
			props: None,
		})
		.collect()
}

async fn read_bounded(response: wreq::Response) -> Result<Bytes, HttpEgressError> {
	if response
		.content_length()
		.is_some_and(|length| length > MAX_TUNNEL_BUFFER_BYTES as u64)
	{
		return Err(HttpEgressError::ResponseTooLarge);
	}
	let mut body = BytesMut::with_capacity(
		response
			.content_length()
			.and_then(|length| usize::try_from(length).ok())
			.unwrap_or_default()
			.min(MAX_TUNNEL_BUFFER_BYTES),
	);
	let mut stream = response.bytes_stream();
	while let Some(chunk) = stream.next().await {
		let chunk = chunk.map_err(HttpEgressError::transport)?;
		if body.len().saturating_add(chunk.len()) > MAX_TUNNEL_BUFFER_BYTES {
			return Err(HttpEgressError::ResponseTooLarge);
		}
		body.extend_from_slice(&chunk);
	}
	Ok(body.freeze())
}
