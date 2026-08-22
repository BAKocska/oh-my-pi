//! Active model-discovery probing over an injected HTTP boundary.

use std::{future::Future, pin::Pin, time::Duration};

use bytes::Bytes;
use omp_core::{Str, sf};
use omp_llm_catalog::{
	DiscoveredModel, ModelLimits, OperationBits, OperationKind, ProviderId, RouteId, WireModelId,
};
use tokio_util::sync::CancellationToken;

use super::endpoints::{DiscoveryEndpoint, DiscoveryEndpointKind};

/// One bounded HTTP probe request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProbeHttpRequest {
	/// HTTP method.
	pub method:   http::Method,
	/// Absolute URL.
	pub url:      Str,
	/// JSON request body for metadata probes.
	pub body:     Bytes,
	/// Endpoint-class deadline.
	pub deadline: Duration,
}

/// Cold injected HTTP future for endpoint discovery.
pub type ProbeHttpFuture =
	Pin<Box<dyn Future<Output = Result<Bytes, ProbeError>> + Send + 'static>>;

/// Injected HTTP transport used by active discovery.
pub trait DiscoveryHttpClient: Send + Sync + 'static {
	/// Executes one bounded request. Implementations must not follow
	/// cross-origin redirects with credentials.
	fn request(&self, request: ProbeHttpRequest, cancellation: CancellationToken)
	-> ProbeHttpFuture;
}

/// Active endpoint probe bound to one provider route.
#[derive(Clone, Debug)]
pub struct DiscoveryProbe {
	/// Commercial/local provider identity.
	pub provider: ProviderId,
	/// Route on which discovered wire model ids are valid.
	pub route:    RouteId,
	/// Typed endpoint.
	pub endpoint: DiscoveryEndpoint,
}

impl DiscoveryProbe {
	/// Probes the endpoint family and returns normalized, secret-free rows.
	pub async fn probe(
		&self,
		client: &dyn DiscoveryHttpClient,
		cancellation: CancellationToken,
	) -> Result<Vec<DiscoveredModel>, ProbeError> {
		let path = match self.endpoint.kind {
			DiscoveryEndpointKind::Ollama => "/api/tags",
			DiscoveryEndpointKind::LlamaCpp => "/v1/models",
			DiscoveryEndpointKind::LmStudio => "/api/v0/models",
			DiscoveryEndpointKind::LiteLlm | DiscoveryEndpointKind::OpenAi => "/v1/models",
		};
		let payload = self
			.request(client, http::Method::GET, path, Bytes::new(), cancellation.clone())
			.await?;
		let mut rows = self.decode_models(&payload)?;
		match self.endpoint.kind {
			DiscoveryEndpointKind::Ollama => {
				for row in &mut rows {
					let body = serde_json::to_vec(&serde_json::json!({"name": row.wire_model.as_str()}))
						.map(Bytes::from)
						.map_err(|_| ProbeError::Protocol)?;
					if let Ok(show) = self
						.request(client, http::Method::POST, "/api/show", body, cancellation.clone())
						.await
					{
						apply_ollama_show(row, &show)?;
					}
				}
			},
			DiscoveryEndpointKind::LlamaCpp => {
				if let Ok(props) = self
					.request(client, http::Method::GET, "/props", Bytes::new(), cancellation)
					.await
				{
					apply_llama_props(&mut rows, &props)?;
				}
			},
			DiscoveryEndpointKind::LmStudio
			| DiscoveryEndpointKind::LiteLlm
			| DiscoveryEndpointKind::OpenAi => {},
		}
		Ok(rows)
	}

	async fn request(
		&self,
		client: &dyn DiscoveryHttpClient,
		method: http::Method,
		path: &str,
		body: Bytes,
		cancellation: CancellationToken,
	) -> Result<Bytes, ProbeError> {
		let mut url = String::with_capacity(self.endpoint.base_url.len() + path.len() + 1);
		url.push_str(self.endpoint.base_url.trim_end_matches('/'));
		url.push_str(path);
		let deadline = self.endpoint.deadline();
		let request = ProbeHttpRequest { method, url: Str::new(url), body, deadline };
		let request_cancellation = cancellation.clone();
		tokio::select! {
			() = cancellation.cancelled() => Err(ProbeError::Cancelled),
			result = tokio::time::timeout(
				deadline,
				client.request(request, request_cancellation),
			) => {
				result.map_err(|_| ProbeError::Timeout)?
			},
		}
	}

