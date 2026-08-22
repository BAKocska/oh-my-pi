//! Legacy MCP HTTP+SSE transport for protocol revision 2024-11-05.

use std::{
	collections::HashMap,
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
	time::Duration,
};

use bytes::Bytes;
use http::{
	HeaderMap, HeaderName, HeaderValue, Method, StatusCode,
	header::{ACCEPT, CONTENT_TYPE},
};
use omp_core::Str;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::{
	header_policy::{RedirectPolicy, redirect_location},
	http::{
		HttpExchange, HttpExchangeError, HttpRequest, HttpResponse, RefreshableHeaders,
		parse_sse_events,
	},
	json_rpc::{RequestId, RequestIdAllocator, RequestIdFormat},
	transport::{
		DispatchState, IncomingMessage, McpTransport, ServerResponseError, TransportError,
		TransportFailure, TransportFuture, TransportResponse,
	},
};

/// Legacy transport configuration.
#[derive(Clone)]
pub struct LegacySseConfig {
	/// Initial SSE discovery endpoint.
	pub url:               Url,
	/// Configured non-reserved headers.
	pub headers:           HeaderMap,
	/// Whether configured headers are origin-locked.
	pub origin_locked:     bool,
	/// Request timeout; `None` disables it.
	pub timeout:           Option<Duration>,
	/// Request-ID representation.
	pub request_id_format: RequestIdFormat,
	/// Optional refreshable auth lease.
	pub auth:              Option<Arc<dyn RefreshableHeaders>>,
}

enum PendingResult {
	Value(Value),
	RpcError(i64),
	Closed,
}

/// Legacy endpoint-event plus concurrent POST-correlation transport.
pub struct LegacySseTransport {
	config:      LegacySseConfig,
	http:        Arc<dyn HttpExchange>,
	endpoint:    Mutex<Option<Url>>,
	ids:         Mutex<RequestIdAllocator>,
	pending:     Mutex<HashMap<RequestId, oneshot::Sender<PendingResult>>>,
	incoming_tx: flume::Sender<IncomingMessage>,
	incoming_rx: flume::Receiver<IncomingMessage>,
	closed:      AtomicBool,
}

impl LegacySseTransport {
	/// Connects to the discovery SSE stream and validates its endpoint event.
	pub async fn connect(
		config: LegacySseConfig,
		http: Arc<dyn HttpExchange>,
		cancellation: CancellationToken,
	) -> Result<Self, TransportError> {
		super::header_policy::validate_configured_headers(&config.headers)
			.map_err(|source| TransportError::pre_dispatch(TransportFailure::HeaderPolicy(source)))?;
		let (incoming_tx, incoming_rx) = flume::bounded(256);
		let transport = Self {
			config,
			http,
			endpoint: Mutex::new(None),
			ids: Mutex::new(RequestIdAllocator::default()),
			pending: Mutex::new(HashMap::new()),
			incoming_tx,
			incoming_rx,
			closed: AtomicBool::new(false),
		};
		let mut generated = transport
			.config
			.auth
			.as_ref()
			.map_or_else(HeaderMap::new, |auth| auth.current());
		generated.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
		let response = transport
			.exchange(
				transport.config.url.clone(),
				Method::GET,
				Bytes::new(),
				generated,
				&cancellation,
				false,
			)
			.await?;
		if !response.status.is_success() {
			return Err(TransportError::pre_dispatch(TransportFailure::HttpStatus {
				status: response.status.as_u16(),
			}));
		}
		transport.consume_discovery(&response.body)?;
		if transport.endpoint.lock().is_none() {
			return Err(TransportError::pre_dispatch(TransportFailure::SseProtocol));
		}
		Ok(transport)
	}

	async fn exchange(
		&self,
		url: Url,
		method: Method,
		body: Bytes,
		mut generated: HeaderMap,
		cancellation: &CancellationToken,
		dispatched: bool,
	) -> Result<HttpResponse, TransportError> {
		if method == Method::POST {
			generated.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
			generated.insert(ACCEPT, HeaderValue::from_static("application/json, text/event-stream"));
		}
		let mut policy = RedirectPolicy::new(url, self.config.origin_locked)
			.map_err(|source| TransportError::pre_dispatch(TransportFailure::HeaderPolicy(source)))?;
		loop {
			let headers = policy
				.headers(&generated, &self.config.headers)
				.map_err(|source| dispatch_error(dispatched, TransportFailure::HeaderPolicy(source)))?;
			let future = self.http.execute(HttpRequest {
				method: method.clone(),
				url: policy.url().clone(),
				headers,
				body: body.clone(),
			});
			let result = if let Some(timeout) = self.config.timeout {
				tokio::select! { () = cancellation.cancelled() => return Err(dispatch_error(dispatched, TransportFailure::Cancelled)), value = tokio::time::timeout(timeout, future) => value.map_err(|_| dispatch_error(dispatched, TransportFailure::TimedOut))? }
			} else {
				tokio::select! { () = cancellation.cancelled() => return Err(dispatch_error(dispatched, TransportFailure::Cancelled)), value = future => value }
			};
			let response = result.map_err(|error| match error {
				HttpExchangeError::Http(source) => {
					dispatch_error(dispatched, TransportFailure::Http(source))
				},
				HttpExchangeError::ResponseTooLarge => {
					dispatch_error(dispatched, TransportFailure::FrameTooLarge)
				},
			})?;
			if policy
				.redirect(&method, response.status, redirect_location(&response.headers))
				.map_err(|source| dispatch_error(dispatched, TransportFailure::HeaderPolicy(source)))?
			{
				continue;
			}
			return Ok(response);
		}
	}

