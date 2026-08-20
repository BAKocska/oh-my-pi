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
		atomic::{AtomicBool, AtomicU64, Ordering},
	},
	time::{Duration, SystemTime},
};

use bytes::{Bytes, BytesMut};
use omp_core::{CowBytes, Duration as CoreDuration, DurationUnit, RestartReason, Str};
use omp_proto::{
	env::v1::{ArgText, ArgsCommitted, Interrupt},
	prost::Message,
	thread::v1::{Blob, Part, part},
	toolhost::v1::{
		ActivateExtension, ActivateReason as WireActivateReason, AdmitExtensions, AdmittedExtension,
		ArgIssue, ArgumentHostEnvelope, ArgumentWorkerEnvelope, CancelTool, ExtensionActivated,
		ExtensionDecl, FreezeDeclarations, HostFrame, InvokeTool, JournalHostEnvelope,
		LifecycleHostEnvelope, OutcomeKind, Ping, Pong, PrincipalRef, ProtocolError,
		ProtocolErrorCode, PullReply, PullRequest, QuotaDrop, QuotaStatus, RegisterTools,
		ResourceUpdate, RestartReason as WireRestartReason, ServiceDispatch as WireServiceDispatch,
		ServiceReply, ServiceResult, ToolAborted, ToolArgs, ToolComplete, ToolDecl, ToolUpdate,
		WorkerFrame, WorkerHello, argument_host_envelope, argument_worker_envelope, host_frame,
		lifecycle_host_envelope, lifecycle_worker_envelope, worker_frame,
	},
};
use omp_tools::read::resolver::SchemeSnapshot;
use parking_lot::Mutex;
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

use crate::{
	envd::policy::{AuthorityTable, Grants},
	exthost::{
		ActivationCause, ActivationEvent, ActivationTrigger, AvailabilityBatch, AvailabilitySink,
		ControlQuotaLedger, DeclarationSet, ExtensionManifest, GenerationFence, LifecycleHost,
		ServiceBroker, ServiceCallId, ServiceConnection, ServiceKey, ServiceRequestMeta,
		ServiceResponse, ToolDeclarationKey,
		control::{
			ExternalJournalRequest, JournalConnectionIdentity, JournalControl, JournalDispatch,
		},
	},
};
/// Child argv selector for the dedicated placed-Python worker runtime.
pub const WORKER_ARG: &str = "__omp-py-worker";

/// Python ABI revision required by this worker implementation.
pub const PYTHON_REV: &str = "3.14t";
/// Canonical import name for the opt-in built-in Python evaluation tool.
pub const PY_EVAL_MODULE: &str = "omp_py_eval";

/// Default upper bound for one encoded tool-host frame.
pub const DEFAULT_MAX_FRAME_BYTES: usize = omp_proto::bounds::FRAME_MAX_BYTES;

/// Stable identity of one extension host.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HostKey(Arc<HostKeyFields>);

#[derive(Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct HostKeyFields {
	/// Extension layer, such as project or user.
	layer:     Str,
	/// Trust or sandbox tier.
	tier:      Str,
	/// Stable extension identity.
	extension: Str,
}

impl std::fmt::Debug for HostKey {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter
			.debug_struct("HostKey")
			.field("layer", self.layer())
			.field("tier", self.tier())
			.field("extension", self.extension())
			.finish()
	}
}

const _: () =
	assert!(std::mem::size_of::<HostKey>() <= 16, "HostKey must remain a cheap identity handle");

impl HostKey {
	/// Builds a host identity.
	#[must_use]
	pub fn new(layer: impl Into<Str>, tier: impl Into<Str>, extension: impl Into<Str>) -> Self {
		Self(Arc::new(HostKeyFields {
			layer:     layer.into(),
			tier:      tier.into(),
			extension: extension.into(),
		}))
	}

	/// Returns the extension layer, such as project or user.
	#[must_use]
	pub fn layer(&self) -> &Str {
		&self.0.layer
	}

	/// Returns the trust or sandbox tier.
	#[must_use]
	pub fn tier(&self) -> &Str {
		&self.0.tier
	}

	/// Returns the stable extension identity.
	#[must_use]
	pub fn extension(&self) -> &Str {
		&self.0.extension
	}

	/// Returns the ordered identity fields used by scoped binding derivation.
	#[must_use]
	pub fn fields(&self) -> [&str; 3] {
		[self.layer().as_str(), self.tier().as_str(), self.extension().as_str()]
	}
}

/// Configuration of one active extension.
#[derive(Clone, Debug)]
pub struct ExtHostSpec {
	/// Stable extension identity.
	pub key:         HostKey,
	/// Authoritative deployment manifest; never inferred from child frames.
	pub manifest:    ExtensionManifest,
	/// Explicit opt-in fate-sharing pool. Absence isolates this extension.
	pub pool:        Option<Str>,
	/// Manifest-derived DATA capabilities for this extension.
	pub data_grants: Grants,
	/// Optional site-packages directory passed through as `OMP_PY_SITE`.
	pub python_site: Option<PathBuf>,
	/// Scoped DATA socket passed only to this extension host.
	pub data_socket: Option<PathBuf>,
}

impl ExtHostSpec {
	/// Builds an isolated extension configuration from an authenticated
	/// manifest.
	#[must_use]
	pub fn new(key: HostKey, manifest: ExtensionManifest) -> Self {
		Self {
			key,
			manifest,
			pool: None,
			data_grants: Grants::default(),
			python_site: None,
			data_socket: None,
		}
	}
}
/// One journal backend request emitted by an authenticated extension host.
///
/// The receiver must send exactly one fused reply sequence. Every sequence is
/// written to the requesting host in order on its existing CONTROL stream.
pub struct ExternalJournalCall {
	/// Core-stamped request with no worker-supplied principal fields.
	pub request:  ExternalJournalRequest,
	/// Authenticated principal, provenance, and generation fences for backend
	/// authority.
	pub identity: JournalConnectionIdentity,
	/// Ordered response stream; dropping the last sender fuses the host stream.
	pub reply:    flume::Sender<Result<JournalHostEnvelope, Str>>,
}

/// Agent-Journal and storage-backend handles installed into extension hosts.
#[derive(Clone)]
pub struct JournalRuntime {
	/// Serialized Agent Journal mailbox sender.
	pub agent:    omp_agent::control::ControlSender,
	/// Environment composition endpoint for session indexes, state, usage, and
	/// artifacts.
	pub external: flume::Sender<ExternalJournalCall>,
}
struct ServiceRouter {
	broker: Mutex<ServiceBroker>,
	routes: Mutex<BTreeMap<HostKey, ProviderRoute>>,
}

#[derive(Clone)]
struct ProviderRoute {
	process_id: ProcessKey,
	commands:   flume::Sender<SupervisorCommand>,
	generation: Arc<AtomicU64>,
}

/// Configuration for all active Python extension hosts.
#[derive(Clone)]
pub struct ExtHostConfig {
	/// Executable to re-enter. Defaults to the current executable.
	pub executable:         PathBuf,
	/// Authenticated daemon principal stamped core-side.
	pub principal:          omp_core::Principal,
	/// Stable active session identity.
	pub session_id:         Str,
	/// Active session generation fence.
	pub session_generation: u64,
	/// Session start timestamp used by activation events.
	pub session_started_at: SystemTime,
	/// Active extensions. An empty set starts no Python process.
	pub extensions:         Vec<ExtHostSpec>,
	/// Expected workspace protobuf schema revision.
	pub schema_rev:         u32,
	/// Expected embedded Python ABI revision.
	pub python_rev:         Str,
	/// Maximum accepted encoded frame size.
	pub max_frame_bytes:    NonZeroUsize,
	/// Time allowed for hello, registration, ping, and individual frame reads.
	pub health_timeout:     Duration,
	/// Idle interval between worker health probes.
	pub ping_interval:      Duration,
	/// Courtesy-interrupt grace period before the process group is killed.
	pub interrupt_grace:    CoreDuration,
	/// Initial delay after an unhealthy host.
	pub initial_backoff:    Duration,
	/// Maximum delay between respawn attempts.
	pub max_backoff:        Duration,
	/// Healthy duration after which the per-host backoff resets.
	pub healthy_reset:      Duration,
	/// Device-hash-keyed URL scheme metadata installed before activation.
	pub scheme_snapshot:    Option<SchemeSnapshot>,
	/// Shared DATA authorization table owned by the Environment.
	pub data_authority:     Option<Arc<AuthorityTable>>,
	/// CONTROL routing to the serialized Agent Journal and external storage
	/// backends.
	pub journal:            Option<JournalRuntime>,
	/// Late-bound, generation-fenced device availability destination.
	availability_sink:      Arc<Mutex<Option<Arc<dyn AvailabilitySink>>>>,
}
impl ExtHostConfig {
	/// Builds the production configuration from authenticated session context.
	#[must_use]
	pub fn new(
		executable: PathBuf,
		principal: omp_core::Principal,
		session_id: Str,
		session_generation: u64,
	) -> Self {
		Self {
			executable,
			principal,
			session_id,
			session_generation,
			session_started_at: SystemTime::now(),
			extensions: Vec::new(),
			schema_rev: omp_proto::SCHEMA_REV,
			python_rev: Str::new_static(PYTHON_REV),
			max_frame_bytes: NonZeroUsize::new(DEFAULT_MAX_FRAME_BYTES)
				.expect("the default worker frame limit is nonzero"),
			health_timeout: Duration::from_secs(5),
			ping_interval: Duration::from_secs(15),
			interrupt_grace: omp_tool::DEFAULT_INTERRUPT_GRACE,
			data_authority: None,
			journal: None,
			initial_backoff: Duration::from_secs(1),
			scheme_snapshot: None,
			availability_sink: Arc::new(Mutex::new(None)),
			max_backoff: Duration::from_secs(30),
			healthy_reset: Duration::from_secs(30),
		}
	}

	/// Binds this supervisor configuration to the Environment's sole DATA
	/// authorization table.
	pub fn bind_data_authority(&mut self, authority: Arc<AuthorityTable>) {
		self.data_authority = Some(authority);
	}

	/// Installs authenticated journal and scoped-state CONTROL routing.
	pub fn bind_journal(&mut self, runtime: JournalRuntime) {
		self.journal = Some(runtime);
	}

	/// Installs the registry-derived URL scheme snapshot for child activation.
	pub fn set_scheme_snapshot(&mut self, snapshot: SchemeSnapshot) {
		self.scheme_snapshot = Some(snapshot);
	}

	/// Builds a configuration that re-enters the current executable.
	///
	/// # Errors
	/// Returns the operating-system error if the current executable cannot be
	/// resolved.
	pub fn current(
		principal: omp_core::Principal,
		session_id: Str,
		session_generation: u64,
	) -> io::Result<Self> {
		std::env::current_exe()
			.map(|executable| Self::new(executable, principal, session_id, session_generation))
	}
}

