//! Supervision and same-binary execution for Python tool workers.

use std::{
	collections::{BTreeMap, HashSet, VecDeque},
	env,
	ffi::CString,
	io::{self, Read, Write},
	num::NonZeroUsize,
	path::PathBuf,
	process::Stdio,
	sync::{
		Arc,
		atomic::{AtomicU64, Ordering},
	},
	time::Duration,
};

use bytes::{Bytes, BytesMut};
use omp_core::Str;
use omp_proto::{
	prost::Message,
	thread::v1::{Blob, Part, part},
	toolhost::v1::{
		ArgIssue, CancelTool, ExtensionDecl, HostFrame, InvokeTool, OutcomeKind, Ping, Pong,
		ProtocolError, ProtocolErrorCode, RegisterTools, ToolAborted, ToolComplete, ToolDecl,
		ToolUpdate, WorkerFrame, WorkerHello, host_frame, worker_frame,
	},
};
use pyo3::{
	exceptions::{PyKeyError, PyTypeError, PyValueError},
	prelude::*,
	types::{PyDict, PyIterator, PyList, PyModule},
	wrap_pyfunction,
};
use thiserror::Error;
use tokio::{
	io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
	process::{Child, ChildStdin, ChildStdout, Command},
	task::JoinHandle,
	time::{Instant, MissedTickBehavior},
};

/// Private argv selector used to re-enter the `omp` executable as a Python tool
/// worker.
pub const WORKER_ARG: &str = "__omp-tool-worker";

/// Python ABI revision required by this worker implementation.
pub const PYTHON_REV: &str = "3.14t";
/// Canonical import name for the opt-in built-in Python evaluation tool.
pub const PY_EVAL_MODULE: &str = "omp_py_eval";

/// Default upper bound for one encoded tool-host frame.
pub const DEFAULT_MAX_FRAME_BYTES: usize = omp_proto::bounds::FRAME_MAX_BYTES;

/// Stable identity of one extension host.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HostKey {
	/// Extension layer, such as project or user.
	pub layer:     Str,
	/// Trust or sandbox tier.
	pub tier:      Str,
	/// Stable extension identity.
	pub extension: Str,
}

impl HostKey {
	/// Builds a host identity.
	#[must_use]
	pub fn new(layer: impl Into<Str>, tier: impl Into<Str>, extension: impl Into<Str>) -> Self {
		Self { layer: layer.into(), tier: tier.into(), extension: extension.into() }
	}

	/// Returns the ordered identity fields used by scoped binding derivation.
	#[must_use]
	pub fn fields(&self) -> [&str; 3] {
		[self.layer.as_str(), self.tier.as_str(), self.extension.as_str()]
	}
}

/// Configuration of one active extension.
#[derive(Clone, Debug)]
pub struct ExtHostSpec {
	/// Stable extension identity.
	pub key:         HostKey,
	/// Python import name containing this extension's declarations.
	pub module:      Str,
	/// Explicit opt-in fate-sharing pool. Absence isolates this extension.
	pub pool:        Option<Str>,
	/// Optional site-packages directory passed through as `OMP_PY_SITE`.
	pub python_site: Option<PathBuf>,
	/// Scoped DATA socket passed only to this extension host.
	pub data_socket: Option<PathBuf>,
}

impl ExtHostSpec {
	/// Builds an isolated extension configuration.
	#[must_use]
	pub fn new(key: HostKey, module: impl Into<Str>) -> Self {
		Self { key, module: module.into(), pool: None, python_site: None, data_socket: None }
	}
}

/// Configuration for all active Python extension hosts.
#[derive(Clone, Debug)]
pub struct ExtHostConfig {
	/// Executable to re-enter. Defaults to the current executable.
	pub executable:      PathBuf,
	/// Active extensions. An empty set starts no Python process.
	pub extensions:      Vec<ExtHostSpec>,
	/// Expected workspace protobuf schema revision.
	pub schema_rev:      u32,
	/// Expected embedded Python ABI revision.
	pub python_rev:      Str,
	/// Maximum accepted encoded frame size.
	pub max_frame_bytes: NonZeroUsize,
	/// Time allowed for hello, registration, ping, and individual frame reads.
	pub health_timeout:  Duration,
	/// Idle interval between worker health probes.
	pub ping_interval:   Duration,
	/// Courtesy-interrupt grace period before the process group is killed.
	pub interrupt_grace: Duration,
	/// Initial delay after an unhealthy host.
	pub initial_backoff: Duration,
	/// Maximum delay between respawn attempts.
	pub max_backoff:     Duration,
	/// Healthy duration after which the per-host backoff resets.
	pub healthy_reset:   Duration,
}

impl ExtHostConfig {
	/// Builds the production configuration for `executable`.
	#[must_use]
	pub const fn new(executable: PathBuf) -> Self {
		Self {
			executable,
			extensions: Vec::new(),
			schema_rev: omp_proto::SCHEMA_REV,
			python_rev: Str::new_static(PYTHON_REV),
			max_frame_bytes: NonZeroUsize::new(DEFAULT_MAX_FRAME_BYTES)
				.expect("the default worker frame limit is nonzero"),
			health_timeout: Duration::from_secs(5),
			ping_interval: Duration::from_secs(15),
			interrupt_grace: Duration::from_millis(150),
			initial_backoff: Duration::from_secs(1),
			max_backoff: Duration::from_secs(30),
			healthy_reset: Duration::from_secs(30),
		}
	}

	/// Builds a configuration that re-enters the current executable.
	///
	/// # Errors
	/// Returns the operating-system error if the current executable cannot be
	/// resolved.
	pub fn current() -> io::Result<Self> {
		std::env::current_exe().map(Self::new)
	}
}

/// A Python invocation whose raw JSON has crossed the environment commitment
/// point.
///
/// This is intentionally the supervisor's only invocation input. Streaming
/// `ArgText` fragments have no representation in the tool-host API and
/// therefore cannot reach Python.
#[derive(Clone, Debug)]
pub struct CommittedToolCall {
	/// Stable call identity.
	pub call_id:   Str,
	/// Registered tool name.
	pub name:      Str,
	/// Registered tool revision.
	pub rev:       Str,
	/// Verbatim committed JSON arguments.
	pub args_json: Bytes,
	/// Maximum execution duration after the worker receives the call.
	pub deadline:  Duration,
}

/// Why the supervisor terminated an invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerAbortKind {
	/// The invocation guard was dropped or explicitly cancelled.
	Cancelled,
	/// The committed invocation exceeded its deadline.
	TimedOut,
	/// The worker exited or violated its protocol during the invocation.
	Crashed,
}

/// Terminal supervisor-owned abort truth.
#[derive(Clone, Debug)]
pub struct WorkerAbort {
	/// Call whose effects are no longer knowable.
	pub call_id:         Str,
	/// Abort classification.
	pub kind:            WorkerAbortKind,
	/// Human-readable owner diagnostic.
	pub reason:          Str,
	/// True after dispatch; false when a queued call is cancelled before
	/// dispatch.
	pub effects_unknown: bool,
}

/// Decoded terminal branch from an extension host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerOutcomeKind {
	/// Successful completion.
	Ok,
	/// Extension-declared fault.
	Faulted,
	/// Structured argument rejection.
	ArgsRejected,
	/// Aborted execution.
	Aborted,
}

/// Validated completion from an extension host.
#[derive(Clone, Debug)]
pub struct WorkerCompletion {
	/// Stable call identity.
	pub call_id:      Str,
	/// Exact terminal branch.
	pub kind:         WorkerOutcomeKind,
	/// Model-facing result parts, each with a present discriminator.
	pub parts:        Vec<Part>,
	/// Inline structured details when the worker did not spill them.
	pub details_json: Option<Bytes>,
	/// Spilled structured details when the worker did not send them inline.
	pub details_blob: Option<Blob>,
	/// Structured argument issue, present only for
	/// [`WorkerOutcomeKind::ArgsRejected`].
	pub args_issue:   Option<ArgIssue>,
	/// Whether model-facing parts may be compacted.
	pub useless:      bool,
}

/// One ordered event from a committed Python invocation.
#[derive(Clone, Debug)]
pub enum WorkerEvent {
	/// Typed JSON progress serialized by the extension.
	Update(ToolUpdate),
	/// Normal terminal completion.
	Complete(WorkerCompletion),
	/// Abnormal terminal completion owned by the supervisor.
	Aborted(WorkerAbort),
}

