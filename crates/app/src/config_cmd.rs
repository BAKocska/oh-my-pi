//! Reflected, typed settings command handlers.

use std::path::{Path, PathBuf};

use miette::IntoDiagnostic as _;
use omp_settings::FieldDescriptor;

use crate::{
	cli::{ConfigCommand, ConfigScope, McpConfigCommand, McpConfigScope},
	envd::mcp::{
		config::McpServerConfig,
		config_store::{McpConfigStore, set_server_enabled},
	},
	settings::manager::{MutationScope, SettingsManager, SettingsPaths},
};

/// Runs a reflected settings operation against the active native roots.
pub fn run(data_dir: &Path, command: &ConfigCommand) -> miette::Result<()> {
	let project = std::env::current_dir().into_diagnostic()?;
	if let ConfigCommand::Mcp { command } = command {
		return run_mcp(data_dir, &project, command);
	}
	let manager =
		SettingsManager::open(SettingsPaths::discover(data_dir, Some(&project))).into_diagnostic()?;
	match command {
		ConfigCommand::List { json } => list(&manager, *json),
		ConfigCommand::Get { key } => get(&manager, key),
		ConfigCommand::Set { key, value, scope } => {
			manager
				.set_sync(mutation_scope(*scope), key, value)
				.into_diagnostic()?;
			Ok(())
		},
		ConfigCommand::Unset { key, scope } => {
			manager
				.unset_sync(mutation_scope(*scope), key)
				.into_diagnostic()?;
			Ok(())
		},
		ConfigCommand::Path { scope } => {
			println!("{}", path(data_dir, &project, *scope).display());
			Ok(())
		},
		ConfigCommand::Mcp { .. } => unreachable!("MCP commands return before settings composition"),
	}
}

fn run_mcp(data_dir: &Path, project: &Path, command: &McpConfigCommand) -> miette::Result<()> {
	let user = McpConfigStore::new(mcp_path(data_dir, project, McpConfigScope::Global));
	let project_store = McpConfigStore::new(mcp_path(data_dir, project, McpConfigScope::Project));
	let root = McpConfigStore::new(mcp_path(data_dir, project, McpConfigScope::Root));
	match command {
		McpConfigCommand::List { scope, json } => {
			let stores: Vec<(McpConfigScope, &McpConfigStore)> = match scope {
				Some(McpConfigScope::Global) => vec![(McpConfigScope::Global, &user)],
				Some(McpConfigScope::Project) => vec![(McpConfigScope::Project, &project_store)],
				Some(McpConfigScope::Root) => vec![(McpConfigScope::Root, &root)],
				None => vec![
					(McpConfigScope::Project, &project_store),
					(McpConfigScope::Global, &user),
					(McpConfigScope::Root, &root),
				],
			};
			if *json {
				let mut output = serde_json::Map::new();
				for (scope, store) in stores {
					for name in store.list().into_diagnostic()? {
						output
							.insert(name.to_string(), serde_json::json!({"scope": mcp_scope_name(scope)}));
					}
				}
				println!("{}", serde_json::to_string_pretty(&output).into_diagnostic()?);
			} else {
				for (scope, store) in stores {
					for name in store.list().into_diagnostic()? {
						println!("{}\t{name}", mcp_scope_name(scope));
					}
				}
			}
			Ok(())
		},
		McpConfigCommand::Get { name } => {
			for (scope, store) in [
				(McpConfigScope::Project, &project_store),
				(McpConfigScope::Global, &user),
				(McpConfigScope::Root, &root),
			] {
				if let Some(server) = store.get(name).into_diagnostic()? {
					println!(
						"{}",
						serde_json::to_string_pretty(&serde_json::json!({
							"name": name,
							"scope": mcp_scope_name(scope),
							"config": redacted_server(&server),
						}))
						.into_diagnostic()?
					);
					return Ok(());
				}
			}
			Err(miette::miette!("MCP server `{name}` was not found in native configuration"))
		},
		McpConfigCommand::Add { name, config, scope } => {
			let server: McpServerConfig = serde_json::from_str(config).into_diagnostic()?;
			mcp_store(*scope, &user, &project_store, &root)
				.add(name, server)
				.into_diagnostic()
		},
		McpConfigCommand::Update { name, config, scope } => {
			let server: McpServerConfig = serde_json::from_str(config).into_diagnostic()?;
			mcp_store(*scope, &user, &project_store, &root)
				.update(name, server)
				.into_diagnostic()
		},
		McpConfigCommand::Remove { name, scope } => mcp_store(*scope, &user, &project_store, &root)
			.remove(name)
			.into_diagnostic(),
		McpConfigCommand::Enable { name } | McpConfigCommand::Disable { name } => set_server_enabled(
			&user,
			&project_store,
			Some((&root, true)),
			name,
			matches!(command, McpConfigCommand::Enable { .. }),
		)
		.into_diagnostic(),
	}
}

fn mcp_store<'a>(
	scope: McpConfigScope,
	user: &'a McpConfigStore,
	project: &'a McpConfigStore,
	root: &'a McpConfigStore,
) -> &'a McpConfigStore {
	match scope {
		McpConfigScope::Global => user,
		McpConfigScope::Project => project,
		McpConfigScope::Root => root,
	}
}

fn mcp_path(data_dir: &Path, project: &Path, scope: McpConfigScope) -> PathBuf {
	match scope {
		McpConfigScope::Global => data_dir.join("mcp.json"),
		McpConfigScope::Project => project.join(".omp/mcp.json"),
		McpConfigScope::Root => project.join(".mcp.json"),
	}
}

