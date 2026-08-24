//! Complete bounded Git status, numstat, and unified-diff read models.

use std::{
	path::Path,
	str,
	str::FromStr as _,
	sync::atomic::{AtomicBool, Ordering},
};

use bytes::Bytes;
use strum::{EnumString, IntoStaticStr};
use tokio_util::sync::CancellationToken;

use super::{
	commands::CommandError,
	native,
	query::GitPath,
	runner::{GitRunError, GitRunOptions, GitRunner},
};

/// Kind of one index or worktree change reported by Git porcelain.
#[derive(Clone, Copy, Debug, EnumString, Eq, IntoStaticStr, PartialEq)]
pub enum ChangeKind {
	/// File contents or metadata changed.
	#[strum(serialize = "M")]
	Modified,
	/// Path was added.
	#[strum(serialize = "A")]
	Added,
	/// Path was deleted.
	#[strum(serialize = "D")]
	Deleted,
	/// Path was renamed.
	#[strum(serialize = "R")]
	Renamed,
	/// Path was copied.
	#[strum(serialize = "C")]
	Copied,
	/// File type changed.
	#[strum(serialize = "T")]
	TypeChanged,
	/// Index stages disagree because a merge is unresolved.
	#[strum(serialize = "U")]
	Unmerged,
}

/// One NUL-safe porcelain-v1 status record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusEntry {
	/// Change between `HEAD` and the index.
	pub staged:     Option<ChangeKind>,
	/// Change between the index and worktree.
	pub worktree:   Option<ChangeKind>,
	/// Whether the XY pair is one of Git's unresolved merge states.
	pub conflicted: bool,
	/// Whether Git reported the path as untracked.
	pub untracked:  bool,
	/// Current repository-relative path.
	pub path:       GitPath,
	/// Original path for a rename or copy.
	pub orig_path:  Option<GitPath>,
}

/// Porcelain status counts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StatusCounts {
	/// Paths changed in the index.
	pub staged:    u32,
	/// Paths changed in the worktree.
	pub unstaged:  u32,
	/// Untracked paths.
	pub untracked: u32,
}

/// A parsed numstat count; binary files have no line count.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LineCount {
	/// Text line count.
	Lines(u64),
	/// Binary content (`-` in Git numstat).
	Binary,
}

/// One NUL-safe numstat entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NumstatEntry {
	/// Added lines or binary marker.
	pub added:    LineCount,
	/// Removed lines or binary marker.
	pub removed:  LineCount,
	/// Original path for a rename or copy.
	pub old_path: Option<GitPath>,
	/// Current path.
	pub path:     GitPath,
}

/// One unified hunk with exact raw bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffHunk {
	/// Old-file starting line.
	pub old_start: u64,
	/// Old-file line count.
	pub old_count: u64,
	/// New-file starting line.
	pub new_start: u64,
	/// New-file line count.
	pub new_count: u64,
	/// Exact hunk bytes, including terminal newline when present.
	pub raw:       Bytes,
}

/// One parsed file patch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileDiff {
	/// Original path for a rename when declared by Git.
	pub old_path:             Option<Bytes>,
	/// Current path when available.
	pub path:                 Option<Bytes>,
	/// Whether Git declared binary patch content.
	pub binary:               bool,
	/// Whether an old-side line lacked its terminal newline.
	pub old_no_final_newline: bool,
	/// Whether a new-side line lacked its terminal newline.
	pub new_no_final_newline: bool,
	/// Parsed unified hunks.
	pub hunks:                Vec<DiffHunk>,
	/// Exact complete file-patch bytes.
	pub raw:                  Bytes,
}

/// Options shared by worktree and cached diffs.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DiffOptions {
	/// Compare the index to HEAD.
	pub cached:  bool,
	/// Include binary patch bodies.
	pub binary:  bool,
	/// Emit summary statistics.
	pub stat:    bool,
	/// Emit numstat records.
	pub numstat: bool,
}

