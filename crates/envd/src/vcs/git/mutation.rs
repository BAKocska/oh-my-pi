//! Consumer-facing Git mutation primitives serialized by primary repository
//! root.

use std::{collections::HashSet, fmt::Write as _, io, path::Path, str};

use bytes::{Bytes, BytesMut};
use omp_core::{IntoStr, Str};
use tokio_util::sync::CancellationToken;

use super::{
	commands::CommandError,
	diff,
	diff::{DiffHunk, DiffOptions, FileDiff, GitDiff},
	lock,
	repo::Repository,
	runner::{GitDeadline, GitRunError, GitRunOptions, GitRunOutput, GitRunner},
};

/// A validated subset of one file's diff hunks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HunkSelector {
	/// Select the complete file patch, including a binary patch body.
	All,
	/// Select one-based hunk indices.
	Indices(Box<[usize]>),
	/// Select hunks intersecting this inclusive new-file line range.
	Lines {
		/// First selected new-file line, inclusive.
		start: u64,
		/// Last selected new-file line, inclusive.
		end:   u64,
	},
}

/// Hunk selection for one exact repository-relative path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HunkSelection {
	/// Exact path as emitted by the complete Git diff.
	pub path:     Str,
	/// Hunk subset to stage.
	pub selector: HunkSelector,
}

/// Inclusive one-based line range on one side of a unified diff.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LineRange {
	/// First selected line, inclusive.
	pub start: u64,
	/// Last selected line, inclusive.
	pub end:   u64,
}

impl LineRange {
	/// Creates an inclusive one-based line range.
	pub const fn new(start: u64, end: u64) -> Self {
		Self { start, end }
	}

	fn contains(self, line: u64) -> bool {
		self.start <= line && line <= self.end
	}

	fn is_valid(self) -> bool {
		self.start != 0 && self.start <= self.end
	}
}

/// Selected old-side deletions and new-side additions for a partial patch.
///
/// At least one side must be present. Supplying both sides allows one visual
/// selection to include removed and added lines from a replacement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiffLineSelection {
	/// Old-file line range selecting `-` records.
	pub old: Option<LineRange>,
	/// New-file line range selecting `+` records.
	pub new: Option<LineRange>,
}

impl DiffLineSelection {
	/// Selects only additions in an inclusive new-file range.
	pub const fn new_lines(start: u64, end: u64) -> Self {
		Self { old: None, new: Some(LineRange::new(start, end)) }
	}
}
/// Direction in which a synthesized line patch will be applied.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinePatchDirection {
	/// Apply the patch from its old side to its new side.
	Apply,
	/// Apply the patch through `git apply --reverse`.
	Reverse,
}

/// Why a selective-hunk request was rejected before mutation.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SelectionError {
	/// No file patch had the requested path.
	#[error("no complete diff exists for path {path}")]
	PathMissing {
		/// Requested path.
		path: Str,
	},
	/// A path appeared more than once in one request.
	#[error("path {path} has duplicate hunk selections")]
	DuplicatePath {
		/// Duplicated path.
		path: Str,
	},
	/// Binary patches can only be selected as a whole.
	#[error("binary path {path} does not support selective hunks")]
	BinarySubset {
		/// Binary path.
		path: Str,
	},
	/// A one-based hunk index was zero or exceeded the complete file diff.
	#[error("path {path} requested hunk {index}, but the diff has {hunk_count} hunks")]
	InvalidHunkIndex {
		/// Requested path.
		path:       Str,
		/// Invalid one-based index.
		index:      usize,
		/// Complete hunk count for the file.
		hunk_count: usize,
	},
	/// A line range was empty or started at line zero.
	#[error("path {path} has an invalid line range")]
	InvalidLineRange {
		/// Requested path.
		path: Str,
	},
	/// None of the file's hunks matched the selector.
	#[error("no hunks matched path {path}")]
	NoMatchingHunks {
		/// Requested path.
		path: Str,
	},
	/// None of the selected line coordinates referred to a changed line.
	#[error("no changed lines matched path {path}")]
	NoMatchingLines {
		/// Requested path.
		path: Str,
	},
}

/// Failure before Git can return an exact mutation outcome.
#[derive(Debug, thiserror::Error)]
pub enum MutationError {
	/// Repository admission failed.
	#[error(transparent)]
	Lock(#[from] lock::LockError),
	/// A selected worktree file could not be read exactly.
	#[error("selected worktree file could not be read")]
	WorktreeRead(#[source] io::Error),
	/// Environment execution failed, timed out, or was cancelled.
	#[error(transparent)]
	Run(#[from] GitRunError),
	/// Selective staging was invalid against the captured complete diff.
	#[error(transparent)]
	Selection(#[from] SelectionError),
	/// Complete diff capture was rejected by Git.
	#[error(transparent)]
	Diff(#[from] CommandError),
	/// Git emitted a non-UTF-8 scalar where its plumbing contract requires text.
	#[error("Git emitted a non-UTF-8 scalar")]
	NonUtf8,
	/// The caller requested an isolation-only operation through the wrong
	/// feature identity.
	#[error("Git isolation operation is not available to this consumer")]
	IsolationConsumer,
	/// The requested feature branch escaped its compile-time namespace.
	#[error("Git isolation branch is outside the consumer namespace")]
	IsolationBranch,
}
/// Closed identities permitted to perform feature-internal Git transactions.
///
/// This is deliberately not stringly typed: adding a consumer is a source
/// change subject to review, and no agent or command can mint a commit
/// authority at runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitMutationConsumer {
	/// Autoresearch experiment isolation transactions.
	Autoresearch,
	/// User-driven `omp git` and `/git` staging-and-commit surface.
	InteractiveGit,
}

/// Fixed autoresearch transaction records accepted by [`GitMutation`].
///
/// There is intentionally no arbitrary commit-message variant. The mutation
/// API renders these records itself so it cannot become the §19 agentic
/// commit surface.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum IsolationCommit<'a> {
	/// Preserve the user's dirty tree as the experiment baseline.
	AutoresearchBaseline,
	/// Record the validated benchmark harness before the first run.
	AutoresearchHarness {
		/// Experiment display name.
		name: &'a str,
		/// Optional user goal.
		goal: Option<&'a str>,
	},
	/// Keep one measured experiment.
	AutoresearchRun {
		/// Human-readable experiment description.
		description:  &'a str,
		/// Canonical JSON metrics payload generated by autoresearch.
		metrics_json: &'a str,
	},
}

/// Exact outcome of a repository mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MutationOutcome {
	/// Git completed the requested mutation.
	Applied(GitRunOutput),
	/// Git stopped with unmerged index entries and preserved recoverable state.
	Conflict(GitRunOutput),
	/// Git rejected the operation without a proven partial effect.
	Rejected(GitRunOutput),
}

impl MutationOutcome {
	/// Returns whether Git completed successfully.
	pub const fn is_applied(&self) -> bool {
		matches!(self, Self::Applied(_))
	}
}

/// Patch application flags accepted by Git's binary-safe stdin path.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PatchOptions {
	/// Permit Git binary patch bodies.
	pub binary:    bool,
	/// Check applicability without changing the repository.
	pub check:     bool,
	/// Apply to the index rather than only the worktree.
	pub cached:    bool,
	/// Apply the patch in reverse.
	pub reverse:   bool,
	/// Fall back to Git's three-way merge machinery.
	pub three_way: bool,
}

/// Exact result of a patch preflight.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PatchCheck {
	/// Whether Git proved that the patch can be applied.
	pub applies: bool,
	/// Complete bounded command output and exit status.
	pub output:  GitRunOutput,
}

