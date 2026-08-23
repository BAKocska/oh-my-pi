//! Typed Git branch, ref, remote, configuration, and checkout commands.

use std::{path::Path, str};

use bytes::Bytes;
use omp_core::{IntoStr, Str};
use tokio_util::sync::CancellationToken;

use super::{
	lock,
	repo::Repository,
	runner::{GitDeadline, GitRunError, GitRunOptions, GitRunOutput, GitRunner},
};

/// A Git command completed unsuccessfully.
#[derive(Debug, thiserror::Error)]
pub enum CommandError {
	/// The process could not produce a complete result.
	#[error(transparent)]
	Run(#[from] GitRunError),
	/// Git rejected the operation.
	#[error("Git command exited with status {code}")]
	Exit {
		/// Process exit code.
		code:   i32,
		/// Bounded standard output.
		stdout: Bytes,
		/// Bounded standard error.
		stderr: Bytes,
	},
	/// A remote name already exists with another URL.
	#[error("Git remote {name} already exists with a different URL")]
	RemoteConflict {
		/// Remote name.
		name:      Str,
		/// Existing URL.
		existing:  Str,
		/// Requested URL.
		requested: Str,
	},
	/// A mutating command was cancelled while waiting for repository authority.
	#[error(transparent)]
	Lock(#[from] lock::LockError),
	/// Git emitted data that is not UTF-8 where its plumbing format requires
	/// UTF-8.
	#[error("Git emitted non-UTF-8 scalar output")]
	NonUtf8,
}

/// Environment-owned typed Git command facade.
#[derive(Clone)]
pub struct GitCommands {
	runner: GitRunner,
}

impl GitCommands {
	/// Creates a command facade over the hardened runner.
	pub const fn new(runner: GitRunner) -> Self {
		Self { runner }
	}

	async fn output(
		&self,
		cwd: &Path,
		argv: &[&str],
		read_only: bool,
		deadline: GitDeadline,
		cancel: &CancellationToken,
	) -> Result<GitRunOutput, CommandError> {
		let result = self
			.runner
			.run(cwd, argv, GitRunOptions { read_only, parse_sensitive: true, deadline }, cancel)
			.await?;
		Ok(result)
	}

	async fn checked(
		&self,
		cwd: &Path,
		argv: &[&str],
		read_only: bool,
		deadline: GitDeadline,
		cancel: &CancellationToken,
	) -> Result<GitRunOutput, CommandError> {
		let result = self.output(cwd, argv, read_only, deadline, cancel).await?;
		if result.exit_code == 0 {
			Ok(result)
		} else {
			Err(CommandError::Exit {
				code:   result.exit_code,
				stdout: result.stdout,
				stderr: result.stderr,
			})
		}
	}

	async fn scalar(
		&self,
		cwd: &Path,
		argv: &[&str],
		cancel: &CancellationToken,
	) -> Result<Option<Str>, CommandError> {
		let result = self
			.output(cwd, argv, true, GitDeadline::Local, cancel)
			.await?;
		if result.exit_code != 0 {
			return Ok(None);
		}
		let value = str::from_utf8(&result.stdout)
			.map_err(|_| CommandError::NonUtf8)?
			.trim();
		Ok((!value.is_empty()).then(|| value.to_str()))
	}

	async fn mutate(
		&self,
		repository: &Repository,
		argv: &[&str],
		deadline: GitDeadline,
		cancel: &CancellationToken,
	) -> Result<(), CommandError> {
		let _guard = lock::write(repository, cancel).await?;
		self
			.checked(&repository.worktree_root, argv, false, deadline, cancel)
			.await?;
		Ok(())
	}

	/// Returns the current branch, or `None` for detached HEAD.
	pub async fn current_branch(
		&self,
		cwd: &Path,
		cancel: &CancellationToken,
	) -> Result<Option<Str>, CommandError> {
		self
			.scalar(cwd, &["symbolic-ref", "--quiet", "--short", "HEAD"], cancel)
			.await
	}

	/// Discovers the symbolic default branch from origin then upstream.
	pub async fn default_branch(
		&self,
		cwd: &Path,
		cancel: &CancellationToken,
	) -> Result<Option<Str>, CommandError> {
		for remote in ["origin", "upstream"] {
			let reference = format!("refs/remotes/{remote}/HEAD");
			if let Some(target) = self
				.scalar(cwd, &["symbolic-ref", "--quiet", reference.as_str()], cancel)
				.await?
			{
				let prefix = format!("refs/remotes/{remote}/");
				if let Some(branch) = target.as_str().strip_prefix(&prefix)
					&& !branch.is_empty()
				{
					return Ok(Some(branch.to_str()));
				}
			}
		}
		Ok(None)
	}

	/// Lists local branches, optionally including remote refs.
	pub async fn list_branches(
		&self,
		cwd: &Path,
		all: bool,
		cancel: &CancellationToken,
	) -> Result<Vec<Str>, CommandError> {
		let argv = if all {
			["branch", "--all", "--format=%(refname:short)"]
		} else {
			["branch", "--list", "--format=%(refname:short)"]
		};
		let output = self
			.checked(cwd, &argv, true, GitDeadline::Local, cancel)
			.await?;
		lines(&output.stdout)
	}

	/// Creates a branch at `start_point`.
	pub async fn create_branch(
		&self,
		repository: &Repository,
		name: &str,
		start_point: &str,
		cancel: &CancellationToken,
	) -> Result<(), CommandError> {
		self
			.mutate(repository, &["branch", name, start_point], GitDeadline::Local, cancel)
			.await
	}

	/// Deletes a branch, forcibly unless `force` is false.
	pub async fn delete_branch(
		&self,
		repository: &Repository,
		name: &str,
		force: bool,
		cancel: &CancellationToken,
	) -> Result<(), CommandError> {
		self
			.mutate(
				repository,
				&["branch", if force { "-D" } else { "-d" }, name],
				GitDeadline::Local,
				cancel,
			)
			.await
	}

	/// Checks out an existing ref.
	pub async fn checkout(
		&self,
		repository: &Repository,
		reference: &str,
		cancel: &CancellationToken,
	) -> Result<(), CommandError> {
		self
			.mutate(repository, &["checkout", reference], GitDeadline::Local, cancel)
			.await
	}

	/// Creates and checks out a branch.
	pub async fn checkout_new(
		&self,
		repository: &Repository,
		name: &str,
		cancel: &CancellationToken,
	) -> Result<(), CommandError> {
		self
			.mutate(repository, &["checkout", "-b", name], GitDeadline::Local, cancel)
			.await
	}

	/// Resolves a revision to its object ID, returning `None` when absent.
	pub async fn resolve_ref(
		&self,
		cwd: &Path,
		reference: &str,
		cancel: &CancellationToken,
	) -> Result<Option<Str>, CommandError> {
		self
			.scalar(cwd, &["rev-parse", "--verify", reference], cancel)
			.await
	}

	/// Tests whether a ref exists.
	pub async fn ref_exists(
		&self,
		cwd: &Path,
		reference: &str,
		cancel: &CancellationToken,
	) -> Result<bool, CommandError> {
		let output = self
			.output(
				cwd,
				&["show-ref", "--verify", "--quiet", reference],
				true,
				GitDeadline::Local,
				cancel,
			)
			.await?;
		Ok(output.exit_code == 0)
	}

	/// Lists version-sorted tags pointing at a ref.
	pub async fn tags(
		&self,
		cwd: &Path,
		reference: &str,
		cancel: &CancellationToken,
	) -> Result<Vec<Str>, CommandError> {
		let output = self
			.checked(
				cwd,
				&[
					"for-each-ref",
					"--points-at",
					reference,
					"--sort=-version:refname",
					"--format=%(refname:strip=2)",
					"refs/tags",
				],
				true,
				GitDeadline::Local,
				cancel,
			)
			.await?;
		lines(&output.stdout)
	}

	/// Lists configured remote names.
	pub async fn remotes(
		&self,
		cwd: &Path,
		cancel: &CancellationToken,
	) -> Result<Vec<Str>, CommandError> {
		let output = self
			.checked(cwd, &["remote"], true, GitDeadline::Local, cancel)
			.await?;
		lines(&output.stdout)
	}

	/// Returns a remote URL when the remote exists.
	pub async fn remote_url(
		&self,
		cwd: &Path,
		name: &str,
		cancel: &CancellationToken,
	) -> Result<Option<Str>, CommandError> {
		self.scalar(cwd, &["remote", "get-url", name], cancel).await
	}

	/// Adds a remote idempotently, rejecting a conflicting existing URL.
	pub async fn add_remote(
		&self,
		repository: &Repository,
		name: &str,
		url: &str,
		cancel: &CancellationToken,
	) -> Result<(), CommandError> {
		let _guard = lock::write(repository, cancel).await?;
		let result = self
			.output(
				&repository.worktree_root,
				&["remote", "add", name, url],
				false,
				GitDeadline::Local,
				cancel,
			)
			.await?;
		if result.exit_code == 0 {
			return Ok(());
		}
		if let Some(existing) = self
			.remote_url(&repository.worktree_root, name, cancel)
			.await?
		{
			if existing.as_str() == url {
				return Ok(());
			}
			return Err(CommandError::RemoteConflict {
				name: name.to_str(),
				existing,
				requested: url.to_str(),
			});
		}
		Err(CommandError::Exit {
			code:   result.exit_code,
			stdout: result.stdout,
			stderr: result.stderr,
		})
	}

	/// Fetches exactly one forced refspec from a remote.
	pub async fn fetch_refspec(
		&self,
		repository: &Repository,
		remote: &str,
		source: &str,
		target: &str,
		cancel: &CancellationToken,
	) -> Result<(), CommandError> {
		let refspec = format!("+{source}:{target}");
		self
			.mutate(
				repository,
				&["fetch", "--no-tags", remote, refspec.as_str()],
				GitDeadline::Network,
				cancel,
			)
			.await
	}

	/// Reads a Git configuration scalar.
	pub async fn config_get(
		&self,
		cwd: &Path,
		key: &str,
		cancel: &CancellationToken,
	) -> Result<Option<Str>, CommandError> {
		self.scalar(cwd, &["config", "--get", key], cancel).await
	}

	/// Writes a Git configuration scalar under repository serialization.
	pub async fn config_set(
		&self,
		repository: &Repository,
		key: &str,
		value: &str,
		cancel: &CancellationToken,
	) -> Result<(), CommandError> {
		self
			.mutate(repository, &["config", key, value], GitDeadline::Local, cancel)
			.await
	}

	/// Resolves `prefix` relative to the repository worktree and returns Git's
	/// canonical workdir-relative prefix.
	pub async fn workdir_prefix(
		&self,
		cwd: &Path,
		cancel: &CancellationToken,
	) -> Result<Option<Str>, CommandError> {
		self
			.scalar(cwd, &["rev-parse", "--show-prefix"], cancel)
			.await
	}
}

fn lines(bytes: &[u8]) -> Result<Vec<Str>, CommandError> {
	let text = str::from_utf8(bytes).map_err(|_| CommandError::NonUtf8)?;
	Ok(text
		.lines()
		.filter(|line| !line.is_empty())
		.map(|line| line.to_str())
		.collect())
}
