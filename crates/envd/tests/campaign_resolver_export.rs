use std::{collections::BTreeSet, sync::Arc};

use omp_core::{ArtifactDigest, Point, Principal, Provenance, Str, sf};
use omp_envd::{
	exthost::{
		CallbackConcurrency,
		control::{ControlConnectionIdentity, ControlDispatch, ControlProtocolError},
		dispatch::CallbackDispatcher,
	},
	worker::{ExtensionCampaignResolver, SealedRegistryEvidence},
};
use parking_lot::Mutex;

#[derive(Default)]
struct RecordingCallbacks {
	calls: Mutex<Vec<(Str, u64, u64)>>,
}

#[async_trait::async_trait]
impl CallbackDispatcher for RecordingCallbacks {
	async fn dispatch(
		&self,
		target: Arc<ControlConnectionIdentity>,
		dispatch: ControlDispatch,
	) -> Result<serde_json::Value, ControlProtocolError> {
		assert_eq!(dispatch.operation, "omp.campaigns.react");
		assert_eq!(dispatch.policy, CallbackConcurrency::Serialized);
		assert_eq!(dispatch.arguments["campaign"], "retry");
		assert_eq!(dispatch.arguments["host_generation"], 7);
		assert_eq!(dispatch.arguments["session_generation"], 11);
		self.calls.lock().push((
			target.extension.clone(),
			target.host_generation,
			target.session_generation,
		));
		Ok(serde_json::json!({
			"engagement_id": dispatch.arguments["engagement_id"],
			"campaign_rev": dispatch.arguments["campaign_rev"],
			"verdict": "continue",
			"verdict_payload": {
				"$bytes": omp_core::base64::encode(b"{}"),
			},
			"new_state": {
				"$bytes": omp_core::base64::encode(b"next"),
			},
		}))
	}
}

fn identity(host_generation: u64) -> Arc<ControlConnectionIdentity> {
	Arc::new(ControlConnectionIdentity {
		extension: sf!("fixture.extension"),
		principal: Principal::new(sf!("fixture"), sf!("Fixture")),
		artifact_digest: sf!("sha256:fixture"),
		layer: sf!("project"),
		tier: sf!("trusted"),
		trust: sf!("trusted"),
		host_generation,
		session_generation: 11,
		capabilities: Arc::new(BTreeSet::new()),
	})
}

fn evidence(identity: Arc<ControlConnectionIdentity>) -> Arc<SealedRegistryEvidence> {
	Arc::new(SealedRegistryEvidence {
		identity,
		session: Some(sf!("session-1")),
		provenance: Provenance::new(
			sf!("publisher"),
			sf!("fixture.extension"),
			sf!("1.0.0"),
			ArtifactDigest::new([7; 32]),
			sf!("project"),
			sf!("trusted"),
			7,
		),
		tools: Arc::from([]),
		hooks: Arc::from([]),
		providers: Arc::from([]),
		campaigns: Arc::from([serde_json::json!({
			"id": "retry",
			"rev": 3,
			"points": ["settle"],
			"scope": "session",
			"exhaust": "settle",
			"state_family": "fixture.RetryState",
			"state_rev": 2,
			"ladder": null,
			"policy": null,
			"when": null,
			"on_failure": "fault",
			"claims": ["mode", "custom-slot"],
			"binds": [],
			"composes": false,
		})]),
	})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retained_freeze_table_resolves_and_dispatches_only_its_live_generation() {
	let live = identity(7);
	let retained = evidence(Arc::clone(&live));
	let callbacks = Arc::new(RecordingCallbacks::default());
	let resolver = ExtensionCampaignResolver::new(callbacks.clone(), move |candidate| {
		(candidate.host_generation == 7).then(|| Arc::clone(&retained))
	});

	let (spec, mut machine) = resolver
		.resolve(&live, "retry", Some("seed"))
		.expect("live frozen declaration");
	assert_eq!(spec.id, "retry");
	assert_eq!(spec.family_rev, "fixture.RetryState@2");
	assert!(spec.points.contains(Point::Settle));
	assert_eq!(resolver.owner("retry").as_deref(), Some("fixture.extension"));

	let reaction = machine.react(Point::Settle, &Default::default());
	assert_eq!(reaction.verdicts, vec![omp_agent::Verdict::Continue]);
	assert_eq!(machine.state(), "next");
	assert_eq!(callbacks.calls.lock().as_slice(), &[(sf!("fixture.extension"), 7, 11)]);

	let stale = identity(8);
	let error = resolver
		.resolve(&stale, "retry", None)
		.err()
		.expect("stale generation must be rejected");
	assert!(error.to_string().contains("generation"));
}
