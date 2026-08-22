//! One-way legacy settings import into native TOML.

use std::{
	fs, io,
	path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::io::{SettingsIoError, atomic_replace};

const MARKER: &str = ".settings-migration-v1";
const RECORD: &str = "settings-migration.toml";

/// Stable migration action vocabulary.
#[derive(
	Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, strum::Display, strum::EnumString,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
pub enum MigrationAction {
	/// A representable value was converted.
	Converted,
	/// An obsolete/unsupported value was removed.
	Dropped,
	/// Credential bytes were refused because no combined import API existed.
	CredentialRejected,
}

/// One secret-free migration decision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MigrationEntry {
	/// Legacy dotted path; never the value.
	pub path:   String,
	/// Stable action.
	pub action: MigrationAction,
	/// Human-readable, value-free rationale.
	pub reason: String,
}

/// Durable record of unsupported, dropped, and converted legacy keys.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct MigrationRecord {
	/// Migration format revision.
	pub revision: u32,
	/// Source labels that participated in the import.
	pub sources:  Vec<String>,
	/// Value-free decisions.
	pub entries:  Vec<MigrationEntry>,
}

/// Result of attempting the one-time migration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MigrationOutcome {
	/// The durable marker already existed.
	AlreadyCompleted,
	/// Migration completed and wrote the marker.
	Completed(MigrationRecord),
}

/// Imports `settings.json`/JSONC and the legacy `agent.db` settings table.
pub fn migrate_legacy_settings(data_dir: &Path) -> Result<MigrationOutcome, MigrationError> {
	fs::create_dir_all(data_dir)
		.map_err(|source| MigrationError::CreateDirectory { path: data_dir.to_owned(), source })?;
	let marker = data_dir.join(MARKER);
	if marker.exists() {
		return Ok(MigrationOutcome::AlreadyCompleted);
	}

	let mut record = MigrationRecord { revision: 1, ..MigrationRecord::default() };
	let mut document = toml::Table::new();
	let settings_json = data_dir.join("settings.json");
	if settings_json.exists() {
		let source = fs::read_to_string(&settings_json)
			.map_err(|source| MigrationError::Read { path: settings_json.clone(), source })?;
		let value: serde_json::Value = omp_slopjson::from_str(&source)?;
		let table = json_table(value)?;
		crate::deep_merge(&mut document, table);
		record.sources.push("settings.json".to_owned());
		backup_file(&settings_json)?;
	}

	let database = data_dir.join("agent.db");
	if database.exists() {
		if let Some(table) = read_database_settings(&database)? {
			crate::deep_merge(&mut document, table);
			record.sources.push("agent.db:settings".to_owned());
			backup_file(&database)?;
		}
	}

	let changelog_version = convert_legacy(&mut document, &mut record);
	remove_unsupported(&mut document, &mut record);
	reject_credentials(&mut document, &mut record, "");

	let config = data_dir.join("config.toml");
	if !document.is_empty() {
		let mut current = match fs::read_to_string(&config) {
			Ok(source) => toml::from_str::<toml::Table>(&source)
				.map_err(|source| MigrationError::ExistingConfig { path: config.clone(), source })?,
			Err(error) if error.kind() == io::ErrorKind::NotFound => toml::Table::new(),
			Err(source) => return Err(MigrationError::Read { path: config.clone(), source }),
		};
		// Native values already chosen by the user win over imported legacy data.
		let mut imported = document;
		crate::deep_merge(&mut imported, current);
		current = imported;
		atomic_replace(&config, &toml::to_string_pretty(&current)?)?;
	}
	if let Some(version) = changelog_version {
		atomic_replace(&data_dir.join("last-changelog-version"), &version)?;
	}
	atomic_replace(&data_dir.join(RECORD), &toml::to_string_pretty(&record)?)?;
	atomic_replace(&marker, "revision = 1\n")?;
	Ok(MigrationOutcome::Completed(record))
}

