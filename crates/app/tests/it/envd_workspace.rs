//! Workspace-generation and detach-in-place integration contracts.

use std::{future::Future, time::Duration};

use bytes::Bytes;
use omp_app::envd::{
	blobs::BlobHost,
	docs::DocumentHost,
	exec::{ExecEvent, ExecHost, ProcessEvent},
	workspace::{WorkspaceHost, WorkspaceOperations},
};
use omp_core::Str;
use omp_docserver::{
	Environment, ServerConfig,
	connection::{ConnectionConfig, serve_connection},
};
use omp_proto::env::v1::{
	AttachOutput, ConflictReason, CreateWorktree, DestroyWorktree, ExecRequest, MergeMode,
	MergeWorktree, OpenSessionRequest, RestoreWorkspace, Script, SnapshotWorkspace,
};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const DEADLINE: Duration = Duration::from_secs(10);

async fn within<T>(future: impl Future<Output = T>) -> T {
	tokio::time::timeout(DEADLINE, future)
		.await
		.expect("workspace operation exceeded its deterministic deadline")
}

async fn operations(root: &TempDir, state: &TempDir) -> (WorkspaceOperations, DocumentHost) {
	let config = ServerConfig::new(root.path()).expect("document config");
	let environment = Environment::new(config).expect("document authority");
	let (client, server) = tokio::io::duplex(256 * 1024);
	tokio::spawn(serve_connection(environment.clone(), server, ConnectionConfig::default()));
	let documents = within(DocumentHost::connect(client))
		.await
		.expect("document host");
	let (external_client, external_server) = tokio::io::duplex(256 * 1024);
	tokio::spawn(serve_connection(environment, external_server, ConnectionConfig::default()));
	let external = within(DocumentHost::connect(external_client))
		.await
		.expect("external document host");
	let workspace = WorkspaceHost::open(root.path()).expect("workspace host");
	let blobs = BlobHost::open(state.path().join("blobs")).expect("blob host");
	let operations =
		WorkspaceOperations::open(workspace, documents, blobs, state.path().join("worktrees"))
			.expect("workspace operations");
	(operations, external)
}

#[tokio::test]
async fn snapshot_restore_is_content_addressed_and_always_produces_undo() {
	let root = TempDir::new().expect("workspace");
	let state = TempDir::new().expect("state");
	std::fs::write(root.path().join("tracked.txt"), b"before\n").expect("fixture");
	let (operations, external) = operations(&root, &state).await;
	let cancel = CancellationToken::new();

	let snapshot = operations
		.snapshot(&SnapshotWorkspace::default(), &cancel)
		.expect("snapshot");
	let duplicate = operations
		.snapshot(&SnapshotWorkspace::default(), &cancel)
		.expect("duplicate snapshot");
	assert_eq!(snapshot.snapshot_id, duplicate.snapshot_id);
	assert_eq!(snapshot.manifest_hash.as_ref(), duplicate.manifest_hash.as_ref());

	std::fs::write(root.path().join("tracked.txt"), b"after\n").expect("mutate fixture");
	let uri = Str::from(
		url::Url::from_file_path(root.path().join("tracked.txt"))
			.expect("tracked file URI")
			.to_string(),
	);
	let external_lease = within(external.open(uri, None, &cancel))
		.await
		.expect("external lease");
	let dry_run = within(operations.restore(
		&RestoreWorkspace {
			snapshot_id: snapshot.snapshot_id.clone(),
			dry_run: true,
			..Default::default()
		},
		&cancel,
	))
	.await
	.expect("dry-run restore");
	assert_ne!(dry_run.undo_snapshot_id, "");
	assert_eq!(dry_run.conflicts.len(), 1);
	assert_eq!(dry_run.conflicts[0].reason, ConflictReason::OpenLease as i32);
	within(external.close(external_lease, &cancel))
		.await
		.expect("release external lease");
	assert_eq!(std::fs::read(root.path().join("tracked.txt")).unwrap(), b"after\n");

	let restored = within(operations.restore(
		&RestoreWorkspace { snapshot_id: snapshot.snapshot_id, ..Default::default() },
		&cancel,
	))
	.await
	.expect("restore");
	assert_ne!(restored.undo_snapshot_id, "");
	assert!(!restored.partial);
	assert!(restored.conflicts.is_empty());
	assert_eq!(std::fs::read(root.path().join("tracked.txt")).unwrap(), b"before\n");
}

