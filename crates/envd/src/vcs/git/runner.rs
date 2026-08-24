//! Hardened asynchronous system-Git execution through Environment authority.

use std::{
	io,
	path::{Path, PathBuf},
	str,
	time::Duration,
};

use bytes::{Bytes, BytesMut};
use omp_proto::env::v1::{
	EnvironmentDelta, ExecOutcome, ExecRequest, OpenSessionRequest, OutputChannel, Script,
};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::exec::{ExecError, ExecEvent, ExecHost};

pub(super) const OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
const LOCAL_DEADLINE: Duration = Duration::from_secs(5 * 60);
const NETWORK_DEADLINE: Duration = Duration::from_secs(30 * 60);
pub(super) const TRUNCATION_MARKER: &[u8] = b"\n[git subprocess output truncated after 8 MiB]\n";
const MISSING_MARKER: &[u8] = b"__OMP_SYSTEM_GIT_MISSING__";
const SYSTEM_GIT: &str = "git";

/// Deadline class selected from the effects of a Git command.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum GitDeadline {
	/// Local repository plumbing and mutation.
	#[default]
	Local,
	/// Clone, fetch, push, or another network transfer.
	Network,
}

impl GitDeadline {
	const fn duration(self) -> Duration {
		match self {
			Self::Local => LOCAL_DEADLINE,
			Self::Network => NETWORK_DEADLINE,
		}
	}
}

/// Policy attached to one Git invocation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GitRunOptions {
	/// Suppress Git's optional lock acquisition for a read-only command.
	pub read_only:       bool,
	/// Reject output if either independently capped stream was truncated.
	pub parse_sensitive: bool,
	/// Select the finite local or network deadline.
	pub deadline:        GitDeadline,
}

/// Why an exit-code 127 result was produced.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitDiagnostic {
	/// The Environment could not find the system `git` executable.
	GitMissing,
}

/// Complete bounded output from one Git process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitRunOutput {
	/// Process exit status.
	pub exit_code:        i32,
	/// Captured standard output, including a truncation marker when applicable.
	pub stdout:           Bytes,
	/// Captured standard error, including a truncation marker when applicable.
	pub stderr:           Bytes,
	/// Whether standard output exceeded 8 MiB.
	pub stdout_truncated: bool,
	/// Whether standard error exceeded 8 MiB.
	pub stderr_truncated: bool,
	/// Stable launch diagnostic, separate from Git's own stderr.
	pub diagnostic:       Option<GitDiagnostic>,
}

/// Failures before a complete Git command result exists.
#[derive(Debug, thiserror::Error)]
pub enum GitRunError {
	/// The selected working directory was deleted or never existed.
	#[error("Git working directory does not exist: {path:?}")]
	DeletedWorkingDirectory {
		/// Missing directory.
		path: PathBuf,
	},
	/// The working directory could not be represented as a local URI.
	#[error("Git working directory cannot be represented as a file URI: {path:?}")]
	InvalidWorkingDirectory {
		/// Invalid directory.
		path: PathBuf,
	},
	/// Environment process authority rejected or failed the invocation.
	#[error("Environment Git execution failed")]
	Environment(#[from] ExecError),
	/// The finite command deadline elapsed and Environment terminated the group.
	#[error("Git command timed out")]
	Timeout,
	/// The caller cancelled and Environment terminated the process group.
	#[error("Git command was cancelled")]
	Cancelled,
	/// A parse-sensitive caller cannot safely consume truncated bytes.
	#[error("Git command output was truncated and is incomplete")]
	Incomplete {
		/// Standard output was truncated.
		stdout: bool,
		/// Standard error was truncated.
		stderr: bool,
	},
	/// Environment stopped without reporting a terminal process outcome.
	#[error("Git command ended without an exit status")]
	MissingExit,
}

/// System-Git runner backed by the project Environment process host.
#[derive(Clone)]
pub struct GitRunner {
	host: ExecHost,
}

impl GitRunner {
	/// Creates a runner using the supplied Environment process authority.
	pub const fn new(host: ExecHost) -> Self {
		Self { host }
	}

	/// Runs one fixed Git argv with sanitized ambient variables and bounded
	/// output.
	pub async fn run(
		&self,
		cwd: &Path,
		argv: &[&str],
		options: GitRunOptions,
		cancel: &CancellationToken,
	) -> Result<GitRunOutput, GitRunError> {
		self
			.run_binary_with_stdin(cwd, SYSTEM_GIT, argv, options, None, None, cancel)
			.await
	}

	/// Runs one fixed Git argv, delivering each bounded standard-output frame
	/// to `on_stdout` as Environment produces it.
	///
	/// The returned output retains the same independent 8 MiB stdout/stderr
	/// caps and terminal diagnostics as [`Self::run`]. Frames delivered to the
	/// callback never include bytes beyond the stdout cap.
	pub async fn run_stream(
		&self,
		cwd: &Path,
		argv: &[&str],
		options: GitRunOptions,
		cancel: &CancellationToken,
		on_stdout: &mut (impl FnMut(Bytes) + Send),
	) -> Result<GitRunOutput, GitRunError> {
		self
			.run_binary_with_stdin(cwd, SYSTEM_GIT, argv, options, None, Some(on_stdout), cancel)
			.await
	}

