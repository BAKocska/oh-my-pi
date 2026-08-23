//! Proves normal server construction installs live production CONTROL owners.
use std::{collections::BTreeSet, fs, path::PathBuf, sync::Arc};

use omp_core::{Principal, sf};
use omp_envd::{
	EnvServer, RegistryBridges,
	exthost::control::{ControlConnectionIdentity, ControlEffect, ControlRequestContext},
	worker::ExtHostConfig,
};
use omp_tool::Registry;
use url::Url;

fn identity(principal: Principal) -> Arc<ControlConnectionIdentity> {
	Arc::new(ControlConnectionIdentity {
		extension: sf!("fixture.extension"),
		principal,
		artifact_digest: sf!("sha256:fixture"),
		layer: sf!("workspace"),
		tier: sf!("trusted"),
		trust: sf!("trusted"),
		host_generation: 7,
		session_generation: 11,
		capabilities: Arc::new(BTreeSet::new()),
	})
}

fn context(identity: Arc<ControlConnectionIdentity>) -> ControlRequestContext {
	ControlRequestContext { connection: identity, request_id: 1, invocation: None }
}

#[tokio::test]
async fn normal_server_construction_installs_live_control_owners() {
	let project = tempfile::tempdir().expect("project directory");
	let state = tempfile::tempdir().expect("state directory");
	let principal = Principal::new(sf!("fixture-principal"), sf!("Fixture Principal"));
	let config = ExtHostConfig::new(
		PathBuf::from("unused-with-empty-extension-set"),
		principal.clone(),
		sf!("fixture-session"),
		11,
	);
	fs::write(project.path().join("control.txt"), b"live-control").expect("control fixture");
	let server = EnvServer::open_local(
		project.path(),
		state.path(),
		Registry::new(),
		config,
		RegistryBridges::default(),
	)
	.await
	.expect("production Environment");

	let identity = identity(principal);
	let authority = server
		.extension_control_authority(Arc::clone(&identity))
		.expect("production CONTROL composition");

	assert!(authority.handles("omp.state_dir"));
	authority
		.authorize(&context(Arc::clone(&identity)), "omp.state_dir", &serde_json::Map::new())
		.expect("connection authority");
	let state_dir = authority
		.request(context(Arc::clone(&identity)), sf!("omp.state_dir"), serde_json::Map::new())
		.await
		.expect("state owner response");
	assert_eq!(state_dir, serde_json::Value::String(state.path().to_string_lossy().into_owned()));

	let url = Url::from_file_path(project.path().join("control.txt"))
		.expect("fixture file URL")
		.to_string();
	let mut url_arguments = serde_json::Map::new();
	url_arguments.insert(String::from("url"), serde_json::Value::String(url));
	let bytes = authority
		.request(context(Arc::clone(&identity)), sf!("omp.urls.read"), url_arguments)
		.await
		.expect("production URL owner");
	assert_eq!(bytes, serde_json::json!({"$bytes": omp_core::base64::encode(b"live-control")}));

	assert!(authority.handles("omp.mcp.servers"));
	let servers = authority
		.request(context(Arc::clone(&identity)), sf!("omp.mcp.servers"), serde_json::Map::new())
		.await
		.expect("scoped MCP owner response");
	assert_eq!(servers["servers"], serde_json::json!([]));
	assert!(servers["definition_epoch"].is_number());

	authority
		.effect(
			context(Arc::clone(&identity)),
			ControlEffect::Intent(serde_json::json!({
				"operation": "omp.intents.set",
				"arguments": {
					"key": "fixture",
					"intents": [{
						"kind": "reasoning",
						"on_unsupported": "error",
						"priority": 10,
						"payload": {"effort": "high"}
					}]
				}
			})),
		)
		.await
		.expect("provider intent owner");
	authority
		.effect(
			context(identity),
			ControlEffect::Intent(serde_json::json!({
				"operation": "omp.intents.clear",
				"arguments": {"key": "fixture"}
			})),
		)
		.await
		.expect("provider intent clear");
}