/// Typed bounded Git diff facade.
#[derive(Clone)]
pub struct GitDiff {
	runner: GitRunner,
}

impl GitDiff {
	/// Creates a diff facade over the hardened runner.
	pub const fn new(runner: GitRunner) -> Self {
		Self { runner }
	}

	async fn invoke(
		&self,
		cwd: &Path,
		argv: &[&str],
		allow_difference: bool,
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
		if output.exit_code == 0 || allow_difference && output.exit_code == 1 {
			Ok(output.stdout)
		} else {
			Err(CommandError::Exit {
				code:   output.exit_code,
				stdout: output.stdout,
				stderr: output.stderr,
			})
		}
	}

	/// Captures a complete raw worktree or cached diff.
	///
	/// Stays on system Git because consumers (`git apply`, external parsers)
	/// require Git's byte-exact patch framing including binary literals.
	pub async fn raw(
		&self,
		cwd: &Path,
		options: DiffOptions,
		paths: &[&str],
		cancel: &CancellationToken,
	) -> Result<Bytes, CommandError> {
		let mut argv = Vec::with_capacity(6 + paths.len());
		argv.push("diff");
		if options.binary {
			argv.push("--binary");
		}
		if options.cached {
			argv.push("--cached");
		}
		if options.stat {
			argv.push("--stat");
		}
		if options.numstat {
			argv.extend(["--numstat", "-z"]);
		}
		if !paths.is_empty() {
			argv.push("--");
			argv.extend_from_slice(paths);
		}
		self.invoke(cwd, &argv, false, cancel).await
	}

	/// Lists changed paths with NUL framing.
	pub async fn names(
		&self,
		cwd: &Path,
		cached: bool,
		cancel: &CancellationToken,
	) -> Result<Vec<GitPath>, CommandError> {
		match native::with_repository(cwd, cancel, move |repository, stop| {
			native_names(repository, stop, cached)
		})
		.await
		{
			Ok(value) => return Ok(value),
			Err(error) if error.is_cancelled() => return Err(GitRunError::Cancelled.into()),
			Err(error) => tracing::debug!(%error, "in-process Git read fell back to system Git"),
		}
		let argv = if cached {
			["diff", "--cached", "--name-only", "-z"]
		} else {
			["diff", "--name-only", "-z", "--"]
		};
		let bytes = self.invoke(cwd, &argv, false, cancel).await?;
		Ok(parse_paths(bytes))
	}

	/// Returns whether a worktree or cached diff exists.
	pub async fn has(
		&self,
		cwd: &Path,
		cached: bool,
		cancel: &CancellationToken,
	) -> Result<bool, CommandError> {
		match native::with_repository(cwd, cancel, move |repository, stop| {
			native_has(repository, stop, cached)
		})
		.await
		{
			Ok(value) => return Ok(value),
			Err(error) if error.is_cancelled() => return Err(GitRunError::Cancelled.into()),
			Err(error) => tracing::debug!(%error, "in-process Git read fell back to system Git"),
		}
		let argv = if cached {
			["diff", "--cached", "--quiet"]
		} else {
			["diff", "--quiet", "--"]
		};
		let output = self
			.runner
			.run(
				cwd,
				&argv,
				GitRunOptions { read_only: true, parse_sensitive: true, ..Default::default() },
				cancel,
			)
			.await?;
		match output.exit_code {
			0 => Ok(false),
			1 => Ok(true),
			code => Err(CommandError::Exit { code, stdout: output.stdout, stderr: output.stderr }),
		}
	}

	/// Captures a complete tree-to-tree patch.
	///
	/// Stays on system Git because consumers (`git apply`, external parsers)
	/// require Git's byte-exact patch framing including binary literals.
	pub async fn tree(
		&self,
		cwd: &Path,
		base: &str,
		head: &str,
		binary: bool,
		cancel: &CancellationToken,
	) -> Result<Bytes, CommandError> {
		let mut argv = vec!["diff-tree", "-r", "-p", "--no-commit-id"];
		if binary {
			argv.push("--binary");
		}
		argv.extend([base, head]);
		self.invoke(cwd, &argv, false, cancel).await
	}

