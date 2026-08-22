//! Direct Git HEAD and ref resolution with reftable fallback and invalidation
//! polling.

use std::{
	io,
	path::{Path, PathBuf},
	time::{Duration, SystemTime},
};

use omp_core::{IntoStr, Str};
use tokio_util::sync::CancellationToken;

use super::{
	repo::Repository,
	runner::{GitRunError, GitRunOptions, GitRunner},
};

const METADATA_LIMIT: u64 = 8 * 1024 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Fully resolved repository HEAD state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HeadState {
	/// HEAD names a branch that resolves to a commit.
	Branch {
		/// Full symbolic ref name.
		reference: Str,
		/// Local branch name when the ref is under `refs/heads`.
		branch:    Option<Str>,
		/// Commit object ID.
		commit:    Str,
	},
	/// HEAD names a branch which has no commit yet.
	Unborn {
		/// Full symbolic ref name.
		reference: Str,
		/// Local branch name when available.
		branch:    Option<Str>,
	},
	/// HEAD directly contains an object ID.
	Detached {
		/// Detached commit object ID.
		commit: Str,
	},
}

impl HeadState {
	/// Returns the resolved commit, absent for an unborn branch.
	pub fn commit(&self) -> Option<&str> {
		match self {
			Self::Branch { commit, .. } | Self::Detached { commit } => Some(commit.as_str()),
			Self::Unborn { .. } => None,
		}
	}

	/// Returns the local branch name when HEAD names one.
	pub fn branch(&self) -> Option<&str> {
		match self {
			Self::Branch { branch, .. } | Self::Unborn { branch, .. } => branch.as_deref(),
			Self::Detached { .. } => None,
		}
	}
}

/// Direct repository metadata resolution failure.
#[derive(Debug, thiserror::Error)]
pub enum RefError {
	/// Filesystem metadata could not be read.
	#[error("failed to read Git ref metadata at {path:?}")]
	Io {
		/// Metadata path.
		path:   PathBuf,
		/// Underlying filesystem failure.
		#[source]
		source: io::Error,
	},
	/// Ref metadata exceeded its finite bound.
	#[error("Git ref metadata at {path:?} exceeds 8 MiB")]
	TooLarge {
		/// Oversized path.
		path: PathBuf,
	},
	/// Ref metadata was malformed or non-UTF-8.
	#[error("malformed Git ref metadata at {path:?}")]
	Malformed {
		/// Malformed path.
		path: PathBuf,
	},
	/// Reftable resolution requires Git, but the command failed.
	#[error(transparent)]
	Run(#[from] GitRunError),
	/// Git could not resolve reftable HEAD.
	#[error("Git could not resolve reftable HEAD (status {code})")]
	Reftable {
		/// Git exit status.
		code: i32,
	},
}

/// Parses one exact ref from `packed-refs`, ignoring comments and peeled lines.
pub fn parse_packed_refs(bytes: &[u8], target: &str) -> Result<Option<Str>, RefError> {
	let text = std::str::from_utf8(bytes)
		.map_err(|_| RefError::Malformed { path: PathBuf::from("packed-refs") })?;
	for line in text.lines() {
		let line = line.trim();
		if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
			continue;
		}
		let Some((object, reference)) = line.split_once(' ') else {
			return Err(RefError::Malformed { path: PathBuf::from("packed-refs") });
		};
		if object.is_empty() || reference.is_empty() || reference.contains(char::is_whitespace) {
			return Err(RefError::Malformed { path: PathBuf::from("packed-refs") });
		}
		if reference == target {
			return Ok(Some(object.to_str()));
		}
	}
	Ok(None)
}

/// Detects Git's reftable ref backend from the repository config.
pub async fn is_reftable(repository: &Repository) -> Result<bool, RefError> {
	let config = repository.common_dir.join("config");
	let Some(bytes) = read_optional(&config).await? else {
		return Ok(false);
	};
	let text = std::str::from_utf8(&bytes).map_err(|_| RefError::Malformed { path: config })?;
	let mut extensions = false;
	for raw in text.lines() {
		let line = strip_config_comment(raw).trim();
		if line.starts_with('[') && line.ends_with(']') {
			extensions = line[1..line.len() - 1]
				.trim()
				.eq_ignore_ascii_case("extensions");
			continue;
		}
		if !extensions {
			continue;
		}
		let Some((key, value)) = line.split_once('=') else {
			continue;
		};
		if key.trim().eq_ignore_ascii_case("refstorage") {
			let value = value.trim().trim_matches('"');
			return Ok(value.eq_ignore_ascii_case("reftable")
				|| value.to_ascii_lowercase().starts_with("reftable:"));
		}
	}
	Ok(false)
}

