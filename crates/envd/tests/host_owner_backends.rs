use std::{collections::BTreeSet, sync::Arc};

use omp_core::{InvocationPhase, LifecyclePhase, Principal, Str, sf};
use omp_envd::{
	exthost::{
		backends::{
			AdmissionSandboxRuntime, DurableDirectFilesystemJournal, HostDirectFilesystemExecutor,
			LiveParameterSource,
		},
		control::{
			AuditedDirectFilesystemRequest, ControlConnectionIdentity, ControlInvocationAuthority,
			ControlRequestContext, DirectFilesystemGrant,
		},
		params::{
			DirectFilesystemExecutor, DirectFilesystemJournal, DirectFilesystemOutput,
			ParameterOperation, ParameterPullRequest, ParameterSource,
		},
	},
	policy::{
		PolicyScope, SandboxCapabilities, SandboxEnforcement, SandboxPolicyRuntime, SandboxProfile,
	},
	worker_pool::{
		SupervisedWorkerProcess, WorkerObservation, WorkerProcessAuthority, WorkerSessionEndpoint,
		WorkerSite, WorkerSupervisor,
	},
};
use omp_tool::{IncomingParams, InvocationFeed};
use serde_json::{Value, json};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

fn capabilities() -> SandboxCapabilities {
	SandboxCapabilities {
		backends:         vec![sf!("native")],
		landlock_abi:     None,
		filesystem:       true,
		network:          true,
		domain_filtering: true,
		resource_limits:  true,
		degraded:         Vec::new(),
	}
}

fn enforcement() -> SandboxEnforcement {
	SandboxEnforcement {
		filesystem:       sf!("hard"),
		network:          sf!("hard"),
		process:          sf!("hard"),
		backend:          sf!("native"),
		degraded_reasons: Vec::new(),
	}
}

#[tokio::test]
async fn sandbox_admission_only_installs_narrowing_owner_fenced_profiles() {
	let runtime = AdmissionSandboxRuntime::new(capabilities());
	let mut baseline = SandboxProfile::default();
	baseline.filesystem.read_default = sf!("allow");
	baseline.filesystem.write_default = sf!("allow");
	runtime
		.activate(sf!("session"), baseline.clone(), enforcement())
		.expect("activate real sandbox receipt");

	let mut narrow = baseline.clone();
	narrow.filesystem.write_default = sf!("deny");
	let installed = runtime
		.install("owner-a", "session", narrow.clone(), PolicyScope::Session)
		.await
		.expect("deny default narrows allow default");
	assert_eq!(runtime.effective_profile("session").await.unwrap(), narrow);
	assert!(
		runtime
			.revoke("owner-b", &installed.handle_id)
			.await
			.is_err()
	);
	runtime
		.revoke("owner-a", &installed.handle_id)
		.await
		.expect("exact owner revokes exact handle");
	assert_eq!(runtime.effective_profile("session").await.unwrap(), baseline);
}

