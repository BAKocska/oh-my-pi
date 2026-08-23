//! Canonical package/content containment and native placeholder substitution.

use std::{
	collections::BTreeMap,
	fs, io,
	path::{Component, Path, PathBuf},
};

use omp_core::Str;

/// Native package placeholders understood inside static declarations.
pub const PACKAGE_ROOT_PLACEHOLDER: &str = "${OMP_PACKAGE_ROOT}";
/// Native package data placeholder understood inside static declarations.
pub const PACKAGE_DATA_PLACEHOLDER: &str = "${OMP_PACKAGE_DATA}";

/// A path failed package containment validation.
#[derive(Debug, thiserror::Error)]
pub enum ContainmentError {
	/// A declared relative path attempted lexical traversal.
	#[error("declared path traverses outside its package root: {path}")]
	Traversal {
		/// Rejected declaration.
		path: PathBuf,
	},
	/// A declared path or one of its symlinks resolves outside the package root.
	#[error("declared path escapes its package root: {path}")]
	Escape {
		/// Rejected declaration.
		path: PathBuf,
	},
	/// Canonicalization failed.
	#[error("failed to canonicalize package path {path}")]
	Canonicalize {
		/// Path being resolved.
		path:   PathBuf,
		/// Filesystem error.
		#[source]
		source: io::Error,
	},
}

/// Canonicalizes an existing declaration path and proves it remains below an
/// existing canonical package/content root. Symlink escapes are rejected.
pub fn contained_existing(root: &Path, declared: &Path) -> Result<PathBuf, ContainmentError> {
	if declared.is_relative()
		&& declared
			.components()
			.any(|part| matches!(part, Component::ParentDir))
	{
		return Err(ContainmentError::Traversal { path: declared.to_path_buf() });
	}
	let canonical_root = fs::canonicalize(root)
		.map_err(|source| ContainmentError::Canonicalize { path: root.to_path_buf(), source })?;
	let candidate = if declared.is_absolute() {
		declared.to_path_buf()
	} else {
		canonical_root.join(declared)
	};
	let canonical = fs::canonicalize(&candidate)
		.map_err(|source| ContainmentError::Canonicalize { path: candidate.clone(), source })?;
	if !canonical.starts_with(&canonical_root) {
		return Err(ContainmentError::Escape { path: candidate });
	}
	Ok(canonical)
}

/// Rebases a relative executable declaration below the package root and
/// validates the result. Bare executable names are retained for Environment
/// PATH resolution; paths with separators must name a contained file.
pub fn rebase_executable(root: &Path, declared: &Path) -> Result<PathBuf, ContainmentError> {
	if declared.is_absolute() || declared.components().count() > 1 {
		contained_existing(root, declared)
	} else {
		Ok(declared.to_path_buf())
	}
}

/// Recursively substitutes native package root/data placeholders in inert JSON
/// manifest data. No shell expansion or foreign placeholder vocabulary exists.
pub fn substitute_package_placeholders(
	value: &mut serde_json::Value,
	package_root: &Path,
	package_data: &Path,
) {
	match value {
		serde_json::Value::String(text) => {
			let root = package_root.to_string_lossy();
			let data = package_data.to_string_lossy();
			*text = text
				.replace(PACKAGE_ROOT_PLACEHOLDER, &root)
				.replace(PACKAGE_DATA_PLACEHOLDER, &data);
		},
		serde_json::Value::Array(values) => {
			for item in values {
				substitute_package_placeholders(item, package_root, package_data);
			}
		},
		serde_json::Value::Object(values) => {
			for item in values.values_mut() {
				substitute_package_placeholders(item, package_root, package_data);
			}
		},
		_ => {},
	}
}

/// Recursively substitutes placeholders in literal environment maps.
pub fn substitute_environment(
	environment: &BTreeMap<Str, Str>,
	package_root: &Path,
	package_data: &Path,
) -> BTreeMap<Str, Str> {
	let root = package_root.to_string_lossy();
	let data = package_data.to_string_lossy();
	environment
		.iter()
		.map(|(key, value)| {
			let value = value
				.as_str()
				.replace(PACKAGE_ROOT_PLACEHOLDER, &root)
				.replace(PACKAGE_DATA_PLACEHOLDER, &data);
			(key.clone(), Str::from(value))
		})
		.collect()
}

#[cfg(test)]
mod tests {
	use std::fs;

	use super::*;

	#[test]
	fn rejects_parent_traversal_and_symlink_escape() {
		let tree = tempfile::tempdir().unwrap();
		let root = tree.path().join("package");
		fs::create_dir(&root).unwrap();
		fs::write(tree.path().join("outside"), "secret").unwrap();
		assert!(matches!(
			contained_existing(&root, Path::new("../outside")),
			Err(ContainmentError::Traversal { .. })
		));
		#[cfg(unix)]
		{
			use std::os::unix::fs;
			fs::symlink(tree.path().join("outside"), root.join("escape")).unwrap();
			assert!(matches!(
				contained_existing(&root, Path::new("escape")),
				Err(ContainmentError::Escape { .. })
			));
		}
	}

	#[test]
	fn recursively_substitutes_only_native_placeholders() {
		let mut value = serde_json::json!({"path": "${OMP_PACKAGE_ROOT}/bin", "nested": ["${OMP_PACKAGE_DATA}/x"]});
		substitute_package_placeholders(&mut value, Path::new("/pkg"), Path::new("/data"));
		assert_eq!(value["path"], "/pkg/bin");
		assert_eq!(value["nested"][0], "/data/x");
	}
}
