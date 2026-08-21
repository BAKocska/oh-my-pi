//! Supervised child-process transport for persistent Python eval sessions.

use std::{
	collections::{BTreeMap, HashMap},
	ffi::{OsStr, OsString},
	future::Future,
	io::{self, Write as _},
	path::{Path, PathBuf},
	process::Stdio,
	sync::{
		Arc,
		atomic::{AtomicBool, AtomicU64, Ordering},
	},
	time::{Duration, Instant},
};

use async_trait::async_trait;
use bytes::Bytes;
use omp_core::{CowBytes, Duration as OmpDuration, DurationError, Str, sf};
use omp_tool::BlobRef;
use omp_tools::eval::{
	CellOutcome, CellStatus, CellValue, DisplayOutput, EvalExec, EvalRun, Fault, OutputChannel,
	PythonException, RunCompletion, RunEvent, RunRequest, Session, Update,
	idle_timeout::TimeoutHandle, kernel::EmbeddedPython,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::{
	io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
	process::{Child, ChildStdin, ChildStdout, Command},
	sync::{Mutex as AsyncMutex, oneshot},
};
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use super::{
	super::blobs::BlobHost,
	bridge::{
		BridgeCapabilities, BridgeHost, BridgeHostError, BridgeNamespaceInstaller,
		ChildBridgeTransport, EvalSessionConfig, SessionBridgeHost,
	},
};

/// Private argv selector used to re-enter `omp` as an eval kernel child.
pub const EVAL_CHILD_ARG: &str = "__omp-eval-child";

const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
const CHILD_TIMEOUT_EXIT: i32 = 124;
const SECRET_MARKERS: &[&str] =
	&["TOKEN", "SECRET", "PASSWORD", "PASSWD", "API_KEY", "PRIVATE_KEY", "CREDENTIAL"];
const OUTPUT_SPILL_THRESHOLD: usize = 128 * 1024;

/// Production [`EvalExec`] that owns one killable same-binary child per
/// session.
#[derive(Clone)]
pub struct ProcessEvalExec {
	inner: Arc<ProcessEvalInner>,
}

struct ProcessEvalInner {
	executable:      PathBuf,
	interpreter:     PathBuf,
	host:            Arc<SessionBridgeHost>,
	blobs:           Option<BlobHost>,
	interrupt_grace: OmpDuration,
	sessions:        Mutex<HashMap<Bytes, Arc<ProcessSession>>>,
	next_cell:       AtomicU64,
}

/// Stable supervisor identity for one Python kernel.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct KernelKey {
	/// Tool-owner session namespace.
	pub session:     Bytes,
	/// Canonical working directory inherited by the kernel.
	pub cwd:         PathBuf,
	/// Interpreter identity selected for this kernel.
	pub interpreter: PathBuf,
}

struct ProcessSession {
	id:          Bytes,
	key:         KernelKey,
	child:       AsyncMutex<Option<EvalChild>>,
	run_gate:    Arc<AsyncMutex<()>>,
	needs_reset: AtomicBool,
}

/// Active cell in a process-backed Python session.
pub struct ProcessEvalRun {
	events:          flume::Receiver<Result<RunEvent, Fault>>,
	cancelled:       CancellationToken,
	terminal:        bool,
	effective_reset: bool,
}

impl ProcessEvalExec {
	/// Resolves the real `omp` executable and constructs the production
	/// Python executor.
	pub fn production(
		host: Arc<SessionBridgeHost>,
		interrupt_grace: OmpDuration,
		blobs: BlobHost,
	) -> Result<Self, io::Error> {
		let executable = resolve_omp_executable()?;
		let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
		let explicit = std::env::var_os("OMP_PYTHON_INTERPRETER");
		let interpreter = discover_external_python(&cwd, explicit.as_deref())
			.unwrap_or_else(|| PathBuf::from("embedded:cpython-3.14t"));
		Ok(Self::new_inner(executable, interpreter, host, interrupt_grace, Some(blobs)))
	}

	fn new_inner(
		executable: PathBuf,
		interpreter: PathBuf,
		host: Arc<SessionBridgeHost>,
		interrupt_grace: OmpDuration,
		blobs: Option<BlobHost>,
	) -> Self {
		Self {
			inner: Arc::new(ProcessEvalInner {
				executable,
				interpreter,
				host,
				blobs,
				interrupt_grace,
				sessions: Mutex::new(HashMap::new()),
				next_cell: AtomicU64::new(1),
			}),
		}
	}
}

impl EvalExec for ProcessEvalExec {
	type Run = ProcessEvalRun;

	fn open_session(&self) -> impl Future<Output = Result<Session, Fault>> + Send + '_ {
		let id = Bytes::from(format!("py-process-{}", Ulid::generate()));
		let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
		let key = KernelKey { session: id.clone(), cwd, interpreter: self.inner.interpreter.clone() };
		self.inner.sessions.lock().insert(
			id.clone(),
			Arc::new(ProcessSession {
				id: id.clone(),
				key,
				child: AsyncMutex::new(None),
				run_gate: Arc::new(AsyncMutex::new(())),
				needs_reset: AtomicBool::new(false),
			}),
		);
		std::future::ready(Ok(Session { id }))
	}

	fn run<'a>(
		&'a self,
		session: &'a Session,
		request: RunRequest,
	) -> impl Future<Output = Result<Self::Run, Fault>> + Send + 'a {
		self.start_run(session, request, false)
	}

	fn run_with_mode<'a>(
		&'a self,
		session: &'a Session,
		request: RunRequest,
		disposable: bool,
	) -> impl Future<Output = Result<Self::Run, Fault>> + Send + 'a {
		self.start_run(session, request, disposable)
	}

	fn dispose_session(
		&self,
		session: &Session,
	) -> impl Future<Output = Result<(), Fault>> + Send + '_ {
		let owned = self.inner.sessions.lock().get(&session.id).cloned();
		async move {
			if let Some(owned) = owned {
				owned.needs_reset.store(true, Ordering::Release);
				if let Some(mut child) = owned.child.lock().await.take() {
					child.terminate().await;
				}
			}
			Ok(())
		}
	}

	fn dispose_all(&self) {
		let sessions = self
			.inner
			.sessions
			.lock()
			.values()
			.cloned()
			.collect::<Vec<_>>();
		for owned in sessions {
			owned.needs_reset.store(true, Ordering::Release);
			if let Ok(runtime) = tokio::runtime::Handle::try_current() {
				runtime.spawn(async move {
					if let Some(mut child) = owned.child.lock().await.take() {
						child.terminate().await;
					}
				});
			}
		}
	}
}