	async fn post(
		&self,
		body: Bytes,
		cancellation: &CancellationToken,
	) -> Result<HttpResponse, TransportError> {
		let endpoint = self
			.endpoint
			.lock()
			.clone()
			.ok_or_else(|| TransportError::pre_dispatch(TransportFailure::NotConnected))?;
		let mut generated = self
			.config
			.auth
			.as_ref()
			.map_or_else(HeaderMap::new, |auth| auth.current());
		let mut response = self
			.exchange(
				endpoint.clone(),
				Method::POST,
				body.clone(),
				generated.clone(),
				cancellation,
				true,
			)
			.await?;
		if matches!(response.status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
			&& let Some(auth) = &self.config.auth
			&& auth.refresh().await
		{
			generated = auth.current();
			response = self
				.exchange(endpoint, Method::POST, body, generated, cancellation, true)
				.await?;
		}
		Ok(response)
	}

	fn consume_discovery(&self, body: &[u8]) -> Result<(), TransportError> {
		for event in parse_sse_events(body).map_err(|cause| TransportError::pre_dispatch(cause))? {
			if event
				.event
				.as_ref()
				.is_some_and(|name| name.as_str() == "endpoint")
			{
				let endpoint = self.config.url.join(event.data.trim()).map_err(|source| {
					TransportError::pre_dispatch(TransportFailure::HeaderPolicy(
						super::header_policy::HeaderPolicyError::Url(source),
					))
				})?;
				if origin(&endpoint) != origin(&self.config.url) {
					return Err(TransportError::pre_dispatch(TransportFailure::SseProtocol));
				}
				*self.endpoint.lock() = Some(endpoint);
			} else if !event.data.is_empty() && event.data != "[DONE]" {
				self.consume_message_data(&event.data)?;
			}
		}
		Ok(())
	}

	fn consume_message_data(&self, data: &str) -> Result<(), TransportError> {
		let payload: Value = serde_json::from_str(data)
			.map_err(|source| TransportError::effects_unknown(TransportFailure::Json(source)))?;
		for message in payload
			.as_array()
			.map_or_else(|| vec![payload.clone()], Clone::clone)
		{
			self.dispatch(&message);
		}
		Ok(())
	}

	fn consume_post_body(&self, response: &HttpResponse) -> Result<(), TransportError> {
		if response.body.is_empty() {
			return Ok(());
		}
		if response
			.headers
			.get(CONTENT_TYPE)
			.and_then(|value| value.to_str().ok())
			.is_some_and(|value| value.contains("text/event-stream"))
		{
			for event in parse_sse_events(&response.body)
				.map_err(|cause| TransportError::effects_unknown(cause))?
			{
				if !event.data.is_empty() {
					self.consume_message_data(&event.data)?;
				}
			}
		} else {
			let value: Value = serde_json::from_slice(&response.body)
				.map_err(|source| TransportError::effects_unknown(TransportFailure::Json(source)))?;
			self.dispatch(&value);
		}
		Ok(())
	}

	fn dispatch(&self, message: &Value) {
		if let Some(id) = message
			.get("id")
			.and_then(|value| serde_json::from_value::<RequestId>(value.clone()).ok())
			&& (message.get("result").is_some() || message.get("error").is_some())
			&& let Some(pending) = self.pending.lock().remove(&id)
		{
			let result = message
				.get("error")
				.and_then(|error| error.get("code"))
				.and_then(Value::as_i64)
				.map_or_else(
					|| PendingResult::Value(message.get("result").cloned().unwrap_or(Value::Null)),
					PendingResult::RpcError,
				);
			let _ = pending.send(result);
			return;
		}
		let Some(method) = message.get("method").and_then(Value::as_str) else {
			return;
		};
		let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
		let incoming = match message
			.get("id")
			.and_then(|value| serde_json::from_value::<RequestId>(value.clone()).ok())
		{
			Some(id) => IncomingMessage::Request { id, method: Str::from(method), params },
			None => IncomingMessage::Notification { method: Str::from(method), params },
		};
		let _ = self.incoming_tx.try_send(incoming);
	}

	async fn request_inner(
		&self,
		method: &str,
		params: Value,
		cancellation: CancellationToken,
	) -> Result<TransportResponse, TransportError> {
		if self.closed.load(Ordering::Acquire) {
			return Err(TransportError::pre_dispatch(TransportFailure::Closed));
		}
		let id = self
			.ids
			.lock()
			.next(self.config.request_id_format)
			.map_err(|_| TransportError::pre_dispatch(TransportFailure::Correlation))?;
		let (sender, receiver) = oneshot::channel();
		self.pending.lock().insert(id.clone(), sender);
		let body =
			serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
				.map(Bytes::from)
				.map_err(|source| TransportError::pre_dispatch(TransportFailure::Json(source)))?;
		let response = match self.post(body, &cancellation).await {
			Ok(response) => response,
			Err(error) => {
				self.pending.lock().remove(&id);
				return Err(error);
			},
		};
		if !response.status.is_success() {
			self.pending.lock().remove(&id);
			return Err(TransportError {
				dispatch: DispatchState::Responded,
				cause:    TransportFailure::HttpStatus { status: response.status.as_u16() },
			});
		}
		self.consume_post_body(&response)?;
		let receive = async { receiver.await.unwrap_or(PendingResult::Closed) };
		let pending = if let Some(timeout) = self.config.timeout {
			tokio::select! { () = cancellation.cancelled() => { self.pending.lock().remove(&id); return Err(TransportError::effects_unknown(TransportFailure::Cancelled)); }, value = tokio::time::timeout(timeout, receive) => match value { Ok(value) => value, Err(_) => { self.pending.lock().remove(&id); return Err(TransportError::effects_unknown(TransportFailure::TimedOut)); } } }
		} else {
			tokio::select! { () = cancellation.cancelled() => { self.pending.lock().remove(&id); return Err(TransportError::effects_unknown(TransportFailure::Cancelled)); }, value = receive => value }
		};
		match pending {
			PendingResult::Value(result) => {
				Ok(TransportResponse { id, result, dispatch: DispatchState::Responded })
			},
			PendingResult::RpcError(code) => Err(TransportError {
				dispatch: DispatchState::Responded,
				cause:    TransportFailure::JsonRpc { code },
			}),
			PendingResult::Closed => Err(TransportError::effects_unknown(TransportFailure::Closed)),
		}
	}
}

impl McpTransport for LegacySseTransport {
	fn request<'a>(
		&'a self,
		method: &'a str,
		params: Value,
		cancellation: CancellationToken,
	) -> TransportFuture<'a, Result<TransportResponse, TransportError>> {
		Box::pin(self.request_inner(method, params, cancellation))
	}

	fn notify<'a>(
		&'a self,
		method: &'a str,
		params: Value,
		cancellation: CancellationToken,
	) -> TransportFuture<'a, Result<DispatchState, TransportError>> {
		Box::pin(async move {
			let body = serde_json::to_vec(&json!({"jsonrpc":"2.0","method":method,"params":params}))
				.map(Bytes::from)
				.map_err(|source| TransportError::pre_dispatch(TransportFailure::Json(source)))?;
			let response = self.post(body, &cancellation).await?;
			if response.status.is_success() || response.status == StatusCode::ACCEPTED {
				self.consume_post_body(&response)?;
				Ok(DispatchState::Dispatched)
			} else {
				Err(TransportError {
					dispatch: DispatchState::Responded,
					cause:    TransportFailure::HttpStatus { status: response.status.as_u16() },
				})
			}
		})
	}

	fn next_message<'a>(
		&'a self,
		cancellation: CancellationToken,
	) -> TransportFuture<'a, Result<IncomingMessage, TransportError>> {
		Box::pin(async move {
			tokio::select! { () = cancellation.cancelled() => Err(TransportError::pre_dispatch(TransportFailure::Cancelled)), value = self.incoming_rx.recv_async() => value.map_err(|_| TransportError::pre_dispatch(TransportFailure::Closed)) }
		})
	}

	fn respond<'a>(
		&'a self,
		id: RequestId,
		result: Result<Value, ServerResponseError>,
		cancellation: CancellationToken,
	) -> TransportFuture<'a, Result<DispatchState, TransportError>> {
		Box::pin(async move {
			let value = match result {
				Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
				Err(error) => {
					json!({"jsonrpc":"2.0","id":id,"error":{"code":error.code,"message":error.message,"data":error.data}})
				},
			};
			let response = self
				.post(
					serde_json::to_vec(&value)
						.map(Bytes::from)
						.map_err(|source| TransportError::pre_dispatch(TransportFailure::Json(source)))?,
					&cancellation,
				)
				.await?;
			if response.status.is_success() {
				Ok(DispatchState::Dispatched)
			} else {
				Err(TransportError {
					dispatch: DispatchState::Responded,
					cause:    TransportFailure::HttpStatus { status: response.status.as_u16() },
				})
			}
		})
	}

	fn close(&self) -> TransportFuture<'_, Result<(), TransportError>> {
		Box::pin(async move {
			self.closed.store(true, Ordering::Release);
			*self.endpoint.lock() = None;
			for (_, sender) in self.pending.lock().drain() {
				let _ = sender.send(PendingResult::Closed);
			}
			let _ = self.incoming_tx.try_send(IncomingMessage::Closed);
			Ok(())
		})
	}
}