/// RAII handle to a Python invocation.
///
/// Dropping a live handle requests cancellation. The supervisor then kills only
/// the worker process group, reports effects-unknown, and replaces the worker
/// before it accepts the next invocation.
pub struct WorkerInvocation {
	id:               u64,
	events:           flume::Receiver<WorkerEvent>,
	commands:         flume::Sender<SupervisorCommand>,
	terminal:         bool,
	cancel_requested: bool,
}

impl WorkerInvocation {
	/// Receives the next update or terminal event.
	///
	/// # Errors
	/// Returns `RecvError` only if the supervisor shuts down without a terminal
	/// event.
	pub async fn next(&mut self) -> Result<WorkerEvent, flume::RecvError> {
		let event = self.events.recv_async().await?;
		if matches!(event, WorkerEvent::Complete(_) | WorkerEvent::Aborted(_)) {
			self.terminal = true;
		}
		Ok(event)
	}

	/// Requests cancellation while retaining the terminal event stream.
	pub fn cancel(&mut self, reason: impl Into<Str>) {
		if self.terminal || self.cancel_requested {
			return;
		}
		if self
			.commands
			.send(SupervisorCommand::Cancel { id: self.id, reason: reason.into() })
			.is_ok()
		{
			self.cancel_requested = true;
		}
	}

	/// Sends a courtesy interrupt without structurally cancelling the
	/// invocation.
	pub fn interrupt(&self, reason: impl Into<Str>) {
		if !self.terminal && !self.cancel_requested {
			let _ = self
				.commands
				.send(SupervisorCommand::Interrupt { id: self.id, reason: reason.into() });
		}
	}
}

impl Drop for WorkerInvocation {
	fn drop(&mut self) {
		if !self.terminal && !self.cancel_requested {
			let _ = self.commands.send(SupervisorCommand::Cancel {
				id:     self.id,
				reason: Str::new_static("invocation guard dropped"),
			});
		}
	}
}

/// A registered declaration and the extension host that owns it.
#[derive(Clone, Debug)]
pub struct OwnedToolDecl {
	/// Owning extension host.
	pub owner:       HostKey,
	/// Worker declaration.
	pub declaration: ToolDecl,
}

/// Independently supervises the process group for each active extension host.
pub struct ExtHostSupervisor {
	routes:          BTreeMap<(Str, Str), HostRoute>,
	registrations:   Arc<[OwnedToolDecl]>,
	next_invocation: AtomicU64,
	actors:          Vec<HostActor>,
}

impl ExtHostSupervisor {
	/// Starts and verifies every configured active extension.
	///
	/// An empty configuration is lazy: it starts no Python interpreter.
	/// Extensions share a process only when every member names the same explicit
	/// pool in the same layer and tier.
	///
	/// # Errors
	/// Returns a startup, identity, registration, or handshake error.
	pub async fn spawn(config: ExtHostConfig) -> Result<Self, WorkerError> {
		let mut groups = BTreeMap::<ProcessKey, Vec<ExtHostSpec>>::new();
		let mut identities = HashSet::with_capacity(config.extensions.len());
		for extension in config.extensions.iter().cloned() {
			validate_extension_spec(&extension)?;
			if !identities.insert(extension.key.clone()) {
				return Err(WorkerError::Protocol(Str::new_static(
					"extension host identity is configured more than once",
				)));
			}
			groups
				.entry(ProcessKey::from_spec(&extension))
				.or_default()
				.push(extension);
		}

		let mut prepared = Vec::with_capacity(groups.len());
		for (key, extensions) in groups {
			let process_config = ProcessConfig::new(&config, key, extensions)?;
			match WorkerProcess::spawn(&process_config, 1).await {
				Ok(process) => prepared.push((process_config, process)),
				Err(error) => {
					for (prepared_config, mut process) in prepared {
						process.terminate(prepared_config.interrupt_grace).await;
					}
					return Err(error);
				},
			}
		}

		let mut routes = BTreeMap::new();
		let mut registrations = Vec::new();
		let mut registration_error = None;
		'registration: for (process_config, process) in &prepared {
			for declaration in &process.registrations {
				let owner = match process_config.owner_for(declaration) {
					Ok(owner) => owner,
					Err(error) => {
						registration_error = Some(error);
						break 'registration;
					},
				};
				let Some(definition) = declaration.definition.as_ref() else {
					continue;
				};
				let route = (Str::from(definition.name.as_str()), Str::from(declaration.rev.as_str()));
				if routes
					.insert(route, (process_config.process_id.clone(), owner.clone()))
					.is_some()
				{
					registration_error = Some(WorkerError::Protocol(Str::new_static(
						"two extension hosts registered the same tool name and revision",
					)));
					break 'registration;
				}
				registrations.push(OwnedToolDecl { owner, declaration: declaration.clone() });
			}
		}
		if let Some(error) = registration_error {
			for (prepared_config, mut process) in prepared.drain(..) {
				process.terminate(prepared_config.interrupt_grace).await;
			}
			return Err(error);
		}

		let mut senders = BTreeMap::new();
		let mut actors = Vec::with_capacity(prepared.len());
		for (process_config, process) in prepared {
			let process_id = process_config.process_id.clone();
			let expected_registrations: Arc<[ToolDecl]> = process.registrations.clone().into();
			let (commands, mailbox) = flume::unbounded();
			let actor = tokio::spawn(run_supervisor(
				process_config,
				process,
				expected_registrations,
				mailbox,
				1,
			));
			senders.insert(process_id, commands.clone());
			actors.push(HostActor { commands, actor });
		}
		let routes = routes
			.into_iter()
			.map(|(route, (process_id, owner))| {
				let commands = senders
					.get(&process_id)
					.expect("every verified process has a command channel")
					.clone();
				(route, HostRoute { commands, owner })
			})
			.collect();
		Ok(Self {
			routes,
			registrations: registrations.into(),
			next_invocation: AtomicU64::new(1),
			actors,
		})
	}

	/// Returns declarations paired with their owning host identity.
	#[must_use]
	pub fn registrations(&self) -> &[OwnedToolDecl] {
		&self.registrations
	}

	/// Starts an invocation from committed raw arguments.
	///
	/// # Errors
	/// Returns [`WorkerError::NotRegistered`] when no active extension owns the
	/// exact name/revision, or [`WorkerError::Unavailable`] when its host actor
	/// has stopped.
	pub fn invoke_committed(
		&self,
		call: CommittedToolCall,
	) -> Result<WorkerInvocation, WorkerError> {
		let route = self
			.routes
			.get(&(call.name.clone(), call.rev.clone()))
			.ok_or_else(|| WorkerError::NotRegistered {
				name: call.name.clone(),
				rev:  call.rev.clone(),
			})?;
		let commands = route.commands.clone();
		let id = self.next_invocation.fetch_add(1, Ordering::Relaxed);
		let (events_tx, events) = flume::unbounded();
		commands
			.send(SupervisorCommand::Invoke {
				id,
				owner: route.owner.clone(),
				call,
				events: events_tx,
			})
			.map_err(|_| WorkerError::Unavailable)?;
		Ok(WorkerInvocation { id, events, commands, terminal: false, cancel_requested: false })
	}

	/// Stops every active host and waits for its process group to exit.
	pub async fn shutdown(self) {
		for host in &self.actors {
			let _ = host.commands.send(SupervisorCommand::Shutdown);
		}
		for host in self.actors {
			let _ = host.actor.await;
		}
	}
}

#[derive(Clone)]
struct HostRoute {
	commands: flume::Sender<SupervisorCommand>,
	owner:    HostKey,
}

struct HostActor {
	commands: flume::Sender<SupervisorCommand>,
	actor:    JoinHandle<()>,
}

