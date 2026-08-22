use std::{path::Path, process::Command, time::Duration};

use bytes::Bytes;
use tokio_util::sync::CancellationToken;

use super::{
	commands::GitCommands,
	diff::{self, GitDiff, LineCount, StatusCounts},
	lock,
	query::GitQuery,
	refs::{self, HeadInvalidations, HeadState},
	repo::{self, RepositoryError},
	runner::{
		CappedOutput, GitDiagnostic, GitRunError, GitRunOptions, GitRunner, OUTPUT_LIMIT,
		TRUNCATION_MARKER, command_source, git_environment,
	},
};
use crate::envd::{
	exec::ExecHost,
	vcs::{self, RepositoryAvailability},
};

fn fixture_git(cwd: &Path, arguments: &[&str]) {
	let output = Command::new("git")
		.current_dir(cwd)
		.args(arguments)
		.env("GIT_TERMINAL_PROMPT", "0")
		.output()
		.expect("fixture git should launch");
	assert!(
		output.status.success(),
		"fixture git {arguments:?} failed: {}",
		String::from_utf8_lossy(&output.stderr)
	);
}

fn repository_fixture() -> tempfile::TempDir {
	let root = tempfile::tempdir().expect("temporary repository root");
	fixture_git(root.path(), &["init", "-b", "main"]);
	fixture_git(root.path(), &["config", "user.name", "OMP Test"]);
	fixture_git(root.path(), &["config", "user.email", "omp@example.invalid"]);
	std::fs::write(root.path().join("seed.txt"), "seed\n").expect("seed file");
	fixture_git(root.path(), &["add", "seed.txt"]);
	fixture_git(root.path(), &["commit", "-m", "seed"]);
	root
}

#[tokio::test]
async fn linked_worktrees_share_primary_root_and_fair_operation_lock() {
	let fixture = repository_fixture();
	let linked = fixture.path().parent().expect("temp parent").join(format!(
		"{}-linked",
		fixture
			.path()
			.file_name()
			.expect("temp basename")
			.to_string_lossy()
	));
	fixture_git(fixture.path(), &[
		"worktree",
		"add",
		"-b",
		"linked",
		linked.to_str().expect("UTF-8 fixture path"),
	]);
	let primary = repo::discover(fixture.path())
		.await
		.expect("primary discovery")
		.expect("primary repository");
	let linked_repository = repo::discover(&linked)
		.await
		.expect("linked discovery")
		.expect("linked repository");
	assert_ne!(primary.worktree_root, linked_repository.worktree_root);
	assert_eq!(primary.primary_root, linked_repository.primary_root);
	assert_eq!(primary.common_dir, linked_repository.common_dir);

	let cancel = CancellationToken::new();
	let first_read = lock::read(&primary, &cancel)
		.await
		.expect("first bounded read");
	let second_read =
		tokio::time::timeout(Duration::from_millis(100), lock::read(&linked_repository, &cancel))
			.await
			.expect("bounded reads should overlap")
			.expect("second bounded read");
	drop((first_read, second_read));

	let writer = lock::write(&primary, &cancel).await.expect("first writer");
	let (acquired_tx, acquired_rx) = flume::bounded(1);
	let linked_for_task = linked_repository.clone();
	let queued_cancel = CancellationToken::new();
	let queued = tokio::spawn(async move {
		let guard = lock::write(&linked_for_task, &queued_cancel)
			.await
			.expect("queued writer");
		acquired_tx
			.send_async(())
			.await
			.expect("acquisition signal");
		guard
	});
	assert!(
		tokio::time::timeout(Duration::from_millis(75), acquired_rx.recv_async())
			.await
			.is_err(),
		"linked-worktree writer must wait for the primary writer"
	);
	drop(writer);
	tokio::time::timeout(Duration::from_secs(1), acquired_rx.recv_async())
		.await
		.expect("queued writer should acquire after release")
		.expect("acquisition signal");
	drop(queued.await.expect("queued writer task"));

	let writer = lock::write(&primary, &cancel)
		.await
		.expect("writer for cancellation");
	let cancelled = CancellationToken::new();
	let waiting = lock::write(&linked_repository, &cancelled);
	tokio::pin!(waiting);
	cancelled.cancel();
	assert!(matches!(waiting.await, Err(lock::LockError::Cancelled)));
	drop(writer);
	fixture_git(fixture.path(), &["worktree", "remove", "--force", linked.to_str().unwrap()]);
}

