//! Build identity of the running executable.
//!
//! Project daemons only need to know whether they were launched from the same
//! local executable generation. Content addressability is unnecessary: the
//! client launches its daemon from the same file, while a relink replaces or
//! mutates that file.
//!
//! The identity hashes the executable path and filesystem generation metadata.
//! Its cost is constant in the executable size on every supported platform; it
//! never opens or reads the executable contents.

use std::{path::Path, sync::LazyLock};

use omp_core::{Hash32, hex::ArrayStr};

/// Returns the memoized local generation identity of the current executable,
/// or an empty string when its filesystem metadata cannot be read.
///
/// An empty identity means "unknown": callers must never initiate daemon
/// replacement from an unknown identity, and must treat an empty advertised
/// identity as stale only when their own identity is known.
pub fn current() -> &'static str {
	static BUILD_ID: LazyLock<ArrayStr<32>> = LazyLock::new(compute);
	BUILD_ID.as_str()
}

/// Returns whether a daemon advertising `theirs` should be replaced by a
/// client whose identity is `ours`.
///
/// Replacement requires a known local identity; a daemon with an unknown
/// (empty) identity predates build identification and counts as stale.
#[must_use]
pub fn is_stale(ours: &str, theirs: &str) -> bool {
	!ours.is_empty() && ours != theirs
}

fn compute() -> ArrayStr<32> {
	std::env::current_exe()
		.and_then(|executable| fingerprint(&executable))
		.unwrap_or_default()
}

fn fingerprint(executable: &Path) -> std::io::Result<ArrayStr<32>> {
	let metadata = std::fs::metadata(executable)?;
	let mut digest = Hash32::hasher();
	digest.update(b"omp/executable-generation/v1");

	let path = executable.as_os_str().as_encoded_bytes();
	digest.update((path.len() as u64).to_le_bytes());
	digest.update(path);
	digest.update(metadata.len().to_le_bytes());

	#[cfg(unix)]
	{
		use std::os::unix::fs::MetadataExt as _;

		digest.update(metadata.dev().to_le_bytes());
		digest.update(metadata.ino().to_le_bytes());
		digest.update(metadata.mtime().to_le_bytes());
		digest.update(metadata.mtime_nsec().to_le_bytes());
		digest.update(metadata.ctime().to_le_bytes());
		digest.update(metadata.ctime_nsec().to_le_bytes());
	}

	#[cfg(windows)]
	{
		use std::os::windows::fs::MetadataExt as _;

		digest.update(metadata.creation_time().to_le_bytes());
		digest.update(metadata.last_write_time().to_le_bytes());
	}

	#[cfg(not(any(unix, windows)))]
	{
		let (before_epoch, modified) =
			match metadata.modified()?.duration_since(std::time::UNIX_EPOCH) {
				Ok(modified) => (false, modified),
				Err(error) => (true, error.duration()),
			};
		digest.update([u8::from(before_epoch)]);
		digest.update(modified.as_secs().to_le_bytes());
		digest.update(modified.subsec_nanos().to_le_bytes());
	}

	Ok(digest.finalize().to_hex())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn current_is_stable_nonempty_hex() {
		let first = current();
		assert_eq!(first, current());
		assert!(!first.is_empty(), "test executable must be identifiable");
		assert_eq!(first.len(), 64);
		assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
	}

	#[test]
	fn fingerprint_is_stable_and_changes_with_file_generation() {
		let directory = tempfile::tempdir().expect("temporary executable directory");
		let executable = directory.path().join("omp");
		std::fs::write(&executable, b"first generation").expect("write first generation");

		let first = fingerprint(&executable).expect("fingerprint first generation");
		assert_eq!(
			first.as_str(),
			fingerprint(&executable)
				.expect("fingerprint unchanged generation")
				.as_str()
		);

		std::fs::write(&executable, b"replacement executable generation")
			.expect("write replacement generation");
		let replacement = fingerprint(&executable).expect("fingerprint replacement generation");
		assert_ne!(first.as_str(), replacement.as_str());
	}

	#[test]
	fn staleness_requires_known_local_identity() {
		assert!(!is_stale("", "abc"));
		assert!(!is_stale("", ""));
		assert!(is_stale("abc", ""));
		assert!(is_stale("abc", "def"));
		assert!(!is_stale("abc", "abc"));
	}
}
