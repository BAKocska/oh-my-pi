//! Typed Git branch, ref, remote, configuration, and checkout commands.

use std::{
	cmp::Ordering,
	path::Path,
	str,
	sync::atomic::{AtomicBool, Ordering as AtomicOrdering},
};

use bytes::Bytes;
use gix::bstr::ByteSlice;
use omp_core::{IntoStr, Str, sf};
use tokio_util::sync::CancellationToken;

use super::{
	lock, native,
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
///
/// Reads run in-process via gitoxide with system-Git fallback; mutations stay
/// on system Git for hook, lock, and worktree semantics.
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
		match native::with_repository(cwd, cancel, native_current_branch).await {
			Ok(value) => return Ok(value),
			Err(error) if error.is_cancelled() => return Err(GitRunError::Cancelled.into()),
			Err(error) => tracing::debug!(%error, "in-process Git read fell back to system Git"),
		}
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
		match native::with_repository(cwd, cancel, native_default_branch).await {
			Ok(value) => return Ok(value),
			Err(error) if error.is_cancelled() => return Err(GitRunError::Cancelled.into()),
			Err(error) => tracing::debug!(%error, "in-process Git read fell back to system Git"),
		}
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
		match native::with_repository(cwd, cancel, move |repository, stop| {
			native_list_branches(repository, stop, all)
		})
		.await
		{
			Ok(value) => return Ok(value),
			Err(error) if error.is_cancelled() => return Err(GitRunError::Cancelled.into()),
			Err(error) => tracing::debug!(%error, "in-process Git read fell back to system Git"),
		}
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
		let native_reference = reference.to_owned();
		match native::with_repository(cwd, cancel, move |repository, stop| {
			native_resolve_ref(repository, stop, &native_reference)
		})
		.await
		{
			Ok(value) => return Ok(value),
			Err(error) if error.is_cancelled() => return Err(GitRunError::Cancelled.into()),
			Err(error) => tracing::debug!(%error, "in-process Git read fell back to system Git"),
		}
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
		let native_reference = reference.to_owned();
		match native::with_repository(cwd, cancel, move |repository, stop| {
			native_ref_exists(repository, stop, &native_reference)
		})
		.await
		{
			Ok(value) => return Ok(value),
			Err(error) if error.is_cancelled() => return Err(GitRunError::Cancelled.into()),
			Err(error) => tracing::debug!(%error, "in-process Git read fell back to system Git"),
		}
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
		let native_reference = reference.to_owned();
		match native::with_repository(cwd, cancel, move |repository, stop| {
			native_tags(repository, stop, &native_reference)
		})
		.await
		{
			Ok(value) => return Ok(value),
			Err(error) if error.is_cancelled() => return Err(GitRunError::Cancelled.into()),
			Err(error) => tracing::debug!(%error, "in-process Git read fell back to system Git"),
		}
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
		match native::with_repository(cwd, cancel, native_remotes).await {
			Ok(value) => return Ok(value),
			Err(error) if error.is_cancelled() => return Err(GitRunError::Cancelled.into()),
			Err(error) => tracing::debug!(%error, "in-process Git read fell back to system Git"),
		}
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
		let native_name = name.to_owned();
		match native::with_repository(cwd, cancel, move |repository, stop| {
			native_remote_url(repository, stop, &native_name)
		})
		.await
		{
			Ok(value) => return Ok(value),
			Err(error) if error.is_cancelled() => return Err(GitRunError::Cancelled.into()),
			Err(error) => tracing::debug!(%error, "in-process Git read fell back to system Git"),
		}
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
		let native_key = key.to_owned();
		match native::with_repository(cwd, cancel, move |repository, stop| {
			native_config_get(repository, stop, &native_key)
		})
		.await
		{
			Ok(value) => return Ok(value),
			Err(error) if error.is_cancelled() => return Err(GitRunError::Cancelled.into()),
			Err(error) => tracing::debug!(%error, "in-process Git read fell back to system Git"),
		}
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
		match native::with_repository(cwd, cancel, native_workdir_prefix).await {
			Ok(value) => return Ok(value),
			Err(error) if error.is_cancelled() => return Err(GitRunError::Cancelled.into()),
			Err(error) => tracing::debug!(%error, "in-process Git read fell back to system Git"),
		}
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

fn cancelled(stop: &AtomicBool) -> Result<(), native::NativeError> {
	if stop.load(AtomicOrdering::Relaxed) {
		Err(native::NativeError::Cancelled)
	} else {
		Ok(())
	}
}

fn native_current_branch(
	repository: &mut gix::Repository,
	stop: &AtomicBool,
) -> Result<Option<Str>, native::NativeError> {
	cancelled(stop)?;
	let head = repository.head().map_err(native::op_error)?;
	let name = match &head.kind {
		gix::head::Kind::Symbolic(reference) => reference.name.as_bstr(),
		gix::head::Kind::Unborn(reference) => reference.as_bstr(),
		gix::head::Kind::Detached { .. } => return Ok(None),
	};
	name
		.strip_prefix(b"refs/heads/")
		.filter(|name| !name.is_empty())
		.map(native_utf8)
		.transpose()
}

fn native_default_branch(
	repository: &mut gix::Repository,
	stop: &AtomicBool,
) -> Result<Option<Str>, native::NativeError> {
	for remote in ["origin", "upstream"] {
		cancelled(stop)?;
		let name = format!("refs/remotes/{remote}/HEAD");
		let Some(reference) = repository
			.try_find_reference(name.as_str())
			.map_err(native::op_error)?
		else {
			continue;
		};
		let gix::refs::TargetRef::Symbolic(target) = reference.target() else {
			continue;
		};
		let prefix = format!("refs/remotes/{remote}/");
		if let Some(branch) = target
			.as_bstr()
			.strip_prefix(prefix.as_bytes())
			.filter(|name| !name.is_empty())
		{
			return native_utf8(branch).map(Some);
		}
	}
	Ok(None)
}

fn native_list_branches(
	repository: &mut gix::Repository,
	stop: &AtomicBool,
	all: bool,
) -> Result<Vec<Str>, native::NativeError> {
	let mut branches = native_refs_with_prefix(repository, stop, b"refs/heads/")?;
	if all {
		branches.extend(native_refs_with_prefix(repository, stop, b"refs/remotes/")?);
	}
	Ok(branches)
}

fn native_refs_with_prefix(
	repository: &mut gix::Repository,
	stop: &AtomicBool,
	prefix: &[u8],
) -> Result<Vec<Str>, native::NativeError> {
	let references = repository.references().map_err(native::op_error)?;
	let mut values = Vec::new();
	for reference in references.prefixed(prefix).map_err(native::op_error)? {
		cancelled(stop)?;
		let reference = reference.map_err(native::NativeError::Operation)?;
		let name = reference
			.name()
			.as_bstr()
			.strip_prefix(prefix)
			.expect("reference iterator returned its requested prefix");
		values.push(native_utf8(name)?);
	}
	Ok(values)
}

fn native_resolve_ref(
	repository: &mut gix::Repository,
	stop: &AtomicBool,
	reference: &str,
) -> Result<Option<Str>, native::NativeError> {
	cancelled(stop)?;
	Ok(repository
		.rev_parse_single(reference)
		.ok()
		.map(|id| sf!("{}", id)))
}

fn native_ref_exists(
	repository: &mut gix::Repository,
	stop: &AtomicBool,
	reference: &str,
) -> Result<bool, native::NativeError> {
	cancelled(stop)?;
	if !reference.starts_with("refs/") {
		return Ok(false);
	}
	Ok(repository
		.try_find_reference(reference)
		.map_err(native::op_error)?
		.is_some())
}

fn native_tags(
	repository: &mut gix::Repository,
	stop: &AtomicBool,
	reference: &str,
) -> Result<Vec<Str>, native::NativeError> {
	cancelled(stop)?;
	let Ok(target) = repository.rev_parse_single(reference) else {
		return Ok(Vec::new());
	};
	let target = target.detach();
	let references = repository.references().map_err(native::op_error)?;
	let mut tags = Vec::new();
	for reference in references.tags().map_err(native::op_error)? {
		cancelled(stop)?;
		let mut reference = reference.map_err(native::NativeError::Operation)?;
		let direct = reference.try_id().map(gix::Id::detach);
		let peeled = reference.peel_to_id().map_err(native::op_error)?.detach();
		if direct == Some(target) || peeled == target {
			let name = reference
				.name()
				.as_bstr()
				.strip_prefix(b"refs/tags/")
				.expect("tag iterator returned a tag");
			tags.push(native_utf8(name)?);
		}
	}
	tags.sort_by(|left, right| versioncmp(right.as_str(), left.as_str()));
	Ok(tags)
}

fn native_remotes(
	repository: &mut gix::Repository,
	stop: &AtomicBool,
) -> Result<Vec<Str>, native::NativeError> {
	repository
		.remote_names()
		.into_iter()
		.map(|name| {
			cancelled(stop)?;
			native_utf8(name.as_bstr())
		})
		.collect()
}

fn native_remote_url(
	repository: &mut gix::Repository,
	stop: &AtomicBool,
	name: &str,
) -> Result<Option<Str>, native::NativeError> {
	cancelled(stop)?;
	let Some(remote) = repository.try_find_remote(name) else {
		return Ok(None);
	};
	let remote = remote.map_err(native::op_error)?;
	let Some(url) = remote.url(gix::remote::Direction::Fetch) else {
		return Ok(None);
	};
	native_utf8(url.to_bstring().as_bstr()).map(Some)
}

fn native_config_get(
	repository: &mut gix::Repository,
	stop: &AtomicBool,
	key: &str,
) -> Result<Option<Str>, native::NativeError> {
	cancelled(stop)?;
	let Some(value) = repository.config_snapshot().string(key) else {
		return Ok(None);
	};
	let value = str::from_utf8(value.as_ref())
		.map_err(|_| native::op_error(CommandError::NonUtf8))?
		.trim();
	Ok((!value.is_empty()).then(|| value.to_str()))
}

fn native_workdir_prefix(
	repository: &mut gix::Repository,
	stop: &AtomicBool,
) -> Result<Option<Str>, native::NativeError> {
	cancelled(stop)?;
	let Some(prefix) = repository.prefix().map_err(native::op_error)? else {
		return Ok(None);
	};
	if prefix.as_os_str().is_empty() {
		return Ok(None);
	}
	let value = prefix
		.to_str()
		.ok_or_else(|| native::op_error(CommandError::NonUtf8))?;
	Ok(Some(format!("{value}/").replace('\\', "/").to_str()))
}

fn native_utf8(bytes: &[u8]) -> Result<Str, native::NativeError> {
	str::from_utf8(bytes)
		.map(|value| value.to_str())
		.map_err(|_| native::op_error(CommandError::NonUtf8))
}

fn versioncmp(left: &str, right: &str) -> Ordering {
	let (left, right) = (left.as_bytes(), right.as_bytes());
	let (mut left_at, mut right_at) = (0, 0);
	while left_at < left.len() && right_at < right.len() {
		if left[left_at].is_ascii_digit() && right[right_at].is_ascii_digit() {
			let left_end = left_at
				+ left[left_at..]
					.iter()
					.take_while(|byte| byte.is_ascii_digit())
					.count();
			let right_end = right_at
				+ right[right_at..]
					.iter()
					.take_while(|byte| byte.is_ascii_digit())
					.count();
			let order = version_digits(&left[left_at..left_end], &right[right_at..right_end]);
			if order != Ordering::Equal {
				return order;
			}
			left_at = left_end;
			right_at = right_end;
		} else {
			let order = left[left_at].cmp(&right[right_at]);
			if order != Ordering::Equal {
				return order;
			}
			left_at += 1;
			right_at += 1;
		}
	}
	left.len().cmp(&right.len())
}

fn version_digits(left: &[u8], right: &[u8]) -> Ordering {
	let left_significant = left
		.iter()
		.position(|byte| *byte != b'0')
		.unwrap_or(left.len());
	let right_significant = right
		.iter()
		.position(|byte| *byte != b'0')
		.unwrap_or(right.len());
	let left_digits = &left[left_significant..];
	let right_digits = &right[right_significant..];
	left_digits
		.len()
		.cmp(&right_digits.len())
		.then_with(|| left_digits.cmp(right_digits))
		.then_with(|| right_significant.cmp(&left_significant))
}

#[cfg(test)]
mod tests {
	use std::{
		fs,
		path::Path,
		process::Command,
		sync::atomic::AtomicBool,
	};

	use super::*;

	fn git(cwd: &Path, arguments: &[&str]) {
		let output = Command::new("git").current_dir(cwd).args(arguments).output().expect("fixture git launches");
		assert!(output.status.success(), "git {arguments:?}: {}", String::from_utf8_lossy(&output.stderr));
	}

	fn fixture() -> tempfile::TempDir {
		let root = tempfile::tempdir().expect("temporary repository");
		git(root.path(), &["init", "-b", "main"]);
		git(root.path(), &["config", "user.name", "OMP Test"]);
		git(root.path(), &["config", "user.email", "omp@example.invalid"]);
		fs::write(root.path().join("seed"), "seed").expect("seed");
		git(root.path(), &["add", "seed"]);
		git(root.path(), &["commit", "-m", "seed"]);
		root
	}

	fn repository(path: &Path) -> gix::Repository {
		gix::discover(path).expect("discover fixture")
	}

	#[test]
	fn native_refs_and_head_paths_match_git() {
		let stop = AtomicBool::new(false);
		let root = fixture();
		let mut repo = repository(root.path());
		assert_eq!(native_current_branch(&mut repo, &stop).expect("head").as_deref(), Some("main"));
		assert!(native_resolve_ref(&mut repo, &stop, "HEAD").expect("resolve").is_some());
		assert_eq!(native_resolve_ref(&mut repo, &stop, "missing").expect("missing"), None);
		assert!(native_ref_exists(&mut repo, &stop, "refs/heads/main").expect("exists"));
		assert!(!native_ref_exists(&mut repo, &stop, "main").expect("not full ref"));
		git(root.path(), &["branch", "topic"]);
		git(root.path(), &["update-ref", "refs/remotes/origin/x", "HEAD"]);
		git(root.path(), &["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/x"]);
		assert_eq!(native_default_branch(&mut repo, &stop).expect("default").as_deref(), Some("x"));
		assert_eq!(native_list_branches(&mut repo, &stop, false).expect("locals"), vec!["main".to_str(), "topic".to_str()]);
		assert_eq!(native_list_branches(&mut repo, &stop, true).expect("all"), vec!["main".to_str(), "topic".to_str(), "origin/HEAD".to_str(), "origin/x".to_str()]);
		git(root.path(), &["checkout", "--detach"]);
		let mut detached = repository(root.path());
		assert_eq!(native_current_branch(&mut detached, &stop).expect("detached"), None);
		let unborn = tempfile::tempdir().expect("unborn root");
		git(unborn.path(), &["init", "-b", "main"]);
		let mut unborn = repository(unborn.path());
		assert_eq!(native_current_branch(&mut unborn, &stop).expect("unborn").as_deref(), Some("main"));
	}

	#[test]
	fn native_tags_remotes_config_and_prefix_match_git() {
		let stop = AtomicBool::new(false);
		let root = fixture();
		git(root.path(), &["tag", "v1.9"]);
		git(root.path(), &["tag", "-a", "v1.10", "-m", "tag"]);
		git(root.path(), &["tag", "v2.0"]);
		git(root.path(), &["remote", "add", "origin", "https://old.example/repo"]);
		git(root.path(), &["config", "answer.value", "  value  "]);
		let mut repo = repository(root.path());
		assert_eq!(native_tags(&mut repo, &stop, "HEAD").expect("tags"), vec!["v2.0".to_str(), "v1.10".to_str(), "v1.9".to_str()]);
		assert_eq!(native_remotes(&mut repo, &stop).expect("remotes"), vec!["origin".to_str()]);
		assert_eq!(native_remote_url(&mut repo, &stop, "origin").expect("url").as_deref(), Some("https://old.example/repo"));
		assert_eq!(native_remote_url(&mut repo, &stop, "missing").expect("missing url"), None);
		assert_eq!(native_config_get(&mut repo, &stop, "answer.value").expect("config").as_deref(), Some("value"));
		assert_eq!(native_config_get(&mut repo, &stop, "missing.value").expect("missing config"), None);
		assert_eq!(native_workdir_prefix(&mut repo, &stop).expect("root prefix"), None);
		fs::create_dir(root.path().join("nested")).expect("nested");
		let mut nested = repository(&root.path().join("nested"));
		assert_eq!(native_workdir_prefix(&mut nested, &stop).expect("nested prefix").as_deref(), Some("nested/"));
	}

	#[test]
	fn version_comparison_matches_version_sort_edges() {
		assert!(versioncmp("v1.9", "v1.10").is_lt());
		assert!(versioncmp("v1.2", "v1.2.1").is_lt());
		assert!(versioncmp("002", "02").is_lt());
		assert!(versioncmp("02", "2").is_lt());
		assert_eq!(versioncmp("v1.0", "v1.0"), Ordering::Equal);
	}
}