/// Worker startup, transport, protocol, or embedded-Python failure.
#[derive(Debug, Error)]
pub enum WorkerError {
	/// Failed to resolve or launch the worker process.
	#[error("python tool worker I/O failed: {0}")]
	Io(#[from] io::Error),
	/// A protobuf frame was malformed.
	#[error("python tool worker sent an invalid protobuf frame: {0}")]
	Decode(#[from] omp_proto::prost::DecodeError),
	/// A protobuf frame could not be encoded.
	#[error("python tool worker frame encoding failed: {0}")]
	Encode(#[from] omp_proto::prost::EncodeError),
	/// A frame length prefix was invalid.
	#[error("python tool worker frame length prefix is invalid")]
	InvalidLength,
	/// A frame exceeded the configured bound.
	#[error("python tool worker frame is {actual} bytes; limit is {limit}")]
	FrameTooLarge {
		/// Encoded message length.
		actual: usize,
		/// Configured maximum.
		limit:  usize,
	},
	/// An encoded frame violated extension-host allocation bounds.
	#[error("python tool worker frame bounds violation: {0}")]
	FrameBounds(#[from] omp_proto::bounds::FrameBoundsError),
	/// The worker did not complete a health operation in time.
	#[error("python tool worker health check timed out")]
	HealthTimeout,
	/// The worker closed its protocol stream.
	#[error("python tool worker exited")]
	Exited,
	/// The worker used an unexpected protocol sequence.
	#[error("python tool worker protocol violation: {0}")]
	Protocol(Str),
	/// Host and worker schema revisions differed.
	#[error("python tool worker schema revision {actual} does not match host {expected}")]
	SchemaRevision {
		/// Host revision.
		expected: u32,
		/// Worker revision.
		actual:   u32,
	},
	/// Host and worker Python revisions differed.
	#[error("python tool worker Python revision {actual} does not match host {expected}")]
	PythonRevision {
		/// Host revision.
		expected: Str,
		/// Worker revision.
		actual:   Str,
	},
	/// No configured extension registered the requested exact tool identity.
	#[error("no extension host registered tool {name} at revision {rev}")]
	NotRegistered {
		/// Requested tool name.
		name: Str,
		/// Requested tool revision.
		rev:  Str,
	},
	/// A Python extension declaration or invocation failed.
	#[error("python tool extension failed: {0}")]
	Python(Str),
	/// The supervisor actor is no longer available.
	#[error("python tool worker supervisor is unavailable")]
	Unavailable,
}

impl From<PyErr> for WorkerError {
	fn from(error: PyErr) -> Self {
		Self::Python(Str::from(error.to_string()))
	}
}

enum SupervisorCommand {
	Invoke {
		id:     u64,
		owner:  HostKey,
		call:   CommittedToolCall,
		events: flume::Sender<WorkerEvent>,
	},
	Cancel {
		id:     u64,
		reason: Str,
	},
	Interrupt {
		id:     u64,
		reason: Str,
	},
	Shutdown,
}

struct PendingInvocation {
	id:        u64,
	owner:     HostKey,
	call:      CommittedToolCall,
	interrupt: Option<Str>,
	events:    flume::Sender<WorkerEvent>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum FateUnit {
	Extension(Str),
	Pool(Str),
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ProcessKey {
	layer: Str,
	tier:  Str,
	unit:  FateUnit,
}

impl ProcessKey {
	fn from_spec(spec: &ExtHostSpec) -> Self {
		let unit = spec
			.pool
			.clone()
			.map_or_else(|| FateUnit::Extension(spec.key.extension.clone()), FateUnit::Pool);
		Self { layer: spec.key.layer.clone(), tier: spec.key.tier.clone(), unit }
	}

	fn pool(&self) -> Option<&Str> {
		match &self.unit {
			FateUnit::Extension(_) => None,
			FateUnit::Pool(pool) => Some(pool),
		}
	}
}

#[derive(Clone, Debug)]
struct ProcessConfig {
	process_id:         ProcessKey,
	executable:         PathBuf,
	python_site:        Option<PathBuf>,
	modules:            Vec<Str>,
	owners:             BTreeMap<Str, HostKey>,
	data_socket:        Option<PathBuf>,
	schema_rev:         u32,
	python_rev:         Str,
	max_frame_bytes:    NonZeroUsize,
	health_timeout:     Duration,
	ping_interval:      Duration,
	interrupt_grace:    Duration,
	initial_backoff:    Duration,
	max_backoff:        Duration,
	healthy_reset:      Duration,
	session_generation: u64,
}

impl ProcessConfig {
	fn new(
		root: &ExtHostConfig,
		process_id: ProcessKey,
		extensions: Vec<ExtHostSpec>,
	) -> Result<Self, WorkerError> {
		let python_site = extensions
			.first()
			.and_then(|extension| extension.python_site.clone());
		if extensions
			.iter()
			.any(|extension| extension.python_site != python_site)
		{
			return Err(WorkerError::Protocol(Str::new_static(
				"extensions in an explicit pool must use the same Python site",
			)));
		}
		let data_socket = extensions
			.first()
			.and_then(|extension| extension.data_socket.clone());
		if extensions
			.iter()
			.any(|extension| extension.data_socket != data_socket)
		{
			return Err(WorkerError::Protocol(Str::new_static(
				"extensions in an explicit pool must use the same scoped DATA socket",
			)));
		}
		let mut owners = BTreeMap::new();
		let mut modules = Vec::with_capacity(extensions.len());
		for extension in extensions {
			if owners
				.insert(extension.module.clone(), extension.key)
				.is_some()
			{
				return Err(WorkerError::Protocol(Str::new_static(
					"an extension module is configured more than once in one host",
				)));
			}
			modules.push(extension.module);
		}
		Ok(Self {
			process_id,
			executable: root.executable.clone(),
			python_site,
			modules,
			owners,
			data_socket,
			schema_rev: root.schema_rev,
			python_rev: root.python_rev.clone(),
			max_frame_bytes: root.max_frame_bytes,
			health_timeout: root.health_timeout,
			ping_interval: root.ping_interval,
			interrupt_grace: root.interrupt_grace,
			initial_backoff: root.initial_backoff,
			max_backoff: root.max_backoff,
			healthy_reset: root.healthy_reset,
			session_generation: 1,
		})
	}

	fn owner_for(&self, declaration: &ToolDecl) -> Result<HostKey, WorkerError> {
		self
			.owners
			.get(declaration.extension_id.as_str())
			.cloned()
			.ok_or_else(|| {
				WorkerError::Protocol(Str::new_static(
					"worker registered a declaration for an unconfigured extension",
				))
			})
	}
}

fn validate_extension_spec(spec: &ExtHostSpec) -> Result<(), WorkerError> {
	if spec.key.layer.is_empty()
		|| spec.key.tier.is_empty()
		|| spec.key.extension.is_empty()
		|| spec.module.is_empty()
		|| spec.pool.as_ref().is_some_and(Str::is_empty)
	{
		return Err(WorkerError::Protocol(Str::new_static(
			"extension host identity, module, and explicit pool names must be nonempty",
		)));
	}
	Ok(())
}

struct WorkerProcess {
	child:         Child,
	stdin:         ChildStdin,
	stdout:        ChildStdout,
	read_scratch:  BytesMut,
	write_scratch: BytesMut,
	registrations: Vec<ToolDecl>,
}

impl WorkerProcess {
	async fn spawn(config: &ProcessConfig, generation: u64) -> Result<Self, WorkerError> {
		let mut command = Command::new(&config.executable);
		command
			.arg(WORKER_ARG)
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::inherit())
			.kill_on_drop(true);
		if let Some(site) = &config.python_site {
			command.env("OMP_PY_SITE", site);
		}
		if config.modules.is_empty() {
			command.env_remove("OMP_PY_MODULES");
		} else {
			let modules = config
				.modules
				.iter()
				.map(Str::as_str)
				.collect::<Vec<_>>()
				.join(",");
			command.env("OMP_PY_MODULES", modules);
		}
		if let Some(socket) = &config.data_socket {
			command.env("OMP_EXT_ENV_SOCKET", socket);
		} else {
			command.env_remove("OMP_EXT_ENV_SOCKET");
		}
		command
			.env("OMP_EXT_LAYER", config.process_id.layer.as_str())
			.env("OMP_EXT_TIER", config.process_id.tier.as_str())
			.env("OMP_EXT_HOST_GENERATION", generation.to_string())
			.env("OMP_EXT_SESSION_GENERATION", config.session_generation.to_string());
		if let Some(pool) = config.process_id.pool() {
			command.env("OMP_EXT_POOL", pool.as_str());
		} else {
			command.env_remove("OMP_EXT_POOL");
		}
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
		let stdin = child
			.stdin
			.take()
			.ok_or_else(|| WorkerError::Protocol(Str::new_static("worker stdin unavailable")))?;
		let stdout = child
			.stdout
			.take()
			.ok_or_else(|| WorkerError::Protocol(Str::new_static("worker stdout unavailable")))?;
		let mut process = Self {
			child,
			stdin,
			stdout,
			read_scratch: BytesMut::with_capacity(8 * 1024),
			write_scratch: BytesMut::with_capacity(8 * 1024),
			registrations: Vec::new(),
		};
		if let Err(error) = process.handshake(config, generation).await {
			process.terminate(config.interrupt_grace).await;
			return Err(error);
		}
		Ok(process)
	}

	async fn handshake(
		&mut self,
		config: &ProcessConfig,
		generation: u64,
	) -> Result<(), WorkerError> {
		let hello_frame = self.read_timeout(config).await?;
		let Some(worker_frame::Body::Hello(hello)) = hello_frame.body else {
			return Err(WorkerError::Protocol(Str::new_static("WorkerHello must be the first frame")));
		};
		if hello.worker_id.is_empty() {
			return Err(WorkerError::Protocol(Str::new_static("WorkerHello has no worker id")));
		}
		if hello.schema_rev != config.schema_rev {
			return Err(WorkerError::SchemaRevision {
				expected: config.schema_rev,
				actual:   hello.schema_rev,
			});
		}
		if hello.python_rev != config.python_rev.as_str() {
			return Err(WorkerError::PythonRevision {
				expected: config.python_rev.clone(),
				actual:   Str::from(hello.python_rev),
			});
		}
		if hello.api_level != 1
			|| hello.layer != config.process_id.layer.as_str()
			|| hello.tier != config.process_id.tier.as_str()
			|| hello.pool != config.process_id.pool().map_or("", Str::as_str)
			|| hello.host_version != env!("CARGO_PKG_VERSION")
			|| hello.host_generation != generation
			|| hello.session_generation != config.session_generation
		{
			return Err(WorkerError::Protocol(Str::new_static(
				"WorkerHello identity or generation did not match the spawned host",
			)));
		}
		let registrations = self.read_timeout(config).await?;
		let Some(worker_frame::Body::RegisterTools(RegisterTools {
			tools,
			generation: registration_generation,
			extensions,
			..
		})) = registrations.body
		else {
			return Err(WorkerError::Protocol(Str::new_static(
				"RegisterTools must follow WorkerHello",
			)));
		};
		if registration_generation != generation {
			return Err(WorkerError::Protocol(Str::new_static("RegisterTools generation is stale")));
		}
		let registered_extensions = extensions
			.iter()
			.map(|extension| extension.extension_id.as_str())
			.collect::<HashSet<_>>();
		if registered_extensions.len() != config.owners.len()
			|| config
				.owners
				.keys()
				.any(|module| !registered_extensions.contains(module.as_str()))
		{
			return Err(WorkerError::Protocol(Str::new_static(
				"RegisterTools extension set did not match the spawned host",
			)));
		}
		validate_registrations(&tools)?;
		self.registrations = tools;
		Ok(())
	}

	async fn read_timeout(&mut self, config: &ProcessConfig) -> Result<WorkerFrame, WorkerError> {
		tokio::time::timeout(
			config.health_timeout,
			read_async_frame(&mut self.stdout, config.max_frame_bytes, &mut self.read_scratch),
		)
		.await
		.map_err(|_| WorkerError::HealthTimeout)?
		.and_then(|frame| frame.ok_or(WorkerError::Exited))
	}

	async fn write(&mut self, frame: &HostFrame, config: &ProcessConfig) -> Result<(), WorkerError> {
		write_async_frame(&mut self.stdin, frame, config.max_frame_bytes, &mut self.write_scratch)
			.await
	}

	fn courtesy_interrupt(&self) {
		let pid = self.child.id();
		#[cfg(unix)]
		if let Some(pid) = pid {
			let _ = nix::sys::signal::killpg(
				nix::unistd::Pid::from_raw(pid.cast_signed()),
				nix::sys::signal::Signal::SIGINT,
			);
		}
		#[cfg(windows)]
		if let Some(pid) = pid {
			unsafe {
				let _ = windows_sys::Win32::System::Console::GenerateConsoleCtrlEvent(
					windows_sys::Win32::System::Console::CTRL_BREAK_EVENT,
					pid,
				);
			}
		}
	}

	async fn terminate(&mut self, grace: Duration) {
		let pid = self.child.id();
		self.courtesy_interrupt();
		if tokio::time::timeout(grace, self.child.wait()).await.is_ok() {
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
			// `start_kill` is the hard fallback on Windows. The worker is a new
			// process-group leader, so the courtesy CTRL_BREAK reaches descendants.
			let _ = self.child.start_kill();
		}
		let _ = self.child.wait().await;
	}
}

async fn run_supervisor(
	config: ProcessConfig,
	mut process: WorkerProcess,
	expected_registrations: Arc<[ToolDecl]>,
	mailbox: flume::Receiver<SupervisorCommand>,
	mut generation: u64,
) {
	let mut pending = VecDeque::new();
	let mut ping_nonce = 1_u64;
	let mut ping_tick = tokio::time::interval(config.ping_interval);
	let mut healthy_since = Instant::now();
	let mut backoff = initial_backoff(&config);
	ping_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
	ping_tick.tick().await;
	loop {
		if let Some(invocation) = pending.pop_front() {
			match run_invocation(&config, &mut process, invocation, &mailbox, &mut pending).await {
				InvocationAction::KeepWorker => {},
				InvocationAction::ReplaceWorker => {
					if healthy_since.elapsed() >= config.healthy_reset {
						backoff = initial_backoff(&config);
					}
					process.terminate(config.interrupt_grace).await;
					process =
						respawn(&config, &expected_registrations, &mut generation, &mut backoff).await;
					healthy_since = Instant::now();
				},
				InvocationAction::Shutdown => {
					process.terminate(config.interrupt_grace).await;
					return;
				},
			}
			continue;
		}

		tokio::select! {
			command = mailbox.recv_async() => match command {
				Ok(SupervisorCommand::Invoke { id, owner, call, events }) => {
					pending.push_back(PendingInvocation { id, owner, call, interrupt: None, events });
				},
				Ok(SupervisorCommand::Cancel { .. } | SupervisorCommand::Interrupt { .. }) => {},
				Ok(SupervisorCommand::Shutdown) | Err(_) => {
					process.terminate(config.interrupt_grace).await;
					return;
				},
			},
			_ = ping_tick.tick() => {
				let frame = HostFrame {
					request_id: 0,
					body: Some(host_frame::Body::Ping(Ping { nonce: ping_nonce, props: None })),
					props: None,
				};
				let healthy = process.write(&frame, &config).await.is_ok()
					&& matches!(process.read_timeout(&config).await,
						Ok(WorkerFrame { body: Some(worker_frame::Body::Pong(Pong { nonce, .. })), .. }) if nonce == ping_nonce);
				ping_nonce = ping_nonce.wrapping_add(1).max(1);
				if !healthy {
					if healthy_since.elapsed() >= config.healthy_reset {
						backoff = initial_backoff(&config);
					}
					process.terminate(config.interrupt_grace).await;
					process = respawn(
						&config,
						&expected_registrations,
						&mut generation,
						&mut backoff,
					)
					.await;
					healthy_since = Instant::now();
				}
			},
		}
	}
}

enum InvocationAction {
	KeepWorker,
	ReplaceWorker,
	Shutdown,
}

async fn run_invocation(
	config: &ProcessConfig,
	process: &mut WorkerProcess,
	mut invocation: PendingInvocation,
	mailbox: &flume::Receiver<SupervisorCommand>,
	pending: &mut VecDeque<PendingInvocation>,
) -> InvocationAction {
	let id = invocation.id;
	let call_id = invocation.call.call_id.clone();
	loop {
		match mailbox.try_recv() {
			Ok(SupervisorCommand::Cancel { id: cancelled, reason }) if cancelled == id => {
				let _ = invocation.events.send(WorkerEvent::Aborted(WorkerAbort {
					call_id,
					kind: WorkerAbortKind::Cancelled,
					reason,
					effects_unknown: false,
				}));
				return InvocationAction::KeepWorker;
			},
			Ok(SupervisorCommand::Invoke { id, owner, call, events }) => {
				pending.push_back(PendingInvocation { id, owner, call, interrupt: None, events });
			},
			Ok(SupervisorCommand::Cancel { id, reason }) => {
				cancel_pending(pending, id, reason);
			},
			Ok(SupervisorCommand::Interrupt { id: interrupted, reason }) if interrupted == id => {
				invocation.interrupt = Some(reason);
			},
			Ok(SupervisorCommand::Interrupt { id, reason }) => {
				interrupt_pending(pending, id, reason);
			},
			Ok(SupervisorCommand::Shutdown) => return InvocationAction::Shutdown,
			Err(flume::TryRecvError::Empty) => break,
			Err(flume::TryRecvError::Disconnected) => return InvocationAction::Shutdown,
		}
	}
	if invocation.events.is_disconnected() {
		return InvocationAction::KeepWorker;
	}
	let request_id = invocation_id(&call_id);
	let frame = HostFrame {
		request_id,
		body: Some(host_frame::Body::InvokeTool(InvokeTool {
			call_id:     call_id.as_str().to_owned(),
			name:        invocation.call.name.as_str().to_owned(),
			args_json:   invocation.call.args_json.clone(),
			deadline_ms: invocation
				.call
				.deadline
				.as_millis()
				.try_into()
				.unwrap_or(u64::MAX),
			rev:         invocation.call.rev.as_str().to_owned(),
			props:       None,
		})),
		props: None,
	};

	if process.write(&frame, config).await.is_err() {
		send_abort(
			&invocation,
			WorkerAbortKind::Crashed,
			"worker exited before accepting invocation",
		);
		return InvocationAction::ReplaceWorker;
	}
	if let Some(reason) = invocation.interrupt.as_ref() {
		interrupt_worker(process, config, request_id, &call_id, reason.as_str()).await;
	}
	let deadline = Instant::now() + invocation.call.deadline;
	loop {
		tokio::select! {
			frame = read_async_frame::<_, WorkerFrame>(&mut process.stdout, config.max_frame_bytes, &mut process.read_scratch) => {
				let Ok(Some(frame)) = frame else {
					send_abort(&invocation, WorkerAbortKind::Crashed, "worker exited during invocation");
					return InvocationAction::ReplaceWorker;
				};
				if frame.request_id != request_id {
					send_abort(&invocation, WorkerAbortKind::Crashed, "worker response request id did not match invocation");
					return InvocationAction::ReplaceWorker;
				}
				match frame.body {
					Some(worker_frame::Body::ToolUpdate(update)) if update.call_id == call_id.as_str() => {
						if invocation.events.send(WorkerEvent::Update(update)).is_err() {
							cancel_worker(process, config, request_id, &call_id, "invocation receiver dropped").await;
							return InvocationAction::ReplaceWorker;
						}
					},
					Some(worker_frame::Body::ToolComplete(complete)) if complete.call_id == call_id.as_str() => {
						match WorkerCompletion::try_from(complete) {
							Ok(complete) => {
								let _ = invocation.events.send(WorkerEvent::Complete(complete));
								return InvocationAction::KeepWorker;
							},
							Err(_) => {
								send_abort(
									&invocation,
									WorkerAbortKind::Crashed,
									"worker sent an invalid ToolComplete",
								);
								return InvocationAction::ReplaceWorker;
							},
						}
					},
					Some(worker_frame::Body::ToolAborted(aborted)) if aborted.call_id == call_id.as_str() => {
						let _ = invocation.events.send(WorkerEvent::Aborted(WorkerAbort {
							call_id,
							kind: WorkerAbortKind::Crashed,
							reason: Str::from(aborted.reason),
							effects_unknown: true,
						}));
						return InvocationAction::ReplaceWorker;
					},
					_ => {
						send_abort(&invocation, WorkerAbortKind::Crashed, "worker sent an invalid invocation frame");
						return InvocationAction::ReplaceWorker;
					},
				}
			},
			command = mailbox.recv_async() => match command {
				Ok(SupervisorCommand::Cancel { id: cancelled, reason }) if cancelled == id => {
					cancel_worker(process, config, request_id, &call_id, reason.as_str()).await;
					let reason = cancellation_reason(config, &invocation.owner, reason.as_str());
					send_abort(&invocation, WorkerAbortKind::Cancelled, reason.as_str());
					return InvocationAction::ReplaceWorker;
				},
				Ok(SupervisorCommand::Invoke { id, owner, call, events }) => {
					pending.push_back(PendingInvocation { id, owner, call, interrupt: None, events });
				},
				Ok(SupervisorCommand::Cancel { id, reason }) => {
					cancel_pending(pending, id, reason);
				},
				Ok(SupervisorCommand::Interrupt { id: interrupted, reason }) if interrupted == id => {
					interrupt_worker(process, config, request_id, &call_id, reason.as_str()).await;
				},
				Ok(SupervisorCommand::Interrupt { id, reason }) => {
					interrupt_pending(pending, id, reason);
				},
				Ok(SupervisorCommand::Shutdown) | Err(_) => return InvocationAction::Shutdown,
			},
			() = tokio::time::sleep_until(deadline) => {
				cancel_worker(process, config, request_id, &call_id, "worker invocation timed out").await;
				send_abort(&invocation, WorkerAbortKind::TimedOut, "worker invocation timed out");
				return InvocationAction::ReplaceWorker;
			},
		}
	}
}

async fn interrupt_worker(
	process: &mut WorkerProcess,
	config: &ProcessConfig,
	request_id: u64,
	call_id: &Str,
	reason: &str,
) {
	let _ = process
		.write(
			&HostFrame {
				request_id,
				body: Some(host_frame::Body::CancelTool(CancelTool {
					call_id: call_id.as_str().to_owned(),
					reason:  reason.to_owned(),
					props:   None,
				})),
				props: None,
			},
			config,
		)
		.await;
	process.courtesy_interrupt();
}

async fn cancel_worker(
	process: &mut WorkerProcess,
	config: &ProcessConfig,
	request_id: u64,
	call_id: &Str,
	reason: &str,
) {
	let _ = process
		.write(
			&HostFrame {
				request_id,
				body: Some(host_frame::Body::CancelTool(CancelTool {
					call_id: call_id.as_str().to_owned(),
					reason:  reason.to_owned(),
					props:   None,
				})),
				props: None,
			},
			config,
		)
		.await;
	process.terminate(config.interrupt_grace).await;
}

fn cancel_pending(pending: &mut VecDeque<PendingInvocation>, id: u64, reason: Str) {
	if let Some(index) = pending.iter().position(|invocation| invocation.id == id) {
		let invocation = pending
			.remove(index)
			.expect("the located queued invocation exists");
		let _ = invocation.events.send(WorkerEvent::Aborted(WorkerAbort {
			call_id: invocation.call.call_id,
			kind: WorkerAbortKind::Cancelled,
			reason,
			effects_unknown: false,
		}));
	}
}

fn interrupt_pending(pending: &mut VecDeque<PendingInvocation>, id: u64, reason: Str) {
	if let Some(invocation) = pending.iter_mut().find(|invocation| invocation.id == id) {
		invocation.interrupt = Some(reason);
	}
}

fn send_abort(invocation: &PendingInvocation, kind: WorkerAbortKind, reason: &str) {
	let _ = invocation.events.send(WorkerEvent::Aborted(WorkerAbort {
		call_id: invocation.call.call_id.clone(),
		kind,
		reason: Str::from(reason),
		effects_unknown: true,
	}));
}

fn initial_backoff(config: &ProcessConfig) -> Duration {
	config
		.initial_backoff
		.max(Duration::from_millis(1))
		.min(config.max_backoff.max(Duration::from_millis(1)))
}

fn cancellation_reason(config: &ProcessConfig, owner: &HostKey, reason: &str) -> Str {
	if let Some(pool) = config.process_id.pool() {
		Str::from(format!(
			"{reason}; effects unknown for {}; explicit pool {pool} fate-sharing terminated sibling \
			 extension calls",
			owner.extension,
		))
	} else {
		Str::from(format!(
			"{reason}; effects unknown for {}; no other extension host was terminated",
			owner.extension,
		))
	}
}

async fn respawn(
	config: &ProcessConfig,
	expected: &[ToolDecl],
	generation: &mut u64,
	backoff: &mut Duration,
) -> WorkerProcess {
	let max_delay = config.max_backoff.max(Duration::from_millis(1));
	loop {
		tokio::time::sleep(*backoff).await;
		*generation = generation.wrapping_add(1).max(1);
		match WorkerProcess::spawn(config, *generation).await {
			Ok(process) if process.registrations.as_slice() == expected => {
				*backoff = backoff.saturating_mul(2).min(max_delay);
				return process;
			},
			Ok(mut process) => process.terminate(config.interrupt_grace).await,
			Err(_) => {},
		}
		*backoff = backoff.saturating_mul(2).min(max_delay);
	}
}
impl TryFrom<ToolComplete> for WorkerCompletion {
	type Error = WorkerError;

	fn try_from(complete: ToolComplete) -> Result<Self, Self::Error> {
		if complete.parts.iter().any(|part| part.kind.is_none()) {
			return Err(WorkerError::Protocol(Str::new_static(
				"ToolComplete contains a part without its presence discriminator",
			)));
		}
		let has_json = !complete.details_json.is_empty();
		let has_blob = complete.details_blob.is_some();
		if has_json == has_blob {
			return Err(WorkerError::Protocol(Str::new_static(
				"ToolComplete must carry exactly one of details_json or details_blob",
			)));
		}
		let kind = match OutcomeKind::try_from(complete.kind).unwrap_or(OutcomeKind::Unspecified) {
			OutcomeKind::Unspecified if complete.is_error => WorkerOutcomeKind::Faulted,
			OutcomeKind::Unspecified => WorkerOutcomeKind::Ok,
			OutcomeKind::Ok => WorkerOutcomeKind::Ok,
			OutcomeKind::Faulted => WorkerOutcomeKind::Faulted,
			OutcomeKind::ArgsRejected => WorkerOutcomeKind::ArgsRejected,
			OutcomeKind::Aborted => WorkerOutcomeKind::Aborted,
		};
		if matches!(kind, WorkerOutcomeKind::ArgsRejected) != complete.args_issue.is_some() {
			return Err(WorkerError::Protocol(Str::new_static(
				"ToolComplete args_issue presence does not match ArgsRejected",
			)));
		}
		Ok(WorkerCompletion {
			call_id: Str::from(complete.call_id),
			kind,
			parts: complete.parts,
			details_json: has_json.then_some(complete.details_json),
			details_blob: complete.details_blob,
			args_issue: complete.args_issue,
			useless: complete.useless,
		})
	}
}

fn validate_registrations(tools: &[ToolDecl]) -> Result<(), WorkerError> {
	let mut names = HashSet::with_capacity(tools.len());
	for tool in tools {
		let Some(definition) = &tool.definition else {
			return Err(WorkerError::Protocol(Str::new_static("registered tool has no definition")));
		};
		if definition.name.is_empty() || tool.rev.is_empty() {
			return Err(WorkerError::Protocol(Str::new_static(
				"registered tool name and revision must be nonempty",
			)));
		}
		if serde_json::from_slice::<serde_json::Value>(&definition.schema_json).is_err() {
			return Err(WorkerError::Protocol(Str::from(format!(
				"worker registered invalid JSON Schema for {}",
				definition.name
			))));
		}
		if !names.insert(definition.name.as_str()) {
			return Err(WorkerError::Protocol(Str::from(format!(
				"worker registered duplicate tool name: {}",
				definition.name
			))));
		}
	}
	Ok(())
}

fn invocation_id(call_id: &str) -> u64 {
	let mut hash = 0xcbf2_9ce4_8422_2325_u64;
	for byte in call_id.bytes() {
		hash ^= u64::from(byte);
		hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
	}
	hash.max(1)
}

#[pyfunction]
fn evaluate_python_expression<'py>(
	py: Python<'py>,
	params: &Bound<'py, PyDict>,
) -> PyResult<Bound<'py, PyDict>> {
	let code = params
		.get_item("code")?
		.ok_or_else(|| PyKeyError::new_err("py_eval requires code"))?
		.extract::<String>()?;
	if code.is_empty() {
		return Err(PyValueError::new_err("py_eval code must be nonempty"));
	}
	let code =
		CString::new(code).map_err(|_| PyValueError::new_err("py_eval code contains a null byte"))?;
	let globals = PyDict::new(py);
	globals.set_item("__builtins__", PyModule::import(py, "builtins")?)?;
	let value = py.eval(code.as_c_str(), Some(&globals), Some(&globals))?;
	let json = PyModule::import(py, "json")?;
	let json_options = PyDict::new(py);
	json_options.set_item("allow_nan", false)?;
	let details = PyDict::new(py);
	if json
		.getattr("dumps")?
		.call((&value,), Some(&json_options))
		.is_ok()
	{
		details.set_item("result", value)?;
	} else {
		details.set_item("result", value.repr()?.to_str()?)?;
	}
	let completion = PyDict::new(py);
	completion.set_item("details", details)?;
	Ok(completion)
}

#[pymodule(gil_used = false)]
fn omp_py_eval(m: &Bound<'_, PyModule>) -> PyResult<()> {
	let py = m.py();
	let declaration = PyDict::new(py);
	declaration.set_item("name", "py_eval")?;
	declaration.set_item("description", "Evaluate one Python expression")?;
	declaration.set_item(
		"schema",
		r#"{"type":"object","properties":{"code":{"type":"string","minLength":1}},"required":["code"],"additionalProperties":false}"#,
	)?;
	declaration.set_item("rev", "1")?;
	declaration.set_item("strict", true)?;
	declaration.set_item("handler", wrap_pyfunction!(evaluate_python_expression, m)?)?;
	m.add("OMP_TOOLS", PyList::new(py, [declaration])?)
}

/// Boots embedded Python, imports configured extension modules, registers their
/// declarations, and serves toolhost/v1 on stdin/stdout.
///
/// `OMP_PY_SITE` selects the optional site-packages directory.
/// `OMP_PY_MODULES` is the comma-separated list of import names enabled for
/// this worker. Every module may expose `OMP_TOOLS`, an iterable of declaration
/// dictionaries with `name`, `description`, `schema`, `rev`, `strict`, and
/// callable `handler` entries.
///
/// # Errors
/// Returns a worker startup, extension import, or stdio protocol error.
pub fn run_worker_entry() -> Result<(), WorkerError> {
	let modules = configured_modules();
	if modules
		.iter()
		.any(|module| module.as_str() == PY_EVAL_MODULE)
	{
		pyo3::append_to_inittab!(omp_py_eval);
	}
	let engine = omp_py::Engine::builder()
		.init()
		.map_err(|error| WorkerError::Python(Str::from(error.to_string())))?;
	serve_worker(&engine, &modules)
}

fn serve_worker(engine: &omp_py::Engine, modules: &[Str]) -> Result<(), WorkerError> {
	engine.attach(|py| -> PyResult<()> {
		let sys = PyModule::import(py, "sys")?;
		sys.setattr("stdout", sys.getattr("stderr")?)?;
		Ok(())
	})?;
	let layer = required_env("OMP_EXT_LAYER")?;
	let tier = required_env("OMP_EXT_TIER")?;
	let pool = env::var("OMP_EXT_POOL").unwrap_or_default();
	let host_generation = required_env_u64("OMP_EXT_HOST_GENERATION")?;
	let session_generation = required_env_u64("OMP_EXT_SESSION_GENERATION")?;
	let tools = load_tools(engine, modules)?;
	let declarations = tools.iter().map(|tool| tool.decl.clone()).collect();
	let extensions = modules
		.iter()
		.map(|module| ExtensionDecl {
			extension_id: module.to_string(),
			version:      env!("CARGO_PKG_VERSION").to_owned(),
			api_level:    1,
			capabilities: Vec::new(),
			props:        None,
		})
		.collect();
	let stdin = io::stdin();
	let stdout = io::stdout();
	let mut reader = stdin.lock();
	let mut writer = stdout.lock();
	let mut read_scratch = BytesMut::with_capacity(8 * 1024);
	let mut write_scratch = BytesMut::with_capacity(8 * 1024);
	let limit = NonZeroUsize::new(DEFAULT_MAX_FRAME_BYTES)
		.expect("the default worker frame limit is nonzero");
	write_sync_frame(
		&mut writer,
		&WorkerFrame {
			request_id: 0,
			body:       Some(worker_frame::Body::Hello(WorkerHello {
				schema_rev: omp_proto::SCHEMA_REV,
				python_rev: PYTHON_REV.to_owned(),
				worker_id: Bytes::copy_from_slice(&std::process::id().to_be_bytes()),
				api_level: 1,
				layer: layer.clone(),
				tier: tier.clone(),
				pool: pool.clone(),
				host_version: env!("CARGO_PKG_VERSION").to_owned(),
				host_generation,
				session_generation,
				props: None,
			})),
			props:      None,
		},
		limit,
		&mut write_scratch,
	)?;
	write_sync_frame(
		&mut writer,
		&WorkerFrame {
			request_id: 0,
			body:       Some(worker_frame::Body::RegisterTools(RegisterTools {
				tools: declarations,
				generation: host_generation,
				extensions,
				props: None,
				..Default::default()
			})),
			props:      None,
		},
		limit,
		&mut write_scratch,
	)?;
	loop {
		let Some(frame) = read_sync_frame::<_, HostFrame>(&mut reader, limit, &mut read_scratch)?
		else {
			return Ok(());
		};
		match frame.body {
			Some(host_frame::Body::InvokeTool(invoke)) => {
				serve_invocation(
					engine,
					&tools,
					frame.request_id,
					invoke,
					&mut writer,
					limit,
					&mut write_scratch,
				)?;
			},
			Some(host_frame::Body::Ping(ping)) => write_sync_frame(
				&mut writer,
				&WorkerFrame {
					request_id: frame.request_id,
					body:       Some(worker_frame::Body::Pong(Pong { nonce: ping.nonce, props: None })),
					props:      None,
				},
				limit,
				&mut write_scratch,
			)?,
			Some(host_frame::Body::CancelTool(cancel)) => write_sync_frame(
				&mut writer,
				&WorkerFrame {
					request_id: frame.request_id,
					body:       Some(worker_frame::Body::ToolAborted(ToolAborted {
						call_id:         cancel.call_id,
						reason:          cancel.reason,
						effects_unknown: true,
						props:           None,
					})),
					props:      None,
				},
				limit,
				&mut write_scratch,
			)?,
			Some(_) => write_protocol_error(
				&mut writer,
				frame.request_id,
				ProtocolErrorCode::Unsupported,
				"host frame operation is not supported by the v1 worker",
				limit,
				&mut write_scratch,
			)?,
			None => write_protocol_error(
				&mut writer,
				frame.request_id,
				ProtocolErrorCode::InvalidArgument,
				"host frame has no body",
				limit,
				&mut write_scratch,
			)?,
		}
	}
}
fn required_env(name: &'static str) -> Result<String, WorkerError> {
	env::var(name).map_err(|_| {
		WorkerError::Protocol(Str::from(format!(
			"worker process is missing required identity variable {name}",
		)))
	})
}

fn required_env_u64(name: &'static str) -> Result<u64, WorkerError> {
	required_env(name)?
		.parse()
		.map_err(|_| WorkerError::Protocol(Str::from(format!("{name} is not an unsigned integer"))))
}

struct PythonTool {
	decl:    ToolDecl,
	handler: Py<PyAny>,
}

fn configured_modules() -> Vec<Str> {
	env::var("OMP_PY_MODULES")
		.unwrap_or_default()
		.split(',')
		.map(str::trim)
		.filter(|module| !module.is_empty())
		.map(Str::from)
		.collect()
}

fn load_tools(engine: &omp_py::Engine, modules: &[Str]) -> Result<Vec<PythonTool>, WorkerError> {
	engine
		.attach(|py| {
			let json = PyModule::import(py, "json")?;
			let mut tools = Vec::new();
			let mut names = HashSet::new();
			for module_name in modules {
				let module = PyModule::import(py, module_name.as_str())?;
				let Ok(declarations) = module.getattr("OMP_TOOLS") else {
					continue;
				};
				for declaration in PyIterator::from_object(&declarations)? {
					let declaration = declaration?;
					let dict = declaration
						.cast::<PyDict>()
						.map_err(|_| PyTypeError::new_err("OMP_TOOLS entries must be dictionaries"))?;
					let name = required_string(dict, "name")?;
					if !names.insert(name.clone()) {
						return Err(PyKeyError::new_err(format!("duplicate Python tool name: {name}")));
					}
					let description = optional_string(dict, "description")?.unwrap_or_default();
					let rev = optional_string(dict, "rev")?.unwrap_or_else(|| "1".to_owned());
					let strict = dict
						.get_item("strict")?
						.map(|value| value.extract::<bool>())
						.transpose()?;
					let schema_json = match dict.get_item("schema")? {
						Some(schema) if schema.is_instance_of::<omp_py::pyo3::types::PyString>() => {
							Bytes::from(schema.extract::<String>()?)
						},
						Some(schema) => Bytes::from(
							json
								.getattr("dumps")?
								.call1((schema,))?
								.extract::<String>()?,
						),
						None => omp_tool::schema::<BTreeMap<String, serde_json::Value>>(),
					};
					let handler = dict.get_item("handler")?.ok_or_else(|| {
						PyKeyError::new_err(format!("Python tool {name} has no handler"))
					})?;
					if !handler.is_callable() {
						return Err(PyTypeError::new_err(format!(
							"Python tool {name} handler is not callable"
						)));
					}
					tools.push(PythonTool {
						decl:    ToolDecl {
							definition: Some(omp_proto::inference::v1::ToolDef {
								name,
								description,
								schema_json,
								strict,
							}),
							rev,
							constraint: None,
							extension_id: module_name.to_string(),
							props: None,
							..Default::default()
						},
						handler: handler.unbind(),
					});
				}
			}
			Ok(tools)
		})
		.map_err(WorkerError::from)
}

fn required_string(dict: &Bound<'_, PyDict>, key: &str) -> PyResult<String> {
	dict
		.get_item(key)?
		.ok_or_else(|| PyKeyError::new_err(format!("Python tool declaration has no {key}")))?
		.extract()
}

fn optional_string(dict: &Bound<'_, PyDict>, key: &str) -> PyResult<Option<String>> {
	dict.get_item(key)?.map(|value| value.extract()).transpose()
}

fn serve_invocation<W: Write>(
	engine: &omp_py::Engine,
	tools: &[PythonTool],
	request_id: u64,
	invoke: InvokeTool,
	writer: &mut W,
	limit: NonZeroUsize,
	scratch: &mut BytesMut,
) -> Result<(), WorkerError> {
	let Some(tool) = tools.iter().find(|tool| {
		tool
			.decl
			.definition
			.as_ref()
			.is_some_and(|definition| definition.name == invoke.name)
			&& tool.decl.rev == invoke.rev
	}) else {
		return write_protocol_error(
			writer,
			request_id,
			ProtocolErrorCode::NotFound,
			"Python tool name/revision is not registered",
			limit,
			scratch,
		);
	};
	let call_id = invoke.call_id.clone();
	let result = engine.attach(|py| -> Result<PythonCompletion, WorkerError> {
		let json = PyModule::import(py, "json")?;
		let args = std::str::from_utf8(invoke.args_json.as_ref())
			.map_err(|_| WorkerError::Python(Str::new_static("committed args are not UTF-8")))?;
		let params = json.getattr("loads")?.call1((args,))?;
		let mut value = tool.handler.bind(py).call1((params,))?;
		let inspect = PyModule::import(py, "inspect")?;
		if inspect
			.getattr("isawaitable")?
			.call1((&value,))?
			.is_truthy()?
		{
			value = PyModule::import(py, "asyncio")?
				.getattr("run")?
				.call1((value,))?;
		}
		if let Ok(dict) = value.cast::<PyDict>() {
			if let Some(updates) = dict.get_item("updates")? {
				for update in PyIterator::from_object(&updates)? {
					write_update(writer, request_id, &call_id, &json, &update?, limit, scratch)?;
				}
			}
			return completion_from_dict(dict, &json);
		}
		if let Ok(iterator) = PyIterator::from_object(&value)
			&& iterator.as_any().is(&value)
		{
			for item in iterator {
				let item = item?;
				if let Ok(dict) = item.cast::<PyDict>()
					&& let Some(complete) = dict.get_item("complete")?
				{
					let complete = complete.cast::<PyDict>().map_err(|_| {
						PyTypeError::new_err("generator complete value must be a dictionary")
					})?;
					return completion_from_dict(complete, &json);
				}
				let update = if let Ok(dict) = item.cast::<PyDict>() {
					dict.get_item("update")?.unwrap_or_else(|| item.clone())
				} else {
					item
				};
				write_update(writer, request_id, &call_id, &json, &update, limit, scratch)?;
			}
			return Ok(PythonCompletion {
				parts:        Vec::new(),
				details_json: Bytes::from_static(b"null"),
				kind:         OutcomeKind::Ok,
			});
		}
		let details_json = Bytes::from(
			json
				.getattr("dumps")?
				.call1((&value,))?
				.extract::<String>()?,
		);
		let text = value.str()?.to_string_lossy().into_owned();
		Ok(PythonCompletion { parts: vec![text_part(text)], details_json, kind: OutcomeKind::Ok })
	});
	let completion = match result {
		Ok(completion) => completion,
		Err(error) => PythonCompletion {
			parts:        vec![text_part(error.to_string())],
			details_json: Bytes::from(
				serde_json::to_vec(&serde_json::json!({
					"kind": "effects_unknown",
					"reason": error.to_string(),
				}))
				.expect("serializing a string abort cannot fail"),
			),
			kind:         OutcomeKind::Aborted,
		},
	};
	write_sync_frame(
		writer,
		&WorkerFrame {
			request_id,
			body: Some(worker_frame::Body::ToolComplete(ToolComplete {
				call_id,
				parts: completion.parts,
				details_json: completion.details_json,
				is_error: matches!(completion.kind, OutcomeKind::Faulted),
				kind: completion.kind.into(),
				props: None,
				..Default::default()
			})),
			props: None,
		},
		limit,
		scratch,
	)
}

struct PythonCompletion {
	parts:        Vec<Part>,
	details_json: Bytes,
	kind:         OutcomeKind,
}

fn completion_from_dict(
	dict: &Bound<'_, PyDict>,
	json: &Bound<'_, PyModule>,
) -> Result<PythonCompletion, WorkerError> {
	let parts = match dict.get_item("parts")? {
		Some(parts) => PyIterator::from_object(&parts)?
			.map(|part| {
				part
					.and_then(|part| part.extract::<String>())
					.map(text_part)
			})
			.collect::<PyResult<Vec<_>>>()?,
		None => Vec::new(),
	};
	let details_json = match dict.get_item("details")? {
		Some(details) => {
			let options = PyDict::new(dict.py());
			options.set_item("separators", (",", ":"))?;
			Bytes::from(
				json
					.getattr("dumps")?
					.call((&details,), Some(&options))?
					.extract::<String>()?,
			)
		},
		None => Bytes::from_static(b"null"),
	};
	let kind = if dict
		.get_item("is_error")?
		.map(|value| value.extract::<bool>())
		.transpose()?
		.unwrap_or(false)
	{
		OutcomeKind::Faulted
	} else {
		OutcomeKind::Ok
	};
	Ok(PythonCompletion { parts, details_json, kind })
}

fn write_update<W: Write>(
	writer: &mut W,
	request_id: u64,
	call_id: &str,
	json: &Bound<'_, PyModule>,
	update: &Bound<'_, PyAny>,
	limit: NonZeroUsize,
	scratch: &mut BytesMut,
) -> Result<(), WorkerError> {
	let bytes = Bytes::from(
		json
			.getattr("dumps")?
			.call1((update,))?
			.extract::<String>()?,
	);
	write_sync_frame(
		writer,
		&WorkerFrame {
			request_id,
			body: Some(worker_frame::Body::ToolUpdate(ToolUpdate {
				call_id: call_id.to_owned(),
				json:    bytes,
				props:   None,
			})),
			props: None,
		},
		limit,
		scratch,
	)
}

const fn text_part(text: String) -> Part {
	Part { kind: Some(part::Kind::Text(text)) }
}

fn write_protocol_error<W: Write>(
	writer: &mut W,
	request_id: u64,
	code: ProtocolErrorCode,
	message: &str,
	limit: NonZeroUsize,
	scratch: &mut BytesMut,
) -> Result<(), WorkerError> {
	write_sync_frame(
		writer,
		&WorkerFrame {
			request_id,
			body: Some(worker_frame::Body::Error(ProtocolError {
				code:    code as i32,
				message: message.to_owned(),
				props:   None,
			})),
			props: None,
		},
		limit,
		scratch,
	)
}

trait BoundedFrame: Message + Default {
	fn validate_raw(bytes: &[u8]) -> Result<(), omp_proto::bounds::FrameBoundsError>;
}

impl BoundedFrame for HostFrame {
	fn validate_raw(bytes: &[u8]) -> Result<(), omp_proto::bounds::FrameBoundsError> {
		omp_proto::bounds::validate_host_frame(bytes)
	}
}

impl BoundedFrame for WorkerFrame {
	fn validate_raw(bytes: &[u8]) -> Result<(), omp_proto::bounds::FrameBoundsError> {
		omp_proto::bounds::validate_worker_frame(bytes)
	}
}

async fn read_async_frame<R, M>(
	reader: &mut R,
	limit: NonZeroUsize,
	scratch: &mut BytesMut,
) -> Result<Option<M>, WorkerError>
where
	R: AsyncRead + Unpin,
	M: BoundedFrame,
{
	let Some(length) = read_async_length(reader).await? else {
		return Ok(None);
	};
	check_length(length, limit)?;
	scratch.clear();
	scratch.resize(length, 0);
	reader.read_exact(scratch).await?;
	M::validate_raw(&scratch[..length])?;
	Ok(Some(M::decode(&scratch[..length])?))
}

async fn write_async_frame<W, M>(
	writer: &mut W,
	frame: &M,
	limit: NonZeroUsize,
	scratch: &mut BytesMut,
) -> Result<(), WorkerError>
where
	W: AsyncWrite + Unpin,
	M: Message,
{
	let length = frame.encoded_len();
	check_length(length, limit)?;
	scratch.clear();
	scratch.reserve(length + encoded_varint_len(length));
	frame.encode_length_delimited(&mut *scratch)?;
	writer.write_all(scratch).await?;
	writer.flush().await?;
	Ok(())
}

fn read_sync_frame<R, M>(
	reader: &mut R,
	limit: NonZeroUsize,
	scratch: &mut BytesMut,
) -> Result<Option<M>, WorkerError>
where
	R: Read,
	M: BoundedFrame,
{
	let Some(length) = read_sync_length(reader)? else {
		return Ok(None);
	};
	check_length(length, limit)?;
	scratch.clear();
	scratch.resize(length, 0);
	reader.read_exact(scratch)?;
	M::validate_raw(&scratch[..length])?;
	Ok(Some(M::decode(&scratch[..length])?))
}

fn write_sync_frame<W, M>(
	writer: &mut W,
	frame: &M,
	limit: NonZeroUsize,
	scratch: &mut BytesMut,
) -> Result<(), WorkerError>
where
	W: Write,
	M: Message,
{
	let length = frame.encoded_len();
	check_length(length, limit)?;
	scratch.clear();
	scratch.reserve(length + encoded_varint_len(length));
	frame.encode_length_delimited(&mut *scratch)?;
	writer.write_all(scratch)?;
	writer.flush()?;
	Ok(())
}

async fn read_async_length<R: AsyncRead + Unpin>(
	reader: &mut R,
) -> Result<Option<usize>, WorkerError> {
	let mut value = 0_u64;
	for shift in (0..70).step_by(7) {
		let mut byte = [0_u8; 1];
		match reader.read_exact(&mut byte).await {
			Ok(_) => {},
			Err(error) if error.kind() == io::ErrorKind::UnexpectedEof && shift == 0 => {
				return Ok(None);
			},
			Err(error) => return Err(error.into()),
		}
		let part = u64::from(byte[0] & 0x7f);
		if shift == 63 && part > 1 {
			return Err(WorkerError::InvalidLength);
		}
		value |= part << shift;
		if byte[0] & 0x80 == 0 {
			return usize::try_from(value)
				.map(Some)
				.map_err(|_| WorkerError::InvalidLength);
		}
	}
	Err(WorkerError::InvalidLength)
}

fn read_sync_length<R: Read>(reader: &mut R) -> Result<Option<usize>, WorkerError> {
	let mut value = 0_u64;
	for shift in (0..70).step_by(7) {
		let mut byte = [0_u8; 1];
		match reader.read_exact(&mut byte) {
			Ok(()) => {},
			Err(error) if error.kind() == io::ErrorKind::UnexpectedEof && shift == 0 => {
				return Ok(None);
			},
			Err(error) => return Err(error.into()),
		}
		let part = u64::from(byte[0] & 0x7f);
		if shift == 63 && part > 1 {
			return Err(WorkerError::InvalidLength);
		}
		value |= part << shift;
		if byte[0] & 0x80 == 0 {
			return usize::try_from(value)
				.map(Some)
				.map_err(|_| WorkerError::InvalidLength);
		}
	}
	Err(WorkerError::InvalidLength)
}

const fn check_length(length: usize, limit: NonZeroUsize) -> Result<(), WorkerError> {
	let limit = if limit.get() < omp_proto::bounds::FRAME_MAX_BYTES {
		limit.get()
	} else {
		omp_proto::bounds::FRAME_MAX_BYTES
	};
	if length > limit {
		Err(WorkerError::FrameTooLarge { actual: length, limit })
	} else {
		Ok(())
	}
}

const fn encoded_varint_len(mut value: usize) -> usize {
	let mut length = 1;
	while value >= 0x80 {
		value >>= 7;
		length += 1;
	}
	length
}
