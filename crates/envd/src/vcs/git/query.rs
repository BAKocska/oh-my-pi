//! NUL-safe repository file and history queries.

use std::path::Path;

use bytes::Bytes;
use omp_core::{IntoStr, Str};
use tokio_util::sync::CancellationToken;

use super::{
	commands::CommandError,
	runner::{GitRunOptions, GitRunner},
};

/// A repository-relative path preserved as raw platform bytes.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GitPath(Bytes);

impl GitPath {
	/// Borrows the exact bytes emitted by Git.
	pub fn as_bytes(&self) -> &[u8] {
		&self.0
	}

	pub(super) fn from_bytes(bytes: &[u8]) -> Self {
		Self(Bytes::copy_from_slice(bytes))
	}
}

/// Commit author and message metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitMetadata {
	/// Full object ID.
	pub commit:       Str,
	/// Author name.
	pub author_name:  Str,
	/// Author email.
	pub author_email: Str,
	/// Strict ISO-8601 author date.
	pub author_date:  Str,
	/// Complete commit message body, including embedded newlines.
	pub body:         Str,
}

/// Typed read-only Git query facade.
#[derive(Clone)]
pub struct GitQuery {
	runner: GitRunner,
}

impl GitQuery {
	/// Creates a query facade over the hardened runner.
	pub const fn new(runner: GitRunner) -> Self {
		Self { runner }
	}

	async fn bytes(
		&self,
		cwd: &Path,
		argv: &[&str],
		cancel: &CancellationToken,
	) -> Result<Bytes, CommandError> {
		let output = self
			.runner
			.run(
				cwd,
				argv,
				GitRunOptions { read_only: true, parse_sensitive: true, ..Default::default() },
				cancel,
			)
			.await?;
		if output.exit_code != 0 {
			return Err(CommandError::Exit {
				code:   output.exit_code,
				stdout: output.stdout,
				stderr: output.stderr,
			});
		}
		Ok(output.stdout)
	}

	/// Lists tracked files without decoding or line-splitting path bytes.
	pub async fn tracked(
		&self,
		cwd: &Path,
		cancel: &CancellationToken,
	) -> Result<Vec<GitPath>, CommandError> {
		Ok(parse_nul_paths(self.bytes(cwd, &["ls-files", "-z"], cancel).await?))
	}

	/// Lists untracked, non-ignored files without lossy path decoding.
	pub async fn untracked(
		&self,
		cwd: &Path,
		cancel: &CancellationToken,
	) -> Result<Vec<GitPath>, CommandError> {
		Ok(parse_nul_paths(
			self
				.bytes(cwd, &["ls-files", "--others", "--exclude-standard", "-z"], cancel)
				.await?,
		))
	}

	/// Lists paths contained in a tree, optionally limited by literal pathspecs.
	pub async fn tree(
		&self,
		cwd: &Path,
		tree: &str,
		paths: &[&str],
		cancel: &CancellationToken,
	) -> Result<Vec<GitPath>, CommandError> {
		let mut argv = Vec::with_capacity(5 + paths.len());
		argv.extend(["ls-tree", "-r", "-z", "--name-only", tree]);
		if !paths.is_empty() {
			argv.push("--");
			argv.extend_from_slice(paths);
		}
		Ok(parse_nul_paths(self.bytes(cwd, &argv, cancel).await?))
	}

	/// Lists tracked gitlink paths and initialized nested submodule paths. The
	/// fixed recursive helper emits NUL-framed display paths.
	pub async fn submodules(
		&self,
		cwd: &Path,
		cancel: &CancellationToken,
	) -> Result<Vec<GitPath>, CommandError> {
		let bytes = self
			.bytes(cwd, &["ls-files", "--stage", "-z"], cancel)
			.await?;
		let mut paths = Vec::new();
		for entry in bytes
			.split(|byte| *byte == 0)
			.filter(|entry| !entry.is_empty())
		{
			if entry.starts_with(b"160000 ")
				&& let Some(tab) = entry.iter().position(|byte| *byte == b'\t')
			{
				paths.push(GitPath(Bytes::copy_from_slice(&entry[tab + 1..])));
			}
		}
		let nested = self
			.bytes(
				cwd,
				&["submodule", "foreach", "--recursive", "--quiet", "printf '%s\\0' \"$displaypath\""],
				cancel,
			)
			.await?;
		for path in parse_nul_paths(nested) {
			if !paths.iter().any(|existing| existing == &path) {
				paths.push(path);
			}
		}
		Ok(paths)
	}

