//! POSIX compatibility-level selection through `_POSIX2_VERSION`.

use std::env;

/// POSIX 1003.2-1992, which selects obsolete compatibility behavior.
pub(crate) const OBSOLETE: i64 = 199_209;
/// POSIX 1003.1-2001, the lower bound for traditional compatibility behavior.
pub(crate) const TRADITIONAL: i64 = 200_112;
/// POSIX 1003.1-2008, the lower bound for modern behavior.
pub(crate) const MODERN: i64 = 200_809;

/// Returns a parsed `_POSIX2_VERSION`, or `None` when it is absent or invalid.
#[inline]
pub(crate) fn posix_version() -> Option<i64> {
	env::var("_POSIX2_VERSION").ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
	use std::{env, ffi::OsString, sync::Mutex};

	use super::{MODERN, OBSOLETE, TRADITIONAL, posix_version};

	static ENV_LOCK: Mutex<()> = Mutex::new(());

	struct Restore(Option<OsString>);

	impl Drop for Restore {
		fn drop(&mut self) {
			if let Some(value) = self.0.take() {
				// SAFETY: ENV_LOCK serializes all mutations made by this test module.
				unsafe { env::set_var("_POSIX2_VERSION", value) };
			} else {
				// SAFETY: ENV_LOCK serializes all mutations made by this test module.
				unsafe { env::remove_var("_POSIX2_VERSION") };
			}
		}
	}

	#[test]
	fn parses_known_versions_and_rejects_invalid_values() {
		let _guard = ENV_LOCK.lock().expect("environment test lock poisoned");
		let _restore = Restore(env::var_os("_POSIX2_VERSION"));

		for expected in [OBSOLETE, TRADITIONAL, MODERN, -1] {
			// SAFETY: ENV_LOCK serializes all mutations made by this test module.
			unsafe { env::set_var("_POSIX2_VERSION", expected.to_string()) };
			assert_eq!(posix_version(), Some(expected));
		}

		// SAFETY: ENV_LOCK serializes all mutations made by this test module.
		unsafe { env::set_var("_POSIX2_VERSION", "not-a-version") };
		assert_eq!(posix_version(), None);
		// SAFETY: ENV_LOCK serializes all mutations made by this test module.
		unsafe { env::remove_var("_POSIX2_VERSION") };
		assert_eq!(posix_version(), None);
	}
}