fn json_table(value: serde_json::Value) -> Result<toml::Table, MigrationError> {
	let serde_json::Value::Object(object) = value else {
		return Err(MigrationError::JsonRootNotObject);
	};
	Ok(object
		.into_iter()
		.filter_map(|(key, value)| json_value(value).map(|value| (key, value)))
		.collect())
}

fn json_value(value: serde_json::Value) -> Option<toml::Value> {
	match value {
		serde_json::Value::Null => None,
		serde_json::Value::Bool(value) => Some(toml::Value::Boolean(value)),
		serde_json::Value::Number(value) => value
			.as_i64()
			.map(toml::Value::Integer)
			.or_else(|| {
				value
					.as_u64()
					.and_then(|value| i64::try_from(value).ok())
					.map(toml::Value::Integer)
			})
			.or_else(|| value.as_f64().map(toml::Value::Float)),
		serde_json::Value::String(value) => Some(toml::Value::String(value)),
		serde_json::Value::Array(values) => {
			Some(toml::Value::Array(values.into_iter().filter_map(json_value).collect()))
		},
		serde_json::Value::Object(values) => Some(toml::Value::Table(
			values
				.into_iter()
				.filter_map(|(key, value)| json_value(value).map(|value| (key, value)))
				.collect(),
		)),
	}
}

fn read_database_settings(path: &Path) -> Result<Option<toml::Table>, MigrationError> {
	let connection =
		rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
	let exists: bool = connection.query_row(
		"SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='settings')",
		[],
		|row| row.get(0),
	)?;
	if !exists {
		return Ok(None);
	}
	let columns = connection
		.prepare("PRAGMA table_info(settings)")?
		.query_map([], |row| row.get::<_, String>(1))?
		.collect::<Result<Vec<_>, _>>()?;
	if columns.iter().any(|column| column == "key") && columns.iter().any(|column| column == "value")
	{
		let mut statement = connection.prepare("SELECT key, value FROM settings ORDER BY key")?;
		let rows =
			statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
		let mut table = toml::Table::new();
		for row in rows {
			let (key, encoded) = row?;
			let value: serde_json::Value = serde_json::from_str(&encoded)
				.map_err(|source| MigrationError::DatabaseValue { key: key.clone(), source })?;
			if let Some(value) = json_value(value) {
				set_dotted(&mut table, &key, value);
			}
		}
		return Ok(Some(table));
	}
	if columns.iter().any(|column| column == "data") {
		let encoded = connection
			.query_row("SELECT data FROM settings WHERE id = 1", [], |row| row.get::<_, String>(0))
			.optional()?;
		return encoded
			.map(|encoded| {
				let value = serde_json::from_str(&encoded).map_err(|source| {
					MigrationError::DatabaseValue { key: "data".to_owned(), source }
				})?;
				json_table(value)
			})
			.transpose();
	}
	Ok(None)
}

fn backup_file(path: &Path) -> Result<(), MigrationError> {
	let backup = path.with_file_name(format!(
		"{}.pre-omp-migration.bak",
		path
			.file_name()
			.and_then(|name| name.to_str())
			.unwrap_or("legacy")
	));
	if !backup.exists() {
		fs::copy(path, &backup).map_err(|source| MigrationError::Backup {
			path: path.to_owned(),
			backup,
			source,
		})?;
	}
	Ok(())
}

