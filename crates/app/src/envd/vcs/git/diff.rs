//! Complete bounded Git status, numstat, and unified-diff read models.

use std::path::Path;

use bytes::Bytes;
use tokio_util::sync::CancellationToken;

use super::{
	commands::CommandError,
	query::GitPath,
	runner::{GitRunOptions, GitRunner},
};

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
		let bytes = self
			.invoke(cwd, &["status", "--porcelain=v2", "-z", "--untracked-files=all"], false, cancel)
			.await?;
		Ok(parse_status(&bytes))
	}
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
	let text = std::str::from_utf8(line).ok()?;
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
	let text = std::str::from_utf8(bytes).map_err(|_| CommandError::NonUtf8)?;
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