	/// Runs Git's no-index diff. Exit status one means a valid difference.
	///
	/// Stays on system Git because consumers (`git apply`, external parsers)
	/// require Git's byte-exact patch framing including binary literals.
	pub async fn no_index(
		&self,
		cwd: &Path,
		left: &str,
		right: &str,
		binary: bool,
		cancel: &CancellationToken,
	) -> Result<Bytes, CommandError> {
		let argv = if binary {
			["diff", "--no-index", "--binary", left, right]
		} else {
			["diff", "--no-index", "--no-ext-diff", left, right]
		};
		self.invoke(cwd, &argv, true, cancel).await
	}

	/// Reads and parses porcelain-v2 status with NUL-delimited path records.
	pub async fn status_counts(
		&self,
		cwd: &Path,
		cancel: &CancellationToken,
	) -> Result<StatusCounts, CommandError> {
		match native::with_repository(cwd, cancel, native_status_counts).await {
			Ok(value) => return Ok(value),
			Err(error) if error.is_cancelled() => return Err(GitRunError::Cancelled.into()),
			Err(error) => tracing::debug!(%error, "in-process Git read fell back to system Git"),
		}
		let bytes = self
			.invoke(cwd, &["status", "--porcelain=v2", "-z", "--untracked-files=all"], false, cancel)
			.await?;
		Ok(parse_status(&bytes))
	}

	/// Reads rich porcelain-v1 status entries with byte-exact NUL-framed paths.
	pub async fn status_entries(
		&self,
		cwd: &Path,
		cancel: &CancellationToken,
	) -> Result<Vec<StatusEntry>, CommandError> {
		let bytes = self
			.invoke(cwd, &["status", "--porcelain=v1", "-z", "--untracked-files=all"], false, cancel)
			.await?;
		Ok(parse_status_entries(&bytes))
	}
}

fn native_status_counts(
	repository: &mut gix::Repository,
	stop: &AtomicBool,
) -> Result<StatusCounts, native::NativeError> {
	let ignored_staged = index_paths_with_flags(
		repository,
		gix::index::entry::Flags::INTENT_TO_ADD | gix::index::entry::Flags::CONFLICTED,
	)?;
	let mut counts = StatusCounts::default();
	let items = status_items(repository, gix::status::UntrackedFiles::Files)?;
	for item in items {
		if stop.load(Ordering::Relaxed) {
			return Err(native::NativeError::Cancelled);
		}
		match item.map_err(native::op_error)? {
			gix::status::Item::TreeIndex(change) => {
				if !path_in(&ignored_staged, change.location().as_ref()) {
					counts.staged += 1;
				}
			},
			gix::status::Item::IndexWorktree(item) => match item {
				entry @ gix::status::index_worktree::Item::DirectoryContents { .. } => {
					if entry.summary().is_some() {
						counts.untracked += 1;
					}
				},
				_ => match item.summary() {
					Some(gix::status::index_worktree::iter::Summary::Conflict) => {
						counts.staged += 1;
						counts.unstaged += 1;
					},
					Some(_) => counts.unstaged += 1,
					None => {},
				},
			},
		}
	}
	Ok(counts)
}

