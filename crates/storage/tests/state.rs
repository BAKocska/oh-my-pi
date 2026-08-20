use std::{
	fs::OpenOptions,
	io::{Seek, SeekFrom, Write},
	sync::{Arc, Barrier},
	thread,
};

use omp_core::{ArtifactDigest, Principal, Provenance, Str};
use omp_storage::state::{
	DurableRequest, Error, GenerationFence, OrganizationAccess, StateAuthority, StateChange,
	StateRevision, StateScope, StateStore,
};
use tempfile::tempdir;

const GENERATION: GenerationFence = GenerationFence { host: 7, session: 11 };

fn state_authority(principal: &str, session: &str, project: &str) -> StateAuthority {
	StateAuthority::new_core(
		Principal::new(Str::new(principal), Str::new(principal)),
		Provenance::new(
			Str::new("publisher-key"),
			Str::new("dev.example.extension"),
			Str::new("1.2.3"),
			ArtifactDigest::new([3; 32]),
			Str::new("user"),
			Str::new("trusted"),
			GENERATION.host,
		),
		"dev.example.extension",
		session,
		project,
		GENERATION,
	)
	.unwrap()
}

fn request(attempt: &str, key: Option<&str>) -> DurableRequest {
	DurableRequest::new(attempt, key.map(Str::new), GENERATION).unwrap()
}

#[test]
fn multiple_authorities_can_open_the_same_store() {
	let directory = tempdir().unwrap();
	let first = StateStore::open(directory.path()).unwrap();
	let second = StateStore::open(directory.path()).unwrap();
	drop((first, second));
}

#[test]
fn authority_scopes_are_isolated_and_session_is_journal_delegated() {
	let directory = tempdir().unwrap();
	let store = StateStore::open(directory.path()).unwrap();
	let first = state_authority("alice", "session-a", "project-a")
		.with_organization(OrganizationAccess::new("org-a", true).unwrap());
	let same_project = state_authority("alice", "session-b", "project-a")
		.with_organization(OrganizationAccess::new("org-a", true).unwrap());
	let other = state_authority("bob", "session-c", "project-b")
		.with_organization(OrganizationAccess::new("org-b", true).unwrap());

	assert!(matches!(
		store.append(
			&first,
			StateScope::Session,
			"dev.example.item",
			"1",
			b"must-use-journal",
			&request("session-append", None),
		),
		Err(Error::WrongAuthority(StateScope::Session))
	));
	for (ordinal, scope) in [StateScope::Project, StateScope::User, StateScope::Organization]
		.into_iter()
		.enumerate()
	{
		store
			.append(
				&first,
				scope,
				"dev.example.item",
				"1",
				format!("scope-{ordinal}").as_bytes(),
				&request(&format!("append-{ordinal}"), None),
			)
			.unwrap();
	}

	assert!(matches!(
		store.entries(
			&same_project,
			StateScope::Session,
			"dev.example.extension",
			"dev.example.item",
			None,
			None,
		),
		Err(Error::WrongAuthority(StateScope::Session))
	));
	assert_eq!(
		store
			.entries(
				&same_project,
				StateScope::Project,
				"dev.example.extension",
				"dev.example.item",
				None,
				None,
			)
			.unwrap()
			.count(),
		1,
	);
	assert_eq!(
		store
			.entries(
				&same_project,
				StateScope::User,
				"dev.example.extension",
				"dev.example.item",
				None,
				None,
			)
			.unwrap()
			.count(),
		1,
	);
	assert_eq!(
		store
			.entries(
				&same_project,
				StateScope::Organization,
				"dev.example.extension",
				"dev.example.item",
				None,
				None,
			)
			.unwrap()
			.count(),
		1,
	);

	for scope in [StateScope::Project, StateScope::User, StateScope::Organization] {
		assert_eq!(
			store
				.entries(&other, scope, "dev.example.extension", "dev.example.item", None, None,)
				.unwrap()
				.count(),
			0,
		);
	}
}