	/// Runs one fixed Git argv and streams exact bytes to its standard input.
	///
	/// Standard input is closed immediately after `input`; callers can therefore
	/// pass binary patches and commit messages without a temporary file or
	/// shell interpolation.
	pub async fn run_with_stdin(
		&self,
		cwd: &Path,
		argv: &[&str],
		options: GitRunOptions,
		input: &[u8],
		cancel: &CancellationToken,
	) -> Result<GitRunOutput, GitRunError> {
		self
			.run_binary_with_stdin(cwd, SYSTEM_GIT, argv, options, Some(input), None, cancel)
			.await
	}

	#[cfg(test)]
	pub(super) async fn run_binary(
		&self,
		cwd: &Path,
		binary: &str,
		argv: &[&str],
		options: GitRunOptions,
		cancel: &CancellationToken,
	) -> Result<GitRunOutput, GitRunError> {
		self
			.run_binary_with_stdin(cwd, binary, argv, options, None, None, cancel)
			.await
	}

	async fn run_binary_with_stdin(
		&self,
		cwd: &Path,
		binary: &str,
		argv: &[&str],
		options: GitRunOptions,
		input: Option<&[u8]>,
		mut stdout_stream: Option<&mut (dyn FnMut(Bytes) + Send)>,
		cancel: &CancellationToken,
	) -> Result<GitRunOutput, GitRunError> {
		let cwd = match tokio::fs::canonicalize(cwd).await {
			Ok(cwd) => cwd,
			Err(error) if error.kind() == io::ErrorKind::NotFound => {
				return Err(GitRunError::DeletedWorkingDirectory { path: cwd.to_path_buf() });
			},
			Err(error) => return Err(ExecError::Io(error).into()),
		};
		match tokio::fs::metadata(&cwd).await {
			Ok(metadata) if metadata.is_dir() => {},
			Ok(_) => {
				return Err(GitRunError::DeletedWorkingDirectory { path: cwd });
			},
			Err(error) if error.kind() == io::ErrorKind::NotFound => {
				return Err(GitRunError::DeletedWorkingDirectory { path: cwd });
			},
			Err(error) => return Err(ExecError::Io(error).into()),
		}
		let cwd_uri = Url::from_file_path(&cwd)
			.map_err(|()| GitRunError::InvalidWorkingDirectory { path: cwd.clone() })?;
		let opened = self
			.host
			.open_session(OpenSessionRequest {
				cwd_uri: cwd_uri.to_string(),
				env_delta: Some(git_environment()),
				..Default::default()
			})
			.await
			.map_err(|error| classify_open_error(&cwd, error))?;
		let source = command_source(binary, argv, options.read_only);
		let execution = self
			.host
			.exec(
				ExecRequest {
					session: opened.session.clone(),
					source: Some(Script { text: source, ..Default::default() }),
					..Default::default()
				},
				Some(options.deadline.duration()),
			)
			.await;
		let (_, run) = match execution {
			Ok(execution) => execution,
			Err(error) => {
				let _ = self.host.close_session(&opened.session);
				return Err(error.into());
			},
		};
		let mut stdout = CappedOutput::new();
		let mut stderr = CappedOutput::new();
		let mut cancellation_requested = false;
		let mut input = input;
		let terminal = loop {
			let event = if cancellation_requested {
				run.next_event().await
			} else {
				tokio::select! {
					biased;
					() = cancel.cancelled() => {
						run.cancel();
						cancellation_requested = true;
						continue;
					},
					event = run.next_event() => event,
				}
			};
			match event {
				Some(ExecEvent::Output(frame)) if frame.channel == OutputChannel::Stdout as i32 => {
					let streamed = stdout.remaining().min(frame.data.len());
					stdout.push(&frame.data);
					if streamed > 0
						&& let Some(on_stdout) = stdout_stream.as_deref_mut()
					{
						on_stdout(frame.data.slice(..streamed));
					}
				},
				Some(ExecEvent::Output(frame)) if frame.channel == OutputChannel::Stderr as i32 => {
					stderr.push(&frame.data);
				},
				Some(ExecEvent::Started { .. }) => {
					if let Some(input) = input.take() {
						self.host.stdin(run.id(), Some(input))?;
						self.host.stdin(run.id(), None)?;
					}
				},
				Some(ExecEvent::Output(_)) => {},
				Some(ExecEvent::Exit(exit)) => break exit.status,
				None => break None,
			}
		};
		let _ = self.host.close_session(&opened.session);
		let status = terminal.ok_or(GitRunError::MissingExit)?;
		if cancellation_requested || status.outcome == ExecOutcome::Cancelled as i32 {
			return Err(GitRunError::Cancelled);
		}
		if status.outcome == ExecOutcome::Timeout as i32 {
			return Err(GitRunError::Timeout);
		}
		let stdout_truncated = stdout.truncated;
		let stderr_truncated = stderr.truncated;
		if options.parse_sensitive && (stdout_truncated || stderr_truncated) {
			return Err(GitRunError::Incomplete {
				stdout: stdout_truncated,
				stderr: stderr_truncated,
			});
		}
		let mut stderr = stderr.finish();
		let diagnostic = if status.exit_code == Some(127) && contains_bytes(&stderr, MISSING_MARKER) {
			stderr = remove_marker(&stderr, MISSING_MARKER);
			Some(GitDiagnostic::GitMissing)
		} else {
			None
		};
		Ok(GitRunOutput {
			exit_code: status.exit_code.unwrap_or(1),
			stdout: stdout.finish(),
			stderr,
			stdout_truncated,
			stderr_truncated,
			diagnostic,
		})
	}
}

fn classify_open_error(cwd: &Path, error: ExecError) -> GitRunError {
	if !cwd.is_dir() {
		GitRunError::DeletedWorkingDirectory { path: cwd.to_path_buf() }
	} else {
		error.into()
	}
}

pub(super) fn git_environment() -> EnvironmentDelta {
	let set = [
		("GIT_OPTIONAL_LOCKS", "0"),
		("GIT_ASKPASS", "true"),
		("GIT_EDITOR", "true"),
		("GIT_TERMINAL_PROMPT", "0"),
		("EDITOR", "true"),
		("VISUAL", "true"),
		("SSH_ASKPASS", "false"),
		("LC_MESSAGES", "C"),
		("LC_CTYPE", "C.UTF-8"),
	]
	.into_iter()
	.map(|(name, value)| (name.to_owned(), value.to_owned()))
	.collect();
	let unset = [
		"GIT_DIR",
		"GIT_COMMON_DIR",
		"GIT_WORK_TREE",
		"GIT_INDEX_FILE",
		"GIT_OBJECT_DIRECTORY",
		"GIT_ALTERNATE_OBJECT_DIRECTORIES",
		"LC_ALL",
	]
	.into_iter()
	.map(str::to_owned)
	.collect();
	EnvironmentDelta { set, unset, ..Default::default() }
}

pub(super) fn command_source(binary: &str, argv: &[&str], read_only: bool) -> String {
	let mut source = String::from("if command -v ");
	push_shell_word(&mut source, binary);
	source.push_str(" >/dev/null 2>&1; then ");
	push_shell_word(&mut source, binary);
	for argument in ["-c", "core.fsmonitor=false", "-c", "core.untrackedCache=false"] {
		source.push(' ');
		push_shell_word(&mut source, argument);
	}
	if read_only {
		source.push(' ');
		push_shell_word(&mut source, "--no-optional-locks");
	}
	for argument in argv {
		source.push(' ');
		push_shell_word(&mut source, argument);
	}
	source.push_str("; else printf '%s\\n' '");
	source.push_str(str::from_utf8(MISSING_MARKER).expect("missing marker is ASCII"));
	source.push_str("' >&2; exit 127; fi");
	source
}

fn push_shell_word(output: &mut String, value: &str) {
	output.push('\'');
	let mut fragments = value.split('\'');
	if let Some(first) = fragments.next() {
		output.push_str(first);
	}
	for fragment in fragments {
		output.push_str("'\"'\"'");
		output.push_str(fragment);
	}
	output.push('\'');
}

pub(super) struct CappedOutput {
	bytes:                BytesMut,
	pub(super) truncated: bool,
}

impl CappedOutput {
	pub(super) fn new() -> Self {
		Self { bytes: BytesMut::with_capacity(16 * 1024), truncated: false }
	}