/// An environment invocation opened against a registered Python tool.
///
/// The host chooses streaming from the registered declaration. Ordinary v1
/// tools are held until [`WorkerInvocation::args_committed`] supplies the one
/// final effective document; streaming tools receive forwarded fragments.
#[derive(Clone, Debug)]
pub struct OpenToolCall {
	/// Environment-plane invocation identity.
	pub invocation_id: Str,
	/// Registered tool name.
	pub name:          Str,
	/// Registered tool revision.
	pub rev:           Str,
	/// Maximum execution duration after the worker receives the call.
	pub deadline:      Duration,
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
	/// One bounded cursor pull awaiting a host reply.
	Pull(PullRequest),
	/// A typed protocol error returned by the extension host.
	ProtocolError(ProtocolError),
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
	id:                 u64,
	invocation_id:      Str,
	streams_args:       bool,
	host_generation:    u64,
	session_generation: u64,
	owner:              HostKey,
	maximum_effects:    omp_tool::Effects,
	data_authority:     Option<Arc<AuthorityTable>>,
	events:             flume::Receiver<WorkerEvent>,
	commands:           flume::Sender<SupervisorCommand>,
	committed:          bool,
	terminal:           bool,
	cancel_requested:   bool,
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
			if let Some(authority) = &self.data_authority {
				authority.settle(&self.owner, self.invocation_id.as_str());
			}
		}
		Ok(event)
	}

	/// Returns the host generation that must fence this invocation's DATA
	/// requests.
	#[must_use]
	pub const fn host_generation(&self) -> u64 {
		self.host_generation
	}

	/// Returns the session generation that must fence this invocation's DATA
	/// requests.
	#[must_use]
	pub const fn session_generation(&self) -> u64 {
		self.session_generation
	}

	/// Returns whether the registered declaration selected streamed arguments.
	#[must_use]
	pub const fn streams_args(&self) -> bool {
		self.streams_args
	}

	/// Forwards one speculative argument fragment verbatim.
	///
	/// # Errors
	/// Returns a typed protocol error for a stale invocation id, a declaration
	/// that did not opt into streaming, a committed invocation, or a stopped
	/// actor.
	pub fn arg_text(&self, frame: ArgText) -> Result<(), WorkerError> {
		self.validate_environment_id(frame.invocation_id.as_str())?;
		if !self.streams_args {
			return Err(WorkerError::Protocol(Str::new_static(
				"tool declaration did not enable streams_args",
			)));
		}
		if self.committed {
			return Err(WorkerError::Protocol(Str::new_static("ArgText arrived after ArgsCommitted")));
		}
		self
			.commands
			.send(SupervisorCommand::ArgText { id: self.id, frame })
			.map_err(|_| WorkerError::Unavailable)
	}

	/// Forwards the assistant-item/effect-authorization receipt verbatim.
	///
	/// The effect token and authorization timestamp remain in this exact frame;
	/// no lifecycle side channel is synthesized.
	///
	/// # Errors
	/// Returns a typed protocol error for a stale invocation id, a duplicate
	/// commit, or a stopped actor.
	pub fn args_committed(&mut self, frame: ArgsCommitted) -> Result<(), WorkerError> {
		self.validate_environment_id(frame.invocation_id.as_str())?;
		if self.committed {
			return Err(WorkerError::Protocol(Str::new_static("ArgsCommitted was already forwarded")));
		}
		let narrowed = frame
			.effects
			.as_ref()
			.map(omp_tool::Effects::try_from)
			.transpose()
			.map_err(|_| WorkerError::Protocol(Str::new_static("ArgsCommitted effects are invalid")))?
			.unwrap_or_default();
		if !narrowed.is_subset_of(&self.maximum_effects) {
			return Err(WorkerError::Protocol(Str::new_static(
				"ArgsCommitted effects exceed the registered tool maximum",
			)));
		}
		if let Some(authority) = &self.data_authority {
			authority
				.authorize(
					&self.owner,
					self.invocation_id.as_str(),
					frame.effect_token.clone(),
					frame
						.effects
						.as_ref()
						.map_or_else(Grants::default, Grants::from_effect_envelope),
					frame.authorized_at_ms,
					self.host_generation,
					self.session_generation,
				)
				.map_err(|error| WorkerError::Protocol(Str::from(error.to_string())))?;
		}
		self
			.commands
			.send(SupervisorCommand::ArgsCommitted { id: self.id, frame })
			.map_err(|_| WorkerError::Unavailable)?;
		self.committed = true;
		Ok(())
	}

	/// Sends a survivable, classed interrupt verbatim.
	///
	/// # Errors
	/// Returns a typed protocol error for a stale invocation id or stopped
	/// actor.
	pub fn interrupt(&self, frame: Interrupt) -> Result<(), WorkerError> {
		self.validate_environment_id(frame.invocation_id.as_str())?;
		if self.terminal || self.cancel_requested {
			return Err(WorkerError::Protocol(Str::new_static("invocation is already terminal")));
		}
		self
			.commands
			.send(SupervisorCommand::Interrupt { id: self.id, frame })
			.map_err(|_| WorkerError::Unavailable)
	}

	/// Replies to the invocation's sole outstanding pull.
	///
	/// # Errors
	/// Returns a typed protocol error for a stale call id or stopped actor.
	pub fn reply_pull(&self, reply: PullReply) -> Result<(), WorkerError> {
		if reply.call_id != self.invocation_id.as_str() {
			return Err(WorkerError::Protocol(Str::new_static(
				"PullReply call id does not match invocation",
			)));
		}
		self
			.commands
			.send(SupervisorCommand::PullReply { id: self.id, reply })
			.map_err(|_| WorkerError::Unavailable)
	}

	fn validate_environment_id(&self, invocation_id: &str) -> Result<(), WorkerError> {
		if invocation_id == self.invocation_id.as_str() {
			Ok(())
		} else {
			Err(WorkerError::Protocol(Str::new_static(
				"stale invocation id does not match worker handle",
			)))
		}
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
}