#[test]
fn compare_exchange_serializes_racing_sessions_without_lost_updates() {
	let directory = tempdir().unwrap();
	let stores =
		[StateStore::open(directory.path()).unwrap(), StateStore::open(directory.path()).unwrap()];
	let barrier = Arc::new(Barrier::new(3));
	let mut joins = Vec::new();

	for (ordinal, store) in stores.into_iter().enumerate() {
		let barrier = Arc::clone(&barrier);
		joins.push(thread::spawn(move || {
			let authority = state_authority("alice", &format!("session-{ordinal}"), "project-a");
			barrier.wait();
			store.compare_exchange(
				&authority,
				StateScope::Project,
				"counter",
				None,
				format!("winner-{ordinal}").as_bytes(),
				&request(&format!("race-attempt-{ordinal}"), Some(&format!("race-key-{ordinal}"))),
			)
		}));
	}
	barrier.wait();
	let results = joins
		.into_iter()
		.map(|join| join.join().unwrap())
		.collect::<Vec<_>>();
	assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
	assert_eq!(
		results
			.iter()
			.filter(|result| matches!(
				result,
				Err(Error::CasConflict { expected: None, actual: Some(_) })
			))
			.count(),
		1,
	);

	let reader = state_authority("alice", "session-reader", "project-a");
	let store = StateStore::open(directory.path()).unwrap();
	let value = store
		.value(&reader, StateScope::Project, "dev.example.extension", "counter")
		.unwrap()
		.unwrap();
	assert!(value.value.as_ref() == b"winner-0" || value.value.as_ref() == b"winner-1");
}

#[test]
fn replay_restores_log_cas_content_roots_and_idempotency() {
	let directory = tempdir().unwrap();
	let authority = state_authority("alice", "session-a", "project-a");
	let append_request = request("append-attempt-1", Some("append-logical"));
	let value_request = request("value-attempt-1", Some("value-logical"));
	let content_request = request("content-attempt-1", Some("content-logical"));
	let (entry_id, installed, rooted) = {
		let store = StateStore::open(directory.path()).unwrap();
		let entry_id = store
			.append(
				&authority,
				StateScope::Project,
				"dev.example.item",
				"1",
				b"typed-payload",
				&append_request,
			)
			.unwrap();
		let installed = store
			.compare_exchange(&authority, StateScope::Project, "key", None, b"value", &value_request)
			.unwrap();
		let rooted = store
			.put_content(&authority, StateScope::Project, b"large immutable value", &content_request)
			.unwrap();
		assert_eq!(entry_id.revision(), StateRevision::new(1));
		assert_eq!(installed.revision, StateRevision::new(2));
		assert_eq!(rooted.revision, StateRevision::new(3));
		(entry_id, installed, rooted)
	};

	let store = StateStore::open(directory.path()).unwrap();
	let replayed_entry = store
		.append(
			&authority,
			StateScope::Project,
			"dev.example.item",
			"1",
			b"typed-payload",
			&request("append-attempt-2", Some("append-logical")),
		)
		.unwrap();
	assert_eq!(replayed_entry, entry_id);
	assert!(matches!(
		store.append(
			&authority,
			StateScope::Project,
			"dev.example.item",
			"1",
			b"different-payload",
			&request("append-attempt-3", Some("append-logical")),
		),
		Err(Error::IdempotencyConflict)
	));
	assert_eq!(
		store
			.compare_exchange(
				&authority,
				StateScope::Project,
				"key",
				None,
				b"value",
				&request("value-attempt-2", Some("value-logical")),
			)
			.unwrap(),
		installed,
	);
	assert_eq!(
		store
			.put_content(
				&authority,
				StateScope::Project,
				b"large immutable value",
				&request("content-attempt-2", Some("content-logical")),
			)
			.unwrap(),
		rooted,
	);
	assert_eq!(
		store
			.get_content(&authority, StateScope::Project, "dev.example.extension", &rooted.reference,)
			.unwrap()
			.as_ref(),
		b"large immutable value",
	);
	let other_project = state_authority("alice", "session-other", "project-other");
	assert!(matches!(
		store.get_content(
			&other_project,
			StateScope::Project,
			"dev.example.extension",
			&rooted.reference,
		),
		Err(Error::ContentNotRooted)
	));
	assert_eq!(
		store
			.entries(
				&authority,
				StateScope::Project,
				"dev.example.extension",
				"dev.example.item",
				None,
				None,
			)
			.unwrap()
			.count(),
		1,
	);
	let (count, watermark) = store
		.fold(
			&authority,
			StateScope::Project,
			"dev.example.extension",
			"dev.example.item",
			None,
			0_usize,
			|count, _entry| count + 1,
		)
		.unwrap();
	assert_eq!(count, 1);
	assert_eq!(watermark, Some(entry_id));
}