	pub(super) fn push(&mut self, bytes: &[u8]) {
		if self.truncated {
			return;
		}
		let remaining = OUTPUT_LIMIT.saturating_sub(self.bytes.len());
		if bytes.len() <= remaining {
			self.bytes.extend_from_slice(bytes);
			return;
		}
		self.bytes.extend_from_slice(&bytes[..remaining]);
		self.bytes.extend_from_slice(TRUNCATION_MARKER);
		self.truncated = true;
	}
	fn remaining(&self) -> usize {
		if self.truncated {
			0
		} else {
			OUTPUT_LIMIT.saturating_sub(self.bytes.len())
		}
	}

	pub(super) fn finish(self) -> Bytes {
		self.bytes.freeze()
	}
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
	haystack
		.windows(needle.len())
		.any(|window| window == needle)
}

fn remove_marker(bytes: &[u8], marker: &[u8]) -> Bytes {
	let Some(position) = bytes
		.windows(marker.len())
		.position(|window| window == marker)
	else {
		return Bytes::copy_from_slice(bytes);
	};
	let mut clean = BytesMut::with_capacity(bytes.len() - marker.len());
	clean.extend_from_slice(&bytes[..position]);
	clean.extend_from_slice(&bytes[position + marker.len()..]);
	clean.freeze()
}