#[tokio::test]
async fn malformed_git_and_escaping_commondir_pointers_are_rejected() {
	let fixture = tempfile::tempdir().expect("temporary pointer fixture");
	std::fs::write(fixture.path().join(".git"), "gitdir: one\ntrailing\n")
		.expect("malformed marker");
	assert!(matches!(
		repo::discover(fixture.path()).await,
		Err(RepositoryError::InvalidPointer { .. })
	));

	let linked = tempfile::tempdir().expect("linked pointer fixture");
	let admin = linked.path().join("admin");
	let escaped = linked.path().join("escaped");
	std::fs::create_dir_all(&admin).expect("admin directory");
	std::fs::create_dir_all(&escaped).expect("escaped directory");
	std::fs::write(admin.join("HEAD"), "ref: refs/heads/main\n").expect("admin HEAD");
	std::fs::write(escaped.join("HEAD"), "ref: refs/heads/main\n").expect("escaped HEAD");
	std::fs::write(admin.join("commondir"), "../escaped\n").expect("escaping commondir");
	std::fs::write(linked.path().join(".git"), "gitdir: admin\n").expect("gitdir pointer");
	assert!(matches!(
		repo::discover(linked.path()).await,
		Err(RepositoryError::InvalidPointer { .. })
	));
}

#[test]
fn runner_builds_fixed_read_only_argv_and_sanitized_environment() {
	let source = command_source("git", &["status", "a'; echo injected"], true);
	assert!(source.contains("'core.fsmonitor=false'"));
	assert!(source.contains("'core.untrackedCache=false'"));
	assert!(source.contains("'--no-optional-locks'"));
	assert!(source.contains("'a'\"'\"'; echo injected'"));
	let environment = git_environment();
	for name in [
		"GIT_DIR",
		"GIT_COMMON_DIR",
		"GIT_WORK_TREE",
		"GIT_INDEX_FILE",
		"GIT_OBJECT_DIRECTORY",
		"GIT_ALTERNATE_OBJECT_DIRECTORIES",
	] {
		assert!(environment.unset.iter().any(|unset| unset == name), "{name} must be removed");
	}
	assert_eq!(
		environment
			.set
			.get("GIT_OPTIONAL_LOCKS")
			.map(String::as_str),
		Some("0")
	);
	assert_eq!(environment.set.get("GIT_ASKPASS").map(String::as_str), Some("true"));
	assert_eq!(environment.set.get("GIT_EDITOR").map(String::as_str), Some("true"));
	assert_eq!(
		environment
			.set
			.get("GIT_TERMINAL_PROMPT")
			.map(String::as_str),
		Some("0")
	);
	assert_eq!(environment.set.get("LC_MESSAGES").map(String::as_str), Some("C"));
	assert_eq!(environment.set.get("LC_CTYPE").map(String::as_str), Some("C.UTF-8"));
}

#[tokio::test]
async fn runner_preserves_utf8_names_and_reports_missing_git_and_deleted_cwd() {
	let fixture = repository_fixture();
	std::fs::write(fixture.path().join("café.txt"), "coffee\n").expect("UTF-8 filename");
	fixture_git(fixture.path(), &["add", "café.txt"]);
	let runner = GitRunner::new(ExecHost::new());
	let cancel = CancellationToken::new();
	let environment = runner
		.run(
			fixture.path(),
			&["-c", "alias.dump=!env", "dump"],
			GitRunOptions { read_only: true, parse_sensitive: true, ..Default::default() },
			&cancel,
		)
		.await
		.expect("environment probe should run");
	let environment = String::from_utf8(environment.stdout.to_vec()).expect("environment is UTF-8");
	for pin in [
		"GIT_OPTIONAL_LOCKS=0",
		"GIT_ASKPASS=true",
		"GIT_EDITOR=true",
		"GIT_TERMINAL_PROMPT=0",
		"LC_MESSAGES=C",
		"LC_CTYPE=C.UTF-8",
	] {
		assert!(environment.lines().any(|line| line == pin), "missing environment pin {pin}");
	}
	let editor = runner
		.run(
			fixture.path(),
			&["var", "GIT_EDITOR"],
			GitRunOptions { read_only: true, parse_sensitive: true, ..Default::default() },
			&cancel,
		)
		.await
		.expect("editor probe should run");
	assert_eq!(editor.stdout.as_ref(), b"true\n");

	let listed = runner
		.run(
			fixture.path(),
			&["ls-files", "-z"],
			GitRunOptions { read_only: true, parse_sensitive: true, ..Default::default() },
			&cancel,
		)
		.await
		.expect("ls-files should run");
	assert_eq!(listed.exit_code, 0);
	assert!(
		listed
			.stdout
			.windows("café.txt".len())
			.any(|window| window == "café.txt".as_bytes())
	);

	let missing = runner
		.run_binary(fixture.path(), "omp-git-does-not-exist", &[], GitRunOptions::default(), &cancel)
		.await
		.expect("missing Git is a typed 127 result");
	assert_eq!(missing.exit_code, 127);
	assert_eq!(missing.diagnostic, Some(GitDiagnostic::GitMissing));

	let deleted = tempfile::tempdir().expect("deleted cwd fixture");
	let deleted_path = deleted.path().to_path_buf();
	drop(deleted);
	assert!(matches!(
		runner
			.run(&deleted_path, &["status"], GitRunOptions::default(), &cancel)
			.await,
		Err(GitRunError::DeletedWorkingDirectory { .. })
	));
}