/// Resolves symbolic, detached, unborn, packed-ref, and reftable HEAD states.
pub async fn resolve_head(
	repository: &Repository,
	runner: &GitRunner,
	cancel: &CancellationToken,
) -> Result<HeadState, RefError> {
	if is_reftable(repository).await? {
		return resolve_reftable(repository, runner, cancel).await;
	}
	let head_path = repository.git_dir.join("HEAD");
	let bytes = read_required(&head_path).await?;
	let text =
		std::str::from_utf8(&bytes).map_err(|_| RefError::Malformed { path: head_path.clone() })?;
	let head = single_line(text).ok_or_else(|| RefError::Malformed { path: head_path.clone() })?;
	if let Some(reference) = head.strip_prefix("ref:") {
		let reference = reference.trim();
		if reference.is_empty() || reference.contains(char::is_whitespace) {
			return Err(RefError::Malformed { path: head_path });
		}
		let branch = reference
			.strip_prefix("refs/heads/")
			.map(|value| value.to_str());
		return match read_ref(repository, reference).await? {
			Some(commit) => Ok(HeadState::Branch { reference: reference.to_str(), branch, commit }),
			None => Ok(HeadState::Unborn { reference: reference.to_str(), branch }),
		};
	}
	if head.is_empty() || head.contains(char::is_whitespace) {
		return Err(RefError::Malformed { path: head_path });
	}
	Ok(HeadState::Detached { commit: head.to_str() })
}

/// Reads a loose or packed ref without invoking Git.
pub async fn read_ref(repository: &Repository, reference: &str) -> Result<Option<Str>, RefError> {
	if !valid_ref_path(reference) {
		return Err(RefError::Malformed { path: PathBuf::from(reference) });
	}
	for directory in [&repository.git_dir, &repository.common_dir] {
		let path = directory.join(reference);
		if let Some(bytes) = read_optional(&path).await? {
			let text =
				std::str::from_utf8(&bytes).map_err(|_| RefError::Malformed { path: path.clone() })?;
			let value = single_line(text).ok_or_else(|| RefError::Malformed { path: path.clone() })?;
			if value.is_empty() {
				return Err(RefError::Malformed { path });
			}
			return Ok(Some(value.to_str()));
		}
	}
	for directory in [&repository.git_dir, &repository.common_dir] {
		let path = directory.join("packed-refs");
		if let Some(bytes) = read_optional(&path).await? {
			if let Some(value) = parse_packed_refs(&bytes, reference)? {
				return Ok(Some(value));
			}
		}
	}
	Ok(None)
}

async fn resolve_reftable(
	repository: &Repository,
	runner: &GitRunner,
	cancel: &CancellationToken,
) -> Result<HeadState, RefError> {
	let symbolic = runner
		.run(
			&repository.worktree_root,
			&["symbolic-ref", "--quiet", "HEAD"],
			GitRunOptions { read_only: true, parse_sensitive: true, ..Default::default() },
			cancel,
		)
		.await?;
	let commit = runner
		.run(
			&repository.worktree_root,
			&["rev-parse", "--verify", "HEAD"],
			GitRunOptions { read_only: true, parse_sensitive: true, ..Default::default() },
			cancel,
		)
		.await?;
	let commit = if commit.exit_code == 0 {
		Some(output_scalar(&commit.stdout)?)
	} else {
		None
	};
	if symbolic.exit_code == 0 {
		let reference = output_scalar(&symbolic.stdout)?;
		let branch = reference
			.strip_prefix("refs/heads/")
			.map(|value| value.to_str());
		return match commit {
			Some(commit) => Ok(HeadState::Branch { reference, branch, commit }),
			None => Ok(HeadState::Unborn { reference, branch }),
		};
	}
	commit
		.map(|commit| HeadState::Detached { commit })
		.ok_or(RefError::Reftable { code: symbolic.exit_code })
}

