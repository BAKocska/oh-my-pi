//! Environment-daemon process and persistent shell-session host.

use std::{
	collections::{HashMap, HashSet},
	io::{Read, Write as _},
	os::fd::{AsFd as _, AsRawFd as _},
	path::PathBuf,
	sync::{
		Arc, Weak,
		atomic::{AtomicBool, AtomicU64, Ordering},
	},
	time::{Duration, Instant},
};

use bytes::{Buf as _, Bytes, BytesMut};
use futures::future::try_join_all;
use omp_core::{Str, sf};
use omp_proto::{
	env::v1::{
		AttachOutput, CloseSessionResponse, EnvironmentDelta, ExecOutcome, ExecRequest, ExecStarted,
		ExecStatusMsg, ExitEvent, OpenSessionRequest, OpenSessionResponse, OutputAttached,
		OutputChannel, OutputFrame, ProcessCommandAccepted, ProcessInfo, ProcessList, ProcessOutput,
		ProcessStarted, ProcessState, PtySpec, ReadyProbe, StartProcess, ready_probe,
	},
	toolhost::v1::{HostFrame, Ping, WorkerFrame, host_frame, worker_frame},
};
use omp_shell_engine::{
	ExecutionParameters, Shell, ShellVariable, SourceInfo, SpawnObserver,
	openfiles::{OpenFile, OpenFiles},
	processes::{ProcessSignal, signal_process_group},
};
use parking_lot::Mutex;
use prost::Message as _;
use regex::bytes::Regex;

const CANCEL_GRACE: Duration = Duration::from_millis(250);
const OUTPUT_CHUNK_BYTES: usize = 16 * 1024;

/// Content identity of the process environment inherited by new shell sessions.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct WorkspaceEnvironmentDigest([u8; 32]);

impl WorkspaceEnvironmentDigest {
	/// Returns the BLAKE3 digest over sorted environment name/value pairs.
	pub const fn as_bytes(&self) -> &[u8; 32] {
		&self.0
	}
}

/// Errors returned by the environment execution host.
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
	/// The requested session does not exist.
	#[error("exec session was not found")]
	SessionNotFound,
	/// The requested execution does not exist or has already finished.
	#[error("execution was not found")]
	RunNotFound,
	/// The requested named process does not exist.
	#[error("named process {0:?} was not found")]
	ProcessNotFound(Str),
	/// A process with this name is already registered.
	#[error("named process {0:?} already exists")]
	ProcessExists(Str),
	/// A named-process request targeted a retired generation.
	#[error(
		"named process {name:?} generation {requested} is stale; current generation is {current}"
	)]
	StaleProcessGeneration {
		/// Stable process name.
		name:      Str,
		/// Generation carried by the request.
		requested: u64,
		/// Current retained generation.
		current:   u64,
	},
	/// A request contained an unsupported signal name.
	#[error("unsupported signal {0:?}")]
	UnsupportedSignal(Str),
	/// A URI could not be used as a local working directory.
	#[error("invalid working-directory URI {0:?}")]
	InvalidCwd(Str),
	/// The shell engine rejected the requested operation.
	#[error("shell execution failed: {0}")]
	Shell(Str),
	/// A named-process readiness probe was invalid or did not pass.
	#[error("process readiness failed: {0}")]
	Readiness(Str),
	/// An operating-system process primitive failed.
	#[error("process I/O failed: {0}")]
	Io(#[from] std::io::Error),
	/// The target actor has stopped.
	#[error("exec session has closed")]
	SessionClosed,
}

/// One ordered event emitted by an execution.
#[derive(Clone, Debug)]
pub enum ExecEvent {
	/// The session dequeued this command and entered execution.
	Started {
		/// Opaque execution identifier assigned when the command was queued.
		exec_id: Bytes,
	},
	/// Bytes written by stdout, stderr, or the PTY.
	Output(OutputFrame),
	/// The terminal execution status. No output follows it.
	Exit(ExitEvent),
}

/// One event emitted by an attached named process.
#[derive(Clone, Debug)]
pub enum ProcessEvent {
	/// Ordered process output.
	Output(ProcessOutput),
	/// A state transition for the process.
	State(ProcessInfo),
}

/// RAII ownership of one command invocation.
///
/// Dropping this value requests TERM-then-KILL teardown of only this command's
/// process groups unless [`ExecHost::detach_exec`] retained the exact run as a
/// named process. The shell session is owned by [`ExecHost`] and survives.
pub struct ExecRun {
	id:      Bytes,
	events:  flume::Receiver<ExecEvent>,
	control: Arc<RunControl>,
}

impl ExecRun {
	/// Returns the opaque wire execution identifier.
	pub fn id(&self) -> &[u8] {
		&self.id
	}

	/// Waits for the next output or terminal event.
	pub async fn next_event(&self) -> Option<ExecEvent> {
		self.events.recv_async().await.ok()
	}

	/// Requests cancellation without dropping the event stream.
	pub fn cancel(&self) {
		self.control.cancel(CANCEL_GRACE);
	}
}

impl Drop for ExecRun {
	fn drop(&mut self) {
		if self
			.control
			.retained
			.lock()
			.as_ref()
			.and_then(Weak::upgrade)
			.is_none()
		{
			self.cancel();
		}
	}
}

