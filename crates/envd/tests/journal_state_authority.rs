//! Durable session-index proof used by the JSON persistence authority.

use std::io;

use omp_core::Str;
use omp_storage::{
	index::{NewSession, SessionIndex, SessionKind},
	transcript::SessionId,
};

#[test]
fn indexed_get_and_lineage_are_authoritative_and_root_first() {
	let scratch = tempfile::tempdir().expect("scratch directory");
	let index =
		SessionIndex::open(scratch.path().join("sessions.sqlite3")).expect("sessions authority");
	let root = SessionId(Str::from("root"));
	let child = SessionId(Str::from("child"));
	index
		.create_session(
			&NewSession {
				id:         &root,
				cwd:        "/workspace",
				project:    "/workspace",
				created_ms: 1,
				kind:       SessionKind::Interactive,
				parent:     None,
				remote:     false,
			},
			|| Ok::<_, io::Error>(((), 1)),
		)
		.expect("index root");
	index
		.create_session(
			&NewSession {
				id:         &child,
				cwd:        "/workspace",
				project:    "/workspace",
				created_ms: 2,
				kind:       SessionKind::Subagent,
				parent:     Some(&root),
				remote:     false,
			},
			|| Ok::<_, io::Error>(((), 1)),
		)
		.expect("index child");

	let row = index.get(&child).expect("get child").expect("child exists");
	assert_eq!(row.parent, Some(root.clone()));
	let lineage = index.lineage(&child).expect("lineage");
	assert_eq!(
		lineage
			.iter()
			.map(|link| link.id.0.as_str())
			.collect::<Vec<_>>(),
		["root", "child"]
	);
}
