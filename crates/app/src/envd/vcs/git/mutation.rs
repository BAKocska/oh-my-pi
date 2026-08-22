//! Consumer-facing Git mutation primitives serialized by primary repository
//! root.

use std::{collections::HashSet, path::Path};

use bytes::{Bytes, BytesMut};
use omp_core::{IntoStr, Str};
use tokio_util::sync::CancellationToken;

use super::{
	diff::{DiffOptions, FileDiff, GitDiff},
	lock,
	repo::Repository,
	runner::{GitRunError, GitRunOptions, GitRunOutput, GitRunner},
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
	#[error("path {path} has an invalid new-file line range")]
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
}

/// Failure before Git can return an exact mutation outcome.
#[derive(Debug, thiserror::Error)]
pub enum MutationError {
	/// Repository admission failed.
	#[error(transparent)]
	Lock(#[from] lock::LockError),
	/// Environment execution failed, timed out, or was cancelled.
	#[error(transparent)]
	Run(#[from] GitRunError),
	/// Selective staging was invalid against the captured complete diff.
	#[error(transparent)]
	Selection(#[from] SelectionError),
	/// Complete diff capture was rejected by Git.
	#[error(transparent)]
	Diff(#[from] super::commands::CommandError),
	/// Git emitted a non-UTF-8 scalar where its plumbing contract requires text.
	#[error("Git emitted a non-UTF-8 scalar")]
	NonUtf8,
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
	#[must_use]
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
}

impl GitMutation {
	/// Creates a mutation facade bound to one canonical repository identity.
	pub const fn new(runner: GitRunner, repository: Repository) -> Self {
		Self { runner, repository }
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
		from_cached_diff: bool,
		cancel: &CancellationToken,
	) -> Result<MutationOutcome, MutationError> {
		let _guard = lock::write(&self.repository, cancel).await?;
		if selections.is_empty() {
			return Ok(MutationOutcome::Applied(noop_output()));
		}
		let raw = GitDiff::new(self.runner.clone())
			.raw(
				self.cwd(),
				DiffOptions { cached: from_cached_diff, binary: true, ..Default::default() },
				&[],
				cancel,
			)
			.await?;
		let patch = build_selected_patch(&raw, selections)?;
		self
			.mutation(&["apply", "--binary", "--cached", "-"], Some(&patch), cancel)
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
			let tree = std::str::from_utf8(&output.stdout)
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
				std::str::from_utf8(path)
					.map(Str::from)
					.map_err(|_| MutationError::NonUtf8)
			})
			.collect()
	}
}

fn stash_tracked_paths(patch: &Bytes) -> Result<Vec<Str>, MutationError> {
	let mut paths = Vec::new();
	for file in super::diff::parse_unified(patch.clone()) {
		for path in [file.old_path, file.path].into_iter().flatten() {
			let path = std::str::from_utf8(&path)
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
	let files = super::diff::parse_unified(raw.clone());
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
				let hunks: Vec<_> = file
					.hunks
					.iter()
					.filter(|hunk| {
						let hunk_end = hunk.new_start.saturating_add(hunk.new_count.max(1) - 1);
						hunk.new_start <= *end && hunk_end >= *start
					})
					.collect();
				append_selected_hunks(&mut patch, file, &hunks, &selection.path)?;
			},
		}
	}
	Ok(patch.freeze())
}

fn file_matches_path(file: &FileDiff, path: &[u8]) -> bool {
	file.path.as_deref() == Some(path) || file.old_path.as_deref() == Some(path)
}

fn append_selected_hunks(
	patch: &mut BytesMut,
	file: &FileDiff,
	hunks: &[&super::diff::DiffHunk],
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
