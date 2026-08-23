//! Proves workspace snapshot protobuf metadata, restore fences, effects, and
//! conflicts round-trip.
use bytes::Bytes;
use omp_proto::{
	SCHEMA_REV,
	env::v1::{
		ConflictReason, ListWorkspaceSnapshots, RestoreWorkspace, SnapshotWorkspace,
		WorkspaceConflict, WorkspaceOp, WorkspaceRestored, WorkspaceResult, WorkspaceSnapshot,
		WorkspaceSnapshotList, workspace_op, workspace_result,
	},
};
use prost::Message as _;

#[test]
fn workspace_snapshot_protocol_round_trips_authoritative_metadata() {
	let snapshot = WorkspaceSnapshot {
		snapshot_id:        "0123456789abcdef".repeat(4),
		manifest_hash:      Bytes::from_static(&[7; 32]),
		files:              3,
		bytes:              144,
		generation:         9,
		label:              Some("before-refactor".to_owned()),
		created_ms:         1_800_000_000_000,
		root_uri:           "file:///workspace".to_owned(),
		parent_snapshot_id: Some("fedcba9876543210".repeat(4)),
		tree_hash:          "0123456789abcdef".repeat(4),
		entry_count:        3,
		partial:            true,
		wire_revision:      SCHEMA_REV,
		props:              Default::default(),
	};
	let listed = WorkspaceResult {
		result: Some(workspace_result::Result::List(WorkspaceSnapshotList {
			snapshots:     vec![snapshot.clone()],
			wire_revision: SCHEMA_REV,
			props:         Default::default(),
		})),
		props:  Default::default(),
	};
	let decoded = WorkspaceResult::decode(listed.encode_to_vec().as_slice()).expect("decode list");
	let Some(workspace_result::Result::List(decoded)) = decoded.result else {
		panic!("workspace snapshot list result");
	};
	assert_eq!(decoded.snapshots, vec![snapshot]);
	assert_eq!(decoded.wire_revision, SCHEMA_REV);
}

#[test]
fn workspace_requests_and_restore_report_keep_fences_and_effects() {
	let requests = [
		WorkspaceOp {
			op:    Some(workspace_op::Op::Snapshot(SnapshotWorkspace {
				scope:               "workspace".to_owned(),
				paths:               vec!["src".to_owned()],
				label:               Some("checkpoint".to_owned()),
				expected_generation: 11,
				wire_revision:       SCHEMA_REV,
				props:               Default::default(),
			})),
			props: Default::default(),
		},
		WorkspaceOp {
			op:    Some(workspace_op::Op::List(ListWorkspaceSnapshots {
				limit:         50,
				wire_revision: SCHEMA_REV,
				props:         Default::default(),
			})),
			props: Default::default(),
		},
		WorkspaceOp {
			op:    Some(workspace_op::Op::Restore(RestoreWorkspace {
				snapshot_id:         "snapshot".to_owned(),
				dry_run:             true,
				scope:               "workspace".to_owned(),
				paths:               vec!["src/lib.rs".to_owned()],
				expected_generation: 11,
				wire_revision:       SCHEMA_REV,
				props:               Default::default(),
			})),
			props: Default::default(),
		},
	];
	for request in requests {
		WorkspaceOp::decode(request.encode_to_vec().as_slice()).expect("decode request");
	}

	let restored = WorkspaceRestored {
		snapshot_id:      "snapshot".to_owned(),
		undo_snapshot_id: "undo".to_owned(),
		conflicts:        vec![WorkspaceConflict {
			path:         "src/lib.rs".to_owned(),
			reason:       ConflictReason::GenerationChanged as i32,
			detail:       Some("stale generation".to_owned()),
			lease_holder: Some("holder-1".to_owned()),
		}],
		partial:          false,
		from_generation:  11,
		to_generation:    11,
		written:          2,
		deleted:          1,
		unchanged:        8,
		dry_run:          true,
		wire_revision:    SCHEMA_REV,
		props:            Default::default(),
	};
	let result = WorkspaceResult {
		result: Some(workspace_result::Result::Restored(restored.clone())),
		props:  Default::default(),
	};
	let decoded =
		WorkspaceResult::decode(result.encode_to_vec().as_slice()).expect("decode restore");
	assert_eq!(
		decoded.result,
		Some(omp_proto::env::v1::workspace_result::Result::Restored(restored))
	);
}