impl ProcessEvalExec {
	async fn start_run(
		&self,
		session: &Session,
		mut request: RunRequest,
		disposable: bool,
	) -> Result<ProcessEvalRun, Fault> {
		let owned = {
			let sessions = self.inner.sessions.lock();
			sessions.get(&session.id).cloned()
		}
		.ok_or_else(|| Fault::SessionLost {
			message: sf!("unknown supervised Python process session"),
		})?;
		let gate = Arc::clone(&owned.run_gate).lock_owned().await;
		let forced_reset = owned.needs_reset.swap(false, Ordering::AcqRel);
		request.reset |= forced_reset || disposable;
		let effective_reset = request.reset;
		let number = self.inner.next_cell.fetch_add(1, Ordering::Relaxed);
		let cell_id =
			Bytes::from(format!("{}:cell-{number}", String::from_utf8_lossy(session.id.as_ref())));
		let (events_tx, events) = flume::unbounded();
		let cancelled = CancellationToken::new();
		let task_cancelled = cancelled.clone();
		let executable = self.inner.executable.clone();
		let host = Arc::clone(&self.inner.host);
		let blobs = self.inner.blobs.clone();
		let interrupt_grace = self.inner.interrupt_grace;
		tokio::spawn(async move {
			let _gate = gate;
			if task_cancelled.is_cancelled() {
				owned.needs_reset.store(true, Ordering::Release);
				return;
			}
			let mut child_slot = owned.child.lock().await;
			if child_slot.as_mut().is_some_and(|child| !child.is_alive()) {
				child_slot.take();
				request.reset = false;
			}
			if request.reset
				&& let Some(mut stale) = child_slot.take()
			{
				stale.terminate().await;
			}
			if child_slot.is_none() {
				match EvalChild::spawn(
					&executable,
					&owned.id,
					&owned.key.cwd,
					Arc::clone(&host),
					interrupt_grace,
				)
				.await
				{
					Ok(child) => *child_slot = Some(child),
					Err(error) => {
						owned.needs_reset.store(true, Ordering::Release);
						let _ = events_tx.send(Err(resource_fault("open_session", error)));
						return;
					},
				}
			}
			if request.reset {
				request.reset = false;
			}
			let child = child_slot.as_mut().expect("eval child initialized above");
			let keep = child
				.run_cell(cell_id, request, task_cancelled, &events_tx, host, &owned.needs_reset, blobs)
				.await && !disposable;
			if !keep {
				child.terminate().await;
				*child_slot = None;
				owned.needs_reset.store(!disposable, Ordering::Release);
			}
		});
		Ok(ProcessEvalRun { events, cancelled, terminal: false, effective_reset })
	}
}

impl EvalRun for ProcessEvalRun {
	async fn next_event(&mut self) -> Result<Option<RunEvent>, Fault> {
		match self.events.recv_async().await {
			Ok(Ok(event)) => {
				if matches!(event, RunEvent::Completed(_)) {
					self.terminal = true;
				}
				Ok(Some(event))
			},
			Ok(Err(error)) => {
				self.terminal = true;
				Err(error)
			},
			Err(_) => Ok(None),
		}
	}

	fn cancel(&self) -> impl Future<Output = Result<(), Fault>> + Send + '_ {
		self.cancelled.cancel();
		std::future::ready(Ok(()))
	}

	fn reset(&self) -> bool {
		self.effective_reset
	}
}

struct OutputSpill {
	host:        Option<BlobHost>,
	buffered:    Vec<u8>,
	stage:       Option<omp_storage::blob::BlobStage>,
	total_lines: usize,
	total_bytes: usize,
}

impl OutputSpill {
	fn new(host: Option<BlobHost>) -> Self {
		Self {
			host,
			buffered: Vec::with_capacity(OUTPUT_SPILL_THRESHOLD.min(64 * 1024)),
			stage: None,
			total_lines: 0,
			total_bytes: 0,
		}
	}

	fn push(&mut self, data: &[u8]) -> Result<(), ProcessError> {
		self.total_bytes = self.total_bytes.saturating_add(data.len());
		self.total_lines = self
			.total_lines
			.saturating_add(bytecount::count(data, b'\n'));
		if let Some(stage) = self.stage.as_mut() {
			stage
				.write_all(data)
				.map_err(|error| ProcessError::Spill(Str::from(error.to_string())))?;
			return Ok(());
		}
		if self.buffered.len().saturating_add(data.len()) <= OUTPUT_SPILL_THRESHOLD {
			self.buffered.extend_from_slice(data);
			return Ok(());
		}
		let Some(host) = self.host.as_ref() else {
			self.buffered.clear();
			return Ok(());
		};
		let mut stage = host
			.begin_spill()
			.map_err(|error| ProcessError::Spill(Str::from(error.to_string())))?;
		stage
			.write_all(&self.buffered)
			.and_then(|()| stage.write_all(data))
			.map_err(|error| ProcessError::Spill(Str::from(error.to_string())))?;
		self.buffered.clear();
		self.stage = Some(stage);
		Ok(())
	}

	async fn finish(self) -> Result<Option<BlobRef>, ProcessError> {
		let Some(stage) = self.stage else {
			return Ok(None);
		};
		let reference = tokio::task::spawn_blocking(move || stage.finish())
			.await
			.map_err(|error| ProcessError::Spill(Str::from(error.to_string())))?
			.map_err(|error| ProcessError::Spill(Str::from(error.to_string())))?;
		let hash = reference.hash.to_hex();
		Ok(Some(BlobRef {
			hash:       Str::from(hash.as_str()),
			media_type: sf!("text/plain; charset=utf-8"),
			byte_len:   reference.size,
		}))
	}
}

struct EvalChild {
	child:           Child,
	stdin:           ChildStdin,
	stdout:          BufReader<ChildStdout>,
	token:           Str,
	next_run:        AtomicU64,
	process_group:   Option<u32>,
	interrupt_grace: Duration,
}

