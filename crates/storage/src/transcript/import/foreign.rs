//! Foreign-session filesystem discovery.

use std::{
	collections::HashMap,
	fs,
	path::{Path, PathBuf},
	time,
};

use omp_core::Str;
use serde_json::Value;

use super::{ForeignFormat, ForeignSessionInfo};

/// Lists Claude Code sessions rooted at `root`.
pub(super) fn list_claude_sessions(root: &Path) -> Vec<ForeignSessionInfo> {
	let mut history = HashMap::<String, (u64, Option<Str>, Option<PathBuf>, u64)>::new();
	if let Ok(input) = fs::read_to_string(root.join("history.jsonl")) {
		for value in input
			.lines()
			.filter_map(|line| serde_json::from_str::<Value>(line).ok())
		{
			let Some(object) = value.as_object() else {
				continue;
			};
			let Some(id) = object
				.get("sessionId")
				.or_else(|| object.get("session_id"))
				.and_then(Value::as_str)
			else {
				continue;
			};
			let timestamp = timestamp(object.get("ts").or_else(|| object.get("timestamp")));
			let text = object
				.get("display")
				.or_else(|| object.get("text"))
				.and_then(Value::as_str)
				.map(preview);
			let cwd = object
				.get("project")
				.and_then(Value::as_str)
				.map(PathBuf::from);
			let entry =
				history
					.entry(id.to_owned())
					.or_insert((timestamp, text.clone(), cwd.clone(), 0));
			entry.0 = entry.0.max(timestamp);
			if entry.1.is_none() {
				entry.1 = text;
			}
			if entry.2.is_none() {
				entry.2 = cwd;
			}
			entry.3 = entry.3.saturating_add(1);
		}
	}
	let registered = registered_projects(root);
	let mut output = Vec::new();
	for container in ["projects", ".projects"] {
		let Ok(projects) = fs::read_dir(root.join(container)) else {
			continue;
		};
		for project in projects
			.flatten()
			.filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
		{
			let encoded = project.file_name().to_string_lossy().into_owned();
			let cwd = registered
				.iter()
				.find(|path| path.to_string_lossy().replace('/', "-") == encoded)
				.cloned()
				.or_else(|| {
					encoded
						.starts_with('-')
						.then(|| PathBuf::from(encoded.replace('-', "/")))
				});
			let Ok(files) = fs::read_dir(project.path()) else {
				continue;
			};
			for file in files
				.flatten()
				.filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
			{
				let path = file.path();
				let Some(id) = path.file_stem().and_then(|id| id.to_str()) else {
					continue;
				};
				let updated = file
					.metadata()
					.ok()
					.and_then(|metadata| metadata.modified().ok())
					.and_then(epoch_millis)
					.unwrap_or_default();
				let (indexed_updated, title, indexed_cwd, count) =
					history.remove(id).unwrap_or((0, None, None, 0));
				output.push(ForeignSessionInfo {
					source: ForeignFormat::ClaudeCode,
					id: Str::new(id),
					path,
					cwd: indexed_cwd.or_else(|| cwd.clone()),
					title,
					updated: updated.max(indexed_updated),
					message_count: (count != 0).then_some(count),
				});
			}
		}
	}
	output
}

/// Lists Codex sessions rooted at `root`.
pub(super) fn list_codex_sessions(root: &Path) -> Vec<ForeignSessionInfo> {
	let mut indexed = sqlite_index(root);
	if indexed.is_empty() {
		indexed = json_index(root);
	}
	let mut paths = HashMap::new();
	for directory in ["sessions", ".sessions", "archived_sessions"] {
		collect_jsonl(&root.join(directory), &mut paths);
	}
	for (id, path) in paths {
		let updated = fs::metadata(&path)
			.ok()
			.and_then(|metadata| metadata.modified().ok())
			.and_then(epoch_millis)
			.unwrap_or_default();
		let mut info = indexed.remove(&id).unwrap_or(ForeignSessionInfo {
			source: ForeignFormat::Codex,
			id: Str::new(&id),
			path: path.clone(),
			cwd: None,
			title: None,
			updated,
			message_count: None,
		});
		info.path = path;
		info.updated = info.updated.max(updated);
		indexed.insert(id, info);
	}
	indexed.into_values().collect()
}