/// Typed cherry-pick result; advancing the sequencer remains caller-controlled.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CherryPickOutcome {
	/// The selected commit was applied.
	Applied(GitRunOutput),
	/// The current sequencer commit collapsed to an empty change.
	Empty(GitRunOutput),
	/// Git left recoverable unmerged entries for the caller to resolve or abort.
	Conflict(GitRunOutput),
	/// Git rejected the request for another reason.
	Rejected(GitRunOutput),
}

/// Result of creating an include-untracked stash.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StashPushOutcome {
	/// Whether `refs/stash` changed to a newly-created entry.
	pub created: bool,
	/// Exact result of `git stash push`.
	pub output:  GitRunOutput,
}

/// Safe top-stash restoration result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StashPopOutcome {
	/// Preflight and pop both succeeded; Git dropped the stash entry.
	Applied(GitRunOutput),
	/// Three-way preflight proved the stash would conflict, so no pop ran.
	PreflightConflict(GitRunOutput),
	/// Pop failed, but stash-scoped tracked restore and untracked cleanup
	/// succeeded.
	RolledBack(GitRunOutput),
	/// Pop failed and at least one bounded rollback operation also failed.
	Partial {
		/// Original failed pop result.
		pop:     GitRunOutput,
		/// Exact tracked-path restore result when it failed.
		restore: Option<GitRunOutput>,
		/// Literal-path cleanup result when it failed.
		clean:   Option<GitRunOutput>,
	},
}

/// Tree-wide reset policy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ResetMode {
	/// Preserve index and worktree.
	Soft,
	/// Reset the index and preserve the worktree.
	#[default]
	Mixed,
	/// Reset both index and worktree.
	Hard,
}

/// Untracked-path cleanup policy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CleanMode {
	/// Remove untracked paths but preserve ignored paths.
	#[default]
	Untracked,
	/// Remove untracked and ignored paths.
	IncludeIgnored,
	/// Remove only ignored paths.
	IgnoredOnly,
}

/// Result of serializing the current index as a Git tree object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WriteTreeOutcome {
	/// Newly written tree object identifier.
	Written(Str),
	/// The index contains unmerged entries.
	Conflict(GitRunOutput),
	/// Git rejected the operation for another reason.
	Rejected(GitRunOutput),
}
/// Result of cloning a repository, including whether the shallow transport
/// needed the compatibility fallback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CloneOutcome {
	/// `git clone --depth=1` completed.
	Shallow(GitRunOutput),
	/// Shallow clone was rejected and an ordinary clone completed.
	Full {
		/// Exact shallow-clone rejection retained for diagnostics.
		shallow_rejection: GitRunOutput,
		/// Exact successful full-clone output.
		output:            GitRunOutput,
	},
	/// Both shallow and ordinary clone attempts were rejected.
	Rejected {
		/// Exact shallow-clone rejection.
		shallow_rejection: GitRunOutput,
		/// Exact full-clone rejection.
		full_rejection:    GitRunOutput,
	},
}

/// Exact identity and timestamp flags for one low-level commit creation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CommitOptions<'a> {
	/// Git-compatible `Name <email>` author identity.
	pub author:      Option<&'a str>,
	/// Git-compatible author and committer date.
	pub date:        Option<&'a str>,
	/// Permit creation when the index tree equals `HEAD`.
	pub allow_empty: bool,
	/// Replace `HEAD` while preserving Git's ordinary amend semantics.
	pub amend:       bool,
}

/// Lease protection accepted by one push.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PushOptions<'a> {
	/// Optional `refname[:expected]` value for `--force-with-lease`.
	pub force_with_lease: Option<&'a str>,
}

/// Clones `remote` into `target`, preferring a one-commit shallow transfer.
///
/// Some servers reject shallow negotiation. Git removes a newly-created
/// destination after a failed clone; only then is the ordinary compatibility
/// attempt made against the same exact target.
pub async fn clone_repository(
	runner: &GitRunner,
	cwd: &Path,
	remote: &str,
	target: &str,
	cancel: &CancellationToken,
) -> Result<CloneOutcome, MutationError> {
	let options = GitRunOptions { deadline: GitDeadline::Network, ..Default::default() };
	let shallow = runner
		.run(cwd, &["clone", "--depth=1", "--no-tags", "--", remote, target], options, cancel)
		.await?;
	if shallow.exit_code == 0 {
		return Ok(CloneOutcome::Shallow(shallow));
	}
	let full = runner
		.run(cwd, &["clone", "--no-tags", "--", remote, target], options, cancel)
		.await?;
	if full.exit_code == 0 {
		Ok(CloneOutcome::Full { shallow_rejection: shallow, output: full })
	} else {
		Ok(CloneOutcome::Rejected { shallow_rejection: shallow, full_rejection: full })
	}
}

/// Low-level mutation facade for named repository consumers.
///
/// The facade deliberately has no commit-message synthesis, automatic staging,
/// changelog, or commit-coordinator surface. Every method performs exactly one
/// caller-selected repair or VCS primitive while holding the lock shared by all
/// linked worktrees of `repository`.
#[derive(Clone)]
pub struct GitMutation {
	runner:     GitRunner,
	repository: Repository,
	consumer:   GitMutationConsumer,
}

impl GitMutation {
	/// Creates a mutation facade bound to one canonical repository identity.
	pub const fn new(
		runner: GitRunner,
		repository: Repository,
		consumer: GitMutationConsumer,
	) -> Self {
		Self { runner, repository, consumer }
	}

	/// Creates a branch inside this consumer's compile-time isolation
	/// namespace.
	pub async fn create_isolation_branch(
		&self,
		branch: &str,
		cancel: &CancellationToken,
	) -> Result<MutationOutcome, MutationError> {
		if self.consumer != GitMutationConsumer::Autoresearch || !valid_autoresearch_branch(branch) {
			return Err(MutationError::IsolationBranch);
		}
		let _guard = lock::write(&self.repository, cancel).await?;
		self
			.mutation(&["switch", "--create", branch], None, cancel)
			.await
	}

	/// Commits only exact paths as one feature-internal isolation
	/// transaction.
	///
	/// The fixed [`IsolationCommit`] vocabulary is the code-level §19 guard:
	/// callers cannot submit a general commit message or ask this facade to
	/// synthesize one.
	pub async fn commit_isolation(
		&self,
		record: IsolationCommit<'_>,
		paths: &[&str],
		cancel: &CancellationToken,
	) -> Result<MutationOutcome, MutationError> {
		if self.consumer != GitMutationConsumer::Autoresearch {
			return Err(MutationError::IsolationConsumer);
		}
		let _guard = lock::write(&self.repository, cancel).await?;
		if paths.is_empty() {
			return Ok(MutationOutcome::Applied(noop_output()));
		}
		let mut add = Vec::with_capacity(paths.len() + 2);
		add.extend(["add", "--"]);
		add.extend_from_slice(paths);
		let staged = self.mutation(&add, None, cancel).await?;
		if !staged.is_applied() {
			return Ok(staged);
		}
		let message = isolation_commit_message(record);
		let mut commit = Vec::with_capacity(paths.len() + 5);
		commit.extend(["commit", "--file=-", "--only", "--"]);
		commit.extend_from_slice(paths);
		self
			.mutation(&commit, Some(message.as_bytes()), cancel)
			.await
	}