fn convert_legacy(document: &mut toml::Table, record: &mut MigrationRecord) -> Option<String> {
	move_key(document, "queueMode", "steering_mode", record);
	move_key(document, "defaultModel", "default_model", record);
	move_key(document, "worktreeDir", "worktree.base", record);
	for (old, new) in [
		("async.maxJobs", "async.max_jobs"),
		("async.pollWaitDuration", "async.poll_wait_duration"),
		("bash.enabled", "shell.enabled"),
		("bash.autoBackground.enabled", "shell.auto_background.enabled"),
		("bash.autoBackground.thresholdMs", "shell.auto_background.threshold_ms"),
		("bash.direnv", "shell.direnv"),
		("bash.direnvLoadTimeoutMs", "shell.direnv_load_timeout_ms"),
		("bashInterceptor.enabled", "shell.interceptor.enabled"),
		("bashInterceptor.patterns", "shell.interceptor.patterns"),
		("shellMinimizer.enabled", "shell.minimizer.enabled"),
		("shellMinimizer.settingsPath", "shell.minimizer.settings_path"),
		("shellMinimizer.only", "shell.minimizer.only"),
		("shellMinimizer.except", "shell.minimizer.except"),
		("shellMinimizer.maxCaptureBytes", "shell.minimizer.max_capture_bytes"),
		("shellMinimizer.sourceOutlineLevel", "shell.minimizer.source_outline_level"),
		("shellMinimizer.legacyFilters", "shell.minimizer.legacy_filters"),
		("shellPath", "shell.executable"),
	] {
		move_key(document, old, new, record);
	}

	if let Some(value) = take_path(document, "collapseChangelog") {
		if let toml::Value::Boolean(collapsed) = value {
			set_dotted(
				document,
				"startup.changelog_mode",
				toml::Value::String(if collapsed { "summary" } else { "expanded" }.to_owned()),
			);
			converted(record, "collapseChangelog", "startup.changelog_mode");
		}
	}
	let changelog_version = take_path(document, "lastChangelogVersion")
		.and_then(|value| value.as_str().map(str::to_owned));
	if changelog_version.is_some() {
		converted(record, "lastChangelogVersion", "last-changelog-version state marker");
	}
	if let Some(toml::Value::String(theme)) = take_path(document, "theme") {
		if theme != "light" && theme != "dark" {
			set_dotted(document, "appearance.theme.dark", toml::Value::String(theme));
		}
		converted(record, "theme", "appearance.theme");
	}
	for (old, new, on, off) in [
		("inspect_image.enabled", "inspect_image.mode", "on", "off"),
		("task.eager", "task.eager", "always", "default"),
		("todo.eager", "todo.eager", "always", "default"),
		("snapcompact.systemPrompt", "snapcompact.system_prompt", "all", "none"),
		("inlineToolDescriptors", "inline_tool_descriptors", "on", "off"),
		("codexResets.autoRedeem", "codex_resets.auto_redeem", "yes", "no"),
	] {
		if let Some(toml::Value::Boolean(enabled)) = take_path(document, old) {
			set_dotted(document, new, toml::Value::String(if enabled { on } else { off }.to_owned()));
			converted(record, old, new);
		}
	}
	if value_at_mut(document, "power.sleep_prevention").is_none() {
		let flags = [
			("power.preventIdleSleep", "idle"),
			("power.preventDisplaySleep", "display"),
			("power.declareUserActive", "system"),
			("power.preventSystemSleep", "system"),
		];
		let mut selected = None;
		let mut any_set = false;
		for (path, mode) in flags {
			if let Some(toml::Value::Boolean(enabled)) = take_path(document, path) {
				any_set = true;
				if enabled {
					selected = Some(mode);
				}
			}
		}
		if any_set {
			let mode = selected.unwrap_or("off");
			set_dotted(document, "power.sleep_prevention", toml::Value::String(mode.to_owned()));
			converted(record, "power.*Sleep", "power.sleep_prevention");
		}
	}
	if let Some(toml::Value::Boolean(enabled)) = take_path(document, "task.isolation.enabled") {
		set_dotted(
			document,
			"task.isolation.mode",
			toml::Value::String(if enabled { "auto" } else { "none" }.to_owned()),
		);
		converted(record, "task.isolation.enabled", "task.isolation.mode");
	}
	if let Some(toml::Value::String(mode)) =
		value_at_mut(document, "task.isolation.mode").map(|value| value.clone())
	{
		let replacement = match mode.as_str() {
			"worktree" => Some("rcopy"),
			"fuse-overlay" => Some("overlayfs"),
			"fuse-projfs" => Some("projfs"),
			_ => None,
		};
		if let Some(replacement) = replacement {
			set_dotted(document, "task.isolation.mode", toml::Value::String(replacement.to_owned()));
			converted(record, "task.isolation.mode", "task.isolation.mode");
		}
	}
	if let Some(toml::Value::String(mode)) = take_path(document, "edit.mode")
		&& (mode == "atom" || mode == "vim")
	{
		set_dotted(document, "tools.edit_dialect", toml::Value::String("hashline".to_owned()));
		converted(record, "edit.mode", "tools.edit_dialect");
	}
	if take_path(document, "edit.modelVariants").is_some() {
		dropped(record, "edit.modelVariants", "model-specific edit variants are unsupported");
	}
	if let Some(toml::Value::String(strategy)) = take_path(document, "compaction.strategy") {
		let order = match strategy.as_str() {
			"off" => Vec::new(),
			"context-full" => vec!["remote", "soft"],
			"handoff" => vec!["handoff", "remote", "soft"],
			"shake" | "shake-summary" => vec!["shake", "remote", "soft"],
			"snapcompact" => vec!["snapcompact", "remote", "soft"],
			_ => Vec::new(),
		};
		if !order.is_empty() || strategy == "off" {
			set_dotted(
				document,
				"compaction.method_order",
				toml::Value::Array(
					order
						.into_iter()
						.map(|item| toml::Value::String(item.to_owned()))
						.collect(),
				),
			);
			converted(record, "compaction.strategy", "compaction.method_order");
		}
	}
	if let Some(toml::Value::Integer(timeout)) =
		value_at_mut(document, "ask.timeout").map(|value| value.clone())
		&& timeout > 1000
	{
		set_dotted(document, "ask.timeout", toml::Value::Integer((timeout + 500) / 1000));
		converted(record, "ask.timeout", "ask.timeout (milliseconds to seconds)");
	}
	for (old, new) in [
		("providers.webSearch", "providers.web_search_order"),
		("providers.image", "providers.image_order"),
	] {
		if let Some(toml::Value::String(provider)) = take_path(document, old)
			&& provider != "auto"
		{
			let order: &[&str] = if old == "providers.webSearch" {
				&[
					"perplexity",
					"gemini",
					"anthropic",
					"codex",
					"xai",
					"zai",
					"exa",
					"tinyfish",
					"jina",
					"kagi",
					"tavily",
					"firecrawl",
					"brave",
					"kimi",
					"parallel",
					"synthetic",
					"searxng",
					"startpage",
					"duckduckgo",
					"ecosia",
					"google",
					"mojeek",
					"public",
				]
			} else {
				&["openai", "openai-codex", "antigravity", "xai", "openrouter", "gemini"]
			};
			if order.contains(&provider.as_str()) {
				let values = std::iter::once(provider.as_str())
					.chain(
						order
							.iter()
							.copied()
							.filter(|candidate| *candidate != provider.as_str()),
					)
					.map(|value| toml::Value::String(value.to_owned()))
					.collect();
				set_dotted(document, new, toml::Value::Array(values));
				converted(record, old, new);
			}
		}
	}
	for (old, new) in [("find", "glob"), ("search", "grep"), ("mnemosyne", "mnemopi")] {
		if value_at_mut(document, new).is_none()
			&& let Some(value) = take_path(document, old)
		{
			set_dotted(document, new, value);
			converted(record, old, new);
		}
	}
	if matches!(
		value_at_mut(document, "memory.backend").and_then(|value| value.as_str()),
		Some("mnemosyne")
	) {
		set_dotted(document, "memory.backend", toml::Value::String("mnemopi".to_owned()));
		converted(record, "memory.backend=mnemosyne", "memory.backend=mnemopi");
	}
	if let Some(enabled) = take_path(document, "memories.enabled") {
		match enabled.as_bool() {
			Some(false) if value_at_mut(document, "memory.backend").is_none() => {
				set_dotted(document, "memory.backend", toml::Value::String("off".to_owned()));
				converted(record, "memories.enabled=false", "memory.backend=off");
			},
			Some(false) => {
				dropped(record, "memories.enabled", "native memory.backend takes precedence");
			},
			Some(true) => {
				dropped(
					record,
					"memories.enabled",
					"legacy local memory is unsupported; memory remains off unless mnemopi is explicit",
				);
			},
			None => {
				dropped(record, "memories.enabled", "invalid legacy memory toggle");
			},
		}
	}
	if let Some(toml::Value::Table(mut exa)) = take_path(document, "exa") {
		let flags = [exa.remove("enabled"), exa.remove("enableSearch")]
			.into_iter()
			.flatten()
			.filter_map(|value| value.as_bool())
			.collect::<Vec<_>>();
		exa.remove("enableResearcher");
		exa.remove("enableWebsets");
		if !flags.is_empty() {
			exa.insert("enabled".to_owned(), toml::Value::Boolean(flags.into_iter().all(|flag| flag)));
		}
		if !exa.is_empty() {
			document.insert("exa".to_owned(), toml::Value::Table(exa));
		}
		converted(record, "exa legacy toggles", "exa.enabled");
	}
	changelog_version
}