/// Coalescing HEAD invalidation stream. The bounded receiver holds at most one
/// pending invalidation while consumers refresh a snapshot.
pub struct HeadInvalidations {
	receiver: flume::Receiver<()>,
	cancel:   CancellationToken,
}

impl HeadInvalidations {
	/// Starts path-based stat polling, which survives Git's atomic inode
	/// replacement.
	pub async fn start(repository: &Repository) -> Result<Self, RefError> {
		let target = if is_reftable(repository).await? {
			repository.git_dir.join("reftable")
		} else {
			repository.git_dir.join("HEAD")
		};
		let (sender, receiver) = flume::bounded(1);
		let cancel = CancellationToken::new();
		let task_cancel = cancel.clone();
		tokio::spawn(async move {
			let mut previous = fingerprint(&target).await;
			let mut interval = tokio::time::interval(POLL_INTERVAL);
			interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
			loop {
				tokio::select! {
					() = task_cancel.cancelled() => break,
					_ = interval.tick() => {
						let current = fingerprint(&target).await;
						if current != previous {
							previous = current;
							let _ = sender.try_send(());
						}
					},
				}
			}
		});
		Ok(Self { receiver, cancel })
	}

	/// Waits for the next coalesced invalidation.
	pub async fn changed(&self) -> Result<(), flume::RecvError> {
		self.receiver.recv_async().await
	}
}

impl Drop for HeadInvalidations {
	fn drop(&mut self) {
		self.cancel.cancel();
	}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Fingerprint {
	modified: Option<SystemTime>,
	len:      u64,
	inode:    u64,
}

async fn fingerprint(path: &Path) -> Option<Fingerprint> {
	let metadata = tokio::fs::metadata(path).await.ok()?;
	#[cfg(unix)]
	let inode = std::os::unix::fs::MetadataExt::ino(&metadata);
	#[cfg(not(unix))]
	let inode = 0;
	Some(Fingerprint { modified: metadata.modified().ok(), len: metadata.len(), inode })
}

fn strip_config_comment(line: &str) -> &str {
	let mut quoted = false;
	for (index, byte) in line.bytes().enumerate() {
		if byte == b'"' {
			quoted = !quoted;
		} else if !quoted && matches!(byte, b'#' | b';') {
			return &line[..index];
		}
	}
	line
}

fn valid_ref_path(reference: &str) -> bool {
	reference.starts_with("refs/")
		&& !reference
			.split('/')
			.any(|part| part.is_empty() || part == "." || part == "..")
}

fn single_line(text: &str) -> Option<&str> {
	let value = text
		.strip_suffix("\r\n")
		.or_else(|| text.strip_suffix('\n'))
		.unwrap_or(text);
	(!value.contains(['\n', '\r'])).then_some(value.trim())
}

fn output_scalar(bytes: &[u8]) -> Result<Str, RefError> {
	let text = std::str::from_utf8(bytes)
		.map_err(|_| RefError::Malformed { path: PathBuf::from("Git output") })?;
	let scalar = single_line(text)
		.filter(|value| !value.is_empty())
		.ok_or_else(|| RefError::Malformed { path: PathBuf::from("Git output") })?;
	Ok(scalar.to_str())
}

async fn read_required(path: &Path) -> Result<Vec<u8>, RefError> {
	read_optional(path).await?.ok_or_else(|| RefError::Io {
		path:   path.to_path_buf(),
		source: io::Error::new(io::ErrorKind::NotFound, "Git metadata not found"),
	})
}

async fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, RefError> {
	let metadata = match tokio::fs::metadata(path).await {
		Ok(metadata) => metadata,
		Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
		Err(source) => return Err(RefError::Io { path: path.to_path_buf(), source }),
	};
	if metadata.len() > METADATA_LIMIT {
		return Err(RefError::TooLarge { path: path.to_path_buf() });
	}
	tokio::fs::read(path)
		.await
		.map(Some)
		.map_err(|source| RefError::Io { path: path.to_path_buf(), source })
}
