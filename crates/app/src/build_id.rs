//! Build identity of the running executable.
//!
//! Project daemons advertise this identity in their hello frames so clients
//! from a different build can detect a stale daemon and replace it. The
//! identity changes on every relink — including dependency-only rebuilds —
//! and is identical for byte-identical binaries regardless of path.
//!
//! On macOS this is the linker-assigned `LC_UUID`, read from the running
//! image's own load commands in microseconds. Elsewhere it falls back to a
//! blake3 content hash of the executable (memory-mapped; the kernels are
//! compiled optimized even in dev profiles — see the root `Cargo.toml`
//! profile override).

use std::sync::LazyLock;

use omp_core::Hash32;

/// Returns the memoized build identity of the current executable, or an
/// empty string when it cannot be determined.
///
/// An empty identity means "unknown": callers must never initiate daemon
/// replacement from an unknown identity, and must treat an empty advertised
/// identity as stale only when their own identity is known.
pub fn current() -> &'static str {
	static BUILD_ID: LazyLock<String> = LazyLock::new(compute);
	&BUILD_ID
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

fn compute() -> String {
	#[cfg(target_os = "macos")]
	if let Some(uuid) = link_uuid() {
		return omp_core::hex::encode(&uuid).into_string();
	}
	content_hash()
}

/// The linker-assigned `LC_UUID` of the main executable image.
///
/// ld64 stamps a fresh UUID on every link, so dependency-only rebuilds are
/// covered without touching the file system.
#[cfg(target_os = "macos")]
fn link_uuid() -> Option<[u8; 16]> {
	use libc::{load_command, mach_header_64};

	// `libc` lacks these two Mach-O items; layouts per <mach-o/loader.h>.
	const LC_UUID: u32 = 0x1b;
	#[repr(C)]
	struct uuid_command {
		cmd:     u32,
		cmdsize: u32,
		uuid:    [u8; 16],
	}

	unsafe extern "C" {
		fn _dyld_get_image_header(image_index: u32) -> *const mach_header_64;
	}

	// SAFETY: image 0 is the main executable; dyld keeps its header and load
	// commands mapped for the process lifetime, so the walk below reads only
	// live image memory bounded by `sizeofcmds`.
	unsafe {
		let header = _dyld_get_image_header(0);
		if header.is_null() {
			return None;
		}
		let mut cursor = header.add(1).cast::<u8>();
		let end = cursor.add((*header).sizeofcmds as usize);
		for _ in 0..(*header).ncmds {
			if cursor.add(size_of::<load_command>()) > end.cast_mut().cast_const() {
				return None;
			}
			let command = cursor.cast::<load_command>();
			let size = (*command).cmdsize as usize;
			if size < size_of::<load_command>() || cursor.add(size) > end {
				return None;
			}
			if (*command).cmd == LC_UUID && size >= size_of::<uuid_command>() {
				return Some((*cursor.cast::<uuid_command>()).uuid);
			}
			cursor = cursor.add(size);
		}
		None
	}
}

/// BLAKE3 content hash of the executable, memory-mapped to avoid loading
/// the whole binary.
fn content_hash() -> String {
	std::env::current_exe()
		.and_then(|exe| {
			let mut hasher = Hash32::hasher();
			hasher.update_mmap(exe)?;
			Ok(hasher.finalize().to_hex().to_string())
		})
		.unwrap_or_default()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn current_is_stable_nonempty_hex() {
		let first = current();
		assert_eq!(first, current());
		assert!(!first.is_empty(), "test executable must be identifiable");
		// LC_UUID renders as 32 hex chars, the content-hash fallback as 64.
		assert!(first.len() == 32 || first.len() == 64, "unexpected length {}", first.len());
		assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
	}

	#[cfg(target_os = "macos")]
	#[test]
	fn link_uuid_is_present_on_macos() {
		assert!(link_uuid().is_some(), "ld64 always emits LC_UUID");
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
