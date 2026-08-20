//! Non-gating recorder for the deployment pooling decision matrix.
//!
//! The executable deliberately has no opinion about the host implementation. A
//! runner receives one matrix cell through environment variables, exercises
//! that cell, and emits its measured JSON object on stdout. This binary
//! validates, labels, and persists the complete matrix so pooling advice can be
//! based on comparable measurements.

use std::{
	ffi::OsString,
	fs::File,
	path::{Path, PathBuf},
	process::Command,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Versioned shape of the pooling benchmark artifact.
const SCHEMA_VERSION: u32 = 1;

/// One fully specified cell of the §6.8.1 pooling matrix.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct MatrixCell {
	/// Number of active extensions.
	pub extensions_active:  u8,
	/// Dependency closure representative for this cell.
	pub dependency_profile: &'static str,
	/// Process lifecycle exercised by this cell.
	pub lifecycle:          &'static str,
	/// Environment-link latency condition.
	pub environment_link:   &'static str,
	/// Number of subscribed hook phases.
	pub hook_load:          &'static str,
	/// Invocation behavior used by the runner.
	pub invocation_pattern: &'static str,
}

/// Metrics emitted by a runner after it has completed one matrix cell.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Measurements {
	/// Resident set size in bytes after the exercised operation settles.
	pub rss_bytes:           u64,
	/// Proportional set size in bytes, including shared pages proportionally.
	pub pss_bytes:           u64,
	/// Spawn through verified `RegisterTools`, in microseconds.
	pub boot_micros:         u64,
	/// Session open through first accepted prompt, in microseconds.
	pub prompt_start_micros: u64,
	/// Event emit through hook decision, in microseconds.
	pub hook_latency_micros: u64,
	/// Hot-reload respawn duration, in microseconds.
	pub reload_micros:       u64,
	/// In-flight calls and state units lost by cancellation.
	pub collateral_loss:     u64,
}

/// One recorded runner sample, including all matrix labels needed for later
/// comparison.
#[derive(Clone, Debug, Serialize)]
pub struct Record {
	/// Matrix condition passed to the runner.
	pub cell:         MatrixCell,
	/// Runner-observed metrics for the condition.
	pub measurements: Measurements,
}

/// Complete, portable artifact emitted by this non-gating recorder.
#[derive(Clone, Debug, Serialize)]
pub struct Artifact {
	/// Schema revision for consumers of this artifact.
	pub schema_version: u32,
	/// All recorded cells in deterministic matrix order.
	pub records:        Vec<Record>,
}

/// Enumerates the full, deterministic §6.8.1 matrix.
pub fn matrix() -> impl Iterator<Item = MatrixCell> {
	const EXTENSIONS: [u8; 4] = [0, 5, 15, 32];
	const DEPENDENCIES: [&str; 3] = ["pure-python", "common-native", "large-ml-wheel"];
	const LIFECYCLES: [&str; 3] = ["cold-boot", "warm-restart", "hot-reload"];
	const LINKS: [&str; 3] = ["local", "remote-20ms-rtt", "remote-100ms-rtt"];
	const HOOKS: [&str; 2] = ["one-phase", "five-phases"];
	const INVOCATIONS: [&str; 3] = ["one-call", "concurrent-calls", "cancellation-mid-call"];
	EXTENSIONS.into_iter().flat_map(|extensions_active| {
		DEPENDENCIES
			.into_iter()
			.flat_map(move |dependency_profile| {
				LIFECYCLES.into_iter().flat_map(move |lifecycle| {
					LINKS.into_iter().flat_map(move |environment_link| {
						HOOKS.into_iter().flat_map(move |hook_load| {
							INVOCATIONS
								.into_iter()
								.map(move |invocation_pattern| MatrixCell {
									extensions_active,
									dependency_profile,
									lifecycle,
									environment_link,
									hook_load,
									invocation_pattern,
								})
						})
					})
				})
			})
	})
}