#[tokio::test]
async fn runner_caps_each_stream_rejects_incomplete_output_and_cancels_process_group() {
	let fixture = repository_fixture();
	let runner = GitRunner::new(ExecHost::new());
	let cancel = CancellationToken::new();
	let oversized_alias = "alias.big=!dd if=/dev/zero bs=1048576 count=9 2>/dev/null";
	let oversized = runner
		.run(fixture.path(), &["-c", oversized_alias, "big"], GitRunOptions::default(), &cancel)
		.await
		.expect("oversized output should return a marked bounded result");
	assert!(oversized.stdout_truncated);
	assert_eq!(oversized.stdout.len(), OUTPUT_LIMIT + TRUNCATION_MARKER.len());
	assert!(oversized.stdout.ends_with(TRUNCATION_MARKER));
	assert!(!oversized.stderr_truncated);
	assert!(matches!(
		runner
			.run(
				fixture.path(),
				&["-c", oversized_alias, "big"],
				GitRunOptions { parse_sensitive: true, ..Default::default() },
				&cancel,
			)
			.await,
		Err(GitRunError::Incomplete { stdout: true, stderr: false })
	));

	let slow_cancel = CancellationToken::new();
	let cancel_trigger = slow_cancel.clone();
	tokio::spawn(async move {
		tokio::time::sleep(Duration::from_millis(50)).await;
		cancel_trigger.cancel();
	});
	let cancelled = tokio::time::timeout(
		Duration::from_secs(3),
		runner.run(
			fixture.path(),
			&["-c", "alias.slow=!sleep 30", "slow"],
			GitRunOptions::default(),
			&slow_cancel,
		),
	)
	.await
	.expect("TERM-to-KILL cancellation must not leave the child alive");
	assert!(matches!(cancelled, Err(GitRunError::Cancelled)));
}