	/// Restores exactly one run's tracked and untracked paths.
	///
	/// Repeating the operation after a crash is safe: already-restored paths
	/// are accepted by `restore`, while `clean` is a no-op for absent paths.
	pub async fn rollback_isolation(
		&self,
		target: &str,
		tracked: &[&str],
		untracked: &[&str],
		cancel: &CancellationToken,
	) -> Result<MutationOutcome, MutationError> {
		if self.consumer != GitMutationConsumer::Autoresearch {
			return Err(MutationError::IsolationConsumer);
		}
		let _guard = lock::write(&self.repository, cancel).await?;
		if !tracked.is_empty() {
			let mut restore = Vec::with_capacity(tracked.len() + 6);
			restore.extend(["restore", "--source", target, "--staged", "--worktree", "--"]);
			restore.extend_from_slice(tracked);
			let outcome = self.mutation(&restore, None, cancel).await?;
			if !outcome.is_applied() {
				return Ok(outcome);
			}
		}
		if untracked.is_empty() {
			return Ok(MutationOutcome::Applied(noop_output()));
		}
		let mut clean = Vec::with_capacity(untracked.len() + 4);
		clean.extend(["--literal-pathspecs", "clean", "-fd", "--"]);
		clean.extend_from_slice(untracked);
		self.mutation(&clean, None, cancel).await
	}

	/// Stages only the supplied exact paths. An empty list is a no-op.
	pub async fn stage_files(
		&self,
		paths: &[&str],
		cancel: &CancellationToken,
	) -> Result<MutationOutcome, MutationError> {
		let _guard = lock::write(&self.repository, cancel).await?;
		if paths.is_empty() {
			return Ok(MutationOutcome::Applied(noop_output()));
		}
		let mut argv = Vec::with_capacity(paths.len() + 2);
		argv.extend(["add", "--"]);
		argv.extend_from_slice(paths);
		self.mutation(&argv, None, cancel).await
	}

	/// Stages every tracked, deleted, and untracked path with `git add -A`.
	pub async fn stage_all(
		&self,
		cancel: &CancellationToken,
	) -> Result<MutationOutcome, MutationError> {
		let _guard = lock::write(&self.repository, cancel).await?;
		self.mutation(&["add", "-A"], None, cancel).await
	}

	/// Resets the complete index to `HEAD` while preserving the worktree.
	pub async fn unstage_all(
		&self,
		cancel: &CancellationToken,
	) -> Result<MutationOutcome, MutationError> {
		let _guard = lock::write(&self.repository, cancel).await?;
		self.mutation(&["reset"], None, cancel).await
	}

	/// Removes only the supplied exact paths from the index. An empty list is a
	/// no-op.
	pub async fn reset_index_entries(
		&self,
		paths: &[&str],
		cancel: &CancellationToken,
	) -> Result<MutationOutcome, MutationError> {
		let _guard = lock::write(&self.repository, cancel).await?;
		if paths.is_empty() {
			return Ok(MutationOutcome::Applied(noop_output()));
		}
		let mut argv = Vec::with_capacity(paths.len() + 2);
		argv.extend(["reset", "--"]);
		argv.extend_from_slice(paths);
		self.mutation(&argv, None, cancel).await
	}

	/// Captures a complete bounded diff, validates every selection, and stages
	/// exactly the selected hunks through `git apply --cached --binary -`.
	pub async fn stage_hunks(
		&self,
		selections: &[HunkSelection],
		cancel: &CancellationToken,
	) -> Result<MutationOutcome, MutationError> {
		let _guard = lock::write(&self.repository, cancel).await?;
		if selections.is_empty() {
			return Ok(MutationOutcome::Applied(noop_output()));
		}
		let raw = GitDiff::new(self.runner.clone())
			.raw(
				self.cwd(),
				DiffOptions { cached: false, binary: true, ..Default::default() },
				&[],
				cancel,
			)
			.await?;
		let patch = build_selected_patch(&raw, selections)?;
		self
			.mutation(&["apply", "--binary", "--cached", "-"], Some(&patch), cancel)
			.await
	}

	/// Unstages selected hunks by reversing a complete cached diff in the index.
	pub async fn unstage_hunks(
		&self,
		selections: &[HunkSelection],
		cancel: &CancellationToken,
	) -> Result<MutationOutcome, MutationError> {
		let _guard = lock::write(&self.repository, cancel).await?;
		if selections.is_empty() {
			return Ok(MutationOutcome::Applied(noop_output()));
		}
		let raw = GitDiff::new(self.runner.clone())
			.raw(
				self.cwd(),
				DiffOptions { cached: true, binary: true, ..Default::default() },
				&[],
				cancel,
			)
			.await?;
		let patch = build_selected_patch(&raw, selections)?;
		self
			.mutation(&["apply", "--binary", "--cached", "--reverse", "-"], Some(&patch), cancel)
			.await
	}

	/// Discards selected worktree hunks by applying their complete diff in
	/// reverse.
	pub async fn discard_hunks(
		&self,
		selections: &[HunkSelection],
		cancel: &CancellationToken,
	) -> Result<MutationOutcome, MutationError> {
		let _guard = lock::write(&self.repository, cancel).await?;
		if selections.is_empty() {
			return Ok(MutationOutcome::Applied(noop_output()));
		}
		let raw = GitDiff::new(self.runner.clone())
			.raw(
				self.cwd(),
				DiffOptions { cached: false, binary: true, ..Default::default() },
				&[],
				cancel,
			)
			.await?;
		let patch = build_selected_patch(&raw, selections)?;
		self
			.mutation(&["apply", "--binary", "--reverse", "-"], Some(&patch), cancel)
			.await
	}

	/// Stages selected old/new changed lines from one worktree file diff.
	pub async fn stage_lines(
		&self,
		path: &str,
		range: DiffLineSelection,
		cancel: &CancellationToken,
	) -> Result<MutationOutcome, MutationError> {
		self
			.apply_selected_lines(path, range, false, false, cancel)
			.await
	}

	/// Unstages selected old/new changed lines from one cached file diff.
	pub async fn unstage_lines(
		&self,
		path: &str,
		range: DiffLineSelection,
		cancel: &CancellationToken,
	) -> Result<MutationOutcome, MutationError> {
		self
			.apply_selected_lines(path, range, true, true, cancel)
			.await
	}

	/// Discards selected old/new changed lines from one worktree file diff.
	pub async fn discard_lines(
		&self,
		path: &str,
		range: DiffLineSelection,
		cancel: &CancellationToken,
	) -> Result<MutationOutcome, MutationError> {
		self
			.apply_selected_lines(path, range, false, true, cancel)
			.await
	}

	/// Applies or checks exact patch bytes without rewriting their terminators.
	pub async fn apply_patch(
		&self,
		patch: &[u8],
		options: PatchOptions,
		cancel: &CancellationToken,
	) -> Result<MutationOutcome, MutationError> {
		let _guard = lock::write(&self.repository, cancel).await?;
		let argv = apply_argv(options);
		self.mutation(&argv, Some(patch), cancel).await
	}