fn native_names(
	repository: &mut gix::Repository,
	stop: &AtomicBool,
	cached: bool,
) -> Result<Vec<GitPath>, native::NativeError> {
	let ignored_staged = cached
		.then(|| index_paths_with_flags(repository, gix::index::entry::Flags::INTENT_TO_ADD))
		.transpose()?
		.unwrap_or_default();
	let mut paths = Vec::new();
	let items = status_items(repository, gix::status::UntrackedFiles::None)?;
	for item in items {
		if stop.load(Ordering::Relaxed) {
			return Err(native::NativeError::Cancelled);
		}
		match item.map_err(native::op_error)? {
			gix::status::Item::TreeIndex(change)
				if cached && !path_in(&ignored_staged, change.location().as_ref()) =>
			{
				paths.push(GitPath::from_bytes(change.location().as_ref()));
			},
			gix::status::Item::IndexWorktree(item)
				if !cached
					&& !matches!(&item, gix::status::index_worktree::Item::DirectoryContents { .. })
					&& item.summary().is_some() =>
			{
				paths.push(GitPath::from_bytes(item.rela_path().as_ref()));
			},
			_ => {},
		}
	}
	paths.sort_unstable_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
	Ok(paths)
}

fn native_has(
	repository: &mut gix::Repository,
	stop: &AtomicBool,
	cached: bool,
) -> Result<bool, native::NativeError> {
	let ignored_staged = cached
		.then(|| index_paths_with_flags(repository, gix::index::entry::Flags::INTENT_TO_ADD))
		.transpose()?
		.unwrap_or_default();
	let items = status_items(repository, gix::status::UntrackedFiles::None)?;
	for item in items {
		if stop.load(Ordering::Relaxed) {
			return Err(native::NativeError::Cancelled);
		}
		match item.map_err(native::op_error)? {
			gix::status::Item::TreeIndex(change)
				if cached && !path_in(&ignored_staged, change.location().as_ref()) =>
			{
				return Ok(true);
			},
			gix::status::Item::IndexWorktree(item)
				if !cached
					&& !matches!(&item, gix::status::index_worktree::Item::DirectoryContents { .. })
					&& item.summary().is_some() =>
			{
				return Ok(true);
			},
			_ => {},
		}
	}
	Ok(false)
}

fn status_items(
	repository: &gix::Repository,
	untracked: gix::status::UntrackedFiles,
) -> Result<gix::status::Iter, native::NativeError> {
	repository
		.status(gix::progress::Discard)
		.map_err(native::op_error)?
		.untracked_files(untracked)
		.tree_index_track_renames(gix::status::tree_index::TrackRenames::AsConfigured)
		.index_worktree_rewrites(gix::diff::Rewrites::default())
		.into_iter(Vec::new())
		.map_err(native::op_error)
}

fn index_paths_with_flags(
	repository: &gix::Repository,
	flags: gix::index::entry::Flags,
) -> Result<Vec<Vec<u8>>, native::NativeError> {
	let index = repository.index_or_empty().map_err(native::op_error)?;
	Ok(index
		.entries()
		.iter()
		.filter(|entry| entry.flags.intersects(flags))
		.map(|entry| entry.path(&index).to_vec())
		.collect())
}

fn path_in(paths: &[Vec<u8>], location: &[u8]) -> bool {
	paths.iter().any(|path| path == location)
}

/// Parses porcelain v1 (line or NUL framed) and v2 records into counts.
/// Parses NUL-framed porcelain-v1 records, including rename origins and
/// unresolved merge states.
pub fn parse_status_entries(bytes: &[u8]) -> Vec<StatusEntry> {
	const CONFLICTS: [[u8; 2]; 7] = [*b"DD", *b"AU", *b"UD", *b"UA", *b"DU", *b"AA", *b"UU"];

	let records: Vec<_> = bytes.split(|byte| *byte == 0).collect();
	let mut entries = Vec::new();
	let mut index = 0;
	while let Some(record) = records.get(index).copied() {
		index += 1;
		if record.len() < 3 || record[2] != b' ' {
			continue;
		}
		let xy = [record[0], record[1]];
		if xy == *b"!!" {
			continue;
		}
		let untracked = xy == *b"??";
		let conflicted = CONFLICTS.contains(&xy);
		let renamed_or_copied = xy.iter().any(|kind| matches!(kind, b'R' | b'C'));
		let orig_path = if renamed_or_copied {
			let origin = records.get(index).copied().filter(|path| !path.is_empty());
			index += usize::from(index < records.len());
			origin.map(GitPath::from_bytes)
		} else {
			None
		};
		let kind = |value: &[u8]| {
			str::from_utf8(value)
				.ok()
				.and_then(|value| ChangeKind::from_str(value).ok())
		};
		entries.push(StatusEntry {
			staged: if untracked { None } else { kind(&record[..1]) },
			worktree: if untracked { None } else { kind(&record[1..2]) },
			conflicted,
			untracked,
			path: GitPath::from_bytes(&record[3..]),
			orig_path,
		});
	}
	entries
}