	fn decode_models(&self, payload: &[u8]) -> Result<Vec<DiscoveredModel>, ProbeError> {
		let envelope: serde_json::Value =
			serde_json::from_slice(payload).map_err(|_| ProbeError::Protocol)?;
		let rows = match self.endpoint.kind {
			DiscoveryEndpointKind::Ollama => envelope.get("models"),
			DiscoveryEndpointKind::LmStudio => envelope.get("data").or_else(|| envelope.get("models")),
			DiscoveryEndpointKind::LlamaCpp
			| DiscoveryEndpointKind::LiteLlm
			| DiscoveryEndpointKind::OpenAi => envelope.get("data").or_else(|| envelope.get("models")),
		}
		.and_then(serde_json::Value::as_array)
		.ok_or(ProbeError::Protocol)?;
		let mut discovered = Vec::with_capacity(rows.len());
		for value in rows {
			let id = value
				.get("id")
				.or_else(|| value.get("name"))
				.or_else(|| value.get("model"))
				.and_then(serde_json::Value::as_str)
				.ok_or(ProbeError::Protocol)?;
			if id.trim().is_empty() {
				return Err(ProbeError::Protocol);
			}
			let context =
				positive_u64(value, &["context_length", "contextWindow", "max_context_length"]);
			let output = positive_u64(value, &["max_output_tokens", "maxTokens"]);
			let limits = (context.is_some() || output.is_some()).then_some(ModelLimits {
				context_window:        context,
				maximum_input_tokens:  None,
				maximum_output_tokens: output,
				maximum_batch:         None,
			});
			let mut operations = OperationBits::empty();
			operations.insert_kind(OperationKind::Chat);
			discovered.push(DiscoveredModel {
				provider:              self.provider.clone(),
				route:                 self.route.clone(),
				wire_model:            WireModelId::from(id),
				aliases:               Box::new([]),
				display_name:          value
					.get("display_name")
					.or_else(|| value.get("displayName"))
					.and_then(serde_json::Value::as_str)
					.map(Str::new),
				declared_class:        None,
				declared_operations:   operations,
				declared_capabilities: None,
				declared_limits:       limits,
				extended_context_mode: None,
				availability:          None,
				source:                sf!("{}:{}", self.endpoint.kind, self.endpoint.base_url),
				observed_at_ms:        None,
				updated_at_ms:         None,
				deprecated:            None,
			});
		}
		Ok(discovered)
	}
}

fn positive_u64(value: &serde_json::Value, keys: &[&str]) -> Option<u64> {
	keys
		.iter()
		.find_map(|key| value.get(*key).and_then(serde_json::Value::as_u64))
		.filter(|value| *value > 0)
}

fn apply_ollama_show(row: &mut DiscoveredModel, payload: &[u8]) -> Result<(), ProbeError> {
	let value: serde_json::Value =
		serde_json::from_slice(payload).map_err(|_| ProbeError::Protocol)?;
	let context = value
		.get("model_info")
		.and_then(serde_json::Value::as_object)
		.and_then(|info| {
			info
				.iter()
				.find(|(key, _)| key.ends_with(".context_length"))
				.and_then(|(_, value)| value.as_u64())
		})
		.or_else(|| positive_u64(&value, &["context_length"]));
	if let Some(context) = context.filter(|value| *value > 0) {
		row.declared_limits
			.get_or_insert(ModelLimits {
				context_window:        None,
				maximum_input_tokens:  None,
				maximum_output_tokens: None,
				maximum_batch:         None,
			})
			.context_window = Some(context);
	}
	Ok(())
}

fn apply_llama_props(rows: &mut [DiscoveredModel], payload: &[u8]) -> Result<(), ProbeError> {
	let value: serde_json::Value =
		serde_json::from_slice(payload).map_err(|_| ProbeError::Protocol)?;
	let context = positive_u64(&value, &["n_ctx", "n_ctx_train", "context_length"]);
	if let Some(context) = context {
		for row in rows {
			row.declared_limits
				.get_or_insert(ModelLimits {
					context_window:        None,
					maximum_input_tokens:  None,
					maximum_output_tokens: None,
					maximum_batch:         None,
				})
				.context_window = Some(context);
		}
	}
	Ok(())
}

/// Typed, redaction-safe probe failure.
#[derive(Clone, Copy, Debug, thiserror::Error, Eq, PartialEq)]
pub enum ProbeError {
	/// The endpoint missed its loopback/remote deadline.
	#[error("model discovery probe timed out")]
	Timeout,
	/// The caller cancelled discovery.
	#[error("model discovery probe was cancelled")]
	Cancelled,
	/// The endpoint transport failed.
	#[error("model discovery transport failed")]
	Transport,
	/// The endpoint response was malformed.
	#[error("model discovery response was malformed")]
	Protocol,
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;

	use super::*;
	use crate::discovery::endpoints::{EndpointOrigin, configured_endpoint};

	#[derive(Clone)]
	struct FixtureClient(Arc<Bytes>);
	impl DiscoveryHttpClient for FixtureClient {
		fn request(&self, _: ProbeHttpRequest, _: CancellationToken) -> ProbeHttpFuture {
			let payload = Arc::clone(&self.0);
			Box::pin(async move { Ok((*payload).clone()) })
		}
	}

	#[tokio::test]
	async fn generic_openai_probe_normalizes_models() {
		let endpoint =
			configured_endpoint(DiscoveryEndpointKind::OpenAi, "https://models.example/v1")
				.expect("endpoint");
		assert_eq!(endpoint.origin, EndpointOrigin::Configured);
		let probe = DiscoveryProbe {
			provider: ProviderId::from("custom"),
			route: RouteId::from("custom-route"),
			endpoint,
		};
		let rows = probe
			.probe(
				&FixtureClient(Arc::new(Bytes::from_static(
					br#"{"data":[{"id":"offline","context_length":8192}]}"#,
				))),
				CancellationToken::new(),
			)
			.await
			.expect("probe");
		assert_eq!(rows[0].wire_model.as_str(), "offline");
		assert_eq!(
			rows[0]
				.declared_limits
				.as_ref()
				.and_then(|limits| limits.context_window),
			Some(8192)
		);
	}
}
