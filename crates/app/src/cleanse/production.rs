//! Production checker, repair-session, picker, and journal composition.

use std::{future::Future, path::{Path, PathBuf}, sync::Arc};

use futures::{StreamExt as _, stream};
use miette::IntoDiagnostic as _;
use omp_agent::{
	EntryKindDecl, Journal, JournalAuthor, JournalOperation, JournalRequest, JournalRequestStamp,
	PendingCustomEntry, TurnId,
};
use omp_core::{ArtifactDigest, Principal, Provenance, Str, sf};
use omp_proto::thread::v1::{Item, Message, Part, Role, item, part};
use omp_storage::{
	index::{SessionIndex, SessionKind},
};
use parking_lot::Mutex;
use serde_json::value::to_raw_value;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use super::{
	Assignment, BinaryResolver, Checker, CheckerRunner, CleanseArgs, CleanseExit, CleanseHost,
	FilesystemResolver, ProcessOutput, RepairOutcome, Report, TargetChoice, assignment_prompt,
	discovery_schema, scan_project_files,
};

const JOURNAL_EXTENSION: &str = "so.omp.cleanse";
const JOURNAL_KIND: &str = "so.omp.cleanse.remainder";
const JOURNAL_REVISION: &str = "cleanse.1";

/// Failure from the production cleanse authorities.
#[derive(Debug, thiserror::Error)]
pub enum ProductionError {
	/// Project traversal failed.
	#[error("failed to snapshot cleanse project files under {root:?}")]
	Scan {
		/// Canonical project root.
		root: PathBuf,
		/// Filesystem failure.
		#[source]
		source: std::io::Error,
	},
	/// A checker process could not be started or observed.
	#[error("failed to run cleanse checker {binary:?}")]
	Checker {
		/// Resolved checker executable.
		binary: PathBuf,
		/// Process failure.
		#[source]
		source: std::io::Error,
	},
	/// Standalone target selection failed.
	#[error(transparent)]
	Picker(#[from] crate::pickers::PickerError),
	/// Production agent composition failed.
	#[error("cleanse child session failed")]
	Session,
	/// The configured data directory or project state could not be opened.
	#[error("cleanse transcript authority could not be opened")]
	JournalOpen(#[source] crate::chat::ChatError),
	/// The session index could not be opened.
	#[error(transparent)]
	SessionIndex(#[from] omp_storage::index::Error),
	/// A cleanse journal declaration or append failed.
	#[error(transparent)]
	Journal(#[from] omp_agent::JournalError),
	/// A static journal revision was invalid.
	#[error(transparent)]
	Revision(#[from] omp_tool::RevParseError),
	/// A journal payload could not be encoded.
	#[error(transparent)]
	Json(#[from] serde_json::Error),
	/// The command has no configured model.
	#[error("cleanse requires a configured model")]
	MissingModel,
}

/// Production owner for one standalone cleanse run.
pub struct ProductionCleanseHost {
	root:     PathBuf,
	files:    Vec<PathBuf>,
	data_dir: PathBuf,
	resolver: FilesystemResolver,
	journal:  Mutex<Journal>,
}

impl ProductionCleanseHost {
	/// Opens checker discovery and a durable parent transcript for one run.
	pub fn open(root: PathBuf, data_dir: PathBuf) -> Result<Self, ProductionError> {
		let root = crate::chat::canonical_project(&root)
			.map_err(ProductionError::JournalOpen)?;
		let files = scan_project_files(&root)
			.map_err(|source| ProductionError::Scan { root: root.clone(), source })?;
		let state_dir = crate::project_state::directory(&data_dir, &root)
			.map_err(|_| ProductionError::Session)?;
		crate::chat::ensure_state_directory(&state_dir)
			.map_err(ProductionError::JournalOpen)?;
		let sessions_dir = state_dir.join("sessions");
		crate::chat::ensure_state_directory(&sessions_dir)
			.map_err(ProductionError::JournalOpen)?;
		let index = Arc::new(SessionIndex::open(state_dir.join("sessions.sqlite3"))?);
		let id = Str::from(omp_core::Ulid::generate().to_string());
		let mut journal = crate::chat::create_indexed_journal(
			&sessions_dir.join(format!("{}.jsonl", id.as_str())),
			&root,
			&id,
			index,
			SessionKind::Interactive,
			None,
		)
		.map_err(ProductionError::JournalOpen)?;
		journal.declare_entry_kinds(
			JOURNAL_EXTENSION,
			[EntryKindDecl::parse(JOURNAL_KIND, JOURNAL_REVISION, false, false, None)?],
		)?;
		Ok(Self { root, files, data_dir, resolver: FilesystemResolver, journal: Mutex::new(journal) })
	}

	async fn child_session(
		&self,
		name: &str,
		model: &str,
		schema_name: &'static str,
		schema: serde_json::Value,
		prompt: Str,
		cancel: &CancellationToken,
	) -> Result<RepairOutcome, ProductionError> {
		let mut session = crate::headless::HeadlessSession::open(
			self.data_dir.clone(),
			crate::headless::HeadlessSessionOptions {
				project: self.root.clone(),
				additional_roots: Box::new([]),
				model: Str::new(model),
				initial_campaign: None,
				initial_prompt_slot: None,
				resume: None,
				fork: None,
				py_eval: false,
				pty_denied: false,
				credential_provider: None,
				api_key: None,
				prompt_cache_affinity: None,
				session_generation: 1,
			},
		)
		.await
		.map_err(|_| ProductionError::Session)?;
		session.set_response_schema(schema_name, schema)?;
		session
			.set_title(Str::new(name))
			.await
			.map_err(|_| ProductionError::Session)?;
		let interrupt = session.interrupt_handle();
		let submitted = tokio::select! {
			result = session.submit(
				[message(Role::System, "You are a bounded cleanse worker. Obey the assignment exactly and return only the required JSON."), message(Role::User, prompt.as_str())],
				TurnId::new(format!("cleanse-{}", omp_core::Ulid::generate())),
			) => Some(result),
			() = cancel.cancelled() => {
				interrupt.interrupt();
				None
			},
		};
		let outcome = match submitted {
			Some(Ok(summary)) => RepairOutcome {
				name: Str::new(name),
				success: !summary.interrupted && summary.final_assistant().is_some(),
				output: summary.final_assistant().map_or_else(|| sf!(""), Str::new),
			},
			Some(Err(_)) => {
				session.dispose().await;
				return Err(ProductionError::Session);
			},
			None => RepairOutcome { name: Str::new(name), success: false, output: sf!("cancelled") },
		};
		session.dispose().await;
		Ok(outcome)
	}
}

impl BinaryResolver for ProductionCleanseHost {
	fn resolve(&self, project_root: &Path, manifest_root: &Path, names: &[&str]) -> Option<PathBuf> {
		self.resolver.resolve(project_root, manifest_root, names)
	}
}

impl CheckerRunner for ProductionCleanseHost {
	type Error = ProductionError;

	fn run_checker(
		&self,
		checker: &Checker,
		cancel: &CancellationToken,
	) -> impl Future<Output = Result<ProcessOutput, Self::Error>> + Send {
		let checker = checker.clone();
		let cancel = cancel.clone();
		async move { run_checker_process(&checker, &cancel).await }
	}
}

async fn run_checker_process(
	checker: &Checker,
	cancel: &CancellationToken,
) -> Result<ProcessOutput, ProductionError> {
	let mut command = Command::new(&checker.binary);
	command.args(checker.args.iter().map(Str::as_str));
	command.current_dir(&checker.cwd).kill_on_drop(true);
	let output = tokio::select! {
		result = command.output() => result.map_err(|source| ProductionError::Checker { binary: checker.binary.clone(), source })?,
		() = cancel.cancelled() => return Ok(ProcessOutput { exit_code: None, stdout: sf!(""), stderr: sf!("cancelled") }),
	};
	Ok(ProcessOutput {
		exit_code: output.status.code(),
		stdout: Str::from(String::from_utf8_lossy(&output.stdout).as_ref()),
		stderr: Str::from(String::from_utf8_lossy(&output.stderr).as_ref()),
	})
}
 
impl CleanseHost for ProductionCleanseHost {
	fn project_root(&self) -> &Path {
		&self.root
	}

	fn project_files(&self) -> &[PathBuf] {
		&self.files
	}

	fn pick_target(
		&self,
		checkers: &[Checker],
		cancel: &CancellationToken,
	) -> impl Future<Output = Result<TargetChoice, Self::Error>> {
		let checkers = checkers.to_vec();
		let cancel = cancel.clone();
		async move {
			tokio::select! {
				result = crate::pickers::pick_cleanse_target(&checkers) => result.map_err(Into::into),
				() = cancel.cancelled() => Ok(TargetChoice::Cancel),
			}
		}
	}

	fn prompt_request(
		&self,
		cancel: &CancellationToken,
	) -> impl Future<Output = Result<Option<Str>, Self::Error>> {
		let cancelled = cancel.is_cancelled();
		std::future::ready(if cancelled {
			Ok(None)
		} else {
			crate::pickers::prompt_cleanse_request().map_err(Into::into)
		})
	}

	fn discover_custom(
		&self,
		request: &str,
		model: &str,
		cancel: &CancellationToken,
	) -> impl Future<Output = Result<Str, Self::Error>> + Send {
		let request = Str::new(request);
		let model = Str::new(model);
		async move {
			let prompt = Str::from(format!(
				"Read project manifests and configs, determine exact argv commands that detect: {request}. Run each candidate once without editing. Return only the schema-constrained checker array; argv must never use a shell wrapper."
			));
			let outcome = self
				.child_session("CleanseDiscovery", model.as_str(), "cleanse_discovery", discovery_schema(), prompt, cancel)
				.await?;
			Ok(outcome.output)
		}
	}

	fn repair(
		&self,
		assignments: &[Assignment],
		model: &str,
		cancel: &CancellationToken,
	) -> impl Future<Output = Result<Vec<RepairOutcome>, Self::Error>> + Send {
		let assignments = assignments.to_vec();
		let model = Str::new(model);
		async move {
			let count = assignments.len();
			stream::iter(assignments.into_iter().enumerate().map(|(index, assignment)| {
				let model = model.clone();
				async move {
					let name = format!("CleanseW1A{}", index + 1);
					let prompt = assignment_prompt(&assignment, &Report::default());
					self.child_session(
						&name,
						model.as_str(),
						"cleanse_repair",
						repair_schema(),
						prompt,
						cancel,
					).await
				}
			}))
			.buffered(count.max(1))
			.collect::<Vec<_>>()
			.await
			.into_iter()
			.collect()
		}
	}

	fn journal_remainder(&self, report: &Report) -> Result<(), Self::Error> {
		let data = to_raw_value(&serde_json::json!({
			"checks": report.checks.len(),
			"diagnostics": report.diagnostics,
			"skipped": report.skipped.iter().map(|item| serde_json::json!({
				"label": item.label,
				"language": item.language,
				"reason": item.reason,
			})).collect::<Vec<_>>(),
		}))?;
		let request_id = sf!("cleanse-remainder-{}", omp_core::Ulid::generate());
		let mut journal = self.journal.lock();
		journal.handle_request(JournalRequest {
			ts: now_ms(),
			stamp: JournalRequestStamp {
				request_id: request_id.clone(),
				idempotency_key: request_id,
				host_generation: 0,
				session_generation: 0,
			},
			author: JournalAuthor {
				principal: Principal::new(sf!("omp.core"), sf!("OMP Core")),
				provenance: Provenance::new(
					sf!("omp"), sf!(JOURNAL_EXTENSION), sf!(env!("CARGO_PKG_VERSION")),
					ArtifactDigest::new([0; 32]), sf!("core"), sf!("builtin"), 0,
				),
			},
			operation: JournalOperation::Append(PendingCustomEntry {
				kind: sf!(JOURNAL_KIND),
				rev: sf!(JOURNAL_REVISION),
				data: Some(data),
				context: None,
				display: Some(false),
			}),
		})?;
		Ok(())
	}
}

/// Runs the production standalone command and renders its bounded report.
pub async fn run_command(args: CleanseArgs) -> miette::Result<()> {
	let root = std::env::current_dir().into_diagnostic()?;
	let host = ProductionCleanseHost::open(root, crate::cli::data_dir(None)?)
		.map_err(|error| miette::miette!(error))?;
	let cancel = CancellationToken::new();
	let result: CleanseExit = super::run(&args, &host, &cancel)
		.await
		.map_err(|error| miette::miette!(error))?;
	for check in &result.report.checks {
		println!("- {}: {} issue(s)", check.checker.label, check.diagnostics.len());
	}
	match result.status {
		super::CleanseStatus::Clean => println!("Clean: all detected diagnostics are resolved."),
		super::CleanseStatus::Unresolved => {
			eprintln!("Unresolved: {} diagnostic(s).", result.report.diagnostics.len());
			for group in &result.remainder {
				eprintln!("- {}: {}", group.file.as_deref().unwrap_or("<project>"), group.diagnostics.len());
			}
			if result.omitted_files != 0 {
				eprintln!("- ... {} more files", result.omitted_files);
			}
		},
		super::CleanseStatus::Unsupported => eprintln!("No supported runnable checker was found."),
		super::CleanseStatus::Cancelled => eprintln!("Cleanse cancelled."),
	}
	if result.code == 0 { Ok(()) } else { Err(miette::miette!("cleanse exited with status {}", result.code)) }
}

fn repair_schema() -> serde_json::Value {
	serde_json::json!({
		"type": "object",
		"additionalProperties": false,
		"required": ["success", "summary"],
		"properties": {
			"success": {"type": "boolean"},
			"summary": {"type": "string"}
		}
	})
}

fn message(role: Role, text: &str) -> Item {
	Item {
		kind: Some(item::Kind::Message(Message {
			role: role as i32,
			parts: vec![Part { kind: Some(part::Kind::Text(text.to_owned())), ..Part::default() }],
			..Message::default()
		})),
		..Item::default()
	}
}

fn now_ms() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.unwrap_or_default()
		.as_millis()
		.try_into()
		.unwrap_or(u64::MAX)
}
#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn production_process_owner_executes_exact_argv() {
		fn assert_host<H: CleanseHost>() {}
		assert_host::<ProductionCleanseHost>();
		let checker = Checker {
			id: sf!("self-list"),
			label: sf!("test harness list"),
			language: sf!("Rust"),
			cwd: std::env::current_dir().expect("test cwd"),
			binary: std::env::current_exe().expect("test executable"),
			args: vec![sf!("--list")],
			parser: crate::cleanse::parsers::ParserKind::Generic,
			effect: crate::cleanse::CheckerEffect::ReadOnly,
			test: false,
		};
		let output = run_checker_process(&checker, &CancellationToken::new())
			.await
			.expect("checker process");
		assert_eq!(output.exit_code, Some(0));
		assert!(output.stdout.contains("production_process_owner_executes_exact_argv"));
	}
}