#[tokio::test]
async fn snapshot_rejects_parent_escape_and_observes_cancellation() {
	let root = TempDir::new().expect("workspace");
	let state = TempDir::new().expect("state");
	std::fs::write(root.path().join("tracked.txt"), b"content").expect("fixture");
	let (operations, _external) = operations(&root, &state).await;
	let cancel = CancellationToken::new();
	assert!(
		operations
			.snapshot(
				&SnapshotWorkspace { paths: vec!["../outside".to_owned()], ..Default::default() },
				&cancel,
			)
			.is_err()
	);
	cancel.cancel();
	assert!(
		operations
			.snapshot(&SnapshotWorkspace::default(), &cancel)
			.is_err()
	);
}

#[tokio::test]
async fn worktree_isolation_persists_and_emits_patch_or_branch_without_git_worktree() {
	let root = TempDir::new().expect("workspace");
	let state = TempDir::new().expect("state");
	std::fs::write(root.path().join("tracked.txt"), b"parent\n").expect("fixture");
	let (operations, external) = operations(&root, &state).await;
	let cancel = CancellationToken::new();
	let created = operations
		.create_worktree(&CreateWorktree { name: "agent".to_owned(), ..Default::default() }, &cancel)
		.expect("create worktree");
	let worktree_root = url::Url::parse(&created.root_uri)
		.expect("root URI")
		.to_file_path()
		.expect("file URI");
	std::fs::write(worktree_root.join("tracked.txt"), b"child\n").expect("child mutation");
	assert_eq!(std::fs::read(root.path().join("tracked.txt")).unwrap(), b"parent\n");
	let reopened = WorkspaceOperations::open(
		WorkspaceHost::open(root.path()).expect("reopened workspace"),
		external,
		BlobHost::open(state.path().join("blobs")).expect("reopened blobs"),
		state.path().join("worktrees"),
	)
	.expect("reopen worktree registry");

	let patch = reopened
		.merge_worktree(
			&MergeWorktree {
				id: created.id.clone(),
				mode: MergeMode::Patch as i32,
				..Default::default()
			},
			&cancel,
		)
		.expect("patch disposition");
	assert!(patch.artifact.is_some());
	assert!(patch.branch.is_none());
	let branch = reopened
		.merge_worktree(
			&MergeWorktree {
				id: created.id.clone(),
				mode: MergeMode::Branch as i32,
				..Default::default()
			},
			&cancel,
		)
		.expect("branch disposition");
	assert_eq!(branch.branch.as_deref(), Some(format!("omp/agent/{}", created.id).as_str()));
	reopened
		.destroy_worktree(
			&DestroyWorktree { id: created.id, force: true, ..Default::default() },
			&cancel,
		)
		.expect("destroy worktree");
	assert!(!worktree_root.exists());
}

#[tokio::test]
async fn detach_reparents_the_exact_foreground_process_without_cancelling_it() {
	let root = TempDir::new().expect("workspace");
	let host = ExecHost::new();
	let cwd_uri = url::Url::from_directory_path(root.path())
		.expect("cwd URI")
		.to_string();
	let opened = host
		.open_session(OpenSessionRequest { cwd_uri, ..Default::default() })
		.await
		.expect("session");
	let (_, run) = host
		.exec(
			ExecRequest {
				session: opened.session,
				source: Some(Script {
					text: "sleep 0.1; printf retained".to_owned(),
					..Default::default()
				}),
				..Default::default()
			},
			None,
		)
		.await
		.expect("foreground execution");
	assert!(matches!(within(run.next_event()).await, Some(ExecEvent::Started { .. })));
	let started = host
		.detach_exec(run.id(), "retained-job")
		.expect("detach in place");
	assert_eq!(started.generation, 1);
	drop(run);

	let attachment = host
		.attach_output(&AttachOutput {
			name: "retained-job".to_owned(),
			generation: 1,
			..Default::default()
		})
		.expect("attachment");
	let mut terminal = attachment.state.status.is_some();
	let mut output = Vec::new();
	for frame in attachment.backlog {
		output.extend_from_slice(&frame.data);
	}
	while !terminal {
		match within(attachment.events.recv_async())
			.await
			.expect("process event")
		{
			ProcessEvent::Output(frame) => output.extend_from_slice(&frame.data),
			ProcessEvent::State(info) => terminal = info.status.is_some(),
		}
	}
	assert_eq!(Bytes::from(output), Bytes::from_static(b"retained"));
}
