//! Environment-executed workspace checker presets and bounded result parsing.

use std::{
	future::Future,
	path::{Path, PathBuf},
};

use omp_core::Str;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// Cache identity for one checker generation.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CheckerCacheKey {
	/// Canonical workspace.
	pub workspace:         PathBuf,
	/// Authority-resolved executable.
	pub executable:        PathBuf,
	/// Checker configuration fingerprint.
	pub config_generation: u64,
	/// LSP binding generation when the checker wraps a server.
	pub server_generation: Option<u64>,
}

/// A bounded process request that must execute through Environment authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckerRequest {
	/// Executable name, resolved by Environment.
	pub program:    Str,
	/// Arguments.
	pub args:       Vec<Str>,
	/// Workspace-relative cwd.
	pub cwd:        PathBuf,
	/// Maximum captured stdout and stderr lines.
	pub line_limit: usize,
}

/// Environment process completion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckerOutput {
	/// Exit status, when a process was launched.
	pub status: Option<i32>,
	/// Bounded stdout.
	pub stdout: Str,
	/// Bounded stderr.
	pub stderr: Str,
}

/// Typed distinction between code findings and a broken toolchain.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CheckerFault {
	/// Executable is unavailable.
	#[error("checker executable is unavailable")]
	ExecutableUnavailable,
	/// Process could not be launched.
	#[error("checker process failed to launch")]
	LaunchFailed,
	/// Process exceeded its authority-owned deadline.
	#[error("checker timed out")]
	TimedOut,
	/// Output was not the checker's declared format.
	#[error("checker emitted malformed output")]
	MalformedOutput,
	/// Caller cancelled the checker.
	#[error("checker was cancelled")]
	Cancelled,
}

/// Authority seam for checker execution.
pub trait CheckerExecutor: Clone + Send + Sync + 'static {
	/// Runs one bounded request under Environment cancellation and ownership.
	fn run(
		&self,
		request: CheckerRequest,
		cancel: CancellationToken,
	) -> impl Future<Output = Result<CheckerOutput, CheckerFault>> + Send + '_;
}

/// Built-in checker command selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display, strum::EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum Preset {
	/// `cargo check --message-format=json`.
	Cargo,
	/// TypeScript no-emit check.
	TypeScript,
	/// Go workspace/package check.
	Go,
	/// Pyright JSON check.
	Pyright,
	/// Biome JSON lint.
	Biome,
	/// SwiftLint JSON lint.
	SwiftLint,
	/// An installed LSP binding acting as checker.
	LspBinding,
}

/// Builds a pi-compatible bounded command for one workspace/file.
#[must_use]
pub fn request(preset: Preset, workspace: &Path, target: Option<&Path>) -> CheckerRequest {
	let target = target.map(|path| Str::from(path.to_string_lossy().as_ref()));
	let (program, mut args): (&str, Vec<Str>) = match preset {
		Preset::Cargo => (
			"cargo",
			["check", "--message-format=json"]
				.into_iter()
				.map(Str::from)
				.collect(),
		),
		Preset::TypeScript => (
			"tsc",
			["--noEmit", "--pretty", "false"]
				.into_iter()
				.map(Str::from)
				.collect(),
		),
		Preset::Go => (
			"go",
			["test", "./...", "-run", "^$"]
				.into_iter()
				.map(Str::from)
				.collect(),
		),
		Preset::Pyright => ("pyright", ["--outputjson"].into_iter().map(Str::from).collect()),
		Preset::Biome => (
			"biome",
			["lint", "--reporter=json"]
				.into_iter()
				.map(Str::from)
				.collect(),
		),
		Preset::SwiftLint => (
			"swiftlint",
			["lint", "--quiet", "--reporter", "json"]
				.into_iter()
				.map(Str::from)
				.collect(),
		),
		Preset::LspBinding => ("", Vec::new()),
	};
	if let Some(target) = target {
		args.push(target);
	}
	CheckerRequest {
		program: Str::from(program),
		args,
		cwd: workspace.to_path_buf(),
		line_limit: 50,
	}
}

/// Selects the nearest authority-discovered `go.work` directory, otherwise the
/// workspace.
#[must_use]
pub fn go_workspace<'a>(
	file: &Path,
	workspace: &'a Path,
	go_work_directories: &'a [PathBuf],
) -> &'a Path {
	go_work_directories
		.iter()
		.filter(|directory| file.starts_with(directory.as_path()))
		.max_by_key(|directory| directory.components().count())
		.map_or(workspace, PathBuf::as_path)
}

/// Enforces the common 50-line projection bound.
#[must_use]
pub fn bounded_lines(text: &str) -> Str {
	Str::from(text.lines().take(50).collect::<Vec<_>>().join("\n"))
}
