//! Canonical Git repository and linked-worktree discovery.

use std::{
	io,
	path::{Path, PathBuf},
};

use tokio::io::AsyncReadExt as _;

const POINTER_LIMIT_BYTES: u64 = 64 * 1024;

/// Canonical repository paths resolved from a working-directory descendant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Repository {
	/// Canonical root of the selected working tree.
	pub worktree_root: PathBuf,
	/// Canonical per-worktree Git administration directory.
	pub git_dir:       PathBuf,
	/// Canonical shared Git administration directory.
	pub common_dir:    PathBuf,
	/// Canonical identity shared by linked worktrees.
	pub primary_root:  PathBuf,
	/// Whether the repository has no working tree.
	pub bare:          bool,
}

/// Structural faults in a `.git` or `commondir` pointer.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PointerError {
	/// The pointer exceeded the bounded metadata size.
	#[error("pointer exceeds 64 KiB")]
	TooLarge,
	/// The pointer was not UTF-8 text.
	#[error("pointer is not UTF-8")]
	NonUtf8,
	/// The pointer contained more than one logical line.
	#[error("pointer must contain exactly one line")]
	MultipleLines,
	/// The `.git` file did not contain the required prefix and path.
	#[error("pointer must have the form `gitdir: <path>`")]
	MalformedGitDir,
	/// `commondir` must be relative to the per-worktree administration
	/// directory.
	#[error("commondir must be a relative path")]
	AbsoluteCommonDir,
	/// The pointer did not resolve to a valid Git administration directory.
	#[error("pointer target is not a Git administration directory")]
	InvalidTarget,
	/// A linked-worktree administration directory escaped its shared repository.
	#[error("linked-worktree administration directory escapes commondir/worktrees")]
	EscapingCommonDir,
}

/// Repository discovery failures.
#[derive(Debug, thiserror::Error)]
pub enum RepositoryError {
	/// Filesystem access failed while resolving repository metadata.
	#[error("failed to inspect repository path {path:?}")]
	Io {
		/// Path being inspected.
		path:   PathBuf,
		/// Underlying filesystem error.
		#[source]
		source: io::Error,
	},
	/// A Git pointer was present but structurally invalid.
	#[error("invalid Git pointer {path:?}")]
	InvalidPointer {
		/// Pointer file.
		path:   PathBuf,
		/// Structural reason the pointer was rejected.
		#[source]
		source: PointerError,
	},
}

/// Searches `start` and its ancestors for a canonical Git repository.
///
/// A malformed marker is rejected rather than skipped: silently walking past it
/// could grant repository authority to an unrelated ancestor.
pub async fn discover(start: &Path) -> Result<Option<Repository>, RepositoryError> {
	let mut current = canonicalize(start).await?;
	if !metadata(&current).await?.is_dir() {
		return Ok(None);
	}
	loop {
		let marker = current.join(".git");
		match tokio::fs::metadata(&marker).await {
			Ok(marker_metadata) => {
				return resolve_marker(current, marker, marker_metadata.is_dir())
					.await
					.map(Some);
			},
			Err(error) if error.kind() == io::ErrorKind::NotFound => {},
			Err(source) => return Err(RepositoryError::Io { path: marker, source }),
		}
		if is_bare_repository(&current).await? {
			return Ok(Some(Repository {
				worktree_root: current.clone(),
				git_dir:       current.clone(),
				common_dir:    current.clone(),
				primary_root:  current,
				bare:          true,
			}));
		}
		let Some(parent) = current.parent() else {
			return Ok(None);
		};
		if parent == current {
			return Ok(None);
		}
		current = parent.to_path_buf();
	}
}

async fn resolve_marker(
	worktree_root: PathBuf,
	marker: PathBuf,
	marker_is_dir: bool,
) -> Result<Repository, RepositoryError> {
	let git_dir = if marker_is_dir {
		canonicalize(&marker).await?
	} else {
		let text = read_pointer(&marker).await?;
		let target = parse_git_dir(&marker, &text)?;
		let target = if target.is_absolute() {
			target
		} else {
			worktree_root.join(target)
		};
		let target = canonicalize_pointer_target(&marker, &target).await?;
		validate_git_dir(&marker, &target).await?;
		target
	};
	validate_git_dir(&marker, &git_dir).await?;

	let common_marker = git_dir.join("commondir");
	let (common_dir, linked) = match tokio::fs::metadata(&common_marker).await {
		Ok(metadata) if metadata.is_file() => {
			let text = read_pointer(&common_marker).await?;
			let relative = parse_single_line(&common_marker, &text)?;
			let relative = PathBuf::from(relative);
			if relative.is_absolute() {
				return invalid(common_marker, PointerError::AbsoluteCommonDir);
			}
			let common_dir =
				canonicalize_pointer_target(&common_marker, &git_dir.join(relative)).await?;
			validate_git_dir(&common_marker, &common_dir).await?;
			let worktrees = common_dir.join("worktrees");
			if !git_dir.starts_with(&worktrees) {
				return invalid(common_marker, PointerError::EscapingCommonDir);
			}
			(common_dir, true)
		},
		Ok(_) => return invalid(common_marker, PointerError::InvalidTarget),
		Err(error) if error.kind() == io::ErrorKind::NotFound => (git_dir.clone(), false),
		Err(source) => return Err(RepositoryError::Io { path: common_marker, source }),
	};

	let primary_root = if common_dir.file_name().is_some_and(|name| name == ".git") {
		common_dir.parent().unwrap_or(&common_dir).to_path_buf()
	} else if linked {
		common_dir.clone()
	} else {
		worktree_root.clone()
	};
	Ok(Repository { worktree_root, git_dir, common_dir, primary_root, bare: false })
}

