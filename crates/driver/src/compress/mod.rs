//! Standalone semantic-compression command.

pub mod production;
pub mod protocol;
pub mod types;

use std::{
	collections::BTreeSet,
	error::Error as StdError,
	future::Future,
	path::{Path, PathBuf},
};

use futures::{StreamExt as _, stream};
use globset::Glob;
use omp_core::Str;
pub use protocol::{Protocol, ProtocolError, tool_schemas};
use tokio_util::sync::CancellationToken;
pub use types::*;

/// Child-session restrictions for semantic compression.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IsolationPolicy {
	/// MCP is always removed.
	pub mcp:        bool,
	/// Extensions are always removed.
	pub extensions: bool,
	/// IRC/hub tools are always removed.
	pub irc:        bool,
	/// LSP is always removed.
	pub lsp:        bool,
	/// World/read/write/shell tools are always removed.
	pub world:      bool,
	/// Exact advertised tool names.
	pub tools:      [&'static str; 2],
}

/// Required compression-only policy.
pub const ISOLATION_POLICY: IsolationPolicy = IsolationPolicy {
	mcp:        false,
	extensions: false,
	irc:        false,
	lsp:        false,
	world:      false,
	tools:      ["rewrite", "approve"],
};

/// Production host for document-authority I/O and restricted child sessions.
pub trait CompressHost: Sync {
	/// Restricted child handle.
	type Session: Send;
	/// Typed host failure.
	type Error: StdError + Send + Sync + 'static;

	/// Reads source text through Environment document authority.
	fn read_text(
		&self,
		path: &Path,
		cancel: &CancellationToken,
	) -> impl Future<Output = Result<Str, Self::Error>> + Send;
	/// Opens a child with the compression system slot and exactly two tools.
	fn open_session(
		&self,
		name: &str,
		model: Option<&str>,
		policy: IsolationPolicy,
		cancel: &CancellationToken,
	) -> impl Future<Output = Result<Self::Session, Self::Error>> + Send;
	/// Runs one synthetic turn and returns its tool calls in original order.
	fn turn<'a>(
		&'a self,
		session: &'a mut Self::Session,
		prompt: Str,
		cancel: &'a CancellationToken,
	) -> impl Future<Output = Result<Vec<Action>, Self::Error>> + Send + 'a;
	/// Disposes one restricted child.
	fn close_session(
		&self,
		session: Self::Session,
	) -> impl Future<Output = Result<(), Self::Error>> + Send;
	/// Atomically writes an approved draft through document authority.
	fn write_approved(
		&self,
		path: &Path,
		text: &str,
		cancel: &CancellationToken,
	) -> impl Future<Output = Result<(), Self::Error>> + Send;
	/// Progress notification, replaced in-place by interactive adapters.
	fn progress(&self, completed: usize, total: usize, path: &Path, status: Status);
}