/// Snapshot plus future events returned by named-process attachment.
pub struct ProcessAttachment {
	/// Attachment acknowledgement.
	pub attached: OutputAttached,
	/// Buffered output strictly newer than the requested sequence.
	pub backlog:  Vec<ProcessOutput>,
	/// Process state captured atomically with the backlog and subscription.
	pub state:    ProcessInfo,
	/// Future output and state transitions.
	pub events:   flume::Receiver<ProcessEvent>,
}

/// Host for persistent shell sessions and named processes.
#[derive(Clone)]
pub struct ExecHost {
	inner: Arc<HostInner>,
}

struct HostInner {
	next_id:     AtomicU64,
	sessions:    Mutex<HashMap<Bytes, SessionHandle>>,
	runs:        Mutex<HashMap<Bytes, Weak<RunControl>>>,
	processes:   Mutex<HashMap<Str, Arc<NamedProcess>>>,
	starting:    Mutex<HashSet<Str>>,
	environment: Mutex<WorkspaceEnvironment>,
}

#[derive(Clone)]
struct SessionHandle {
	tx:  flume::Sender<SessionCommand>,
	pty: Option<PtySpec>,
}

struct NamedProcess {
	name:       Str,
	generation: u64,
	control:    Arc<RunControl>,
	stream:     Mutex<ProcessStreamState>,
}

struct ProcessStreamState {
	info:        ProcessInfo,
	history:     Vec<ProcessOutput>,
	subscribers: Vec<flume::Sender<ProcessEvent>>,
}

struct ProcessReservation {
	host: Weak<HostInner>,
	name: Str,
}

struct RunControl {
	cancel_tx: flume::Sender<CancelRequest>,
	input:     Mutex<Option<InputSink>>,
	spawns:    Arc<SpawnBook>,
	finished:  AtomicBool,
	retained:  Mutex<Option<Weak<NamedProcess>>>,
}

struct CancelRequest {
	grace: Duration,
}
struct WorkspaceEnvironment {
	variables: Arc<[(Str, Str)]>,
	digest:    WorkspaceEnvironmentDigest,
}

enum InputSink {
	Pipe(std::io::PipeWriter),
	Pty(std::fs::File),
}

struct SpawnBook {
	groups: Mutex<Vec<i32>>,
}

struct SessionCommand {
	exec:      Bytes,
	source:    Str,
	timeout:   Option<Duration>,
	pty:       Option<PtySpec>,
	control:   Arc<RunControl>,
	cancel_rx: flume::Receiver<CancelRequest>,
	events:    flume::Sender<ExecEvent>,
}

impl Default for ExecHost {
	fn default() -> Self {
		Self::new()
	}
}

impl ExecHost {
	/// Creates an empty execution host. Sessions are opened lazily by callers.
	pub fn new() -> Self {
		Self {
			inner: Arc::new(HostInner {
				next_id:     AtomicU64::new(1),
				sessions:    Mutex::new(HashMap::new()),
				runs:        Mutex::new(HashMap::new()),
				processes:   Mutex::new(HashMap::new()),
				starting:    Mutex::new(HashSet::new()),
				environment: Mutex::new(read_workspace_environment()),
			}),
		}
	}

	/// Opens a persistent shell carrying its own cwd and environment state.
	pub async fn open_session(
		&self,
		request: OpenSessionRequest,
	) -> Result<OpenSessionResponse, ExecError> {
		let cwd = cwd_from_uri(&request.cwd_uri)?.map_or_else(std::env::current_dir, Ok)?;
		let variables = Arc::clone(&self.inner.environment.lock().variables);
		let mut builder = Shell::builder()
			.profile(omp_shell_engine::ProfileLoadBehavior::Skip)
			.rc(omp_shell_engine::RcLoadBehavior::Skip)
			.working_dir(cwd)
			.do_not_inherit_env(true)
			.builtins(omp_shell_engine::builtins::default_builtins());
		for (name, value) in variables.iter() {
			let mut variable = ShellVariable::new(value.to_string());
			variable.export();
			builder = builder.var(name.to_string(), variable);
		}
		let mut shell = builder.build().await.map_err(shell_error)?;
		if let Some(pty) = request.pty.as_ref()
			&& !pty.terminal.is_empty()
		{
			let mut terminal = ShellVariable::new(pty.terminal.clone());
			terminal.export();
			shell
				.set_env_global("TERM", terminal)
				.map_err(shell_error)?;
		}
		apply_env_delta(&mut shell, request.env_delta.as_ref()).map_err(shell_error)?;

		let session = self.new_id();
		let lease = self.new_id();
		let (tx, rx) = flume::unbounded();
		let sessions = Arc::downgrade(&self.inner);
		let session_for_task = session.clone();
		tokio::spawn(async move {
			session_loop(shell, rx).await;
			if let Some(host) = sessions.upgrade() {
				host.sessions.lock().remove(&session_for_task);
			}
		});
		self
			.inner
			.sessions
			.lock()
			.insert(session.clone(), SessionHandle { tx, pty: request.pty.clone() });

		Ok(OpenSessionResponse {
			session,
			lease,
			cwd_uri: request.cwd_uri,
			props: Default::default(),
		})
	}

	/// Returns the digest of the cached environment inherited by new sessions.
	pub fn workspace_environment_digest(&self) -> WorkspaceEnvironmentDigest {
		self.inner.environment.lock().digest
	}

