//! Active repository adoption for a non-repository workspace directory.

use std::{
	env, fs, io,
	path::{Component, Path, PathBuf},
};

/// Why a repository root was adopted for workspace context.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum ActiveRepoSource {
	/// Exactly one direct child carries a Git directory or worktree marker.
	SingleDirectChildRepo,
}

/// Repository context adopted without changing the process working directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveRepoContext {
	/// Absolute workspace directory supplied by the caller.
	pub cwd:                PathBuf,
	/// Absolute adopted repository root.
	pub repo_root:          PathBuf,
	/// Repository root relative to `cwd`.
	pub relative_repo_root: PathBuf,
	/// Evidence used for adoption.
	pub source:             ActiveRepoSource,
}

/// Returns an adopted direct-child repository only when `cwd` is outside any
/// Git repository and exactly one direct child has a `.git` file or directory.
///
/// Directory symlinks are followed for classification, matching ordinary
/// filesystem navigation, while the returned root preserves the caller-visible
/// child path.
pub fn resolve_active_repo_context(cwd: &Path) -> io::Result<Option<ActiveRepoContext>> {
	let cwd = absolute_lexical(cwd)?;
	if is_inside_repository(&cwd) {
		return Ok(None);
	}
	let mut children = fs::read_dir(&cwd)?
		.filter_map(Result::ok)
		.collect::<Vec<_>>();
	children.sort_unstable_by(|left, right| left.file_name().cmp(&right.file_name()));
	let mut adopted = None;
	for child in children {
		let child_path = cwd.join(child.file_name());
		let is_directory = child
			.file_type()
			.map(|kind| kind.is_dir() || kind.is_symlink())
			.unwrap_or(false)
			&& fs::metadata(&child_path).is_ok_and(|metadata| metadata.is_dir());
		if !is_directory || !is_git_marker(&child_path.join(".git")) {
			continue;
		}
		if adopted.is_some() {
			return Ok(None);
		}
		adopted = Some(child_path);
	}
	Ok(adopted.map(|repo_root| ActiveRepoContext {
		relative_repo_root: repo_root
			.strip_prefix(&cwd)
			.expect("direct child is below cwd")
			.to_path_buf(),
		cwd,
		repo_root,
		source: ActiveRepoSource::SingleDirectChildRepo,
	}))
}

fn is_inside_repository(cwd: &Path) -> bool {
	cwd.ancestors()
		.any(|ancestor| is_git_marker(&ancestor.join(".git")))
}

fn is_git_marker(path: &Path) -> bool {
	fs::metadata(path).is_ok_and(|metadata| metadata.is_dir() || metadata.is_file())
}

fn absolute_lexical(path: &Path) -> io::Result<PathBuf> {
	let joined = if path.is_absolute() {
		path.to_path_buf()
	} else {
		env::current_dir()?.join(path)
	};
	let mut normalized = PathBuf::new();
	for component in joined.components() {
		match component {
			Component::CurDir => {},
			Component::ParentDir => {
				normalized.pop();
			},
			other => normalized.push(other.as_os_str()),
		}
	}
	Ok(normalized)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn adopts_exactly_one_direct_child_repo() {
		let tree = tempfile::tempdir().unwrap();
		let repo = tree.path().join("only");
		fs::create_dir_all(repo.join(".git")).unwrap();
		fs::create_dir_all(tree.path().join("plain")).unwrap();
		let context = resolve_active_repo_context(tree.path())
			.unwrap()
			.expect("adopted");
		assert_eq!(context.cwd, tree.path());
		assert_eq!(context.repo_root, repo);
		assert_eq!(context.relative_repo_root, Path::new("only"));
	}

	#[test]
	fn rejects_multiple_children_and_existing_repository() {
		let tree = tempfile::tempdir().unwrap();
		for name in ["a", "b"] {
			fs::create_dir_all(tree.path().join(name).join(".git")).unwrap();
		}
		assert!(resolve_active_repo_context(tree.path()).unwrap().is_none());
		fs::remove_dir_all(tree.path().join("b")).unwrap();
		fs::create_dir(tree.path().join(".git")).unwrap();
		assert!(resolve_active_repo_context(tree.path()).unwrap().is_none());
	}
}