/// Parses porcelain v1 (line or NUL framed) and v2 records into counts.
pub fn parse_status(bytes: &[u8]) -> StatusCounts {
	let nul_framed = bytes.contains(&0);
	let records: Vec<_> = bytes
		.split(|byte| *byte == 0 || !nul_framed && *byte == b'\n')
		.filter(|record| !record.is_empty())
		.collect();
	let mut counts = StatusCounts::default();
	let mut index = 0;
	while index < records.len() {
		let record = records[index];
		let mut consumes_origin = false;
		let xy = match record.first().copied() {
			Some(b'?') if record.get(1) == Some(&b'?') || record.get(1) == Some(&b' ') => {
				counts.untracked = counts.untracked.saturating_add(1);
				index += 1;
				continue;
			},
			Some(b'!') | Some(b'#') => {
				index += 1;
				continue;
			},
			Some(b'1' | b'u') => record.split(|byte| *byte == b' ').nth(1),
			Some(b'2') => {
				consumes_origin = nul_framed;
				record.split(|byte| *byte == b' ').nth(1)
			},
			_ => {
				consumes_origin = nul_framed && matches!(record.first(), Some(b'R' | b'C'));
				record.get(..2)
			},
		};
		if let Some(xy) = xy.filter(|xy| xy.len() >= 2) {
			if !matches!(xy[0], b' ' | b'.' | b'?' | b'!') {
				counts.staged = counts.staged.saturating_add(1);
			}
			if !matches!(xy[1], b' ' | b'.' | b'?' | b'!') {
				counts.unstaged = counts.unstaged.saturating_add(1);
			}
		}
		index += if consumes_origin { 2 } else { 1 };
	}
	counts
}

/// Parses `git diff --numstat -z`, including its three-record rename form.
pub fn parse_numstat(bytes: Bytes) -> Result<Vec<NumstatEntry>, CommandError> {
	let fields: Vec<_> = bytes
		.split(|byte| *byte == 0)
		.filter(|field| !field.is_empty())
		.collect();
	let mut result = Vec::new();
	let mut index = 0;
	while index < fields.len() {
		let record = fields[index];
		let first = record
			.iter()
			.position(|byte| *byte == b'\t')
			.ok_or(CommandError::NonUtf8)?;
		let second_rel = record[first + 1..]
			.iter()
			.position(|byte| *byte == b'\t')
			.ok_or(CommandError::NonUtf8)?;
		let second = first + 1 + second_rel;
		let added = parse_count(&record[..first])?;
		let removed = parse_count(&record[first + 1..second])?;
		let inline_path = &record[second + 1..];
		if inline_path.is_empty() {
			let old = fields.get(index + 1).ok_or(CommandError::NonUtf8)?;
			let new = fields.get(index + 2).ok_or(CommandError::NonUtf8)?;
			result.push(NumstatEntry {
				added,
				removed,
				old_path: Some(GitPath::from_bytes(old)),
				path: GitPath::from_bytes(new),
			});
			index += 3;
		} else {
			result.push(NumstatEntry {
				added,
				removed,
				old_path: None,
				path: GitPath::from_bytes(inline_path),
			});
			index += 1;
		}
	}
	Ok(result)
}