	/// Explicitly refreshes the environment inherited by subsequently opened
	/// sessions.
	///
	/// Existing sessions retain their own shell state.
	pub fn refresh_workspace_environment(&self) -> WorkspaceEnvironmentDigest {
		let environment = read_workspace_environment();
		let digest = environment.digest;
		*self.inner.environment.lock() = environment;
		digest
	}

	/// Closes a session and all shell-owned background jobs.
	pub fn close_session(&self, session: &[u8]) -> Result<CloseSessionResponse, ExecError> {
		let Some(handle) = self.inner.sessions.lock().remove(session) else {
			return Err(ExecError::SessionNotFound);
		};
		drop(handle);
		Ok(CloseSessionResponse {
			session: Bytes::copy_from_slice(session),
			props:   Default::default(),
		})
	}

	/// Starts a script in a session. A session serializes its scripts.
	pub async fn exec(
		&self,
		request: ExecRequest,
		timeout: Option<Duration>,
	) -> Result<(ExecStarted, ExecRun), ExecError> {
		let session = self
			.inner
			.sessions
			.lock()
			.get(&request.session)
			.cloned()
			.ok_or(ExecError::SessionNotFound)?;
		let source = request
			.source
			.ok_or_else(|| ExecError::Shell(sf!("missing script")))?;
		let exec = self.new_id();
		let (events_tx, events) = flume::unbounded();
		let (cancel_tx, cancel_rx) = flume::bounded(1);
		let control = Arc::new(RunControl {
			cancel_tx,
			input: Mutex::new(None),
			spawns: Arc::new(SpawnBook { groups: Mutex::new(Vec::new()) }),
			finished: AtomicBool::new(false),
			retained: Mutex::new(None),
		});
		let command = SessionCommand {
			exec: exec.clone(),
			source: Str::from(source.text),
			timeout,
			pty: session.pty,
			control: control.clone(),
			cancel_rx,
			events: events_tx,
		};
		session
			.tx
			.send_async(command)
			.await
			.map_err(|_| ExecError::SessionClosed)?;
		self
			.inner
			.runs
			.lock()
			.insert(exec.clone(), Arc::downgrade(&control));
		Ok((
			ExecStarted {
				session: request.session,
				exec:    exec.clone(),
				props:   Default::default(),
			},
			ExecRun { id: exec, events, control },
		))
	}

	/// Writes input or closes stdin for a running command.
	pub fn stdin(&self, exec: &[u8], data: Option<&[u8]>) -> Result<(), ExecError> {
		let control = self.run(exec)?;
		write_input(&control, data)
	}

	/// Sends a named signal to all process groups owned by a command.
	pub fn signal(&self, exec: &[u8], signal: &str) -> Result<(), ExecError> {
		let control = self.run(exec)?;
		if control.finished.load(Ordering::Acquire) {
			return Err(ExecError::RunNotFound);
		}
		control.spawns.signal(parse_signal(signal)?)?;
		Ok(())
	}

	/// Changes the terminal window size for a PTY-backed command.
	pub fn resize(&self, exec: &[u8], rows: u32, columns: u32) -> Result<(), ExecError> {
		let control = self.run(exec)?;
		let input = control.input.lock();
		let Some(InputSink::Pty(master)) = input.as_ref() else {
			return Err(ExecError::Io(std::io::Error::new(
				std::io::ErrorKind::Unsupported,
				"execution has no PTY",
			)));
		};
		resize_fd(master.as_fd(), rows, columns)?;
		control.spawns.signal(ProcessSignal::WindowChanged)?;
		Ok(())
	}

	/// Cancels a command without closing its session.
	pub fn cancel(&self, exec: &[u8]) -> Result<(), ExecError> {
		self.run(exec)?.cancel(CANCEL_GRACE);
		Ok(())
	}

	/// Starts a persistent named process and waits for every readiness probe.
	pub async fn start_process(&self, request: StartProcess) -> Result<ProcessStarted, ExecError> {
		let name = Str::from(request.name);
		let _reservation = self.reserve_process(name.clone())?;
		let ready = request.ready;
		let spec = request
			.spec
			.ok_or_else(|| ExecError::Shell(sf!("missing process spec")))?;
		let opened = self
			.open_session(OpenSessionRequest {
				cwd_uri:   spec.cwd_uri,
				env_delta: spec.env_delta,
				pty:       spec.pty,
				lease:     None,
				props:     Default::default(),
			})
			.await?;
		let (started, run) = self
			.exec(
				ExecRequest {
					session: opened.session,
					source:  spec.source,
					props:   Default::default(),
				},
				None,
			)
			.await?;
		let generation = 1;
		let process = Arc::new(NamedProcess {
			name: name.clone(),
			generation,
			control: run.control.clone(),
			stream: Mutex::new(ProcessStreamState {
				info:        ProcessInfo {
					name: name.to_string(),
					generation,
					state: if ready.is_empty() {
						ProcessState::Running as i32
					} else {
						ProcessState::Starting as i32
					},
					status: None,
					props: Default::default(),
				},
				history:     Vec::new(),
				subscribers: Vec::new(),
			}),
		});
		self
			.inner
			.processes
			.lock()
			.insert(name.clone(), process.clone());
		tokio::spawn(forward_named_process(process.clone(), run, started.exec));
		if let Err(error) = try_join_all(
			ready
				.into_iter()
				.map(|probe| wait_ready_probe(process.clone(), probe)),
		)
		.await
		{
			process.control.cancel(CANCEL_GRACE);
			self.inner.processes.lock().remove(&name);
			return Err(error);
		}
		let terminal = {
			let mut stream = process.stream.lock();
			if stream.info.state == ProcessState::Starting as i32 {
				stream.info.state = ProcessState::Ready as i32;
				let info = stream.info.clone();
				stream.broadcast(ProcessEvent::State(info));
				false
			} else {
				process_state_is_terminal(stream.info.state)
			}
		};
		if terminal {
			process.control.cancel(CANCEL_GRACE);
			self.inner.processes.lock().remove(&name);
			return Err(ExecError::Readiness(sf!(
				"process exited while readiness probes were running",
			)));
		}
		Ok(ProcessStarted { name: process.name.to_string(), generation, props: Default::default() })
	}