	/// Checks exact patch bytes under the write lock used by a subsequent apply.
	pub async fn check_patch(
		&self,
		patch: &[u8],
		mut options: PatchOptions,
		cancel: &CancellationToken,
	) -> Result<PatchCheck, MutationError> {
		let _guard = lock::write(&self.repository, cancel).await?;
		options.check = true;
		let output = self
			.invoke(&apply_argv(options), Some(patch), cancel)
			.await?;
		Ok(PatchCheck { applies: output.exit_code == 0, output })
	}

	/// Starts a cherry-pick without automatically skipping or aborting failures.
	pub async fn cherry_pick(
		&self,
		revision: &str,
		cancel: &CancellationToken,
	) -> Result<CherryPickOutcome, MutationError> {
		let _guard = lock::write(&self.repository, cancel).await?;
		let output = self
			.invoke(&["cherry-pick", revision], None, cancel)
			.await?;
		self.classify_cherry_pick(output, cancel).await
	}

	/// Explicitly aborts the current cherry-pick sequence.
	pub async fn cherry_pick_abort(
		&self,
		cancel: &CancellationToken,
	) -> Result<MutationOutcome, MutationError> {
		let _guard = lock::write(&self.repository, cancel).await?;
		self
			.mutation(&["cherry-pick", "--abort"], None, cancel)
			.await
	}

	/// Explicitly skips the current cherry-pick commit and advances the
	/// sequence.
	pub async fn cherry_pick_skip(
		&self,
		cancel: &CancellationToken,
	) -> Result<MutationOutcome, MutationError> {
		let _guard = lock::write(&self.repository, cancel).await?;
		self
			.mutation(&["cherry-pick", "--skip"], None, cancel)
			.await
	}

	/// Creates a stash containing index, worktree, and untracked changes.
	pub async fn stash_push(
		&self,
		message: Option<&str>,
		cancel: &CancellationToken,
	) -> Result<StashPushOutcome, MutationError> {
		let _guard = lock::write(&self.repository, cancel).await?;
		let before = self.resolve_stash(cancel).await?;
		let mut argv = vec!["stash", "push", "--include-untracked"];
		if let Some(message) = message {
			argv.extend(["-m", message]);
		}
		let output = self.invoke(&argv, None, cancel).await?;
		if output.exit_code != 0 {
			return Ok(StashPushOutcome { created: false, output });
		}
		let after = self.resolve_stash(cancel).await?;
		Ok(StashPushOutcome { created: after.is_some() && after != before, output })
	}

	/// Returns the top stash's complete working-tree binary patch, or empty
	/// bytes.
	pub async fn stash_show(&self, cancel: &CancellationToken) -> Result<Bytes, MutationError> {
		let _guard = lock::write(&self.repository, cancel).await?;
		self.stash_patch(cancel).await
	}

	/// Pops the top stash without automatic rollback.
	pub async fn stash_pop(
		&self,
		restore_index: bool,
		cancel: &CancellationToken,
	) -> Result<MutationOutcome, MutationError> {
		let _guard = lock::write(&self.repository, cancel).await?;
		let mut argv = vec!["stash", "pop"];
		if restore_index {
			argv.push("--index");
		}
		self.mutation(&argv, None, cancel).await
	}

	/// Preflights a stash pop and rolls back only effects proven to originate in
	/// that stash when the real pop still fails.
	pub async fn stash_try_pop(
		&self,
		restore_index: bool,
		cancel: &CancellationToken,
	) -> Result<StashPopOutcome, MutationError> {
		let _guard = lock::write(&self.repository, cancel).await?;
		let patch = self.stash_patch(cancel).await?;
		if !patch.is_empty() {
			let check = self
				.invoke(&["apply", "--binary", "--3way", "--check", "-"], Some(&patch), cancel)
				.await?;
			if check.exit_code != 0 {
				return Ok(StashPopOutcome::PreflightConflict(check));
			}
		}
		let tracked = stash_tracked_paths(&patch)?;
		let untracked = self.stash_untracked(cancel).await?;
		let mut pop_argv = vec!["stash", "pop"];
		if restore_index {
			pop_argv.push("--index");
		}
		let pop = self.invoke(&pop_argv, None, cancel).await?;
		if pop.exit_code == 0 {
			return Ok(StashPopOutcome::Applied(pop));
		}
		let restore_failure = if tracked.is_empty() {
			None
		} else {
			let mut argv = Vec::with_capacity(tracked.len() + 6);
			argv.extend(["restore", "--source=HEAD", "--staged", "--worktree", "--"]);
			for path in &tracked {
				argv.push(path.as_str());
			}
			let restore = self.invoke(&argv, None, cancel).await?;
			(restore.exit_code != 0).then_some(restore)
		};
		let clean_failure = if untracked.is_empty() {
			None
		} else {
			let mut argv = Vec::with_capacity(untracked.len() + 4);
			argv.extend(["--literal-pathspecs", "clean", "-fdx", "--"]);
			for path in &untracked {
				argv.push(path.as_str());
			}
			let clean = self.invoke(&argv, None, cancel).await?;
			(clean.exit_code != 0).then_some(clean)
		};
		if restore_failure.is_none() && clean_failure.is_none() {
			Ok(StashPopOutcome::RolledBack(pop))
		} else {
			Ok(StashPopOutcome::Partial { pop, restore: restore_failure, clean: clean_failure })
		}
	}

	/// Restores exact paths from an optional source into index and/or worktree.
	pub async fn restore(
		&self,
		paths: &[&str],
		source: Option<&str>,
		staged: bool,
		worktree: bool,
		cancel: &CancellationToken,
	) -> Result<MutationOutcome, MutationError> {
		let _guard = lock::write(&self.repository, cancel).await?;
		let mut argv = Vec::with_capacity(paths.len() + 6);
		argv.push("restore");
		if let Some(source) = source {
			argv.extend(["--source", source]);
		}
		if staged {
			argv.push("--staged");
		}
		if worktree {
			argv.push("--worktree");
		}
		if !paths.is_empty() {
			argv.push("--");
			argv.extend_from_slice(paths);
		}
		self.mutation(&argv, None, cancel).await
	}

	/// Resets the repository tree with one explicit mode and optional target.
	pub async fn reset(
		&self,
		mode: ResetMode,
		target: Option<&str>,
		cancel: &CancellationToken,
	) -> Result<MutationOutcome, MutationError> {
		let _guard = lock::write(&self.repository, cancel).await?;
		let mode = match mode {
			ResetMode::Soft => "--soft",
			ResetMode::Mixed => "--mixed",
			ResetMode::Hard => "--hard",
		};
		let mut argv = vec!["reset", mode];
		if let Some(target) = target {
			argv.push(target);
		}
		self.mutation(&argv, None, cancel).await
	}

