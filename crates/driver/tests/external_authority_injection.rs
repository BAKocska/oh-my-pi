use std::{collections::BTreeSet, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use omp_core::{ArtifactDigest, Principal, Provenance, Str, sf};
use omp_envd::{
	ProjectEnvironment, RegistryBridges,
	exthost::{
		ControlAuthority, ControlAuthorityFactory, ControlCompositionError,
		ExternalDomainControlFactories, ExtensionManifest,
		control::{
			ControlConnectionIdentity, ControlEffect, ControlProtocolError, ControlRequestContext,
		},
	},
	worker::{ExtHostSpec, HostKey},
};
use serde_json::Value;

struct TaggedFactory {
	operation: &'static str,
	tag:       &'static str,
}

impl ControlAuthorityFactory for TaggedFactory {
	fn bind(
		&self,
		identity: Arc<ControlConnectionIdentity>,
	) -> Result<Arc<dyn ControlAuthority>, ControlCompositionError> {
		Ok(Arc::new(TaggedAuthority {
			identity,
			operation: self.operation,
			tag: self.tag,
		}))
	}
}

struct TaggedAuthority {
	identity:  Arc<ControlConnectionIdentity>,
	operation: &'static str,
	tag:       &'static str,
}

#[async_trait]
impl ControlAuthority for TaggedAuthority {
	fn handles(&self, operation: &str) -> bool {
		operation == self.operation
	}

	fn authorize(
		&self,
		context: &ControlRequestContext,
		operation: &str,
		_arguments: &serde_json::Map<String, Value>,
	) -> Result<(), ControlProtocolError> {
		if Arc::ptr_eq(&self.identity, &context.connection) && self.handles(operation) {
			Ok(())
		} else {
			Err(ControlProtocolError::new(
				"StaleGeneration",
				"request did not reach its exact session owner",
			))
		}
	}

	async fn request(
		&self,
		context: ControlRequestContext,
		operation: Str,
		arguments: serde_json::Map<String, Value>,
	) -> Result<Value, ControlProtocolError> {
		self.authorize(&context, operation.as_str(), &arguments)?;
		Ok(Value::String(self.tag.to_owned()))
	}

	async fn effect(
		&self,
		_context: ControlRequestContext,
		_effect: ControlEffect,
	) -> Result<(), ControlProtocolError> {
		Err(ControlProtocolError::new("UnsupportedEffect", "test owner accepts requests only"))
	}
}

fn factory(operation: &'static str, tag: &'static str) -> Arc<dyn ControlAuthorityFactory> {
	Arc::new(TaggedFactory { operation, tag })
}

fn factories(generation: &'static str) -> ExternalDomainControlFactories {
	ExternalDomainControlFactories {
		policy: Some(factory("omp.policy.capabilities", generation)),
		parameters: Some(factory("omp.params.args", generation)),
		workers: Some(factory("omp.workers.list", generation)),
		direct_filesystem: Some(factory("omp.direct_filesystem.request", generation)),
		credentials: Some(factory("omp.creds.list", generation)),
		prompts: Some(factory("omp.prompts.invalidate", generation)),
		ui: Some(factory("omp.ui.presentation", generation)),
		telemetry: Some(factory("omp.telemetry.query", generation)),
		verdicts: Some(factory("omp.jobs.register", generation)),
		provider: Some(factory("omp.provider.models", generation)),
		campaigns: Some(factory("omp.campaigns.active", generation)),
		// Envd replaces this sentinel with its own sole live service broker/router.
		services: Some(factory("omp.services.connect", "must-be-overridden-by-envd")),
	}
}

fn identity() -> Arc<ControlConnectionIdentity> {
	Arc::new(ControlConnectionIdentity {
		extension:          sf!("fixture.extension"),
		principal:          Principal::new(sf!("fixture"), sf!("Fixture")),
		artifact_digest:    sf!("sha256:fixture"),
		layer:              sf!("workspace"),
		tier:               sf!("trusted"),
		trust:              sf!("trusted"),
		host_generation:    7,
		session_generation: 11,
		capabilities:       Arc::new(BTreeSet::new()),
	})
}

fn extension() -> ExtHostSpec {
	let provenance = Provenance::new(
		sf!("publisher-key"),
		sf!("fixture.extension"),
		sf!("1.0.0"),
		ArtifactDigest::new([7; 32]),
		sf!("workspace"),
		sf!("trusted"),
		1,
	);
	ExtHostSpec::new(
		HostKey::new("workspace", "trusted", "fixture.extension"),
		ExtensionManifest::py_eval(provenance, []),
	)
}

#[cfg(unix)]
#[tokio::test]
async fn every_external_domain_uses_one_atomic_session_lease() {
	let scratch = tempfile::tempdir().expect("scratch");
	let root = scratch.path().join("project");
	let state = scratch.path().join("state");
	std::fs::create_dir_all(&root).expect("project root");
	std::fs::create_dir_all(&state).expect("state root");
	let environment = ProjectEnvironment::connect_or_start(
		&root,
		&state,
		&state.join("env.sock"),
		&state.join("docs.sock"),
		false,
		&[extension()],
		omp_tool::DEFAULT_INTERRUPT_GRACE,
		RegistryBridges::default(),
	)
	.await
	.expect("embedded environment");

	let first_lease = environment.bind_external_control_authorities(
		factory("omp.agents.list", "session-one"),
		factories("session-one"),
	);
	let connection = identity();
	let authority = environment
		.extension_control_authority(Arc::clone(&connection))
		.expect("production control authority");
	let context = ControlRequestContext {
		connection: Arc::clone(&connection),
		request_id: 1,
		invocation: None,
	};
	for operation in [
		"omp.agents.list",
		"omp.policy.capabilities",
		"omp.params.args",
		"omp.workers.list",
		"omp.direct_filesystem.request",
		"omp.creds.list",
		"omp.prompts.invalidate",
		"omp.ui.presentation",
		"omp.telemetry.query",
		"omp.jobs.register",
		"omp.provider.models",
		"omp.campaigns.active",
	] {
		let value = authority
			.request(
				context.clone(),
				Str::new(operation),
				serde_json::Map::new(),
			)
			.await
			.unwrap_or_else(|error| panic!("{operation} did not reach its owner: {error}"));
		assert_eq!(value, Value::String("session-one".to_owned()));
	}

	let service = authority
		.request(
			context.clone(),
			sf!("omp.services.connect"),
			serde_json::Map::new(),
		)
		.await
		.expect_err("the real service broker validates its service key");
	assert_ne!(service.message.as_str(), "must-be-overridden-by-envd");

	let second_lease = environment.bind_external_control_authorities(
		factory("omp.agents.list", "session-two"),
		factories("session-two"),
	);
	drop(first_lease);
	let stale = authority
		.request(context, sf!("omp.provider.models"), serde_json::Map::new())
		.await
		.expect_err("a connection from the superseded session is fenced");
	assert_eq!(stale.code.as_str(), "StaleGeneration");

	let replacement = environment
		.extension_control_authority(Arc::clone(&connection))
		.expect("replacement control connection");
	let value = replacement
		.request(
						ControlRequestContext {
				connection: Arc::clone(&connection),
				request_id: 2,
				invocation: None,
			},
			sf!("omp.provider.models"),
			serde_json::Map::new(),
		)
		.await
		.expect("replacement reaches new session owner");
	assert_eq!(value, Value::String("session-two".to_owned()));
	drop(second_lease);
	let revoked = replacement
		.request(
			ControlRequestContext {
				connection: Arc::clone(&connection),
				request_id: 3,
				invocation: None,
			},
			sf!("omp.agents.list"),
			serde_json::Map::new(),
		)
		.await
		.expect_err("dropping the atomic lease revokes agents and domain authorities together");
	assert_eq!(revoked.code.as_str(), "StaleGeneration");
}