fn remove_unsupported(document: &mut toml::Table, record: &mut MigrationRecord) {
	for path in [
		"bm25",
		"task.simple",
		"computer.backend",
		"read.model",
		"readHashLines",
		"read.hashLines",
		"providers.parallelFetch",
		"providers.parallel_fetch",
		"lsp.shared",
		"bash",
		"bashInterceptor",
		"shellMinimizer",
	] {
		if take_path(document, path).is_some() {
			dropped(record, path, "retired setting");
		}
	}
	for path in [
		"memories",
		"hindsight",
		"localMemory",
		"local_memory",
		"mentalModel",
		"mental_model",
		"commit",
		"claude",
		"codex",
		"gemini",
		"foreignSource",
		"foreign_source",
	] {
		if take_path(document, path).is_some() {
			dropped(record, path, "unsupported or dropped OMP scope");
		}
	}
	if matches!(
		value_at_mut(document, "memory.backend").and_then(|value| value.as_str()),
		Some("local" | "local-lite" | "hindsight")
	) {
		set_dotted(document, "memory.backend", toml::Value::String("off".to_owned()));
		dropped(record, "memory.backend", "unsupported memory backend; reset to off");
	}
}

fn reject_credentials(table: &mut toml::Table, record: &mut MigrationRecord, prefix: &str) {
	let keys = table.keys().cloned().collect::<Vec<_>>();
	for key in keys {
		let path = if prefix.is_empty() {
			key.clone()
		} else {
			format!("{prefix}.{key}")
		};
		let normalized = key.to_ascii_lowercase().replace(['-', '_'], "");
		if normalized.contains("apikey")
			|| normalized.contains("accesstoken")
			|| normalized.contains("refreshtoken")
			|| normalized == "token"
			|| normalized == "secret"
		{
			table.remove(&key);
			record.entries.push(MigrationEntry {
				path,
				action: MigrationAction::CredentialRejected,
				reason: "combined provider/MCP token import API unavailable".to_owned(),
			});
		} else if let Some(child) = table.get_mut(&key).and_then(toml::Value::as_table_mut) {
			reject_credentials(child, record, &path);
		}
	}
}