	/// Converts an active foreground execution into a retained named process.
	///
	/// The existing process groups, input handles, execution identifier, and
	/// output sequencer are preserved. Dropping the foreground [`ExecRun`] after
	/// this succeeds no longer requests process-tree cancellation.
	pub fn detach_exec(&self, exec: &[u8], name: &str) -> Result<ProcessStarted, ExecError> {
		let name = Str::from(name);
		let _reservation = self.reserve_process(name.clone())?;
		let control = self.run(exec)?;
		let mut retained = control.retained.lock();
		if control.finished.load(Ordering::Acquire) {
			return Err(ExecError::RunNotFound);
		}
		let generation = 1;
		let process = Arc::new(NamedProcess {
			name: name.clone(),
			generation,
			control: control.clone(),
			stream: Mutex::new(ProcessStreamState {
				info:        ProcessInfo {
					name: name.to_string(),
					generation,
					state: ProcessState::Running as i32,
					status: None,
					props: Default::default(),
				},
				history:     Vec::new(),
				subscribers: Vec::new(),
			}),
		});
		self.inner.processes.lock().insert(name, process.clone());
		*retained = Some(Arc::downgrade(&process));
		Ok(ProcessStarted { name: process.name.to_string(), generation, props: Default::default() })
	}

	/// Lists every registered named process in stable name order.
	pub fn list_processes(&self) -> ProcessList {
		let mut processes: Vec<_> = self
			.inner
			.processes
			.lock()
			.values()
			.map(|process| process.stream.lock().info.clone())
			.collect();
		processes.sort_unstable_by(|left, right| left.name.cmp(&right.name));
		ProcessList { processes, props: Default::default() }
	}

	/// Attaches to buffered and future named-process output.
	pub fn attach_output(&self, request: &AttachOutput) -> Result<ProcessAttachment, ExecError> {
		let process = self.named_process(&request.name, request.generation)?;
		let (tx, events) = flume::unbounded();
		let mut stream = process.stream.lock();
		let backlog = stream
			.history
			.iter()
			.filter(|event| event.sequence > request.after_sequence)
			.cloned()
			.collect();
		stream.subscribers.push(tx);
		let state = stream.info.clone();
		drop(stream);
		Ok(ProcessAttachment {
			attached: OutputAttached {
				name:       request.name.clone(),
				generation: process.generation,
				props:      Default::default(),
			},
			backlog,
			state,
			events,
		})
	}

	/// Writes input or closes stdin for a named process.
	pub fn send_process_input(
		&self,
		name: &str,
		generation: u64,
		data: Option<&[u8]>,
	) -> Result<ProcessCommandAccepted, ExecError> {
		let process = self.named_process(name, generation)?;
		write_input(&process.control, data)?;
		Ok(ProcessCommandAccepted { name: name.to_owned(), props: Default::default() })
	}

	/// Sends a named signal to every group owned by a named process.
	pub fn signal_process(
		&self,
		name: &str,
		generation: u64,
		signal: &str,
	) -> Result<ProcessCommandAccepted, ExecError> {
		let process = self.named_process(name, generation)?;
		if process.control.finished.load(Ordering::Acquire) {
			return Err(ExecError::RunNotFound);
		}
		process.control.spawns.signal(parse_signal(signal)?)?;
		Ok(ProcessCommandAccepted { name: name.to_owned(), props: Default::default() })
	}

	/// TERM-then-KILLs a named process. Its registration and terminal state
	/// remain available to list and attach calls.
	pub fn stop_process(
		&self,
		name: &str,
		generation: u64,
		grace: Duration,
	) -> Result<ProcessCommandAccepted, ExecError> {
		let process = self.named_process(name, generation)?;
		process.control.cancel(grace);
		Ok(ProcessCommandAccepted { name: name.to_owned(), props: Default::default() })
	}

	fn named_process(&self, name: &str, generation: u64) -> Result<Arc<NamedProcess>, ExecError> {
		let key = Str::from(name);
		let process = self
			.inner
			.processes
			.lock()
			.get(&key)
			.cloned()
			.ok_or_else(|| ExecError::ProcessNotFound(key.clone()))?;
		if process.generation != generation {
			return Err(ExecError::StaleProcessGeneration {
				name:      key,
				requested: generation,
				current:   process.generation,
			});
		}
		Ok(process)
	}

	fn run(&self, exec: &[u8]) -> Result<Arc<RunControl>, ExecError> {
		self
			.inner
			.runs
			.lock()
			.get(exec)
			.and_then(Weak::upgrade)
			.ok_or(ExecError::RunNotFound)
	}