impl EvalChild {
	async fn spawn(
		executable: &Path,
		session_id: &Bytes,
		cwd: &Path,
		host: Arc<SessionBridgeHost>,
		interrupt_grace: OmpDuration,
	) -> Result<Self, ProcessError> {
		let interrupt_grace_std = interrupt_grace.to_std()?;
		let capabilities = host.capabilities()?.allowed_names();
		let config = host.session_config().map(WireSessionConfig::from);
		let token = Str::from(Ulid::generate().to_string());
		let mut command = Command::new(executable);
		command
			.arg(EVAL_CHILD_ARG)
			.current_dir(cwd)
			.env_clear()
			.envs(sanitized_spawn_env())
			.env("PYTHONUNBUFFERED", "1")
			.env("PYTHONIOENCODING", "utf-8")
			.env("MPLBACKEND", "Agg")
			.env("OMP_EVAL_SESSION", String::from_utf8_lossy(session_id.as_ref()).as_ref())
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::inherit())
			.kill_on_drop(true);
		#[cfg(unix)]
		{
			use std::os::unix::process::CommandExt;
			command.as_std_mut().process_group(0);
		}
		#[cfg(windows)]
		{
			use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;
			command.creation_flags(CREATE_NEW_PROCESS_GROUP);
		}
		let mut child = command.spawn()?;
		let process_group = child.id();
		let stdin = child
			.stdin
			.take()
			.ok_or_else(|| ProcessError::Protocol(sf!("eval child stdin unavailable")))?;
		let stdout = child
			.stdout
			.take()
			.ok_or_else(|| ProcessError::Protocol(sf!("eval child stdout unavailable")))?;
		let mut process = Self {
			child,
			stdin,
			stdout: BufReader::new(stdout),
			token: token.clone(),
			next_run: AtomicU64::new(1),
			process_group,
			interrupt_grace: interrupt_grace_std,
		};
		write_frame(&mut process.stdin, &ParentFrame::Init {
			token,
			session_id: session_id.clone(),
			capabilities,
			config,
			interrupt_grace: Str::from(interrupt_grace.to_string()),
		})
		.await?;
		match tokio::time::timeout(Duration::from_secs(5), read_frame(&mut process.stdout)).await {
			Ok(Ok(Some(ChildFrame::Ready))) => Ok(process),
			Ok(Ok(Some(ChildFrame::Fatal { message }))) => Err(ProcessError::Protocol(message)),
			Ok(Ok(Some(_))) => {
				Err(ProcessError::Protocol(sf!("eval child did not send Ready as its first frame",)))
			},
			Ok(Ok(None)) => Err(ProcessError::Exited),
			Ok(Err(error)) => Err(error),
			Err(_) => Err(ProcessError::Protocol(sf!("eval child startup timed out"))),
		}
	}

	async fn run_cell(
		&mut self,
		cell_id: Bytes,
		request: RunRequest,
		cancelled: CancellationToken,
		events: &flume::Sender<Result<RunEvent, Fault>>,
		host: Arc<SessionBridgeHost>,
		needs_reset: &AtomicBool,
		blobs: Option<BlobHost>,
	) -> bool {
		let run_id = self.next_run.fetch_add(1, Ordering::Relaxed);
		let started = Instant::now();
		let timeout = TimeoutHandle::new(request.timeout);
		let Ok(timeout_ns) = request
			.timeout
			.map(|duration| u64::try_from(duration.as_nanos()))
			.transpose()
		else {
			let _ = events
				.send(Err(resource_fault("run", ProcessError::Duration(DurationError::Overflow))));
			return false;
		};
		if let Err(error) = write_frame(&mut self.stdin, &ParentFrame::Run {
			run_id,
			cell_id: cell_id.clone(),
			code: request.code,
			timeout_ns,
			reset: request.reset,
		})
		.await
		{
			needs_reset.store(true, Ordering::Release);
			let _ = events.send(Err(session_lost(error)));
			return false;
		}

		let mut result = None;
		let mut display_outputs = Vec::new();
		let mut exception = None;
		let mut spill = OutputSpill::new(blobs);
		let mut wire_sequence = 0_u64;
		loop {
			let frame = tokio::select! {
				() = cancelled.cancelled() => {
					needs_reset.store(true, Ordering::Release);
					timeout.dispose();
					self.interrupt();
					tokio::time::sleep(self.interrupt_grace).await;
					let _ = events.send(Ok(RunEvent::Completed(cancelled_completion(
						elapsed_ms(started),
					))));
					return false;
				},
				() = timeout.expired() => {
					needs_reset.store(true, Ordering::Release);
					self.interrupt();
					tokio::time::sleep(self.interrupt_grace).await;
					let _ = events.send(Ok(RunEvent::Completed(timeout_completion(elapsed_ms(started)))));
					return false;
				},
				frame = read_frame(&mut self.stdout) => frame,
			};
			let frame = match frame {
				Ok(Some(frame)) => frame,
				Ok(None) | Err(ProcessError::Exited) => {
					needs_reset.store(true, Ordering::Release);
					if self
						.child
						.try_wait()
						.ok()
						.flatten()
						.and_then(|status| status.code())
						== Some(CHILD_TIMEOUT_EXIT)
					{
						let _ =
							events.send(Ok(RunEvent::Completed(timeout_completion(elapsed_ms(started)))));
					} else {
						let _ = events.send(Err(Fault::SessionLost {
							message: sf!("Python eval child exited during the active cell"),
						}));
					}
					return false;
				},
				Err(error) => {
					needs_reset.store(true, Ordering::Release);
					let _ = events.send(Err(session_lost(error)));
					return false;
				},
			};
			match frame {
				ChildFrame::Started { run_id: actual, cell_id: actual_cell }
					if actual == run_id && actual_cell == cell_id =>
				{
					let _ = events.send(Ok(RunEvent::Started { cell_id: actual_cell }));
				},
				ChildFrame::Stdout { run_id: actual, mut update }
				| ChildFrame::Stderr { run_id: actual, mut update }
					if actual == run_id =>
				{
					if let Err(error) = spill.push(update.data.as_ref()) {
						let _ = events.send(Err(resource_fault("spill_output", error)));
						return false;
					}
					update.sequence = wire_sequence;
					wire_sequence = wire_sequence.saturating_add(1);
					let _ = events.send(Ok(RunEvent::Output(update)));
				},
				ChildFrame::Display { run_id: actual, output } if actual == run_id => {
					display_outputs.push(output);
				},
				ChildFrame::Result { run_id: actual, value } if actual == run_id => {
					result = Some(value);
				},
				ChildFrame::Error { run_id: actual, value } if actual == run_id => {
					exception = Some(value);
				},
				ChildFrame::Done {
					run_id: actual,
					mut status,
					truncated,
					spilled_output,
					total_lines,
					total_bytes,
				} if actual == run_id => {
					timeout.dispose();
					status.exception = exception;
					let spill_total_lines = spill.total_lines;
					let spill_total_bytes = spill.total_bytes;
					let spilled = match spill.finish().await {
						Ok(value) => value,
						Err(error) => {
							let _ = events.send(Err(resource_fault("spill_output", error)));
							return false;
						},
					};
					let _ = events.send(Ok(RunEvent::Completed(RunCompletion {
						status,
						result,
						display_outputs,
						truncated: truncated || spilled.is_some(),
						spilled_output: spilled.or(spilled_output),
						total_lines: total_lines.max(spill_total_lines),
						total_bytes: total_bytes.max(spill_total_bytes),
					})));
					return true;
				},
				ChildFrame::BridgeCall { run_id: actual, request_id, token, name, args }
					if actual == run_id && token == self.token =>
				{
					match host.capabilities() {
						Ok(value) if value.allows(name.as_str()) => {},
						Ok(_) => {
							if write_frame(&mut self.stdin, &ParentFrame::BridgeResponse {
								request_id,
								value: None,
								error: Some(Str::from(format!("bridge capability denied: {name}"))),
							})
							.await
							.is_err()
							{
								needs_reset.store(true, Ordering::Release);
								return false;
							}
							continue;
						},
						Err(error) => {
							if write_frame(&mut self.stdin, &ParentFrame::BridgeResponse {
								request_id,
								value: None,
								error: Some(Str::from(error.to_string())),
							})
							.await
							.is_err()
							{
								needs_reset.store(true, Ordering::Release);
								return false;
							}
							continue;
						},
					}
					let call = timeout.host_wait(host.call(name.as_str(), args));
					tokio::pin!(call);
					let response = tokio::select! {
						() = cancelled.cancelled() => {
							needs_reset.store(true, Ordering::Release);
							timeout.dispose();
							self.interrupt();
							let _ = events.send(Ok(RunEvent::Completed(cancelled_completion(
								elapsed_ms(started),
							))));
							return false;
						},
						result = &mut call => result,
					};
					let (value, error) = match response {
						Ok(value) => (Some(value), None),
						Err(error) => (None, Some(Str::from(error.to_string()))),
					};
					if write_frame(&mut self.stdin, &ParentFrame::BridgeResponse {
						request_id,
						value,
						error,
					})
					.await
					.is_err()
					{
						needs_reset.store(true, Ordering::Release);
						let _ = events.send(Err(Fault::SessionLost {
							message: sf!("Python eval child exited during a host bridge response",),
						}));
						return false;
					}
				},
				ChildFrame::Fatal { message } => {
					needs_reset.store(true, Ordering::Release);
					let _ = events.send(Err(Fault::SessionLost { message }));
				},
				_ => {
					needs_reset.store(true, Ordering::Release);
					let _ = events.send(Err(Fault::SessionLost {
						message: sf!("Python eval child sent an invalid or out-of-order frame",),
					}));

					return false;
				},
			}
		}
	}

	fn is_alive(&mut self) -> bool {
		self.child.try_wait().is_ok_and(|status| status.is_none())
	}

	fn interrupt(&self) {
		#[cfg(unix)]
		if let Some(pid) = self.process_group {
			let _ = nix::sys::signal::killpg(
				nix::unistd::Pid::from_raw(pid.cast_signed()),
				nix::sys::signal::Signal::SIGINT,
			);
		}
		#[cfg(windows)]
		if let Some(pid) = self.process_group {
			unsafe {
				let _ = windows_sys::Win32::System::Console::GenerateConsoleCtrlEvent(
					windows_sys::Win32::System::Console::CTRL_BREAK_EVENT,
					pid,
				);
			}
		}
	}

	async fn terminate(&mut self) {
		let _ = write_frame(&mut self.stdin, &ParentFrame::Exit).await;
		if tokio::time::timeout(self.interrupt_grace, self.child.wait())
			.await
			.is_ok_and(|status| status.is_ok())
		{
			self.process_group.take();
			return;
		}
		let pid = self.process_group.take();
		#[cfg(unix)]
		if let Some(pid) = pid {
			let _ = nix::sys::signal::killpg(
				nix::unistd::Pid::from_raw(pid.cast_signed()),
				nix::sys::signal::Signal::SIGTERM,
			);
		}
		#[cfg(windows)]
		if pid.is_some() {
			let _ = self.child.start_kill();
		}
		if tokio::time::timeout(self.interrupt_grace, self.child.wait())
			.await
			.is_ok_and(|status| status.is_ok())
		{
			return;
		}
		#[cfg(unix)]
		if let Some(pid) = pid {
			let _ = nix::sys::signal::killpg(
				nix::unistd::Pid::from_raw(pid.cast_signed()),
				nix::sys::signal::Signal::SIGKILL,
			);
		}
		#[cfg(windows)]
		{
			let _ = self.child.start_kill();
		}
		let _ = self.child.wait().await;
	}
}
impl Drop for EvalChild {
	fn drop(&mut self) {
		let pid = self.process_group;
		#[cfg(unix)]
		if let Some(pid) = pid {
			let _ = nix::sys::signal::killpg(
				nix::unistd::Pid::from_raw(pid.cast_signed()),
				nix::sys::signal::Signal::SIGKILL,
			);
		}
		#[cfg(windows)]
		{
			let _ = self.child.start_kill();
		}
	}
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ParentFrame {
	Init {
		token:           Str,
		session_id:      Bytes,
		capabilities:    Vec<Str>,
		config:          Option<WireSessionConfig>,
		interrupt_grace: Str,
	},
	Run {
		run_id:     u64,
		cell_id:    Bytes,
		code:       Str,
		timeout_ns: Option<u64>,
		reset:      bool,
	},
	BridgeResponse {
		request_id: u64,
		value:      Option<Value>,
		error:      Option<Str>,
	},
	Exit,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ChildFrame {
	Ready,
	Started {
		run_id:  u64,
		cell_id: Bytes,
	},
	Stdout {
		run_id: u64,
		update: Update,
	},
	Stderr {
		run_id: u64,
		update: Update,
	},
	Display {
		run_id: u64,
		output: DisplayOutput,
	},
	Result {
		run_id: u64,
		value:  CellValue,
	},
	Error {
		run_id: u64,
		value:  PythonException,
	},
	Done {
		run_id:         u64,
		status:         CellStatus,
		truncated:      bool,
		spilled_output: Option<BlobRef>,
		total_lines:    usize,
		total_bytes:    usize,
	},
	BridgeCall {
		run_id:     u64,
		request_id: u64,
		token:      Str,
		name:       Str,
		args:       Value,
	},
	Fatal {
		message: Str,
	},
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WireSessionConfig {
	local_roots_json: Str,
	artifacts_dir:    Str,
	session_file:     Str,
}

impl From<EvalSessionConfig> for WireSessionConfig {
	fn from(config: EvalSessionConfig) -> Self {
		Self {
			local_roots_json: config.local_roots_json,
			artifacts_dir:    config.artifacts_dir,
			session_file:     config.session_file,
		}
	}
}

impl From<WireSessionConfig> for EvalSessionConfig {
	fn from(config: WireSessionConfig) -> Self {
		Self {
			local_roots_json: config.local_roots_json,
			artifacts_dir:    config.artifacts_dir,
			session_file:     config.session_file,
		}
	}
}

struct ChildBridgeHost {
	token:        Str,
	capabilities: BridgeCapabilities,
	config:       Option<EvalSessionConfig>,
	outgoing:     flume::Sender<ChildFrame>,
	pending:      Mutex<BTreeMap<u64, oneshot::Sender<Result<Value, Str>>>>,
	next_request: AtomicU64,
	active_run:   AtomicU64,
}

impl ChildBridgeHost {
	fn resolve(&self, request_id: u64, result: Result<Value, Str>) {
		let pending = self.pending.lock().remove(&request_id);
		if let Some(pending) = pending {
			let _ = pending.send(result);
		}
	}
}

#[async_trait]
impl ChildBridgeTransport for ChildBridgeHost {
	fn capabilities(&self) -> BridgeCapabilities {
		self.capabilities.clone()
	}

	fn session_config(&self) -> Option<EvalSessionConfig> {
		self.config.clone()
	}

	async fn call(&self, name: &str, args: Value) -> Result<Value, BridgeHostError> {
		if !self.capabilities.allows(name) {
			return Err(BridgeHostError::message(format!("bridge capability denied: {name}")));
		}
		let request_id = self.next_request.fetch_add(1, Ordering::Relaxed);
		let run_id = self.active_run.load(Ordering::Acquire);
		let (sender, receiver) = oneshot::channel();
		self.pending.lock().insert(request_id, sender);
		if self
			.outgoing
			.send(ChildFrame::BridgeCall {
				run_id,
				request_id,
				token: self.token.clone(),
				name: Str::from(name),
				args,
			})
			.is_err()
		{
			self.pending.lock().remove(&request_id);
			return Err(BridgeHostError::message("eval parent bridge disconnected"));
		}
		receiver
			.await
			.map_err(|_| BridgeHostError::message("eval parent bridge response was dropped"))?
			.map_err(BridgeHostError::message)
	}
}

type ProtocolInput = Box<dyn AsyncRead + Unpin + Send>;
type ProtocolOutput = Box<dyn AsyncWrite + Unpin + Send>;
#[cfg(unix)]
type ProtocolCapture = Option<(std::fs::File, std::fs::File)>;
#[cfg(not(unix))]
type ProtocolCapture = ();

struct ShieldedProtocol {
	input:   ProtocolInput,
	output:  ProtocolOutput,
	capture: ProtocolCapture,
}
#[cfg(unix)]
fn shield_protocol_fds() -> io::Result<ShieldedProtocol> {
	use std::os::fd::{AsRawFd, FromRawFd};

	fn duplicate(fd: libc::c_int) -> io::Result<libc::c_int> {
		// SAFETY: `dup` only borrows the valid process fd and returns a new fd.
		let duplicate = unsafe { libc::dup(fd) };
		if duplicate < 0 {
			return Err(io::Error::last_os_error());
		}
		// Protocol duplicates must never leak into subprocesses spawned by cells.
		// SAFETY: the duplicate is owned here and `F_SETFD` does not access memory.
		if unsafe { libc::fcntl(duplicate, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
			// SAFETY: the duplicate is owned here.
			unsafe { libc::close(duplicate) };
			return Err(io::Error::last_os_error());
		}
		Ok(duplicate)
	}

	fn pipe() -> io::Result<[libc::c_int; 2]> {
		let mut descriptors = [-1; 2];
		// SAFETY: `descriptors` points to two writable integers.
		if unsafe { libc::pipe(descriptors.as_mut_ptr()) } < 0 {
			return Err(io::Error::last_os_error());
		}
		Ok(descriptors)
	}

	let protocol_in = duplicate(libc::STDIN_FILENO)?;
	let protocol_out = duplicate(libc::STDOUT_FILENO)?;
	let stdout_pipe = pipe()?;
	let stderr_pipe = pipe()?;
	let null = std::fs::File::open("/dev/null")?;
	// Preserve private protocol duplicates, then make fd 0 inert and route all
	// native/user child output into capture drains. This prevents `input()` and
	// `os.write(1, ...)` from consuming or spoofing protocol frames.
	// SAFETY: every source and destination is a valid open descriptor.
	let redirected = unsafe {
		libc::dup2(null.as_raw_fd(), libc::STDIN_FILENO) >= 0
			&& libc::dup2(stdout_pipe[1], libc::STDOUT_FILENO) >= 0
			&& libc::dup2(stderr_pipe[1], libc::STDERR_FILENO) >= 0
	};
	// SAFETY: the duplicated write ends are no longer needed after `dup2`.
	unsafe {
		libc::close(stdout_pipe[1]);
		libc::close(stderr_pipe[1]);
	}
	if !redirected {
		return Err(io::Error::last_os_error());
	}
	// SAFETY: every raw fd is uniquely owned after the operations above.
	let input = unsafe { std::fs::File::from_raw_fd(protocol_in) };
	// SAFETY: see above.
	let output = unsafe { std::fs::File::from_raw_fd(protocol_out) };
	// SAFETY: see above.
	let stdout_capture = unsafe { std::fs::File::from_raw_fd(stdout_pipe[0]) };
	// SAFETY: see above.
	let stderr_capture = unsafe { std::fs::File::from_raw_fd(stderr_pipe[0]) };
	for capture in [&stdout_capture, &stderr_capture] {
		// SAFETY: `capture` owns a valid descriptor and these operations only
		// update its file-status flags.
		let flags = unsafe { libc::fcntl(capture.as_raw_fd(), libc::F_GETFL) };
		if flags < 0
			|| unsafe { libc::fcntl(capture.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
		{
			return Err(io::Error::last_os_error());
		}
	}
	Ok(ShieldedProtocol {
		input:   Box::new(tokio::fs::File::from_std(input)),
		output:  Box::new(tokio::fs::File::from_std(output)),
		capture: Some((stdout_capture, stderr_capture)),
	})
}

#[cfg(not(unix))]
fn shield_protocol_fds() -> io::Result<ShieldedProtocol> {
	Ok(ShieldedProtocol {
		input:   Box::new(tokio::io::stdin()),
		output:  Box::new(tokio::io::stdout()),
		capture: (),
	})
}

#[derive(Default)]
struct CaptureBarrier {
	commands: Vec<flume::Sender<flume::Sender<()>>>,
}

impl CaptureBarrier {
	async fn drain(&self) {
		for commands in &self.commands {
			let (acknowledge, acknowledged) = flume::bounded(1);
			if commands.send(acknowledge).is_ok() {
				let _ = acknowledged.recv_async().await;
			}
		}
	}
}

#[cfg(unix)]
fn start_fd_capture(
	capture: ProtocolCapture,
	host: &Arc<ChildBridgeHost>,
) -> io::Result<CaptureBarrier> {
	use std::io::Read as _;

	let Some((stdout, stderr)) = capture else {
		return Ok(CaptureBarrier::default());
	};
	let mut commands = Vec::with_capacity(2);
	for (mut reader, channel) in [(stdout, OutputChannel::Stdout), (stderr, OutputChannel::Stderr)] {
		let host = Arc::clone(host);
		let (command_tx, command_rx) = flume::unbounded::<flume::Sender<()>>();
		commands.push(command_tx);
		std::thread::Builder::new()
			.name(format!("omp-eval-fd-{channel:?}"))
			.spawn(move || {
				let mut buffer = [0_u8; 16 * 1024];
				loop {
					match reader.read(&mut buffer) {
						Ok(0) => break,
						Ok(read) => {
							let run_id = host.active_run.load(Ordering::Acquire);
							if run_id == 0 {
								continue;
							}
							let update = Update {
								channel,
								data: CowBytes::from(buffer[..read].to_vec()),
								sequence: 0,
							};
							let frame = match channel {
								OutputChannel::Stdout => ChildFrame::Stdout { run_id, update },
								OutputChannel::Stderr => ChildFrame::Stderr { run_id, update },
							};
							if host.outgoing.send(frame).is_err() {
								break;
							}
						},
						Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
							while let Ok(acknowledge) = command_rx.try_recv() {
								let _ = acknowledge.send(());
							}
							std::thread::sleep(Duration::from_millis(1));
						},
						Err(_) => break,
					}
				}
			})?;
	}
	Ok(CaptureBarrier { commands })
}

#[cfg(not(unix))]
fn start_fd_capture(
	_capture: ProtocolCapture,
	_host: &Arc<ChildBridgeHost>,
) -> io::Result<CaptureBarrier> {
	Ok(CaptureBarrier::default())
}

/// Runs the hidden eval child entry before ordinary CLI or telemetry startup.
pub async fn run_eval_child_entry() -> Result<(), ProcessError> {
	let ShieldedProtocol { input, mut output, capture } = shield_protocol_fds()?;
	let mut stdin = BufReader::new(input);
	let (token, capabilities, config, interrupt_grace) =
		match read_frame::<_, ParentFrame>(&mut stdin).await? {
			Some(ParentFrame::Init {
				token,
				session_id: _,
				capabilities,
				config,
				interrupt_grace,
			}) => (token, capabilities, config, interrupt_grace.parse::<OmpDuration>()?),
			Some(_) => {
				return Err(ProcessError::Protocol(sf!("Init must be the first eval child frame",)));
			},
			None => return Ok(()),
		};
	let (outgoing, outgoing_rx) = flume::unbounded();
	let child_host = Arc::new(ChildBridgeHost {
		token,
		capabilities: BridgeCapabilities::from_allowed_names(capabilities),
		config: config.map(EvalSessionConfig::from),
		outgoing,
		pending: Mutex::new(BTreeMap::new()),
		next_request: AtomicU64::new(1),
		active_run: AtomicU64::new(0),
	});
	let capture_barrier = Arc::new(start_fd_capture(capture, &child_host)?);
	let writer = tokio::spawn(async move {
		while let Ok(frame) = outgoing_rx.recv_async().await {
			write_frame(&mut output, &frame).await?;
		}
		Ok::<(), ProcessError>(())
	});
	let runtime = tokio::runtime::Handle::current();
	let transport: Arc<dyn ChildBridgeTransport> = child_host.clone();
	let installer = Arc::new(BridgeNamespaceInstaller::new_child(transport, runtime));
	let engine = omp_py::Engine::builder()
		.init()
		.map(Arc::new)
		.map_err(|error| ProcessError::Python(Str::from(error.to_string())))?;
	let eval = EmbeddedPython::with_installer(engine, installer, interrupt_grace)?;
	let session = eval.open_session().await.map_err(ProcessError::Eval)?;
	child_host
		.outgoing
		.send(ChildFrame::Ready)
		.map_err(|_| ProcessError::Exited)?;
	let active = Arc::new(AtomicBool::new(false));
	loop {
		match read_frame::<_, ParentFrame>(&mut stdin).await? {
			Some(ParentFrame::Run { run_id, cell_id, code, timeout_ns, reset }) => {
				if active.swap(true, Ordering::AcqRel) {
					child_host
						.outgoing
						.send(ChildFrame::Fatal {
							message: sf!("eval child received overlapping Run frames"),
						})
						.map_err(|_| ProcessError::Exited)?;
					continue;
				}
				child_host.active_run.store(run_id, Ordering::Release);
				let mut run = match eval
					.run(&session, RunRequest {
						code,
						timeout: timeout_ns.map(Duration::from_nanos),
						reset,
					})
					.await
				{
					Ok(run) => run,
					Err(error) => {
						active.store(false, Ordering::Release);
						child_host.active_run.store(0, Ordering::Release);
						child_host
							.outgoing
							.send(ChildFrame::Fatal { message: Str::from(format!("{error:?}")) })
							.map_err(|_| ProcessError::Exited)?;
						continue;
					},
				};
				let outgoing = child_host.outgoing.clone();
				let active_flag = Arc::clone(&active);
				let run_route = Arc::clone(&child_host);
				let capture_barrier = Arc::clone(&capture_barrier);
				tokio::spawn(async move {
					loop {
						match run.next_event().await {
							Ok(Some(RunEvent::Started { .. })) => {
								let _ =
									outgoing.send(ChildFrame::Started { run_id, cell_id: cell_id.clone() });
							},
							Ok(Some(RunEvent::Output(update))) => {
								let frame = match update.channel {
									omp_tools::eval::OutputChannel::Stdout => {
										ChildFrame::Stdout { run_id, update }
									},
									omp_tools::eval::OutputChannel::Stderr => {
										ChildFrame::Stderr { run_id, update }
									},
								};
								let _ = outgoing.send(frame);
							},
							Ok(Some(RunEvent::Completed(completion))) => {
								capture_barrier.drain().await;
								run_route.active_run.store(0, Ordering::Release);
								active_flag.store(false, Ordering::Release);
								let RunCompletion {
									mut status,
									result,
									display_outputs,
									truncated,
									spilled_output,
									total_lines,
									total_bytes,
								} = completion;
								for output in display_outputs {
									let _ = outgoing.send(ChildFrame::Display { run_id, output });
								}
								if let Some(value) = result {
									let _ = outgoing.send(ChildFrame::Result { run_id, value });
								}
								if let Some(value) = status.exception.take() {
									let _ = outgoing.send(ChildFrame::Error { run_id, value });
								}
								let _ = outgoing.send(ChildFrame::Done {
									run_id,
									status,
									truncated,
									spilled_output,
									total_lines,
									total_bytes,
								});
								break;
							},
							Ok(None) => {
								run_route.active_run.store(0, Ordering::Release);
								active_flag.store(false, Ordering::Release);
								let _ = outgoing.send(ChildFrame::Fatal {
									message: sf!("embedded eval stream ended without completion",),
								});
								break;
							},
							Err(error) => {
								run_route.active_run.store(0, Ordering::Release);
								active_flag.store(false, Ordering::Release);
								let _ = outgoing
									.send(ChildFrame::Fatal { message: Str::from(format!("{error:?}")) });
								break;
							},
						}
					}
				});
			},
			Some(ParentFrame::BridgeResponse { request_id, value, error }) => {
				let result = match (value, error) {
					(Some(value), None) => Ok(value),
					(None, Some(error)) => Err(error),
					_ => Err(sf!("malformed eval parent bridge response")),
				};
				child_host.resolve(request_id, result);
			},
			Some(ParentFrame::Init { .. }) => {
				return Err(ProcessError::Protocol(sf!("duplicate eval child Init frame")));
			},
			Some(ParentFrame::Exit) => break,
			None => break,
		}
	}
	writer.abort();
	let _ = writer.await;
	Ok(())
}

/// Eval child startup, framing, bridge, or embedded-runtime failure.
#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
	/// Standard-I/O transport failed.
	#[error("eval child I/O failed: {0}")]
	Io(#[from] io::Error),
	/// A frame exceeded the fixed transport bound.
	#[error("eval child frame exceeded the {MAX_FRAME_BYTES}-byte limit")]
	FrameTooLarge,
	/// A bounded frame did not contain valid protocol JSON.
	#[error("eval child sent an invalid frame: {0}")]
	Json(#[from] serde_json::Error),
	/// Parent and child violated the expected protocol sequence.
	#[error("eval child protocol violation: {0}")]
	Protocol(Str),
	/// The child could not initialize embedded Python.
	#[error("eval child embedded Python failed: {0}")]
	Python(Str),
	/// A configured or serialized duration was not representable.
	#[error("eval child duration failed: {0}")]
	Duration(#[from] DurationError),
	/// The child's embedded eval kernel rejected an operation.
	#[error("eval child kernel failed: {0:?}")]
	Eval(Fault),
	/// Durable oversized-output staging failed.
	#[error("eval child output spill failed: {0}")]
	Spill(Str),
	/// The child closed its protocol stream.
	#[error("eval child exited")]
	Exited,
	/// The authenticated host bridge rejected startup or dispatch.
	#[error(transparent)]
	Bridge(#[from] BridgeHostError),
}

async fn write_frame<W: AsyncWrite + Unpin + Send, T: Serialize + Sync>(
	writer: &mut W,
	frame: &T,
) -> Result<(), ProcessError> {
	let encoded = serde_json::to_vec(frame)?;
	if encoded.is_empty() || encoded.len() > MAX_FRAME_BYTES {
		return Err(ProcessError::FrameTooLarge);
	}
	write_encoded_frame(writer, &encoded).await
}

async fn write_encoded_frame<W: AsyncWrite + Unpin + Send>(
	writer: &mut W,
	encoded: &[u8],
) -> Result<(), ProcessError> {
	writer.write_all(encoded).await?;
	writer.write_all(b"\n").await?;
	writer.flush().await?;
	Ok(())
}
fn sanitized_spawn_env() -> Vec<(OsString, OsString)> {
	std::env::vars_os()
		.filter(|(name, _)| spawn_env_allowed(name))
		.collect()
}

fn spawn_env_allowed(name: &OsStr) -> bool {
	let upper = name.to_string_lossy().to_ascii_uppercase();
	let secret = SECRET_MARKERS.iter().any(|marker| upper.contains(marker));
	!secret
		&& (matches!(
			upper.as_str(),
			"PATH"
				| "HOME" | "USER"
				| "LOGNAME"
				| "SHELL"
				| "TMPDIR"
				| "TEMP" | "TMP"
				| "LANG" | "TERM"
				| "COLORTERM"
				| "NO_COLOR"
				| "SYSTEMROOT"
				| "WINDIR"
				| "COMSPEC"
				| "PATHEXT"
				| "USERPROFILE"
				| "APPDATA"
				| "LOCALAPPDATA"
		) || upper.starts_with("LC_")
			|| upper.starts_with("OMP_EVAL_")
			|| upper.starts_with("OMP_PY_"))
}

/// Discovers the Python interpreter identity for a supervised kernel.
///
/// Selection follows the explicit override, active virtual environment,
/// project virtual environments, Conda, uv, pyenv, then `PATH`. Production
/// falls back to the embedded CPython identity when this returns `None`.
#[must_use]
pub fn discover_external_python(cwd: &Path, explicit: Option<&OsStr>) -> Option<PathBuf> {
	let executable = if cfg!(windows) {
		"python.exe"
	} else {
		"python"
	};
	let mut candidates = Vec::new();
	if let Some(explicit) = explicit {
		candidates.push(expand_home(PathBuf::from(explicit)));
	}
	for name in ["VIRTUAL_ENV", "CONDA_PREFIX", "UV_PROJECT_ENVIRONMENT"] {
		if let Some(root) = std::env::var_os(name) {
			candidates.push(interpreter_below(Path::new(&root), executable));
		}
	}
	candidates.push(interpreter_below(&cwd.join(".venv"), executable));
	candidates.push(interpreter_below(&cwd.join("venv"), executable));
	if let (Some(root), Some(version)) =
		(std::env::var_os("PYENV_ROOT"), std::env::var_os("PYENV_VERSION"))
	{
		candidates
			.push(interpreter_below(&PathBuf::from(root).join("versions").join(version), executable));
	}
	if let Some(path) = std::env::var_os("PATH") {
		candidates.extend(std::env::split_paths(&path).flat_map(|directory| {
			["python3", executable]
				.into_iter()
				.map(move |name| directory.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)))
		}));
	}
	candidates.into_iter().find(|candidate| candidate.is_file())
}

fn interpreter_below(root: &Path, executable: &str) -> PathBuf {
	if cfg!(windows) {
		root.join("Scripts").join(executable)
	} else {
		root.join("bin").join(executable)
	}
}

fn expand_home(path: PathBuf) -> PathBuf {
	let Some(text) = path.to_str() else {
		return path;
	};
	if text == "~" {
		return std::env::var_os("HOME").map(PathBuf::from).unwrap_or(path);
	}
	let Some(rest) = text.strip_prefix("~/") else {
		return path;
	};
	std::env::var_os("HOME")
		.map(|home| PathBuf::from(home).join(rest))
		.unwrap_or(path)
}

async fn read_frame<R: AsyncBufRead + Unpin + Send, T: DeserializeOwned>(
	reader: &mut R,
) -> Result<Option<T>, ProcessError> {
	let mut encoded = Vec::new();
	loop {
		let available = reader.fill_buf().await?;
		if available.is_empty() {
			if encoded.is_empty() {
				return Ok(None);
			}
			return Err(ProcessError::Protocol(sf!("unterminated NDJSON frame")));
		}
		let newline = available.iter().position(|byte| *byte == b'\n');
		let take = newline.map_or(available.len(), |index| index + 1);
		if encoded.len().saturating_add(take) > MAX_FRAME_BYTES.saturating_add(1) {
			return Err(ProcessError::FrameTooLarge);
		}
		if let Some(index) = newline {
			encoded.extend_from_slice(&available[..index]);
			reader.consume(take);
			break;
		}
		encoded.extend_from_slice(&available[..take]);
		reader.consume(take);
	}
	if encoded.last() == Some(&b'\r') {
		encoded.pop();
	}
	if encoded.is_empty() {
		return Err(ProcessError::Protocol(sf!("empty NDJSON frame")));
	}
	serde_json::from_slice(&encoded)
		.map(Some)
		.map_err(ProcessError::from)
}

fn resolve_omp_executable() -> io::Result<PathBuf> {
	if let Some(path) = std::env::var_os("CARGO_BIN_EXE_omp") {
		let path = PathBuf::from(path);
		if path.is_file() {
			return Ok(path);
		}
	}
	let current = std::env::current_exe()?;
	if current.file_stem().is_some_and(|name| name == "omp") {
		return Ok(current);
	}
	let mut directory = current
		.parent()
		.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "current executable has no parent"))?;
	if directory.file_name().is_some_and(|name| name == "deps") {
		directory = directory.parent().ok_or_else(|| {
			io::Error::new(io::ErrorKind::NotFound, "target deps directory has no parent")
		})?;
	}
	let sibling = directory.join(format!("omp{}", std::env::consts::EXE_SUFFIX));
	if sibling.is_file() {
		return Ok(sibling);
	}
	Err(io::Error::new(
		io::ErrorKind::NotFound,
		format!(
			"real omp executable not found (set CARGO_BIN_EXE_omp or build {})",
			sibling.display()
		),
	))
}

fn elapsed_ms(started: Instant) -> u64 {
	u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

const fn cancelled_completion(duration_ms: u64) -> RunCompletion {
	RunCompletion {
		status:          CellStatus {
			outcome: CellOutcome::Cancelled,
			exit_code: None,
			duration_ms,
			exception: Some(PythonException {
				name:      sf!("KeyboardInterrupt"),
				message:   sf!("OMP eval cell interrupted"),
				traceback: Vec::new(),
			}),
		},
		result:          None,
		display_outputs: Vec::new(),
		truncated:       false,
		spilled_output:  None,
		total_lines:     0,
		total_bytes:     0,
	}
}

const fn timeout_completion(duration_ms: u64) -> RunCompletion {
	RunCompletion {
		status:          CellStatus {
			outcome: CellOutcome::Timeout,
			exit_code: Some(1),
			duration_ms,
			exception: Some(PythonException {
				name:      sf!("TimeoutError"),
				message:   sf!("OMP eval cell timed out"),
				traceback: Vec::new(),
			}),
		},
		result:          None,
		display_outputs: Vec::new(),
		truncated:       false,
		spilled_output:  None,
		total_lines:     0,
		total_bytes:     0,
	}
}

fn resource_fault(operation: &'static str, error: ProcessError) -> Fault {
	Fault::Resource { operation: sf!(operation), message: Str::from(error.to_string()) }
}

fn session_lost(error: ProcessError) -> Fault {
	Fault::SessionLost { message: Str::from(error.to_string()) }
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn spawn_environment_is_allowlisted_and_rejects_secret_names() {
		assert!(spawn_env_allowed(OsStr::new("PATH")));
		assert!(spawn_env_allowed(OsStr::new("LC_ALL")));
		assert!(spawn_env_allowed(OsStr::new("OMP_EVAL_MODE")));
		assert!(!spawn_env_allowed(OsStr::new("AWS_SECRET_ACCESS_KEY")));
		assert!(!spawn_env_allowed(OsStr::new("OMP_EVAL_TOKEN")));
		assert!(!spawn_env_allowed(OsStr::new("RANDOM_AMBIENT_VALUE")));
	}

	#[tokio::test]
	async fn protocol_is_bounded_ndjson() {
		let (mut writer, reader) = tokio::io::duplex(256);
		write_frame(&mut writer, &ParentFrame::Exit)
			.await
			.expect("frame writes");
		drop(writer);
		let mut reader = BufReader::new(reader);
		assert!(matches!(
			read_frame::<_, ParentFrame>(&mut reader)
				.await
				.expect("frame reads"),
			Some(ParentFrame::Exit)
		));
		assert!(
			read_frame::<_, ParentFrame>(&mut reader)
				.await
				.expect("EOF reads")
				.is_none()
		);
	}

	#[tokio::test]
	async fn protocol_rejects_empty_and_unterminated_frames() {
		for bytes in [b"\n".as_slice(), b"{\"kind\":\"exit\"}".as_slice()] {
			let mut reader = BufReader::new(bytes);
			assert!(read_frame::<_, ParentFrame>(&mut reader).await.is_err());
		}
	}
}
