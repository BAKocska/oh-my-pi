//! Schema-validated settings command handlers.

use std::{
	path::{Path, PathBuf},
	str::FromStr,
};

use miette::IntoDiagnostic as _;
use omp_core::Duration;

use crate::{settings::Settings, usage_error::CliUsageError};

/// Runs a schema-validated settings operation against the active data
/// directory.
pub fn run(data_dir: &Path, command: &crate::cli::ConfigCommand) -> miette::Result<()> {
	match command {
		crate::cli::ConfigCommand::List { json } => list(data_dir, *json),
		crate::cli::ConfigCommand::Get { key } => get(data_dir, key),
		crate::cli::ConfigCommand::Set { key, value } => set(data_dir, key, value),
		crate::cli::ConfigCommand::Reset { key } => reset(data_dir, key),
		crate::cli::ConfigCommand::Path => {
			println!("{}", path(data_dir).display());
			Ok(())
		},
	}
}

/// Returns the active settings path.
#[must_use]
pub fn path(data_dir: &Path) -> std::path::PathBuf {
	data_dir.join("config.toml")
}

fn list(data_dir: &Path, json: bool) -> miette::Result<()> {
	let settings = Settings::load(data_dir);
	if json {
		println!("{}", serde_json::to_string_pretty(&settings).into_diagnostic()?);
	} else {
		println!("default_model\tstring\t{}", settings.default_model.as_deref().unwrap_or("<unset>"));
		println!("runtime.interrupt_grace\tduration\t{}", settings.runtime.interrupt_grace);
		println!(
			"worktree.base\tpath\t{}",
			settings
				.worktree
				.base
				.as_deref()
				.map_or_else(|| "<unset>".to_owned(), |path| path.display().to_string())
		);
	}
	Ok(())
}

fn get(data_dir: &Path, key: &str) -> miette::Result<()> {
	let settings = Settings::load(data_dir);
	match key {
		"default_model" => println!("{}", settings.default_model.as_deref().unwrap_or("")),
		"runtime.interrupt_grace" => println!("{}", settings.runtime.interrupt_grace),
		"worktree.base" => println!(
			"{}",
			settings
				.worktree
				.base
				.as_deref()
				.map_or_else(String::new, |path| path.display().to_string())
		),
		_ => return Err(CliUsageError::new(format!("unknown setting `{key}`")).into()),
	}
	Ok(())
}

fn set(data_dir: &Path, key: &str, value: &str) -> miette::Result<()> {
	let mut settings = Settings::load(data_dir);
	match key {
		"default_model" if !value.trim().is_empty() => settings.default_model = Some(value.into()),
		"default_model" => return Err(CliUsageError::new("default_model must not be empty").into()),
		"runtime.interrupt_grace" => {
			let duration =
				Duration::from_str(value).map_err(|error| CliUsageError::new(error.to_string()))?;
			if duration.value() == 0 {
				return Err(CliUsageError::new("runtime.interrupt_grace must be non-zero").into());
			}
			settings.runtime.interrupt_grace = duration;
		},
		"worktree.base" if !value.trim().is_empty() => {
			settings.worktree.base = Some(PathBuf::from(value));
		},
		"worktree.base" => {
			return Err(CliUsageError::new("worktree.base must not be empty").into());
		},
		_ => return Err(CliUsageError::new(format!("unknown setting `{key}`")).into()),
	}
	settings.save(data_dir).into_diagnostic()?;
	Ok(())
}

fn reset(data_dir: &Path, key: &str) -> miette::Result<()> {
	let mut settings = Settings::load(data_dir);
	match key {
		"default_model" => settings.default_model = None,
		"runtime.interrupt_grace" => settings.runtime = Default::default(),
		"worktree.base" => settings.worktree.base = None,
		_ => return Err(CliUsageError::new(format!("unknown setting `{key}`")).into()),
	}
	settings.save(data_dir).into_diagnostic()?;
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn validates_the_documented_setting_schema() {
		let state = tempfile::tempdir().expect("state");
		set(state.path(), "runtime.interrupt_grace", "250ms").expect("duration");
		set(state.path(), "worktree.base", "isolated").expect("worktree base");
		assert_eq!(Settings::load(state.path()).worktree.base, Some(PathBuf::from("isolated")));
		assert!(set(state.path(), "runtime.interrupt_grace", "none").is_err());
		assert!(set(state.path(), "unknown", "value").is_err());
	}
}