#[test]
fn capped_output_marks_only_the_stream_that_overflows() {
	let mut output = CappedOutput::new();
	output.push(&vec![b'x'; OUTPUT_LIMIT + 1]);
	assert!(output.truncated);
	assert!(output.finish().ends_with(TRUNCATION_MARKER));
}
#[test]
fn vcs_parsers_preserve_porcelain_renames_binary_and_terminal_newlines() {
	assert_eq!(diff::parse_status(b"M  staged\n M unstaged\n?? untracked\n"), StatusCounts {
		staged:    1,
		unstaged:  1,
		untracked: 1,
	});
	assert_eq!(
		diff::parse_status(
			b"1 M. N... 100644 100644 100644 a a tracked\0? odd\nname\02 R. N... 100644 100644 100644 a a R100 new\0old\0"
		),
		StatusCounts { staged: 2, unstaged: 0, untracked: 1 }
	);

	let numstat = diff::parse_numstat(Bytes::from_static(
		b"3\t2\tplain\0-\t-\tbin\01\t0\t\0old name\0new name\0",
	))
	.expect("NUL numstat");
	assert_eq!(numstat.len(), 3);
	assert_eq!(numstat[0].added, LineCount::Lines(3));
	assert_eq!(numstat[1].added, LineCount::Binary);
	assert_eq!(
		numstat[2]
			.old_path
			.as_ref()
			.expect("rename old path")
			.as_bytes(),
		b"old name"
	);
	assert_eq!(numstat[2].path.as_bytes(), b"new name");

	let raw = Bytes::from_static(
		b"diff --git a/old b/new\nsimilarity index 90%\nrename from old\nrename to new\n--- a/old\n+++ b/new\n@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file\ndiff --git a/image.bin b/image.bin\nnew file mode 100644\nindex 0000000..1111111\nGIT binary patch\nliteral 1\nA\n",
	);
	let parsed = diff::parse_unified(raw.clone());
	assert_eq!(parsed.len(), 2);
	assert_eq!(parsed[0].old_path.as_deref(), Some(b"old".as_slice()));
	assert_eq!(parsed[0].path.as_deref(), Some(b"new".as_slice()));
	assert!(parsed[0].old_no_final_newline);
	assert!(parsed[0].new_no_final_newline);
	assert_eq!(parsed[0].hunks.len(), 1);
	assert!(parsed[1].binary);
	assert_eq!(
		parsed.iter().map(|file| file.raw.len()).sum::<usize>(),
		raw.len(),
		"file patches must retain every input byte"
	);
}

#[tokio::test]
async fn vcs_snapshots_cover_normal_linked_bare_detached_unborn_packed_and_reftable() {
	let runner = GitRunner::new(ExecHost::new());
	let cancel = CancellationToken::new();
	let fixture = repository_fixture();
	let normal = vcs::snapshot(fixture.path(), &runner, &cancel)
		.await
		.expect("normal snapshot");
	assert_eq!(normal.availability, RepositoryAvailability::Available);
	assert_eq!(normal.branch.as_deref(), Some("main"));
	assert!(normal.head.is_some());
	assert_eq!(normal.status_counts, StatusCounts::default());

	fixture_git(fixture.path(), &["checkout", "--detach", "HEAD"]);
	let repository = repo::discover(fixture.path()).await.unwrap().unwrap();
	assert!(matches!(
		refs::resolve_head(&repository, &runner, &cancel)
			.await
			.unwrap(),
		HeadState::Detached { .. }
	));
	fixture_git(fixture.path(), &["checkout", "main"]);
	fixture_git(fixture.path(), &["pack-refs", "--all", "--prune"]);
	assert!(!repository.common_dir.join("refs/heads/main").exists());
	assert!(matches!(
		refs::resolve_head(&repository, &runner, &cancel).await.unwrap(),
		HeadState::Branch { branch: Some(branch), .. } if branch == "main"
	));

	let unborn = tempfile::tempdir().expect("unborn fixture");
	fixture_git(unborn.path(), &["init", "-b", "fresh"]);
	let unborn_repository = repo::discover(unborn.path()).await.unwrap().unwrap();
	assert!(matches!(
		refs::resolve_head(&unborn_repository, &runner, &cancel).await.unwrap(),
		HeadState::Unborn { branch: Some(branch), .. } if branch == "fresh"
	));

	let linked = fixture.path().with_extension("linked-vcs-p2");
	fixture_git(fixture.path(), &["worktree", "add", "-b", "linked-p2", linked.to_str().unwrap()]);
	let linked_snapshot = vcs::snapshot(&linked, &runner, &cancel).await.unwrap();
	assert_eq!(linked_snapshot.branch.as_deref(), Some("linked-p2"));
	assert_eq!(linked_snapshot.primary_root, normal.primary_root);
	assert_ne!(linked_snapshot.worktree_root, normal.worktree_root);
	fixture_git(fixture.path(), &["worktree", "remove", "--force", linked.to_str().unwrap()]);

	let bare_parent = tempfile::tempdir().expect("bare parent");
	let bare = bare_parent.path().join("fixture.git");
	fixture_git(bare_parent.path(), &[
		"clone",
		"--bare",
		fixture.path().to_str().unwrap(),
		bare.to_str().unwrap(),
	]);
	let bare_snapshot = vcs::snapshot(&bare, &runner, &cancel).await.unwrap();
	assert_eq!(bare_snapshot.availability, RepositoryAvailability::Available);
	assert_eq!(bare_snapshot.worktree_root.as_deref(), Some(bare.as_path()));
	assert_eq!(bare_snapshot.primary_root.as_deref(), Some(bare.as_path()));
	assert_eq!(bare_snapshot.status_counts, StatusCounts::default());

	let reftable = tempfile::tempdir().expect("reftable fixture");
	fixture_git(reftable.path(), &["init", "--ref-format=reftable", "-b", "table"]);
	fixture_git(reftable.path(), &["config", "user.name", "OMP Test"]);
	fixture_git(reftable.path(), &["config", "user.email", "omp@example.invalid"]);
	std::fs::write(reftable.path().join("seed"), "table\n").unwrap();
	fixture_git(reftable.path(), &["add", "seed"]);
	fixture_git(reftable.path(), &["commit", "-m", "reftable"]);
	let reftable_repository = repo::discover(reftable.path()).await.unwrap().unwrap();
	assert!(refs::is_reftable(&reftable_repository).await.unwrap());
	assert!(matches!(
		refs::resolve_head(&reftable_repository, &runner, &cancel).await.unwrap(),
		HeadState::Branch { branch: Some(branch), .. } if branch == "table"
	));
}