fn sqlite_index(root: &Path) -> HashMap<String, ForeignSessionInfo> {
	let Ok(entries) = fs::read_dir(root) else {
		return HashMap::new();
	};
	let mut databases: Vec<_> = entries
		.flatten()
		.filter(|entry| {
			entry.file_name().to_string_lossy().starts_with("state_")
				&& entry.path().extension().is_some_and(|ext| ext == "sqlite")
		})
		.collect();
	databases.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.file_name()));
	for database in databases {
		let Ok(connection) = rusqlite::Connection::open_with_flags(
			database.path(),
			rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
		) else {
			continue;
		};
		let Ok(mut statement) = connection.prepare(
			"SELECT id, rollout_path, updated_at, cwd, title, first_user_message FROM threads",
		) else {
			continue;
		};
		let Ok(rows) = statement.query_map([], |row| {
			Ok(ForeignSessionInfo {
				source:        ForeignFormat::Codex,
				id:            Str::from(row.get::<_, String>(0)?),
				path:          PathBuf::from(row.get::<_, String>(1)?),
				updated:       row.get::<_, Option<u64>>(2)?.unwrap_or_default(),
				cwd:           row.get::<_, Option<String>>(3)?.map(PathBuf::from),
				title:         row
					.get::<_, Option<String>>(4)?
					.or(row.get::<_, Option<String>>(5)?)
					.map(Str::from),
				message_count: None,
			})
		}) else {
			continue;
		};
		return rows
			.flatten()
			.map(|mut info| {
				if info.path.is_relative() {
					info.path = root.join(&info.path);
				}
				(info.id.to_string(), info)
			})
			.collect();
	}
	HashMap::new()
}

fn json_index(root: &Path) -> HashMap<String, ForeignSessionInfo> {
	let Ok(input) = fs::read_to_string(root.join("session_index.jsonl")) else {
		return HashMap::new();
	};
	input
		.lines()
		.filter_map(|line| serde_json::from_str::<Value>(line).ok())
		.filter_map(|value| {
			let object = value.as_object()?;
			let id = object.get("id")?.as_str()?;
			Some((id.to_owned(), ForeignSessionInfo {
				source:        ForeignFormat::Codex,
				id:            Str::new(id),
				path:          PathBuf::new(),
				cwd:           None,
				title:         object
					.get("thread_name")
					.and_then(Value::as_str)
					.map(Str::new),
				updated:       timestamp(object.get("updated_at")),
				message_count: None,
			}))
		})
		.collect()
}

fn registered_projects(root: &Path) -> Vec<PathBuf> {
	fs::read_to_string(root.parent().unwrap_or(root).join(".claude.json"))
		.ok()
		.and_then(|input| serde_json::from_str::<Value>(&input).ok())
		.and_then(|value| value.get("projects")?.as_object().cloned())
		.map_or_else(Vec::new, |projects| {
			projects
				.into_iter()
				.map(|(key, _)| PathBuf::from(key))
				.filter(|path| path.is_absolute())
				.collect()
		})
}

fn collect_jsonl(directory: &Path, paths: &mut HashMap<String, PathBuf>) {
	let Ok(entries) = fs::read_dir(directory) else {
		return;
	};
	for entry in entries.flatten() {
		let path = entry.path();
		if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
			collect_jsonl(&path, paths);
		} else if path.extension().is_some_and(|ext| ext == "jsonl") {
			if let Some(id) = path.file_stem().and_then(|id| id.to_str()) {
				paths.insert(id.to_owned(), path);
			}
		}
	}
}

fn timestamp(value: Option<&Value>) -> u64 {
	match value {
		Some(Value::Number(number)) => number.as_u64().unwrap_or_default(),
		Some(Value::String(value)) => omp_core::time::parse_rfc3339(value)
			.and_then(|time| time.duration_since(time::UNIX_EPOCH).ok())
			.and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
			.unwrap_or_default(),
		_ => 0,
	}
}
fn epoch_millis(time: time::SystemTime) -> Option<u64> {
	time
		.duration_since(time::UNIX_EPOCH)
		.ok()
		.and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
}
fn preview(value: &str) -> Str {
	Str::from(value.split_whitespace().collect::<Vec<_>>().join(" "))
}