	/// Removes untracked paths with literal pathspecs and an explicit ignore
	/// mode.
	pub async fn clean(
		&self,
		mode: CleanMode,
		paths: &[&str],
		cancel: &CancellationToken,
	) -> Result<MutationOutcome, MutationError> {
		let _guard = lock::write(&self.repository, cancel).await?;
		let flag = match mode {
			CleanMode::Untracked => "-fd",
			CleanMode::IncludeIgnored => "-fdx",
			CleanMode::IgnoredOnly => "-fdX",
		};
		let mut argv = Vec::with_capacity(paths.len() + 4);
		argv.extend(["--literal-pathspecs", "clean", flag]);
		if !paths.is_empty() {
			argv.push("--");
			argv.extend_from_slice(paths);
		}
		self.mutation(&argv, None, cancel).await
	}

	/// Reads one tree-ish into the index.
	pub async fn read_tree(
		&self,
		treeish: &str,
		cancel: &CancellationToken,
	) -> Result<MutationOutcome, MutationError> {
		let _guard = lock::write(&self.repository, cancel).await?;
		self.mutation(&["read-tree", treeish], None, cancel).await
	}

	/// Writes the current index as a tree and returns its object identifier.
	pub async fn write_tree(
		&self,
		cancel: &CancellationToken,
	) -> Result<WriteTreeOutcome, MutationError> {
		let _guard = lock::write(&self.repository, cancel).await?;
		let output = self.invoke(&["write-tree"], None, cancel).await?;
		if output.exit_code == 0 {
			let tree = str::from_utf8(&output.stdout)
				.map_err(|_| MutationError::NonUtf8)?
				.trim();
			return Ok(WriteTreeOutcome::Written(tree.to_str()));
		}
		if self.has_unmerged(cancel).await? {
			Ok(WriteTreeOutcome::Conflict(output))
		} else {
			Ok(WriteTreeOutcome::Rejected(output))
		}
	}

	/// Creates one commit from the current index using exact stdin message
	/// bytes and caller-selected identity/date flags.
	pub async fn create_commit(
		&self,
		message: &[u8],
		options: CommitOptions<'_>,
		cancel: &CancellationToken,
	) -> Result<MutationOutcome, MutationError> {
		let _guard = lock::write(&self.repository, cancel).await?;
		let mut argv = Vec::with_capacity(8);
		argv.extend(["commit", "--file=-"]);
		if let Some(author) = options.author {
			argv.extend(["--author", author]);
		}
		if let Some(date) = options.date {
			argv.extend(["--date", date]);
		}
		if options.allow_empty {
			argv.push("--allow-empty");
		}
		if options.amend {
			argv.push("--amend");
		}
		self.mutation(&argv, Some(message), cancel).await
	}

	/// Pushes exact refspecs without following tags and with optional
	/// force-with-lease protection.
	pub async fn push(
		&self,
		remote: &str,
		refspecs: &[&str],
		options: PushOptions<'_>,
		cancel: &CancellationToken,
	) -> Result<MutationOutcome, MutationError> {
		let _guard = lock::write(&self.repository, cancel).await?;
		let mut argv = Vec::with_capacity(refspecs.len() + 5);
		argv.extend(["push", "--no-follow-tags"]);
		let lease_flag = options
			.force_with_lease
			.filter(|lease| !lease.is_empty())
			.map(|lease| format!("--force-with-lease={lease}"));
		if options.force_with_lease.is_some() {
			argv.push(lease_flag.as_deref().unwrap_or("--force-with-lease"));
		}
		argv.push(remote);
		argv.extend_from_slice(refspecs);
		self.network_mutation(&argv, cancel).await
	}

	async fn apply_selected_lines(
		&self,
		path: &str,
		range: DiffLineSelection,
		cached: bool,
		reverse: bool,
		cancel: &CancellationToken,
	) -> Result<MutationOutcome, MutationError> {
		let _guard = lock::write(&self.repository, cancel).await?;
		let raw = GitDiff::new(self.runner.clone())
			.raw(self.cwd(), DiffOptions { cached, binary: true, ..Default::default() }, &[], cancel)
			.await?;
		let files = diff::parse_unified(raw);
		let file = files
			.iter()
			.find(|file| file_matches_path(file, path.as_bytes()))
			.ok_or_else(|| SelectionError::PathMissing { path: path.to_str() })?;
		let patch = build_line_patch_with_endings(
			file,
			path,
			range,
			if reverse {
				LinePatchDirection::Reverse
			} else {
				LinePatchDirection::Apply
			},
			&self.line_endings(file, cached, path, cancel).await?,
		)?;
		let options =
			PatchOptions { binary: true, cached: cached || !reverse, reverse, ..Default::default() };
		self
			.mutation(&apply_argv(options), Some(&patch), cancel)
			.await
	}

	async fn line_endings(
		&self,
		file: &FileDiff,
		cached: bool,
		path: &str,
		cancel: &CancellationToken,
	) -> Result<LineEndings, MutationError> {
		let old_path = file
			.old_path
			.as_deref()
			.and_then(|path| str::from_utf8(path).ok())
			.unwrap_or(path);
		let new_path = file
			.path
			.as_deref()
			.and_then(|path| str::from_utf8(path).ok())
			.unwrap_or(path);
		let old_spec = if cached {
			format!("HEAD:{old_path}")
		} else {
			format!(":0:{old_path}")
		};
		let old = self.blob_or_empty(&old_spec, cancel).await?;
		let new = if cached {
			self.blob_or_empty(&format!(":0:{new_path}"), cancel).await?
		} else {
			match tokio::fs::read(self.cwd().join(new_path)).await {
				Ok(bytes) => Bytes::from(bytes),
				Err(error) if error.kind() == io::ErrorKind::NotFound => Bytes::new(),
				Err(error) => return Err(MutationError::WorktreeRead(error)),
			}
		};
		Ok(LineEndings::from_contents(&old, &new))
	}

	async fn blob_or_empty(
		&self,
		spec: &str,
		cancel: &CancellationToken,
	) -> Result<Bytes, MutationError> {
		let output = self.invoke_complete(&["show", spec], None, cancel).await?;
		Ok(if output.exit_code == 0 {
			output.stdout
		} else {
			Bytes::new()
		})
	}

	fn cwd(&self) -> &Path {
		&self.repository.worktree_root
	}

	async fn invoke(
		&self,
		argv: &[&str],
		input: Option<&[u8]>,
		cancel: &CancellationToken,
	) -> Result<GitRunOutput, MutationError> {
		self
			.invoke_with_completeness(argv, input, false, cancel)
			.await
	}

	async fn invoke_complete(
		&self,
		argv: &[&str],
		input: Option<&[u8]>,
		cancel: &CancellationToken,
	) -> Result<GitRunOutput, MutationError> {
		self
			.invoke_with_completeness(argv, input, true, cancel)
			.await
	}

	async fn invoke_with_completeness(
		&self,
		argv: &[&str],
		input: Option<&[u8]>,
		parse_sensitive: bool,
		cancel: &CancellationToken,
	) -> Result<GitRunOutput, MutationError> {
		let options = GitRunOptions { parse_sensitive, ..Default::default() };
		match input {
			Some(input) => Ok(self
				.runner
				.run_with_stdin(self.cwd(), argv, options, input, cancel)
				.await?),
			None => Ok(self.runner.run(self.cwd(), argv, options, cancel).await?),
		}
	}