	fn reserve_process(&self, name: Str) -> Result<ProcessReservation, ExecError> {
		if !self.inner.starting.lock().insert(name.clone()) {
			return Err(ExecError::ProcessExists(name));
		}
		if self.inner.processes.lock().contains_key(&name) {
			self.inner.starting.lock().remove(&name);
			return Err(ExecError::ProcessExists(name));
		}
		Ok(ProcessReservation { host: Arc::downgrade(&self.inner), name })
	}

	fn new_id(&self) -> Bytes {
		Bytes::copy_from_slice(
			&self
				.inner
				.next_id
				.fetch_add(1, Ordering::Relaxed)
				.to_be_bytes(),
		)
	}
}

impl Drop for ProcessReservation {
	fn drop(&mut self) {
		if let Some(host) = self.host.upgrade() {
			host.starting.lock().remove(&self.name);
		}
	}
}
impl RunControl {
	fn cancel(&self, grace: Duration) {
		let _ = self.cancel_tx.try_send(CancelRequest { grace });
	}

	fn close_input(&self) {
		self.input.lock().take();
	}
}

impl SpawnObserver for SpawnBook {
	fn on_spawn(&self, _pid: i32, pgid: Option<i32>) {
		let Some(pgid) = pgid else { return };
		let mut groups = self.groups.lock();
		if !groups.contains(&pgid) {
			groups.push(pgid);
		}
	}
}

impl SpawnBook {
	fn signal(&self, signal: ProcessSignal) -> Result<(), std::io::Error> {
		for pgid in self.groups.lock().iter().copied() {
			signal_process_group(pgid, signal)?;
		}
		Ok(())
	}

	async fn terminate(&self, grace: Duration) {
		if self.groups.lock().is_empty() {
			return;
		}
		let _ = self.signal(ProcessSignal::Terminate);
		if !grace.is_zero() {
			tokio::time::sleep(grace).await;
		}
		let _ = self.signal(ProcessSignal::Kill);
	}
}

async fn session_loop(mut shell: Shell, commands: flume::Receiver<SessionCommand>) {
	let mut cancellation_deadline = None;
	loop {
		let command = if let Some(deadline) = cancellation_deadline {
			tokio::select! {
				command = commands.recv_async() => match command {
					Ok(command) => command,
					Err(_) => break,
				},
				() = tokio::time::sleep_until(deadline) => {
					cancellation_deadline = None;
					continue;
				},
			}
		} else {
			match commands.recv_async().await {
				Ok(command) => command,
				Err(_) => break,
			}
		};

		if let Some(deadline) = cancellation_deadline {
			match tokio::time::timeout_at(deadline, command.cancel_rx.recv_async()).await {
				Ok(Ok(_) | Err(_)) => {
					finish_session_command(&command, RunTerminal::Cancelled, Duration::ZERO);
					cancellation_deadline = Some(tokio::time::Instant::now() + CANCEL_GRACE);
					continue;
				},
				Err(_) => cancellation_deadline = None,
			}
		}
		if run_session_command(&mut shell, command).await {
			cancellation_deadline = Some(tokio::time::Instant::now() + CANCEL_GRACE);
		}
	}
}

async fn run_session_command(shell: &mut Shell, command: SessionCommand) -> bool {
	let started_at = Instant::now();
	match command.cancel_rx.try_recv() {
		Ok(_) | Err(flume::TryRecvError::Disconnected) => {
			finish_session_command(&command, RunTerminal::Cancelled, started_at.elapsed());
			return true;
		},
		Err(flume::TryRecvError::Empty) => {},
	}
	let _ = command
		.events
		.send(ExecEvent::Started { exec_id: command.exec.clone() });
	let cancel_rx = command.cancel_rx.clone();
	let setup = setup_io(
		command.pty.as_ref(),
		command.control.clone(),
		command.exec.clone(),
		command.events.clone(),
	);
	let Ok((mut params, readers)) = setup else {
		finish_session_command(&command, RunTerminal::Failed, started_at.elapsed());
		return false;
	};
	params.process_group_policy = omp_shell_engine::ProcessGroupPolicy::NewProcessGroup;
	params.set_spawn_observer(command.control.spawns.clone());
	let source_info = SourceInfo::from("env/v1 exec");
	let result = {
		let timeout = async {
			match command.timeout {
				Some(timeout) => tokio::time::sleep(timeout).await,
				None => std::future::pending().await,
			}
		};
		tokio::pin!(timeout);
		let execution = shell.run_string(command.source.to_string(), &source_info, &params);
		tokio::pin!(execution);
		tokio::select! {
			result = &mut execution => match result {
				Ok(result) => RunTerminal::Exited(i32::from(u8::from(result.exit_code))),
				Err(_error) => RunTerminal::Failed,
			},
			request = cancel_rx.recv_async() => {
				let request = request.unwrap_or(CancelRequest { grace: CANCEL_GRACE });
				command.control.spawns.terminate(request.grace).await;
				RunTerminal::Cancelled
			},
			() = &mut timeout => {
				command.control.spawns.terminate(CANCEL_GRACE).await;
				RunTerminal::Timeout
			},
		}
	};
	drop(params);
	command.control.close_input();
	for reader in readers {
		let _ = reader.await;
	}
	let cancelled = result == RunTerminal::Cancelled;
	finish_session_command(&command, result, started_at.elapsed());
	cancelled
}

