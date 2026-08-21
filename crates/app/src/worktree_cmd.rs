//! Environment-owned worktree discovery and maintenance commands.

use std::{
	fs, io,
	path::{Path, PathBuf},
};

use miette::IntoDiagnostic as _;
use serde::{Deserialize, Serialize};

use crate::cli::{WorktreeArgs, WorktreeCommand};

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
	#[serde(skip)]
	record_path:     Option<PathBuf>,
}

/// Resolves the project-specific worktree root used by the Environment.
pub(crate) fn project_worktree_root(state_dir: &Path) -> io::Result<PathBuf> {
	let data_dir = data_dir_from_project_state(state_dir);
	let base = configured_base(&data_dir);
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

fn configured_base(data_dir: &Path) -> PathBuf {
	if let Some(path) = std::env::var_os("OMP_WORKTREE_DIR").filter(|value| !value.is_empty()) {
		return PathBuf::from(path);
	}
	match crate::settings::Settings::load(data_dir).worktree.base {
		Some(path) if path.is_absolute() => path,
		Some(path) => data_dir.join(path),
		None => data_dir.join("worktrees"),
	}
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
			let selected = rows
				.into_iter()
				.filter(|row| *all || row.orphan)
				.collect::<Vec<_>>();
			if !dry_run {
				for row in &selected {
					remove_worktree(row).into_diagnostic()?;
				}
			}
			print_rows(&selected, *json).into_diagnostic()
		},
	}
}

fn discover(data_dir: &Path) -> io::Result<Vec<WorktreeRow>> {
	let mut roots = Vec::new();
	let base = configured_base(data_dir);
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
		let class = classify_unregistered(&entry.path())?;
		rows.push(WorktreeRow {
			id: name.to_string_lossy().into_owned(),
			path: entry.path(),
			class,
			orphan: true,
			owner_pid: None,
			source_root: None,
			branch: None,
			record_path: None,
		});
	}
	Ok(())
}

fn classify_unregistered(path: &Path) -> io::Result<&'static str> {
	if fs::read_dir(path)?.next().is_none() {
		return Ok("empty");
	}
	if path
		.file_name()
		.and_then(|name| name.to_str())
		.and_then(|name| name.rsplit_once('-').map(|(_, suffix)| suffix))
		.is_some_and(|suffix| omp_core::Ulid::from_string(suffix).is_ok())
	{
		return Ok("task-isolation");
	}
	if path.join(".git").is_file()
		|| path
			.file_name()
			.is_some_and(|name| name.to_string_lossy().starts_with("pr-"))
	{
		Ok("pr-checkout")
	} else {
		Ok("stray")
	}
}

fn remove_worktree(row: &WorktreeRow) -> io::Result<()> {
	if row.path.is_dir() {
		fs::remove_dir_all(&row.path)?;
	} else if row.path.exists() && row.record_path.as_ref() != Some(&row.path) {
		fs::remove_file(&row.path)?;
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
	Ok(())
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
		assert_eq!(classify_unregistered(&empty).unwrap(), "empty");
		let pr = root.path().join("pr-42");
		fs::create_dir(&pr).expect("pr");
		fs::write(pr.join("file"), b"x").expect("file");
		assert_eq!(classify_unregistered(&pr).unwrap(), "pr-checkout");
		let stray = root.path().join("other");
		fs::create_dir(&stray).expect("stray");
		fs::write(stray.join("file"), b"x").expect("file");
		assert_eq!(classify_unregistered(&stray).unwrap(), "stray");
	}
}