#[test]
fn any_corrupt_record_prevents_the_store_from_opening() {
	let directory = tempdir().unwrap();
	let authority = state_authority("alice", "session-a", "project-a");
	{
		let store = StateStore::open(directory.path()).unwrap();
		store
			.append(
				&authority,
				StateScope::Project,
				"dev.example.item",
				"1",
				b"truth",
				&request("append", None),
			)
			.unwrap();
	}

	let mut file = OpenOptions::new()
		.read(true)
		.write(true)
		.open(directory.path().join("state-v1.jsonl"))
		.unwrap();
	file.seek(SeekFrom::Start(12)).unwrap();
	file.write_all(b"X").unwrap();
	file.sync_data().unwrap();

	assert!(matches!(StateStore::open(directory.path()), Err(Error::CorruptRecord { line: 1, .. })));
}

#[test]
fn principal_namespace_generation_and_organization_restrictions_fail_closed() {
	let directory = tempdir().unwrap();
	let store = StateStore::open(directory.path()).unwrap();
	let alice = state_authority("alice", "session-a", "project-a");
	let bob = state_authority("bob", "session-b", "project-b");
	store
		.append(
			&alice,
			StateScope::User,
			"dev.example.item",
			"1",
			b"alice-only",
			&request("alice-append", None),
		)
		.unwrap();
	assert_eq!(
		store
			.entries(&bob, StateScope::User, "dev.example.extension", "dev.example.item", None, None,)
			.unwrap()
			.count(),
		0,
	);
	assert!(matches!(
		store.entries(
			&alice,
			StateScope::User,
			"dev.other.extension",
			"dev.example.item",
			None,
			None,
		),
		Err(Error::NamespaceDenied)
	));
	let mut granted = alice.clone();
	granted.grant_read_namespace("dev.other.extension").unwrap();
	assert!(granted.may_read_namespace("dev.other.extension"));

	let org_reader = alice
		.clone()
		.with_organization(OrganizationAccess::new("org-a", false).unwrap());
	assert!(matches!(
		store.append(
			&org_reader,
			StateScope::Organization,
			"dev.example.item",
			"1",
			b"forbidden",
			&request("org-append", None),
		),
		Err(Error::ScopeDenied(StateScope::Organization))
	));

	let stale = DurableRequest::new("stale", None, GenerationFence {
		host:    GENERATION.host - 1,
		session: GENERATION.session,
	})
	.unwrap();
	assert!(matches!(
		store.append(&alice, StateScope::Project, "dev.example.item", "1", b"zombie-write", &stale,),
		Err(Error::StaleGeneration)
	));

	let mismatched_provenance = Provenance::new(
		Str::new("publisher-key"),
		Str::new("dev.example.extension"),
		Str::new("1.2.3"),
		ArtifactDigest::new([3; 32]),
		Str::new("user"),
		Str::new("trusted"),
		GENERATION.host - 1,
	);
	assert!(matches!(
		StateAuthority::new_core(
			Principal::new(Str::new("alice"), Str::new("Alice")),
			mismatched_provenance,
			"dev.example.extension",
			"session-a",
			"project-a",
			GENERATION,
		),
		Err(Error::InvalidAuthority)
	));
}

#[test]
fn dropping_a_watcher_cancels_it_without_blocking_the_writer() {
	let directory = tempdir().unwrap();
	let store = StateStore::open(directory.path()).unwrap();
	let authority = state_authority("alice", "session-a", "project-a");
	let cancelled = store
		.watch(&authority, StateScope::Project, "dev.example.extension", None)
		.unwrap();
	drop(cancelled);

	let live = store
		.watch(&authority, StateScope::Project, "dev.example.extension", Some(StateRevision::new(0)))
		.unwrap();
	let id = store
		.append(
			&authority,
			StateScope::Project,
			"dev.example.item",
			"1",
			b"after-cancel",
			&request("after-cancel", None),
		)
		.unwrap();
	let StateChange::Entry(change) = live.try_recv().unwrap() else {
		panic!("watcher received wrong change type");
	};
	assert_eq!(change.id, id);
	assert_eq!(change.raw, b"after-cancel");
}