	async fn mutation(
		&self,
		argv: &[&str],
		input: Option<&[u8]>,
		cancel: &CancellationToken,
	) -> Result<MutationOutcome, MutationError> {
		let output = self.invoke(argv, input, cancel).await?;
		if output.exit_code == 0 {
			return Ok(MutationOutcome::Applied(output));
		}
		if self.has_unmerged(cancel).await? {
			Ok(MutationOutcome::Conflict(output))
		} else {
			Ok(MutationOutcome::Rejected(output))
		}
	}

	async fn network_mutation(
		&self,
		argv: &[&str],
		cancel: &CancellationToken,
	) -> Result<MutationOutcome, MutationError> {
		let output = self
			.runner
			.run(
				self.cwd(),
				argv,
				GitRunOptions { deadline: GitDeadline::Network, ..Default::default() },
				cancel,
			)
			.await?;
		if output.exit_code == 0 {
			return Ok(MutationOutcome::Applied(output));
		}
		if self.has_unmerged(cancel).await? {
			Ok(MutationOutcome::Conflict(output))
		} else {
			Ok(MutationOutcome::Rejected(output))
		}
	}

	async fn classify_cherry_pick(
		&self,
		output: GitRunOutput,
		cancel: &CancellationToken,
	) -> Result<CherryPickOutcome, MutationError> {
		if output.exit_code == 0 {
			return Ok(CherryPickOutcome::Applied(output));
		}
		if self.has_unmerged(cancel).await? {
			return Ok(CherryPickOutcome::Conflict(output));
		}
		let sequencer = self
			.invoke(&["rev-parse", "--verify", "-q", "CHERRY_PICK_HEAD"], None, cancel)
			.await?;
		if sequencer.exit_code == 0 {
			let staged = self
				.invoke(&["diff", "--cached", "--quiet"], None, cancel)
				.await?;
			if staged.exit_code == 0 {
				return Ok(CherryPickOutcome::Empty(output));
			}
		}
		Ok(CherryPickOutcome::Rejected(output))
	}

	async fn has_unmerged(&self, cancel: &CancellationToken) -> Result<bool, MutationError> {
		let output = self
			.invoke_complete(&["ls-files", "-u", "-z"], None, cancel)
			.await?;
		Ok(output.exit_code == 0 && !output.stdout.is_empty())
	}

	async fn resolve_stash(
		&self,
		cancel: &CancellationToken,
	) -> Result<Option<Bytes>, MutationError> {
		let output = self
			.invoke_complete(&["rev-parse", "--verify", "-q", "refs/stash"], None, cancel)
			.await?;
		Ok((output.exit_code == 0).then_some(output.stdout))
	}

	async fn stash_patch(&self, cancel: &CancellationToken) -> Result<Bytes, MutationError> {
		let output = self
			.invoke_complete(&["stash", "show", "-p", "--binary", "stash@{0}"], None, cancel)
			.await?;
		Ok(if output.exit_code == 0 {
			output.stdout
		} else {
			Bytes::new()
		})
	}

	async fn stash_untracked(&self, cancel: &CancellationToken) -> Result<Vec<Str>, MutationError> {
		let output = self
			.invoke_complete(&["ls-tree", "-r", "-z", "--name-only", "stash@{0}^3"], None, cancel)
			.await?;
		if output.exit_code != 0 {
			return Ok(Vec::new());
		}
		output
			.stdout
			.split(|byte| *byte == 0)
			.filter(|path| !path.is_empty())
			.map(|path| {
				str::from_utf8(path)
					.map(Str::from)
					.map_err(|_| MutationError::NonUtf8)
			})
			.collect()
	}
}

fn valid_autoresearch_branch(branch: &str) -> bool {
	let Some(suffix) = branch.strip_prefix("autoresearch/") else {
		return false;
	};
	!suffix.is_empty()
		&& suffix.len() <= 48
		&& suffix
			.bytes()
			.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'/'))
}

fn isolation_commit_message(record: IsolationCommit<'_>) -> String {
	match record {
		IsolationCommit::AutoresearchBaseline => "autoresearch: preserve dirty baseline\n".to_owned(),
		IsolationCommit::AutoresearchHarness { name, goal } => {
			let mut message = String::from("autoresearch: harness setup\n\n");
			let _ = writeln!(message, "Experiment: {name}");
			if let Some(goal) = goal {
				let _ = writeln!(message, "Goal: {goal}");
			}
			message
		},
		IsolationCommit::AutoresearchRun { description, metrics_json } => {
			let mut message = String::new();
			let _ = writeln!(message, "{description}\n\nAutoresearch-Result: {metrics_json}");
			message
		},
	}
}

fn stash_tracked_paths(patch: &Bytes) -> Result<Vec<Str>, MutationError> {
	let mut paths = Vec::new();
	for file in diff::parse_unified(patch.clone()) {
		for path in [file.old_path, file.path].into_iter().flatten() {
			let path = str::from_utf8(&path)
				.map_err(|_| MutationError::NonUtf8)?
				.to_str();
			if !paths.contains(&path) {
				paths.push(path);
			}
		}
	}
	Ok(paths)
}

fn apply_argv(options: PatchOptions) -> Vec<&'static str> {
	let mut argv = Vec::with_capacity(7);
	argv.push("apply");
	if options.binary {
		argv.push("--binary");
	}
	if options.check {
		argv.push("--check");
	}
	if options.cached {
		argv.push("--cached");
	}
	if options.reverse {
		argv.push("--reverse");
	}
	if options.three_way {
		argv.push("--3way");
	}
	argv.push("-");
	argv
}

fn build_selected_patch(
	raw: &Bytes,
	selections: &[HunkSelection],
) -> Result<Bytes, SelectionError> {
	let files = diff::parse_unified(raw.clone());
	let mut seen = HashSet::with_capacity(selections.len());
	let mut patch = BytesMut::with_capacity(raw.len());
	for selection in selections {
		if !seen.insert(selection.path.clone()) {
			return Err(SelectionError::DuplicatePath { path: selection.path.clone() });
		}
		let file = files
			.iter()
			.find(|file| file_matches_path(file, selection.path.as_bytes()))
			.ok_or_else(|| SelectionError::PathMissing { path: selection.path.clone() })?;
		match &selection.selector {
			HunkSelector::All => append_patch_part(&mut patch, &file.raw),
			HunkSelector::Indices(indices) => {
				if file.binary {
					return Err(SelectionError::BinarySubset { path: selection.path.clone() });
				}
				if let Some(index) = indices
					.iter()
					.copied()
					.find(|index| *index == 0 || *index > file.hunks.len())
				{
					return Err(SelectionError::InvalidHunkIndex {
						path: selection.path.clone(),
						index,
						hunk_count: file.hunks.len(),
					});
				}
				let wanted: HashSet<usize> = indices.iter().copied().collect();
				let hunks: Vec<_> = file
					.hunks
					.iter()
					.enumerate()
					.filter(|(index, _)| wanted.contains(&(index + 1)))
					.map(|(_, hunk)| hunk)
					.collect();
				append_selected_hunks(&mut patch, file, &hunks, &selection.path)?;
			},
			HunkSelector::Lines { start, end } => {
				if file.binary {
					return Err(SelectionError::BinarySubset { path: selection.path.clone() });
				}
				if *start == 0 || start > end {
					return Err(SelectionError::InvalidLineRange { path: selection.path.clone() });
				}
				let selected = build_line_patch(
					file,
					selection.path.as_str(),
					DiffLineSelection::new_lines(*start, *end),
					LinePatchDirection::Apply,
				)?;
				append_patch_part(&mut patch, &selected);
			},
		}
	}
	Ok(patch.freeze())
}
/// Synthesizes one standalone apply-intent patch containing only selected
/// changed lines from `file`.
///
/// For [`LinePatchDirection::Apply`], unselected additions are omitted and
/// unselected deletions become context. Reverse patches use the inverse
/// transformation so their source is the complete new side. Hunk coordinates
/// and no-final-newline markers are rewritten without decoding file content.
pub fn build_line_patch(
	file: &FileDiff,
	path: &str,
	selection: DiffLineSelection,
	direction: LinePatchDirection,
) -> Result<Bytes, SelectionError> {
	build_line_patch_with_endings(file, path, selection, direction, &LineEndings::default())
}

