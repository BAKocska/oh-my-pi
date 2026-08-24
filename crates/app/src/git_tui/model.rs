//! Environment-backed repository model for the interactive Git workbench.

use std::{
	collections::HashMap,
	path::{Path, PathBuf},
};

use bytes::Bytes;
use omp_chat_ui::git::{
	GitArea, GitChangeKind, GitCommitInfo, GitFileContents, GitFileRow, GitPatchOp, GitSnapshot,
};
use omp_core::{IntoStr as _, Str};
use omp_envd::{
	exec::ExecHost,
	vcs::git::{
		commands::{CommandError, GitCommands},
		diff::{self, ChangeKind, DiffOptions, GitDiff, LineCount, NumstatEntry, StatusEntry},
		mutation::{
			CommitOptions, DiffLineSelection, GitMutation, GitMutationConsumer, LineRange,
			MutationError,
		},
		query::GitQuery,
		refs::{self, RefError},
		repo::{self, Repository, RepositoryError},
		runner::{GitRunError, GitRunOptions, GitRunner},
	},
};
use tokio::io::AsyncReadExt as _;
use tokio_util::sync::CancellationToken;

const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// Failures produced while reading or mutating an interactive Git repository.
#[derive(Debug, thiserror::Error)]
pub enum GitModelError {
	/// No repository contains the selected working directory.
	#[error("Not a git repository")]
	NotRepository,
	/// Repository discovery failed.
	#[error(transparent)]
	Repository(#[from] RepositoryError),
	/// A Git read command failed.
	#[error(transparent)]
	Command(#[from] CommandError),
	/// Direct HEAD resolution failed.
	#[error(transparent)]
	Reference(#[from] RefError),
	/// A Git mutation failed.
	#[error(transparent)]
	Mutation(#[from] MutationError),
	/// A raw Git invocation could not complete.
	#[error(transparent)]
	Run(#[from] GitRunError),
	/// Git rejected a raw status or revision-diff invocation.
	#[error("Git command exited with status {code}")]
	Exit {
		/// Process exit status.
		code: i32,
	},
	/// The requested revision does not resolve to a commit.
	#[error("Cannot resolve revision: {revision}")]
	RevisionMissing {
		/// User-supplied revision.
		revision: Str,
	},
	/// A worktree file could not be inspected or read.
	#[error("failed to read worktree file {path:?}")]
	WorktreeIo {
		/// File that could not be read.
		path:   PathBuf,
		/// Underlying filesystem failure.
		#[source]
		source: std::io::Error,
	},
}

/// Environment-backed state for one Git workbench.
pub struct GitModel {
	cwd:         PathBuf,
	repository:  Repository,
	runner:      GitRunner,
	diff:        GitDiff,
	query:       GitQuery,
	commands:    GitCommands,
	mutation:    GitMutation,
	pinned_sha:  Option<Str>,
	fingerprint: Option<[u8; 32]>,
	head:        Option<GitCommitInfo>,
}

impl GitModel {
	/// Discovers a repository and optionally resolves one pinned revision.
	pub async fn open(
		cwd: &Path,
		revision: Option<&str>,
		cancel: &CancellationToken,
	) -> Result<Self, GitModelError> {
		let repository = repo::discover(cwd)
			.await?
			.ok_or(GitModelError::NotRepository)?;
		let cwd = repository.worktree_root.clone();
		let runner = GitRunner::new(ExecHost::new());
		let commands = GitCommands::new(runner.clone());
		let pinned_sha = match revision {
			Some(revision) => {
				let commit_revision = format!("{revision}^{{commit}}");
				Some(
					commands
						.resolve_ref(&cwd, &commit_revision, cancel)
						.await?
						.ok_or_else(|| GitModelError::RevisionMissing { revision: revision.to_str() })?,
				)
			},
			None => None,
		};
		Ok(Self {
			cwd,
			repository: repository.clone(),
			diff: GitDiff::new(runner.clone()),
			query: GitQuery::new(runner.clone()),
			commands,
			mutation: GitMutation::new(
				runner.clone(),
				repository,
				GitMutationConsumer::InteractiveGit,
			),
			runner,
			pinned_sha,
			fingerprint: None,
			head: None,
		})
	}

	/// Returns the canonical worktree root.
	pub fn cwd(&self) -> &Path {
		&self.cwd
	}

	/// Re-reads repository state, returning `None` when its fingerprint is
	/// unchanged.
	pub async fn refresh(
		&mut self,
		cancel: &CancellationToken,
	) -> Result<Option<GitSnapshot>, GitModelError> {
		if let Some(sha) = self.pinned_sha.clone() {
			let fingerprint = fingerprint(sha.as_bytes(), &[]);
			if self.fingerprint == Some(fingerprint) {
				return Ok(None);
			}
			let head = self.load_commit(sha.as_str(), cancel).await?;
			self.fingerprint = Some(fingerprint);
			self.head = Some(head.clone());
			return Ok(Some(GitSnapshot {
				branch:   None,
				unstaged: Vec::new(),
				staged:   Vec::new(),
				head:     Some(head),
				pinned:   true,
			}));
		}

		let status_output = self
			.runner
			.run(
				&self.cwd,
				&["status", "--porcelain=v1", "-z", "--untracked-files=all"],
				GitRunOptions { read_only: true, parse_sensitive: true, ..Default::default() },
				cancel,
			)
			.await?;
		if status_output.exit_code != 0 {
			return Err(GitModelError::Exit { code: status_output.exit_code });
		}
		let entries = diff::parse_status_entries(&status_output.stdout);
		let branch = self.commands.current_branch(&self.cwd, cancel).await?;
		let head_state = refs::resolve_head(&self.repository, &self.runner, cancel).await?;
		let head_sha = head_state.commit().map(str::to_owned);
		let fingerprint =
			fingerprint(head_sha.as_deref().unwrap_or_default().as_bytes(), &status_output.stdout);
		if self.fingerprint == Some(fingerprint) {
			return Ok(None);
		}

		let (worktree_stats, staged_stats) =
			tokio::try_join!(self.numstat(false, cancel), self.numstat(true, cancel),)?;
		let (unstaged, staged) = rows_from_status(&entries, &worktree_stats, &staged_stats);
		if self.head.as_ref().map(|head| head.sha.as_str()) != head_sha.as_deref() {
			self.head = match head_sha.as_deref() {
				Some(sha) => Some(self.load_commit(sha, cancel).await?),
				None => None,
			};
		}
		self.fingerprint = Some(fingerprint);
		Ok(Some(GitSnapshot { branch, unstaged, staged, head: self.head.clone(), pinned: false }))
	}

	/// Invalidates fingerprint deduplication and returns a fresh snapshot.
	pub async fn force_refresh(
		&mut self,
		cancel: &CancellationToken,
	) -> Result<GitSnapshot, GitModelError> {
		self.fingerprint = None;
		Ok(self
			.refresh(cancel)
			.await?
			.expect("cleared fingerprint must produce a snapshot"))
	}

	/// Resolves both sides of one selected file.
	pub async fn contents(
		&self,
		area: GitArea,
		path: &str,
		orig_path: Option<&str>,
		cancel: &CancellationToken,
	) -> Result<GitFileContents, GitModelError> {
		let (old, new, too_large) = match area {
			GitArea::Unstaged => {
				let old = self.show_or_empty(&format!(":0:{path}"), cancel).await?;
				let (new, too_large) = self.read_worktree(path).await?;
				(old, new, too_large)
			},
			GitArea::Staged => {
				let old = self
					.show_or_empty(&format!("HEAD:{}", orig_path.unwrap_or(path)), cancel)
					.await?;
				let new = self.show_or_empty(&format!(":0:{path}"), cancel).await?;
				(old, new, false)
			},
			GitArea::Commit => {
				let Some(head) = self.head.as_ref() else {
					return Ok(empty_contents());
				};
				let old = match head.parents.first() {
					Some(parent) => {
						self
							.show_or_empty(&format!("{}:{}", parent, orig_path.unwrap_or(path)), cancel)
							.await?
					},
					None => Bytes::new(),
				};
				let new = self
					.show_or_empty(&format!("{}:{path}", head.sha), cancel)
					.await?;
				(old, new, false)
			},
		};
		let binary = old.contains(&0) || new.contains(&0);
		Ok(GitFileContents {
			old_text: String::from_utf8_lossy(&old).as_ref().to_str(),
			new_text: String::from_utf8_lossy(&new).as_ref().to_str(),
			binary,
			too_large,
		})
	}

	/// Stages one file, or every change when no path is supplied.
	pub async fn stage(
		&self,
		path: Option<&str>,
		cancel: &CancellationToken,
	) -> Result<Str, GitModelError> {
		match path {
			Some(path) => {
				self.mutation.stage_files(&[path], cancel).await?;
				Ok(omp_core::sf!("Staged {path}"))
			},
			None => {
				self.mutation.stage_all(cancel).await?;
				Ok(Str::new_static("Staged all changes"))
			},
		}
	}

	/// Unstages one file, or the complete index when no path is supplied.
	pub async fn unstage(
		&self,
		path: Option<&str>,
		cancel: &CancellationToken,
	) -> Result<Str, GitModelError> {
		match path {
			Some(path) => {
				self.mutation.reset_index_entries(&[path], cancel).await?;
				Ok(omp_core::sf!("Unstaged {path}"))
			},
			None => {
				self.mutation.unstage_all(cancel).await?;
				Ok(Str::new_static("Unstaged all changes"))
			},
		}
	}

	/// Applies one inclusive diff-line selection.
	pub async fn apply_lines(
		&self,
		op: GitPatchOp,
		path: &str,
		old: (u32, u32),
		new: (u32, u32),
		cancel: &CancellationToken,
	) -> Result<Str, GitModelError> {
		let selection = DiffLineSelection { old: line_range(old), new: line_range(new) };
		match op {
			GitPatchOp::Stage => {
				self.mutation.stage_lines(path, selection, cancel).await?;
				Ok(Str::new_static("Staged selection"))
			},
			GitPatchOp::Unstage => {
				self.mutation.unstage_lines(path, selection, cancel).await?;
				Ok(Str::new_static("Unstaged selection"))
			},
			GitPatchOp::Discard => {
				self.mutation.discard_lines(path, selection, cancel).await?;
				Ok(Str::new_static("Discarded selection"))
			},
		}
	}

	/// Creates or amends one commit, optionally staging every change first.
	pub async fn commit(
		&self,
		message: &str,
		amend: bool,
		stage_all: bool,
		cancel: &CancellationToken,
	) -> Result<Str, GitModelError> {
		if stage_all {
			self.mutation.stage_all(cancel).await?;
		}
		self
			.mutation
			.create_commit(message.as_bytes(), CommitOptions { amend, ..Default::default() }, cancel)
			.await?;
		Ok(Str::new_static(if amend {
			"Amended commit"
		} else {
			"Created commit"
		}))
	}

	async fn numstat(
		&self,
		cached: bool,
		cancel: &CancellationToken,
	) -> Result<Vec<NumstatEntry>, GitModelError> {
		let raw = self
			.diff
			.raw(&self.cwd, DiffOptions { cached, numstat: true, ..Default::default() }, &[], cancel)
			.await?;
		Ok(diff::parse_numstat(raw)?)
	}

	async fn load_commit(
		&self,
		sha: &str,
		cancel: &CancellationToken,
	) -> Result<GitCommitInfo, GitModelError> {
		let metadata = self.query.commit_metadata(&self.cwd, sha, cancel).await?;
		let base = metadata.parents.first().map_or(EMPTY_TREE, Str::as_str);
		let output = self
			.runner
			.run(
				&self.cwd,
				&["diff", "--numstat", "-z", base, metadata.hash.as_str()],
				GitRunOptions { read_only: true, parse_sensitive: true, ..Default::default() },
				cancel,
			)
			.await?;
		if output.exit_code != 0 {
			return Err(GitModelError::Exit { code: output.exit_code });
		}
		let files = diff::parse_numstat(output.stdout)?
			.into_iter()
			.map(commit_row)
			.collect();
		let (subject, body) = metadata
			.body
			.as_str()
			.split_once('\n')
			.unwrap_or((metadata.body.as_str(), ""));
		Ok(GitCommitInfo {
			sha: metadata.hash,
			subject: subject.to_str(),
			body: body.trim().to_str(),
			author_name: metadata.author_name,
			author_email: metadata.author_email,
			author_date: metadata.author_date,
			parents: metadata.parents,
			files,
		})
	}

	async fn show_or_empty(
		&self,
		spec: &str,
		cancel: &CancellationToken,
	) -> Result<Bytes, GitModelError> {
		match self.query.show_path(&self.cwd, spec, cancel).await {
			Ok(bytes) => Ok(bytes),
			Err(CommandError::Exit { .. }) => Ok(Bytes::new()),
			Err(error) => Err(error.into()),
		}
	}

	async fn read_worktree(&self, path: &str) -> Result<(Bytes, bool), GitModelError> {
		let full_path = self.cwd.join(path);
		let mut file = match tokio::fs::File::open(&full_path).await {
			Ok(file) => file,
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
				return Ok((Bytes::new(), false));
			},
			Err(source) => return Err(GitModelError::WorktreeIo { path: full_path, source }),
		};
		let mut bytes = Vec::with_capacity(usize::try_from(MAX_FILE_BYTES).unwrap_or_default());
		let mut limited = (&mut file).take(MAX_FILE_BYTES + 1);
		limited
			.read_to_end(&mut bytes)
			.await
			.map_err(|source| GitModelError::WorktreeIo { path: full_path, source })?;
		if bytes.len() > usize::try_from(MAX_FILE_BYTES).unwrap_or(usize::MAX) {
			return Ok((Bytes::new(), true));
		}
		Ok((Bytes::from(bytes), false))
	}
}

fn fingerprint(head: &[u8], status: &[u8]) -> [u8; 32] {
	let mut hasher = blake3::Hasher::new();
	hasher.update(head);
	hasher.update(&[0]);
	hasher.update(status);
	*hasher.finalize().as_bytes()
}

fn line_range((start, end): (u32, u32)) -> Option<LineRange> {
	(start != 0 || end != 0).then(|| LineRange::new(u64::from(start), u64::from(end)))
}

fn empty_contents() -> GitFileContents {
	GitFileContents {
		old_text:  Str::new_static(""),
		new_text:  Str::new_static(""),
		binary:    false,
		too_large: false,
	}
}

fn rows_from_status(
	entries: &[StatusEntry],
	worktree_stats: &[NumstatEntry],
	staged_stats: &[NumstatEntry],
) -> (Vec<GitFileRow>, Vec<GitFileRow>) {
	let worktree_counts = count_map(worktree_stats);
	let staged_counts = count_map(staged_stats);
	let mut unstaged = Vec::new();
	let mut staged = Vec::new();
	for entry in entries {
		let path = lossy(entry.path.as_bytes());
		let orig_path = entry.orig_path.as_ref().map(|path| lossy(path.as_bytes()));
		if entry.untracked {
			unstaged.push(row(path, None, GitChangeKind::Untracked, GitArea::Unstaged, None));
			continue;
		}
		if entry.conflicted {
			unstaged.push(row(path, None, GitChangeKind::Conflicted, GitArea::Unstaged, None));
			continue;
		}
		if let Some(kind) = entry.staged {
			staged.push(row(
				path.clone(),
				orig_path,
				change_kind(kind),
				GitArea::Staged,
				staged_counts.get(entry.path.as_bytes()).copied(),
			));
		}
		if let Some(kind) = entry.worktree {
			unstaged.push(row(
				path,
				None,
				change_kind(kind),
				GitArea::Unstaged,
				worktree_counts.get(entry.path.as_bytes()).copied(),
			));
		}
	}
	(unstaged, staged)
}

fn count_map(entries: &[NumstatEntry]) -> HashMap<&[u8], (Option<u64>, Option<u64>)> {
	entries
		.iter()
		.map(|entry| (entry.path.as_bytes(), (line_count(entry.added), line_count(entry.removed))))
		.collect()
}

fn row(
	path: Str,
	orig_path: Option<Str>,
	kind: GitChangeKind,
	area: GitArea,
	counts: Option<(Option<u64>, Option<u64>)>,
) -> GitFileRow {
	let (additions, deletions) = counts.unwrap_or((None, None));
	GitFileRow { path, orig_path, kind, area, additions, deletions }
}

fn commit_row(entry: NumstatEntry) -> GitFileRow {
	let additions = line_count(entry.added);
	let deletions = line_count(entry.removed);
	let kind = if additions.unwrap_or_default() > 0 && deletions == Some(0) {
		GitChangeKind::Added
	} else {
		GitChangeKind::Modified
	};
	GitFileRow {
		path: lossy(entry.path.as_bytes()),
		orig_path: entry.old_path.map(|path| lossy(path.as_bytes())),
		kind,
		area: GitArea::Commit,
		additions,
		deletions,
	}
}

fn line_count(count: LineCount) -> Option<u64> {
	match count {
		LineCount::Lines(lines) => Some(lines),
		LineCount::Binary => None,
	}
}

fn change_kind(kind: ChangeKind) -> GitChangeKind {
	match kind {
		ChangeKind::Added => GitChangeKind::Added,
		ChangeKind::Deleted => GitChangeKind::Deleted,
		ChangeKind::Renamed | ChangeKind::Copied => GitChangeKind::Renamed,
		ChangeKind::Unmerged => GitChangeKind::Conflicted,
		ChangeKind::Modified | ChangeKind::TypeChanged => GitChangeKind::Modified,
	}
}

fn lossy(bytes: &[u8]) -> Str {
	String::from_utf8_lossy(bytes).as_ref().to_str()
}

#[cfg(test)]
mod tests {
	use std::{fs, process::Command};

	use omp_envd::vcs::git::diff::parse_status_entries;

	use super::*;

	#[test]
	fn status_rows_preserve_git_areas_conflicts_renames_and_counts() {
		let entries = parse_status_entries(
			b"M  staged.txt\0 M work.txt\0?? new.txt\0UU conflict.txt\0R  renamed.txt\0old.txt\0",
		);
		let worktree = diff::parse_numstat(Bytes::from_static(b"3\t1\twork.txt\0")).unwrap();
		let staged_stats = diff::parse_numstat(Bytes::from_static(
			b"2\t0\tstaged.txt\x000\t0\t\x00old.txt\x00renamed.txt\x00",
		))
		.unwrap();
		let (unstaged, staged) = rows_from_status(&entries, &worktree, &staged_stats);
		assert_eq!(unstaged.len(), 3);
		assert_eq!(unstaged[0].additions, Some(3));
		assert_eq!(unstaged[1].kind, GitChangeKind::Untracked);
		assert_eq!(unstaged[2].kind, GitChangeKind::Conflicted);
		assert_eq!(staged.len(), 2);
		assert_eq!(staged[1].kind, GitChangeKind::Renamed);
		assert_eq!(staged[1].orig_path.as_deref(), Some("old.txt"));
	}
	fn fixture_git(cwd: &Path, arguments: &[&str]) {
		let output = Command::new("git")
			.current_dir(cwd)
			.args(arguments)
			.output()
			.expect("fixture git should launch");
		assert!(
			output.status.success(),
			"fixture git {arguments:?} failed: {}",
			String::from_utf8_lossy(&output.stderr)
		);
	}

	#[tokio::test]
	async fn real_repository_refresh_deduplicates_until_status_changes() {
		let fixture = tempfile::tempdir().expect("temporary repository");
		fixture_git(fixture.path(), &["init", "-b", "main"]);
		fixture_git(fixture.path(), &["config", "user.name", "OMP Test"]);
		fixture_git(fixture.path(), &["config", "user.email", "omp@example.invalid"]);
		fs::write(fixture.path().join("tracked.txt"), "first\n").expect("seed file");
		fixture_git(fixture.path(), &["add", "tracked.txt"]);
		fixture_git(fixture.path(), &["commit", "-m", "seed"]);

		let cancel = CancellationToken::new();
		let mut model = GitModel::open(fixture.path(), None, &cancel).await.unwrap();
		let initial = model
			.refresh(&cancel)
			.await
			.unwrap()
			.expect("initial snapshot");
		assert_eq!(initial.branch.as_deref(), Some("main"));
		assert!(initial.unstaged.is_empty());
		assert!(initial.staged.is_empty());
		assert!(model.refresh(&cancel).await.unwrap().is_none());

		fs::write(fixture.path().join("tracked.txt"), "first\nsecond\n").expect("changed file");
		let changed = model
			.refresh(&cancel)
			.await
			.unwrap()
			.expect("changed snapshot");
		assert_eq!(changed.unstaged.len(), 1);
		assert_eq!(changed.unstaged[0].path.as_str(), "tracked.txt");
		assert_eq!(changed.unstaged[0].additions, Some(1));
		assert!(model.refresh(&cancel).await.unwrap().is_none());
	}

	#[test]
	fn fingerprint_distinguishes_head_and_raw_status_but_deduplicates_exact_input() {
		let first = fingerprint(b"abc", b" M file\0");
		assert_eq!(first, fingerprint(b"abc", b" M file\0"));
		assert_ne!(first, fingerprint(b"def", b" M file\0"));
		assert_ne!(first, fingerprint(b"abc", b"M  file\0"));
	}
}