#[tokio::test]
async fn live_parameter_source_consumes_the_registered_invocation_feed() {
	let source = LiveParameterSource::default();
	let (feed, params): (InvocationFeed, IncomingParams<'static>) = IncomingParams::channel();
	source.register(sf!("invocation-1"), params).unwrap();
	feed
		.args_committed(Str::new_static(r#"{"path":"src/lib.rs","count":2}"#))
		.unwrap();
	let pulled = source
		.pull(
			ParameterPullRequest {
				invocation_id: sf!("invocation-1"),
				operation:     ParameterOperation::Args,
				path:          Vec::new(),
				mode:          None,
				aliases:       Vec::new(),
				coercions:     Vec::new(),
				example:       None,
				expected:      None,
				offset:        None,
				index:         None,
				optional:      false,
				interruptible: false,
			},
			CancellationToken::new(),
		)
		.await
		.unwrap();
	assert_eq!(pulled.0, json!({"path": "src/lib.rs", "count": 2}));
	assert!(source.unregister("invocation-1"));
}

#[tokio::test]
async fn worker_authority_reads_generation_fenced_process_state() {
	let supervisor = WorkerSupervisor::new(2, 1);
	let (route, lease) = supervisor
		.open(omp_envd::worker_pool::WorkerKey {
			extension: sf!("fixture.extension"),
			name:      sf!("worker"),
			site:      sf!("env"),
		})
		.unwrap();
	lease.relinquish();
	let observation = WorkerObservation {
		name:            route.key.name.clone(),
		generation:      route.generation,
		state:           sf!("ready"),
		site:            WorkerSite::default(),
		pid:             Some(std::process::id()),
		spawned_at_ms:   1,
		last_call_at_ms: None,
		calls:           0,
		in_flight:       0,
		code_cached:     0,
		enforced:        vec![sf!("generation")],
		fault:           None,
	};
	supervisor
		.publish_process(&route, SupervisedWorkerProcess {
			observation: observation.clone(),
			endpoint:    Some(WorkerSessionEndpoint {
				generation: route.generation,
				family:     sf!("unix"),
				address:    Value::String("/tmp/worker.sock".to_owned()),
				authkey:    Some(bytes::Bytes::from_static(b"secret")),
			}),
			cancel:      CancellationToken::new(),
			terminated:  {
				let terminated = CancellationToken::new();
				terminated.cancel();
				terminated
			},
		})
		.unwrap();
	assert_eq!(supervisor.observe(&route).await.unwrap(), observation);
	assert_eq!(
		supervisor
			.session(&route, CancellationToken::new())
			.await
			.unwrap()
			.generation,
		route.generation
	);
}

fn request_context() -> ControlRequestContext {
	let connection = Arc::new(ControlConnectionIdentity {
		extension:          sf!("fixture.extension"),
		principal:          Principal::new(sf!("fixture"), sf!("Fixture")),
		artifact_digest:    sf!("digest"),
		layer:              sf!("project"),
		tier:               sf!("trusted"),
		trust:              sf!("trusted"),
		host_generation:    7,
		session_generation: 11,
		capabilities:       Arc::new(BTreeSet::from([sf!("trusted.direct-filesystem")])),
	});
	ControlRequestContext {
		connection,
		request_id: 9,
		invocation: Some(ControlInvocationAuthority {
			invocation:        sf!("call-1"),
			phase:             InvocationPhase::EffectsAuthorized,
			session:           sf!("session"),
			turn:              Some(1),
			event:             None,
			call:              Some(sf!("call-1")),
			device:            None,
			effects:           Box::new([]),
			place_kind:        sf!("host"),
			lifecycle:         LifecyclePhase::Active,
			roots:             Box::new([]),
			remote:            false,
			has_ui:            false,
			headless:          true,
			settings:          serde_json::Map::new(),
			secret_settings:   Box::new([]),
			data:              None,
			direct_filesystem: None,
		}),
	}
}

#[tokio::test]
async fn direct_filesystem_journal_is_durable_before_bounded_execution() {
	let directory = tempdir().unwrap();
	let audit_path = directory.path().join("audit.jsonl");
	let target = directory.path().join("payload.bin");
	let journal = DurableDirectFilesystemJournal::new(audit_path.clone());
	let request = AuditedDirectFilesystemRequest {
		operation: sf!("write"),
		path:      target.clone(),
		data:      bytes::Bytes::from_static(b"payload"),
		grant:     DirectFilesystemGrant {
			extension_id:      sf!("fixture.extension"),
			publisher:         sf!("publisher"),
			capability_digest: sf!("capability"),
			grant_id:          sf!("grant"),
			generation:        7,
		},
	};
	let receipt = journal
		.append_request(&request_context(), &request)
		.await
		.unwrap();
	assert!(!receipt.is_empty());
	let durable = tokio::fs::read_to_string(&audit_path).await.unwrap();
	assert!(durable.contains("direct_filesystem_request"));
	assert!(durable.contains("fixture.extension"));

	let output = HostDirectFilesystemExecutor
		.execute(request, CancellationToken::new())
		.await
		.unwrap();
	assert_eq!(output, DirectFilesystemOutput::Applied);
	assert_eq!(tokio::fs::read(target).await.unwrap(), b"payload");
}