fn build_line_patch_with_endings(
	file: &FileDiff,
	path: &str,
	selection: DiffLineSelection,
	direction: LinePatchDirection,
	line_endings: &LineEndings,
) -> Result<Bytes, SelectionError> {
	let path = path.to_str();
	if file.binary {
		return Err(SelectionError::BinarySubset { path });
	}
	if selection.old.is_none() && selection.new.is_none()
		|| selection.old.is_some_and(|range| !range.is_valid())
		|| selection.new.is_some_and(|range| !range.is_valid())
	{
		return Err(SelectionError::InvalidLineRange { path });
	}
	if file.hunks.is_empty() {
		return Err(SelectionError::NoMatchingLines { path });
	}

	let header_end = find_bytes(&file.raw, &file.hunks[0].raw).unwrap_or(file.raw.len());
	let mut transformed_hunks = Vec::with_capacity(file.hunks.len());
	let mut delta = 0_i64;
	let mut selected_changes = 0_usize;
	for hunk in &file.hunks {
		let Some(transformed) =
			transform_hunk(hunk, selection, delta, direction, line_endings)
		else {
			continue;
		};
		selected_changes += transformed.selected_changes;
		delta += transformed.delta;
		transformed_hunks.push(transformed);
	}
	if transformed_hunks.is_empty() {
		return Err(SelectionError::NoMatchingLines { path });
	}
	let total_changes = file
		.hunks
		.iter()
		.flat_map(|hunk| hunk.raw.split_inclusive(|byte| *byte == b'\n').skip(1))
		.filter(|line| matches!(line.first(), Some(b'+' | b'-')))
		.count();
	let mut patch = BytesMut::with_capacity(file.raw.len());
	append_line_patch_header(
		&mut patch,
		file,
		header_end,
		direction,
		selected_changes == total_changes,
	);
	for transformed in transformed_hunks {
		patch.extend_from_slice(&transformed.raw);
	}
	if !patch.ends_with(b"\n") {
		patch.extend_from_slice(b"\n");
	}
	Ok(patch.freeze())
}
fn append_line_patch_header(
	patch: &mut BytesMut,
	file: &FileDiff,
	header_end: usize,
	direction: LinePatchDirection,
	complete: bool,
) {
	let normalize = match (&file.old_path, &file.path) {
		(Some(old_path), Some(path)) => old_path != path,
		(Some(_), None) => direction == LinePatchDirection::Apply && !complete,
		(None, Some(_)) => direction == LinePatchDirection::Reverse && !complete,
		(None, None) => false,
	};
	if normalize && let Some(path) = file.path.as_ref().or(file.old_path.as_ref()) {
		patch.extend_from_slice(b"diff --git a/");
		patch.extend_from_slice(path);
		patch.extend_from_slice(b" b/");
		patch.extend_from_slice(path);
		patch.extend_from_slice(b"\n--- a/");
		patch.extend_from_slice(path);
		patch.extend_from_slice(b"\n+++ b/");
		patch.extend_from_slice(path);
		patch.extend_from_slice(b"\n");
		return;
	}
	patch.extend_from_slice(&file.raw[..header_end]);
}

#[derive(Default)]
struct LineEndings {
	old_crlf: Vec<bool>,
	new_crlf: Vec<bool>,
}

impl LineEndings {
	fn from_contents(old: &[u8], new: &[u8]) -> Self {
		Self { old_crlf: crlf_lines(old), new_crlf: crlf_lines(new) }
	}

	fn old_is_crlf(&self, line: u64) -> bool {
		line.checked_sub(1)
			.and_then(|index| usize::try_from(index).ok())
			.and_then(|index| self.old_crlf.get(index))
			.copied()
			.unwrap_or(false)
	}

	fn new_is_crlf(&self, line: u64) -> bool {
		line.checked_sub(1)
			.and_then(|index| usize::try_from(index).ok())
			.and_then(|index| self.new_crlf.get(index))
			.copied()
			.unwrap_or(false)
	}
}

fn crlf_lines(contents: &[u8]) -> Vec<bool> {
	contents
		.split_inclusive(|byte| *byte == b'\n')
		.map(|line| line.ends_with(b"\r\n"))
		.collect()
}

struct TransformedHunk {
	raw:              Bytes,
	delta:            i64,
	selected_changes: usize,
}

fn transform_hunk(
	hunk: &DiffHunk,
	selection: DiffLineSelection,
	delta_before: i64,
	direction: LinePatchDirection,
	line_endings: &LineEndings,
) -> Option<TransformedHunk> {
	let header_end = hunk
		.raw
		.iter()
		.position(|byte| *byte == b'\n')
		.map_or(hunk.raw.len(), |position| position + 1);
	let header = &hunk.raw[..header_end];
	let closing = find_bytes(header.get(2..).unwrap_or_default(), b"@@").map(|offset| offset + 2)?;
	let suffix = &header[closing + 2..];
	let mut body = BytesMut::with_capacity(hunk.raw.len().saturating_sub(header_end));
	let mut old_line = hunk.old_start;
	let mut new_line = hunk.new_start;
	let mut old_count = 0_u64;
	let mut new_count = 0_u64;
	let mut selected_additions = 0_i64;
	let mut selected_deletions = 0_i64;
	let mut matched = false;
	let mut deletions = Vec::new();
	let mut additions = Vec::new();
	let mut lines = hunk.raw[header_end..]
		.split_inclusive(|byte| *byte == b'\n')
		.peekable();

	while let Some(line) = lines.next() {
		let marker = if lines
			.peek()
			.is_some_and(|next| next.first() == Some(&b'\\'))
		{
			lines.next()
		} else {
			None
		};
		match line.first().copied() {
			Some(b' ') => {
				append_transformed_change_block(
					&mut body,
					&mut deletions,
					&mut additions,
					direction,
					&mut old_count,
					&mut new_count,
					&mut selected_deletions,
					&mut selected_additions,
				);
				append_context_hunk_line(
					&mut body,
					line,
					marker,
					line_endings.old_is_crlf(old_line),
					line_endings.new_is_crlf(new_line),
				);
				old_count += 1;
				new_count += 1;
				old_line += 1;
				new_line += 1;
			},
			Some(b'-') => {
				let selected = selection.old.is_some_and(|range| range.contains(old_line));
				matched |= selected;
				deletions.push(PendingHunkLine { raw: line, marker, selected, crlf: line_endings.old_is_crlf(old_line) });
				old_line += 1;
			},
			Some(b'+') => {
				let selected = selection.new.is_some_and(|range| range.contains(new_line));
				matched |= selected;
				additions.push(PendingHunkLine { raw: line, marker, selected, crlf: line_endings.new_is_crlf(new_line) });
				new_line += 1;
			},
			_ => {
				append_transformed_change_block(
					&mut body,
					&mut deletions,
					&mut additions,
					direction,
					&mut old_count,
					&mut new_count,
					&mut selected_deletions,
					&mut selected_additions,
				);
				append_hunk_line(&mut body, line, marker, false);
			},
		}
	}
	append_transformed_change_block(
		&mut body,
		&mut deletions,
		&mut additions,
		direction,
		&mut old_count,
		&mut new_count,
		&mut selected_deletions,
		&mut selected_additions,
	);
	if !matched {
		return None;
	}

	let (old_start, new_start) = match direction {
		LinePatchDirection::Apply => {
			(hunk.old_start, transformed_new_start(hunk.old_start, old_count, new_count, delta_before))
		},
		LinePatchDirection::Reverse => {
			(transformed_old_start(hunk.new_start, old_count, new_count, delta_before), hunk.new_start)
		},
	};
	let mut raw = BytesMut::with_capacity(header.len() + body.len() + 48);
	let header = format!("@@ -{},{} +{},{} @@", old_start, old_count, new_start, new_count);
	raw.extend_from_slice(header.as_bytes());
	raw.extend_from_slice(suffix);
	raw.extend_from_slice(&body);
	Some(TransformedHunk {
		raw:              raw.freeze(),
		delta:            selected_additions - selected_deletions,
		selected_changes: (selected_additions + selected_deletions) as usize,
	})
}

