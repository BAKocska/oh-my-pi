//! Verified subagent artifact publication and historical name reservation.

use std::{
	ffi,
	fs::{self, OpenOptions},
	io::{self, Read as _, Write as _},
	path::{Path, PathBuf},
	sync::atomic::{AtomicU64, Ordering},
};

use omp_agent::AgentTree;
use omp_core::{
	Str,
	fs::{AtomicReplaceError, replace_file_atomically},
};

static NEXT_STAGED_ARTIFACT: AtomicU64 = AtomicU64::new(1);

/// Failure while staging, verifying, or publishing a subagent artifact.
#[derive(Debug, thiserror::Error)]
pub enum ArtifactWriteError {
	/// The unique sibling staging file could not be created.
	#[error("subagent artifact staging file could not be created")]
	Create(#[source] io::Error),
	/// The complete payload could not be written.
	#[error("subagent artifact staging write failed")]
	Write(#[source] io::Error),
	/// The staged payload could not be flushed to the filesystem.
	#[error("subagent artifact staging flush failed")]
	Flush(#[source] io::Error),
	/// Staged-file metadata could not be inspected.
	#[error("subagent artifact staging metadata could not be read")]
	Metadata(#[source] io::Error),
	/// The staged file did not contain the exact expected byte count.
	#[error("subagent artifact size mismatch: found {actual} of {expected} bytes")]
	SizeMismatch {
		/// Expected UTF-8 payload byte count.
		expected: u64,
		/// Observed staged-file byte count.
		actual:   u64,
	},
	/// The staged payload could not be read back.
	#[error("subagent artifact staging file is not readable")]
	Read(#[source] io::Error),
	/// The verified staged file could not be atomically published.
	#[error("subagent artifact could not be published")]
	Publish(#[source] AtomicReplaceError),
}

/// Stages complete artifact bytes to a unique sibling, verifies their exact
/// length and readability, then atomically publishes them.
///
/// Any failure removes the staging file and leaves an existing destination
/// untouched. The returned count is the verified on-disk byte length.
pub fn write_verified(path: &Path, content: &[u8]) -> Result<u64, ArtifactWriteError> {
	let staged = staged_path(path);
	let expected = u64::try_from(content.len()).unwrap_or(u64::MAX);
	let result = (|| {
		let mut output = OpenOptions::new()
			.write(true)
			.create_new(true)
			.open(&staged)
			.map_err(ArtifactWriteError::Create)?;
		output
			.write_all(content)
			.map_err(ArtifactWriteError::Write)?;
		output.flush().map_err(ArtifactWriteError::Flush)?;
		output.sync_all().map_err(ArtifactWriteError::Flush)?;
		drop(output);
		let actual = fs::metadata(&staged)
			.map_err(ArtifactWriteError::Metadata)?
			.len();
		if actual != expected {
			return Err(ArtifactWriteError::SizeMismatch { expected, actual });
		}
		let mut readable = fs::File::open(&staged).map_err(ArtifactWriteError::Read)?;
		let mut probe = [0_u8; 1];
		if expected != 0 {
			readable
				.read_exact(&mut probe)
				.map_err(ArtifactWriteError::Read)?;
		}
		drop(readable);
		replace_file_atomically(&staged, path).map_err(ArtifactWriteError::Publish)?;
		Ok(expected)
	})();
	if result.is_err() {
		let _ = fs::remove_file(staged);
	}
	result
}

fn staged_path(path: &Path) -> PathBuf {
	let sequence = NEXT_STAGED_ARTIFACT.fetch_add(1, Ordering::Relaxed);
	let name = path
		.file_name()
		.and_then(|name| name.to_str())
		.unwrap_or("artifact");
	path.with_file_name(format!(".{name}.tmp-{}-{sequence}", std::process::id()))
}

/// Scans journal and output stems before the first new display-name allocation.
pub fn reserve_historical_stems(tree: &AgentTree, directory: &Path) -> io::Result<usize> {
	let mut stems = Vec::new();
	let entries = match fs::read_dir(directory) {
		Ok(entries) => entries,
		Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
		Err(error) => return Err(error),
	};
	for entry in entries {
		let entry = entry?;
		if !entry.file_type()?.is_file() {
			continue;
		}
		let path = entry.path();
		if !matches!(path.extension().and_then(std::ffi::OsStr::to_str), Some("md" | "jsonl")) {
			continue;
		}
		if let Some(stem) = path.file_stem().and_then(ffi::OsStr::to_str)
			&& !stem.starts_with('.')
		{
			stems.push(Str::new(stem));
		}
	}
	stems.sort();
	stems.dedup();
	for stem in &stems {
		tree.reserve_historical_name(stem.as_str());
	}
	Ok(stems.len())
}

/// Normalizes a tiny-model one-line label when no caller name was supplied.
pub fn normalize_generated_label(candidate: &str) -> Option<Str> {
	let line = candidate.lines().next()?.trim();
	if line.is_empty() {
		return None;
	}
	let mut output = String::with_capacity(line.len().min(32));
	for character in line.chars() {
		if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
			output.push(character);
		} else if character.is_whitespace() && !output.ends_with('-') {
			output.push('-');
		}
		if output.len() >= 32 {
			break;
		}
	}
	while output.ends_with('-') {
		output.pop();
	}
	(!output.is_empty()).then(|| Str::from(output))
}
#[cfg(test)]
mod tests {
	use std::fs;

	use super::write_verified;

	#[test]
	fn verified_publication_replaces_complete_artifact_without_staging_leaks() {
		let directory = tempfile::tempdir().expect("temporary directory");
		let target = directory.path().join("Worker.md");
		fs::write(&target, b"prior complete report").expect("seed artifact");

		let written = write_verified(&target, b"replacement report").expect("publish artifact");

		assert_eq!(written, 18);
		assert_eq!(fs::read(&target).expect("read artifact"), b"replacement report");
		assert_eq!(
			fs::read_dir(directory.path())
				.expect("list artifacts")
				.count(),
			1,
			"verified publication must not leak staging or backup files"
		);
	}
}
