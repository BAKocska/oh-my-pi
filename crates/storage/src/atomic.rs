//! Guarded whole-file atomic commits.
//!
//! Temporary files are created beside the destination so the final rename is
//! on one filesystem. A rejected guard removes the temporary file and leaves
//! the destination untouched.

use std::{
	fs::{File, OpenOptions},
	io::Write as _,
	path::{Path, PathBuf},
};

use thiserror::Error;

/// Failure from a guarded whole-file commit.
#[derive(Debug, Error)]
pub enum Error {
	/// Destination has no parent directory.
	#[error("atomic destination has no parent directory")]
	MissingParent,
	/// Temporary-file or rename I/O failed.
	#[error("atomic file commit failed for {path}")]
	Io {
		/// Destination being committed.
		path:   PathBuf,
		/// Underlying filesystem failure.
		#[source]
		source: std::io::Error,
	},
	/// The synchronous guard refused publication.
	#[error("atomic file commit guard refused publication")]
	GuardRefused,
}

/// Writes `bytes`, synchronizes them, checks `guard`, and atomically renames.
pub fn commit(path: &Path, bytes: &[u8], guard: impl FnOnce() -> bool) -> Result<(), Error> {
	let parent = path.parent().ok_or(Error::MissingParent)?;
	let name = path
		.file_name()
		.ok_or(Error::MissingParent)?
		.to_string_lossy();
	let nonce = omp_core::Ulid::generate();
	let temporary = parent.join(format!(".{name}.{nonce}.tmp"));
	let result = commit_inner(path, &temporary, bytes, guard);
	if result.is_err() {
		match std::fs::remove_file(&temporary) {
			Ok(()) => {},
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
			Err(_) => {},
		}
	}
	result
}

fn commit_inner(
	path: &Path,
	temporary: &Path,
	bytes: &[u8],
	guard: impl FnOnce() -> bool,
) -> Result<(), Error> {
	let mut file = OpenOptions::new()
		.write(true)
		.create_new(true)
		.open(temporary)
		.map_err(|source| Error::Io { path: path.to_owned(), source })?;
	file
		.write_all(bytes)
		.map_err(|source| Error::Io { path: path.to_owned(), source })?;
	file
		.sync_all()
		.map_err(|source| Error::Io { path: path.to_owned(), source })?;
	if !guard() {
		return Err(Error::GuardRefused);
	}
	std::fs::rename(temporary, path)
		.map_err(|source| Error::Io { path: path.to_owned(), source })?;
	sync_parent(path)?;
	Ok(())
}

fn sync_parent(path: &Path) -> Result<(), Error> {
	let parent = path.parent().ok_or(Error::MissingParent)?;
	let directory =
		File::open(parent).map_err(|source| Error::Io { path: path.to_owned(), source })?;
	directory
		.sync_all()
		.map_err(|source| Error::Io { path: path.to_owned(), source })
}

#[cfg(test)]
mod tests {
	use tempfile::tempdir;

	use super::*;

	#[test]
	fn guard_refusal_keeps_prior_file() {
		let directory = tempdir().expect("temporary directory");
		let path = directory.path().join("state.json");
		std::fs::write(&path, b"old").expect("seed file");
		assert!(matches!(commit(&path, b"new", || false), Err(Error::GuardRefused)));
		assert_eq!(std::fs::read(path).expect("read destination"), b"old");
	}

	#[test]
	fn accepted_commit_replaces_whole_file() {
		let directory = tempdir().expect("temporary directory");
		let path = directory.path().join("state.json");
		std::fs::write(&path, b"old").expect("seed file");
		commit(&path, b"new", || true).expect("commit");
		assert_eq!(std::fs::read(path).expect("read destination"), b"new");
	}
}