fn origin(url: &Url) -> (&str, Option<&str>, Option<u16>) {
	(url.scheme(), url.host_str(), url.port_or_known_default())
}
fn dispatch_error(dispatched: bool, cause: TransportFailure) -> TransportError {
	if dispatched {
		TransportError::effects_unknown(cause)
	} else {
		TransportError::pre_dispatch(cause)
	}
}

#[cfg(test)]
mod tests {
	use std::sync::atomic::AtomicUsize;

	use super::*;
	use crate::mcp::http::HttpFuture;

	struct Fixture {
		calls: AtomicUsize,
	}
	impl HttpExchange for Fixture {
		fn execute(&self, request: HttpRequest) -> HttpFuture<'_> {
			self.calls.fetch_add(1, Ordering::SeqCst);
			Box::pin(async move {
				if request.method == Method::GET {
					return Ok(HttpResponse {
						status:  StatusCode::OK,
						headers: HeaderMap::from_iter([(
							CONTENT_TYPE,
							HeaderValue::from_static("text/event-stream"),
						)]),
						body:    Bytes::from_static(b"event: endpoint\ndata: /messages\n\n"),
					});
				}
				let request: Value = serde_json::from_slice(&request.body).expect("request");
				let id = request.get("id").cloned().unwrap_or(Value::Null);
				let body = format!(
					"event: message\ndata: \
					 {{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"id\":{id}}}}}\n\n"
				);
				Ok(HttpResponse {
					status:  StatusCode::ACCEPTED,
					headers: HeaderMap::from_iter([(
						CONTENT_TYPE,
						HeaderValue::from_static("text/event-stream"),
					)]),
					body:    Bytes::from(body),
				})
			})
		}
	}

	#[tokio::test]
	async fn endpoint_discovery_and_concurrent_ids_are_correlated() {
		let fixture = Arc::new(Fixture { calls: AtomicUsize::new(0) });
		let http: Arc<dyn HttpExchange> = fixture;
		let transport = LegacySseTransport::connect(
			LegacySseConfig {
				url:               Url::parse("https://legacy.test/events").expect("url"),
				headers:           HeaderMap::new(),
				origin_locked:     true,
				timeout:           Some(Duration::from_secs(1)),
				request_id_format: RequestIdFormat::Number,
				auth:              None,
			},
			http,
			CancellationToken::new(),
		)
		.await
		.expect("connect");
		let (left, right) = tokio::join!(
			transport.request("left", json!({}), CancellationToken::new()),
			transport.request("right", json!({}), CancellationToken::new())
		);
		assert_eq!(left.expect("left").result["id"], 1);
		assert_eq!(right.expect("right").result["id"], 2);
	}

	#[tokio::test]
	async fn cross_origin_endpoint_is_rejected() {
		struct Bad;
		impl HttpExchange for Bad {
			fn execute(&self, _: HttpRequest) -> HttpFuture<'_> {
				Box::pin(async {
					Ok(HttpResponse {
						status:  StatusCode::OK,
						headers: HeaderMap::new(),
						body:    Bytes::from_static(
							b"event: endpoint\ndata: https://evil.test/messages\n\n",
						),
					})
				})
			}
		}
		let error = LegacySseTransport::connect(
			LegacySseConfig {
				url:               Url::parse("https://legacy.test/events").expect("url"),
				headers:           HeaderMap::new(),
				origin_locked:     true,
				timeout:           None,
				request_id_format: RequestIdFormat::Number,
				auth:              None,
			},
			Arc::new(Bad),
			CancellationToken::new(),
		)
		.await
		.err()
		.expect("reject");
		assert!(matches!(error.cause, TransportFailure::SseProtocol));
	}
}
