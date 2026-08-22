//! Bounded multi-checker repair command.

pub mod balance;
pub mod checkers;
pub mod parsers;
pub mod types;

use std::{error::Error as StdError, future::Future, path::PathBuf};

use balance::{group_by_file, pack};
pub use checkers::{
	BinaryResolver, CheckerRunner, CustomCheckerSpec, FilesystemResolver, ProcessOutput, Suite,
	custom_suite, discover, parse_custom_specs, run_suite, scan_project_files,
};
use omp_core::Str;
use tokio_util::sync::CancellationToken;
pub use types::*;

/// Result from one bounded repair child.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepairOutcome {
	/// Stable child name.
	pub name:    Str,
	/// Whether the child settled successfully.
	pub success: bool,
	/// Bounded terminal output.
	pub output:  Str,
}

/// Production seams for picker, model discovery, repair children, and journal.
pub trait CleanseHost: BinaryResolver + CheckerRunner {
	/// Returns the canonical project root.
	fn project_root(&self) -> &std::path::Path;
	/// Returns the bounded project file snapshot used for discovery.
	fn project_files(&self) -> &[PathBuf];
	/// Runs the one-shot interactive picker.
	fn pick_target(
		&self,
		checkers: &[Checker],
		cancel: &CancellationToken,
	) -> impl Future<Output = Result<TargetChoice, <Self as CheckerRunner>::Error>> + Send;
	/// Prompts for a free-form request when no built-in checker is runnable.
	fn prompt_request(
		&self,
		cancel: &CancellationToken,
	) -> impl Future<Output = Result<Option<Str>, <Self as CheckerRunner>::Error>> + Send;
	/// Uses a schema-constrained child to discover exact checker argv.
	fn discover_custom(
		&self,
		request: &str,
		model: &str,
		cancel: &CancellationToken,
	) -> impl Future<Output = Result<Str, <Self as CheckerRunner>::Error>> + Send;
	/// Dispatches one file-disjoint repair wave with explicit LSP and IRC
	/// grants.
	fn repair(
		&self,
		assignments: &[Assignment],
		model: &str,
		cancel: &CancellationToken,
	) -> impl Future<Output = Result<Vec<RepairOutcome>, <Self as CheckerRunner>::Error>> + Send;
	/// Appends the verification remainder as a cleanse custom journal fact.
	fn journal_remainder(&self, report: &Report) -> Result<(), <Self as CheckerRunner>::Error>;
}

