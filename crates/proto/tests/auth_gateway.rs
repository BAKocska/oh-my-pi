//! Gateway forwarding and credential-health wire contract tests.

use bytes::Bytes;
use omp_proto::{
	auth::v1::{CredentialHealth, ProbeCredentialsResponse, credential_health},
	gateway::v1::ForwardRequest,
	inference::v1::{
		NativeRequest,
		native_request::{self, Path},
	},
};
use prost::Message as _;

#[test]
fn forward_request_has_no_client_credential_surface() {
	let request = ForwardRequest {
		request: Some(NativeRequest {
			model:              "openai/gpt-5".to_owned(),
			method:             native_request::Method::Post as i32,
			path:               Path::Responses as i32,
			payload:            Some(native_request::Payload::Json(Bytes::from_static(b"{}"))),
			framing:            native_request::Framing::Json as i32,
			max_response_bytes: 1024,
		}),
	};
	let value = serde_json::to_value(request).expect("serialize forward request");
	let text = value.to_string();
	assert!(!text.contains("authorization"));
	assert!(!text.contains("api_key"));
	assert!(!text.contains("cookie"));
}

#[test]
fn credential_health_round_trips_typed_status_latency_and_error_class() {
	let response = ProbeCredentialsResponse {
		credentials: vec![CredentialHealth {
			credential_id: 42,
			provider:      "openai".to_owned(),
			healthy:       false,
			status_code:   Some(401),
			latency_ms:    73,
			error_class:   credential_health::ErrorClass::Authentication as i32,
		}],
	};
	let decoded = ProbeCredentialsResponse::decode(response.encode_to_vec().as_slice())
		.expect("decode credential health");
	assert_eq!(decoded, response);
}