/// Parses complete unified diff bytes while retaining every original byte.
pub fn parse_unified(bytes: Bytes) -> Vec<FileDiff> {
	let starts = find_all(&bytes, b"diff --git ");
	let mut files = Vec::with_capacity(starts.len());
	for (position, start) in starts.iter().copied().enumerate() {
		let end = starts.get(position + 1).copied().unwrap_or(bytes.len());
		let raw = bytes.slice(start..end);
		files.push(parse_file(raw));
	}
	files
}

fn parse_file(raw: Bytes) -> FileDiff {
	let mut old_path = None;
	let mut path = None;
	let mut binary = false;
	let mut old_no_final_newline = false;
	let mut new_no_final_newline = false;
	let mut hunks = Vec::new();
	let mut offset = 0;
	let mut hunk_start = None;
	let mut hunk_range = None;
	let mut previous_prefix = None;
	for line in raw.split_inclusive(|byte| *byte == b'\n') {
		let line_without_newline = line.strip_suffix(b"\n").unwrap_or(line);
		if let Some(value) = line_without_newline.strip_prefix(b"rename from ") {
			old_path = Some(Bytes::copy_from_slice(value));
		} else if let Some(value) = line_without_newline.strip_prefix(b"rename to ") {
			path = Some(Bytes::copy_from_slice(value));
		} else if old_path.is_none()
			&& let Some(value) = line_without_newline.strip_prefix(b"--- a/")
		{
			old_path = Some(Bytes::copy_from_slice(value));
		} else if path.is_none()
			&& let Some(value) = line_without_newline.strip_prefix(b"+++ b/")
		{
			path = Some(Bytes::copy_from_slice(value));
		} else if line_without_newline.starts_with(b"Binary files ")
			|| line_without_newline == b"GIT binary patch"
		{
			binary = true;
		} else if line_without_newline.starts_with(b"@@ ") {
			if let (Some(start), Some(range)) = (hunk_start.take(), hunk_range.take()) {
				hunks.push(make_hunk(&raw, start, offset, range));
			}
			hunk_range = parse_hunk_header(line_without_newline);
			hunk_start = Some(offset);
		} else if line_without_newline == b"\\ No newline at end of file" {
			match previous_prefix {
				Some(b'-') => old_no_final_newline = true,
				Some(b'+') => new_no_final_newline = true,
				_ => {},
			}
		}
		if matches!(line_without_newline.first(), Some(b'+' | b'-'))
			&& !line_without_newline.starts_with(b"+++")
			&& !line_without_newline.starts_with(b"---")
		{
			previous_prefix = line_without_newline.first().copied();
		}
		offset += line.len();
	}
	if let (Some(start), Some(range)) = (hunk_start, hunk_range) {
		hunks.push(make_hunk(&raw, start, raw.len(), range));
	}
	FileDiff { old_path, path, binary, old_no_final_newline, new_no_final_newline, hunks, raw }
}

fn make_hunk(raw: &Bytes, start: usize, end: usize, range: (u64, u64, u64, u64)) -> DiffHunk {
	DiffHunk {
		old_start: range.0,
		old_count: range.1,
		new_start: range.2,
		new_count: range.3,
		raw:       raw.slice(start..end),
	}
}

fn parse_hunk_header(line: &[u8]) -> Option<(u64, u64, u64, u64)> {
	let text = str::from_utf8(line).ok()?;
	let mut fields = text.split_whitespace();
	(fields.next()? == "@@").then_some(())?;
	let old = parse_range(fields.next()?.strip_prefix('-')?)?;
	let new = parse_range(fields.next()?.strip_prefix('+')?)?;
	Some((old.0, old.1, new.0, new.1))
}

fn parse_range(value: &str) -> Option<(u64, u64)> {
	match value.split_once(',') {
		Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
		None => Some((value.parse().ok()?, 1)),
	}
}

fn parse_count(bytes: &[u8]) -> Result<LineCount, CommandError> {
	if bytes == b"-" {
		return Ok(LineCount::Binary);
	}
	let text = str::from_utf8(bytes).map_err(|_| CommandError::NonUtf8)?;
	text
		.parse()
		.map(LineCount::Lines)
		.map_err(|_| CommandError::NonUtf8)
}

