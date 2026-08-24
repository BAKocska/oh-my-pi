//! Proves provider service CONTROL uses manifest routes, host generations, and
//! sealed codecs.
use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use omp_core::{Principal, Str, sf};
use omp_envd::{
	exthost::{
		control::{ControlAuthorityFactory as _, ControlConnectionIdentity, ControlRequestContext},
		services::{
			ServiceBroker, ServiceControlAuthorityFactory, ServiceDispatch, ServiceDispatchBackend,
			ServiceKey, ServiceManifest, ServiceMethodSchema, ServiceProviderDeclaration,
			ServiceResponse,
		},
	},
	worker::HostKey,
};
use parking_lot::Mutex;
use serde_json::json;

struct EchoBackend;

#[async_trait]
impl ServiceDispatchBackend for EchoBackend {
	async fn activate(&self, _provider: &HostKey, _service: &ServiceKey) -> Result<(), Str> {
		Err(sf!("unexpected lazy activation"))
	}

	async fn dispatch(&self, dispatch: ServiceDispatch) -> Result<ServiceResponse, Str> {
		Ok(ServiceResponse::Success(dispatch.payload.into_owned()))
	}
}

fn identity() -> Arc<ControlConnectionIdentity> {
	Arc::new(ControlConnectionIdentity {
		extension:          sf!("consumer.extension"),
		principal:          Principal::new(sf!("test"), sf!("Test")),
		artifact_digest:    sf!("sha256:consumer"),
		layer:              sf!("project"),
		tier:               sf!("trusted"),
		trust:              sf!("trusted"),
		host_generation:    7,
		session_generation: 11,
		capabilities:       Arc::new(BTreeSet::new()),
	})
}

#[tokio::test]
async fn service_control_uses_manifest_route_generation_and_sealed_codecs() {
	let caller = HostKey::new("project", "trusted", "consumer.extension");
	let provider = HostKey::new("project", "trusted", "provider.extension");
	let service = ServiceKey::new("dev.example.echo", 1);
	let mut broker = ServiceBroker::new(11);
	broker
		.publish_manifest(caller.clone(), ServiceManifest::new([], [service.clone()]))
		.expect("consumer manifest");
	broker
		.publish_manifest(provider.clone(), ServiceManifest::new([service.clone()], []))
		.expect("provider manifest");
	broker
		.activate_provider(&caller, 7, [])
		.expect("consumer generation");
	broker
		.activate_provider_declarations(&provider, 13, [ServiceProviderDeclaration {
			service: service.clone(),
			methods: Arc::from([ServiceMethodSchema {
				name:          sf!("echo"),
				input_schema:  json!({"type": "object"}),
				result_schema: json!({"type": "object"}),
			}]),
		}])
		.expect("sealed provider declaration");
	let factory =
		ServiceControlAuthorityFactory::new(Arc::new(Mutex::new(broker)), Arc::new(EchoBackend));
	let identity = identity();
	let authority = factory
		.bind(Arc::clone(&identity))
		.expect("bound authority");
	let context = |request_id| ControlRequestContext {
		connection: Arc::clone(&identity),
		request_id,
		invocation: None,
	};
	let connect = serde_json::from_value(json!({
		"name": "dev.example.echo",
		"rev": 1,
	}))
	.expect("connect arguments");
	authority
		.authorize(&context(1), "omp.services.connect", &connect)
		.expect("connect authorized");
	let connected = authority
		.request(context(1), sf!("omp.services.connect"), connect)
		.await
		.expect("connected");
	assert_eq!(connected["methods"][0]["name"], "echo");

	let call = serde_json::from_value(json!({
		"name": "dev.example.echo",
		"rev": 1,
		"method": "echo",
		"args": ["hello"],
		"kwargs": {"punctuation": "!"},
	}))
	.expect("call arguments");
	let result = authority
		.request(context(2), sf!("omp.services.call"), call)
		.await
		.expect("service result");
	assert_eq!(result, json!({"args": ["hello"], "kwargs": {"punctuation": "!"}}));
}
