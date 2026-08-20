//! Native OMP filesystem discovery with explicit, bounded ancestor walks.

use std::{
	env,
	path::{Path, PathBuf},
};

use omp_walker::WalkRequest;

/// A configuration root with explicit compatibility precedence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigRoot {
	/// Root directory containing the configuration.
	pub path:     PathBuf,
	/// `true` for a user-home root, `false` for a project root.
	pub user:     bool,
	/// Larger precedence wins; native OMP is always highest.
	pub priority: u8,
}

/// Returns existing configuration roots in precedence order: native OMP,
/// Claude, Codex, then Gemini, with project roots ahead of user roots inside a
/// family. The result is data for config/model overlay loading, not an implicit
/// parser or global registry.
pub fn config_roots(cwd: &Path, home: &Path, max_depth: usize) -> Vec<ConfigRoot> {
	let mut roots = Vec::new();
	for (name, priority) in [(".omp", 4), (".claude", 3), (".codex", 2), (".gemini", 1)] {
		let mut current = cwd;
		for _ in 0..=max_depth {
			let path = current.join(name);
			if path.is_dir() {
				roots.push(ConfigRoot { path, user: false, priority });
			}
			let Some(parent) = current.parent() else {
				break;
			};
			if parent == current || current == home {
				break;
			}
			current = parent;
		}
		let user = if name == ".omp" {
			user_config_root(home)
		} else {
			home.join(name)
		};
		if user.is_dir() {
			roots.push(ConfigRoot { path: user, user: true, priority });
		}
	}
	roots.sort_by(|left, right| {
		right
			.priority
			.cmp(&left.priority)
			.then_with(|| left.user.cmp(&right.user))
	});
	roots
}

/// Native configuration roots ordered from highest to lowest precedence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeRoots {
	/// Profile-scoped user agent directory.
	pub user:    PathBuf,
	/// Nearest-first `.omp` directories between the cwd and filesystem root.
	pub project: Vec<PathBuf>,
	/// Nearest-first standalone instruction files.
	pub agents:  Vec<PathBuf>,
}

/// Resolves the native user config root. `OMP_PROFILE` scopes profiles without
/// changing the project `.omp` convention.
pub fn user_config_root(home: &Path) -> PathBuf {
	match env::var("OMP_PROFILE")
		.ok()
		.filter(|profile| !profile.is_empty())
	{
		Some(profile) => home.join(".omp/profiles").join(profile).join("agent"),
		None => home.join(".omp/agent"),
	}
}

/// Collects native `.omp` and standalone `AGENTS.md` walk-ups. The cap is an
/// I/O bound as well as a cycle guard for malformed synthetic test paths.
pub fn discover_roots(cwd: &Path, home: &Path, max_depth: usize) -> NativeRoots {
	let mut project = Vec::new();
	let mut agents = Vec::new();
	let mut current = cwd;
	for _ in 0..=max_depth {
		let omp = current.join(".omp");
		if omp.is_dir() {
			project.push(omp);
		}
		let agents_file = current.join("AGENTS.md");
		if agents_file.is_file() {
			agents.push(agents_file);
		}
		if current == home {
			break;
		}
		let Some(parent) = current.parent() else {
			break;
		};
		if parent == current {
			break;
		}
		current = parent;
	}
	NativeRoots { user: user_config_root(home), project, agents }
}

/// Scans one capability directory without recursive imports, hidden entries,
/// or ignored files. `omp-walker` owns full gitignore semantics.
pub fn scan_capability_dir(root: &Path) -> Vec<PathBuf> {
	WalkRequest::new(root)
		.hidden(false)
		.gitignore(true)
		.skip_git(true)
		.depth(1, 1)
		.collect_files()
		.unwrap_or_default()
		.into_iter()
		.map(|entry| entry.absolute_path(root))
		.collect()
}

#[cfg(test)]
mod tests {
	use std::fs;

	use super::*;
	#[test]
	fn compatibility_roots_follow_family_precedence() {
		let tree = tempfile::tempdir().expect("tree");
		let home = tree.path().join("home");
		let cwd = tree.path().join("repo/work");
		fs::create_dir_all(cwd.join(".claude")).expect("project");
		fs::create_dir_all(home.join(".omp/agent")).expect("user");
		let roots = config_roots(&cwd, &home, 3);
		assert_eq!(roots[0].path, home.join(".omp/agent"));
		assert_eq!(roots[0].priority, 4);
		assert_eq!(roots[1].path, cwd.join(".claude"));
		assert_eq!(roots[1].priority, 3);
	}
	#[test]
	fn walkups_are_nearest_first_and_depth_bounded() {
		let tree = tempfile::tempdir().expect("tree");
		let root = tree.path();
		let cwd = root.join("a/b/c");
		fs::create_dir_all(cwd.join(".omp")).expect("nested");
		fs::create_dir_all(root.join("a/.omp")).expect("parent");
		fs::write(root.join("a/AGENTS.md"), "parent").expect("agents");
		let roots = discover_roots(&cwd, root, 2);
		assert_eq!(roots.project, vec![cwd.join(".omp"), root.join("a/.omp")]);
		assert_eq!(roots.agents, vec![root.join("a/AGENTS.md")]);
	}
	#[test]
	fn scan_respects_gitignore_and_is_non_recursive() {
		let tree = tempfile::tempdir().expect("tree");
		let root = tree.path();
		fs::write(root.join(".gitignore"), "ignored.md\n").expect("ignore");
		fs::write(root.join("kept.md"), "x").expect("kept");
		fs::write(root.join("ignored.md"), "x").expect("ignored");
		fs::create_dir(root.join("nested")).expect("nested");
		fs::write(root.join("nested/child.md"), "x").expect("child");
		assert_eq!(scan_capability_dir(root), vec![root.join("kept.md")]);
	}
}