struct PendingHunkLine<'a> {
	raw:      &'a [u8],
	marker:   Option<&'a [u8]>,
	selected: bool,
	crlf:     bool,
}

fn append_transformed_change_block(
	body: &mut BytesMut,
	deletions: &mut Vec<PendingHunkLine<'_>>,
	additions: &mut Vec<PendingHunkLine<'_>>,
	direction: LinePatchDirection,
	old_count: &mut u64,
	new_count: &mut u64,
	selected_deletions: &mut i64,
	selected_additions: &mut i64,
) {
	let rows = deletions.len().max(additions.len());
	for index in 0..rows {
		if let Some(line) = deletions.get(index) {
			match (direction, line.selected) {
				(_, true) => {
					append_hunk_line(body, line.raw, line.marker, line.crlf);
					*old_count += 1;
					*selected_deletions += 1;
				},
				(LinePatchDirection::Apply, false) => {
					append_context_hunk_line(body, line.raw, line.marker, line.crlf, line.crlf);
					*old_count += 1;
					*new_count += 1;
				},
				(LinePatchDirection::Reverse, false) => {},
			}
		}
		if let Some(line) = additions.get(index) {
			match (direction, line.selected) {
				(_, true) => {
					append_hunk_line(body, line.raw, line.marker, line.crlf);
					*new_count += 1;
					*selected_additions += 1;
				},
				(LinePatchDirection::Reverse, false) => {
					append_context_hunk_line(body, line.raw, line.marker, line.crlf, line.crlf);
					*old_count += 1;
					*new_count += 1;
				},
				(LinePatchDirection::Apply, false) => {},
			}
		}
	}
	deletions.clear();
	additions.clear();
}

fn append_hunk_line(
	body: &mut BytesMut,
	line: &[u8],
	marker: Option<&[u8]>,
	crlf: bool,
) {
	if crlf {
		let content = line
			.strip_suffix(b"\r\n")
			.or_else(|| line.strip_suffix(b"\n"))
			.unwrap_or(line);
		body.extend_from_slice(content);
		body.extend_from_slice(b"\r\n");
	} else {
		body.extend_from_slice(line);
	}
	if let Some(marker) = marker {
		body.extend_from_slice(marker);
	}
}

fn append_context_hunk_line(
	body: &mut BytesMut,
	line: &[u8],
	marker: Option<&[u8]>,
	old_crlf: bool,
	new_crlf: bool,
) {
	if old_crlf || new_crlf {
		let content = line
			.strip_suffix(b"\r\n")
			.or_else(|| line.strip_suffix(b"\n"))
			.unwrap_or(line);
		body.extend_from_slice(b"-");
		body.extend_from_slice(&content[1..]);
		body.extend_from_slice(if old_crlf { b"\r\n" } else { b"\n" });
		body.extend_from_slice(b"+");
		body.extend_from_slice(&content[1..]);
		body.extend_from_slice(if new_crlf { b"\r\n" } else { b"\n" });
	} else {
		body.extend_from_slice(b" ");
		body.extend_from_slice(&line[1..]);
	}
	if let Some(marker) = marker {
		body.extend_from_slice(marker);
	}
}

fn transformed_new_start(old_start: u64, old_count: u64, new_count: u64, delta_before: i64) -> u64 {
	let base = if old_count == 0 {
		old_start.saturating_add(1)
	} else if new_count == 0 {
		old_start.saturating_sub(1)
	} else {
		old_start
	};
	if delta_before.is_negative() {
		base.saturating_sub(delta_before.unsigned_abs())
	} else {
		base.saturating_add(delta_before as u64)
	}
}
fn transformed_old_start(new_start: u64, old_count: u64, new_count: u64, delta_before: i64) -> u64 {
	let base = if old_count == 0 {
		new_start.saturating_sub(1)
	} else if new_count == 0 {
		new_start.saturating_add(1)
	} else {
		new_start
	};
	if delta_before.is_negative() {
		base.saturating_add(delta_before.unsigned_abs())
	} else {
		base.saturating_sub(delta_before as u64)
	}
}

fn file_matches_path(file: &FileDiff, path: &[u8]) -> bool {
	file.path.as_deref() == Some(path) || file.old_path.as_deref() == Some(path)
}

fn append_selected_hunks(
	patch: &mut BytesMut,
	file: &FileDiff,
	hunks: &[&DiffHunk],
	path: &Str,
) -> Result<(), SelectionError> {
	if hunks.is_empty() {
		return Err(SelectionError::NoMatchingHunks { path: path.clone() });
	}
	let header_end = find_bytes(&file.raw, &file.hunks[0].raw).unwrap_or(file.raw.len());
	patch.extend_from_slice(&file.raw[..header_end]);
	for hunk in hunks {
		patch.extend_from_slice(&hunk.raw);
	}
	if !patch.ends_with(b"\n") {
		patch.extend_from_slice(b"\n");
	}
	Ok(())
}

fn append_patch_part(patch: &mut BytesMut, part: &[u8]) {
	patch.extend_from_slice(part);
	if !part.ends_with(b"\n") {
		patch.extend_from_slice(b"\n");
	}
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
	if needle.is_empty() {
		return Some(0);
	}
	haystack
		.windows(needle.len())
		.position(|window| window == needle)
}

fn noop_output() -> GitRunOutput {
	GitRunOutput {
		exit_code:        0,
		stdout:           Bytes::new(),
		stderr:           Bytes::new(),
		stdout_truncated: false,
		stderr_truncated: false,
		diagnostic:       None,
	}
}
