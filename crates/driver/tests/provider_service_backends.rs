use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use omp_core::{Principal, Str, sf};
use omp_driver::{
	chat::{CampaignControlResolver as _, ChatProviderControlBackend, ProviderApplicationOwner},
	discovery::runtime::{SealedCampaignControlResolver, SealedCampaignDeclaration},
	model_controls::{
		ProviderControlBackend as _, ProviderControlError, ProviderControlRequest,
		ProviderControlResult, ProviderDeclarationDocument,
	},
};
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
use omp_inference::Registry;
use parking_lot::Mutex;
use serde_json::json;

struct RegistryOwner {
	registry: Registry,
}

#[async_trait]
impl ProviderApplicationOwner for RegistryOwner {
	fn registry(&self) -> Registry {
		self.registry.clone()
	}

	async fn replace_provider(
		&self,
		_identity: &ControlConnectionIdentity,
		_declaration: ProviderDeclarationDocument,
	) -> Result<(), ProviderControlError> {
		Err(ProviderControlError::Authorization)
	}

	async fn retract_provider(
		&self,
		_identity: &ControlConnectionIdentity,
		_provider: &str,
	) -> Result<(), ProviderControlError> {
		Err(ProviderControlError::Authorization)
	}

	async fn provider_request(
		&self,
		_identity: &ControlConnectionIdentity,
		_request: ProviderControlRequest,
	) -> Result<ProviderControlResult, ProviderControlError> {
		Err(ProviderControlError::Authorization)
	}
}

#[tokio::test]
async fn provider_catalog_backend_projects_the_live_registry_generation() {
	let catalog = Arc::new(omp_catalog::snapshot::Catalog::embedded().clone());
	let registry = Registry::builder(catalog).build().expect("registry");
	let generation = registry.generation();
	let backend = ChatProviderControlBackend::new(Arc::new(RegistryOwner { registry }));
	let cards = backend.models(None).await.expect("model cards");
	assert!(!cards.is_empty());
	let events = backend.watch_models(None).await.expect("catalog snapshot");
	assert!(events.len() > 1);
	let cursor_generation = match &events[0] {
		omp_driver::model_controls::ProviderModelEvent::Reset { cursor } => cursor.generation,
		_ => panic!("catalog snapshot begins with a reset fence"),
	};
	assert_eq!(cursor_generation, generation);
}

#[test]
fn campaign_resolver_uses_only_the_sealed_owner_generation() {
	use omp_agent::CampaignMachine as _;

	let mut spec = omp_agent::plan_regime_spec();
	spec.id = sf!("dev.example.campaign");
	let declaration = SealedCampaignDeclaration::new(
		"consumer.extension",
		7,
		11,
		Arc::new(spec),
		|state| {
			let mut machine = omp_agent::RegimeMachine;
			if let Some(state) = state {
				machine.restore(state).map_err(|_| {
					omp_envd::exthost::control::ControlProtocolError::new(
						"InvalidCampaignState",
						"campaign state does not match its sealed codec",
					)
				})?;
			}
			Ok(Box::new(machine) as Box<dyn omp_agent::CampaignMachine>)
		},
	);
	let resolver = SealedCampaignControlResolver::new([declaration]).expect("sealed resolver");
	let identity = identity();
	let (resolved, machine) = resolver
		.resolve(&identity, "dev.example.campaign", Some("{}"))
		.expect("owned generation resolves");
	assert_eq!(resolved.id, "dev.example.campaign");
	assert_eq!(machine.state(), "{}");

	let mut stale = (*identity).clone();
	stale.host_generation += 1;
	let error = resolver
		.resolve(&stale, "dev.example.campaign", None)
		.err()
		.expect("old declaration cannot bind a replacement generation");
	assert_eq!(error.code, "StaleGeneration");
}

struct EchoBackend;

#[async_trait]
impl ServiceDispatchBackend for EchoBackend {
	async fn activate(&self, _provider: &HostKey, _service: &ServiceKey) -> Result<(), Str> {
		Err(sf!("unexpected activation"))
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
async fn service_backend_enforces_sealed_input_and_result_schemas() {
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
	broker.activate_provider(&caller, 7, []).expect("consumer generation");
	broker
		.activate_provider_declarations(&provider, 13, [ServiceProviderDeclaration {
			service: service.clone(),
			methods: Arc::from([ServiceMethodSchema {
				name:          sf!("echo"),
				input_schema:  json!({
					"type": "object",
					"properties": {"message": {"type": "string"}},
					"required": ["message"],
					"additionalProperties": false
				}),
				result_schema: json!({
					"type": "object",
					"properties": {
						"args": {"type": "array"},
						"kwargs": {"type": "object"}
					},
					"required": ["args", "kwargs"],
					"additionalProperties": false
				}),
			}]),
		}])
		.expect("provider declarations");
	let factory =
		ServiceControlAuthorityFactory::new(Arc::new(Mutex::new(broker)), Arc::new(EchoBackend));
	let identity = identity();
	let authority = factory.bind(Arc::clone(&identity)).expect("authority");
	let context = |request_id| ControlRequestContext {
		connection: Arc::clone(&identity),
		request_id,
		invocation: None,
	};
	let valid = serde_json::from_value(json!({
		"name": "dev.example.echo",
		"rev": 1,
		"method": "echo",
		"args": ["hello"],
		"kwargs": {}
	}))
	.expect("valid call");
	let result = authority
		.request(context(1), sf!("omp.services.call"), valid)
		.await
		.expect("schema-valid call");
	assert_eq!(result["args"], json!(["hello"]));

	let invalid = serde_json::from_value(json!({
		"name": "dev.example.echo",
		"rev": 1,
		"method": "echo",
		"args": [42],
		"kwargs": {}
	}))
	.expect("invalid call");
	let error = authority
		.request(context(2), sf!("omp.services.call"), invalid)
		.await
		.expect_err("sealed input schema rejects number");
	assert_eq!(error.code, "InvalidServiceArguments");
}
