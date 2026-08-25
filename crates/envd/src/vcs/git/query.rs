//! NUL-safe repository file and history queries.

use std::{
	collections::{BinaryHeap, HashSet},
	path::Path,
	str,
	sync::atomic::{AtomicBool, Ordering},
};

use bytes::Bytes;
use omp_core::{IntoStr, Str};
use tokio_util::sync::CancellationToken;

use super::{
	commands::CommandError,
	native::{self, NativeError},
	runner::{GitRunError, GitRunOptions, GitRunner},
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
	pub hash:         Str,
	/// Full object IDs of this commit's parents, in commit order.
	pub parents:      Vec<Str>,
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

	/// Reads exact blob bytes from any `git show` object spec.
	///
	/// Examples include `HEAD:path`, `<object-id>:path`, and `:0:path`.
	pub async fn show_path(
		&self,
		cwd: &Path,
		spec: &str,
		cancel: &CancellationToken,
	) -> Result<Bytes, CommandError> {
		self.bytes(cwd, &["show", spec], cancel).await
	}

	/// Streams exact blob stdout frames from one `git show` object spec.
	///
	/// `on_stdout` observes bounded chunks as they arrive. The returned bytes
	/// are the complete bounded stdout so callers can resolve binary and media
	/// content after progressive text delivery.
	pub async fn show_path_stream(
		&self,
		cwd: &Path,
		spec: &str,
		cancel: &CancellationToken,
		on_stdout: &mut (impl FnMut(Bytes) + Send),
	) -> Result<Bytes, CommandError> {
		let output = self
			.runner
			.run_stream(
				cwd,
				&["show", spec],
				GitRunOptions { read_only: true, parse_sensitive: true, ..Default::default() },
				cancel,
				on_stdout,
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

	/// Lists tracked files in-process via gitoxide, falling back to `git
	/// ls-files -z`.
	pub async fn tracked(
		&self,
		cwd: &Path,
		cancel: &CancellationToken,
	) -> Result<Vec<GitPath>, CommandError> {
		match native::with_repository(cwd, cancel, native_tracked).await {
			Ok(value) => return Ok(value),
			Err(error) if error.is_cancelled() => return Err(GitRunError::Cancelled.into()),
			Err(error) => tracing::debug!(%error, "in-process Git read fell back to system Git"),
		}
		Ok(parse_nul_paths(self.bytes(cwd, &["ls-files", "-z"], cancel).await?))
	}

	/// Lists untracked, non-ignored files in-process via gitoxide, falling back
	/// to `git ls-files --others --exclude-standard -z`.
	pub async fn untracked(
		&self,
		cwd: &Path,
		cancel: &CancellationToken,
	) -> Result<Vec<GitPath>, CommandError> {
		match native::with_repository(cwd, cancel, native_untracked).await {
			Ok(value) => return Ok(value),
			Err(error) if error.is_cancelled() => return Err(GitRunError::Cancelled.into()),
			Err(error) => tracing::debug!(%error, "in-process Git read fell back to system Git"),
		}
		Ok(parse_nul_paths(
			self
				.bytes(cwd, &["ls-files", "--others", "--exclude-standard", "-z"], cancel)
				.await?,
		))
	}

	/// Lists paths contained in a tree in-process via gitoxide, falling back to
	/// `git ls-tree -r -z --name-only TREE [-- paths]`.
	pub async fn tree(
		&self,
		cwd: &Path,
		tree: &str,
		paths: &[&str],
		cancel: &CancellationToken,
	) -> Result<Vec<GitPath>, CommandError> {
		let tree_owned = tree.to_owned();
		let paths_owned = paths
			.iter()
			.map(|path| (*path).to_owned())
			.collect::<Vec<_>>();
		match native::with_repository(cwd, cancel, move |repository, stop| {
			native_tree(repository, stop, &tree_owned, &paths_owned)
		})
		.await
		{
			Ok(value) => return Ok(value),
			Err(error) if error.is_cancelled() => return Err(GitRunError::Cancelled.into()),
			Err(error) => tracing::debug!(%error, "in-process Git read fell back to system Git"),
		}
		let mut argv = Vec::with_capacity(5 + paths.len());
		argv.extend(["ls-tree", "-r", "-z", "--name-only", tree]);
		if !paths.is_empty() {
			argv.push("--");
			argv.extend_from_slice(paths);
		}
		Ok(parse_nul_paths(self.bytes(cwd, &argv, cancel).await?))
	}

	/// Lists tracked gitlinks and initialized nested submodules in-process via
	/// gitoxide, falling back to `git ls-files --stage -z` and `git submodule
	/// foreach`.
	pub async fn submodules(
		&self,
		cwd: &Path,
		cancel: &CancellationToken,
	) -> Result<Vec<GitPath>, CommandError> {
		match native::with_repository(cwd, cancel, native_submodules).await {
			Ok(value) => return Ok(value),
			Err(error) if error.is_cancelled() => return Err(GitRunError::Cancelled.into()),
			Err(error) => tracing::debug!(%error, "in-process Git read fell back to system Git"),
		}
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

	/// Returns recent commit subjects in-process via gitoxide, falling back to
	/// `git log -nN --pretty=format:%s`.
	pub async fn log_subjects(
		&self,
		cwd: &Path,
		count: usize,
		cancel: &CancellationToken,
	) -> Result<Vec<Str>, CommandError> {
		match native::with_repository(cwd, cancel, move |repository, stop| {
			native_log_subjects(repository, stop, count)
		})
		.await
		{
			Ok(value) => return Ok(value),
			Err(error) if error.is_cancelled() => return Err(GitRunError::Cancelled.into()),
			Err(error) => tracing::debug!(%error, "in-process Git read fell back to system Git"),
		}
		let count = format!("-n{count}");
		parse_lines(
			self
				.bytes(cwd, &["log", count.as_str(), "--pretty=format:%s"], cancel)
				.await?,
		)
	}

	/// Returns recent `<short-sha> <subject>` lines in-process via gitoxide,
	/// falling back to `git log -N --oneline --no-decorate`.
	pub async fn log_onelines(
		&self,
		cwd: &Path,
		count: usize,
		cancel: &CancellationToken,
	) -> Result<Vec<Str>, CommandError> {
		match native::with_repository(cwd, cancel, move |repository, stop| {
			native_log_onelines(repository, stop, count)
		})
		.await
		{
			Ok(value) => return Ok(value),
			Err(error) if error.is_cancelled() => return Err(GitRunError::Cancelled.into()),
			Err(error) => tracing::debug!(%error, "in-process Git read fell back to system Git"),
		}
		let count = format!("-{count}");
		parse_lines(
			self
				.bytes(cwd, &["log", count.as_str(), "--oneline", "--no-decorate"], cancel)
				.await?,
		)
	}

	/// Lists commits in `base..head`, oldest first, in-process via gitoxide,
	/// falling back to `git rev-list --reverse base..head`.
	pub async fn rev_list_range(
		&self,
		cwd: &Path,
		base: &str,
		head: &str,
		cancel: &CancellationToken,
	) -> Result<Vec<Str>, CommandError> {
		let base_owned = base.to_owned();
		let head_owned = head.to_owned();
		match native::with_repository(cwd, cancel, move |repository, stop| {
			native_rev_list_range(repository, stop, &base_owned, &head_owned)
		})
		.await
		{
			Ok(value) => return Ok(value),
			Err(error) if error.is_cancelled() => return Err(GitRunError::Cancelled.into()),
			Err(error) => tracing::debug!(%error, "in-process Git read fell back to system Git"),
		}
		let range = format!("{base}..{head}");
		parse_lines(
			self
				.bytes(cwd, &["rev-list", "--reverse", range.as_str()], cancel)
				.await?,
		)
	}

	/// Lists commits touching one literal path in-process via gitoxide, falling
	/// back to `git rev-list --max-count=N ref -- path`.
	pub async fn rev_list_touching(
		&self,
		cwd: &Path,
		reference: &str,
		path: &str,
		limit: usize,
		cancel: &CancellationToken,
	) -> Result<Vec<Str>, CommandError> {
		let reference_owned = reference.to_owned();
		let path_owned = path.to_owned();
		match native::with_repository(cwd, cancel, move |repository, stop| {
			native_rev_list_touching(repository, stop, &reference_owned, &path_owned, limit)
		})
		.await
		{
			Ok(value) => return Ok(value),
			Err(error) if error.is_cancelled() => return Err(GitRunError::Cancelled.into()),
			Err(error) => tracing::debug!(%error, "in-process Git read fell back to system Git"),
		}
		let limit = format!("--max-count={limit}");
		parse_lines(
			self
				.bytes(cwd, &["rev-list", limit.as_str(), reference, "--", path], cancel)
				.await?,
		)
	}

	/// Reads identity, parents, author, date, and complete message body metadata
	/// in-process via gitoxide, falling back to `git show -s
	/// --format=%H%x00%P%x00%an%x00%ae%x00%aI%x00%B rev`.
	pub async fn commit_metadata(
		&self,
		cwd: &Path,
		revision: &str,
		cancel: &CancellationToken,
	) -> Result<CommitMetadata, CommandError> {
		let revision_owned = revision.to_owned();
		match native::with_repository(cwd, cancel, move |repository, stop| {
			native_commit_metadata(repository, stop, &revision_owned)
		})
		.await
		{
			Ok(value) => return Ok(value),
			Err(error) if error.is_cancelled() => return Err(GitRunError::Cancelled.into()),
			Err(error) => tracing::debug!(%error, "in-process Git read fell back to system Git"),
		}
		let bytes = self
			.bytes(
				cwd,
				&["show", "-s", "--format=%H%x00%P%x00%an%x00%ae%x00%aI%x00%B", revision],
				cancel,
			)
			.await?;
		let mut fields = bytes.splitn(6, |byte| *byte == 0);
		let mut next = || -> Result<Str, CommandError> {
			let field = fields.next().ok_or(CommandError::NonUtf8)?;
			str::from_utf8(field)
				.map(|value| value.to_str())
				.map_err(|_| CommandError::NonUtf8)
		};
		let metadata = CommitMetadata {
			hash:         next()?,
			parents:      next()?
				.split_ascii_whitespace()
				.map(|parent| parent.to_str())
				.collect(),
			author_name:  next()?,
			author_email: next()?,
			author_date:  next()?,
			body:         next()?,
		};
		Ok(metadata)
	}
}

fn native_tracked(
	repository: &mut gix::Repository,
	stop: &AtomicBool,
) -> Result<Vec<GitPath>, NativeError> {
	let index = repository.index_or_empty().map_err(native::op_error)?;
	let mut paths = Vec::with_capacity(index.entries().len());
	for entry in index.entries() {
		check_cancelled(stop)?;
		paths.push(GitPath::from_bytes(entry.path(&index)));
	}
	Ok(paths)
}

fn native_untracked(
	repository: &mut gix::Repository,
	stop: &AtomicBool,
) -> Result<Vec<GitPath>, NativeError> {
	let mut paths = Vec::new();
	let status = repository
		.status(gix::progress::Discard)
		.map_err(native::op_error)?
		.untracked_files(gix::status::UntrackedFiles::Files);
	for item in status
		.into_index_worktree_iter(Vec::new())
		.map_err(native::op_error)?
	{
		check_cancelled(stop)?;
		let item = item.map_err(native::op_error)?;
		if let gix::status::index_worktree::Item::DirectoryContents { entry, .. } = item
			&& matches!(entry.status, gix::dir::entry::Status::Untracked)
		{
			paths.push(GitPath::from_bytes(entry.rela_path.as_ref()));
		}
	}
	paths.sort_unstable();
	Ok(paths)
}

fn native_tree(
	repository: &mut gix::Repository,
	stop: &AtomicBool,
	tree_spec: &str,
	pathspecs: &[String],
) -> Result<Vec<GitPath>, NativeError> {
	let tree = repository
		.rev_parse_single(tree_spec)
		.map_err(native::op_error)?
		.object()
		.map_err(native::op_error)?
		.peel_to_tree()
		.map_err(native::op_error)?;
	let mut recorder = gix::traverse::tree::Recorder::default();
	tree
		.traverse()
		.depthfirst(&mut recorder)
		.map_err(native::op_error)?;
	let pathspecs = pathspecs.iter().map(String::as_bytes).collect::<Vec<_>>();
	let mut paths = Vec::with_capacity(recorder.records.len());
	for entry in recorder.records {
		check_cancelled(stop)?;
		if !entry.mode.is_tree()
			&& (pathspecs.is_empty()
				|| pathspecs
					.iter()
					.any(|pathspec| literal_pathspec_matches(entry.filepath.as_ref(), pathspec)))
		{
			paths.push(GitPath::from_bytes(entry.filepath.as_ref()));
		}
	}
	Ok(paths)
}

fn native_submodules(
	repository: &mut gix::Repository,
	stop: &AtomicBool,
) -> Result<Vec<GitPath>, NativeError> {
	let mut paths = Vec::new();
	let mut seen = HashSet::new();
	native_submodules_in(repository, stop, &[], &mut paths, &mut seen)?;
	Ok(paths)
}

fn native_submodules_in(
	repository: &mut gix::Repository,
	stop: &AtomicBool,
	prefix: &[u8],
	paths: &mut Vec<GitPath>,
	seen: &mut HashSet<GitPath>,
) -> Result<(), NativeError> {
	let index = repository.index_or_empty().map_err(native::op_error)?;
	let workdir = repository.workdir().map(Path::to_path_buf);
	let mut nested = Vec::new();
	for entry in index.entries() {
		check_cancelled(stop)?;
		if entry.mode != gix::index::entry::Mode::COMMIT {
			continue;
		}
		let local = entry.path(&index);
		let mut path =
			Vec::with_capacity(prefix.len() + local.len() + usize::from(!prefix.is_empty()));
		if !prefix.is_empty() {
			path.extend_from_slice(prefix);
			path.push(b'/');
		}
		path.extend_from_slice(local);
		let git_path = GitPath::from_bytes(&path);
		if seen.insert(git_path.clone()) {
			paths.push(git_path);
			if workdir.is_some() {
				let local = str::from_utf8(local).map_err(native::op_error)?;
				nested.push((path, local.to_owned()));
			}
		}
	}
	let Some(workdir) = workdir else {
		return Ok(());
	};
	for (path, local) in nested {
		check_cancelled(stop)?;
		let submodule = workdir.join(local);
		if !submodule.join(".git").exists() {
			continue;
		}
		if let Ok(mut nested) = gix::open(&submodule) {
			native_submodules_in(&mut nested, stop, &path, paths, seen)?;
		}
	}
	Ok(())
}

fn native_log_subjects(
	repository: &mut gix::Repository,
	stop: &AtomicBool,
	count: usize,
) -> Result<Vec<Str>, NativeError> {
	if count == 0 {
		return Ok(Vec::new());
	}
	let head = repository.head_id().map_err(native::op_error)?;
	let walk = repository
		.rev_walk([head.detach()])
		.sorting(gix::revision::walk::Sorting::ByCommitTime(Default::default()))
		.all()
		.map_err(native::op_error)?;
	let mut subjects = Vec::with_capacity(count);
	for info in walk {
		check_cancelled(stop)?;
		let commit = info
			.map_err(native::op_error)?
			.object()
			.map_err(native::op_error)?;
		let decoded = commit.decode().map_err(native::op_error)?;
		let summary = decoded.message().summary();
		let summary = str::from_utf8(summary.as_ref()).map_err(native::op_error)?;
		if !summary.is_empty() {
			subjects.push(summary.to_str());
			if subjects.len() == count {
				break;
			}
		}
	}
	Ok(subjects)
}

fn native_log_onelines(
	repository: &mut gix::Repository,
	stop: &AtomicBool,
	count: usize,
) -> Result<Vec<Str>, NativeError> {
	if count == 0 {
		return Ok(Vec::new());
	}
	let head = repository.head_id().map_err(native::op_error)?;
	let walk = repository
		.rev_walk([head.detach()])
		.sorting(gix::revision::walk::Sorting::ByCommitTime(Default::default()))
		.all()
		.map_err(native::op_error)?;
	let mut lines = Vec::with_capacity(count);
	for info in walk {
		check_cancelled(stop)?;
		let info = info.map_err(native::op_error)?;
		let commit = info.object().map_err(native::op_error)?;
		let decoded = commit.decode().map_err(native::op_error)?;
		let summary = decoded.message().summary();
		let summary = str::from_utf8(summary.as_ref()).map_err(native::op_error)?;
		if !summary.is_empty() {
			lines.push(format!("{} {summary}", info.id().shorten_or_id()).to_str());
			if lines.len() == count {
				break;
			}
		}
	}
	Ok(lines)
}

fn native_rev_list_range(
	repository: &mut gix::Repository,
	stop: &AtomicBool,
	base: &str,
	head: &str,
) -> Result<Vec<Str>, NativeError> {
	let base = repository
		.rev_parse_single(base)
		.map_err(native::op_error)?;
	let head = repository
		.rev_parse_single(head)
		.map_err(native::op_error)?;
	let walk = repository
		.rev_walk([head.detach()])
		.with_hidden([base.detach()])
		.sorting(gix::revision::walk::Sorting::ByCommitTime(Default::default()))
		.all()
		.map_err(native::op_error)?;
	let mut commits = Vec::new();
	for info in walk {
		check_cancelled(stop)?;
		commits.push(format!("{}", info.map_err(native::op_error)?.id).to_str());
	}
	commits.reverse();
	Ok(commits)
}

fn native_rev_list_touching(
	repository: &mut gix::Repository,
	stop: &AtomicBool,
	reference: &str,
	path: &str,
	limit: usize,
) -> Result<Vec<Str>, NativeError> {
	if limit == 0 {
		return Ok(Vec::new());
	}
	let head = repository
		.rev_parse_single(reference)
		.map_err(native::op_error)?
		.detach();
	let path = Path::new(path);
	let mut frontier = BinaryHeap::new();
	frontier.push((commit_seconds(repository, head)?, head));
	let mut seen = HashSet::new();
	let mut commits = Vec::with_capacity(limit);
	while let Some((_, id)) = frontier.pop() {
		check_cancelled(stop)?;
		if !seen.insert(id) {
			continue;
		}
		let commit = repository.find_commit(id).map_err(native::op_error)?;
		let parents = commit
			.parent_ids()
			.map(|parent| parent.detach())
			.collect::<Vec<_>>();
		let current = tree_entry_id(&commit, path)?;
		if parents.is_empty() {
			if current.is_some() {
				commits.push(format!("{id}").to_str());
			}
		} else {
			let mut treesame = None;
			for parent in &parents {
				let parent_commit = repository.find_commit(*parent).map_err(native::op_error)?;
				if tree_entry_id(&parent_commit, path)? == current {
					treesame = Some(*parent);
					break;
				}
			}
			if let Some(parent) = treesame {
				frontier.push((commit_seconds(repository, parent)?, parent));
			} else {
				commits.push(format!("{id}").to_str());
				if commits.len() == limit {
					break;
				}
				for parent in parents {
					frontier.push((commit_seconds(repository, parent)?, parent));
				}
			}
		}
	}
	Ok(commits)
}

fn native_commit_metadata(
	repository: &mut gix::Repository,
	stop: &AtomicBool,
	revision: &str,
) -> Result<CommitMetadata, NativeError> {
	check_cancelled(stop)?;
	let commit = repository
		.rev_parse_single(revision)
		.map_err(native::op_error)?
		.object()
		.map_err(native::op_error)?
		.peel_to_commit()
		.map_err(native::op_error)?;
	let decoded = commit.decode().map_err(native::op_error)?;
	let author = decoded.author().map_err(native::op_error)?;
	let author_name = str::from_utf8(author.name)
		.map_err(native::op_error)?
		.to_str();
	let author_email = str::from_utf8(author.email)
		.map_err(native::op_error)?
		.to_str();
	let author_date = author
		.time()
		.map_err(native::op_error)?
		.format(gix::date::time::format::ISO8601_STRICT)
		.map_err(native::op_error)?
		.to_str();
	let body = str::from_utf8(commit.message_raw().map_err(native::op_error)?.as_ref())
		.map_err(native::op_error)?
		.to_str();
	Ok(CommitMetadata {
		hash: format!("{}", commit.id()).to_str(),
		parents: commit
			.parent_ids()
			.map(|parent| format!("{}", parent.detach()).to_str())
			.collect(),
		author_name,
		author_email,
		author_date,
		body,
	})
}

fn tree_entry_id(
	commit: &gix::Commit<'_>,
	path: &Path,
) -> Result<Option<gix::hash::ObjectId>, NativeError> {
	commit
		.tree()
		.map_err(native::op_error)?
		.lookup_entry_by_path(path)
		.map_err(native::op_error)
		.map(|entry| entry.map(|entry| entry.object_id()))
}

fn commit_seconds(
	repository: &gix::Repository,
	id: gix::hash::ObjectId,
) -> Result<i64, NativeError> {
	repository
		.find_commit(id)
		.map_err(native::op_error)?
		.time()
		.map(|time| time.seconds)
		.map_err(native::op_error)
}

fn literal_pathspec_matches(path: &[u8], pathspec: &[u8]) -> bool {
	path == pathspec
		|| path
			.strip_prefix(pathspec)
			.is_some_and(|remainder| remainder.starts_with(b"/"))
}

fn check_cancelled(stop: &AtomicBool) -> Result<(), NativeError> {
	if stop.load(Ordering::Relaxed) {
		Err(NativeError::Cancelled)
	} else {
		Ok(())
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
	let text = str::from_utf8(&bytes).map_err(|_| CommandError::NonUtf8)?;
	Ok(text
		.lines()
		.filter(|line| !line.is_empty())
		.map(|line| line.to_str())
		.collect())
}

#[cfg(test)]
mod tests {
	use std::{fs, path::Path, process::Command};

	use super::*;

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

	fn fixture() -> tempfile::TempDir {
		let root = tempfile::tempdir().expect("temporary repository root");
		fixture_git(root.path(), &["init", "-b", "main"]);
		fixture_git(root.path(), &["config", "user.name", "OMP Test"]);
		fixture_git(root.path(), &["config", "user.email", "omp@example.invalid"]);
		root
	}

	fn commit(root: &Path, message: &str) {
		fixture_git(root, &["add", "."]);
		fixture_git(root, &["commit", "-m", message]);
	}

	fn repository(root: &Path) -> gix::Repository {
		gix::discover(root).expect("open fixture repository")
	}

	#[test]
	fn native_paths_follow_index_status_and_tree_semantics() {
		let fixture = fixture();
		fs::create_dir_all(fixture.path().join("dir")).expect("create directory");
		fs::write(fixture.path().join("dir/tracked"), "tracked\n").expect("write tracked file");
		fs::write(fixture.path().join(".gitignore"), "ignored\n").expect("write ignore");
		commit(fixture.path(), "initial");
		fs::write(fixture.path().join("added"), "added\n").expect("write added file");
		fixture_git(fixture.path(), &["add", "added"]);
		fs::write(fixture.path().join("untracked-z"), "z\n").expect("write untracked file");
		fs::write(fixture.path().join("untracked-a"), "a\n").expect("write untracked file");
		fs::write(fixture.path().join("ignored"), "ignored\n").expect("write ignored file");
		let mut repository = repository(fixture.path());
		let stop = AtomicBool::new(false);
		assert_eq!(
			native_tracked(&mut repository, &stop)
				.expect("tracked")
				.iter()
				.map(GitPath::as_bytes)
				.collect::<Vec<_>>(),
			vec![b".gitignore".as_slice(), b"added", b"dir/tracked"]
		);
		assert_eq!(
			native_untracked(&mut repository, &stop)
				.expect("untracked")
				.iter()
				.map(GitPath::as_bytes)
				.collect::<Vec<_>>(),
			vec![b"untracked-a".as_slice(), b"untracked-z"]
		);
		let head = repository.head_id().expect("head");
		fixture_git(fixture.path(), &[
			"update-index",
			"--add",
			"--cacheinfo",
			&format!("160000,{},submodule", head),
		]);
		assert_eq!(
			native_submodules(&mut repository, &stop)
				.expect("submodules")
				.iter()
				.map(GitPath::as_bytes)
				.collect::<Vec<_>>(),
			vec![b"submodule".as_slice()]
		);
		let tree_id = String::from_utf8(
			Command::new("git")
				.current_dir(fixture.path())
				.args(["write-tree"])
				.output()
				.expect("write tree")
				.stdout,
		)
		.expect("UTF-8 tree id")
		.trim()
		.to_owned();
		let tree = native_tree(&mut repository, &stop, &tree_id, &[]).expect("tree");
		assert!(tree.iter().any(|path| path.as_bytes() == b"submodule"));
		assert_eq!(
			native_tree(&mut repository, &stop, "HEAD", &["dir".to_owned()])
				.expect("filtered tree")
				.iter()
				.map(GitPath::as_bytes)
				.collect::<Vec<_>>(),
			vec![b"dir/tracked".as_slice()]
		);
	}

	#[test]
	fn native_history_and_metadata_match_git_shapes() {
		let fixture = fixture();
		fs::write(fixture.path().join("watched"), "one\n").expect("write watched file");
		fs::write(fixture.path().join("other"), "one\n").expect("write other file");
		commit(fixture.path(), "first subject");
		let first = String::from_utf8(
			Command::new("git")
				.current_dir(fixture.path())
				.args(["rev-parse", "HEAD"])
				.output()
				.expect("read first id")
				.stdout,
		)
		.expect("UTF-8 id")
		.trim()
		.to_owned();
		fs::write(fixture.path().join("other"), "two\n").expect("update other file");
		commit(fixture.path(), "second subject");
		fs::write(fixture.path().join("watched"), "two\n").expect("update watched file");
		fixture_git(fixture.path(), &["add", "watched"]);
		fixture_git(fixture.path(), &["commit", "-m", "third subject", "-m", "body line"]);
		let mut repository = repository(fixture.path());
		let stop = AtomicBool::new(false);
		assert_eq!(native_log_subjects(&mut repository, &stop, 2).expect("subjects"), vec![
			"third subject".to_str(),
			"second subject".to_str()
		]);
		let onelines = native_log_onelines(&mut repository, &stop, 1).expect("oneline");
		let (short, subject) = onelines[0].split_once(' ').expect("short sha and subject");
		assert!(short.len() >= 7);
		assert_eq!(subject, "third subject");
		assert_eq!(
			native_rev_list_range(&mut repository, &stop, &first, "HEAD")
				.expect("range")
				.len(),
			2
		);
		assert!(
			native_rev_list_range(&mut repository, &stop, "HEAD", "HEAD")
				.expect("empty range")
				.is_empty()
		);
		assert_eq!(
			native_rev_list_touching(&mut repository, &stop, "HEAD", "watched", 10)
				.expect("touching")
				.len(),
			2
		);
		assert_eq!(
			native_rev_list_touching(&mut repository, &stop, "HEAD", "watched", 1)
				.expect("limited touching")
				.len(),
			1
		);
		let metadata = native_commit_metadata(&mut repository, &stop, "HEAD").expect("metadata");
		let date = metadata.author_date.as_str().as_bytes();
		assert_eq!(date.len(), 25);
		assert_eq!(
			(date[4], date[7], date[10], date[13], date[16], date[19], date[22]),
			(b'-', b'-', b'T', b':', b':', b'+', b':')
		);
		assert_eq!(metadata.body, "third subject\n\nbody line\n".to_str());
	}
}