#[tokio::test]
async fn vcs_head_poll_survives_atomic_replacement_and_coalesces_invalidations() {
	let fixture = repository_fixture();
	let repository = repo::discover(fixture.path()).await.unwrap().unwrap();
	let invalidations = HeadInvalidations::start(&repository).await.unwrap();
	tokio::time::sleep(Duration::from_millis(300)).await;
	let head = repository.git_dir.join("HEAD");
	let replacement = repository.git_dir.join("HEAD.omp-replacement");
	std::fs::write(&replacement, "ref: refs/heads/main\n").unwrap();
	std::fs::rename(&replacement, &head).unwrap();
	tokio::time::timeout(Duration::from_secs(2), invalidations.changed())
		.await
		.expect("atomic replacement invalidation")
		.expect("watch remains live");
	assert!(
		tokio::time::timeout(Duration::from_millis(500), invalidations.changed())
			.await
			.is_err(),
		"one atomic replacement must debounce to one pending invalidation"
	);
}

#[tokio::test]
async fn vcs_commands_queries_and_diff_round_trip_real_repository_bytes() {
	let fixture = repository_fixture();
	let repository = repo::discover(fixture.path()).await.unwrap().unwrap();
	let runner = GitRunner::new(ExecHost::new());
	let commands = GitCommands::new(runner.clone());
	let query = GitQuery::new(runner.clone());
	let diffs = GitDiff::new(runner);
	let cancel = CancellationToken::new();

	commands
		.config_set(&repository, "omp.fixture", "yes", &cancel)
		.await
		.unwrap();
	assert_eq!(
		commands
			.config_get(fixture.path(), "omp.fixture", &cancel)
			.await
			.unwrap()
			.as_deref(),
		Some("yes")
	);
	commands
		.create_branch(&repository, "topic", "HEAD", &cancel)
		.await
		.unwrap();
	commands
		.checkout(&repository, "topic", &cancel)
		.await
		.unwrap();
	assert_eq!(
		commands
			.current_branch(fixture.path(), &cancel)
			.await
			.unwrap()
			.as_deref(),
		Some("topic")
	);
	commands
		.checkout(&repository, "main", &cancel)
		.await
		.unwrap();
	commands
		.delete_branch(&repository, "topic", true, &cancel)
		.await
		.unwrap();
	assert!(
		commands
			.list_branches(fixture.path(), false, &cancel)
			.await
			.unwrap()
			.iter()
			.any(|b| b == "main")
	);

	let local_url = fixture.path().to_str().unwrap();
	commands
		.add_remote(&repository, "origin", local_url, &cancel)
		.await
		.unwrap();
	commands
		.add_remote(&repository, "origin", local_url, &cancel)
		.await
		.unwrap();
	assert_eq!(
		commands
			.remote_url(fixture.path(), "origin", &cancel)
			.await
			.unwrap()
			.as_deref(),
		Some(local_url)
	);
	commands
		.fetch_refspec(&repository, "origin", "refs/heads/main", "refs/remotes/origin/main", &cancel)
		.await
		.unwrap();
	fixture_git(fixture.path(), &[
		"symbolic-ref",
		"refs/remotes/origin/HEAD",
		"refs/remotes/origin/main",
	]);
	assert_eq!(
		commands
			.default_branch(fixture.path(), &cancel)
			.await
			.unwrap()
			.as_deref(),
		Some("main")
	);
	assert!(
		commands
			.ref_exists(fixture.path(), "refs/heads/main", &cancel)
			.await
			.unwrap()
	);
	assert!(
		commands
			.resolve_ref(fixture.path(), "HEAD", &cancel)
			.await
			.unwrap()
			.is_some()
	);
	fixture_git(fixture.path(), &["tag", "v1.9"]);
	fixture_git(fixture.path(), &["tag", "v1.10"]);
	assert_eq!(
		commands
			.tags(fixture.path(), "HEAD", &cancel)
			.await
			.unwrap()[0]
			.as_str(),
		"v1.10"
	);
	std::fs::create_dir(fixture.path().join("nested")).unwrap();
	assert_eq!(
		commands
			.workdir_prefix(&fixture.path().join("nested"), &cancel)
			.await
			.unwrap()
			.as_deref(),
		Some("nested/")
	);

	let odd = "odd\nname.txt";
	std::fs::write(fixture.path().join(odd), "odd\n").unwrap();
	fixture_git(fixture.path(), &["add", odd]);
	let head = commands
		.resolve_ref(fixture.path(), "HEAD", &cancel)
		.await
		.unwrap()
		.unwrap();
	let cache = format!("160000,{head},deps/sub");
	fixture_git(fixture.path(), &["update-index", "--add", "--cacheinfo", cache.as_str()]);
	let tracked = query.tracked(fixture.path(), &cancel).await.unwrap();
	assert!(tracked.iter().any(|path| path.as_bytes() == odd.as_bytes()));
	assert!(
		query
			.submodules(fixture.path(), &cancel)
			.await
			.unwrap()
			.iter()
			.any(|path| path.as_bytes() == b"deps/sub")
	);
	assert!(
		query
			.tree(fixture.path(), "HEAD", &[], &cancel)
			.await
			.unwrap()
			.iter()
			.any(|path| path.as_bytes() == b"seed.txt")
	);
	assert_eq!(
		query
			.log_subjects(fixture.path(), 1, &cancel)
			.await
			.unwrap()[0]
			.as_str(),
		"seed"
	);
	assert_eq!(
		query
			.log_onelines(fixture.path(), 1, &cancel)
			.await
			.unwrap()
			.len(),
		1
	);
	assert!(
		query
			.rev_list_range(fixture.path(), &head, &head, &cancel)
			.await
			.unwrap()
			.is_empty()
	);
	assert_eq!(
		query
			.rev_list_touching(fixture.path(), "HEAD", "seed.txt", 1, &cancel)
			.await
			.unwrap()
			.len(),
		1
	);
	let metadata = query
		.commit_metadata(fixture.path(), "HEAD", &cancel)
		.await
		.unwrap();
	assert_eq!(metadata.author_name.as_str(), "OMP Test");
	assert!(metadata.body.as_str().starts_with("seed"));

	std::fs::write(fixture.path().join("seed.txt"), "changed without newline").unwrap();
	std::fs::write(fixture.path().join("untracked.bin"), [0, 1, 2, 0xff]).unwrap();
	let counts = diffs.status_counts(fixture.path(), &cancel).await.unwrap();
	assert!(counts.staged >= 2);
	assert_eq!(counts.unstaged, 1);
	assert_eq!(counts.untracked, 1);
	let raw = diffs
		.raw(fixture.path(), Default::default(), &[], &cancel)
		.await
		.unwrap();
	let parsed = diff::parse_unified(raw.clone());
	assert_eq!(parsed.len(), 1);
	assert!(parsed[0].new_no_final_newline);
	assert_eq!(parsed[0].raw, raw);
	assert!(diffs.has(fixture.path(), false, &cancel).await.unwrap());
	assert!(
		diffs
			.names(fixture.path(), false, &cancel)
			.await
			.unwrap()
			.iter()
			.any(|path| path.as_bytes() == b"seed.txt")
	);
	let numstat = diffs
		.raw(
			fixture.path(),
			diff::DiffOptions { cached: true, numstat: true, ..Default::default() },
			&[],
			&cancel,
		)
		.await
		.unwrap();
	assert!(!diff::parse_numstat(numstat).unwrap().is_empty());
}
