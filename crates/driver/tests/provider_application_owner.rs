use omp_core::sf;
use omp_driver::model_controls::{
	ProviderControlError, ProviderDeclarationDocument, lower_provider_declaration,
};
use serde_json::{Value, json};

fn declaration(spec: Value) -> ProviderDeclarationDocument {
	ProviderDeclarationDocument { provider: sf!("acme"), document: spec }
}

fn provider_spec() -> Value {
	json!({
		"id": "acme",
		"name": "Acme",
		"routes": [{
			"id": "primary",
			"base_url": "https://api.acme.test/v1",
			"api": "openai_chat",
			"transport": "http",
			"auth": {
				"mode": "none",
				"header": null,
				"prefix": null,
				"query": null,
				"scopes": [],
				"audience": null,
				"account_scope": "provider",
				"sources": [],
				"oauth": null,
				"signing": null
			},
			"headers": {},
			"region": null,
			"discovery": null,
			"trust": {
				"origin": "https://api.acme.test",
				"redirects": "same_origin",
				"allow_plaintext": false
			},
			"limits": {
				"operations": null,
				"max_context_tokens": null,
				"max_output_tokens": null,
				"disable_server_state": false,
				"disable_prompt_caching": false
			},
			"compat": {"schema_flavor": null, "watchdog": null},
			"codec_profile": "standard",
			"priority": null
		}],
		"models": [{
			"id": "chat-one",
			"display_name": "Chat One",
			"routes": ["primary"],
			"wire_ids": {},
			"operations": ["chat"],
			"family": "chat-one",
			"context_window": 8192,
			"max_input_tokens": null,
			"max_output_tokens": 1024,
			"max_batch": null,
			"input_modalities": ["text"],
			"thinking": null,
			"thinking_routing": null,
			"cost": {
				"input": "0.25",
				"output": "1.50",
				"cache_read": 0,
				"cache_write": 0,
				"image": 0,
				"video_second": 0,
				"audio_second": 0,
				"char_input": 0,
				"request": 0,
				"tiers": []
			},
			"premium_multiplier": null,
			"compat": {"schema_flavor": null, "watchdog": null},
			"context": {"mode": "replay", "retention": [], "min_prefix_tokens": null, "max_breakpoints": null},
			"availability": null,
			"context_promotion_target": null,
			"remote_compaction": null,
			"chat": {},
			"embeddings": null,
			"image": null,
			"video": null,
			"speech": null,
			"transcription": null,
			"realtime": null,
			"search": null,
			"tokenization": null
		}],
		"management": {
			"operations": [],
			"multiple_accounts": false,
			"refresh": false,
			"principal_quota": false
		},
		"discovery_defaults": null,
		"mapping": "concrete",
		"aliases": [],
		"model_overlays": []
	})
}

#[test]
fn sealed_python_provider_lowers_into_resolvable_runtime_records() {
	let base = omp_catalog::snapshot::Catalog::embedded();
	let records = lower_provider_declaration(base, &declaration(provider_spec()))
		.expect("lower provider declaration");
	assert_eq!(records.provider.id.as_str(), "acme");
	assert_eq!(records.routes[0].id.as_str(), "acme/primary");
	assert_eq!(records.models[0].key.as_str(), "acme/chat-one");
	assert_eq!(records.models[0].wire_ids[0].1.as_str(), "chat-one");
	assert_eq!(records.models[0].pricing.components[0].nanos_usd, 250_000_000);

	let rebuilt = base
		.with_runtime_provider(&records)
		.expect("validated catalog swap");
	let provider = omp_catalog::ProviderId::from("acme");
	let model = omp_catalog::ModelKey::from("acme/chat-one");
	assert!(rebuilt.provider(&provider).is_some());
	assert!(rebuilt.model_for_provider(&provider, &model).is_some());
}

#[test]
fn lowering_rejects_trust_widening_without_mutating_the_catalog() {
	let base = omp_catalog::snapshot::Catalog::embedded();
	let revision = base.revision().clone();
	let mut spec = provider_spec();
	spec["routes"][0]["base_url"] = json!("http://api.acme.test/v1");
	spec["routes"][0]["trust"] = json!({
		"origin": "http://api.acme.test",
		"redirects": "same_origin",
		"allow_plaintext": true
	});
	let error = lower_provider_declaration(base, &declaration(spec))
		.expect_err("non-loopback plaintext must be refused");
	assert!(matches!(error, ProviderControlError::InvalidDeclaration(_)));
	assert_eq!(base.revision(), &revision);
}

#[test]
fn lowering_refuses_codec_capability_widening() {
	let base = omp_catalog::snapshot::Catalog::embedded();
	let mut spec = provider_spec();
	spec["routes"][0]["api"] = json!("openai_responses");
	spec["models"][0]["operations"] = json!(["generate_image"]);
	spec["models"][0]["image"] = json!({
		"features": ["generate"],
		"sizes": [{"width": 1024, "height": 1024}],
		"formats": ["png"],
		"max_references": null
	});
	let error = lower_provider_declaration(base, &declaration(spec))
		.expect_err("codec cannot advertise an unsupported media operation");
	assert!(matches!(error, ProviderControlError::CapabilityDenied));
}