fn finish_session_command(command: &SessionCommand, result: RunTerminal, elapsed: Duration) {
	let retained = command.control.retained.lock();
	command.control.finished.store(true, Ordering::Release);
	let event = ExecEvent::Exit(ExitEvent {
		exec:   command.exec.clone(),
		status: Some(result.status(elapsed)),
		props:  Default::default(),
	});
	let _ = command.events.send(event.clone());
	if let Some(process) = retained.as_ref().and_then(Weak::upgrade) {
		route_named_event(&process, event);
	}
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum RunTerminal {
	Exited(i32),
	Failed,
	Timeout,
	Cancelled,
}

impl RunTerminal {
	fn status(self, elapsed: Duration) -> ExecStatusMsg {
		let (outcome, exit_code, aborted) = match self {
			Self::Exited(code) if code == 0 => (ExecOutcome::Exited, Some(code), false),
			Self::Exited(code) => (ExecOutcome::Failed, Some(code), false),
			Self::Failed => (ExecOutcome::Failed, None, false),
			Self::Timeout => (ExecOutcome::Timeout, None, true),
			Self::Cancelled => (ExecOutcome::Cancelled, None, true),
		};
		ExecStatusMsg {
			outcome: outcome as i32,
			exit_code,
			signal: String::new(),
			wall_clock_ms: elapsed.as_millis().try_into().unwrap_or(u64::MAX),
			spilled_output: None,
			aborted,
			props: Default::default(),
		}
	}
}

fn write_input(control: &RunControl, data: Option<&[u8]>) -> Result<(), ExecError> {
	if control.finished.load(Ordering::Acquire) {
		return Err(ExecError::RunNotFound);
	}
	let mut input = control.input.lock();
	if let Some(data) = data {
		match input.as_mut().ok_or(ExecError::RunNotFound)? {
			InputSink::Pipe(writer) => writer.write_all(data)?,
			InputSink::Pty(master) => master.write_all(data)?,
		}
	} else {
		input.take();
	}
	Ok(())
}

fn setup_io(
	pty: Option<&PtySpec>,
	control: Arc<RunControl>,
	exec: Bytes,
	events: flume::Sender<ExecEvent>,
) -> Result<(ExecutionParameters, Vec<tokio::task::JoinHandle<()>>), ExecError> {
	let mut params = ExecutionParameters::default();
	let sequencer =
		Arc::new(Mutex::new(OutputSequencer { next: 1, events, control: control.clone() }));
	if let Some(pty) = pty {
		let winsize = nix::pty::Winsize {
			ws_row:    clamp_u16(pty.rows),
			ws_col:    clamp_u16(pty.columns),
			ws_xpixel: 0,
			ws_ypixel: 0,
		};
		let opened = nix::pty::openpty(Some(&winsize), None).map_err(errno_io)?;
		let master_read = opened.master.as_fd().try_clone_to_owned()?;
		let master_write = std::fs::File::from(opened.master);
		let slave = std::fs::File::from(opened.slave);
		params.set_fd(OpenFiles::STDIN_FD, OpenFile::from(slave.try_clone()?));
		params.set_fd(OpenFiles::STDOUT_FD, OpenFile::from(slave.try_clone()?));
		params.set_fd(OpenFiles::STDERR_FD, OpenFile::from(slave));
		*control.input.lock() = Some(InputSink::Pty(master_write));
		let reader =
			spawn_reader(std::fs::File::from(master_read), OutputChannel::Pty, exec, sequencer);
		Ok((params, vec![reader]))
	} else {
		let (stdin_read, stdin_write) = std::io::pipe()?;
		let (stdout_read, stdout_write) = std::io::pipe()?;
		let (stderr_read, stderr_write) = std::io::pipe()?;
		params.set_fd(OpenFiles::STDIN_FD, stdin_read.into());
		params.set_fd(OpenFiles::STDOUT_FD, stdout_write.into());
		params.set_fd(OpenFiles::STDERR_FD, stderr_write.into());
		*control.input.lock() = Some(InputSink::Pipe(stdin_write));
		Ok((params, vec![
			spawn_reader(stdout_read, OutputChannel::Stdout, exec.clone(), sequencer.clone()),
			spawn_reader(stderr_read, OutputChannel::Stderr, exec, sequencer),
		]))
	}
}

struct OutputSequencer {
	next:    u64,
	events:  flume::Sender<ExecEvent>,
	control: Arc<RunControl>,
}

fn spawn_reader<R: Read + Send + 'static>(
	mut reader: R,
	channel: OutputChannel,
	exec: Bytes,
	sequencer: Arc<Mutex<OutputSequencer>>,
) -> tokio::task::JoinHandle<()> {
	tokio::task::spawn_blocking(move || {
		let mut buffer = vec![0_u8; OUTPUT_CHUNK_BYTES];
		loop {
			let read = match reader.read(&mut buffer) {
				Ok(0) | Err(_) => break,
				Ok(read) => read,
			};
			let mut sequencer = sequencer.lock();
			let frame = OutputFrame {
				exec:     exec.clone(),
				channel:  channel as i32,
				data:     Bytes::copy_from_slice(&buffer[..read]),
				sequence: sequencer.next,
				props:    Default::default(),
			};
			sequencer.next += 1;
			let event = ExecEvent::Output(frame);
			let _ = sequencer.events.send(event.clone());
			let process = {
				let retained = sequencer.control.retained.lock();
				retained.as_ref().and_then(Weak::upgrade)
			};
			if let Some(process) = process {
				route_named_event(&process, event);
			}
		}
	})
}