/// Cleanse orchestration failure before an exit code is available.
#[derive(Debug, thiserror::Error)]
pub enum Error<E: StdError + 'static> {
	/// `--agents` was zero.
	#[error("--agents must be a positive integer")]
	InvalidAgents,
	/// Schema-constrained custom discovery returned malformed JSON.
	#[error("cleanse checker discovery returned invalid structured output")]
	Discovery(#[from] serde_json::Error),
	/// Runtime picker, process, repair, or journal host failed.
	#[error("cleanse runtime host failed")]
	Host(#[source] E),
}

/// Detects, repairs once, verifies, journals the remainder, and returns exactly
/// exit 0, 1, or 130.
pub async fn run<H: CleanseHost>(
	args: &CleanseArgs,
	host: &H,
	cancel: &CancellationToken,
) -> Result<CleanseExit, Error<<H as CheckerRunner>::Error>> {
	if args.agents == 0 {
		return Err(Error::InvalidAgents);
	}
	if cancel.is_cancelled() {
		return Ok(cancelled());
	}
	let mut suite = discover(host.project_root(), host.project_files(), host, args.tests);
	if let Some(request) = args.request.as_deref() {
		suite = discover_requested(request, args.model.as_str(), host, cancel).await?;
	} else if !args.all && !suite.checkers.is_empty() {
		match host
			.pick_target(&suite.checkers, cancel)
			.await
			.map_err(Error::Host)?
		{
			TargetChoice::All => {},
			TargetChoice::Checker(id) => suite.checkers.retain(|checker| checker.id == id),
			TargetChoice::Request(request) => {
				suite = discover_requested(request.as_str(), args.model.as_str(), host, cancel).await?;
			},
			TargetChoice::Cancel => return Ok(cancelled()),
		}
	}
	if args.request.is_none() && !args.all && suite.checkers.is_empty() {
		if let Some(request) = host.prompt_request(cancel).await.map_err(Error::Host)? {
			suite = discover_requested(request.as_str(), args.model.as_str(), host, cancel).await?;
		}
	}
	if cancel.is_cancelled() {
		return Ok(cancelled());
	}
	if suite.checkers.is_empty() {
		return Ok(CleanseExit {
			code:          1,
			status:        CleanseStatus::Unsupported,
			report:        Report { skipped: suite.skipped, ..Report::default() },
			remainder:     Vec::new(),
			omitted_files: 0,
		});
	}
	let initial = match run_suite(host.project_root(), &suite, host, cancel).await {
		Ok(report) => report,
		Err(_) if cancel.is_cancelled() => return Ok(cancelled()),
		Err(error) => return Err(Error::Host(error)),
	};
	if initial.diagnostics.is_empty() {
		host.journal_remainder(&initial).map_err(Error::Host)?;
		return Ok(CleanseExit {
			code:          0,
			status:        CleanseStatus::Clean,
			report:        initial,
			remainder:     Vec::new(),
			omitted_files: 0,
		});
	}
	let assignments = pack(group_by_file(&initial.diagnostics), args.agents);
	match host.repair(&assignments, args.model.as_str(), cancel).await {
		Ok(_) => {},
		Err(_) if cancel.is_cancelled() => return Ok(cancelled()),
		Err(error) => return Err(Error::Host(error)),
	}
	if cancel.is_cancelled() {
		return Ok(cancelled());
	}
	let verified = match run_suite(host.project_root(), &suite, host, cancel).await {
		Ok(report) => report,
		Err(_) if cancel.is_cancelled() => return Ok(cancelled()),
		Err(error) => return Err(Error::Host(error)),
	};
	host.journal_remainder(&verified).map_err(Error::Host)?;
	let groups = group_by_file(&verified.diagnostics);
	let omitted_files = groups.len().saturating_sub(50);
	let remainder = groups.into_iter().take(50).collect();
	Ok(CleanseExit {
		code: u8::from(!verified.diagnostics.is_empty()),
		status: if verified.diagnostics.is_empty() {
			CleanseStatus::Clean
		} else {
			CleanseStatus::Unresolved
		},
		report: verified,
		remainder,
		omitted_files,
	})
}

async fn discover_requested<H: CleanseHost>(
	request: &str,
	model: &str,
	host: &H,
	cancel: &CancellationToken,
) -> Result<Suite, Error<<H as CheckerRunner>::Error>> {
	let json = host
		.discover_custom(request, model, cancel)
		.await
		.map_err(Error::Host)?;
	let specs = parse_custom_specs(json.as_str())?;
	Ok(custom_suite(host.project_root(), specs, host))
}

fn cancelled() -> CleanseExit {
	CleanseExit {
		code:          130,
		status:        CleanseStatus::Cancelled,
		report:        Report::default(),
		remainder:     Vec::new(),
		omitted_files: 0,
	}
}

/// Assignment brief installed in each bounded repair child.
pub fn assignment_prompt(assignment: &Assignment, report: &Report) -> Str {
	let mut text = String::from(
		"Fix only the assigned whole-file diagnostics. Use LSP only when granted; coordinate \
		 overlapping dependencies through IRC. Do not run project-wide tests or edit files outside \
		 the assignment. Preserve user work.\n\nAssigned files:\n",
	);
	for group in &assignment.groups {
		use std::fmt::Write as _;
		let _ = writeln!(
			text,
			"- {} (weight {})",
			group.file.as_deref().unwrap_or("<project>"),
			group.weight
		);
		for diagnostic in &group.diagnostics {
			let _ = writeln!(
				text,
				"  - {:?} {}:{}: {}",
				diagnostic.severity,
				diagnostic.line.unwrap_or(0),
				diagnostic.column.unwrap_or(0),
				diagnostic.message
			);
		}
	}
	use std::fmt::Write as _;
	let _ = writeln!(
		text,
		"\nChecker count: {}. Reverification belongs to the parent.",
		report.checks.len()
	);
	Str::from(text)
}

/// Strict JSON Schema installed for the freeform checker-discovery child.
pub fn discovery_schema() -> serde_json::Value {
	serde_json::json!({
		"type": "array",
		"items": {
			"type": "object",
			"additionalProperties": false,
			"required": ["id", "label", "language", "binary", "args", "parser", "mutating"],
			"properties": {
				"id": {"type": "string", "minLength": 1},
				"label": {"type": "string", "minLength": 1},
				"language": {"type": "string", "minLength": 1},
				"cwd": {"type": "string"},
				"binary": {"type": "string", "minLength": 1},
				"args": {"type": "array", "items": {"type": "string"}},
				"parser": {"enum": ["rust", "rust-test", "go", "go-test", "staticcheck", "golangci", "ruff", "pyright", "mypy", "pylint", "flake8", "ty", "eslint", "biome", "oxlint", "deno-lint", "stylelint", "rubocop", "phpstan", "psalm", "swiftlint", "dart", "credo", "shellcheck", "hlint", "terraform", "tflint", "actionlint", "generic"]},
				"mutating": {"type": "boolean"}
			}
		}
	})
}