async fn is_bare_repository(path: &Path) -> Result<bool, RepositoryError> {
	for child in ["HEAD", "objects", "refs"] {
		let candidate = path.join(child);
		match tokio::fs::metadata(&candidate).await {
			Ok(metadata)
				if child == "HEAD" && metadata.is_file() || child != "HEAD" && metadata.is_dir() => {},
			Ok(_) => return Ok(false),
			Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
			Err(source) => return Err(RepositoryError::Io { path: candidate, source }),
		}
	}
	Ok(true)
}

async fn validate_git_dir(pointer: &Path, target: &Path) -> Result<(), RepositoryError> {
	let head = target.join("HEAD");
	match tokio::fs::metadata(&head).await {
		Ok(metadata) if metadata.is_file() => Ok(()),
		Ok(_) => invalid(pointer.to_path_buf(), PointerError::InvalidTarget),
		Err(error) if error.kind() == io::ErrorKind::NotFound => {
			invalid(pointer.to_path_buf(), PointerError::InvalidTarget)
		},
		Err(source) => Err(RepositoryError::Io { path: head, source }),
	}
}

async fn read_pointer(path: &Path) -> Result<String, RepositoryError> {
	let length = metadata(path).await?.len();
	if length > POINTER_LIMIT_BYTES {
		return invalid(path.to_path_buf(), PointerError::TooLarge);
	}
	let file = tokio::fs::File::open(path)
		.await
		.map_err(|source| RepositoryError::Io { path: path.to_path_buf(), source })?;
	let mut bytes = Vec::with_capacity(length as usize);
	file
		.take(POINTER_LIMIT_BYTES + 1)
		.read_to_end(&mut bytes)
		.await
		.map_err(|source| RepositoryError::Io { path: path.to_path_buf(), source })?;
	if bytes.len() as u64 > POINTER_LIMIT_BYTES {
		return invalid(path.to_path_buf(), PointerError::TooLarge);
	}
	String::from_utf8(bytes).map_err(|_| RepositoryError::InvalidPointer {
		path:   path.to_path_buf(),
		source: PointerError::NonUtf8,
	})
}

fn parse_git_dir(path: &Path, text: &str) -> Result<PathBuf, RepositoryError> {
	let line = parse_single_line(path, text)?;
	let prefix_length = "gitdir:".len();
	if !line
		.as_bytes()
		.get(..prefix_length)
		.is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"gitdir:"))
	{
		return invalid(path.to_path_buf(), PointerError::MalformedGitDir);
	}
	let Some(value) = line.get(prefix_length..) else {
		return invalid(path.to_path_buf(), PointerError::MalformedGitDir);
	};
	let value = value.trim();
	if value.is_empty() {
		return invalid(path.to_path_buf(), PointerError::MalformedGitDir);
	}
	Ok(PathBuf::from(value))
}

fn parse_single_line<'a>(path: &Path, text: &'a str) -> Result<&'a str, RepositoryError> {
	if text.contains('\0') {
		return invalid(path.to_path_buf(), PointerError::MultipleLines);
	}
	let trimmed = text.trim_end_matches(['\r', '\n']);
	if trimmed.is_empty() || trimmed.contains(['\r', '\n']) {
		return invalid(path.to_path_buf(), PointerError::MultipleLines);
	}
	Ok(trimmed.trim())
}

async fn canonicalize(path: &Path) -> Result<PathBuf, RepositoryError> {
	tokio::fs::canonicalize(path)
		.await
		.map_err(|source| RepositoryError::Io { path: path.to_path_buf(), source })
}

async fn canonicalize_pointer_target(
	pointer: &Path,
	target: &Path,
) -> Result<PathBuf, RepositoryError> {
	tokio::fs::canonicalize(target)
		.await
		.map_err(|error| match error.kind() {
			io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => {
				RepositoryError::InvalidPointer {
					path:   pointer.to_path_buf(),
					source: PointerError::InvalidTarget,
				}
			},
			_ => RepositoryError::Io { path: target.to_path_buf(), source: error },
		})
}

async fn metadata(path: &Path) -> Result<std::fs::Metadata, RepositoryError> {
	tokio::fs::metadata(path)
		.await
		.map_err(|source| RepositoryError::Io { path: path.to_path_buf(), source })
}

fn invalid<T>(path: PathBuf, source: PointerError) -> Result<T, RepositoryError> {
	Err(RepositoryError::InvalidPointer { path, source })
}
