//! Shared model-path normalization for workspace tools.

use std::path::{Component, Path, PathBuf};

use omp_core::Str;

/// A colon selector split from its path without mistaking Windows drive syntax.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathSelector {
	/// Path spelling with the selector suffix removed.
	pub path:     Str,
	/// Trailing selector expression when present.
	pub selector: Option<Str>,
}
/// Splits a trailing colon selector while retaining `C:` and `C:\...` drives.
#[must_use]
pub fn split_colon_selector(input: &str) -> PathSelector {
	let drive_end = usize::from(
		input.len() >= 2 && input.as_bytes()[0].is_ascii_alphabetic() && input.as_bytes()[1] == b':',
	) * 2;
	let selector = input[drive_end..]
		.find(':')
		.map(|index| drive_end + index)
		.filter(|index| !input[index + 1..].is_empty());
	match selector {
		Some(index) => PathSelector {
			path:     Str::from(&input[..index]),
			selector: Some(Str::from(&input[index + 1..])),
		},
		None => PathSelector { path: Str::from(input), selector: None },
	}
}
/// Resolves a model-facing workspace path without permitting traversal outside
/// `root`.
pub fn confined(root: &Path, input: &str) -> Result<PathBuf, PathError> {
	let candidate = Path::new(input);
	if candidate.is_absolute() {
		return Err(PathError::Absolute);
	}
	let mut resolved = PathBuf::from(root);
	for component in candidate.components() {
		match component {
			Component::Normal(part) => resolved.push(part),
			Component::CurDir => {},
			Component::ParentDir => {
				if resolved == root {
					return Err(PathError::Escape);
				}
				resolved.pop();
			},
			Component::RootDir | Component::Prefix(_) => return Err(PathError::Absolute),
		}
	}
	Ok(resolved)
}
/// Path normalization rejection.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PathError {
	/// An absolute input cannot be confined to the workspace root.
	#[error("absolute paths are outside the workspace")]
	Absolute,
	/// Parent traversal would escape the workspace root.
	#[error("path escapes the workspace")]
	Escape,
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn preserves_windows_drive_while_splitting_selector() {
		assert_eq!(split_colon_selector("C:\\repo\\a.rs:4-8"), PathSelector {
			path:     Str::from("C:\\repo\\a.rs"),
			selector: Some(Str::from("4-8")),
		});
	}
	#[test]
	fn confines_relative_paths() {
		let root = Path::new("/workspace");
		assert_eq!(
			confined(root, "src/../Cargo.toml").unwrap(),
			PathBuf::from("/workspace/Cargo.toml")
		);
		assert_eq!(confined(root, "../secret"), Err(PathError::Escape));
	}
}