fn converted(record: &mut MigrationRecord, old: &str, new: &str) {
	record.entries.push(MigrationEntry {
		path:   old.to_owned(),
		action: MigrationAction::Converted,
		reason: format!("moved to {new}"),
	});
}

fn dropped(record: &mut MigrationRecord, path: &str, reason: &str) {
	record.entries.push(MigrationEntry {
		path:   path.to_owned(),
		action: MigrationAction::Dropped,
		reason: reason.to_owned(),
	});
}

fn move_key(document: &mut toml::Table, old: &str, new: &str, record: &mut MigrationRecord) {
	if value_at_mut(document, new).is_none()
		&& let Some(value) = take_path(document, old)
	{
		set_dotted(document, new, value);
		converted(record, old, new);
	}
}

fn set_dotted(document: &mut toml::Table, path: &str, value: toml::Value) {
	let mut segments = path.split('.').peekable();
	let mut table = document;
	while let Some(segment) = segments.next() {
		if segments.peek().is_none() {
			table.insert(segment.to_owned(), value);
			return;
		}
		let entry = table
			.entry(segment.to_owned())
			.or_insert_with(|| toml::Value::Table(toml::Table::new()));
		if !entry.is_table() {
			*entry = toml::Value::Table(toml::Table::new());
		}
		table = entry.as_table_mut().expect("table established above");
	}
}