	/// Returns recent commit subjects.
	pub async fn log_subjects(
		&self,
		cwd: &Path,
		count: usize,
		cancel: &CancellationToken,
	) -> Result<Vec<Str>, CommandError> {
		let count = format!("-n{count}");
		parse_lines(
			self
				.bytes(cwd, &["log", count.as_str(), "--pretty=format:%s"], cancel)
				.await?,
		)
	}

	/// Returns recent `<short-sha> <subject>` lines without decorations.
	pub async fn log_onelines(
		&self,
		cwd: &Path,
		count: usize,
		cancel: &CancellationToken,
	) -> Result<Vec<Str>, CommandError> {
		let count = format!("-{count}");
		parse_lines(
			self
				.bytes(cwd, &["log", count.as_str(), "--oneline", "--no-decorate"], cancel)
				.await?,
		)
	}

	/// Lists commits in `base..head`, oldest first.
	pub async fn rev_list_range(
		&self,
		cwd: &Path,
		base: &str,
		head: &str,
		cancel: &CancellationToken,
	) -> Result<Vec<Str>, CommandError> {
		let range = format!("{base}..{head}");
		parse_lines(
			self
				.bytes(cwd, &["rev-list", "--reverse", range.as_str()], cancel)
				.await?,
		)
	}

	/// Lists commits touching one literal path, newest first and bounded by
	/// `limit`.
	pub async fn rev_list_touching(
		&self,
		cwd: &Path,
		reference: &str,
		path: &str,
		limit: usize,
		cancel: &CancellationToken,
	) -> Result<Vec<Str>, CommandError> {
		let limit = format!("--max-count={limit}");
		parse_lines(
			self
				.bytes(cwd, &["rev-list", limit.as_str(), reference, "--", path], cancel)
				.await?,
		)
	}

	/// Reads author, date, and complete body metadata using NUL field framing.
	pub async fn commit_metadata(
		&self,
		cwd: &Path,
		revision: &str,
		cancel: &CancellationToken,
	) -> Result<CommitMetadata, CommandError> {
		let bytes = self
			.bytes(
				cwd,
				&["show", "-s", "--format=%H%x00%an%x00%ae%x00%aI%x00%B%x00", revision],
				cancel,
			)
			.await?;
		let mut fields = bytes.split(|byte| *byte == 0);
		let mut next = || -> Result<Str, CommandError> {
			let field = fields.next().ok_or(CommandError::NonUtf8)?;
			std::str::from_utf8(field)
				.map(|value| value.to_str())
				.map_err(|_| CommandError::NonUtf8)
		};
		let metadata = CommitMetadata {
			commit:       next()?,
			author_name:  next()?,
			author_email: next()?,
			author_date:  next()?,
			body:         next()?,
		};
		if fields.any(|field| !field.is_empty() && field != b"\n") {
			return Err(CommandError::NonUtf8);
		}
		Ok(metadata)
	}
}

fn parse_nul_paths(bytes: Bytes) -> Vec<GitPath> {
	let mut paths = Vec::new();
	let mut start = 0;
	for (index, byte) in bytes.iter().enumerate() {
		if *byte == 0 {
			if index > start {
				paths.push(GitPath(bytes.slice(start..index)));
			}
			start = index + 1;
		}
	}
	if start < bytes.len() {
		paths.push(GitPath(bytes.slice(start..)));
	}
	paths
}

fn parse_lines(bytes: Bytes) -> Result<Vec<Str>, CommandError> {
	let text = std::str::from_utf8(&bytes).map_err(|_| CommandError::NonUtf8)?;
	Ok(text
		.lines()
		.filter(|line| !line.is_empty())
		.map(|line| line.to_str())
		.collect())
}
