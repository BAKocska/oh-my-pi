//! Integration coverage for environment-daemon policy enforcement.

use std::sync::Arc;

use bytes::Bytes;
use omp_app::envd::{
	policy::{
		AuthorityTable, DataAuthority, Grants, PolicyError, QuotaAccount, require_sandbox_enforcement,
	},
	worker::HostKey,
};
use omp_core::Str;

fn host() -> HostKey {
	HostKey::new("workspace", "sandboxed", "dev.example.policy")
}

#[test]
fn hello_grants_are_requested_intersection_without_wildcards() {
	let actual = Grants::supported(["env.doc.read", "env.blob", "*"]);
	let requested = vec!["*".to_owned(), "env.doc.read".to_owned(), "env.process".to_owned()];
	let granted = actual.requested(&requested);
	assert!(granted.contains("env.doc.read"));
	assert!(!granted.contains("env.blob"));
	assert!(!granted.contains("env.process"));
	assert!(!granted.contains("*"));
}

#[test]
fn core_effect_envelope_maps_to_exact_worker_data_bounds() {
	let envelope = omp_proto::policy::v1::EffectEnvelope {
		documents: Some(omp_proto::policy::v1::DocEffects {
			read:        true,
			write_globs: Vec::new(),
			props:       Default::default(),
		}),
		exec:      Some(omp_proto::policy::v1::ExecEffects {
			commands: vec!["ruff".to_owned()],
			network:  false,
			props:    Default::default(),
		}),
		inference: None,
		subagents: 0,
		props:     Default::default(),
	};
	let grants = Grants::from_effect_envelope(&envelope);
	for expected in ["env.doc.read", "env.fs.read", "env.search", "env.lsp", "env.exec", "env.blob"]
	{
		assert!(grants.contains(expected), "missing {expected}");
	}
	assert!(!grants.contains("env.process"));
	assert!(!grants.contains("invocation"));
}

#[test]
fn data_is_fail_closed_until_exact_token_and_generations_are_authorized() {
	let table = AuthorityTable::default();
	let host = host();
	table.register_host(host.clone(), Grants::supported(["env.doc.read"]));
	table.open(host.clone(), Str::from("call-1"));
	let owner = table.connection_owner();
	let authority = || DataAuthority {
		invocation_id:      "call-1",
		effect_token:       b"core-token",
		host_generation:    7,
		session_generation: 3,
	};
	assert_eq!(
		table.validate(&host, owner, authority(), "env.doc.read"),
		Err(PolicyError::EffectsNotAuthorized),
	);
	table
		.authorize(
			&host,
			"call-1",
			Bytes::from_static(b"core-token"),
			Grants::supported(["env.doc.read"]),
			100,
			7,
			3,
		)
		.expect("authorize");
	assert_eq!(table.validate(&host, owner, authority(), "env.doc.read"), Ok(()));
	assert_eq!(
		table.validate(
			&host,
			owner,
			DataAuthority { host_generation: 8, ..authority() },
			"env.doc.read",
		),
		Err(PolicyError::StaleGeneration),
	);
}

#[test]
fn effect_tokens_and_leases_are_connection_bound_and_revoked_at_settle() {
	let table = AuthorityTable::default();
	let host = host();
	table.register_host(host.clone(), Grants::supported(["env.doc.read"]));
	table.open(host.clone(), Str::from("call-2"));
	table
		.authorize(
			&host,
			"call-2",
			Bytes::from_static(b"token-2"),
			Grants::supported(["env.doc.read"]),
			100,
			1,
			1,
		)
		.expect("authorize");
	let first = table.connection_owner();
	let second = table.connection_owner();
	let credentials = || DataAuthority {
		invocation_id:      "call-2",
		effect_token:       b"token-2",
		host_generation:    1,
		session_generation: 1,
	};
	assert_eq!(table.validate(&host, first, credentials(), "env.doc.read"), Ok(()));
	assert_eq!(
		table.validate(&host, second, credentials(), "env.doc.read"),
		Err(PolicyError::InvalidEffectToken),
	);
	let lease = Bytes::from_static(b"lease");
	table.register_lease(lease.clone(), first);
	assert_eq!(table.check_lease(&lease, second), Err(PolicyError::LeaseNotOwned));
	table.settle(&host, "call-2");
	assert_eq!(
		table.validate(&host, first, credentials(), "env.doc.read"),
		Err(PolicyError::EffectsNotAuthorized),
	);
}

#[test]
fn envelope_escalation_quota_and_deferred_enforce_are_typed_refusals() {
	let table = Arc::new(AuthorityTable::default());
	let host = host();
	table.register_host(host.clone(), Grants::supported(["env.doc.read"]));
	table.open(host.clone(), Str::from("call-3"));
	table
		.authorize(
			&host,
			"call-3",
			Bytes::from_static(b"token-3"),
			Grants::supported(["env.doc.read", "env.doc.write"]),
			100,
			1,
			1,
		)
		.expect("authorize");
	let owner = table.connection_owner();
	let credentials = DataAuthority {
		invocation_id:      "call-3",
		effect_token:       b"token-3",
		host_generation:    1,
		session_generation: 1,
	};
	assert_eq!(
		table.validate(&host, owner, credentials, "env.doc.write"),
		Err(PolicyError::Denied { capability: "env.doc.write" }),
	);
	let mut quota = QuotaAccount::new(table, Some(host));
	assert!(matches!(
		quota.charge_blob_bytes(256 * 1024 * 1024 + 1),
		Err(PolicyError::QuotaExceeded { quota: "blob_ingest_bytes", .. })
	));
	assert_eq!(require_sandbox_enforcement(true), Err(PolicyError::EnforcementUnavailable));
}
