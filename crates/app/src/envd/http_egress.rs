//! Capability-gated, bounded HTTP egress owned by the Environment.

use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures::StreamExt as _;
use http::{
	HeaderMap, HeaderName, HeaderValue, Method, StatusCode,
	header::{
		AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, COOKIE, HOST, LOCATION, PROXY_AUTHORIZATION,
	},
};
use omp_proto::env::v1 as pb;
use thiserror::Error;

use super::worker_pool::MAX_TUNNEL_BUFFER_BYTES;

// Reuse the existing retained wire-buffer ceiling rather than defining a
// second Environment response-size policy.
const MAX_REDIRECTS: u32 = 10;

#[derive(Clone)]
pub(crate) struct HttpEgressHost {
	client: wreq::Client,
}

impl HttpEgressHost {
	pub(crate) fn new() -> Self {
		let client = wreq::Client::builder()
			.redirect(wreq::redirect::Policy::none())
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
		if request.redirects > MAX_REDIRECTS {
			return Err(HttpEgressError::InvalidArgument(format!(
				"HTTP egress redirects must be between 0 and {MAX_REDIRECTS}"
			)));
		}
		let mut method = parse_method(&request.method)?;
		let mut url = parse_url(&request.url)?;
		let mut headers = parse_headers(&request.headers)?;
		let mut body = request.body;
		let mut followed = 0;

		loop {
			let response = self
				.client
				.request(method.clone(), url.as_str())
				.headers(headers.clone())
				.body(body.clone())
				.send()
				.await
				.map_err(HttpEgressError::transport)?;
			let status_code = response.status();
			if followed < request.redirects {
				if let Some(location) = redirect_location(status_code, response.headers()) {
					let next_url = parse_redirect_url(&url, &location)?;
					if !same_origin(&url, &next_url) {
						headers.remove(AUTHORIZATION);
						headers.remove(COOKIE);
						headers.remove(HOST);
						headers.remove(PROXY_AUTHORIZATION);
					}
					if redirects_to_get(status_code, &method) {
						method = Method::GET;
						body = Bytes::new();
						headers.remove(CONTENT_LENGTH);
						headers.remove(CONTENT_TYPE);
					}
					url = next_url;
					followed += 1;
					continue;
				}
			}

			let status = u32::from(status_code.as_u16());
			let response_headers = response_headers(response.headers());
			let body = read_bounded(response).await?;
			return Ok(pb::HttpResponse {
				status,
				headers: response_headers,
				body,
				final_url: if followed == 0 {
					request.url
				} else {
					url.as_str().to_owned()
				},
				props: None,
			});
		}
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

fn parse_url(value: &str) -> Result<url::Url, HttpEgressError> {
	let url = url::Url::parse(value)
		.map_err(|error| HttpEgressError::InvalidArgument(error.to_string()))?;
	if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
		return Err(HttpEgressError::InvalidArgument(
			"HTTP egress URL must use http or https and include a host".to_owned(),
		));
	}
	Ok(url)
}

fn redirect_location(status: StatusCode, headers: &HeaderMap) -> Option<String> {
	if !matches!(
		status,
		StatusCode::MOVED_PERMANENTLY
			| StatusCode::FOUND
			| StatusCode::SEE_OTHER
			| StatusCode::TEMPORARY_REDIRECT
			| StatusCode::PERMANENT_REDIRECT
	) {
		return None;
	}
	headers
		.get(LOCATION)
		.and_then(|location| location.to_str().ok())
		.map(str::to_owned)
}

fn parse_redirect_url(base: &url::Url, location: &str) -> Result<url::Url, HttpEgressError> {
	let url = base.join(location).map_err(|error| {
		HttpEgressError::InvalidArgument(format!("invalid redirect URL: {error}"))
	})?;
	if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
		return Err(HttpEgressError::InvalidArgument(
			"HTTP redirect URL must use http or https and include a host".to_owned(),
		));
	}
	Ok(url)
}

fn same_origin(left: &url::Url, right: &url::Url) -> bool {
	left.scheme() == right.scheme()
		&& left.host_str() == right.host_str()
		&& left.port_or_known_default() == right.port_or_known_default()
}

fn redirects_to_get(status: StatusCode, method: &Method) -> bool {
	(status == StatusCode::SEE_OTHER && *method != Method::GET && *method != Method::HEAD)
		|| (matches!(status, StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND)
			&& *method == Method::POST)
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