fn find_all(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
	let mut offsets = Vec::new();
	let mut position = 0;
	while position + needle.len() <= haystack.len() {
		if (position == 0 || haystack[position - 1] == b'\n')
			&& &haystack[position..position + needle.len()] == needle
		{
			offsets.push(position);
			position += needle.len();
		} else {
			position += 1;
		}
	}
	offsets
}

fn parse_paths(bytes: Bytes) -> Vec<GitPath> {
	bytes
		.split(|byte| *byte == 0)
		.filter(|path| !path.is_empty())
		.map(GitPath::from_bytes)
		.collect()
}

#[cfg(test)]
mod tests {
	use std::{fs, path::Path, process::Command, sync::atomic::AtomicBool};

	use bytes::Bytes;

	use super::{native_has, native_names, native_status_counts, parse_paths, parse_status};

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

	fn fixture_output(cwd: &Path, arguments: &[&str]) -> Vec<u8> {
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
		output.stdout
	}

	fn fixture() -> tempfile::TempDir {
		let root = tempfile::tempdir().expect("temporary repository root");
		fixture_git(root.path(), &["init", "-b", "main"]);
		fixture_git(root.path(), &["config", "user.name", "OMP Test"]);
		fixture_git(root.path(), &["config", "user.email", "omp@example.invalid"]);
		fs::write(root.path().join(".gitignore"), "ignored\n").expect("write ignore");
		fs::write(root.path().join("tracked.txt"), "before\n").expect("write tracked");
		fs::write(root.path().join("old.txt"), "rename me\n").expect("write old");
		fixture_git(root.path(), &["add", "."]);
		fixture_git(root.path(), &["commit", "-m", "seed"]);
		fs::write(root.path().join("tracked.txt"), "staged\n").expect("stage tracked");
		fixture_git(root.path(), &["add", "tracked.txt"]);
		fixture_git(root.path(), &["mv", "old.txt", "renamed.txt"]);
		fs::write(root.path().join("tracked.txt"), "staged and unstaged\n").expect("edit tracked");
		fs::write(root.path().join("untracked.txt"), "new\n").expect("write untracked");
		fs::write(root.path().join("ignored"), "ignored\n").expect("write ignored");
		fs::write(root.path().join("intent.txt"), "pending\n").expect("write intent");
		fixture_git(root.path(), &["add", "-N", "intent.txt"]);
		root
	}

	#[test]
	fn native_status_and_diff_names_match_git_including_intent_to_add() {
		let fixture = fixture();
		let mut repository = gix::discover(fixture.path()).expect("discover fixture");
		let stop = AtomicBool::new(false);
		let expected_status = parse_status(&fixture_output(fixture.path(), &[
			"status",
			"--porcelain=v2",
			"-z",
			"--untracked-files=all",
		]));
		assert_eq!(
			native_status_counts(&mut repository, &stop).expect("native status"),
			expected_status
		);
		let expected_cached = parse_paths(Bytes::from(fixture_output(fixture.path(), &[
			"diff",
			"--cached",
			"--name-only",
			"-z",
		])));
		assert!(expected_cached.windows(2).all(|paths| paths[0] <= paths[1]));
		assert_eq!(
			native_names(&mut repository, &stop, true).expect("native cached names"),
			expected_cached
		);
		let expected_worktree = parse_paths(Bytes::from(fixture_output(fixture.path(), &[
			"diff",
			"--name-only",
			"-z",
			"--",
		])));
		assert!(
			expected_worktree
				.windows(2)
				.all(|paths| paths[0] <= paths[1])
		);
		assert_eq!(
			native_names(&mut repository, &stop, false).expect("native worktree names"),
			expected_worktree
		);
		assert!(native_has(&mut repository, &stop, true).expect("native cached has"));
		assert!(native_has(&mut repository, &stop, false).expect("native worktree has"));
	}
}