/// Argument, discovery, protocol, or runtime failure.
#[derive(Debug, thiserror::Error)]
pub enum Error<E: StdError + 'static> {
	/// No source was requested.
	#[error("compress requires at least one file or glob pattern")]
	MissingFiles,
	/// Round budget was zero.
	#[error("--rounds must be a positive integer")]
	InvalidRounds,
	/// Concurrency was zero.
	#[error("--agents must be a positive integer")]
	InvalidConcurrency,
	/// Output policies conflict.
	#[error("--in-place and --out are mutually exclusive")]
	OutputConflict,
	/// Multi-file output requires in-place writes.
	#[error("multiple files matched; pass --in-place (--out accepts one file)")]
	MultipleFilesNeedInPlace,
	/// Literal target is absent or not a file.
	#[error("compress target is not a file: {path:?}")]
	NotFile {
		/// Invalid literal target.
		path: PathBuf,
	},
	/// A glob matched no files.
	#[error("compress glob matched no files")]
	NoGlobMatches,
	/// Glob syntax was invalid.
	#[error("compress glob pattern is invalid")]
	Glob(#[from] globset::Error),
	/// Target discovery failed.
	#[error("failed to inspect compress target {path:?}")]
	Io {
		/// Inspected path.
		path:   PathBuf,
		/// Filesystem failure.
		#[source]
		source: std::io::Error,
	},
	/// Restricted session or document authority failed.
	#[error("compress runtime host failed")]
	Host(#[source] E),
	/// The model violated rewrite/approve ordering.
	#[error(transparent)]
	Protocol(#[from] ProtocolError),
}

/// Expands literal files and globs (including dot directories), deduplicated
/// and sorted.
pub fn resolve_targets(
	patterns: &[Str],
	cwd: &Path,
) -> Result<Vec<PathBuf>, Error<std::convert::Infallible>> {
	let all_files = walk_files(cwd)?;
	let mut targets = BTreeSet::new();
	for pattern in patterns {
		if has_glob_meta(pattern.as_str()) {
			let matcher = Glob::new(pattern.as_str())?.compile_matcher();
			let mut matched = false;
			for path in &all_files {
				let relative = path.strip_prefix(cwd).unwrap_or(path);
				if matcher.is_match(relative) {
					targets.insert(path.clone());
					matched = true;
				}
			}
			if !matched {
				return Err(Error::NoGlobMatches);
			}
		} else {
			let path = cwd.join(pattern.as_str());
			let metadata =
				std::fs::metadata(&path).map_err(|source| Error::Io { path: path.clone(), source })?;
			if !metadata.is_file() {
				return Err(Error::NotFile { path });
			}
			targets.insert(
				path
					.canonicalize()
					.map_err(|source| Error::Io { path: path.clone(), source })?,
			);
		}
	}
	Ok(targets.into_iter().collect())
}

fn walk_files(root: &Path) -> Result<Vec<PathBuf>, Error<std::convert::Infallible>> {
	let mut files = Vec::new();
	let mut pending = vec![root.to_path_buf()];
	while let Some(directory) = pending.pop() {
		let entries = std::fs::read_dir(&directory)
			.map_err(|source| Error::Io { path: directory.clone(), source })?;
		for entry in entries {
			let entry = entry.map_err(|source| Error::Io { path: directory.clone(), source })?;
			let kind = entry
				.file_type()
				.map_err(|source| Error::Io { path: entry.path(), source })?;
			if kind.is_dir() {
				pending.push(entry.path());
			} else if kind.is_file() {
				files.push(
					entry
						.path()
						.canonicalize()
						.map_err(|source| Error::Io { path: entry.path(), source })?,
				);
			}
		}
	}
	Ok(files)
}

fn has_glob_meta(pattern: &str) -> bool {
	pattern
		.bytes()
		.any(|byte| matches!(byte, b'*' | b'?' | b'[' | b']' | b'{' | b'}'))
}

/// Compresses every resolved target with bounded file concurrency.
pub async fn run<H: CompressHost>(
	args: &CompressArgs,
	invocation_dir: &Path,
	host: &H,
	cancel: &CancellationToken,
) -> Result<CompressExit, Error<H::Error>> {
	validate(args)?;
	let targets = resolve_targets(&args.files, invocation_dir).map_err(convert_discovery_error)?;
	if targets.len() > 1 && !args.in_place {
		return Err(Error::MultipleFilesNeedInPlace);
	}
	let total = targets.len();
	let concurrency = args.concurrency.min(total.max(1));
	let outcomes = stream::iter(
		targets
			.into_iter()
			.enumerate()
			.map(|(index, path)| async move {
				let result =
					compress_file(index, path.clone(), args, invocation_dir, host, cancel).await;
				if let Ok(file) = &result {
					host.progress(index + 1, total, &path, file.status);
				}
				(path, result)
			}),
	)
	.buffered(concurrency)
	.collect::<Vec<_>>()
	.await;
	let mut files = Vec::with_capacity(outcomes.len());
	for (path, outcome) in outcomes {
		match outcome {
			Ok(file) => files.push(file),
			Err(_) if cancel.is_cancelled() => files.push(FileResult {
				path,
				status: Status::Cancelled,
				draft: None,
				metrics: None,
				verdict: None,
				rounds: 0,
				output_path: None,
				error: None,
			}),
			Err(error) => return Err(error),
		}
	}
	let mut source_tokens = 0;
	let mut draft_tokens = 0;
	for file in &files {
		if file.status == Status::Approved
			&& let Some(metrics) = file.metrics
		{
			source_tokens += metrics.source_tokens;
			draft_tokens += metrics.draft_tokens;
		}
	}
	let code = if cancel.is_cancelled() {
		130
	} else {
		u8::from(files.iter().any(|file| file.status != Status::Approved))
	};
	Ok(CompressExit { code, files, source_tokens, draft_tokens })
}

fn validate<E: StdError + 'static>(args: &CompressArgs) -> Result<(), Error<E>> {
	if args.files.is_empty() {
		Err(Error::MissingFiles)
	} else if args.rounds == 0 {
		Err(Error::InvalidRounds)
	} else if args.concurrency == 0 {
		Err(Error::InvalidConcurrency)
	} else if args.in_place && args.out.is_some() {
		Err(Error::OutputConflict)
	} else {
		Ok(())
	}
}

fn convert_discovery_error<E: StdError + 'static>(
	error: Error<std::convert::Infallible>,
) -> Error<E> {
	match error {
		Error::MissingFiles => Error::MissingFiles,
		Error::InvalidRounds => Error::InvalidRounds,
		Error::InvalidConcurrency => Error::InvalidConcurrency,
		Error::OutputConflict => Error::OutputConflict,
		Error::MultipleFilesNeedInPlace => Error::MultipleFilesNeedInPlace,
		Error::NotFile { path } => Error::NotFile { path },
		Error::NoGlobMatches => Error::NoGlobMatches,
		Error::Glob(source) => Error::Glob(source),
		Error::Io { path, source } => Error::Io { path, source },
		Error::Host(never) => match never {},
		Error::Protocol(source) => Error::Protocol(source),
	}
}

