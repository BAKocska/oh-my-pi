use bytes::{Bytes, BytesMut};
use futures::{FutureExt as _, future::BoxFuture};
use http::{
	HeaderMap, HeaderValue, Method, Request,
	header::{ACCEPT, CONTENT_TYPE},
};
use http_body_util::{BodyExt as _, Full};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
	client::legacy::{Client, connect::HttpConnector},
	rt::TokioExecutor,
};
use omp_core::{ExposeSecret as _, SecretString};
use url::Url;

const FORM_CONTENT_TYPE: &str = "application/x-www-form-urlencoded";
/// Hard ceiling for a single OAuth response body.
pub const MAX_OAUTH_RESPONSE_BYTES: usize = 1024 * 1024;

/// A secret-bearing OAuth request handed directly to an injected transport.
pub struct OAuthHttpRequest {
	method:  Method,
	url:     Url,
	headers: HeaderMap,
	body:    Option<SecretString>,
}

impl OAuthHttpRequest {
	/// Creates a bounded OAuth request.
	pub fn new(
		method: Method,
		url: &str,
		mut headers: HeaderMap,
		body: Option<SecretString>,
	) -> Result<Self, OAuthRequestError> {
		let url = Url::parse(url).map_err(|_| OAuthRequestError::InvalidUrl)?;
		if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
			return Err(OAuthRequestError::InvalidUrl);
		}
		Ok(Self { method, url, headers, body })
	}

	/// Creates a form-encoded secret POST request.
	pub fn secret_form(url: &str, body: SecretString) -> Result<Self, OAuthRequestError> {
		let mut headers = HeaderMap::new();
		headers.insert(CONTENT_TYPE, HeaderValue::from_static(FORM_CONTENT_TYPE));
		headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
		Self::new(Method::POST, url, headers, Some(body))
	}

	/// Consumes the request into transport-ready parts.
	pub fn into_parts(self) -> (Method, Url, HeaderMap, Option<SecretString>) {
		(self.method, self.url, self.headers, self.body)
	}
}

/// OAuth request construction failed before any I/O.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum OAuthRequestError {
	/// URL is not an absolute HTTP(S) URL.
	#[error("OAuth endpoint URL is invalid")]
	InvalidUrl,
}

/// Secret-bearing bounded OAuth response.
pub struct OAuthHttpResponse {
	/// HTTP status code.
	pub status:  u16,
	/// Response headers.
	pub headers: HeaderMap,
	/// Bounded response body.
	pub body:    SecretString,
}

/// Cold OAuth I/O boundary.
pub trait OAuthHttpClient: Send + Sync {
	/// Executes one request without exposing its secret body.
	fn execute(
		&self,
		request: OAuthHttpRequest,
	) -> BoxFuture<'_, Result<OAuthHttpResponse, OAuthTransportError>>;
}

/// OAuth transport failed or exceeded its bounded response ceiling.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("OAuth HTTP transport failed")]
pub struct OAuthTransportError;

type PooledOAuthClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

/// Production rustls OAuth transport with a one-MiB response ceiling.
#[derive(Clone)]
pub struct SystemOAuthHttpClient {
	inner: PooledOAuthClient,
}

impl SystemOAuthHttpClient {
	/// Constructs a pooled HTTP/1.1 and HTTP/2 client.
	pub fn new() -> Self {
		let _ = rustls::crypto::ring::default_provider().install_default();
		let connector = HttpsConnectorBuilder::new()
			.with_webpki_roots()
			.https_or_http()
			.enable_http1()
			.enable_http2()
			.build();
		Self { inner: Client::builder(TokioExecutor::new()).build(connector) }
	}
}

impl Default for SystemOAuthHttpClient {
	fn default() -> Self {
		Self::new()
	}
}

impl std::fmt::Debug for SystemOAuthHttpClient {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter.write_str("SystemOAuthHttpClient(..)")
	}
}

impl OAuthHttpClient for SystemOAuthHttpClient {
	fn execute(
		&self,
		request: OAuthHttpRequest,
	) -> BoxFuture<'_, Result<OAuthHttpResponse, OAuthTransportError>> {
		let client = self.inner.clone();
		async move {
			let (method, url, headers, body) = request.into_parts();
			let body = body.as_ref().map_or_else(Bytes::new, |body| {
				Bytes::copy_from_slice(body.expose_secret().as_bytes())
			});
			let mut outbound = Request::builder()
				.method(method)
				.uri(url.as_str())
				.body(Full::new(body))
				.map_err(|_| OAuthTransportError)?;
			*outbound.headers_mut() = headers;
			let response = client
				.request(outbound)
				.await
				.map_err(|_| OAuthTransportError)?;
			let status = response.status().as_u16();
			let headers = response.headers().clone();
			let mut incoming = response.into_body();
			let mut bytes = BytesMut::new();
			while let Some(frame) = incoming.frame().await {
				let frame = frame.map_err(|_| OAuthTransportError)?;
				if let Some(data) = frame.data_ref() {
					if bytes.len().saturating_add(data.len()) > MAX_OAUTH_RESPONSE_BYTES {
						return Err(OAuthTransportError);
					}
					bytes.extend_from_slice(data);
				}
			}
			let body = String::from_utf8(bytes.to_vec()).map_err(|_| OAuthTransportError)?;
			Ok(OAuthHttpResponse { status, headers, body: SecretString::from(body) })
		}
		.boxed()
	}
}
