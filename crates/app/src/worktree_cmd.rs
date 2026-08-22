//! Environment-owned worktree discovery and maintenance commands.

use std::{
	collections::BTreeSet,
	fs, io,
	path::{Path, PathBuf},
	process::{Command, Stdio},
};

use miette::IntoDiagnostic as _;
use serde::{Deserialize, Serialize};

use crate::cli::{WorktreeArgs, WorktreeCommand};

/// Current isolation-owner marker written by workspace operations.
const ISOLATION_OWNER_FILE: &str = ".omp-isolation-owner";
/// pi-compatible isolation-owner marker recognized during cleanup.
const LEGACY_ISOLATION_OWNER_FILE: &str = ".omp-isolation-owner.json";

/// Owner metadata parsed from an isolation marker file.
#[derive(Debug, Deserialize)]
struct IsolationOwner {
	pid: u32,
}

/// Classification facts derived for a directory without a durable record.
struct Classification {
	class:       &'static str,
	orphan:      bool,
	owner_pid:   Option<u32>,
	source_root: Option<PathBuf>,
	branch:      Option<String>,
	parent_repo: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct DurableRecord {
	version:     u8,
	id:          String,
	root:        PathBuf,
	branch:      Option<String>,
	owner_pid:   u32,
	class:       String,
	source_root: PathBuf,
}

/// One classified worktree found in a current or legacy layout.
#[derive(Clone, Debug, Serialize)]
pub struct WorktreeRow {
	/// Stable Environment identity.
	pub id:          String,
	/// Absolute worktree path.
	pub path:        PathBuf,
	/// `pr-checkout`, `task-isolation`, `empty`, or `stray`.
	pub class:       &'static str,
	/// Whether the recorded owner no longer exists.
	pub orphan:      bool,
	/// Recorded owner process, when metadata is valid.
	pub owner_pid:   Option<u32>,
	/// Source workspace, when metadata is valid.
	pub source_root: Option<PathBuf>,
	/// Internal branch disposition, when one was produced.
	pub branch:      Option<String>,
	/// Containing repository for validated PR checkouts.
	#[serde(skip)]
	pub parent_repo: Option<PathBuf>,
	/// Whether a clear operation removed this worktree.
	pub removed:     Option<bool>,
	/// Failure detail when removal or pruning failed.
	pub error:       Option<String>,
	#[serde(skip)]
	record_path:     Option<PathBuf>,
}

/// Resolves the project-specific worktree root used by the Environment.
pub(crate) fn project_worktree_root(state_dir: &Path) -> io::Result<PathBuf> {
	let data_dir = data_dir_from_project_state(state_dir);
	let base = configured_base(&data_dir)?;
	let project_key = state_dir
		.file_name()
		.filter(|name| !name.is_empty())
		.map_or_else(
			|| {
				omp_core::Hash32::sum(state_dir.as_os_str().as_encoded_bytes())
					.to_hex()
					.to_string()
			},
			|name| name.to_string_lossy().into_owned(),
		);
	Ok(base.join(project_key))
}

fn data_dir_from_project_state(state_dir: &Path) -> PathBuf {
	state_dir
		.parent()
		.filter(|parent| parent.file_name().is_some_and(|name| name == "projects"))
		.and_then(Path::parent)
		.map_or_else(|| state_dir.to_path_buf(), Path::to_path_buf)
}

fn configured_base(data_dir: &Path) -> io::Result<PathBuf> {
	if let Some(path) = std::env::var_os("OMP_WORKTREE_DIR").filter(|value| !value.is_empty()) {
		return Ok(PathBuf::from(path));
	}
	Ok(
		match crate::settings::current(data_dir)
			.map_err(io::Error::other)?
			.worktree
			.base
		{
			Some(path) if path.is_absolute() => path,
			Some(path) => data_dir.join(path),
			None => data_dir.join("worktrees"),
		},
	)
}

pub(crate) fn run(data_dir: &Path, args: &WorktreeArgs) -> miette::Result<()> {
	let rows = discover(data_dir).into_diagnostic()?;
	match &args.command {
		WorktreeCommand::List { json, all } => {
			let rows = rows
				.into_iter()
				.filter(|row| *all || row.class != "stray")
				.collect::<Vec<_>>();
			print_rows(&rows, *json).into_diagnostic()
		},
		WorktreeCommand::Clear { all, dry_run, json } => {
			let mut selected = rows
				.into_iter()
				.filter(|row| *all || row.orphan)
				.collect::<Vec<_>>();
			if !dry_run {
				let mut parents_to_prune = BTreeSet::new();
				for row in &mut selected {
					match remove_worktree(row) {
						Ok(parent) => {
							row.removed = Some(true);
							if let Some(parent) = parent {
								parents_to_prune.insert(parent);
							}
						},
						Err(error) => {
							row.removed = Some(false);
							row.error = Some(error.to_string());
						},
					}
				}
				for parent in parents_to_prune {
					if let Err(error) = prune_git_worktrees(&parent) {
						for row in &mut selected {
							if row.parent_repo.as_ref() == Some(&parent) {
								row.removed = Some(false);
								row.error = Some(error.to_string());
							}
						}
					}
				}
			}
			print_rows(&selected, *json).into_diagnostic()
		},
	}
}

fn discover(data_dir: &Path) -> io::Result<Vec<WorktreeRow>> {
	let mut roots = Vec::new();
	let base = configured_base(data_dir)?;
	if base.is_dir() {
		for entry in fs::read_dir(&base)? {
			let entry = entry?;
			if entry.file_type()?.is_dir() {
				roots.push(entry.path());
			}
		}
	}
	let legacy_projects = data_dir.join("projects");
	if legacy_projects.is_dir() {
		for entry in fs::read_dir(legacy_projects)? {
			let legacy = entry?.path().join("workspace-ops");
			if legacy.is_dir() && !roots.contains(&legacy) {
				roots.push(legacy);
			}
		}
	}
	let mut rows = Vec::new();
	for root in roots {
		discover_root(&root, &mut rows)?;
	}
	rows.sort_by(|left, right| left.path.cmp(&right.path));
	Ok(rows)
}

fn discover_root(root: &Path, rows: &mut Vec<WorktreeRow>) -> io::Result<()> {
	let records_dir = root.join(".records");
	if records_dir.is_dir() {
		for entry in fs::read_dir(&records_dir)? {
			let entry = entry?;
			if !entry.file_type()?.is_file() {
				continue;
			}
			let Ok(record) = fs::read(entry.path()).and_then(|bytes| {
				serde_json::from_slice::<DurableRecord>(&bytes)
					.map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
			}) else {
				rows.push(WorktreeRow {
					id:          entry.file_name().to_string_lossy().into_owned(),
					path:        entry.path(),
					class:       "stray",
					orphan:      true,
					owner_pid:   None,
					source_root: None,
					branch:      None,
					parent_repo: None,
					removed:     None,
					error:       None,
					record_path: Some(entry.path()),
				});
				continue;
			};
			let class = if record.version == 1 {
				match record.class.as_str() {
					"pr-checkout" => "pr-checkout",
					"task-isolation" => "task-isolation",
					_ => "stray",
				}
			} else {
				"stray"
			};
			rows.push(WorktreeRow {
				id: record.id,
				path: record.root,
				class,
				orphan: !process_is_live(record.owner_pid),
				owner_pid: Some(record.owner_pid),
				source_root: Some(record.source_root),
				branch: record.branch,
				parent_repo: None,
				removed: None,
				error: None,
				record_path: Some(entry.path()),
			});
		}
	}
	for entry in fs::read_dir(root)? {
		let entry = entry?;
		let name = entry.file_name();
		if name.to_string_lossy().starts_with('.') || !entry.file_type()?.is_dir() {
			continue;
		}
		if rows.iter().any(|row| row.path == entry.path()) {
			continue;
		}
		let classified = classify_unregistered(&entry.path())?;
		rows.push(WorktreeRow {
			id:          name.to_string_lossy().into_owned(),
			path:        entry.path(),
			class:       classified.class,
			orphan:      classified.orphan,
			owner_pid:   classified.owner_pid,
			source_root: classified.source_root,
			branch:      classified.branch,
			parent_repo: classified.parent_repo,
			removed:     None,
			error:       None,
			record_path: None,
		});
	}
	Ok(())
}

fn classify_unregistered(path: &Path) -> io::Result<Classification> {
	if fs::read_dir(path)?.next().is_none() {
		return Ok(Classification {
			class:       "empty",
			orphan:      true,
			owner_pid:   None,
			source_root: None,
			branch:      None,
			parent_repo: None,
		});
	}
	let owner = read_isolation_owner(path);
	let has_mount = ["m", "merged"]
		.into_iter()
		.any(|name| path.join(name).is_dir());
	if owner.is_some() || has_mount {
		let owner_pid = owner.as_ref().map(|owner| owner.pid);
		return Ok(Classification {
			class: "task-isolation",
			orphan: owner_pid.is_none_or(|pid| !process_is_live(pid)),
			owner_pid,
			source_root: None,
			branch: None,
			parent_repo: None,
		});
	}
	if path.join(".git").is_file()
		&& let Some((parent_repo, branch)) = validate_pr_checkout(path)
	{
		return Ok(Classification {
			class:       "pr-checkout",
			orphan:      false,
			owner_pid:   None,
			source_root: None,
			branch:      Some(branch),
			parent_repo: Some(parent_repo),
		});
	}
	Ok(Classification {
		class:       "stray",
		orphan:      true,
		owner_pid:   None,
		source_root: None,
		branch:      None,
		parent_repo: None,
	})
}

fn read_isolation_owner(path: &Path) -> Option<IsolationOwner> {
	[ISOLATION_OWNER_FILE, LEGACY_ISOLATION_OWNER_FILE]
		.into_iter()
		.find_map(|name| {
			fs::read(path.join(name))
				.ok()
				.and_then(|bytes| serde_json::from_slice::<IsolationOwner>(&bytes).ok())
				.filter(|owner| owner.pid != 0)
		})
}

fn validate_pr_checkout(path: &Path) -> Option<(PathBuf, String)> {
	let Ok(pointer) = fs::read_to_string(path.join(".git")) else {
		return None;
	};
	let Some(raw_gitdir) = pointer
		.lines()
		.find_map(|line| line.strip_prefix("gitdir:"))
	else {
		return None;
	};
	let raw_gitdir = raw_gitdir.trim();
	if raw_gitdir.is_empty() {
		return None;
	}
	let gitdir = PathBuf::from(raw_gitdir);
	let gitdir = if gitdir.is_absolute() {
		gitdir
	} else {
		path.join(gitdir)
	};
	let Ok(gitdir) = fs::canonicalize(gitdir) else {
		return None;
	};
	if !gitdir.is_dir() {
		return None;
	}
	let Ok(commondir) = fs::read_to_string(gitdir.join("commondir")) else {
		return None;
	};
	let commondir = commondir.trim();
	if commondir.is_empty() {
		return None;
	}
	let commondir = PathBuf::from(commondir);
	let commondir = if commondir.is_absolute() {
		commondir
	} else {
		gitdir.join(commondir)
	};
	let Ok(commondir) = fs::canonicalize(commondir) else {
		return None;
	};
	if !commondir.is_dir() || commondir.file_name().is_none_or(|name| name != ".git") {
		return None;
	}
	let Some(parent_repo) = commondir.parent() else {
		return None;
	};
	if !parent_repo.is_dir() {
		return None;
	}
	let Ok(head) = fs::read_to_string(gitdir.join("HEAD")) else {
		return None;
	};
	let Some(branch) = head
		.trim()
		.strip_prefix("ref: refs/heads/")
		.filter(|branch| !branch.is_empty())
	else {
		return None;
	};
	Some((parent_repo.to_path_buf(), branch.to_owned()))
}

fn remove_worktree(row: &WorktreeRow) -> io::Result<Option<PathBuf>> {
	let mut parent_to_prune = row.parent_repo.clone();
	if let Some(parent) = &row.parent_repo
		&& row.class == "pr-checkout"
	{
		match run_git(parent, &[
			std::ffi::OsStr::new("worktree"),
			std::ffi::OsStr::new("remove"),
			std::ffi::OsStr::new("--force"),
			row.path.as_os_str(),
		]) {
			Ok(true) => {},
			Ok(false) | Err(_) => {
				remove_path(&row.path, row.record_path.as_deref())?;
				parent_to_prune = Some(parent.clone());
			},
		}
	} else {
		remove_path(&row.path, row.record_path.as_deref())?;
	}
	if let Some(record) = &row.record_path
		&& let Some(container) = record.parent().and_then(Path::parent)
	{
		let branch = container.join(".branches").join(&row.id);
		match fs::remove_file(branch) {
			Ok(()) => {},
			Err(error) if error.kind() == io::ErrorKind::NotFound => {},
			Err(error) => return Err(error),
		}
		prune_empty(&container.join(".branches"))?;
	}
	if let Some(record) = &row.record_path {
		match fs::remove_file(record) {
			Ok(()) => {},
			Err(error) if error.kind() == io::ErrorKind::NotFound => {},
			Err(error) => return Err(error),
		}
		if let Some(parent) = record.parent() {
			prune_empty(parent)?;
		}
	}
	if let Some(parent) = row.path.parent() {
		prune_empty(parent)?;
	}
	Ok(parent_to_prune)
}

fn remove_path(path: &Path, record_path: Option<&Path>) -> io::Result<()> {
	if path.is_dir() {
		fs::remove_dir_all(path)
	} else if path.exists() && record_path != Some(path) {
		fs::remove_file(path)
	} else {
		Ok(())
	}
}

fn prune_git_worktrees(parent: &Path) -> io::Result<()> {
	if run_git(parent, &[std::ffi::OsStr::new("worktree"), std::ffi::OsStr::new("prune")])? {
		Ok(())
	} else {
		Err(io::Error::other("git worktree prune failed"))
	}
}

fn run_git(cwd: &Path, args: &[&std::ffi::OsStr]) -> io::Result<bool> {
	let mut command = Command::new("git");
	command
		.current_dir(cwd)
		.stdin(Stdio::null())
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.env_remove("GIT_DIR")
		.env_remove("GIT_COMMON_DIR")
		.env_remove("GIT_WORK_TREE")
		.env_remove("GIT_INDEX_FILE")
		.env("GIT_TERMINAL_PROMPT", "0")
		.args(["-c", "core.askPass=", "-c", "core.editor=true"])
		.args(args);
	Ok(command.status()?.success())
}

fn prune_empty(path: &Path) -> io::Result<()> {
	if path.is_dir() && fs::read_dir(path)?.next().is_none() {
		fs::remove_dir(path)?;
	}
	Ok(())
}

fn print_rows(rows: &[WorktreeRow], json: bool) -> io::Result<()> {
	use io::Write as _;
	let stdout = io::stdout();
	let mut output = stdout.lock();
	if json {
		serde_json::to_writer_pretty(&mut output, rows).map_err(io::Error::other)?;
		writeln!(output)?;
		return Ok(());
	}
	for row in rows {
		let status = if row.orphan { "orphan" } else { "live" };
		writeln!(output, "{}\t{}\t{}\t{}", row.id, row.class, status, row.path.display())?;
		if let Some(error) = &row.error {
			writeln!(output, "\tfailed: {error}")?;
		}
	}
	Ok(())
}

#[cfg(unix)]
fn process_is_live(pid: u32) -> bool {
	let Ok(pid) = i32::try_from(pid) else {
		return false;
	};
	// SAFETY: signal zero performs only a process-existence/permission probe.
	unsafe {
		libc::kill(pid, 0) == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
	}
}

#[cfg(windows)]
fn process_is_live(pid: u32) -> bool {
	use windows_sys::Win32::{
		Foundation::CloseHandle,
		System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
	};
	// SAFETY: the returned process handle is checked and immediately closed.
	let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
	if handle.is_null() {
		false
	} else {
		// SAFETY: `handle` is a live owned process handle.
		unsafe { CloseHandle(handle) };
		true
	}
}

#[cfg(not(any(unix, windows)))]
fn process_is_live(pid: u32) -> bool {
	pid == std::process::id()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn classifies_empty_pr_and_stray_layouts() {
		let root = tempfile::tempdir().expect("root");
		let empty = root.path().join("empty");
		fs::create_dir(&empty).expect("empty");
		assert_eq!(classify_unregistered(&empty).unwrap().class, "empty");
		let pr = root.path().join("pr-42");
		fs::create_dir(&pr).expect("pr");
		fs::write(pr.join("file"), b"x").expect("file");
		assert_eq!(classify_unregistered(&pr).unwrap().class, "stray");
		let stray = root.path().join("other");
		fs::create_dir(&stray).expect("stray");
		fs::write(stray.join("file"), b"x").expect("file");
		assert_eq!(classify_unregistered(&stray).unwrap().class, "stray");
	}
}
