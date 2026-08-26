//! Purely lexical path normalization.
//!
//! This module removes redundant path components without consulting the
//! filesystem, so normalization neither resolves symbolic links nor requires
//! the path to exist.
//!
//! # Example
//!
//! ```
//! use std::path::Path;
//!
//! use omp_core::NormalizePath as _;
//!
//! assert_eq!(Path::new("alpha/./beta/../gamma").normalize(), Path::new("alpha/gamma"));
//! ```

use std::path::{Component, Path, PathBuf};
/// Shortens an absolute display path under `home` to `~`.
///
/// Windows drive and UNC paths compare the home prefix case-insensitively and
/// normalize the retained suffix to forward slashes. Other path styles retain
/// case-sensitive prefix semantics.
pub fn shorten_home_path(path: &str, home: &str) -> Option<String> {
	if home.is_empty() || path.len() < home.len() {
		return None;
	}
	let windows_style = is_windows_absolute(home);
	let prefix = path.as_bytes().get(..home.len())?;
	let matches = if windows_style {
		path
			.get(..home.len())?
			.chars()
			.flat_map(char::to_lowercase)
			.eq(home.chars().flat_map(char::to_lowercase))
	} else {
		prefix == home.as_bytes()
	};
	if !matches {
		return None;
	}
	let suffix = path.get(home.len()..)?;
	if !suffix.is_empty() && !suffix.starts_with('/') && !suffix.starts_with('\\') {
		return None;
	}
	Some(format!("~{}", suffix.replace('\\', "/")))
}

fn is_windows_absolute(path: &str) -> bool {
	let bytes = path.as_bytes();
	(bytes.len() >= 3
		&& bytes[0].is_ascii_alphabetic()
		&& bytes[1] == b':'
		&& matches!(bytes[2], b'/' | b'\\'))
		|| path.starts_with(r"\\")
}

/// Extends [`Path`] with purely lexical normalization.
pub trait NormalizePath {
	/// Normalizes this path without accessing the filesystem.
	///
	/// Current-directory components are removed and parent-directory
	/// components cancel preceding normal components. Parent components above
	/// an absolute root are discarded, while leading parent components in a
	/// relative path are preserved.
	fn normalize(&self) -> PathBuf;

	/// Normalizes a relative path that must not escape its starting directory.
	///
	/// Returns `None` for absolute or prefixed paths and when a parent-directory
	/// component would escape above the path's starting directory.
	fn try_normalize(&self) -> Option<PathBuf>;
}

impl NormalizePath for Path {
	fn normalize(&self) -> PathBuf {
		let mut normalized = PathBuf::new();
		let mut normal_depth = 0usize;
		let mut anchored = false;

		for component in self.components() {
			match component {
				Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
				Component::RootDir => {
					normalized.push(component.as_os_str());
					normal_depth = 0;
					anchored = true;
				},
				Component::CurDir => {},
				Component::ParentDir if normal_depth != 0 => {
					let removed = normalized.pop();
					debug_assert!(removed, "normal component must be removable");
					normal_depth -= 1;
				},
				Component::ParentDir if !anchored => normalized.push(component.as_os_str()),
				Component::ParentDir => {},
				Component::Normal(part) => {
					normalized.push(part);
					normal_depth += 1;
				},
			}
		}

		normalized
	}

	fn try_normalize(&self) -> Option<PathBuf> {
		let mut normalized = PathBuf::new();

		for component in self.components() {
			match component {
				Component::Prefix(_) | Component::RootDir => return None,
				Component::CurDir => {},
				Component::ParentDir if normalized.pop() => {},
				Component::ParentDir => return None,
				Component::Normal(part) => normalized.push(part),
			}
		}

		Some(normalized)
	}
}

#[cfg(test)]
mod tests {
	use std::path::{Path, PathBuf};

	use super::{NormalizePath as _, shorten_home_path};

	#[test]
	fn collapses_dot_dotdot_and_trailing_separators() {
		assert_eq!(Path::new("alpha/./beta/../gamma//").normalize(), Path::new("alpha/gamma"));
	}
	#[test]
	fn shortens_windows_home_case_insensitively_at_component_boundary() {
		assert_eq!(
			shorten_home_path(r"c:\users\alice\projects\demo", r"C:\Users\Alice",).as_deref(),
			Some("~/projects/demo")
		);
		assert_eq!(shorten_home_path(r"C:\Users\Alice2\demo", r"C:\Users\Alice"), None);
	}

	#[cfg(unix)]
	#[test]
	fn parent_components_cannot_escape_root() {
		assert_eq!(Path::new("/../../alpha").normalize(), Path::new("/alpha"));
	}

	#[test]
	fn preserves_leading_relative_parent_components() {
		assert_eq!(Path::new("../../alpha/../beta").normalize(), Path::new("../../beta"));
		assert_eq!(Path::new("alpha/../../beta").normalize(), Path::new("../beta"));
	}

	#[test]
	fn rejects_unsafe_relative_paths() {
		assert_eq!(Path::new("alpha/../beta").try_normalize(), Some(PathBuf::from("beta")));
		assert_eq!(Path::new("alpha/../../beta").try_normalize(), None);
		assert_eq!(Path::new("../alpha").try_normalize(), None);
	}

	#[test]
	fn normalization_is_idempotent() {
		for path in ["", ".", "alpha", "../alpha", "alpha/./beta/../../gamma"] {
			let normalized = Path::new(path).normalize();
			assert_eq!(normalized.normalize(), normalized);
		}
	}

	#[cfg(windows)]
	#[test]
	fn preserves_drive_unc_and_verbatim_prefixes() {
		assert_eq!(Path::new(r"C:\alpha\.\beta\..\gamma").normalize(), Path::new(r"C:\alpha\gamma"));
		assert_eq!(
			Path::new(r"\\server\share\alpha\..\beta").normalize(),
			Path::new(r"\\server\share\beta")
		);
		assert_eq!(Path::new(r"\\?\C:\alpha\..\beta").normalize(), Path::new(r"\\?\C:\beta"));
		assert_eq!(
			Path::new(r"\\?\UNC\server\share\alpha\..\beta").normalize(),
			Path::new(r"\\?\UNC\server\share\beta")
		);
	}
}