fn take_path(document: &mut toml::Table, path: &str) -> Option<toml::Value> {
	let mut segments = path.split('.').peekable();
	let mut table = document;
	while let Some(segment) = segments.next() {
		if segments.peek().is_none() {
			return table.remove(segment);
		}
		table = table.get_mut(segment)?.as_table_mut()?;
	}
	None
}

fn value_at_mut<'a>(document: &'a mut toml::Table, path: &str) -> Option<&'a mut toml::Value> {
	let mut segments = path.split('.').peekable();
	let mut table = document;
	while let Some(segment) = segments.next() {
		if segments.peek().is_none() {
			return table.get_mut(segment);
		}
		table = table.get_mut(segment)?.as_table_mut()?;
	}
	None
}

use rusqlite::OptionalExtension as _;

/// Legacy settings migration failure.
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
	/// Migration directory creation failed.
	#[error("failed to create migration directory {path}")]
	CreateDirectory {
		path:   PathBuf,
		#[source]
		source: io::Error,
	},
	/// A legacy source could not be read.
	#[error("failed to read legacy settings source {path}")]
	Read {
		path:   PathBuf,
		#[source]
		source: io::Error,
	},
	/// A source backup could not be created.
	#[error("failed to back up legacy settings source {path} to {backup}")]
	Backup {
		path:   PathBuf,
		backup: PathBuf,
		#[source]
		source: io::Error,
	},
	/// JSONC parsing failed.
	#[error(transparent)]
	Jsonc(#[from] omp_slopjson::ParseError),
	/// A legacy JSON root was not an object.
	#[error("legacy settings JSON root must be an object")]
	JsonRootNotObject,
	/// Existing native configuration was invalid and cannot safely be merged.
	#[error("existing native settings file {path} is invalid")]
	ExistingConfig {
		path:   PathBuf,
		#[source]
		source: toml::de::Error,
	},
	/// Legacy database access failed.
	#[error(transparent)]
	Database(#[from] rusqlite::Error),
	/// One database setting did not contain valid JSON.
	#[error("legacy database setting {key} is invalid JSON")]
	DatabaseValue {
		key:    String,
		#[source]
		source: serde_json::Error,
	},
	/// Native TOML encoding failed.
	#[error(transparent)]
	Encode(#[from] toml::ser::Error),
	/// Atomic persistence failed.
	#[error(transparent)]
	Io(#[from] SettingsIoError),
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn migration_is_recorded_secret_free_and_idempotent() {
		let directory = tempfile::tempdir().expect("directory");
		fs::write(
			directory.path().join("settings.json"),
			"{ // legacy\n defaultModel: 'demo/model', collapseChangelog: false, apiKey: \
			 'never-report', hindsight: { enabled: true }, }",
		)
		.expect("legacy");
		let first = migrate_legacy_settings(directory.path()).expect("migrate");
		let MigrationOutcome::Completed(record) = first else {
			panic!("first migration")
		};
		assert!(
			record
				.entries
				.iter()
				.any(|entry| entry.action == MigrationAction::CredentialRejected)
		);
		assert!(record.entries.iter().any(|entry| entry.path == "hindsight"));
		let report = fs::read_to_string(directory.path().join(RECORD)).expect("record");
		assert!(!report.contains("never-report"));
		let config = fs::read_to_string(directory.path().join("config.toml")).expect("config");
		assert!(config.contains("default_model = \"demo/model\""));
		assert!(!config.contains("apiKey"));
		assert_eq!(
			migrate_legacy_settings(directory.path()).expect("idempotent"),
			MigrationOutcome::AlreadyCompleted,
		);
	}
}