async fn wait_ready_probe(process: Arc<NamedProcess>, probe: ReadyProbe) -> Result<(), ExecError> {
	let timeout = Duration::from_millis(probe.timeout_ms);
	match probe.probe {
		Some(ready_probe::Probe::Log(log)) => {
			let pattern = Regex::new(&log.pattern)
				.map_err(|error| ExecError::Readiness(Str::from(error.to_string())))?;
			let (backlog, events) = readiness_events(&process);
			let wait = async {
				let mut output = Vec::new();
				for frame in backlog {
					output.extend_from_slice(&frame.data);
				}
				if pattern.is_match(&output) {
					return Ok(());
				}
				while let Ok(event) = events.recv_async().await {
					match event {
						ProcessEvent::Output(frame) => {
							output.extend_from_slice(&frame.data);
							if pattern.is_match(&output) {
								return Ok(());
							}
						},
						ProcessEvent::State(info) if process_state_is_terminal(info.state) => {
							return Err(ExecError::Readiness(sf!(
								"process exited before its log probe passed",
							)));
						},
						_ => {},
					}
				}
				Err(ExecError::Readiness(sf!("process output closed before its log probe passed",)))
			};
			tokio::time::timeout(timeout, wait)
				.await
				.map_err(|_| ExecError::Readiness(sf!("log probe timed out")))?
		},
		Some(ready_probe::Probe::Tcp(tcp)) => {
			let port = u16::try_from(tcp.port)
				.map_err(|_| ExecError::Readiness(sf!("TCP probe port is out of range")))?;
			let wait = async {
				loop {
					if process.control.finished.load(Ordering::Acquire) {
						return Err(ExecError::Readiness(sf!(
							"process exited before its TCP probe passed",
						)));
					}
					if tokio::net::TcpStream::connect((tcp.host.as_str(), port))
						.await
						.is_ok()
					{
						return Ok(());
					}
					tokio::time::sleep(Duration::from_millis(50)).await;
				}
			};
			tokio::time::timeout(timeout, wait)
				.await
				.map_err(|_| ExecError::Readiness(sf!("TCP probe timed out")))?
		},
		Some(ready_probe::Probe::Ping(ping)) => {
			let (_, events) = readiness_events(&process);
			let encoded = HostFrame {
				request_id: 0,
				body:       Some(host_frame::Body::Ping(Ping { nonce: ping.nonce, props: None })),
				props:      None,
			}
			.encode_length_delimited_to_vec();
			write_input(&process.control, Some(&encoded))?;
			let wait = async {
				let mut output = BytesMut::new();
				while let Ok(event) = events.recv_async().await {
					match event {
						ProcessEvent::Output(frame) if frame.channel != OutputChannel::Stderr as i32 => {
							output.extend_from_slice(&frame.data);
							while let Some(worker) = take_worker_frame(&mut output)? {
								if matches!(
									&worker.body,
									Some(worker_frame::Body::Pong(pong)) if pong.nonce == ping.nonce
								) {
									return Ok(());
								}
							}
						},
						ProcessEvent::State(info) if process_state_is_terminal(info.state) => {
							return Err(ExecError::Readiness(sf!(
								"process exited before its Ping probe passed",
							)));
						},
						_ => {},
					}
				}
				Err(ExecError::Readiness(sf!("process output closed before its Ping probe passed",)))
			};
			tokio::time::timeout(timeout, wait)
				.await
				.map_err(|_| ExecError::Readiness(sf!("Ping probe timed out")))?
		},
		None => Err(ExecError::Readiness(sf!("readiness probe has no kind"))),
	}
}

fn readiness_events(process: &NamedProcess) -> (Vec<ProcessOutput>, flume::Receiver<ProcessEvent>) {
	let (sender, receiver) = flume::unbounded();
	let mut stream = process.stream.lock();
	let backlog = stream.history.clone();
	stream.subscribers.push(sender);
	(backlog, receiver)
}

fn process_state_is_terminal(state: i32) -> bool {
	matches!(
		ProcessState::try_from(state),
		Ok(ProcessState::Exited | ProcessState::Stopped | ProcessState::Failed)
	)
}

fn take_worker_frame(buffer: &mut BytesMut) -> Result<Option<WorkerFrame>, ExecError> {
	let mut length = 0_u64;
	let mut prefix = None;
	for (index, byte) in buffer.iter().copied().take(10).enumerate() {
		if index == 9 && byte > 1 {
			return Err(ExecError::Readiness(sf!("Ping probe received an invalid frame length",)));
		}
		length |= u64::from(byte & 0x7f) << (index * 7);
		if byte & 0x80 == 0 {
			prefix = Some(index + 1);
			break;
		}
	}
	let Some(prefix) = prefix else {
		if buffer.len() >= 10 {
			return Err(ExecError::Readiness(sf!("Ping probe received an invalid frame length",)));
		}
		return Ok(None);
	};
	let length = usize::try_from(length)
		.map_err(|_| ExecError::Readiness(sf!("Ping probe frame is too large")))?;
	let total = prefix
		.checked_add(length)
		.ok_or_else(|| ExecError::Readiness(sf!("Ping probe frame length overflow")))?;
	if buffer.len() < total {
		return Ok(None);
	}
	let mut framed = buffer.split_to(total);
	framed.advance(prefix);
	WorkerFrame::decode(framed)
		.map(Some)
		.map_err(|error| ExecError::Readiness(Str::from(error.to_string())))
}

