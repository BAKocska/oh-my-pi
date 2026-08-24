//! Environment-owned version-control substrate.

use std::path::{Path, PathBuf};

use omp_core::{IntoStr, Str};
use tokio_util::sync::CancellationToken;

use self::git::{
	commands::CommandError,
	diff::{GitDiff, StatusCounts},
	refs,
	repo::{self, RepositoryError},
	runner::{GitDiagnostic, GitRunError, GitRunOptions, GitRunner},
};

pub mod git;

/// Availability of repository facts from Environment authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryAvailability {
	/// Git metadata and the configured system Git are available.
	Available,
	/// No Git repository contains the requested root.
	NotRepository,
	/// Repository metadata exists but the configured system Git is unavailable.
	GitUnavailable,
}

/// Immutable repository facts passed to prompts and presentation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositorySnapshot {
	/// Fact availability; unavailable facts are never guessed.
	pub availability:  RepositoryAvailability,
	/// Canonical selected worktree root when repository metadata exists.
	pub worktree_root: Option<PathBuf>,
	/// Canonical primary repository identity, shared by linked worktrees.
	pub primary_root:  Option<PathBuf>,
	/// Full HEAD object ID, absent for unborn or unavailable state.
	pub head:          Option<Str>,
	/// Local branch name, absent for detached or unavailable state.
	pub branch:        Option<Str>,
	/// Staged, unstaged, and untracked counts; zero only means clean when
	/// availability is `Available`.
	pub status_counts: StatusCounts,
}

/// Repository snapshot acquisition failure.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
	/// Repository metadata discovery failed.
	#[error(transparent)]
	Repository(#[from] RepositoryError),
	/// Git execution failed before an availability decision could be made.
	#[error(transparent)]
	Run(#[from] GitRunError),
	/// HEAD metadata was malformed.
	#[error(transparent)]
	Ref(#[from] refs::RefError),
	/// Status capture failed.
	#[error(transparent)]
	Command(#[from] CommandError),
}

/// Captures one complete immutable repository snapshot asynchronously without
/// probing system Git for ordinary file-ref repositories.
pub async fn snapshot(
	root: &Path,
	runner: &GitRunner,
	cancel: &CancellationToken,
) -> Result<RepositorySnapshot, SnapshotError> {
	let Some(repository) = repo::discover(root).await? else {
		return Ok(RepositorySnapshot {
			availability:  RepositoryAvailability::NotRepository,
			worktree_root: None,
			primary_root:  None,
			head:          None,
			branch:        None,
			status_counts: StatusCounts::default(),
		});
	};
	if refs::is_reftable(&repository).await?
		&& !system_git_available(runner, &repository, cancel).await?
	{
		return Ok(git_unavailable_snapshot(&repository));
	}
	let head = refs::resolve_head(&repository, runner, cancel).await?;
	let status_counts = if repository.bare {
		StatusCounts::default()
	} else {
		match GitDiff::new(runner.clone())
			.status_counts(&repository.worktree_root, cancel)
			.await
		{
			Ok(counts) => counts,
			Err(error) if system_git_missing_error(&error) => {
				if !system_git_available(runner, &repository, cancel).await? {
					return Ok(git_unavailable_snapshot(&repository));
				}
				return Err(error.into());
			},
			Err(error) => return Err(error.into()),
		}
	};
	Ok(RepositorySnapshot {
		availability: RepositoryAvailability::Available,
		worktree_root: Some(repository.worktree_root),
		primary_root: Some(repository.primary_root),
		head: head.commit().map(|value| value.to_str()),
		branch: head.branch().map(|value| value.to_str()),
		status_counts,
	})
}

fn git_unavailable_snapshot(repository: &repo::Repository) -> RepositorySnapshot {
	RepositorySnapshot {
		availability:  RepositoryAvailability::GitUnavailable,
		worktree_root: Some(repository.worktree_root.clone()),
		primary_root:  Some(repository.primary_root.clone()),
		head:          None,
		branch:        None,
		status_counts: StatusCounts::default(),
	}
}

async fn system_git_available(
	runner: &GitRunner,
	repository: &repo::Repository,
	cancel: &CancellationToken,
) -> Result<bool, GitRunError> {
	let probe = runner
		.run(
			&repository.worktree_root,
			&["--version"],
			GitRunOptions { read_only: true, parse_sensitive: true, ..Default::default() },
			cancel,
		)
		.await?;
	Ok(probe.exit_code == 0 && probe.diagnostic != Some(GitDiagnostic::GitMissing))
}

fn system_git_missing_error(error: &CommandError) -> bool {
	matches!(error, CommandError::Exit { code: 127, .. } | CommandError::Run(_))
}