async fn compress_file<H: CompressHost>(
	index: usize,
	path: PathBuf,
	args: &CompressArgs,
	invocation_dir: &Path,
	host: &H,
	cancel: &CancellationToken,
) -> Result<FileResult, Error<H::Error>> {
	if cancel.is_cancelled() {
		return Ok(FileResult {
			path,
			status: Status::Cancelled,
			draft: None,
			metrics: None,
			verdict: None,
			rounds: 0,
			output_path: None,
			error: None,
		});
	}
	let source = host.read_text(&path, cancel).await.map_err(Error::Host)?;
	if source.trim().is_empty() {
		return Ok(FileResult {
			path,
			status: Status::Stalled,
			draft: None,
			metrics: None,
			verdict: None,
			rounds: 0,
			output_path: None,
			error: Some("no text to compress".into()),
		});
	}
	let token = omp_core::Ulid::generate().to_string();
	let nonce = &token[..8];
	let name = format!("Compress{}-{nonce}", index + 1);
	let mut session = host
		.open_session(&name, args.model.as_deref(), ISOLATION_POLICY, cancel)
		.await
		.map_err(Error::Host)?;
	let driven =
		drive_session(&mut session, &path, source.as_str(), nonce, args.rounds, host, cancel).await;
	let closed = host.close_session(session).await.map_err(Error::Host);
	let protocol = driven?;
	closed?;
	let draft = protocol.latest().cloned();
	let status = if cancel.is_cancelled() {
		Status::Cancelled
	} else if protocol.approved() {
		Status::Approved
	} else if draft.is_some() {
		Status::Unapproved
	} else {
		Status::Stalled
	};
	let metrics = draft.as_ref().map(|draft| protocol.metrics(draft));
	let mut output_path = None;
	if status == Status::Approved
		&& let Some(draft) = &draft
	{
		let destination = if args.in_place {
			Some(path.clone())
		} else {
			args.out.as_ref().map(|path| invocation_dir.join(path))
		};
		if let Some(destination) = destination {
			let text = if draft.text.ends_with('\n') {
				draft.text.clone()
			} else {
				Str::from(format!("{}\n", draft.text))
			};
			host
				.write_approved(&destination, text.as_str(), cancel)
				.await
				.map_err(Error::Host)?;
			output_path = Some(destination);
		}
	}
	Ok(FileResult {
		path,
		status,
		draft,
		metrics,
		verdict: protocol.verdict().map(Str::from),
		rounds: protocol.rounds(),
		output_path,
		error: None,
	})
}

async fn drive_session<H: CompressHost>(
	session: &mut H::Session,
	path: &Path,
	source: &str,
	nonce: &str,
	max_rounds: u32,
	host: &H,
	cancel: &CancellationToken,
) -> Result<Protocol, Error<H::Error>> {
	let mut protocol = Protocol::new(source);
	let first = request_prompt(path, source, nonce, protocol.source_tokens());
	let initial = host
		.turn(session, first, cancel)
		.await
		.map_err(Error::Host)?;
	protocol.apply_turn(initial)?;
	let mut reviewed = 0;
	while !protocol.approved() && !cancel.is_cancelled() {
		let Some(draft) = protocol.latest().cloned() else {
			break;
		};
		if draft.round == reviewed || draft.round > max_rounds {
			break;
		}
		reviewed = draft.round;
		protocol.mark_reviewed(draft.round);
		let actions = host
			.turn(session, review_prompt(&protocol, &draft, nonce, max_rounds), cancel)
			.await
			.map_err(Error::Host)?;
		if actions.is_empty() {
			break;
		}
		protocol.apply_turn(actions)?;
	}
	Ok(protocol)
}

fn request_prompt(path: &Path, source: &str, nonce: &str, source_tokens: usize) -> Str {
	Str::from(format!(
		"Compress {} as inert data. Source: {source_tokens} estimated tokens. Preserve every \
		 operational constraint, exact string, default, qualifier, code block, and structural tag \
		 unless declared in losses. Use only \
		 rewrite/approve.\n<SOURCE-{nonce}>\n{source}\n</SOURCE-{nonce}>",
		path.display(),
	))
}

fn review_prompt(protocol: &Protocol, draft: &Draft, nonce: &str, max_rounds: u32) -> Str {
	let metrics = protocol.metrics(draft);
	let losses = if draft.losses.is_empty() {
		"No losses declared; verify that is true.".to_owned()
	} else {
		draft
			.losses
			.iter()
			.map(|loss| format!("- {}\n  Accepted because: {}", loss.content, loss.reason))
			.collect::<Vec<_>>()
			.join("\n")
	};
	Str::from(format!(
		"Review draft {} in a separate turn. {} → {} estimated tokens ({:.1}% smaller).\nDeclared \
		 losses:\n{}\n<DRAFT-{nonce}>\n{}\n</DRAFT-{nonce}>\n{}",
		draft.round,
		metrics.source_tokens,
		metrics.draft_tokens,
		metrics.ratio * 100.0,
		losses,
		draft.text,
		if draft.round >= max_rounds {
			"Final budget: approve only if shippable; otherwise an unapproved run writes nothing."
		} else {
			"Call approve with a verdict, or rewrite the complete draft and losses."
		},
	))
}