async fn forward_named_process(process: Arc<NamedProcess>, run: ExecRun, _exec: Bytes) {
	while let Some(event) = run.next_event().await {
		let terminal = matches!(event, ExecEvent::Exit(_));
		route_named_event(&process, event);
		if terminal {
			break;
		}
	}
}

fn route_named_event(process: &NamedProcess, event: ExecEvent) {
	match event {
		ExecEvent::Started { .. } => {},
		ExecEvent::Output(output) => {
			let output = ProcessOutput {
				name:       process.name.to_string(),
				generation: process.generation,
				channel:    output.channel,
				data:       output.data,
				sequence:   output.sequence,
				props:      Default::default(),
			};
			let mut stream = process.stream.lock();
			stream.history.push(output.clone());
			stream.broadcast(ProcessEvent::Output(output));
		},
		ExecEvent::Exit(exit) => {
			let mut stream = process.stream.lock();
			stream.info.status = exit.status;
			stream.info.state = match stream.info.status.as_ref().map(|status| status.outcome) {
				Some(value) if value == ExecOutcome::Exited as i32 => ProcessState::Exited as i32,
				Some(value) if value == ExecOutcome::Cancelled as i32 => ProcessState::Stopped as i32,
				_ => ProcessState::Failed as i32,
			};
			let info = stream.info.clone();
			drop(process.control.input.lock());
			stream.broadcast(ProcessEvent::State(info));
		},
	}
}

impl ProcessStreamState {
	fn broadcast(&mut self, event: ProcessEvent) {
		self
			.subscribers
			.retain(|subscriber| subscriber.send(event.clone()).is_ok());
	}
}

fn apply_env_delta(
	shell: &mut Shell,
	delta: Option<&EnvironmentDelta>,
) -> Result<(), omp_shell_engine::Error> {
	let Some(delta) = delta else { return Ok(()) };
	for name in &delta.unset {
		shell.env_mut().unset(name)?;
	}
	for (name, value) in &delta.set {
		let mut variable = ShellVariable::new(value.clone());
		variable.export();
		shell.set_env_global(name, variable)?;
	}
	Ok(())
}

fn read_workspace_environment() -> WorkspaceEnvironment {
	let mut variables: Vec<_> = std::env::vars_os()
		.filter_map(|(name, value)| {
			Some((Str::from(name.into_string().ok()?), Str::from(value.into_string().ok()?)))
		})
		.collect();
	variables.sort_unstable_by(|left, right| left.0.cmp(&right.0));
	let mut hasher = blake3::Hasher::new();
	for (name, value) in &variables {
		let name = name.as_bytes();
		let value = value.as_bytes();
		hasher.update(&(name.len() as u64).to_be_bytes());
		hasher.update(name);
		hasher.update(&(value.len() as u64).to_be_bytes());
		hasher.update(value);
	}
	WorkspaceEnvironment {
		variables: variables.into(),
		digest:    WorkspaceEnvironmentDigest(*hasher.finalize().as_bytes()),
	}
}

fn cwd_from_uri(uri: &str) -> Result<Option<PathBuf>, ExecError> {
	if uri.is_empty() {
		return Ok(None);
	}
	if !uri.contains("://") {
		return Ok(Some(PathBuf::from(uri)));
	}
	let parsed = url::Url::parse(uri).map_err(|_| ExecError::InvalidCwd(Str::from(uri)))?;
	parsed
		.to_file_path()
		.map(Some)
		.map_err(|()| ExecError::InvalidCwd(Str::from(uri)))
}

fn parse_signal(name: &str) -> Result<ProcessSignal, ExecError> {
	let normalized = name.to_ascii_uppercase();
	let normalized = normalized.strip_prefix("SIG").unwrap_or(&normalized);
	match normalized {
		"HUP" => Ok(ProcessSignal::Hangup),
		"INT" => Ok(ProcessSignal::Interrupt),
		"QUIT" => Ok(ProcessSignal::Quit),
		"TERM" => Ok(ProcessSignal::Terminate),
		"KILL" => Ok(ProcessSignal::Kill),
		"USR1" => Ok(ProcessSignal::User1),
		"USR2" => Ok(ProcessSignal::User2),
		"CONT" => Ok(ProcessSignal::Continue),
		"STOP" => Ok(ProcessSignal::Stop),
		"WINCH" => Ok(ProcessSignal::WindowChanged),
		_ => Err(ExecError::UnsupportedSignal(Str::from(name))),
	}
}

fn resize_fd(fd: std::os::fd::BorrowedFd<'_>, rows: u32, columns: u32) -> Result<(), ExecError> {
	let winsize = libc::winsize {
		ws_row:    clamp_u16(rows),
		ws_col:    clamp_u16(columns),
		ws_xpixel: 0,
		ws_ypixel: 0,
	};
	// SAFETY: fd is a live PTY master and the pointer references a valid winsize.
	let result = unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ, &winsize) };
	if result == -1 {
		return Err(ExecError::Io(std::io::Error::last_os_error()));
	}
	Ok(())
}

fn clamp_u16(value: u32) -> u16 {
	value.min(u32::from(u16::MAX)) as u16
}

fn shell_error(error: omp_shell_engine::Error) -> ExecError {
	ExecError::Shell(Str::from(error.to_string()))
}

fn errno_io(error: nix::errno::Errno) -> ExecError {
	ExecError::Io(std::io::Error::from_raw_os_error(error as i32))
}