fn mcp_scope_name(scope: McpConfigScope) -> &'static str {
	scope.into()
}

fn redacted_server(server: &McpServerConfig) -> serde_json::Value {
	let mut value = serde_json::to_value(server).unwrap_or(serde_json::Value::Null);
	if let Some(url) = value.get_mut("url")
		&& let Some(raw) = url.as_str()
	{
		*url =
			serde_json::Value::String(crate::envd::mcp::json_rpc::redact_url_for_log(raw).to_string());
	}
	for map_name in ["env", "headers"] {
		if let Some(values) = value
			.get_mut(map_name)
			.and_then(serde_json::Value::as_object_mut)
		{
			for (name, value) in values {
				let name = name.to_ascii_lowercase();
				if ["key", "token", "secret", "authorization", "cookie"]
					.iter()
					.any(|needle| name.contains(needle))
				{
					*value = serde_json::Value::String("[REDACTED]".to_owned());
				}
			}
		}
	}
	value
}

/// Returns the selected native settings path.
#[must_use]
pub fn path(data_dir: &Path, project: &Path, scope: ConfigScope) -> PathBuf {
	match scope {
		ConfigScope::Global => data_dir.join("config.toml"),
		ConfigScope::Project => project
			.ancestors()
			.find(|ancestor| ancestor.join(".omp").is_dir())
			.unwrap_or(project)
			.join(".omp/config.toml"),
	}
}

fn list(manager: &SettingsManager, json: bool) -> miette::Result<()> {
	let snapshot = manager.snapshot();
	if json {
		let mut output = serde_json::Map::new();
		for field in manager.fields() {
			let value = value_at(snapshot.document(), field.path);
			let mut row = serde_json::Map::new();
			let kind: &'static str = field.kind.into();
			row.insert("type".to_owned(), serde_json::Value::String(kind.to_owned()));
			row.insert(
				"description".to_owned(),
				serde_json::Value::String(field.description.to_owned()),
			);
			if field.secret && value.is_some() {
				row.insert("redacted".to_owned(), serde_json::Value::Bool(true));
			} else if let Some(value) = value {
				row.insert("value".to_owned(), serde_json::to_value(value).into_diagnostic()?);
			}
			output.insert(field.path.to_owned(), serde_json::Value::Object(row));
		}
		println!("{}", serde_json::to_string_pretty(&output).into_diagnostic()?);
		return Ok(());
	}
	for field in manager.fields() {
		let rendered = if field.secret && value_at(snapshot.document(), field.path).is_some() {
			"<redacted>".to_owned()
		} else {
			value_at(snapshot.document(), field.path)
				.map(render_value)
				.unwrap_or_else(|| "<unset>".to_owned())
		};
		let kind: &'static str = field.kind.into();
		println!("{}\t{}\t{}", field.path, kind, rendered);
	}
	Ok(())
}

fn get(manager: &SettingsManager, path: &str) -> miette::Result<()> {
	let field = require_field(manager, path)?;
	let snapshot = manager.snapshot();
	match value_at(snapshot.document(), field.path) {
		Some(_) if field.secret => println!("<redacted>"),
		Some(value) => println!("{}", render_value(value)),
		None => println!(),
	}
	Ok(())
}

fn require_field(manager: &SettingsManager, path: &str) -> miette::Result<FieldDescriptor> {
	manager.field(path).ok_or_else(|| {
		let nearest = manager
			.fields()
			.into_iter()
			.filter(|field| field.path.starts_with(path) || path.starts_with(field.path))
			.map(|field| field.path)
			.take(5)
			.collect::<Vec<_>>();
		if nearest.is_empty() {
			miette::miette!("unsupported settings key `{path}`; run `omp config list`")
		} else {
			miette::miette!("unsupported settings key `{path}`; related keys: {}", nearest.join(", "))
		}
	})
}

const fn mutation_scope(scope: ConfigScope) -> MutationScope {
	match scope {
		ConfigScope::Global => MutationScope::Global,
		ConfigScope::Project => MutationScope::Project,
	}
}

fn value_at<'a>(document: &'a toml::Table, path: &str) -> Option<&'a toml::Value> {
	let mut segments = path.split('.');
	let mut value = document.get(segments.next()?)?;
	for segment in segments {
		value = value.as_table()?.get(segment)?;
	}
	Some(value)
}

fn render_value(value: &toml::Value) -> String {
	match value {
		toml::Value::String(value) => value.clone(),
		_ => value.to_string(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::settings::Settings;

	#[test]
	fn reflected_mutations_validate_and_persist() {
		let state = tempfile::tempdir().expect("state");
		let project = tempfile::tempdir().expect("project");
		let manager =
			SettingsManager::open(SettingsPaths::discover(state.path(), Some(project.path())))
				.expect("manager");
		manager
			.set_sync(MutationScope::Global, "runtime.interrupt_grace", "250ms")
			.expect("duration");
		manager
			.set_sync(MutationScope::Project, "worktree.base", "isolated")
			.expect("worktree base");
		assert_eq!(
			manager
				.snapshot()
				.project::<Settings>()
				.expect("projection")
				.get()
				.worktree
				.base,
			Some(PathBuf::from("isolated")),
		);
		assert!(
			manager
				.set_sync(MutationScope::Global, "runtime.interrupt_grace", "none")
				.is_err()
		);
		assert!(
			manager
				.set_sync(MutationScope::Global, "unknown", "value")
				.is_err()
		);
	}
}