impl Drop for WorkerInvocation {
	fn drop(&mut self) {
		if !self.terminal && !self.cancel_requested {
			let _ = self.commands.send(SupervisorCommand::Cancel {
				id:     self.id,
				reason: Str::new_static("invocation guard dropped"),
			});
		}
		if let Some(authority) = &self.data_authority {
			authority.settle(&self.owner, self.invocation_id.as_str());
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
	routes:               BTreeMap<(Str, Str), HostRoute>,
	registrations:        Arc<[OwnedToolDecl]>,
	next_invocation:      AtomicU64,
	actors:               Vec<HostActor>,
	data_authority:       Option<Arc<AuthorityTable>>,
	journal_runtime:      Arc<Mutex<Option<JournalRuntime>>>,
	availability_pending: Arc<Mutex<VecDeque<AvailabilityBatch>>>,
	availability_sink:    Arc<Mutex<Option<Arc<dyn AvailabilitySink>>>>,
	children_active:      AtomicBool,
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
		let mut service_broker = ServiceBroker::new(config.session_generation);
		for extension in &config.extensions {
			service_broker
				.publish_manifest(extension.key.clone(), extension.manifest.services.clone())
				.map_err(|error| WorkerError::Protocol(Str::from(error.to_string())))?;
		}
		let service_router = Arc::new(ServiceRouter {
			broker: Mutex::new(service_broker),
			routes: Mutex::new(BTreeMap::new()),
		});
		let mut groups = BTreeMap::<ProcessKey, Vec<ExtHostSpec>>::new();
		let mut identities = HashSet::with_capacity(config.extensions.len());
		let data_authority = config.data_authority.clone();
		let resources = Arc::new(Mutex::new(ControlQuotaLedger::new()));
		let availability_sink = Arc::clone(&config.availability_sink);
		let availability_pending = Arc::new(Mutex::new(VecDeque::new()));
		let journal_runtime = Arc::new(Mutex::new(config.journal.clone()));
		let children_active = AtomicBool::new(false);
		for extension in config.extensions.iter().cloned() {
			validate_extension_spec(&extension)?;
			if !identities.insert(extension.key.clone()) {
				return Err(WorkerError::Protocol(Str::new_static(
					"extension host identity is configured more than once",
				)));
			}
			if let Some(authority) = &config.data_authority {
				authority.register_host(extension.key.clone(), extension.data_grants.clone());
			}
			resources
				.lock()
				.register_limits(
					extension.key.clone(),
					extension.manifest.resource_limits.iter().cloned(),
				)
				.map_err(|error| WorkerError::Protocol(Str::from(error.to_string())))?;
			groups
				.entry(ProcessKey::from_spec(&extension))
				.or_default()
				.push(extension);
		}

		let mut prepared = Vec::with_capacity(groups.len());
		for (key, extensions) in groups {
			let process_config = ProcessConfig::new(
				&config,
				key,
				extensions,
				Arc::clone(&resources),
				Arc::clone(&availability_pending),
				Arc::clone(&journal_runtime),
			)?;
			match WorkerProcess::spawn(&process_config, 1, ActivationCause::FirstReach).await {
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
				let maximum_effects = if let Ok(effects) = declaration
					.effects
					.as_ref()
					.map(omp_tool::Effects::try_from)
					.transpose()
				{
					effects.unwrap_or_default()
				} else {
					registration_error = Some(WorkerError::Protocol(Str::new_static(
						"registered tool effects are invalid",
					)));
					break 'registration;
				};
				let route = (Str::from(definition.name.as_str()), Str::from(declaration.rev.as_str()));
				if routes
					.insert(
						route,
						(
							process_config.process_id.clone(),
							owner.clone(),
							declaration.streams_args,
							maximum_effects,
						),
					)
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
			for (prepared_config, mut process) in prepared {
				process.terminate(prepared_config.interrupt_grace).await;
			}
			return Err(error);
		}

		let mut senders = BTreeMap::new();
		let mut actors = Vec::with_capacity(prepared.len());
		for (process_config, process) in prepared {
			let process_id = process_config.process_id.clone();
			let session_generation = process_config.session_generation;
			let host_generation = Arc::new(AtomicU64::new(1));
			let expected_registrations: Arc<[ToolDecl]> = process.registrations.clone().into();
			let (commands, mailbox) = flume::unbounded();
			for (owner, manifest) in &process_config.manifests {
				service_router
					.broker
					.lock()
					.activate_provider(owner, 1, manifest.services.provides().cloned())
					.map_err(|error| WorkerError::Protocol(Str::from(error.to_string())))?;
				service_router
					.routes
					.lock()
					.insert(owner.clone(), ProviderRoute {
						process_id: process_id.clone(),
						commands:   commands.clone(),
						generation: host_generation.clone(),
					});
			}
			let actor = tokio::spawn(run_supervisor(
				process_config,
				process,
				expected_registrations,
				mailbox,
				host_generation.clone(),
				1,
				Arc::clone(&service_router),
			));
			senders.insert(process_id, (commands.clone(), host_generation, session_generation));
			actors.push(HostActor { commands, actor });
		}
		let routes = routes
			.into_iter()
			.map(|(route, (process_id, owner, streams_args, maximum_effects))| {
				let (commands, host_generation, session_generation) = senders
					.get(&process_id)
					.expect("every verified process has a command channel");
				(route, HostRoute {
					commands: commands.clone(),
					owner,
					streams_args,
					maximum_effects,
					host_generation: host_generation.clone(),
					session_generation: *session_generation,
				})
			})
			.collect();
		Ok(Self {
			routes,
			registrations: registrations.into(),
			next_invocation: AtomicU64::new(1),
			actors,
			data_authority,
			journal_runtime,
			availability_sink,
			availability_pending,
			children_active,
		})
	}

	/// Returns declarations paired with their owning host identity.
	#[must_use]
	pub fn registrations(&self) -> &[OwnedToolDecl] {
		&self.registrations
	}

	/// Installs Agent Journal CONTROL routing before any extension child reaches
	/// activation.
	///
	/// # Errors
	/// Fails closed once a child is active so authenticated mailbox ownership
	/// cannot change beneath callbacks already admitted by that generation.
	pub fn bind_journal_runtime(&self, runtime: JournalRuntime) -> Result<(), WorkerError> {
		if self.children_active.load(Ordering::Acquire) {
			return Err(WorkerError::Protocol(Str::new_static(
				"journal runtime must be bound before the first extension child is active",
			)));
		}
		if self.journal_runtime.lock().is_some() {
			return Err(WorkerError::Protocol(Str::new_static("journal runtime is already bound")));
		}
		*self.journal_runtime.lock() = Some(runtime);
		Ok(())
	}

	/// Binds the active Agent mailbox's device availability destination.
	pub fn bind_availability_sink(&self, sink: Arc<dyn AvailabilitySink>) {
		let pending = {
			let mut availability_sink = self.availability_sink.lock();
			*availability_sink = Some(Arc::clone(&sink));
			std::mem::take(&mut *self.availability_pending.lock())
		};
		for batch in pending {
			sink.set_availability(batch);
		}
	}

	/// Opens one invocation and establishes its host-owned request mapping.
	///
	/// The declaration's `streams_args` bit selects the protocol. Non-streaming
	/// tools are not dispatched until the final [`ArgsCommitted`] frame arrives.
	///
	/// # Errors
	/// Returns [`WorkerError::NotRegistered`] when no active extension owns the
	/// exact name/revision, or [`WorkerError::Unavailable`] when its host actor
	/// has stopped.
	pub fn open(&self, call: OpenToolCall) -> Result<WorkerInvocation, WorkerError> {
		let route = self
			.routes
			.get(&(call.name.clone(), call.rev.clone()))
			.ok_or_else(|| WorkerError::NotRegistered {
				name: call.name.clone(),
				rev:  call.rev.clone(),
			})?;
		self.children_active.store(true, Ordering::Release);
		let commands = route.commands.clone();
		let id = self.next_invocation.fetch_add(1, Ordering::Relaxed).max(1);
		let invocation_id = call.invocation_id.clone();
		if let Some(authority) = &self.data_authority {
			authority.open(route.owner.clone(), invocation_id.clone());
		}
		let (events_tx, events) = flume::unbounded();
		if commands
			.send(SupervisorCommand::Open {
				id,
				owner: route.owner.clone(),
				call,
				streams_args: route.streams_args,
				events: events_tx,
			})
			.is_err()
		{
			if let Some(authority) = &self.data_authority {
				authority.settle(&route.owner, invocation_id.as_str());
			}
			return Err(WorkerError::Unavailable);
		}
		Ok(WorkerInvocation {
			id,
			invocation_id,
			owner: route.owner.clone(),
			data_authority: self.data_authority.clone(),
			streams_args: route.streams_args,
			maximum_effects: route.maximum_effects.clone(),
			host_generation: route.host_generation.load(Ordering::Acquire),
			session_generation: route.session_generation,
			events,
			commands,
			committed: false,
			terminal: false,
			cancel_requested: false,
		})
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
	commands:           flume::Sender<SupervisorCommand>,
	owner:              HostKey,
	streams_args:       bool,
	maximum_effects:    omp_tool::Effects,
	host_generation:    Arc<AtomicU64>,
	session_generation: u64,
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
	/// Named-worker routing refused immediate placement.
	#[error(transparent)]
	WorkerUnavailable(#[from] crate::envd::worker_pool::WorkerUnavailable),
}

impl From<PyErr> for WorkerError {
	fn from(error: PyErr) -> Self {
		Self::Python(Str::from(error.to_string()))
	}
}

enum SupervisorCommand {
	Open {
		id:           u64,
		owner:        HostKey,
		call:         OpenToolCall,
		streams_args: bool,
		events:       flume::Sender<WorkerEvent>,
	},
	ArgText {
		id:    u64,
		frame: ArgText,
	},
	ArgsCommitted {
		id:    u64,
		frame: ArgsCommitted,
	},
	PullReply {
		id:    u64,
		reply: PullReply,
	},
	ServiceDispatch {
		request_id: u64,
		frame:      WireServiceDispatch,
		reply:      flume::Sender<Result<ServiceResult, WorkerError>>,
	},
	Cancel {
		id:     u64,
		reason: Str,
	},
	Interrupt {
		id:    u64,
		frame: Interrupt,
	},
	Shutdown,
}

struct PendingInvocation {
	id:           u64,
	owner:        HostKey,
	call:         OpenToolCall,
	streams_args: bool,
	arguments:    VecDeque<ArgText>,
	committed:    Option<ArgsCommitted>,
	interrupt:    Option<Interrupt>,
	events:       flume::Sender<WorkerEvent>,
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
			.map_or_else(|| FateUnit::Extension(spec.key.extension().clone()), FateUnit::Pool);
		Self { layer: spec.key.layer().clone(), tier: spec.key.tier().clone(), unit }
	}

	const fn pool(&self) -> Option<&Str> {
		match &self.unit {
			FateUnit::Extension(_) => None,
			FateUnit::Pool(pool) => Some(pool),
		}
	}
}

#[derive(Clone)]
struct ProcessConfig {
	process_id:           ProcessKey,
	executable:           PathBuf,
	python_site:          Option<PathBuf>,
	modules:              Vec<Str>,
	manifests:            BTreeMap<HostKey, ExtensionManifest>,
	data_socket:          Option<PathBuf>,
	schema_rev:           u32,
	python_rev:           Str,
	principal:            omp_core::Principal,
	session_started_at:   SystemTime,
	session_id:           Str,
	max_frame_bytes:      NonZeroUsize,
	health_timeout:       Duration,
	ping_interval:        Duration,
	interrupt_grace:      Duration,
	initial_backoff:      Duration,
	max_backoff:          Duration,
	healthy_reset:        Duration,
	session_generation:   u64,
	scheme_snapshot:      Option<SchemeSnapshot>,
	journal:              Arc<Mutex<Option<JournalRuntime>>>,
	resources:            Arc<Mutex<ControlQuotaLedger>>,
	availability_sink:    Arc<Mutex<Option<Arc<dyn AvailabilitySink>>>>,
	availability_pending: Arc<Mutex<VecDeque<AvailabilityBatch>>>,
}

impl ProcessConfig {
	fn new(
		root: &ExtHostConfig,
		process_id: ProcessKey,
		extensions: Vec<ExtHostSpec>,
		resources: Arc<Mutex<ControlQuotaLedger>>,
		availability_pending: Arc<Mutex<VecDeque<AvailabilityBatch>>>,
		journal: Arc<Mutex<Option<JournalRuntime>>>,
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
		let mut modules_seen = HashSet::new();
		let mut manifests = BTreeMap::new();
		let mut modules = Vec::new();
		for extension in extensions {
			let key = extension.key;
			for module in std::iter::once(extension.manifest.entry.clone())
				.chain(extension.manifest.declaration_modules.iter().cloned())
			{
				if !modules_seen.insert(module.clone()) {
					return Err(WorkerError::Protocol(Str::new_static(
						"an extension declaration module is configured more than once in one host",
					)));
				}
				modules.push(module);
			}
			manifests.insert(key, extension.manifest);
		}
		Ok(Self {
			process_id,
			executable: root.executable.clone(),
			python_site,
			modules,
			manifests,
			data_socket,
			schema_rev: root.schema_rev,
			python_rev: root.python_rev.clone(),
			principal: root.principal.clone(),
			session_id: root.session_id.clone(),
			session_started_at: root.session_started_at,
			max_frame_bytes: root.max_frame_bytes,
			health_timeout: root.health_timeout,
			ping_interval: root.ping_interval,
			interrupt_grace: root
				.interrupt_grace
				.to_std()
				.map_err(|_| WorkerError::Protocol(Str::new_static("interrupt grace is too large")))?,
			initial_backoff: root.initial_backoff,
			max_backoff: root.max_backoff,
			healthy_reset: root.healthy_reset,
			session_generation: root.session_generation,
			scheme_snapshot: root.scheme_snapshot.clone(),
			journal,
			resources,
			availability_sink: Arc::clone(&root.availability_sink),
			availability_pending,
		})
	}

	fn owner_for(&self, declaration: &ToolDecl) -> Result<HostKey, WorkerError> {
		self
			.manifests
			.keys()
			.find(|owner| owner.extension().as_str() == declaration.extension_id)
			.cloned()
			.ok_or_else(|| {
				WorkerError::Protocol(Str::new_static(
					"worker registered a declaration for an unconfigured extension",
				))
			})
	}
}

fn validate_extension_spec(spec: &ExtHostSpec) -> Result<(), WorkerError> {
	if spec.key.layer().is_empty()
		|| spec.key.tier().is_empty()
		|| spec.key.extension().is_empty()
		|| spec.manifest.entry.is_empty()
		|| spec.pool.as_ref().is_some_and(Str::is_empty)
	{
		return Err(WorkerError::Protocol(Str::new_static(
			"extension host identity, manifest entry, and explicit pool names must be nonempty",
		)));
	}
	if spec.manifest.provenance.extension_id() != spec.key.extension().as_str()
		|| spec.manifest.provenance.layer() != spec.key.layer().as_str()
		|| spec.manifest.provenance.tier() != spec.key.tier().as_str()
	{
		return Err(WorkerError::Protocol(Str::new_static(
			"extension manifest provenance does not match its authenticated host key",
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
	async fn spawn(
		config: &ProcessConfig,
		generation: u64,
		cause: ActivationCause,
	) -> Result<Self, WorkerError> {
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
		if let Some(snapshot) = &config.scheme_snapshot {
			let entries = snapshot
				.entries
				.iter()
				.map(|entry| {
					serde_json::json!([
						entry.member.as_str(),
						entry.readable,
						entry.mintable,
						entry.selectors,
						entry.description.as_str()
					])
				})
				.collect::<Vec<_>>();
			let encoded = serde_json::to_string(&serde_json::json!({
				"device_hash": snapshot.device_hash,
				"entries": entries,
			}))
			.map_err(|error| WorkerError::Protocol(Str::from(error.to_string())))?;
			command.env("OMP_EXT_SCHEME_SNAPSHOT", encoded);
		} else {
			command.env_remove("OMP_EXT_SCHEME_SNAPSHOT");
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
		if let Err(error) = process.handshake(config, generation, cause).await {
			process.terminate(config.interrupt_grace).await;
			return Err(error);
		}
		Ok(process)
	}

	async fn handshake(
		&mut self,
		config: &ProcessConfig,
		generation: u64,
		cause: ActivationCause,
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
		let admitted = config
			.manifests
			.iter()
			.flat_map(|(key, manifest)| {
				std::iter::once(&manifest.entry)
					.chain(manifest.declaration_modules.iter())
					.map(|module| AdmittedExtension {
						extension_id: key.extension().to_string(),
						module:       module.to_string(),
						rev:          manifest.provenance.version().to_owned(),
					})
			})
			.collect();
		self
			.write(
				&HostFrame {
					request_id: 0,
					body:       Some(host_frame::Body::Lifecycle(LifecycleHostEnvelope {
						body:  Some(lifecycle_host_envelope::Body::AdmitExtensions(AdmitExtensions {
							extensions: admitted,
							generation,
							props: None,
						})),
						props: None,
					})),
					props:      None,
				},
				config,
			)
			.await?;
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
		if registered_extensions.len() != config.manifests.len()
			|| config
				.manifests
				.keys()
				.any(|owner| !registered_extensions.contains(owner.extension().as_str()))
		{
			return Err(WorkerError::Protocol(Str::new_static(
				"RegisterTools extension set did not match the spawned host",
			)));
		}
		validate_registrations(&tools)?;
		validate_manifest_registrations(config, &tools)?;
		self.registrations = tools;
		self.activate_manifests(config, generation, cause).await?;
		Ok(())
	}

	async fn activate_manifests(
		&mut self,
		config: &ProcessConfig,
		generation: u64,
		cause: ActivationCause,
	) -> Result<(), WorkerError> {
		let mut request_id = 1_u64;
		for (owner, manifest) in &config.manifests {
			let declared = actual_declarations(config, &self.registrations, owner)?;
			let mut machine = manifest.lifecycle(config.session_started_at, config.session_generation);
			let mut host = WorkerLifecycleAdapter {
				process: self,
				config,
				extension_id: owner.extension().clone(),
				generation,
				request_id: &mut request_id,
			};
			machine
				.activate_declared(
					&mut host,
					&declared,
					GenerationFence { host: generation, session: config.session_generation },
					ActivationTrigger::FirstReach,
					cause,
					&config.principal,
				)
				.await
				.map_err(|error| WorkerError::Protocol(Str::from(error.to_string())))?;
		}
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

struct WorkerLifecycleAdapter<'a> {
	process:      &'a mut WorkerProcess,
	config:       &'a ProcessConfig,
	extension_id: Str,
	generation:   u64,
	request_id:   &'a mut u64,
}

impl LifecycleHost for WorkerLifecycleAdapter<'_> {
	async fn freeze(&mut self) -> Result<(), Str> {
		let request_id = take_request_id(self.request_id);
		self
			.process
			.write(
				&HostFrame {
					request_id,
					body: Some(host_frame::Body::Lifecycle(LifecycleHostEnvelope {
						body:  Some(lifecycle_host_envelope::Body::FreezeDeclarations(
							FreezeDeclarations {
								extension_id: self.extension_id.to_string(),
								generation:   self.generation,
								props:        None,
							},
						)),
						props: None,
					})),
					props: None,
				},
				self.config,
			)
			.await
			.map_err(|error| Str::from(error.to_string()))
	}

	fn activate(
		&mut self,
		event: &ActivationEvent,
		principal: &omp_core::Principal,
	) -> impl Future<Output = Result<(), Str>> + Send {
		let event = event.clone();
		let principal = principal.clone();
		async move {
			let request_id = take_request_id(self.request_id);
			let session_started_at_ms = event
				.session_started_at
				.duration_since(SystemTime::UNIX_EPOCH)
				.map_err(|_| Str::new_static("session start precedes the Unix epoch"))?
				.as_millis()
				.try_into()
				.map_err(|_| Str::new_static("session start does not fit the lifecycle wire"))?;
			self
				.process
				.write(
					&HostFrame {
						request_id,
						body: Some(host_frame::Body::Lifecycle(LifecycleHostEnvelope {
							body:  Some(lifecycle_host_envelope::Body::ActivateExtension(
								ActivateExtension {
									extension_id: self.extension_id.to_string(),
									reason: wire_activate_reason(event.reason).into(),
									session_started_at_ms,
									generation: event.generation,
									principal: Some(PrincipalRef {
										id:      principal.id().to_owned(),
										display: principal.display().to_owned(),
										props:   None,
									}),
									restart_reason: event
										.restart_reason
										.map(wire_restart_reason)
										.map(Into::into),
									props: None,
								},
							)),
							props: None,
						})),
						props: None,
					},
					self.config,
				)
				.await
				.map_err(|error| Str::from(error.to_string()))?;
			loop {
				let reply = self
					.process
					.read_timeout(self.config)
					.await
					.map_err(|error| Str::from(error.to_string()))?;
				let Some(worker_frame::Body::Lifecycle(envelope)) = reply.body else {
					return Err(Str::new_static("activation did not return a lifecycle envelope"));
				};
				match envelope.body {
					Some(lifecycle_worker_envelope::Body::ResourceQuery(query))
						if query.extension_id == self.extension_id.as_str() =>
					{
						send_resource_update(
							self.process,
							self.config,
							reply.request_id,
							&self.extension_id,
						)
						.await
						.map_err(|error| Str::from(error.to_string()))?;
					},
					Some(lifecycle_worker_envelope::Body::ExtensionActivated(activated)) => {
						if reply.request_id != request_id
							|| activated.extension_id != self.extension_id.as_str()
							|| activated.generation != self.generation
						{
							return Err(Str::new_static(
								"activation reply correlation or generation is stale",
							));
						}
						if activated.degraded {
							return Err(Str::from(
								activated
									.error
									.unwrap_or_else(|| "extension activation degraded".into()),
							));
						}
						return Ok(());
					},
					_ => {
						return Err(Str::new_static(
							"activation returned an unsupported lifecycle frame",
						));
					},
				}
			}
		}
	}
}

async fn send_resource_update(
	process: &mut WorkerProcess,
	config: &ProcessConfig,
	request_id: u64,
	extension_id: &str,
) -> Result<(), WorkerError> {
	let owner = config
		.manifests
		.keys()
		.find(|owner| owner.extension().as_str() == extension_id)
		.ok_or_else(|| WorkerError::Protocol(Str::new_static("resource query is not admitted")))?;
	let receipt = config
		.resources
		.lock()
		.resources(config.session_id.as_str(), owner, std::time::Instant::now())
		.map_err(|error| WorkerError::Protocol(Str::from(error.to_string())))?;
	let quotas = receipt
		.quotas
		.into_iter()
		.map(|(name, status)| {
			let window_ms = status
				.window
				.map(CoreDuration::to_std)
				.transpose()
				.map_err(|_| WorkerError::Protocol(Str::new_static("quota window is too large")))?
				.map(|window| window.as_millis().try_into())
				.transpose()
				.map_err(|_| WorkerError::Protocol(Str::new_static("quota window is too large")))?;
			Ok(QuotaStatus {
				name: name.to_string(),
				limit: status.limit,
				used: status.used,
				window_ms,
				props: None,
			})
		})
		.collect::<Result<Vec<_>, WorkerError>>()?;
	let dropped = receipt
		.dropped
		.into_iter()
		.map(|(name, count)| QuotaDrop { name: name.to_string(), count, props: None })
		.collect();
	process
		.write(
			&HostFrame {
				request_id,
				body: Some(host_frame::Body::Lifecycle(LifecycleHostEnvelope {
					body:  Some(lifecycle_host_envelope::Body::ResourceUpdate(ResourceUpdate {
						extension_id: extension_id.to_owned(),
						quotas,
						dropped,
						props: None,
					})),
					props: None,
				})),
				props: None,
			},
			config,
		)
		.await
}

fn take_request_id(next: &mut u64) -> u64 {
	let request_id = *next;
	*next = next.wrapping_add(1).max(1);
	request_id
}

const fn wire_activate_reason(reason: omp_core::ActivateReason) -> WireActivateReason {
	match reason {
		omp_core::ActivateReason::FirstReach => WireActivateReason::FirstReach,
		omp_core::ActivateReason::Restart => WireActivateReason::Restart,
		omp_core::ActivateReason::HotReload => WireActivateReason::HotReload,
	}
}

const fn wire_restart_reason(reason: RestartReason) -> WireRestartReason {
	match reason {
		RestartReason::Crash => WireRestartReason::Crash,
		RestartReason::HotReload => WireRestartReason::HotReload,
		RestartReason::CancelEscalation => WireRestartReason::CancelEscalation,
		RestartReason::ProtocolError => WireRestartReason::ProtocolError,
		RestartReason::Oom => WireRestartReason::Oom,
		RestartReason::HealthTimeout => WireRestartReason::HealthTimeout,
	}
}

async fn run_supervisor(
	config: ProcessConfig,
	mut process: WorkerProcess,
	expected_registrations: Arc<[ToolDecl]>,
	mailbox: flume::Receiver<SupervisorCommand>,
	host_generation: Arc<AtomicU64>,
	mut generation: u64,
	service_router: Arc<ServiceRouter>,
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
			match run_invocation(
				&config,
				&mut process,
				invocation,
				&mailbox,
				&mut pending,
				&service_router,
				generation,
			)
			.await
			{
				InvocationAction::KeepWorker => {},
				InvocationAction::ReplaceWorker(reason) => {
					if healthy_since.elapsed() >= config.healthy_reset {
						backoff = initial_backoff(&config);
					}
					process.terminate(config.interrupt_grace).await;
					process =
						respawn(&config, &expected_registrations, &mut generation, &mut backoff, reason)
							.await;
					host_generation.store(generation, Ordering::Release);
					let mut broker = service_router.broker.lock();
					for (owner, manifest) in &config.manifests {
						broker.deactivate_provider(owner, "provider process restarted");
						let _ = broker.activate_provider(
							owner,
							generation,
							manifest.services.provides().cloned(),
						);
					}
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
				Ok(SupervisorCommand::Open { id, owner, call, streams_args, events }) => {
					pending.push_back(PendingInvocation {
						id,
						owner,
						call,
						streams_args,
						arguments: VecDeque::new(),
						committed: None,
						interrupt: None,
						events,
					});
				},
				Ok(SupervisorCommand::ServiceDispatch { request_id, frame, reply }) => {
					let result = async {
						process
							.write(
								&HostFrame {
									request_id,
									body: Some(host_frame::Body::Lifecycle(LifecycleHostEnvelope {
										body: Some(lifecycle_host_envelope::Body::ServiceDispatch(frame)),
										props: None,
									})),
									props: None,
								},
								&config,
							)
							.await?;
						let response = process.read_timeout(&config).await?;
						let Some(worker_frame::Body::Lifecycle(envelope)) = response.body else {
							return Err(WorkerError::Protocol(Str::new_static(
								"provider did not return a lifecycle envelope",
							)));
						};
						let Some(lifecycle_worker_envelope::Body::ServiceResult(result)) =
							envelope.body
						else {
							return Err(WorkerError::Protocol(Str::new_static(
								"provider did not return ServiceResult",
							)));
						};
						if response.request_id != request_id {
							return Err(WorkerError::Protocol(Str::new_static(
								"provider ServiceResult correlation is stale",
							)));
						}
						Ok(result)
					}
					.await;
					let _ = reply.send(result);
				},
				Ok(SupervisorCommand::Shutdown) => {
					process.terminate(config.interrupt_grace).await;
					return;
				},
				Ok(command) => stage_pending(&mut pending, command),
				Err(_) => {
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
						RestartReason::HealthTimeout,
					)
					.await;
					host_generation.store(generation, Ordering::Release);
					healthy_since = Instant::now();
				}
			},
		}
	}
}

enum InvocationAction {
	KeepWorker,
	ReplaceWorker(RestartReason),
	Shutdown,
}

async fn dispatch_journal_control(
	process: &mut WorkerProcess,
	config: &ProcessConfig,
	invocation: &PendingInvocation,
	host_generation: u64,
	request_id: u64,
	envelope: omp_proto::toolhost::v1::JournalWorkerEnvelope,
) -> Result<(), WorkerError> {
	if request_id == 0 {
		return Err(WorkerError::Protocol(Str::new_static(
			"journal CONTROL request_id must be nonzero",
		)));
	}
	let runtime =
		config.journal.lock().clone().ok_or_else(|| {
			WorkerError::Protocol(Str::new_static("journal CONTROL is not installed"))
		})?;
	let manifest = config.manifests.get(&invocation.owner).ok_or_else(|| {
		WorkerError::Protocol(Str::new_static("journal CONTROL owner is not admitted"))
	})?;
	let committed = invocation.committed.as_ref().ok_or_else(|| {
		WorkerError::Protocol(Str::new_static("journal CONTROL cannot run before ArgsCommitted"))
	})?;
	let identity = JournalConnectionIdentity {
		principal: config.principal.clone(),
		provenance: manifest.provenance.clone(),
		host_generation,
		session_generation: config.session_generation,
	};
	let control = JournalControl::new(
		runtime.agent.clone(),
		invocation.owner.extension().clone(),
		Vec::new(),
		identity.clone(),
	);
	match control
		.dispatch(request_id, envelope, committed.authorized_at_ms)
		.await
		.map_err(|error| WorkerError::Protocol(Str::from(error.to_string())))?
	{
		JournalDispatch::Reply(reply) => {
			process
				.write(
					&HostFrame { request_id, body: Some(host_frame::Body::Journal(reply)), props: None },
					config,
				)
				.await
		},
		JournalDispatch::Rows { request_id, rows } => {
			for reply in crate::exthost::control::journal_rows(&rows) {
				process
					.write(
						&HostFrame {
							request_id,
							body: Some(host_frame::Body::Journal(reply)),
							props: None,
						},
						config,
					)
					.await?;
			}
			Ok(())
		},
		JournalDispatch::External(request) => {
			let (reply, replies) = flume::unbounded();
			runtime
				.external
				.send_async(ExternalJournalCall { request, identity, reply })
				.await
				.map_err(|_| WorkerError::Unavailable)?;
			while let Ok(row) = replies.recv_async().await {
				let reply = row.map_err(WorkerError::Protocol)?;
				process
					.write(
						&HostFrame {
							request_id,
							body: Some(host_frame::Body::Journal(reply)),
							props: None,
						},
						config,
					)
					.await?;
			}
			Ok(())
		},
	}
}

async fn dispatch_service_call(
	process: &mut WorkerProcess,
	config: &ProcessConfig,
	invocation: &PendingInvocation,
	router: &Arc<ServiceRouter>,
	host_generation: u64,
	request_id: u64,
	call: omp_proto::toolhost::v1::ServiceCall,
) -> Result<(), WorkerError> {
	if request_id == 0
		|| call.extension_id != invocation.owner.extension().as_str()
		|| call.host_generation != host_generation
		|| call.session_generation != config.session_generation
	{
		return Err(WorkerError::Protocol(Str::new_static(
			"service call identity or generation is stale",
		)));
	}
	let service = ServiceKey::new(call.service.as_str(), call.rev);
	let (dispatch, pending) = {
		let broker = router.broker.lock();
		let connection = broker
			.connect(&invocation.owner, service)
			.map_err(|error| WorkerError::Protocol(Str::from(error.to_string())))?;
		let ServiceConnection::Active(route) = connection else {
			return Err(WorkerError::Protocol(Str::new_static(
				"service provider requires activation",
			)));
		};
		broker
			.begin_call(
				route,
				ServiceRequestMeta {
					host_generation:    call.host_generation,
					session_generation: call.session_generation,
					deadline:           CoreDuration::new(call.deadline_ms, DurationUnit::Milliseconds),
				},
				call.method.as_str(),
				CowBytes::from(call.payload),
			)
			.map_err(|error| WorkerError::Protocol(Str::from(error.to_string())))?
	};
	let provider = router
		.routes
		.lock()
		.get(&dispatch.route.provider)
		.cloned()
		.ok_or_else(|| WorkerError::Protocol(Str::new_static("service provider is unavailable")))?;
	if provider.process_id == config.process_id {
		return Err(WorkerError::Protocol(Str::new_static(
			"reentrant service callback into the active worker is disabled",
		)));
	}
	let provider_generation = provider.generation.load(Ordering::Acquire);
	if provider_generation != dispatch.route.provider_generation {
		return Err(WorkerError::Protocol(Str::new_static("service provider generation is stale")));
	}
	let provider_id = dispatch.id.0;
	let provider_host = dispatch.route.provider.clone();
	let wire = WireServiceDispatch {
		provider_extension_id: provider_host.extension().to_string(),
		service: dispatch.route.service.name.to_string(),
		rev: dispatch.route.service.rev,
		method: dispatch.method.to_string(),
		payload: dispatch.payload.into_owned().to_vec().into(),
		deadline_ms: call.deadline_ms,
		caller_request_id: request_id,
		caller_host_generation: call.host_generation,
		session_generation: call.session_generation,
		provider_generation,
		props: None,
	};
	let (reply, response) = flume::bounded(1);
	provider
		.commands
		.send_async(SupervisorCommand::ServiceDispatch {
			request_id: provider_id,
			frame: wire,
			reply,
		})
		.await
		.map_err(|_| WorkerError::Unavailable)?;
	let result =
		tokio::time::timeout(Duration::from_millis(call.deadline_ms), response.recv_async())
			.await
			.map_err(|_| WorkerError::Protocol(Str::new_static("service call deadline elapsed")))?
			.map_err(|_| WorkerError::Unavailable)??;
	if result.caller_request_id != request_id || result.provider_generation != provider_generation {
		return Err(WorkerError::Protocol(Str::new_static(
			"provider ServiceResult identity is stale",
		)));
	}
	let response = if let Some(error) = result.error {
		if !result.payload.is_empty() {
			return Err(WorkerError::Protocol(Str::new_static(
				"provider ServiceResult carries both payload and error",
			)));
		}
		ServiceResponse::Failure(Str::from(error.message))
	} else {
		ServiceResponse::Success(CowBytes::from(result.payload))
	};
	router
		.broker
		.lock()
		.complete(&provider_host, provider_generation, ServiceCallId(provider_id), response)
		.map_err(|error| WorkerError::Protocol(Str::from(error.to_string())))?;
	let reply = match pending.response().await {
		Ok(payload) => ServiceReply {
			payload: payload.into_owned().to_vec().into(),
			error:   None,
			props:   None,
		},
		Err(error) => ServiceReply {
			payload: Bytes::new(),
			error:   Some(ProtocolError {
				code:    ProtocolErrorCode::Internal.into(),
				message: error.to_string(),
				props:   None,
			}),
			props:   None,
		},
	};
	process
		.write(
			&HostFrame {
				request_id,
				body: Some(host_frame::Body::Lifecycle(LifecycleHostEnvelope {
					body:  Some(lifecycle_host_envelope::Body::ServiceReply(reply)),
					props: None,
				})),
				props: None,
			},
			config,
		)
		.await
}
async fn run_invocation(
	config: &ProcessConfig,
	process: &mut WorkerProcess,
	mut invocation: PendingInvocation,
	mailbox: &flume::Receiver<SupervisorCommand>,
	pending: &mut VecDeque<PendingInvocation>,
	service_router: &Arc<ServiceRouter>,
	host_generation: u64,
) -> InvocationAction {
	let id = invocation.id;
	let call_id = invocation.call.invocation_id.clone();

	while !invocation.streams_args && invocation.committed.is_none() {
		match mailbox.recv_async().await {
			Ok(SupervisorCommand::ArgsCommitted { id: committed, frame }) if committed == id => {
				if frame.invocation_id != call_id.as_str() {
					send_host_protocol_error(
						&invocation,
						ProtocolErrorCode::InvalidArgument,
						"ArgsCommitted invocation id is stale",
					);
					return InvocationAction::KeepWorker;
				}
				invocation.committed = Some(frame);
			},
			Ok(SupervisorCommand::Cancel { id: cancelled, reason }) if cancelled == id => {
				let _ = invocation.events.send(WorkerEvent::Aborted(WorkerAbort {
					call_id,
					kind: WorkerAbortKind::Cancelled,
					reason,
					effects_unknown: false,
				}));
				return InvocationAction::KeepWorker;
			},
			Ok(SupervisorCommand::Interrupt { id: interrupted, frame }) if interrupted == id => {
				if frame.invocation_id == call_id.as_str() {
					invocation.interrupt = Some(frame);
				} else {
					send_host_protocol_error(
						&invocation,
						ProtocolErrorCode::InvalidArgument,
						"Interrupt invocation id is stale",
					);
				}
			},
			Ok(SupervisorCommand::ArgText { id: streamed, .. }) if streamed == id => {
				send_host_protocol_error(
					&invocation,
					ProtocolErrorCode::Unsupported,
					"tool declaration did not enable streams_args",
				);
			},
			Ok(SupervisorCommand::PullReply { id: replied, .. }) if replied == id => {
				send_host_protocol_error(
					&invocation,
					ProtocolErrorCode::Busy,
					"PullReply has no outstanding pull",
				);
			},
			Ok(SupervisorCommand::Open { id, owner, call, streams_args, events }) => {
				pending.push_back(PendingInvocation {
					id,
					owner,
					call,
					streams_args,
					arguments: VecDeque::new(),
					committed: None,
					interrupt: None,
					events,
				});
			},
			Ok(SupervisorCommand::Shutdown) | Err(_) => return InvocationAction::Shutdown,
			Ok(command) => stage_pending(pending, command),
		}
	}
	if invocation.events.is_disconnected() {
		return InvocationAction::KeepWorker;
	}

	let request_id = id.max(1);
	let args_json = invocation
		.committed
		.as_ref()
		.map_or_else(Bytes::new, |commit| commit.raw.clone());
	let frame = HostFrame {
		request_id,
		body: Some(host_frame::Body::InvokeTool(InvokeTool {
			call_id: call_id.to_string(),
			name: invocation.call.name.to_string(),
			args_json,
			deadline_ms: invocation
				.call
				.deadline
				.as_millis()
				.try_into()
				.unwrap_or(u64::MAX),
			rev: invocation.call.rev.to_string(),
			props: None,
		})),
		props: None,
	};
	if process.write(&frame, config).await.is_err() {
		send_abort(
			&invocation,
			WorkerAbortKind::Crashed,
			"worker exited before accepting invocation",
		);
		return InvocationAction::ReplaceWorker(RestartReason::Crash);
	}

	while let Some(fragment) = invocation.arguments.pop_front() {
		if write_argument_frame(
			process,
			config,
			request_id,
			argument_host_envelope::Body::ArgText(fragment),
		)
		.await
		.is_err()
		{
			send_abort(&invocation, WorkerAbortKind::Crashed, "worker exited during ArgText");
			return InvocationAction::ReplaceWorker(RestartReason::Crash);
		}
	}
	if let Some(commit) = invocation.committed.as_ref()
		&& write_argument_frame(
			process,
			config,
			request_id,
			argument_host_envelope::Body::ArgsCommitted(commit.clone()),
		)
		.await
		.is_err()
	{
		send_abort(&invocation, WorkerAbortKind::Crashed, "worker exited during ArgsCommitted");
		return InvocationAction::ReplaceWorker(RestartReason::Crash);
	}
	if let Some(interrupt) = invocation.interrupt.as_ref()
		&& write_argument_frame(
			process,
			config,
			request_id,
			argument_host_envelope::Body::Interrupt(interrupt.clone()),
		)
		.await
		.is_err()
	{
		send_abort(&invocation, WorkerAbortKind::Crashed, "worker exited during Interrupt");
		return InvocationAction::ReplaceWorker(RestartReason::Crash);
	}

	let deadline = Instant::now() + invocation.call.deadline;
	let mut pull_open = false;
	loop {
		tokio::select! {
			frame = read_async_frame::<_, WorkerFrame>(&mut process.stdout, config.max_frame_bytes, &mut process.read_scratch) => {
				let Ok(Some(frame)) = frame else {
					send_abort(&invocation, WorkerAbortKind::Crashed, "worker exited during invocation");
					return InvocationAction::ReplaceWorker(RestartReason::Crash);
				};
				if let Some(worker_frame::Body::Lifecycle(envelope)) = &frame.body
					&& let Some(lifecycle_worker_envelope::Body::SetAvailability(availability)) =
						&envelope.body
				{
					if availability.deltas.iter().any(|delta| !owns_availability(config, delta)) {
						send_abort(
							&invocation,
							WorkerAbortKind::Crashed,
							"worker availability named an undeclared device",
						);
						return InvocationAction::ReplaceWorker(RestartReason::ProtocolError);
					}
					let batch = AvailabilityBatch::from_wire(availability.clone());
					let sink = config.availability_sink.lock().as_ref().map(Arc::clone);
					match sink {
						Some(sink) => sink.set_availability(batch),
						None => config.availability_pending.lock().push_back(batch),
					}
					continue;
				}
				if let Some(worker_frame::Body::Lifecycle(envelope)) = &frame.body
					&& let Some(lifecycle_worker_envelope::Body::ResourceQuery(query)) =
						&envelope.body
				{
					if query.extension_id != invocation.owner.extension().as_str()
						|| send_resource_update(
							process,
							config,
							frame.request_id,
							query.extension_id.as_str(),
						)
						.await
						.is_err()
					{
						send_abort(
							&invocation,
							WorkerAbortKind::Crashed,
							"worker resource query was stale or could not be answered",
						);
						return InvocationAction::ReplaceWorker(RestartReason::ProtocolError);
					}
					continue;
				}
				if let Some(worker_frame::Body::Journal(envelope)) = &frame.body {
					if dispatch_journal_control(
						process,
						config,
						&invocation,
						host_generation,
						frame.request_id,
						envelope.clone(),
					)
					.await
					.is_err()
					{
						send_host_protocol_error(
							&invocation,
							ProtocolErrorCode::InvalidArgument,
							"journal CONTROL request was rejected",
						);
						return InvocationAction::ReplaceWorker(RestartReason::ProtocolError);
					}
					continue;
				}
				if let Some(worker_frame::Body::Lifecycle(envelope)) = &frame.body
					&& let Some(lifecycle_worker_envelope::Body::ServiceCall(call)) = &envelope.body
				{
					if let Err(error) = dispatch_service_call(
						process,
						config,
						&invocation,
						service_router,
						host_generation,
						frame.request_id,
						call.clone(),
					)
					.await
					{
						let _ = process
							.write(
								&HostFrame {
									request_id: frame.request_id,
									body: Some(host_frame::Body::Lifecycle(LifecycleHostEnvelope {
										body: Some(lifecycle_host_envelope::Body::ServiceReply(
											ServiceReply {
												payload: Bytes::new(),
												error: Some(ProtocolError {
													code: ProtocolErrorCode::InvalidArgument.into(),
													message: error.to_string(),
													props: None,
												}),
												props: None,
											},
										)),
										props: None,
									})),
									props: None,
								},
								config,
							)
							.await;
					}
					continue;
				}
				if frame.request_id != request_id {
					send_abort(&invocation, WorkerAbortKind::Crashed, "worker response request id did not match invocation");
					return InvocationAction::ReplaceWorker(RestartReason::ProtocolError);
				}
				match frame.body {
					Some(worker_frame::Body::ToolUpdate(update)) if update.call_id == call_id.as_str() => {
						if invocation.events.send(WorkerEvent::Update(update)).is_err() {
							cancel_worker(process, config, request_id, &call_id, "invocation receiver dropped").await;
							return InvocationAction::ReplaceWorker(RestartReason::CancelEscalation);
						}
					},
					Some(worker_frame::Body::ToolComplete(complete)) if complete.call_id == call_id.as_str() => {
						let Ok(complete) = WorkerCompletion::try_from(complete) else {
							send_abort(
								&invocation,
								WorkerAbortKind::Crashed,
								"worker sent an invalid ToolComplete",
							);
							return InvocationAction::ReplaceWorker(RestartReason::ProtocolError);
						};
						let _ = invocation.events.send(WorkerEvent::Complete(complete));
						return InvocationAction::KeepWorker;
					},
					Some(worker_frame::Body::Arguments(arguments)) => match arguments.body {
						Some(omp_proto::toolhost::v1::argument_worker_envelope::Body::PullRequest(pull)) => {
							if !invocation.streams_args || pull.call_id != call_id.as_str() || pull_open {
								let message = if pull_open {
									"only one argument pull may be outstanding"
								} else {
									"argument pull does not match a streaming invocation"
								};
								let issue = ArgIssue {
									kind: "protocol".into(),
									expected: message.into(),
									..Default::default()
								};
								let _ = invocation.events.send(WorkerEvent::Complete(args_rejected(&call_id, issue)));
								cancel_worker(process, config, request_id, &call_id, message).await;
								return InvocationAction::ReplaceWorker(RestartReason::CancelEscalation);
							}
							pull_open = true;
							if invocation.events.send(WorkerEvent::Pull(pull)).is_err() {
								cancel_worker(process, config, request_id, &call_id, "invocation receiver dropped").await;
								return InvocationAction::ReplaceWorker(RestartReason::CancelEscalation);
							}
						},
						Some(omp_proto::toolhost::v1::argument_worker_envelope::Body::ToolArgs(args)) => {
							if args.call_id != call_id.as_str() {
								send_abort(&invocation, WorkerAbortKind::Crashed, "ToolArgs call id did not match invocation");
								return InvocationAction::ReplaceWorker(RestartReason::ProtocolError);
							}
							let Some(issue) = args.issue else {
								send_abort(&invocation, WorkerAbortKind::Crashed, "ToolArgs omitted its ArgIssue");
								return InvocationAction::ReplaceWorker(RestartReason::ProtocolError);
							};
							let _ = invocation.events.send(WorkerEvent::Complete(args_rejected(&call_id, issue)));
							return InvocationAction::KeepWorker;
						},
						None => {
							send_host_protocol_error(&invocation, ProtocolErrorCode::Unsupported, "unsupported argument worker frame");
							return InvocationAction::ReplaceWorker(RestartReason::ProtocolError);
						},
					},
					Some(worker_frame::Body::ToolAborted(aborted)) if aborted.call_id == call_id.as_str() => {
						let _ = invocation.events.send(WorkerEvent::Aborted(WorkerAbort {
							call_id,
							kind: WorkerAbortKind::Crashed,
							reason: Str::from(aborted.reason),
							effects_unknown: aborted.effects_unknown,
						}));
						return InvocationAction::ReplaceWorker(RestartReason::ProtocolError);
					},
					Some(worker_frame::Body::Error(error)) => {
						let _ = invocation.events.send(WorkerEvent::ProtocolError(error));
						return InvocationAction::ReplaceWorker(RestartReason::ProtocolError);
					},
					_ => {
						send_host_protocol_error(&invocation, ProtocolErrorCode::Unsupported, "unsupported invocation worker frame");
						return InvocationAction::ReplaceWorker(RestartReason::ProtocolError);
					},
				}
			},
			command = mailbox.recv_async() => match command {
				Ok(SupervisorCommand::Cancel { id: cancelled, reason }) if cancelled == id => {
					cancel_worker(process, config, request_id, &call_id, reason.as_str()).await;
					let reason = cancellation_reason(config, &invocation.owner, reason.as_str());
					send_abort(&invocation, WorkerAbortKind::Cancelled, reason.as_str());
					return InvocationAction::ReplaceWorker(RestartReason::CancelEscalation);
				},
				Ok(SupervisorCommand::ArgText { id: streamed, frame }) if streamed == id => {
					if !invocation.streams_args || invocation.committed.is_some() || frame.invocation_id != call_id.as_str() {
						send_host_protocol_error(&invocation, ProtocolErrorCode::InvalidArgument, "stale or illegal ArgText");
						return InvocationAction::ReplaceWorker(RestartReason::ProtocolError);
					}
					if write_argument_frame(process, config, request_id, argument_host_envelope::Body::ArgText(frame)).await.is_err() {
						send_abort(&invocation, WorkerAbortKind::Crashed, "worker exited during ArgText");
						return InvocationAction::ReplaceWorker(RestartReason::Crash);
					}
				},
				Ok(SupervisorCommand::ArgsCommitted { id: committed, frame }) if committed == id => {
					if invocation.committed.is_some() || frame.invocation_id != call_id.as_str() {
						send_host_protocol_error(&invocation, ProtocolErrorCode::InvalidArgument, "stale or duplicate ArgsCommitted");
						return InvocationAction::ReplaceWorker(RestartReason::ProtocolError);
					}
					if write_argument_frame(process, config, request_id, argument_host_envelope::Body::ArgsCommitted(frame.clone())).await.is_err() {
						send_abort(&invocation, WorkerAbortKind::Crashed, "worker exited during ArgsCommitted");
						return InvocationAction::ReplaceWorker(RestartReason::Crash);
					}
					invocation.committed = Some(frame);
				},
				Ok(SupervisorCommand::PullReply { id: replied, reply }) if replied == id => {
					if !pull_open || reply.call_id != call_id.as_str() || reply.chunk.len() > omp_proto::bounds::PULL_CHUNK_MAX_BYTES {
						send_host_protocol_error(&invocation, ProtocolErrorCode::InvalidArgument, "stale, oversized, or unsolicited PullReply");
						return InvocationAction::ReplaceWorker(RestartReason::ProtocolError);
					}
					let terminal = reply.complete || reply.issue.is_some();
					if write_argument_frame(process, config, request_id, argument_host_envelope::Body::PullReply(reply)).await.is_err() {
						send_abort(&invocation, WorkerAbortKind::Crashed, "worker exited during PullReply");
						return InvocationAction::ReplaceWorker(RestartReason::Crash);
					}
					if terminal {
						pull_open = false;
					}
				},
				Ok(SupervisorCommand::Interrupt { id: interrupted, frame }) if interrupted == id => {
					if frame.invocation_id != call_id.as_str()
						|| write_argument_frame(process, config, request_id, argument_host_envelope::Body::Interrupt(frame)).await.is_err()
					{
						send_host_protocol_error(&invocation, ProtocolErrorCode::InvalidArgument, "stale or undeliverable Interrupt");
						return InvocationAction::ReplaceWorker(RestartReason::ProtocolError);
					}
				},
				Ok(SupervisorCommand::Open { id, owner, call, streams_args, events }) => {
					pending.push_back(PendingInvocation {
						id,
						owner,
						call,
						streams_args,
						arguments: VecDeque::new(),
						committed: None,
						interrupt: None,
						events,
					});
				},
				Ok(SupervisorCommand::Shutdown) | Err(_) => return InvocationAction::Shutdown,
				Ok(command) => stage_pending(pending, command),
			},
			() = tokio::time::sleep_until(deadline) => {
				cancel_worker(process, config, request_id, &call_id, "worker invocation timed out").await;
				send_abort(&invocation, WorkerAbortKind::TimedOut, "worker invocation timed out");
				return InvocationAction::ReplaceWorker(RestartReason::CancelEscalation);
			},
		}
	}
}

fn owns_availability(
	config: &ProcessConfig,
	delta: &omp_proto::toolhost::v1::AvailabilityDelta,
) -> bool {
	config.manifests.values().any(|manifest| {
		manifest
			.declarations
			.tools()
			.any(|tool| tool.name.as_str() == delta.name && tool.rev.to_string() == delta.rev)
	})
}

async fn write_argument_frame(
	process: &mut WorkerProcess,
	config: &ProcessConfig,
	request_id: u64,
	body: argument_host_envelope::Body,
) -> Result<(), WorkerError> {
	process
		.write(
			&HostFrame {
				request_id,
				body: Some(host_frame::Body::Arguments(ArgumentHostEnvelope {
					body:  Some(body),
					props: None,
				})),
				props: None,
			},
			config,
		)
		.await
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

fn stage_pending(pending: &mut VecDeque<PendingInvocation>, command: SupervisorCommand) {
	let command = match command {
		SupervisorCommand::ServiceDispatch { reply, .. } => {
			let _ = reply.send(Err(WorkerError::Protocol(Str::new_static(
				"provider worker is busy; reentrant callbacks are disabled",
			))));
			return;
		},
		command => command,
	};
	let id = match &command {
		SupervisorCommand::ArgText { id, .. }
		| SupervisorCommand::ArgsCommitted { id, .. }
		| SupervisorCommand::PullReply { id, .. }
		| SupervisorCommand::Cancel { id, .. }
		| SupervisorCommand::Interrupt { id, .. } => *id,
		SupervisorCommand::Open { .. }
		| SupervisorCommand::ServiceDispatch { .. }
		| SupervisorCommand::Shutdown => return,
	};
	let Some(index) = pending.iter().position(|invocation| invocation.id == id) else {
		return;
	};
	if let SupervisorCommand::Cancel { reason, .. } = &command {
		let reason = reason.clone();
		let invocation = pending
			.remove(index)
			.expect("the located queued invocation exists");
		let _ = invocation.events.send(WorkerEvent::Aborted(WorkerAbort {
			call_id: invocation.call.invocation_id,
			kind: WorkerAbortKind::Cancelled,
			reason,
			effects_unknown: false,
		}));
		return;
	}
	let invocation = &mut pending[index];
	match command {
		SupervisorCommand::ArgText { frame, .. }
			if invocation.streams_args
				&& invocation.committed.is_none()
				&& frame.invocation_id == invocation.call.invocation_id.as_str() =>
		{
			invocation.arguments.push_back(frame);
		},
		SupervisorCommand::ArgsCommitted { frame, .. }
			if invocation.committed.is_none()
				&& frame.invocation_id == invocation.call.invocation_id.as_str() =>
		{
			invocation.committed = Some(frame);
		},
		SupervisorCommand::Interrupt { frame, .. }
			if frame.invocation_id == invocation.call.invocation_id.as_str() =>
		{
			invocation.interrupt = Some(frame);
		},
		SupervisorCommand::PullReply { .. } => send_host_protocol_error(
			invocation,
			ProtocolErrorCode::Busy,
			"PullReply has no outstanding pull",
		),
		SupervisorCommand::ArgText { .. } => send_host_protocol_error(
			invocation,
			ProtocolErrorCode::Unsupported,
			"stale ArgText or declaration did not enable streams_args",
		),
		SupervisorCommand::ArgsCommitted { .. } => send_host_protocol_error(
			invocation,
			ProtocolErrorCode::InvalidArgument,
			"stale or duplicate ArgsCommitted",
		),
		SupervisorCommand::Interrupt { .. } => {
			send_host_protocol_error(
				invocation,
				ProtocolErrorCode::InvalidArgument,
				"stale Interrupt",
			);
		},
		SupervisorCommand::Open { .. }
		| SupervisorCommand::ServiceDispatch { .. }
		| SupervisorCommand::Cancel { .. }
		| SupervisorCommand::Shutdown => {},
	}
}

fn send_host_protocol_error(
	invocation: &PendingInvocation,
	code: ProtocolErrorCode,
	message: &'static str,
) {
	let _ = invocation
		.events
		.send(WorkerEvent::ProtocolError(ProtocolError {
			code:    code.into(),
			message: message.into(),
			props:   None,
		}));
}

fn args_rejected(call_id: &Str, issue: ArgIssue) -> WorkerCompletion {
	WorkerCompletion {
		call_id:      call_id.clone(),
		kind:         WorkerOutcomeKind::ArgsRejected,
		parts:        Vec::new(),
		details_json: Some(Bytes::from_static(b"null")),
		details_blob: None,
		args_issue:   Some(issue),
		useless:      false,
	}
}

fn send_abort(invocation: &PendingInvocation, kind: WorkerAbortKind, reason: &str) {
	let _ = invocation.events.send(WorkerEvent::Aborted(WorkerAbort {
		call_id: invocation.call.invocation_id.clone(),
		kind,
		reason: Str::from(reason),
		effects_unknown: invocation.committed.is_some(),
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
			owner.extension(),
		))
	} else {
		Str::from(format!(
			"{reason}; effects unknown for {}; no other extension host was terminated",
			owner.extension(),
		))
	}
}

async fn respawn(
	config: &ProcessConfig,
	expected: &[ToolDecl],
	generation: &mut u64,
	backoff: &mut Duration,
	reason: RestartReason,
) -> WorkerProcess {
	let max_delay = config.max_backoff.max(Duration::from_millis(1));
	loop {
		tokio::time::sleep(*backoff).await;
		*generation = generation.wrapping_add(1).max(1);
		match WorkerProcess::spawn(config, *generation, ActivationCause::Restart(reason)).await {
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
		Ok(Self {
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

fn validate_manifest_registrations(
	config: &ProcessConfig,
	tools: &[ToolDecl],
) -> Result<(), WorkerError> {
	for (owner, manifest) in &config.manifests {
		let actual = actual_declarations(config, tools, owner)?;
		if actual != manifest.declarations {
			return Err(WorkerError::Protocol(manifest_registration_diff(
				owner,
				manifest,
				tools,
				&actual,
			)));
		}
	}
	Ok(())
}

fn manifest_registration_diff(
	owner: &HostKey,
	manifest: &ExtensionManifest,
	tools: &[ToolDecl],
	actual: &DeclarationSet,
) -> Str {
	let missing = manifest
		.declarations
		.tools()
		.filter(|expected| !actual.tools().any(|registered| *registered == **expected))
		.map(|tool| format!("{}@{}:{}", tool.name, tool.family, tool.rev))
		.collect::<Vec<_>>();
	let unexpected = actual
		.tools()
		.filter(|registered| !manifest.declarations.tools().any(|expected| *expected == **registered))
		.map(|tool| format!("{}@{}:{}", tool.name, tool.family, tool.rev))
		.collect::<Vec<_>>();
	let mismatches = manifest
		.declarations
		.tools()
		.filter_map(|expected| {
			actual.tools().find(|registered| registered.name == expected.name).and_then(|registered| {
				(registered.rev != expected.rev || registered.family != expected.family).then(|| {
					format!(
						"name {} has registered rev {}@{} instead of {}@{}",
						expected.name, registered.family, registered.rev, expected.family, expected.rev
					)
				})
			})
		})
		.collect::<Vec<_>>();
	let flags = tools
		.iter()
		.filter(|tool| tool.extension_id == owner.extension().as_str())
		.map(|tool| {
			format!(
				"{}: streams_args={}, effects={}",
				tool.definition.as_ref().map_or("", |definition| definition.name.as_str()),
				tool.streams_args,
				tool.effects.is_some()
			)
		})
		.collect::<Vec<_>>();
	Str::from(format!(
		"frozen worker declarations differ from authenticated manifest for {}: missing=[{}]; \
		 unexpected=[{}]; name/rev mismatches=[{}]; registered flags=[{}]",
		owner.extension(),
		missing.join(", "),
		unexpected.join(", "),
		mismatches.join(", "),
		flags.join(", "),
	))
}

fn actual_declarations(
	_config: &ProcessConfig,
	tools: &[ToolDecl],
	owner: &HostKey,
) -> Result<DeclarationSet, WorkerError> {
	let tools = tools
		.iter()
		.filter(|tool| tool.extension_id == owner.extension().as_str())
		.map(|tool| {
			let definition = tool.definition.as_ref().ok_or_else(|| {
				WorkerError::Protocol(Str::new_static("registered tool has no definition"))
			})?;
			let rev = tool.rev.parse::<omp_tool::Rev>().map_err(|_| {
				WorkerError::Protocol(Str::new_static("registered tool revision is not canonical"))
			})?;
			Ok(ToolDeclarationKey::new(definition.name.as_str(), rev.family, rev.n))
		})
		.collect::<Result<Vec<_>, WorkerError>>()?;
	Ok(DeclarationSet::new(tools, []))
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
pub fn run_py_worker_entry() -> Result<(), WorkerError> {
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
	install_scheme_snapshot()?;
	serve_worker(&engine, &modules)
}

fn install_scheme_snapshot() -> Result<(), WorkerError> {
	let Ok(encoded) = env::var("OMP_EXT_SCHEME_SNAPSHOT") else {
		return Ok(());
	};
	let value: serde_json::Value = serde_json::from_str(&encoded)
		.map_err(|error| WorkerError::Protocol(Str::from(error.to_string())))?;
	let hash_values = value
		.get("device_hash")
		.and_then(serde_json::Value::as_array)
		.ok_or_else(|| WorkerError::Protocol(Str::new_static("scheme snapshot has no hash")))?;
	let hash = <[u8; 32]>::try_from(
		hash_values
			.iter()
			.map(|value| value.as_u64().and_then(|value| u8::try_from(value).ok()))
			.collect::<Option<Vec<_>>>()
			.ok_or_else(|| WorkerError::Protocol(Str::new_static("scheme snapshot hash is invalid")))?
			.as_slice(),
	)
	.map_err(|_| WorkerError::Protocol(Str::new_static("scheme snapshot hash is invalid")))?;
	let entries = value
		.get("entries")
		.and_then(serde_json::Value::as_array)
		.ok_or_else(|| WorkerError::Protocol(Str::new_static("scheme snapshot has no entries")))?
		.iter()
		.map(|entry| {
			let entry = entry.as_array().ok_or_else(|| {
				WorkerError::Protocol(Str::new_static("scheme snapshot entry is invalid"))
			})?;
			let [member, readable, mintable, selectors, description] = entry.as_slice() else {
				return Err(WorkerError::Protocol(Str::new_static("scheme snapshot entry is invalid")));
			};
			Ok((
				Str::from(member.as_str().ok_or_else(|| {
					WorkerError::Protocol(Str::new_static("scheme snapshot member is invalid"))
				})?),
				readable.as_bool().ok_or_else(|| {
					WorkerError::Protocol(Str::new_static("scheme snapshot readable bit is invalid"))
				})?,
				mintable.as_bool().ok_or_else(|| {
					WorkerError::Protocol(Str::new_static("scheme snapshot mintable bit is invalid"))
				})?,
				selectors.as_bool().ok_or_else(|| {
					WorkerError::Protocol(Str::new_static("scheme snapshot selector bit is invalid"))
				})?,
				Str::from(description.as_str().ok_or_else(|| {
					WorkerError::Protocol(Str::new_static("scheme snapshot description is invalid"))
				})?),
			))
		})
		.collect::<Result<Vec<_>, WorkerError>>()?;
	omp_py::set_scheme_snapshot(hash, entries);
	Ok(())
}

fn serve_worker(engine: &omp_py::Engine, modules: &[Str]) -> Result<(), WorkerError> {
	engine.attach(|py| -> PyResult<()> {
		let sys = PyModule::import(py, "sys")?;
		if let Ok(site) = env::var("OMP_PY_SITE") {
			let path = sys.getattr("path")?;
			let path = path.cast::<PyList>()?;
			path.insert(0, site)?;
		}
		sys.setattr("stdout", sys.getattr("stderr")?)?;
		Ok(())
	})?;
	let layer = required_env("OMP_EXT_LAYER")?;
	let tier = required_env("OMP_EXT_TIER")?;
	let pool = env::var("OMP_EXT_POOL").unwrap_or_default();
	let host_generation = required_env_u64("OMP_EXT_HOST_GENERATION")?;
	let session_generation = required_env_u64("OMP_EXT_SESSION_GENERATION")?;
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
				layer,
				tier,
				pool,
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
	let admit_frame = read_sync_frame::<_, HostFrame>(&mut reader, limit, &mut read_scratch)?
		.ok_or(WorkerError::Exited)?;
	let HostFrame {
		request_id: 0,
		body:
			Some(host_frame::Body::Lifecycle(LifecycleHostEnvelope {
				body: Some(lifecycle_host_envelope::Body::AdmitExtensions(admitted)),
				..
			})),
		..
	} = admit_frame
	else {
		return Err(WorkerError::Protocol(Str::new_static(
			"AdmitExtensions must follow WorkerHello",
		)));
	};
	if admitted.generation != host_generation {
		return Err(WorkerError::Protocol(Str::new_static("AdmitExtensions generation is stale")));
	}
	let admitted_modules = admitted
		.extensions
		.iter()
		.map(|extension| (extension.module.as_str(), extension.extension_id.as_str()))
		.collect::<BTreeMap<_, _>>();
	if admitted_modules.len() != modules.len()
		|| modules
			.iter()
			.any(|module| !admitted_modules.contains_key(module.as_str()))
	{
		return Err(WorkerError::Protocol(Str::new_static(
			"AdmitExtensions modules differ from the spawned worker configuration",
		)));
	}
	let mut tools = load_tools(engine, modules)?;
	for tool in &mut tools {
		let extension_id = admitted_modules
			.get(tool.decl.extension_id.as_str())
			.expect("loaded tool module was admitted");
		tool.decl.extension_id = (*extension_id).to_owned();
	}
	let declarations = tools.iter().map(|tool| tool.decl.clone()).collect();
	let entry_modules =
		admitted
			.extensions
			.iter()
			.fold(BTreeMap::<Str, Str>::new(), |mut entries, extension| {
				entries
					.entry(Str::from(extension.extension_id.as_str()))
					.or_insert_with(|| Str::from(extension.module.as_str()));
				entries
			});
	let mut seen_extensions = HashSet::new();
	let extensions = admitted
		.extensions
		.into_iter()
		.filter(|extension| seen_extensions.insert(extension.extension_id.clone()))
		.map(|extension| ExtensionDecl {
			extension_id: extension.extension_id,
			version:      extension.rev,
			api_level:    1,
			capabilities: Vec::new(),
			props:        None,
		})
		.collect();
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
	fn dispatch_python_service(
		engine: &omp_py::Engine,
		request_id: u64,
		dispatch: &WireServiceDispatch,
	) -> Result<Vec<u8>, WorkerError> {
		let payload = std::str::from_utf8(&dispatch.payload).map_err(|_| {
			WorkerError::Protocol(Str::new_static("service payload is not UTF-8 JSON"))
		})?;
		engine
			.attach(|py| -> PyResult<Vec<u8>> {
				let json = PyModule::import(py, "json")?;
				let decoded = json.call_method1("loads", (payload,))?;
				let args = decoded.get_item("args")?;
				let kwargs = decoded.get_item("kwargs")?;
				let registry = PyModule::import(py, "omp._registry")?;
				let awaitable = registry.call_method1(
					"dispatch_service",
					(
						request_id,
						dispatch.service.as_str(),
						dispatch.rev,
						dispatch.method.as_str(),
						args,
						kwargs,
					),
				)?;
				let asyncio = PyModule::import(py, "asyncio")?;
				let result = asyncio.call_method1("run", (awaitable,))?;
				let echoed: u64 = result.get_item(0)?.extract()?;
				if echoed != request_id {
					return Err(PyValueError::new_err("service provider returned stale correlation"));
				}
				let value = result.get_item(1)?;
				let encoded: String = json.call_method1("dumps", (value,))?.extract()?;
				Ok(encoded.into_bytes())
			})
			.map_err(WorkerError::from)
	}

	loop {
		let Some(frame) = read_sync_frame::<_, HostFrame>(&mut reader, limit, &mut read_scratch)?
		else {
			return Ok(());
		};
		match frame.body {
			Some(host_frame::Body::Lifecycle(LifecycleHostEnvelope {
				body: Some(lifecycle_host_envelope::Body::FreezeDeclarations(freeze)),
				..
			})) => {
				if freeze.generation != host_generation
					|| !entry_modules.contains_key(freeze.extension_id.as_str())
				{
					return Err(WorkerError::Protocol(Str::new_static(
						"FreezeDeclarations carries stale extension identity or generation",
					)));
				}
				freeze_python_declarations(engine)?;
			},
			Some(host_frame::Body::Lifecycle(LifecycleHostEnvelope {
				body: Some(lifecycle_host_envelope::Body::ActivateExtension(activate)),
				..
			})) => {
				let result = activate_python_extension(engine, &entry_modules, &activate);
				let (degraded, error) = match result {
					Ok(()) => (false, None),
					Err(error) => (true, Some(error.to_string())),
				};
				write_sync_frame(
					&mut writer,
					&WorkerFrame {
						request_id: frame.request_id,
						body:       Some(worker_frame::Body::Lifecycle(
							omp_proto::toolhost::v1::LifecycleWorkerEnvelope {
								body:  Some(lifecycle_worker_envelope::Body::ExtensionActivated(
									ExtensionActivated {
										extension_id: activate.extension_id,
										generation: activate.generation,
										degraded,
										error,
										props: None,
									},
								)),
								props: None,
							},
						)),
						props:      None,
					},
					limit,
					&mut write_scratch,
				)?;
			},
			Some(host_frame::Body::Lifecycle(LifecycleHostEnvelope {
				body: Some(lifecycle_host_envelope::Body::ResourceUpdate(update)),
				..
			})) => {
				omp_py::set_resource_receipt(
					update.quotas.into_iter().map(|quota| {
						(
							Str::from(quota.name),
							quota.limit,
							quota.used,
							quota
								.window_ms
								.map(|millis| CoreDuration::new(millis, DurationUnit::Milliseconds)),
						)
					}),
					update
						.dropped
						.into_iter()
						.map(|drop| (Str::from(drop.name), drop.count)),
				);
			},
			Some(host_frame::Body::Lifecycle(LifecycleHostEnvelope {
				body: Some(lifecycle_host_envelope::Body::ServiceDispatch(dispatch)),
				..
			})) => {
				let result = if dispatch.provider_generation != host_generation
					|| dispatch.session_generation != session_generation
					|| !entry_modules.contains_key(dispatch.provider_extension_id.as_str())
				{
					Err(WorkerError::Protocol(Str::new_static(
						"ServiceDispatch carries stale provider identity or generation",
					)))
				} else {
					dispatch_python_service(engine, frame.request_id, &dispatch)
				};
				let (payload, error) = match result {
					Ok(payload) => (payload, None),
					Err(error) => (
						Vec::new(),
						Some(ProtocolError {
							code:    ProtocolErrorCode::Internal.into(),
							message: error.to_string(),
							props:   None,
						}),
					),
				};
				write_sync_frame(
					&mut writer,
					&WorkerFrame {
						request_id: frame.request_id,
						body:       Some(worker_frame::Body::Lifecycle(
							omp_proto::toolhost::v1::LifecycleWorkerEnvelope {
								body:  Some(lifecycle_worker_envelope::Body::ServiceResult(
									ServiceResult {
										caller_request_id: dispatch.caller_request_id,
										provider_generation: dispatch.provider_generation,
										payload: payload.into(),
										error,
										props: None,
									},
								)),
								props: None,
							},
						)),
						props:      None,
					},
					limit,
					&mut write_scratch,
				)?;
			},
			Some(host_frame::Body::InvokeTool(invoke)) => {
				let Some(commit_frame) =
					read_sync_frame::<_, HostFrame>(&mut reader, limit, &mut read_scratch)?
				else {
					return Ok(());
				};
				let commit = match commit_frame {
					HostFrame {
						request_id,
						body:
							Some(host_frame::Body::Arguments(ArgumentHostEnvelope {
								body: Some(argument_host_envelope::Body::ArgsCommitted(commit)),
								..
							})),
						..
					} if request_id == frame.request_id
						&& commit.invocation_id == invoke.call_id
						&& commit.raw == invoke.args_json =>
					{
						commit
					},
					_ => {
						write_protocol_error(
							&mut writer,
							frame.request_id,
							ProtocolErrorCode::InvalidArgument,
							"non-streaming InvokeTool must be followed by its exact ArgsCommitted",
							limit,
							&mut write_scratch,
						)?;
						continue;
					},
				};
				debug_assert_eq!(commit.raw, invoke.args_json);
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

fn freeze_python_declarations(engine: &omp_py::Engine) -> Result<(), WorkerError> {
	engine
		.attach(|py| -> PyResult<()> {
			PyModule::import(py, "omp._registry")?
				.getattr("freeze_declarations")?
				.call0()?;
			Ok(())
		})
		.map_err(WorkerError::from)
}

fn activate_python_extension(
	engine: &omp_py::Engine,
	entry_modules: &BTreeMap<Str, Str>,
	activate: &ActivateExtension,
) -> Result<(), WorkerError> {
	if activate.generation == 0 {
		return Err(WorkerError::Protocol(Str::new_static(
			"ActivateExtension generation must be nonzero",
		)));
	}
	let module = entry_modules
		.get(activate.extension_id.as_str())
		.ok_or_else(|| WorkerError::Protocol(Str::new_static("ActivateExtension is not admitted")))?
		.clone();
	engine
		.attach(|py| -> PyResult<()> {
			let extension = PyModule::import(py, module.as_str())?;
			let Ok(callback) = extension.getattr("extension_activate") else {
				return Ok(());
			};
			let payload = PyDict::new(py);
			payload.set_item("reason", activate.reason)?;
			payload.set_item("restart_reason", activate.restart_reason)?;
			payload.set_item("session_started_at_ms", activate.session_started_at_ms)?;
			payload.set_item("generation", activate.generation)?;
			let context = PyDict::new(py);
			if let Some(principal) = &activate.principal {
				let identity = PyDict::new(py);
				identity.set_item("id", principal.id.as_str())?;
				identity.set_item("display", principal.display.as_str())?;
				context.set_item("principal", identity)?;
			}
			let result = callback.call1((payload, context))?;
			if result.hasattr("__await__")? {
				PyModule::import(py, "asyncio")?
					.getattr("run")?
					.call1((result,))?;
			}
			Ok(())
		})
		.map_err(WorkerError::from)
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
					let streams_args = dict
						.get_item("streams_args")?
						.map(|value| value.extract::<bool>())
						.transpose()?
						.unwrap_or(false);
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
							streams_args,
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
				args_issue:   None,
			});
		}
		let details_json = Bytes::from(
			json
				.getattr("dumps")?
				.call1((&value,))?
				.extract::<String>()?,
		);
		let text = value.str()?.to_string_lossy().into_owned();
		Ok(PythonCompletion {
			parts: vec![text_part(text)],
			details_json,
			kind: OutcomeKind::Ok,
			args_issue: None,
		})
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
			args_issue:   None,
		},
	};
	let PythonCompletion { parts, details_json, kind, args_issue } = completion;
	let body = if let Some(issue) = args_issue {
		worker_frame::Body::Arguments(ArgumentWorkerEnvelope {
			body:  Some(argument_worker_envelope::Body::ToolArgs(ToolArgs {
				call_id,
				issue: Some(issue),
				props: None,
			})),
			props: None,
		})
	} else {
		worker_frame::Body::ToolComplete(ToolComplete {
			call_id,
			parts,
			details_json,
			is_error: matches!(kind, OutcomeKind::Faulted),
			kind: kind.into(),
			props: None,
			..Default::default()
		})
	};
	write_sync_frame(
		writer,
		&WorkerFrame { request_id, body: Some(body), props: None },
		limit,
		scratch,
	)
}

struct PythonCompletion {
	parts:        Vec<Part>,
	details_json: Bytes,
	kind:         OutcomeKind,
	args_issue:   Option<ArgIssue>,
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
	let args_issue = dict
		.get_item("args_issue")?
		.map(|issue| {
			let issue = issue
				.cast::<PyDict>()
				.map_err(|_| WorkerError::Python(Str::new_static("args_issue must be a dictionary")))?;
			python_arg_issue(issue)
		})
		.transpose()?;
	let kind = if args_issue.is_some() {
		OutcomeKind::ArgsRejected
	} else if dict
		.get_item("is_error")?
		.map(|value| value.extract::<bool>())
		.transpose()?
		.unwrap_or(false)
	{
		OutcomeKind::Faulted
	} else {
		OutcomeKind::Ok
	};
	Ok(PythonCompletion { parts, details_json, kind, args_issue })
}

fn python_arg_issue(dict: &Bound<'_, PyDict>) -> Result<ArgIssue, WorkerError> {
	let path = match dict.get_item("path")? {
		Some(path) => PyIterator::from_object(&path)?
			.map(|segment| segment.and_then(|segment| segment.extract::<String>()))
			.collect::<PyResult<Vec<_>>>()?,
		None => Vec::new(),
	};
	Ok(ArgIssue {
		path,
		expected: optional_string(dict, "expected")?.unwrap_or_default(),
		kind: optional_string(dict, "kind")?.unwrap_or_else(|| "protocol".into()),
		example: optional_string(dict, "example")?,
		found: optional_string(dict, "found")?,
		props: None,
	})
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
