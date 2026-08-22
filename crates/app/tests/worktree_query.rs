use std::path::Path;

use omp_docserver::{
	Environment, ServerConfig,
	connection::{ConnectionConfig, serve_connection},
};
use omp_envd::{
	blobs::BlobHost,
	docs::DocumentHost,
	workspace::{WorkspaceHost, WorkspaceOperations},
};
use omp_proto::env::v1::{CreateWorktree, DestroyWorktree};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

async fn operations(root: &Path, state: &Path) -> (WorkspaceOperations, DocumentHost, BlobHost) {
	let environment = Environment::new(ServerConfig::new(root).expect("document config"))
		.expect("document authority");
	let (client, server) = tokio::io::duplex(256 * 1024);
	tokio::spawn(serve_connection(environment, server, ConnectionConfig::default()));
	let documents = DocumentHost::connect(client).await.expect("document host");
	let blobs = BlobHost::open(state.join("blobs")).expect("blob host");
	let workspace = WorkspaceHost::open(root).expect("workspace host");
	let operations = WorkspaceOperations::open(
		workspace,
		documents.clone(),
		blobs.clone(),
		state.join("worktrees"),
	)
	.expect("workspace operations");
	(operations, documents, blobs)
}

#[tokio::test]
async fn current_worktree_follows_the_registered_worktree_lifecycle() {
	let root = TempDir::new().expect("workspace");
	let state = TempDir::new().expect("state");
	std::fs::write(root.path().join("tracked.txt"), b"primary\n").expect("fixture");
	let (primary, documents, blobs) = operations(root.path(), state.path()).await;
	let cancel = CancellationToken::new();

	assert_eq!(primary.current_worktree().expect("primary query"), None);

	let created = primary
		.create_worktree(&CreateWorktree { name: "query".to_owned(), ..Default::default() }, &cancel)
		.expect("create worktree");
	let worktree_root = url::Url::parse(&created.root_uri)
		.expect("worktree root URI")
		.to_file_path()
		.expect("worktree file URI");
	let isolated = WorkspaceOperations::open(
		WorkspaceHost::open(&worktree_root).expect("isolated workspace host"),
		documents,
		blobs,
		state.path().join("worktrees"),
	)
	.expect("isolated workspace operations");

	assert_eq!(isolated.current_worktree().expect("active query"), Some(created.clone()));

	primary
		.destroy_worktree(
			&DestroyWorktree { id: created.id, force: true, ..Default::default() },
			&cancel,
		)
		.expect("destroy worktree");
	assert_eq!(isolated.current_worktree().expect("stale query"), None);
}