/// Prints command use and the artifact schema expected from a runner.
fn help() {
	println!(
		"pooling-bench --runner PATH --output ARTIFACT.json [--limit CELLS]\n\nRecords the full 4 × \
		 3 × 3 × 3 × 2 × 3 pooling matrix (648 cells) by default. --limit records only the first \
		 CELLS in deterministic matrix order for smoke testing; it does not change cell \
		 semantics.\nThe runner is invoked once per cell with these environment \
		 variables:\nOMP_POOL_EXTENSIONS_ACTIVE, OMP_POOL_DEPENDENCY_PROFILE, \
		 OMP_POOL_LIFECYCLE,\nOMP_POOL_ENVIRONMENT_LINK, OMP_POOL_HOOK_LOAD, \
		 OMP_POOL_INVOCATION_PATTERN\nIt must write one JSON Measurements object to stdout with: \
		 rss_bytes, pss_bytes,\nboot_micros, prompt_start_micros, hook_latency_micros, \
		 reload_micros, collateral_loss.\n\n--help prints this schema and does not execute a runner."
	);
}

/// Parses the intentionally small command surface without bringing a CLI
/// dependency into e2e.
fn arguments() -> Result<(PathBuf, PathBuf, Option<usize>)> {
	let mut arguments = std::env::args_os().skip(1);
	let mut runner = None;
	let mut output = None;
	let mut limit = None;
	while let Some(argument) = arguments.next() {
		match argument.to_string_lossy().as_ref() {
			"--help" | "-h" => {
				help();
				std::process::exit(0);
			},
			"--runner" => runner = Some(PathBuf::from(next_value(&mut arguments, "--runner")?)),
			"--output" => output = Some(PathBuf::from(next_value(&mut arguments, "--output")?)),
			"--limit" => {
				let value = next_value(&mut arguments, "--limit")?;
				let parsed = value
					.to_string_lossy()
					.parse::<usize>()
					.context("--limit must be a positive integer no greater than 648")?;
				if !(1..=648).contains(&parsed) {
					bail!("--limit must be a positive integer no greater than 648");
				}
				limit = Some(parsed);
			},
			unknown => bail!("unknown argument {unknown:?}; pass --help for the matrix schema"),
		}
	}
	Ok((
		runner.context("--runner is required; pass --help for the matrix schema")?,
		output.context("--output is required")?,
		limit,
	))
}

/// Takes one required OS argument value.
fn next_value(arguments: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<OsString> {
	arguments
		.next()
		.with_context(|| format!("{flag} requires a value"))
}

/// Runs the supplied experimental runner for one condition and decodes its
/// measurements.
fn sample(runner: &Path, cell: MatrixCell) -> Result<Measurements> {
	let output = Command::new(runner)
		.env("OMP_POOL_EXTENSIONS_ACTIVE", cell.extensions_active.to_string())
		.env("OMP_POOL_DEPENDENCY_PROFILE", cell.dependency_profile)
		.env("OMP_POOL_LIFECYCLE", cell.lifecycle)
		.env("OMP_POOL_ENVIRONMENT_LINK", cell.environment_link)
		.env("OMP_POOL_HOOK_LOAD", cell.hook_load)
		.env("OMP_POOL_INVOCATION_PATTERN", cell.invocation_pattern)
		.output()
		.with_context(|| format!("could not execute pooling runner {}", runner.display()))?;
	if !output.status.success() {
		bail!(
			"pooling runner failed for {cell:?}: {}",
			String::from_utf8_lossy(&output.stderr).trim()
		);
	}
	serde_json::from_slice(&output.stdout)
		.with_context(|| format!("pooling runner emitted invalid measurements for {cell:?}"))
}

/// Executes the selected cells and writes one self-describing artifact.
fn record(runner: &Path, limit: Option<usize>) -> Result<Artifact> {
	let limit = limit.unwrap_or(648);
	let mut records = Vec::with_capacity(limit);
	for cell in matrix().take(limit) {
		records.push(Record { cell, measurements: sample(runner, cell)? });
	}
	Ok(Artifact { schema_version: SCHEMA_VERSION, records })
}

/// Runs the non-gating recorder.
fn main() -> Result<()> {
	let (runner, output, limit) = arguments()?;
	let artifact = record(&runner, limit)?;
	let file =
		File::create(&output).with_context(|| format!("could not create {}", output.display()))?;
	serde_json::to_writer_pretty(file, &artifact)
		.context("could not write pooling benchmark artifact")?;
	println!("recorded {} pooling matrix cells to {}", artifact.records.len(), output.display());
	Ok(())
}
