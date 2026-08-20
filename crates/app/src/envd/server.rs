//! Transport-neutral `env/v1` dispatch and owner-local UDS serving.

use std::{
	collections::{BTreeMap, HashMap},
	io,
	ops::ControlFlow,
	path::{Path, PathBuf},
	sync::{
		Arc,
		atomic::{AtomicU8, AtomicU64, Ordering},
	},
	time::Duration,
};

use bytes::{Bytes, BytesMut};
use futures::StreamExt as _;
use omp_core::Str;
use omp_env::{EnvClient, InProcessEnvTransport};
use omp_proto::{
	blob::v1 as blob_pb,
	document::v1 as document_pb,
	env::v1::{self as pb, client_frame, server_frame},
	prost::Message as _,
};
use omp_storage::{index::SessionIndex, state::StateStore};
use omp_tool::{
	Abort, ArgIssue, ArgPath, CallOutcome, Effects, ErasedEv, ErasedOutcome, IncomingParams,
	Interrupt, Registry, RegistryError, ToolRoute, ToolTerminal,
};
use omp_tools::device::{DeviceInvokeRequest, DeviceInvoker};
use omp_walker::{
	CompiledWalkGlob, DirectoryErrorMode, FileType, FollowLinks, WalkDetail, WalkFilter,
	WalkOptions, WalkOrder, WalkRequest, WalkStatus,
};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio_util::sync::CancellationToken;
use url::Url;

use super::{
	admission::{AdmissionDecision, AdmissionGate, effects_narrow_or_refuse},
	blobs::{BlobError, BlobHost},
	docs::{
		DocumentError, DocumentEvents, DocumentHost, DocumentLease, LspEvents, LspRegistryEvent,
	},
	eval::SessionBridgeHost,
	exec::{ExecError, ExecEvent, ExecHost, ProcessEvent},
	journal_runtime::ExternalJournalActor,
	policy::{AuthorityTable, DataAuthority, Grants, PolicyError, QuotaAccount},
	site::{SiteError, SiteMaterializer, record_modules},
	tools::production_registry,
	worker::{
		ExtHostConfig, ExtHostSpec, ExtHostSupervisor, HostKey, JournalRuntime, OpenToolCall,
		WorkerError, WorkerEvent, WorkerOutcomeKind,
	},
	worker_pool::{WorkerKey, WorkerRoute, WorkerSupervisor, WorkerUnavailable},
	workspace::{
		WorkspaceError, WorkspaceHost, WorkspaceOperationError, WorkspaceOperations,
		WorkspaceSearchCase, WorkspaceSearchOptions,
	},
};
use crate::{cli::EnvdArgs, exthost::RegistryAvailabilitySink};

const MIN_SCHEMA_REV: u32 = 4;
const FRAME_LIMIT: usize = 64 * 1024 * 1024;
const BLOB_CHUNK_BYTES: usize = 64 * 1024;
const DEFAULT_TOOL_DEADLINE: Duration = Duration::from_secs(300);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const NATIVE_CANCEL_GRACE: Duration = Duration::from_millis(250);
const INVOCATION_RESPONSE_SEND_GRACE: Duration = Duration::from_millis(250);
const WORKER_LAYER_CEILING: u64 = 8;
const MAX_CONCURRENT_SPAWNS: u64 = 4;
static NEXT_CONNECTION_OWNER: AtomicU64 = AtomicU64::new(1);

/// Environment-daemon assembly or serving failure.
#[derive(Debug, Error)]
pub enum EnvdError {
	/// A local filesystem, socket, or child-process operation failed.
	#[error(transparent)]
	Io(#[from] io::Error),
	/// The document authority could not be connected or verified.
	#[error("document authority failed: {0}")]
	Document(Str),
	/// The canonical workspace could not be opened.
	#[error("workspace host failed: {0}")]
	Workspace(Str),
	/// The content-addressed blob store could not be opened.
	#[error("blob host failed: {0}")]
	Blob(Str),
	/// The authoritative sessions index could not be opened.
	#[error("sessions index failed: {0}")]
	SessionIndex(Str),
	/// The non-session durable state authority could not be opened.
	#[error("state authority failed: {0}")]
	State(Str),
	/// The embedded Python runtime used by `eval` could not be initialized.
	#[error("eval runtime failed: {0}")]
	Eval(Str),
	/// The Python tool worker could not be started or supervised.
	#[error(transparent)]
	Worker(#[from] WorkerError),
	/// A native or worker tool declaration could not be registered.
	#[error(transparent)]
	Registry(#[from] RegistryError),
	/// A worker advertised a declaration that cannot have a stable registry
	/// identity.
	#[error("invalid worker tool declaration: {0}")]
	WorkerDeclaration(Str),
	/// The selected edit dialect was not a registered built-in revision.
	#[error("invalid edit dialect: {0}")]
	EditDialect(Str),
	/// Production assembly encountered a second live declaration for one name.
	#[error("duplicate production tool name: {0}")]
	DuplicateToolName(Str),
	/// The environment client could not complete its protocol handshake.
	#[error(transparent)]
	Client(#[from] omp_env::ClientError),
	/// A spawned environment connection task failed.
	#[error("environment connection task failed: {0}")]
	Task(#[from] tokio::task::JoinError),
	/// The embedded document authority exited before accepting a verified hello.
	#[error("embedded document authority exited before its hello handshake")]
	DocserverExited,
	/// Another process already serves this project's document authority.
	#[error(
		"project document authority is already served by another process; retry after it drains"
	)]
	DocumentAuthorityHeld,
}

impl From<DocumentError> for EnvdError {
	fn from(error: DocumentError) -> Self {
		Self::Document(Str::from(error.to_string()))
	}
}

impl From<WorkspaceError> for EnvdError {
	fn from(error: WorkspaceError) -> Self {
		Self::Workspace(Str::from(error.to_string()))
	}
}

impl From<BlobError> for EnvdError {
	fn from(error: BlobError) -> Self {
		Self::Blob(Str::from(error.to_string()))
	}
}

impl From<WorkspaceOperationError> for EnvdError {
	fn from(error: WorkspaceOperationError) -> Self {
		Self::Workspace(Str::from(error.to_string()))
	}
}

/// Identity advertised by every transport served from one environment.
#[derive(Clone, Debug)]
pub struct ServerIdentity {
	/// Canonical document workspace identity.
	pub workspace_id:   Bytes,
	/// Canonical workspace root URI.
	pub root_uri:       Str,
	/// Epoch of the connected document authority.
	pub server_epoch:   Bytes,
	/// Human-readable server build version.
	pub server_version: Str,
	/// Build identity of the serving environment; empty means unknown.
	pub server_build:   Str,
}

/// Per-connection transport and exact DATA grant bounds.
#[derive(Clone)]
pub(crate) struct ConnectionPolicy {
	retire:  Option<CancellationToken>,
	grants:  Grants,
	host:    Option<HostKey>,
	ambient: bool,
}

impl ConnectionPolicy {
	fn in_process() -> Self {
		Self { retire: None, grants: Grants::all(), host: None, ambient: true }
	}

	/// Grants owner-local lifecycle traffic while retaining DATA phase checks.
	pub(crate) fn external(retire: Option<CancellationToken>) -> Self {
		Self { retire, grants: Grants::all(), host: None, ambient: false }
	}

	/// Restricts an extension-host connection to explicitly granted, reachable
	/// DATA capabilities.
	pub(crate) fn extension<I, S>(host: HostKey, grants: I) -> Self
	where
		I: IntoIterator<Item = S>,
		S: AsRef<str>,
	{
		Self {
			retire:  None,
			grants:  Grants::supported(grants),
			host:    Some(host),
			ambient: false,
		}
	}
}

/// One extension host's isolated DATA listener identity and grants.
#[derive(Clone, Debug)]
pub(crate) struct ExtensionDataBinding {
	key:    HostKey,
	path:   PathBuf,
	grants: Grants,
}

impl ExtensionDataBinding {
	/// Derives the deterministic owner-local socket path and the exact
	/// built-in read/walk/search grant set for `key`.
	#[must_use]
	pub(crate) fn built_in(
		state_dir: &Path,
		key: HostKey,
		session_id: &str,
		session_generation: u64,
	) -> Self {
		let mut hasher = blake3::Hasher::new();
		hasher.update(b"omp/extension-data-binding/v1");
		hasher.update(&(session_id.len() as u64).to_le_bytes());
		hasher.update(session_id.as_bytes());
		hasher.update(&session_generation.to_le_bytes());
		for field in key.fields() {
			hasher.update(&(field.len() as u64).to_le_bytes());
			hasher.update(field.as_bytes());
		}
		let digest = omp_core::encoding::hex::encode_n(hasher.finalize().as_bytes());
		let grants = Grants::supported([
			"env.doc.read",
			"env.doc.write",
			"env.fs.read",
			"env.fs.write",
			"env.exec",
			"env.blob",
			"env.search",
			"env.lsp",
		]);
		Self { key, path: state_dir.join("ext-env").join(format!("{digest}.sock")), grants }
	}

	/// Returns the socket path passed only to this binding's child.
	pub(crate) fn path(&self) -> &Path {
		&self.path
	}

	/// Returns the exact grants enforced for this listener.
	pub(crate) const fn grants(&self) -> &Grants {
		&self.grants
	}

	/// Returns the immutable connection policy carried by this listener.
	pub(crate) fn policy(&self) -> ConnectionPolicy {
		ConnectionPolicy::extension(self.key.clone(), self.grants.iter())
	}
}
struct DocumentAuthority {
	shutdown: CancellationToken,
	#[cfg(unix)]
	task:     Option<tokio::task::JoinHandle<omp_docserver::daemon::Result>>,
	#[cfg(windows)]
	task: Option<tokio::task::JoinHandle<Result<(), omp_docserver::windows::WindowsTransportError>>>,
}

#[cfg(unix)]
impl DocumentAuthority {
	async fn finished_result(
		&mut self,
	) -> Option<Result<omp_docserver::daemon::Result, tokio::task::JoinError>> {
		if !self.task.as_ref()?.is_finished() {
			return None;
		}
		Some(
			self
				.task
				.take()
				.expect("finished document authority task")
				.await,
		)
	}
}

impl Drop for DocumentAuthority {
	fn drop(&mut self) {
		self.shutdown.cancel();
	}
}

/// Concrete environment host shared by in-process and UDS connections.
///
/// Executors remain env-side beside these resources. The server never passes a
/// capability/facet trait bundle through a tool signature.
/// Device-router bridge for final, worker-routed invocations.
#[derive(Clone)]
struct WorkerDeviceInvoker {
	hosts: Arc<ExtHostSupervisor>,
}

impl WorkerDeviceInvoker {
	fn new(hosts: Arc<ExtHostSupervisor>) -> Self {
		Self { hosts }
	}
}

impl DeviceInvoker for WorkerDeviceInvoker {
	async fn invoke(&self, request: DeviceInvokeRequest) -> omp_tool::ErasedStream<'static> {
		let hosts = Arc::clone(&self.hosts);
		Box::pin(async_stream::stream! {
			let deadline = match request.deadline.to_std() {
				Ok(deadline) => deadline,
				Err(error) => {
					yield Err(RegistryError::VerdictShape(Str::from(error.to_string())));
					return;
				},
			};
			let mut invocation = match hosts.open(OpenToolCall {
				invocation_id: request.invocation_id.clone(),
				name: request.name.clone(),
				rev: request.rev.clone(),
				deadline,
			}) {
				Ok(invocation) => invocation,
				Err(error) => {
					yield Err(RegistryError::VerdictShape(Str::from(error.to_string())));
					return;
				},
			};
			let committed = omp_proto::env::v1::ArgsCommitted {
				invocation_id: request.invocation_id.to_string(),
				raw: request.args_json,
				..omp_proto::env::v1::ArgsCommitted::default()
			};
			if let Err(error) = invocation.args_committed(committed) {
				yield Err(RegistryError::VerdictShape(Str::from(error.to_string())));
				return;
			}
			while let Ok(event) = invocation.next().await {
				match event {
					WorkerEvent::Update(update) => yield Ok(ErasedEv::Update(Bytes::from(update.encode_to_vec()))),
					WorkerEvent::Complete(complete) => match worker_completion_json(&complete) {
						Ok((verdict, _, _)) => {
							yield Ok(ErasedEv::Done(ErasedOutcome::Done {
								verdict,
								useless: complete.useless,
							}));
							return;
						},
						Err(error) => {
							yield Err(RegistryError::VerdictShape(error));
							return;
						},
					},
					WorkerEvent::Aborted(abort) => {
						yield Err(RegistryError::VerdictShape(abort.reason));
						return;
					},
					WorkerEvent::Pull(_) | WorkerEvent::ProtocolError(_) => {
						yield Err(RegistryError::VerdictShape(Str::new_static("worker device protocol rejected final invocation")));
						return;
					},
				}
			}
		})
	}
}

/// Owner-local `env/v1` dispatch state serving one project environment.
pub struct EnvServer {
	identity:            ServerIdentity,
	documents:           DocumentHost,
	_document_authority: Option<DocumentAuthority>,
	exec:                ExecHost,
	workspace:           WorkspaceHost,
	blobs:               BlobHost,
	sites:               SiteMaterializer,
	registry:            Arc<Registry>,
	workspace_ops:       WorkspaceOperations,
	ext_hosts:           Arc<ExtHostSupervisor>,
	eval_bridge:         Arc<SessionBridgeHost>,
	eval_control:        omp_tools::eval::EvalSessionControl,
	sessions_index:      Arc<SessionIndex>,
	journal_external:    ExternalJournalActor,
	workers:             Arc<WorkerSupervisor>,
	authority:           Arc<AuthorityTable>,
}

fn open_journal_authorities(
	state_dir: &Path,
	writer: bool,
) -> Result<(Arc<SessionIndex>, Option<Arc<StateStore>>), EnvdError> {
	let index_path = state_dir.join("sessions.sqlite3");
	let sessions_index = if writer {
		SessionIndex::open(index_path)
	} else {
		SessionIndex::open_authoritative_reader(index_path)
	}
	.map_err(|error| EnvdError::SessionIndex(Str::from(error.to_string())))?;
	let state_store = writer
		.then(|| StateStore::open(state_dir.join("state")))
		.transpose()
		.map_err(|error| EnvdError::State(Str::from(error.to_string())))?
		.map(Arc::new);
	Ok((Arc::new(sessions_index), state_store))
}

fn worker_operation(request: &pb::WorkerOp) -> &'static str {
	use pb::worker_op::Op;

	match request.op.as_ref() {
		Some(Op::Open(_)) => "omp.env.worker.open",
		Some(Op::Close(_)) => "omp.env.worker.close",
		Some(Op::Data(_)) => "omp.env.worker.data",
		Some(Op::Info(_)) => "omp.env.worker.info",
		Some(Op::List(_)) | None => "omp.env.worker.list",
	}
}

fn worker_info(route: &WorkerRoute) -> pb::WorkerInfo {
	pb::WorkerInfo {
		name: route.key.name.to_string(),
		generation: route.generation,
		state: pb::WorkerState::Ready as i32,
		..pb::WorkerInfo::default()
	}
}

impl EnvServer {
	#[must_use]
	fn new(
		identity: ServerIdentity,
		documents: DocumentHost,
		document_authority: Option<DocumentAuthority>,
		exec: ExecHost,
		workspace: WorkspaceHost,
		blobs: BlobHost,
		sites: SiteMaterializer,
		registry: Arc<Registry>,
		workspace_ops: WorkspaceOperations,
		ext_hosts: Arc<ExtHostSupervisor>,
		eval_bridge: Arc<SessionBridgeHost>,
		eval_control: omp_tools::eval::EvalSessionControl,
		sessions_index: Arc<SessionIndex>,
		journal_external: ExternalJournalActor,
		authority: Arc<AuthorityTable>,
	) -> Self {
		Self {
			identity,
			documents,
			_document_authority: document_authority,
			exec,
			workspace,
			blobs,
			sites,
			registry,
			workspace_ops,
			ext_hosts,
			eval_bridge,
			eval_control,
			sessions_index,
			journal_external,
			workers: Arc::new(WorkerSupervisor::new(WORKER_LAYER_CEILING, MAX_CONCURRENT_SPAWNS)),
			authority,
		}
	}

	/// Opens a complete local environment host rooted at `root`.
	///
	/// The document authority, workspace, blob store, executor, and Python
	/// worker are real environment-owned resources. `state_dir` is kept
	/// separate from the workspace so callers can use an isolated scratch
	/// directory without adding daemon state to the project tree.
	pub async fn open_local(
		root: &Path,
		state_dir: &Path,
		registry: Registry,
		mut ext_host_config: ExtHostConfig,
	) -> Result<Self, EnvdError> {
		let workspace = WorkspaceHost::open(root)?;
		let doc_config = omp_docserver::ServerConfig::new(root)
			.map_err(|error| EnvdError::Document(Str::from(error.to_string())))?
			.with_server_build(crate::build_id::current());
		let environment = omp_docserver::Environment::new(doc_config)
			.map_err(|error| EnvdError::Document(Str::from(error.to_string())))?;
		let (document_client, document_server) = tokio::io::duplex(64 * 1024);
		tokio::spawn(async move {
			let _ = omp_docserver::connection::serve_connection(
				environment,
				document_server,
				omp_docserver::connection::ConnectionConfig::default(),
			)
			.await;
		});
		let documents = DocumentHost::connect(document_client).await?;
		let hello = documents.hello().clone();
		let interrupt_grace = ext_host_config.interrupt_grace;
		let session_id = ext_host_config.session_id.clone();
		let (sessions_index, state_store) = open_journal_authorities(state_dir, true)?;
		let authority = Arc::new(AuthorityTable::default());
		ext_host_config.bind_data_authority(Arc::clone(&authority));
		let ext_hosts = Arc::new(ExtHostSupervisor::spawn(ext_host_config).await?);
		let exec = ExecHost::new();
		let blobs = BlobHost::open(state_dir.join("blobs"))?;
		let sites = SiteMaterializer::open(state_dir.join("ext"), blobs.store().clone())
			.map_err(|error| EnvdError::Blob(Str::from(error.to_string())))?;
		let project_scope = Str::from(omp_core::hex::encode(&hello.workspace_id).to_string());
		let project_path = Str::from(workspace.root().to_string_lossy().as_ref());
		let journal_external = ExternalJournalActor::spawn(
			Arc::clone(&sessions_index),
			state_store.clone(),
			blobs.clone(),
			session_id,
			project_scope,
			project_path,
		)?;
		let workspace_ops = WorkspaceOperations::open(
			workspace.clone(),
			documents.clone(),
			blobs.clone(),
			state_dir.join("workspace-ops"),
		)?;
		let tool_settings = crate::settings::Settings::load(state_dir).tools;
		let (registry, eval_bridge, eval_control) = production_registry(
			&documents,
			&blobs,
			&exec,
			&workspace,
			&hello.root_uri,
			ext_hosts.as_ref(),
			interrupt_grace,
			&tool_settings,
			WorkerDeviceInvoker::new(Arc::clone(&ext_hosts)),
			omp_tool::ToolsPolicy::Auto,
			registry,
		)?;
		let identity = ServerIdentity {
			workspace_id:   hello.workspace_id,
			root_uri:       hello.root_uri,
			server_epoch:   hello.server_epoch,
			server_version: Str::from(env!("CARGO_PKG_VERSION")),
			server_build:   Str::from(crate::build_id::current()),
		};
		Ok(Self::new(
			identity,
			documents,
			None,
			exec,
			workspace,
			blobs,
			sites,
			registry,
			workspace_ops,
			ext_hosts,
			eval_bridge,
			eval_control,
			sessions_index,
			journal_external,
			authority,
		))
	}

	/// Opens project resources through the owner-local document authority.
	#[cfg(any(unix, windows))]
	pub(crate) async fn open_project(
		root: &Path,
		state_dir: &Path,
		docserver_socket: &Path,
		registry: Registry,
		mut ext_host_config: ExtHostConfig,
		doc_connections: Option<tokio::sync::watch::Sender<usize>>,
	) -> Result<Self, EnvdError> {
		let workspace = WorkspaceHost::open(root)?;
		let root = workspace.root().to_path_buf();
		let (documents, document_authority) =
			connect_or_start_docserver(&root, docserver_socket, doc_connections.clone()).await?;
		let hello = documents.hello().clone();
		let interrupt_grace = ext_host_config.interrupt_grace;
		let session_id = ext_host_config.session_id.clone();
		let writer = doc_connections.is_none();
		let (sessions_index, state_store) = open_journal_authorities(state_dir, writer)?;
		let authority = Arc::new(AuthorityTable::default());
		ext_host_config.bind_data_authority(Arc::clone(&authority));
		let ext_hosts = Arc::new(ExtHostSupervisor::spawn(ext_host_config).await?);
		let exec = ExecHost::new();
		let blobs = BlobHost::open(state_dir.join("blobs"))?;
		let sites = SiteMaterializer::open(state_dir.join("ext"), blobs.store().clone())
			.map_err(|error| EnvdError::Blob(Str::from(error.to_string())))?;
		let project_scope = Str::from(omp_core::hex::encode(&hello.workspace_id).to_string());
		let project_path = Str::from(root.to_string_lossy().as_ref());
		let journal_external = ExternalJournalActor::spawn(
			Arc::clone(&sessions_index),
			state_store.clone(),
			blobs.clone(),
			session_id,
			project_scope,
			project_path,
		)?;
		let workspace_ops = WorkspaceOperations::open(
			workspace.clone(),
			documents.clone(),
			blobs.clone(),
			state_dir.join("workspace-ops"),
		)?;
		let tool_settings = crate::settings::Settings::load(state_dir).tools;
		let (registry, eval_bridge, eval_control) = production_registry(
			&documents,
			&blobs,
			&exec,
			&workspace,
			&hello.root_uri,
			ext_hosts.as_ref(),
			interrupt_grace,
			&tool_settings,
			WorkerDeviceInvoker::new(Arc::clone(&ext_hosts)),
			omp_tool::ToolsPolicy::Auto,
			registry,
		)?;
		let identity = ServerIdentity {
			workspace_id:   hello.workspace_id,
			root_uri:       hello.root_uri,
			server_epoch:   hello.server_epoch,
			server_version: Str::from(env!("CARGO_PKG_VERSION")),
			server_build:   Str::from(crate::build_id::current()),
		};
		Ok(Self::new(
			identity,
			documents,
			document_authority,
			exec,
			workspace,
			blobs,
			sites,
			registry,
			workspace_ops,
			ext_hosts,
			eval_bridge,
			eval_control,
			sessions_index,
			journal_external,
			authority,
		))
	}

	/// Connects an `EnvClient` transport to an owner-only environment socket.
	#[cfg(unix)]
	pub async fn connect_owner_uds(
		path: &Path,
	) -> Result<(EnvClient, tokio::task::JoinHandle<Result<(), EnvdError>>), EnvdError> {
		use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};

		let metadata = tokio::fs::symlink_metadata(path).await?;
		if !metadata.file_type().is_socket()
			|| metadata.uid() != nix::unistd::geteuid().as_raw()
			|| metadata.permissions().mode() & 0o077 != 0
		{
			return Err(
				io::Error::new(
					io::ErrorKind::PermissionDenied,
					"environment socket must be owner-only and owned by the current user",
				)
				.into(),
			);
		}
		let stream = tokio::net::UnixStream::connect(path).await?;
		let (client, transport) = EnvClient::in_process(64);
		let (requests, responses) = transport.into_parts();
		let task = tokio::spawn(async move {
			let (mut reader, mut writer) = stream.into_split();
			let shutdown = CancellationToken::new();
			let read_shutdown = shutdown.clone();
			let read = async move {
				let mut scratch = BytesMut::new();
				loop {
					let frame = tokio::select! {
						() = read_shutdown.cancelled() => return Ok::<(), io::Error>(()),
						result = read_server_frame(&mut reader, &mut scratch) => result?,
					};
					let Some(frame) = frame else { return Ok(()) };
					if responses.send_async(frame).await.is_err() {
						return Ok(());
					}
				}
			};
			let write = async move {
				let result = async {
					let mut scratch = BytesMut::new();
					while let Ok(frame) = requests.recv_async().await {
						write_client_frame(&mut writer, &frame, &mut scratch).await?;
					}
					Ok::<(), io::Error>(())
				}
				.await;
				shutdown.cancel();
				result
			};
			let (read_result, write_result) = tokio::join!(read, write);
			read_result?;
			write_result?;
			Ok(())
		});
		Ok((client, task))
	}

	/// Enforces persisted RECORD ownership before a trusted extension imports a
	/// module from its materialized site tree.
	pub(crate) fn require_record_owner(
		&self,
		site_key: &str,
		module: impl Into<Str>,
		owner: impl Into<Str>,
	) -> Result<(), SiteError> {
		self.sites.require_record_owner(site_key, module, owner)
	}

	/// Returns the exact registry shared by this server's dispatch paths.
	#[must_use]
	pub fn registry(&self) -> Arc<Registry> {
		Arc::clone(&self.registry)
	}

	/// Returns the session bridge binding retained by this environment.
	pub(crate) fn eval_bridge(&self) -> Arc<SessionBridgeHost> {
		Arc::clone(&self.eval_bridge)
	}

	pub(crate) fn eval_control(&self) -> omp_tools::eval::EvalSessionControl {
		self.eval_control.clone()
	}

	/// Returns the single authoritative sessions index shared with the Agent
	/// Journal.
	#[must_use]
	pub(crate) fn sessions_index(&self) -> Arc<SessionIndex> {
		Arc::clone(&self.sessions_index)
	}

	/// Binds the active Agent Journal mailbox to authenticated extension
	/// CONTROL.
	///
	/// # Errors
	///
	/// Fails if journal routing was already bound or an extension child already
	/// activated.
	pub(crate) fn bind_agent_control(
		&self,
		sender: omp_agent::control::ControlSender,
	) -> Result<(), EnvdError> {
		self.journal_external.bind_agent(sender.clone())?;
		self.ext_hosts.bind_journal_runtime(JournalRuntime {
			agent:    sender,
			external: self.journal_external.sender(),
		})?;
		Ok(())
	}

	/// Binds extension device availability to the active Agent turn boundary.
	pub(crate) fn bind_device_availability(&self, mailbox: omp_agent::MailboxSender) {
		self
			.ext_hosts
			.bind_availability_sink(Arc::new(RegistryAvailabilitySink::new(
				Arc::clone(&self.registry),
				mailbox,
			)));
	}

	/// Serves the server half returned by [`omp_env::EnvClient::in_process`].
	pub async fn serve_in_process(&self, transport: InProcessEnvTransport) {
		let (requests, responses) = transport.into_parts();
		self
			.serve_frames(requests, responses, ConnectionPolicy::in_process())
			.await;
	}

	/// Serves one already-accepted byte stream with varint protobuf framing.
	pub async fn serve_io<S>(&self, stream: S) -> Result<(), EnvdError>
	where
		S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
	{
		self
			.serve_io_with_policy(stream, ConnectionPolicy::external(None))
			.await
	}

	pub(crate) async fn serve_io_with_policy<S>(
		&self,
		stream: S,
		policy: ConnectionPolicy,
	) -> Result<(), EnvdError>
	where
		S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
	{
		let (mut reader, mut writer) = tokio::io::split(stream);
		let (request_tx, requests) = flume::bounded(64);
		let (responses, response_rx) = flume::bounded(64);
		let retire = policy.retire.clone();
		let dispatch = self.serve_frames(requests, responses, policy);
		let io_shutdown = CancellationToken::new();
		let read_shutdown = io_shutdown.clone();
		let read = async move {
			let mut scratch = BytesMut::new();
			loop {
				let frame = tokio::select! {
					() = read_shutdown.cancelled() => return Ok::<(), io::Error>(()),
					result = read_client_frame(&mut reader, &mut scratch) => result?,
				};
				let Some(frame) = frame else { return Ok(()) };
				if request_tx.send_async(frame).await.is_err() {
					return Ok(());
				}
			}
		};
		let write = async move {
			let result = async {
				let mut scratch = BytesMut::new();
				while let Ok(frame) = response_rx.recv_async().await {
					write_server_frame(&mut writer, &frame, &mut scratch).await?;
					if matches!(frame.body, Some(server_frame::Body::RetireStarted(_)))
						&& let Some(retire) = &retire
					{
						retire.cancel();
					}
				}
				Ok::<(), io::Error>(())
			}
			.await;
			io_shutdown.cancel();
			result
		};
		let (read_result, (), write_result) = tokio::join!(read, dispatch, write);
		read_result?;
		write_result?;
		Ok(())
	}

	/// Binds and serves an owner-only project Unix socket until cancellation.
	///
	/// Retirement unlinks the path immediately and drains accepted
	/// connections; external shutdown aborts them. A stale non-accepting
	/// socket file is replaced; a live listener yields
	/// [`io::ErrorKind::AddrInUse`].
	#[cfg(unix)]
	pub async fn serve_uds(
		self: Arc<Self>,
		path: &Path,
		shutdown: CancellationToken,
		connection_gauge: Option<tokio::sync::watch::Sender<usize>>,
	) -> Result<(), EnvdError> {
		self
			.serve_uds_with_policy(path, shutdown, None, connection_gauge)
			.await
	}

	/// Binds a Unix socket whose connections are restricted to one extension
	/// host binding.
	#[cfg(unix)]
	pub(crate) async fn serve_extension_uds(
		self: Arc<Self>,
		binding: ExtensionDataBinding,
		shutdown: CancellationToken,
	) -> Result<(), EnvdError> {
		self
			.authority
			.register_host(binding.key.clone(), binding.grants.clone());
		let policy = binding.policy();
		self
			.serve_uds_with_policy(&binding.path, shutdown, Some(policy), None)
			.await
	}

	#[cfg(unix)]
	async fn serve_uds_with_policy(
		self: Arc<Self>,
		path: &Path,
		shutdown: CancellationToken,
		connection_policy: Option<ConnectionPolicy>,
		connection_gauge: Option<tokio::sync::watch::Sender<usize>>,
	) -> Result<(), EnvdError> {
		use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};

		let parent = path.parent().ok_or_else(|| {
			io::Error::new(io::ErrorKind::InvalidInput, "environment socket has no parent")
		})?;
		ensure_directory(parent)?;
		match tokio::fs::symlink_metadata(path).await {
			Ok(metadata) if metadata.file_type().is_socket() => {
				if tokio::net::UnixStream::connect(path).await.is_ok() {
					return Err(
						io::Error::new(
							io::ErrorKind::AddrInUse,
							"environment socket is already accepting connections",
						)
						.into(),
					);
				}
				tokio::fs::remove_file(path).await?;
			},
			Ok(_) => {
				return Err(
					io::Error::new(
						io::ErrorKind::AlreadyExists,
						"refusing to replace a non-socket environment path",
					)
					.into(),
				);
			},
			Err(error) if error.kind() == io::ErrorKind::NotFound => {},
			Err(error) => return Err(error.into()),
		}
		let listener = tokio::net::UnixListener::bind(path)?;
		tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
		let socket_metadata = std::fs::symlink_metadata(path)?;
		let retire = CancellationToken::new();
		let mut listener = Some(listener);
		let mut connections = tokio::task::JoinSet::new();
		let mut abort_connections = false;
		if let Some(gauge) = &connection_gauge {
			gauge.send_replace(0);
		}
		loop {
			if retire.is_cancelled() && listener.is_some() {
				drop(listener.take());
				if let Ok(metadata) = std::fs::symlink_metadata(path)
					&& metadata.dev() == socket_metadata.dev()
					&& metadata.ino() == socket_metadata.ino()
				{
					let _ = tokio::fs::remove_file(path).await;
				}
				if connections.is_empty() {
					break;
				}
			}
			tokio::select! {
				() = shutdown.cancelled() => {
					abort_connections = true;
					break;
				},
				() = retire.cancelled(), if listener.is_some() => {},
				accepted = async {
					listener.as_ref().expect("guarded listener").accept().await
				}, if listener.is_some() => {
					let (stream, _) = accepted?;
					let server = Arc::clone(&self);
					let policy = connection_policy.clone().unwrap_or_else(|| {
						ConnectionPolicy::external(Some(retire.clone()))
					});
					connections.spawn(async move {
						server.serve_io_with_policy(stream, policy).await
					});
					if let Some(gauge) = &connection_gauge {
						gauge.send_replace(connections.len());
					}
				},
				completed = connections.join_next(), if !connections.is_empty() => {
					if let Some(gauge) = &connection_gauge {
						gauge.send_replace(connections.len());
					}
					match completed {
						Some(Ok(Ok(()))) | None => {},
						Some(Ok(Err(error))) => return Err(error),
						Some(Err(error)) => return Err(error.into()),
					}
					if listener.is_none() && connections.is_empty() {
						break;
					}
				},
			}
		}
		if listener.take().is_some()
			&& let Ok(metadata) = std::fs::symlink_metadata(path)
			&& metadata.dev() == socket_metadata.dev()
			&& metadata.ino() == socket_metadata.ino()
		{
			let _ = tokio::fs::remove_file(path).await;
		}
		if abort_connections {
			connections.abort_all();
			while let Some(result) = connections.join_next().await {
				if let Err(error) = result
					&& !error.is_cancelled()
				{
					return Err(error.into());
				}
			}
		}
		Ok(())
	}

	async fn serve_frames(
		&self,
		requests: flume::Receiver<pb::ClientFrame>,
		responses: flume::Sender<pb::ServerFrame>,
		policy: ConnectionPolicy,
	) {
		let first = match tokio::time::timeout(HANDSHAKE_TIMEOUT, requests.recv_async()).await {
			Ok(Ok(first)) => first,
			Ok(Err(_)) => return,
			Err(_) => {
				send_error(
					&responses,
					0,
					pb::ProtocolErrorCode::DeadlineExceeded,
					"environment hello handshake timed out",
				)
				.await;
				return;
			},
		};
		let Some(grants) = self.accept_hello(first, &responses, &policy).await else {
			return;
		};
		let (finished_tx, finished) = flume::unbounded();
		let mut connection =
			ConnectionState::new(self.exec.clone(), grants, Arc::clone(&self.authority), &policy);
		loop {
			let admission_deadline = connection.next_admission_deadline();
			let next = tokio::select! {
				result = requests.recv_async() => match result {
					Ok(frame) => Some(LoopEvent::Frame(Box::new(frame))),
					Err(_) => None,
				},
				result = finished.recv_async() => match result {
					Ok(done) => Some(LoopEvent::Finished(done)),
					Err(_) => None,
				},
				() = async {
					if let Some(deadline) = admission_deadline {
						tokio::time::sleep_until(deadline).await;
					} else {
						std::future::pending::<()>().await;
					}
				} => Some(LoopEvent::AdmissionDeadline),
			};
			let Some(next) = next else { break };
			match next {
				LoopEvent::Finished(done) => connection.finish(done),
				LoopEvent::AdmissionDeadline => {
					for (request_id, invocation_id, denied) in connection.take_expired_admissions() {
						connection.abandon_admission(request_id, &invocation_id);
						send_policy_denied_verdict(&responses, request_id, &invocation_id, denied).await;
					}
				},
				LoopEvent::Frame(frame) => {
					while let Ok(done) = finished.try_recv() {
						connection.finish(done);
					}
					self
						.dispatch(*frame, &responses, &finished_tx, &mut connection, &policy)
						.await;
				},
			}
		}
		connection.cancel_all(&self.exec);
	}

	async fn accept_hello(
		&self,
		frame: pb::ClientFrame,
		responses: &flume::Sender<pb::ServerFrame>,
		policy: &ConnectionPolicy,
	) -> Option<Grants> {
		let Some(client_frame::Body::Hello(hello)) = frame.body else {
			send_error(
				responses,
				frame.request_id,
				pb::ProtocolErrorCode::InvalidArgument,
				"the first client frame must be ClientHello",
			)
			.await;
			return None;
		};
		if frame.request_id != 0 {
			send_error(
				responses,
				frame.request_id,
				pb::ProtocolErrorCode::InvalidArgument,
				"ClientHello must use request_id 0",
			)
			.await;
			return None;
		}
		if hello.schema_rev < MIN_SCHEMA_REV || hello.schema_rev > omp_proto::SCHEMA_REV {
			send_error(
				responses,
				0,
				pb::ProtocolErrorCode::Unsupported,
				&format!(
					"unsupported env schema revision {}; server supports {MIN_SCHEMA_REV}..={}",
					hello.schema_rev,
					omp_proto::SCHEMA_REV
				),
			)
			.await;
			return None;
		}
		let grants = if hello.capabilities.is_empty() && policy.host.is_none() {
			policy.grants.clone()
		} else {
			policy.grants.requested(&hello.capabilities)
		};
		responses
			.send_async(server_frame(
				0,
				server_frame::Body::Hello(pb::ServerHello {
					schema_rev:     omp_proto::SCHEMA_REV,
					min_schema_rev: MIN_SCHEMA_REV,
					capabilities:   grants.iter().map(str::to_owned).collect(),
					server_version: self.identity.server_version.to_string(),
					workspace_id:   self.identity.workspace_id.clone(),
					root_uri:       self.identity.root_uri.to_string(),
					server_epoch:   self.identity.server_epoch.clone(),
					server_build:   self.identity.server_build.to_string(),
					props:          Default::default(),
				}),
			))
			.await
			.ok()
			.map(|()| grants)
	}

	async fn dispatch(
		&self,
		frame: pb::ClientFrame,
		responses: &flume::Sender<pb::ServerFrame>,
		finished: &flume::Sender<Finished>,
		connection: &mut ConnectionState,
		policy: &ConnectionPolicy,
	) {
		let scope = frame.scope.clone();
		let Some(body) = frame.body else {
			send_error(
				responses,
				frame.request_id,
				pb::ProtocolErrorCode::InvalidArgument,
				"client frame body is missing",
			)
			.await;
			return;
		};
		let worker_scope = connection.host.as_ref().is_some_and(|host| {
			scope.as_ref().is_some_and(|scope| {
				connection
					.authority
					.is_worker_invocation(host, &scope.invocation_id)
			})
		});
		if matches!(&body, client_frame::Body::InvokeTool(_))
			&& (!connection.grants.contains("invocation") || worker_scope)
		{
			send_error(
				responses,
				frame.request_id,
				pb::ProtocolErrorCode::PermissionDenied,
				"connection was not granted invocation dispatch",
			)
			.await;
			return;
		}
		if let client_frame::Body::Cancel(cancel) = body {
			if frame.request_id != 0 {
				send_error(
					responses,
					frame.request_id,
					pb::ProtocolErrorCode::InvalidArgument,
					"cancel control frames must use request_id 0",
				)
				.await;
				return;
			}
			connection
				.cancel(cancel, &self.exec, responses, finished)
				.await;
			return;
		}
		if frame.request_id == 0 {
			send_error(
				responses,
				0,
				pb::ProtocolErrorCode::InvalidArgument,
				"ordinary frames must use a nonzero request_id",
			)
			.await;
			return;
		}
		if let Some((operation, capability)) = frame_data_operation(&body)
			&& !authorize_data_operation(
				connection,
				scope.as_ref(),
				operation,
				capability,
				responses,
				frame.request_id,
			)
			.await
		{
			return;
		}
		let continuation = matches!(
			&body,
			client_frame::Body::ArgText(_)
				| client_frame::Body::Admission(_)
				| client_frame::Body::ArgsCommitted(_)
				| client_frame::Body::Interrupt(_)
				| client_frame::Body::Stdin(_)
				| client_frame::Body::Signal(_)
				| client_frame::Body::Resize(_)
				| client_frame::Body::BlobPutChunk(_)
				| client_frame::Body::BlobPutCommit(_)
		);
		if !continuation && connection.requests.contains_key(&frame.request_id) {
			send_error(
				responses,
				frame.request_id,
				pb::ProtocolErrorCode::AlreadyExists,
				"request_id is already open",
			)
			.await;
			return;
		}

		match body {
			client_frame::Body::Hello(_) => {
				send_error(
					responses,
					frame.request_id,
					pb::ProtocolErrorCode::AlreadyExists,
					"the connection hello is already complete",
				)
				.await;
			},
			client_frame::Body::Retire(_) => {
				if policy.retire.is_some() {
					send_body(
						responses,
						frame.request_id,
						server_frame::Body::RetireStarted(pb::RetireStarted::default()),
					)
					.await;
				} else {
					send_error(
						responses,
						frame.request_id,
						pb::ProtocolErrorCode::Unsupported,
						"retire is not available on this transport",
					)
					.await;
				}
			},
			client_frame::Body::InvokeTool(request) => {
				self
					.open_invocation(frame.request_id, request, responses, finished, connection)
					.await;
			},
			client_frame::Body::ArgText(request) => {
				let result = connection.invocation_mut(frame.request_id, &request.invocation_id);
				let query = match result {
					Ok(InvocationState::Native { feed, lifecycle, admission, .. })
						if !lifecycle.is_committed() && !lifecycle.is_terminal() =>
					{
						let query = admission.push_fragment(
							&request.fragment,
							self.workspace.root(),
							self.workspace.root(),
						);
						if feed.arg_text(Str::from(request.fragment)).is_err() {
							send_error(
								responses,
								frame.request_id,
								pb::ProtocolErrorCode::Cancelled,
								"invocation input is closed",
							)
							.await;
							None
						} else {
							query
						}
					},
					Ok(InvocationState::Worker {
						invocation: Some(invocation),
						committed,
						admission,
						..
					}) if !*committed => {
						let query = admission.push_fragment(
							&request.fragment,
							self.workspace.root(),
							self.workspace.root(),
						);
						if let Err(error) = invocation.arg_text(request) {
							send_error(
								responses,
								frame.request_id,
								pb::ProtocolErrorCode::PreconditionFailed,
								&error.to_string(),
							)
							.await;
							None
						} else {
							query
						}
					},
					Ok(_) => {
						send_error(
							responses,
							frame.request_id,
							pb::ProtocolErrorCode::PreconditionFailed,
							"ArgText cannot follow ArgsCommitted",
						)
						.await;
						None
					},
					Err((code, message)) => {
						send_error(responses, frame.request_id, code, message).await;
						None
					},
				};
				if let Some(query) = query {
					send_body(responses, frame.request_id, server_frame::Body::AdmitInvocation(query))
						.await;
				}
			},
			client_frame::Body::Admission(admission) => {
				let result = connection.invocation_mut(frame.request_id, &admission.invocation_id);
				let pending = match result {
					Ok(InvocationState::Native { admission: gate, pending_commit, .. })
					| Ok(InvocationState::Worker { admission: gate, pending_commit, .. }) => {
						if let Err(error) = gate.answer(admission) {
							send_error(
								responses,
								frame.request_id,
								pb::ProtocolErrorCode::PreconditionFailed,
								&error.to_string(),
							)
							.await;
							None
						} else {
							pending_commit.take()
						}
					},
					Err((code, message)) => {
						send_error(responses, frame.request_id, code, message).await;
						None
					},
				};
				if let Some(request) = pending {
					self
						.commit_invocation(frame.request_id, request, responses, finished, connection)
						.await;
				}
			},
			client_frame::Body::ArgsCommitted(request) => {
				let query = match connection.invocation_mut(frame.request_id, &request.invocation_id) {
					Ok(InvocationState::Native { admission, .. })
					| Ok(InvocationState::Worker { admission, .. }) => {
						match admission.finalize(
							&request.raw,
							self.workspace.root(),
							self.workspace.root(),
						) {
							Ok(query) => query,
							Err(error) => {
								send_error(
									responses,
									frame.request_id,
									pb::ProtocolErrorCode::InvalidArgument,
									&error.to_string(),
								)
								.await;
								return;
							},
						}
					},
					Err((code, message)) => {
						send_error(responses, frame.request_id, code, message).await;
						return;
					},
				};
				if let Some(query) = query {
					send_body(responses, frame.request_id, server_frame::Body::AdmitInvocation(query))
						.await;
				}
				self
					.commit_invocation(frame.request_id, request, responses, finished, connection)
					.await;
			},
			client_frame::Body::Interrupt(request) => {
				connection
					.interrupt(frame.request_id, request, responses, finished)
					.await;
			},
			client_frame::Body::OpenSession(request) => {
				if reject_duplicate_open(connection, frame.request_id, responses).await {
					return;
				}
				if let Err(error) = connection.quotas.reserve_exec() {
					send_policy_error(responses, frame.request_id, error).await;
					return;
				}
				match self.exec.open_session(request).await {
					Ok(response) => {
						send_body(
							responses,
							frame.request_id,
							server_frame::Body::SessionOpened(response),
						)
						.await;
					},
					Err(error) => {
						connection.quotas.release_exec();
						send_exec_error(responses, frame.request_id, &error).await;
					},
				}
			},
			client_frame::Body::CloseSession(request) => {
				match self.exec.close_session(&request.session) {
					Ok(response) => {
						connection.quotas.release_exec();
						send_body(
							responses,
							frame.request_id,
							server_frame::Body::SessionClosed(response),
						)
						.await;
					},
					Err(error) => send_exec_error(responses, frame.request_id, &error).await,
				}
			},
			client_frame::Body::Exec(request) => {
				if reject_duplicate_open(connection, frame.request_id, responses).await {
					return;
				}
				if let Err(error) = connection.quotas.reserve_exec() {
					send_policy_error(responses, frame.request_id, error).await;
					return;
				}
				match self.exec.exec(request, None).await {
					Ok((started, run)) => {
						let exec = Bytes::copy_from_slice(run.id());
						let cancel = CancellationToken::new();
						connection
							.requests
							.insert(frame.request_id, RequestState::Exec {
								exec:   exec.clone(),
								cancel: cancel.clone(),
							});
						send_body(responses, frame.request_id, server_frame::Body::ExecStarted(started))
							.await;
						spawn_exec(frame.request_id, run, cancel, responses.clone(), finished.clone());
					},
					Err(error) => {
						connection.quotas.release_exec();
						send_exec_error(responses, frame.request_id, &error).await;
					},
				}
			},
			client_frame::Body::Stdin(request) => {
				if let Some(exec) = connection
					.exec_id(frame.request_id, &request.exec, responses)
					.await
				{
					let data = match request.input {
						Some(pb::stdin_frame::Input::Data(data)) => Some(data),
						Some(pb::stdin_frame::Input::Eof(true)) => None,
						_ => {
							send_error(
								responses,
								frame.request_id,
								pb::ProtocolErrorCode::InvalidArgument,
								"stdin frame has no data or eof marker",
							)
							.await;
							return;
						},
					};
					if let Err(error) = self.exec.stdin(&exec, data.as_deref()) {
						send_exec_error(responses, frame.request_id, &error).await;
					}
				}
			},
			client_frame::Body::Signal(request) => {
				if let Some(exec) = connection
					.exec_id(frame.request_id, &request.exec, responses)
					.await && let Err(error) = self.exec.signal(&exec, &request.signal)
				{
					send_exec_error(responses, frame.request_id, &error).await;
				}
			},
			client_frame::Body::Resize(request) => {
				if let Some(exec) = connection
					.exec_id(frame.request_id, &request.exec, responses)
					.await && let Err(error) = self.exec.resize(&exec, request.rows, request.columns)
				{
					send_exec_error(responses, frame.request_id, &error).await;
				}
			},
			client_frame::Body::StartProcess(request) => {
				if let Err(error) = connection.quotas.charge_process_start() {
					send_policy_error(responses, frame.request_id, error).await;
					return;
				}
				match self.exec.start_process(request).await {
					Ok(response) => {
						send_body(
							responses,
							frame.request_id,
							server_frame::Body::ProcessStarted(response),
						)
						.await;
					},
					Err(error) => send_exec_error(responses, frame.request_id, &error).await,
				}
			},
			client_frame::Body::ListProcesses(_) => {
				send_body(
					responses,
					frame.request_id,
					server_frame::Body::ProcessList(self.exec.list_processes()),
				)
				.await;
			},
			client_frame::Body::AttachOutput(request) => {
				if reject_duplicate_open(connection, frame.request_id, responses).await {
					return;
				}
				if let Err(error) = connection.quotas.reserve_stream() {
					send_policy_error(responses, frame.request_id, error).await;
					return;
				}
				match self.exec.attach_output(&request) {
					Ok(attachment) => {
						let cancel = CancellationToken::new();
						let process_name = Str::from(request.name);
						connection
							.requests
							.insert(frame.request_id, RequestState::ProcessAttach {
								cancel: cancel.clone(),
							});
						send_body(
							responses,
							frame.request_id,
							server_frame::Body::OutputAttached(attachment.attached),
						)
						.await;
						for output in attachment.backlog {
							send_body(
								responses,
								frame.request_id,
								server_frame::Body::ProcessOutput(output),
							)
							.await;
						}
						send_body(
							responses,
							frame.request_id,
							server_frame::Body::ProcessState(pb::ProcessStateEvent {
								process: Some(attachment.state),
								props:   Default::default(),
							}),
						)
						.await;
						spawn_process_attachment(
							frame.request_id,
							process_name,
							attachment.events,
							cancel,
							responses.clone(),
							finished.clone(),
						);
					},
					Err(error) => {
						connection.quotas.release_stream();
						send_exec_error(responses, frame.request_id, &error).await;
					},
				}
			},
			client_frame::Body::SendInput(request) => {
				let data = match request.input {
					Some(pb::send_input::Input::Data(data)) => Some(data),
					Some(pb::send_input::Input::Eof(true)) => None,
					_ => {
						send_error(
							responses,
							frame.request_id,
							pb::ProtocolErrorCode::InvalidArgument,
							"process input has no data or eof marker",
						)
						.await;
						return;
					},
				};
				match self.exec.send_process_input(&request.name, data.as_deref()) {
					Ok(response) => {
						send_body(
							responses,
							frame.request_id,
							server_frame::Body::ProcessCommandAccepted(response),
						)
						.await;
					},
					Err(error) => send_exec_error(responses, frame.request_id, &error).await,
				}
			},
			client_frame::Body::SignalProcess(request) => {
				match self.exec.signal_process(&request.name, &request.signal) {
					Ok(response) => {
						send_body(
							responses,
							frame.request_id,
							server_frame::Body::ProcessCommandAccepted(response),
						)
						.await;
					},
					Err(error) => send_exec_error(responses, frame.request_id, &error).await,
				}
			},
			client_frame::Body::StopProcess(request) => {
				match self
					.exec
					.stop_process(&request.name, Duration::from_millis(request.grace_ms))
				{
					Ok(response) => {
						send_body(
							responses,
							frame.request_id,
							server_frame::Body::ProcessCommandAccepted(response),
						)
						.await;
					},
					Err(error) => send_exec_error(responses, frame.request_id, &error).await,
				}
			},
			client_frame::Body::BlobStat(request) => match self.blobs.stat(&request.hash) {
				Ok(response) => {
					send_body(responses, frame.request_id, server_frame::Body::BlobStat(response)).await;
				},
				Err(error) => send_blob_error(responses, frame.request_id, &error).await,
			},
			client_frame::Body::Data(request) => {
				self
					.dispatch_data(
						frame.request_id,
						request,
						scope.as_ref(),
						responses,
						finished,
						connection,
					)
					.await;
			},
			client_frame::Body::BlobGet(request) => {
				if reject_duplicate_open(connection, frame.request_id, responses).await {
					return;
				}
				if let Err(error) = connection.quotas.reserve_stream() {
					send_policy_error(responses, frame.request_id, error).await;
					return;
				}
				match self.blobs.get_request(&request) {
					Ok(read) => {
						let cancel = CancellationToken::new();
						connection
							.requests
							.insert(frame.request_id, RequestState::BlobGet { cancel: cancel.clone() });
						spawn_blob_get(
							frame.request_id,
							read,
							cancel,
							responses.clone(),
							finished.clone(),
						);
					},
					Err(error) => {
						connection.quotas.release_stream();
						send_blob_error(responses, frame.request_id, &error).await;
					},
				}
			},
			client_frame::Body::BlobPutChunk(chunk) => {
				self
					.put_chunk(frame.request_id, chunk, responses, connection)
					.await;
			},
			client_frame::Body::BlobPutCommit(_) => {
				self
					.commit_blob(frame.request_id, responses, connection)
					.await;
			},
			client_frame::Body::BlobDelete(request) => match self.blobs.delete(&request.hash) {
				Ok(response) => {
					send_body(responses, frame.request_id, server_frame::Body::BlobDeleted(response))
						.await;
				},
				Err(error) => send_blob_error(responses, frame.request_id, &error).await,
			},
			client_frame::Body::Cancel(_) => unreachable!("cancel handled before ordinary dispatch"),
		}
	}

	async fn dispatch_data(
		&self,
		request_id: u64,
		request: pb::DataRequest,
		scope: Option<&pb::InvocationScope>,
		responses: &flume::Sender<pb::ServerFrame>,
		finished: &flume::Sender<Finished>,
		connection: &mut ConnectionState,
	) {
		use pb::data_request::Body;

		match request.body {
			Some(Body::Worker(request)) => {
				if !authorize_data_operation(
					connection,
					scope,
					worker_operation(&request),
					"env.worker",
					responses,
					request_id,
				)
				.await
				{
					return;
				}
				self.dispatch_worker(request_id, request, responses).await;
			},
			Some(Body::Document(request)) => {
				self
					.dispatch_document(request_id, request, scope, responses, finished, connection)
					.await;
			},
			Some(Body::Walk(request)) => {
				if !authorize_data_operation(
					connection,
					scope,
					"omp.env.find.walk",
					"env.search",
					responses,
					request_id,
				)
				.await
				{
					return;
				}
				let walk = match workspace_walk_request(&self.workspace, &request) {
					Ok(walk) => walk,
					Err((code, message)) => {
						send_error(responses, request_id, code, &message).await;
						return;
					},
				};
				if let Err(error) = connection.quotas.reserve_stream() {
					send_policy_error(responses, request_id, error).await;
					return;
				}
				let cancel = CancellationToken::new();
				connection
					.requests
					.insert(request_id, RequestState::DataStream { cancel: cancel.clone() });
				spawn_workspace_walk(
					request_id,
					self.workspace.clone(),
					walk,
					cancel,
					responses.clone(),
					finished.clone(),
				);
			},
			Some(Body::Search(request)) => {
				if !authorize_data_operation(
					connection,
					scope,
					"omp.env.find.search",
					"env.search",
					responses,
					request_id,
				)
				.await
				{
					return;
				}
				let Some(wire_walk) = request.walk.as_ref() else {
					send_error(
						responses,
						request_id,
						pb::ProtocolErrorCode::InvalidArgument,
						"search walk request is missing",
					)
					.await;
					return;
				};
				let walk = match workspace_walk_request(&self.workspace, wire_walk) {
					Ok(walk) => walk,
					Err((code, message)) => {
						send_error(responses, request_id, code, &message).await;
						return;
					},
				};
				let pattern = match std::str::from_utf8(&request.pattern) {
					Ok(pattern) if !pattern.is_empty() => Str::from(pattern),
					_ => {
						send_error(
							responses,
							request_id,
							pb::ProtocolErrorCode::InvalidArgument,
							"search pattern must be nonempty UTF-8",
						)
						.await;
						return;
					},
				};
				let options = WorkspaceSearchOwned {
					pattern,
					case: if request.case_sensitive {
						WorkspaceSearchCase::Sensitive
					} else {
						WorkspaceSearchCase::Insensitive
					},
					limit: request.limit,
				};
				if let Err(error) = connection.quotas.reserve_stream() {
					send_policy_error(responses, request_id, error).await;
					return;
				}
				let cancel = CancellationToken::new();
				connection
					.requests
					.insert(request_id, RequestState::DataStream { cancel: cancel.clone() });
				spawn_workspace_search(
					request_id,
					self.workspace.clone(),
					walk,
					options,
					cancel,
					responses.clone(),
					finished.clone(),
				);
			},
			Some(Body::Workspace(request)) => {
				self
					.dispatch_workspace(request_id, request, scope, responses, connection)
					.await;
			},
			Some(Body::Worktree(request)) => {
				self
					.dispatch_worktree(request_id, request, scope, responses, connection)
					.await;
			},
			Some(Body::Site(request)) => {
				if !authorize_data_operation(
					connection,
					scope,
					"omp.env.site.materialize",
					"env.site",
					responses,
					request_id,
				)
				.await
				{
					return;
				}
				if connection.host.is_some() {
					send_error(
						responses,
						request_id,
						pb::ProtocolErrorCode::PermissionDenied,
						"site trees and their store are installer-owned and read-only to extensions",
					)
					.await;
					return;
				}
				let module_paths = record_modules(&request.files);
				match self.sites.materialize(request) {
					Ok(materialized) => {
						for module in module_paths {
							if let Err(error) = self.require_record_owner(
								&materialized.site_key,
								module,
								&materialized.site_key,
							) {
								send_error(
									responses,
									request_id,
									pb::ProtocolErrorCode::PermissionDenied,
									&error.to_string(),
								)
								.await;
								return;
							}
						}
						send_data_response(
							responses,
							request_id,
							pb::data_response::Body::Site(materialized),
						)
						.await;
					},
					Err(
						error @ (SiteError::InvalidSiteKey
						| SiteError::InvalidFilePath(_)
						| SiteError::InvalidBlobHash),
					) => {
						send_error(
							responses,
							request_id,
							pb::ProtocolErrorCode::InvalidArgument,
							&error.to_string(),
						)
						.await;
					},
					Err(error @ SiteError::TrustedLoad(_)) => {
						send_error(
							responses,
							request_id,
							pb::ProtocolErrorCode::PermissionDenied,
							&error.to_string(),
						)
						.await;
					},
					Err(error) => {
						send_error(
							responses,
							request_id,
							pb::ProtocolErrorCode::Internal,
							&error.to_string(),
						)
						.await;
					},
				}
			},
			Some(Body::DetachExec(request)) => {
				if !authorize_data_operation(
					connection,
					scope,
					"omp.env.sh.detach",
					"env.exec",
					responses,
					request_id,
				)
				.await
				{
					return;
				}
				match self.exec.detach_exec(&request.exec, &request.name) {
					Ok(response) => {
						send_data_response(
							responses,
							request_id,
							pb::data_response::Body::DetachedExec(response),
						)
						.await;
					},
					Err(error) => send_exec_error(responses, request_id, &error).await,
				}
			},
			None => {
				send_error(
					responses,
					request_id,
					pb::ProtocolErrorCode::InvalidArgument,
					"DATA request body is missing",
				)
				.await;
			},
		}
	}

	async fn dispatch_worker(
		&self,
		request_id: u64,
		request: pb::WorkerOp,
		responses: &flume::Sender<pb::ServerFrame>,
	) {
		use pb::{worker_op::Op, worker_result::Result as WorkerResult};

		let result = match request.op {
			Some(Op::Open(open)) => {
				let key = WorkerKey {
					extension: Str::new_static("env"),
					name:      Str::from(open.name.as_str()),
					site:      Str::new_static("env"),
				};
				match self.workers.open(key) {
					Ok((route, lease)) => {
						lease.relinquish();
						Ok(WorkerResult::Opened(pb::WorkerOpened {
							name: route.key.name.to_string(),
							generation: route.generation,
							..pb::WorkerOpened::default()
						}))
					},
					Err(WorkerUnavailable::LayerCeiling | WorkerUnavailable::SpawnCeiling) => {
						Err((pb::ProtocolErrorCode::ResourceExhausted, "WorkerUnavailable"))
					},
					Err(WorkerUnavailable::StaleGeneration) => {
						Err((pb::ProtocolErrorCode::InvalidArgument, "stale worker generation"))
					},
				}
			},
			Some(Op::Close(close)) => match self.workers.close(&close.name, close.generation) {
				true => Ok(WorkerResult::Closed(pb::ProcessCommandAccepted::default())),
				false => Err((pb::ProtocolErrorCode::InvalidArgument, "stale worker generation")),
			},
			Some(Op::Data(data)) => match self.workers.demux(data) {
				Ok(accepted) => Ok(WorkerResult::Data(pb::WorkerData {
					name: accepted.route.key.name.to_string(),
					generation: accepted.route.generation,
					channel: accepted.channel,
					data: Bytes::copy_from_slice(&accepted.data),
					..pb::WorkerData::default()
				})),
				Err(WorkerUnavailable::StaleGeneration) => {
					Err((pb::ProtocolErrorCode::InvalidArgument, "stale worker generation"))
				},
				Err(WorkerUnavailable::LayerCeiling | WorkerUnavailable::SpawnCeiling) => {
					Err((pb::ProtocolErrorCode::ResourceExhausted, "WorkerUnavailable"))
				},
			},
			Some(Op::Info(info)) => match self.workers.route(&info.name) {
				Some(route) => Ok(WorkerResult::Info(worker_info(&route))),
				None => Err((pb::ProtocolErrorCode::InvalidArgument, "unknown worker")),
			},
			Some(Op::List(_)) => Ok(WorkerResult::List(pb::WorkerList {
				workers: self.workers.routes().iter().map(worker_info).collect(),
				..pb::WorkerList::default()
			})),
			None => Err((pb::ProtocolErrorCode::InvalidArgument, "worker operation is missing")),
		};
		match result {
			Ok(result) => {
				send_body(
					responses,
					request_id,
					server_frame::Body::Data(pb::DataResponse {
						body: Some(pb::data_response::Body::Worker(pb::WorkerResult {
							result: Some(result),
							..pb::WorkerResult::default()
						})),
						..pb::DataResponse::default()
					}),
				)
				.await
			},
			Err((code, message)) => send_error(responses, request_id, code, message).await,
		}
	}

	async fn dispatch_workspace(
		&self,
		request_id: u64,
		request: pb::WorkspaceOp,
		scope: Option<&pb::InvocationScope>,
		responses: &flume::Sender<pb::ServerFrame>,
		connection: &mut ConnectionState,
	) {
		use pb::{workspace_op::Op, workspace_result};

		let Some(operation) = request.op else {
			send_error(
				responses,
				request_id,
				pb::ProtocolErrorCode::InvalidArgument,
				"workspace operation is missing",
			)
			.await;
			return;
		};
		let operation_name = match &operation {
			Op::Snapshot(_) => "omp.env.workspace.snapshot",
			Op::Restore(_) => "omp.env.workspace.restore",
		};
		if !authorize_data_operation(
			connection,
			scope,
			operation_name,
			"env.workspace.snapshot",
			responses,
			request_id,
		)
		.await
		{
			return;
		}
		let cancel = CancellationToken::new();
		let result = match operation {
			Op::Snapshot(request) => self
				.workspace_ops
				.snapshot(&request, &cancel)
				.map(workspace_result::Result::Snapshot),
			Op::Restore(request) => self
				.workspace_ops
				.restore(&request, &cancel)
				.await
				.map(workspace_result::Result::Restored),
		};
		match result {
			Ok(result) => {
				send_data_response(
					responses,
					request_id,
					pb::data_response::Body::Workspace(pb::WorkspaceResult {
						result: Some(result),
						props:  Default::default(),
					}),
				)
				.await;
			},
			Err(error) => send_workspace_operation_error(responses, request_id, &error).await,
		}
	}

	async fn dispatch_worktree(
		&self,
		request_id: u64,
		request: pb::WorktreeOp,
		scope: Option<&pb::InvocationScope>,
		responses: &flume::Sender<pb::ServerFrame>,
		connection: &mut ConnectionState,
	) {
		use pb::worktree_op::Op;

		let Some(operation) = request.op else {
			send_error(
				responses,
				request_id,
				pb::ProtocolErrorCode::InvalidArgument,
				"worktree operation is missing",
			)
			.await;
			return;
		};
		let operation_name = match &operation {
			Op::Create(_) => "omp.env.worktree.create",
			Op::Destroy(_) => "omp.env.worktree.destroy",
			Op::Merge(_) => "omp.env.worktree.merge",
		};
		if !authorize_data_operation(
			connection,
			scope,
			operation_name,
			"env.worktree",
			responses,
			request_id,
		)
		.await
		{
			return;
		}
		if matches!(
			&operation,
			Op::Merge(request) if matches!(request.strategy.as_str(), "patch" | "branch")
		) {
			send_unsupported_data(
				responses,
				request_id,
				"worktree patch/branch merge results without a lossless wire arm",
			)
			.await;
			return;
		}
		let cancel = CancellationToken::new();
		let result = match operation {
			Op::Create(request) => {
				self
					.workspace_ops
					.create_worktree(&request, &cancel)
					.map(|worktree| pb::WorktreeResult {
						worktree:  Some(worktree),
						conflicts: Vec::new(),
						props:     Default::default(),
					})
			},
			Op::Destroy(request) => {
				self
					.workspace_ops
					.destroy_worktree(&request, &cancel)
					.map(|worktree| pb::WorktreeResult {
						worktree:  Some(worktree),
						conflicts: Vec::new(),
						props:     Default::default(),
					})
			},
			Op::Merge(request) => self
				.workspace_ops
				.merge_worktree(&request, &cancel)
				.map(|merge| pb::WorktreeResult {
					worktree:  Some(merge.worktree),
					conflicts: merge.conflicts,
					props:     Default::default(),
				}),
		};
		match result {
			Ok(result) => {
				send_data_response(responses, request_id, pb::data_response::Body::Worktree(result))
					.await;
			},
			Err(error) => send_workspace_operation_error(responses, request_id, &error).await,
		}
	}

	async fn dispatch_document(
		&self,
		request_id: u64,
		request: pb::DocumentOp,
		scope: Option<&pb::InvocationScope>,
		responses: &flume::Sender<pb::ServerFrame>,
		finished: &flume::Sender<Finished>,
		connection: &mut ConnectionState,
	) {
		use pb::{document_op::Op, document_result};

		let Some(operation) = request.op else {
			send_error(
				responses,
				request_id,
				pb::ProtocolErrorCode::InvalidArgument,
				"document operation is missing",
			)
			.await;
			return;
		};
		let (operation_name, required) = match &operation {
			Op::Open(_) => ("omp.env.docs.open", "env.doc.read"),
			Op::Close(_) => ("omp.env.docs.close", "env.doc.read"),
			Op::Read(_) => ("omp.env.docs.read", "env.doc.read"),
			Op::Summarize(_) => ("omp.env.docs.summarize", "env.doc.read"),
			Op::CommitTransaction(_) => ("omp.env.docs.commit_transaction", "env.doc.write"),
			Op::Canonicalize(_) => ("omp.env.fs.canonicalize", "env.fs.read"),
			Op::Stat(_) => ("omp.env.fs.stat", "env.fs.read"),
			Op::ListDirectory(_) => ("omp.env.fs.list_directory", "env.fs.read"),
			Op::CreateDirectory(_) => ("omp.env.fs.create_directory", "env.fs.write"),
			Op::Remove(_) => ("omp.env.fs.remove", "env.fs.write"),
			Op::Rename(_) => ("omp.env.fs.rename", "env.fs.write"),
			Op::Copy(_) => ("omp.env.fs.copy", "env.fs.write"),
			Op::ReadLink(_) => ("omp.env.fs.read_link", "env.fs.read"),
			Op::CreateSymlink(_) => ("omp.env.fs.create_symlink", "env.fs.write"),
			Op::CreateHardLink(_) => ("omp.env.fs.create_hard_link", "env.fs.write"),
			Op::SetPermissions(_) => ("omp.env.fs.set_permissions", "env.fs.write"),
			Op::GetLspBindings(_) => ("omp.env.lsp.get_bindings", "env.lsp"),
			Op::LspRequest(_) => ("omp.env.lsp.request", "env.lsp"),
			Op::LspNotification(_) => ("omp.env.lsp.notification", "env.lsp"),
		};
		if !authorize_data_operation(
			connection,
			scope,
			operation_name,
			required,
			responses,
			request_id,
		)
		.await
		{
			return;
		}

		let cancel = CancellationToken::new();
		let mut opened_events: Option<(DocumentEvents, CancellationToken)> = None;
		let mut lsp_events: Option<(LspEvents, CancellationToken)> = None;
		let result = match operation {
			Op::Open(request) => {
				if let Err(error) = connection.quotas.reserve_document_lease() {
					send_policy_error(responses, request_id, error).await;
					return;
				}
				match self.documents.open_request(request, &cancel).await {
					Ok((mut lease, response)) => {
						if let Some(events) = lease.take_events() {
							if let Err(error) = connection.quotas.reserve_stream() {
								connection.quotas.release_document_lease();
								send_policy_error(responses, request_id, error).await;
								return;
							}
							let stream_cancel = CancellationToken::new();
							connection
								.requests
								.insert(request_id, RequestState::DocumentEvents {
									lease_id: lease.id().clone(),
									cancel:   stream_cancel.clone(),
								});
							opened_events = Some((events, stream_cancel));
						}
						self
							.authority
							.register_lease(lease.id().clone(), connection.connection_owner);
						connection.document_leases.insert(lease.id().clone(), lease);
						Ok(document_result::Result::Opened(response))
					},
					Err(error) => {
						connection.quotas.release_document_lease();
						Err(error)
					},
				}
			},
			Op::Close(request) => {
				if let Err(error) = self
					.authority
					.check_lease(&request.lease_id, connection.connection_owner)
				{
					send_policy_error(responses, request_id, error).await;
					return;
				}
				let lease_id = request.lease_id.clone();
				let Some(mut lease) = connection.document_leases.remove(&request.lease_id) else {
					send_error(
						responses,
						request_id,
						pb::ProtocolErrorCode::NotFound,
						"document lease is not owned by this connection",
					)
					.await;
					return;
				};
				match self
					.documents
					.close_request(&mut lease, request, &cancel)
					.await
				{
					Ok(response) => {
						let stream_request = connection.requests.iter().find_map(|(request, state)| {
							matches!(
								state,
								RequestState::DocumentEvents { lease_id: owned, .. }
									if owned == &lease_id
							)
							.then_some(*request)
						});
						if let Some(stream_request) = stream_request
							&& let Some(RequestState::DocumentEvents { cancel, .. }) =
								connection.requests.remove(&stream_request)
						{
							cancel.cancel();
							connection.quotas.release_stream();
						}
						self
							.authority
							.release_lease(&lease_id, connection.connection_owner);
						connection.quotas.release_document_lease();
						Ok(document_result::Result::Closed(response))
					},
					Err(error) => {
						connection.document_leases.insert(lease.id().clone(), lease);
						Err(error)
					},
				}
			},
			Op::Read(request) => {
				if let Some(lease_id) = connection_lease_id(request.document.as_ref())
					&& let Err(error) = self
						.authority
						.check_lease(lease_id, connection.connection_owner)
				{
					send_policy_error(responses, request_id, error).await;
					return;
				}
				let Some(lease) = connection_lease(connection, request.document.as_ref()) else {
					send_error(
						responses,
						request_id,
						pb::ProtocolErrorCode::NotFound,
						"document lease is not owned by this connection",
					)
					.await;
					return;
				};
				self
					.documents
					.read_request(lease, request, &cancel)
					.await
					.map(document_result::Result::Read)
			},
			Op::Summarize(request) => {
				if let Some(lease_id) = connection_lease_id(request.document.as_ref())
					&& let Err(error) = self
						.authority
						.check_lease(lease_id, connection.connection_owner)
				{
					send_policy_error(responses, request_id, error).await;
					return;
				}
				let Some(lease) = connection_lease(connection, request.document.as_ref()) else {
					send_error(
						responses,
						request_id,
						pb::ProtocolErrorCode::NotFound,
						"document lease is not owned by this connection",
					)
					.await;
					return;
				};
				self
					.documents
					.summarize_request(lease, request, &cancel)
					.await
					.map(document_result::Result::Summarized)
			},
			Op::CommitTransaction(request) => {
				let lease_ids: Vec<Bytes> = request
					.operations
					.iter()
					.filter_map(|operation| connection_lease_id(operation.document.as_ref()).cloned())
					.collect();
				if let Some(error) = lease_ids.iter().find_map(|lease_id| {
					self
						.authority
						.check_lease(lease_id, connection.connection_owner)
						.err()
				}) {
					send_policy_error(responses, request_id, error).await;
					return;
				}
				if lease_ids.len() != request.operations.len()
					|| lease_ids
						.iter()
						.any(|lease_id| !connection.document_leases.contains_key(lease_id))
				{
					send_error(
						responses,
						request_id,
						pb::ProtocolErrorCode::NotFound,
						"transaction contains a document lease not owned by this connection",
					)
					.await;
					return;
				}
				match self
					.documents
					.commit_transaction_request(request, &cancel)
					.await
				{
					Ok(response) => {
						if let Some(document_pb::commit_transaction_response::Outcome::Committed(
							committed,
						)) = &response.outcome
						{
							for operation in &committed.operations {
								let Some(lease_id) = lease_ids.get(operation.operation_index as usize)
								else {
									continue;
								};
								if let (Some(lease), Some(head)) =
									(connection.document_leases.get_mut(lease_id), operation.head.clone())
									&& let Err(error) = lease.advance(head)
								{
									return send_document_error(responses, request_id, &error).await;
								}
							}
						}
						Ok(document_result::Result::Transaction(response))
					},
					Err(error) => Err(error),
				}
			},
			Op::Canonicalize(request) => self
				.documents
				.canonicalize(request, &cancel)
				.await
				.map(document_result::Result::Canonicalized),
			Op::Stat(request) => self
				.documents
				.stat(request, &cancel)
				.await
				.map(document_result::Result::Stat),
			Op::ListDirectory(request) => self
				.documents
				.list_directory(request, &cancel)
				.await
				.map(document_result::Result::Directory),
			Op::CreateDirectory(request) => self
				.documents
				.create_directory(request, &cancel)
				.await
				.map(document_result::Result::DirectoryCreated),
			Op::Remove(request) => self
				.documents
				.remove(request, &cancel)
				.await
				.map(document_result::Result::Removed),
			Op::Rename(request) => self
				.documents
				.rename(request, &cancel)
				.await
				.map(document_result::Result::Renamed),
			Op::Copy(request) => self
				.documents
				.copy(request, &cancel)
				.await
				.map(document_result::Result::Copied),
			Op::ReadLink(request) => self
				.documents
				.read_link(request, &cancel)
				.await
				.map(document_result::Result::Link),
			Op::CreateSymlink(request) => self
				.documents
				.create_symlink(request, &cancel)
				.await
				.map(document_result::Result::SymlinkCreated),
			Op::CreateHardLink(request) => self
				.documents
				.create_hard_link(request, &cancel)
				.await
				.map(document_result::Result::HardLinkCreated),
			Op::SetPermissions(request) => self
				.documents
				.set_permissions(request, &cancel)
				.await
				.map(document_result::Result::PermissionsSet),
			Op::GetLspBindings(request) => {
				match self.documents.get_lsp_bindings(request, &cancel).await {
					Ok(response) => {
						if let Some(events) = self.documents.take_lsp_events() {
							if let Err(error) = connection.quotas.reserve_stream() {
								send_policy_error(responses, request_id, error).await;
								return;
							}
							let stream_cancel = CancellationToken::new();
							connection
								.requests
								.insert(request_id, RequestState::LspEvents {
									cancel: stream_cancel.clone(),
								});
							lsp_events = Some((events, stream_cancel));
						}
						Ok(document_result::Result::LspBindings(response))
					},
					Err(error) => Err(error),
				}
			},
			Op::LspRequest(request) => self
				.documents
				.lsp_request(request, &cancel)
				.await
				.map(document_result::Result::LspResponse),
			Op::LspNotification(request) => self
				.documents
				.lsp_notification(request, &cancel)
				.await
				.map(document_result::Result::LspNotified),
		};
		match result {
			Ok(result) => {
				send_data_response(
					responses,
					request_id,
					pb::data_response::Body::Document(pb::DocumentResult {
						result: Some(result),
						props:  Default::default(),
					}),
				)
				.await;
				if let Some((events, cancel)) = opened_events {
					spawn_document_events(
						request_id,
						events,
						cancel,
						responses.clone(),
						finished.clone(),
					);
				}
				if let Some((events, cancel)) = lsp_events {
					spawn_lsp_events(request_id, events, cancel, responses.clone(), finished.clone());
				}
			},
			Err(error) => send_document_error(responses, request_id, &error).await,
		}
	}

	async fn open_invocation(
		&self,
		request_id: u64,
		request: pb::InvokeTool,
		responses: &flume::Sender<pb::ServerFrame>,
		finished: &flume::Sender<Finished>,
		connection: &mut ConnectionState,
	) {
		if reject_duplicate_open(connection, request_id, responses).await {
			return;
		}
		if !connection.ambient && request.name == "eval" {
			send_error(
				responses,
				request_id,
				pb::ProtocolErrorCode::PermissionDenied,
				"eval is available only through the session-local environment",
			)
			.await;
			return;
		}
		let invocation_id = Str::from(request.invocation_id.as_str());
		if invocation_id.is_empty() {
			send_error(
				responses,
				request_id,
				pb::ProtocolErrorCode::InvalidArgument,
				"invocation_id must not be empty",
			)
			.await;
			return;
		}
		if connection.invocation_ids.contains_key(&invocation_id) {
			send_error(
				responses,
				request_id,
				pb::ProtocolErrorCode::AlreadyExists,
				"invocation_id is already open on this connection",
			)
			.await;
			return;
		}
		let registry = self.registry();
		let Some((_, revision)) = registry.live_identity(&request.name) else {
			send_error(
				responses,
				request_id,
				pb::ProtocolErrorCode::NotFound,
				"tool name and revision are not registered",
			)
			.await;
			return;
		};
		if revision.to_string() != request.rev {
			send_error(
				responses,
				request_id,
				pb::ProtocolErrorCode::PreconditionFailed,
				"requested tool revision is not live",
			)
			.await;
			return;
		}
		let route = registry
			.route(&request.name)
			.expect("a live registry identity always has an execution route");
		let deadline = if request.deadline_ms == 0 {
			DEFAULT_TOOL_DEADLINE
		} else {
			Duration::from_millis(request.deadline_ms)
		};
		let maximum_effects = registry
			.effects(&request.name)
			.expect("a routed tool has a declared effect envelope")
			.clone();
		let cancel = CancellationToken::new();
		if route == ToolRoute::Native {
			let (feed, params) = IncomingParams::owned_channel(connection.owner.clone());
			let lifecycle = Arc::new(NativeLifecycle::default());
			let name = Str::from(request.name);
			let admission = AdmissionGate::new(invocation_id.clone(), name.clone(), deadline);
			connection.requests.insert(
				request_id,
				RequestState::Invocation(InvocationState::Native {
					id: invocation_id.clone(),
					feed: feed.clone(),
					lifecycle: Arc::clone(&lifecycle),
					admission,
					pending_commit: None,
					maximum_effects: maximum_effects.clone(),
					cancel: cancel.clone(),
				}),
			);
			connection
				.invocation_ids
				.insert(invocation_id.clone(), request_id);
			send_body(
				responses,
				request_id,
				server_frame::Body::InvocationAccepted(pb::InvokeAccepted {
					invocation_id: invocation_id.to_string(),
					props:         Default::default(),
				}),
			)
			.await;
			spawn_native_invocation(
				request_id,
				invocation_id,
				name,
				feed,
				deadline,
				params,
				Arc::clone(&registry),
				lifecycle,
				cancel,
				responses.clone(),
				finished.clone(),
			)
			.await;
		} else if matches!(route, ToolRoute::Worker { .. }) {
			let Some(owner) = self.worker_owner(&request.name, &request.rev) else {
				send_error(
					responses,
					request_id,
					pb::ProtocolErrorCode::NotFound,
					"tool name and revision are not registered to an extension host",
				)
				.await;
				return;
			};
			let name = Str::from(request.name);
			let invocation = match self.ext_hosts.open(OpenToolCall {
				invocation_id: invocation_id.clone(),
				name: name.clone(),
				rev: Str::from(request.rev),
				deadline,
			}) {
				Ok(invocation) => invocation,
				Err(error) => {
					send_error(
						responses,
						request_id,
						pb::ProtocolErrorCode::Internal,
						&error.to_string(),
					)
					.await;
					return;
				},
			};
			let (interrupt, interrupts) = flume::unbounded();
			connection.requests.insert(
				request_id,
				RequestState::Invocation(InvocationState::Worker {
					id: invocation_id.clone(),
					owner,
					invocation: Some(invocation),
					committed: false,
					admission: AdmissionGate::new(invocation_id.clone(), name, deadline),
					pending_commit: None,
					maximum_effects,
					interrupt,
					interrupts: Some(interrupts),
					cancel,
				}),
			);
			connection
				.invocation_ids
				.insert(invocation_id.clone(), request_id);
			send_body(
				responses,
				request_id,
				server_frame::Body::InvocationAccepted(pb::InvokeAccepted {
					invocation_id: invocation_id.to_string(),
					props:         Default::default(),
				}),
			)
			.await;
		} else {
			send_error(
				responses,
				request_id,
				pb::ProtocolErrorCode::NotFound,
				"tool name and revision are not registered",
			)
			.await;
		}
	}

	async fn commit_invocation(
		&self,
		request_id: u64,
		mut request: pb::ArgsCommitted,
		responses: &flume::Sender<pb::ServerFrame>,
		finished: &flume::Sender<Finished>,
		connection: &mut ConnectionState,
	) {
		let already_committed = match connection.invocation_mut(request_id, &request.invocation_id) {
			Ok(InvocationState::Native { lifecycle, .. }) => lifecycle.is_committed(),
			Ok(InvocationState::Worker { committed, .. }) => *committed,
			Err((code, message)) => {
				send_error(responses, request_id, code, message).await;
				return;
			},
		};
		if already_committed {
			send_error(
				responses,
				request_id,
				pb::ProtocolErrorCode::AlreadyExists,
				"ArgsCommitted was already received",
			)
			.await;
			return;
		}

		match connection.invocation_mut(request_id, &request.invocation_id) {
			Ok(InvocationState::Native { admission, pending_commit, .. })
			| Ok(InvocationState::Worker { admission, pending_commit, .. })
				if !admission.is_answered() =>
			{
				if pending_commit.is_some() {
					send_error(
						responses,
						request_id,
						pb::ProtocolErrorCode::AlreadyExists,
						"ArgsCommitted was already received",
					)
					.await;
				} else {
					*pending_commit = Some(request);
				}
				return;
			},
			Ok(_) => {},
			Err((code, message)) => {
				send_error(responses, request_id, code, message).await;
				return;
			},
		}

		let (admission, maximum_effects) =
			match connection.invocation_mut(request_id, &request.invocation_id) {
				Ok(InvocationState::Native { admission, maximum_effects, .. })
				| Ok(InvocationState::Worker { admission, maximum_effects, .. }) => (
					admission
						.decide(self.workspace.root(), self.workspace.root())
						.await,
					maximum_effects.clone(),
				),
				Err((code, message)) => {
					send_error(responses, request_id, code, message).await;
					return;
				},
			};
		request.raw = match admission {
			AdmissionDecision::Allowed { raw, bash } => {
				let _effective_bash = bash;
				raw
			},
			AdmissionDecision::Denied(policy) => {
				let invocation_id = Str::from(request.invocation_id.as_str());
				connection.abandon_admission(request_id, &invocation_id);
				send_policy_denied_verdict(responses, request_id, &invocation_id, policy).await;
				return;
			},
		};
		let narrowed_effects =
			match effects_narrow_or_refuse(request.effects.as_ref(), &maximum_effects) {
				Some(effects) => effects,
				None => {
					send_error(
						responses,
						request_id,
						pb::ProtocolErrorCode::PermissionDenied,
						"ArgsCommitted effect envelope widens the declared tool authority",
					)
					.await;
					return;
				},
			};
		request.effects = Some((&narrowed_effects).into());
		let result = connection.invocation_mut(request_id, &request.invocation_id);
		match result {
			Ok(InvocationState::Native { feed, lifecycle, .. }) => {
				let Ok(raw) = std::str::from_utf8(&request.raw) else {
					send_error(
						responses,
						request_id,
						pb::ProtocolErrorCode::InvalidArgument,
						"committed arguments are not UTF-8",
					)
					.await;
					return;
				};
				match lifecycle.commit() {
					Ok(()) => {},
					Err(NativeCommitError::AlreadyCommitted) => {
						send_error(
							responses,
							request_id,
							pb::ProtocolErrorCode::AlreadyExists,
							"ArgsCommitted was already received",
						)
						.await;
						return;
					},
					Err(NativeCommitError::Terminal) => {
						send_error(
							responses,
							request_id,
							pb::ProtocolErrorCode::PreconditionFailed,
							"native invocation is already terminal",
						)
						.await;
						return;
					},
				}
				if feed.args_committed(Str::from(raw)).is_err() {
					send_error(
						responses,
						request_id,
						pb::ProtocolErrorCode::Cancelled,
						"invocation input is closed",
					)
					.await;
				}
			},
			Ok(InvocationState::Worker {
				id,
				owner,
				invocation,
				committed,
				cancel,
				interrupts,
				..
			}) => {
				if *committed {
					send_error(
						responses,
						request_id,
						pb::ProtocolErrorCode::AlreadyExists,
						"ArgsCommitted was already received",
					)
					.await;
					return;
				}
				if request.effect_token.is_empty() || request.authorized_at_ms == 0 {
					send_policy_error(responses, request_id, PolicyError::InvalidEffectToken).await;
					return;
				}
				let Some(worker) = invocation.as_mut() else {
					send_error(
						responses,
						request_id,
						pb::ProtocolErrorCode::PreconditionFailed,
						"worker invocation was already dispatched",
					)
					.await;
					return;
				};
				if let Err(error) = worker.args_committed(request) {
					self.authority.settle(owner, id);
					send_error(
						responses,
						request_id,
						pb::ProtocolErrorCode::PreconditionFailed,
						&error.to_string(),
					)
					.await;
					return;
				}
				*committed = true;
				let Some(invocation) = invocation.take() else {
					return;
				};
				let Some(interrupts) = interrupts.take() else {
					return;
				};
				spawn_worker_invocation(
					request_id,
					id.clone(),
					invocation,
					cancel.clone(),
					interrupts,
					responses.clone(),
					finished.clone(),
				);
			},
			Err((code, message)) => send_error(responses, request_id, code, message).await,
		}
	}

	fn worker_owner(&self, name: &str, rev: &str) -> Option<HostKey> {
		self
			.ext_hosts
			.registrations()
			.iter()
			.find_map(|registration| {
				let declaration = &registration.declaration;
				(declaration.rev == rev
					&& declaration
						.definition
						.as_ref()
						.is_some_and(|definition| definition.name == name))
				.then(|| registration.owner.clone())
			})
	}

	async fn put_chunk(
		&self,
		request_id: u64,
		chunk: blob_pb::Chunk,
		responses: &flume::Sender<pb::ServerFrame>,
		connection: &mut ConnectionState,
	) {
		if let Err(error) = connection.quotas.charge_blob_bytes(chunk.data.len()) {
			send_policy_error(responses, request_id, error).await;
			return;
		}
		connection
			.requests
			.entry(request_id)
			.or_insert_with(|| RequestState::BlobPut(BlobUpload::default()));
		let Some(RequestState::BlobPut(upload)) = connection.requests.get_mut(&request_id) else {
			send_error(
				responses,
				request_id,
				pb::ProtocolErrorCode::AlreadyExists,
				"request_id is already open for another operation",
			)
			.await;
			return;
		};
		if upload.chunks != 0 && (!chunk.hash.is_empty() || chunk.size.is_some()) {
			send_error(
				responses,
				request_id,
				pb::ProtocolErrorCode::InvalidArgument,
				"blob hash and size metadata are legal only on the first chunk",
			)
			.await;
			return;
		}
		if upload.chunks == 0 {
			upload.expected_hash = (!chunk.hash.is_empty()).then_some(chunk.hash);
			upload.expected_size = chunk.size;
		}
		upload.data.extend_from_slice(&chunk.data);
		upload.chunks += 1;
	}

	async fn commit_blob(
		&self,
		request_id: u64,
		responses: &flume::Sender<pb::ServerFrame>,
		connection: &mut ConnectionState,
	) {
		let upload = match connection.requests.remove(&request_id) {
			Some(RequestState::BlobPut(upload)) => upload,
			Some(other) => {
				connection.requests.insert(request_id, other);
				send_error(
					responses,
					request_id,
					pb::ProtocolErrorCode::AlreadyExists,
					"request_id is already open for another operation",
				)
				.await;
				return;
			},
			None => BlobUpload::default(),
		};
		match self.blobs.put_checked(
			&upload.data,
			upload.expected_hash.as_deref(),
			upload.expected_size,
		) {
			Ok(id) => {
				send_body(
					responses,
					request_id,
					server_frame::Body::BlobPut(blob_pb::PutResponse {
						hash: Bytes::copy_from_slice(&id.hash),
						size: id.size,
					}),
				)
				.await;
			},
			Err(error) => send_blob_error(responses, request_id, &error).await,
		}
	}
}

struct ConnectionState {
	owner:            Str,
	requests:         HashMap<u64, RequestState>,
	invocation_ids:   HashMap<Str, u64>,
	exec_host:        ExecHost,
	document_leases:  HashMap<Bytes, DocumentLease>,
	grants:           Grants,
	host:             Option<HostKey>,
	ambient:          bool,
	authority:        Arc<AuthorityTable>,
	connection_owner: u64,
	quotas:           QuotaAccount,
}

enum RequestState {
	Invocation(InvocationState),
	InvocationFinishing,
	Exec { exec: Bytes, cancel: CancellationToken },
	ProcessAttach { cancel: CancellationToken },
	BlobPut(BlobUpload),
	BlobGet { cancel: CancellationToken },
	DataStream { cancel: CancellationToken },
	DocumentEvents { lease_id: Bytes, cancel: CancellationToken },
	LspEvents { cancel: CancellationToken },
}

enum InvocationState {
	Native {
		id:              Str,
		feed:            omp_tool::InvocationFeed,
		lifecycle:       Arc<NativeLifecycle>,
		admission:       AdmissionGate,
		pending_commit:  Option<pb::ArgsCommitted>,
		maximum_effects: Effects,
		cancel:          CancellationToken,
	},
	Worker {
		id:              Str,
		owner:           HostKey,
		invocation:      Option<super::worker::WorkerInvocation>,
		committed:       bool,
		admission:       AdmissionGate,
		pending_commit:  Option<pb::ArgsCommitted>,
		maximum_effects: Effects,
		interrupt:       flume::Sender<pb::Interrupt>,
		interrupts:      Option<flume::Receiver<pb::Interrupt>>,
		cancel:          CancellationToken,
	},
}

const NATIVE_COMMITTED: u8 = 1;
const NATIVE_TERMINAL: u8 = 2;

#[derive(Default)]
struct NativeLifecycle {
	state: AtomicU8,
}

enum NativeCommitError {
	AlreadyCommitted,
	Terminal,
}

impl NativeLifecycle {
	fn commit(&self) -> Result<(), NativeCommitError> {
		self
			.state
			.try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
				(state & (NATIVE_COMMITTED | NATIVE_TERMINAL) == 0).then_some(state | NATIVE_COMMITTED)
			})
			.map(|_| ())
			.map_err(|state| {
				if state & NATIVE_COMMITTED != 0 {
					NativeCommitError::AlreadyCommitted
				} else {
					NativeCommitError::Terminal
				}
			})
	}

	fn is_committed(&self) -> bool {
		self.state.load(Ordering::Acquire) & NATIVE_COMMITTED != 0
	}

	fn is_terminal(&self) -> bool {
		self.state.load(Ordering::Acquire) & NATIVE_TERMINAL != 0
	}

	fn claim_terminal(&self) -> bool {
		self
			.state
			.try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
				(state & NATIVE_TERMINAL == 0).then_some(state | NATIVE_TERMINAL)
			})
			.is_ok()
	}

	fn claim_precommit_terminal(&self) -> bool {
		self
			.state
			.compare_exchange(0, NATIVE_TERMINAL, Ordering::AcqRel, Ordering::Acquire)
			.is_ok()
	}
}

#[derive(Default)]
struct BlobUpload {
	data:          BytesMut,
	expected_hash: Option<Bytes>,
	expected_size: Option<u64>,
	chunks:        usize,
}

struct Finished {
	request_id:    u64,
	invocation_id: Option<Str>,
}

enum LoopEvent {
	/// Boxes the foreign generated protobuf frame to keep this local event enum
	/// compact.
	Frame(Box<pb::ClientFrame>),
	Finished(Finished),
	/// The env-owned deadline of at least one pending admission elapsed.
	AdmissionDeadline,
}

impl ConnectionState {
	fn new(
		exec_host: ExecHost,
		grants: Grants,
		authority: Arc<AuthorityTable>,
		policy: &ConnectionPolicy,
	) -> Self {
		let owner = policy.host.as_ref().map_or_else(
			|| {
				let number = NEXT_CONNECTION_OWNER.fetch_add(1, Ordering::Relaxed);
				Str::from(format!("env-connection-{number}"))
			},
			|host| {
				let [layer, tier, extension] = host.fields();
				Str::from(format!("extension:{layer}:{tier}:{extension}"))
			},
		);
		let connection_owner = authority.connection_owner();
		let quotas = QuotaAccount::new(Arc::clone(&authority), policy.host.clone());
		Self {
			owner,
			requests: HashMap::new(),
			invocation_ids: HashMap::new(),
			exec_host,
			document_leases: HashMap::new(),
			grants,
			host: policy.host.clone(),
			ambient: policy.ambient,
			authority,
			connection_owner,
			quotas,
		}
	}

	fn next_admission_deadline(&self) -> Option<tokio::time::Instant> {
		self
			.requests
			.values()
			.filter_map(|state| match state {
				RequestState::Invocation(invocation) => invocation.pending_admission_deadline(),
				_ => None,
			})
			.min()
	}

	fn take_expired_admissions(&mut self) -> Vec<(u64, Str, omp_proto::policy::v1::PolicyDenied)> {
		let now = tokio::time::Instant::now();
		self
			.requests
			.iter_mut()
			.filter_map(|(request_id, state)| match state {
				RequestState::Invocation(invocation) => invocation
					.expire_admission(now)
					.map(|denied| (*request_id, Str::from(invocation.id()), denied)),
				_ => None,
			})
			.collect()
	}

	fn grants(&self, capability: &str) -> bool {
		self.grants.contains(capability)
	}

	fn invocation_mut(
		&mut self,
		request_id: u64,
		invocation_id: &str,
	) -> Result<&mut InvocationState, (pb::ProtocolErrorCode, &'static str)> {
		match self.requests.get_mut(&request_id) {
			Some(RequestState::Invocation(state)) if state.id() == invocation_id => Ok(state),
			Some(RequestState::Invocation(_)) => Err((
				pb::ProtocolErrorCode::InvalidArgument,
				"invocation_id does not match the open request",
			)),
			Some(_) => Err((
				pb::ProtocolErrorCode::PreconditionFailed,
				"request_id is not an invocation stream",
			)),
			None => Err((pb::ProtocolErrorCode::NotFound, "invocation is not open")),
		}
	}

	/// Removes a denied pre-authorization invocation before its executor sees
	/// finalized arguments.
	fn abandon_admission(&mut self, request_id: u64, invocation_id: &Str) {
		match self.requests.remove(&request_id) {
			Some(RequestState::Invocation(InvocationState::Native { lifecycle, cancel, .. })) => {
				lifecycle.claim_precommit_terminal();
				cancel.cancel();
			},
			Some(RequestState::Invocation(InvocationState::Worker { owner, id, cancel, .. })) => {
				cancel.cancel();
				self.authority.settle(&owner, &id);
			},
			_ => {},
		}
		self.invocation_ids.remove(invocation_id);
	}

	async fn exec_id(
		&self,
		request_id: u64,
		expected: &[u8],
		responses: &flume::Sender<pb::ServerFrame>,
	) -> Option<Bytes> {
		match self.requests.get(&request_id) {
			Some(RequestState::Exec { exec, .. }) if exec.as_ref() == expected => Some(exec.clone()),
			Some(RequestState::Exec { .. }) => {
				send_error(
					responses,
					request_id,
					pb::ProtocolErrorCode::InvalidArgument,
					"exec id does not match the open request",
				)
				.await;
				None
			},
			Some(_) => {
				send_error(
					responses,
					request_id,
					pb::ProtocolErrorCode::PreconditionFailed,
					"request_id is not an exec stream",
				)
				.await;
				None
			},
			None => {
				send_error(
					responses,
					request_id,
					pb::ProtocolErrorCode::NotFound,
					"execution is not open",
				)
				.await;
				None
			},
		}
	}

	fn finish(&mut self, done: Finished) {
		match self.requests.remove(&done.request_id) {
			Some(RequestState::Invocation(InvocationState::Worker { owner, id, .. })) => {
				self.authority.settle(&owner, &id);
			},
			Some(RequestState::Exec { .. }) => self.quotas.release_exec(),
			Some(
				RequestState::ProcessAttach { .. }
				| RequestState::BlobGet { .. }
				| RequestState::DataStream { .. }
				| RequestState::DocumentEvents { .. }
				| RequestState::LspEvents { .. },
			) => self.quotas.release_stream(),
			_ => {},
		}
		if let Some(invocation_id) = done.invocation_id {
			self.invocation_ids.remove(&invocation_id);
		}
	}

	async fn cancel(
		&mut self,
		request: pb::CancelRequest,
		exec_host: &ExecHost,
		responses: &flume::Sender<pb::ServerFrame>,
		finished: &flume::Sender<Finished>,
	) {
		use pb::cancel_request::Target;
		match request.target {
			Some(Target::TargetRequestId(request_id)) => {
				if let Some(RequestState::Exec { exec, .. }) = self.requests.get(&request_id) {
					let _ = exec_host.cancel(exec);
				} else {
					self
						.cancel_request(request_id, exec_host, responses, finished)
						.await;
				}
			},
			Some(Target::InvocationId(invocation_id)) => {
				if let Some(request_id) = self.invocation_ids.get(invocation_id.as_str()).copied() {
					self
						.cancel_request(request_id, exec_host, responses, finished)
						.await;
				}
			},
			Some(Target::Exec(exec_id)) => {
				let _ = exec_host.cancel(&exec_id);
			},
			None => {},
		}
	}

	async fn cancel_request(
		&mut self,
		request_id: u64,
		exec_host: &ExecHost,
		responses: &flume::Sender<pb::ServerFrame>,
		finished: &flume::Sender<Finished>,
	) {
		if let Some(RequestState::Invocation(state)) = self.requests.get_mut(&request_id) {
			let terminal = match state {
				InvocationState::Native { id, feed, lifecycle, cancel, .. } => {
					if lifecycle.is_committed() {
						let _ = feed.interrupt(Interrupt {
							class:  Str::new_static("cancel"),
							reason: Str::new_static("invocation cancelled by client"),
						});
						cancel.cancel();
						None
					} else if lifecycle.claim_precommit_terminal() {
						cancel.cancel();
						Some((id.clone(), omp_tool::Abort::Skipped {
							reason: Str::new_static("invocation cancelled before argument commitment"),
						}))
					} else {
						cancel.cancel();
						None
					}
				},
				InvocationState::Worker { id, owner, committed, cancel, .. } => {
					cancel.cancel();
					(!*committed).then(|| {
						self.authority.settle(owner, id);
						(id.clone(), omp_tool::Abort::Skipped {
							reason: Str::new_static("invocation cancelled before argument commitment"),
						})
					})
				},
			};
			if terminal.is_some() {
				self
					.requests
					.insert(request_id, RequestState::InvocationFinishing);
			}
			if let Some((invocation_id, abort)) = terminal {
				send_abort_verdict(responses, request_id, &invocation_id, abort).await;
				let _ = finished
					.send_async(Finished { request_id, invocation_id: Some(invocation_id) })
					.await;
			}
			return;
		}
		if matches!(self.requests.get(&request_id), Some(RequestState::InvocationFinishing)) {
			return;
		}

		let Some(state) = self.requests.remove(&request_id) else {
			return;
		};
		match state {
			RequestState::Invocation(_) => unreachable!("invocations were handled without removal"),
			RequestState::InvocationFinishing => {},
			RequestState::Exec { exec, cancel } => {
				let _ = exec_host.cancel(&exec);
				cancel.cancel();
			},
			RequestState::ProcessAttach { cancel }
			| RequestState::BlobGet { cancel }
			| RequestState::DataStream { cancel }
			| RequestState::DocumentEvents { cancel, .. }
			| RequestState::LspEvents { cancel } => cancel.cancel(),
			RequestState::BlobPut(_) => {},
		}
	}

	async fn interrupt(
		&mut self,
		request_id: u64,
		request: pb::Interrupt,
		responses: &flume::Sender<pb::ServerFrame>,
		finished: &flume::Sender<Finished>,
	) {
		let mut settle = None;
		let result = self.invocation_mut(request_id, &request.invocation_id);
		let terminal = match result {
			Ok(InvocationState::Native { id, feed, lifecycle, cancel, .. }) => {
				let reason = Str::from(request.reason);
				let _ = feed.interrupt(Interrupt {
					class:  Str::new_static("immediate"),
					reason: reason.clone(),
				});
				if lifecycle.is_committed() {
					None
				} else if lifecycle.claim_precommit_terminal() {
					cancel.cancel();
					Some((id.clone(), omp_tool::Abort::Interrupted { reason }))
				} else {
					cancel.cancel();
					None
				}
			},
			Ok(InvocationState::Worker { id, owner, committed, cancel, interrupt, .. }) => {
				let reason = Str::from(request.reason.as_str());
				if *committed {
					let _ = interrupt.send(request);
					None
				} else {
					settle = Some((owner.clone(), id.clone()));
					cancel.cancel();
					Some((id.clone(), omp_tool::Abort::Interrupted { reason }))
				}
			},
			Err((code, message)) => {
				send_error(responses, request_id, code, message).await;
				return;
			},
		};
		if let Some((owner, invocation_id)) = settle {
			self.authority.settle(&owner, &invocation_id);
		}
		if terminal.is_some() {
			self
				.requests
				.insert(request_id, RequestState::InvocationFinishing);
		}
		if let Some((invocation_id, abort)) = terminal {
			send_abort_verdict(responses, request_id, &invocation_id, abort).await;
			let _ = finished
				.send_async(Finished { request_id, invocation_id: Some(invocation_id) })
				.await;
		}
	}

	fn cancel_all(&mut self, exec_host: &ExecHost) {
		for (_, state) in std::mem::take(&mut self.requests) {
			match state {
				RequestState::Invocation(InvocationState::Native {
					feed, lifecycle, cancel, ..
				}) => {
					if lifecycle.is_committed() {
						let _ = feed.interrupt(Interrupt {
							class:  Str::new_static("disconnect"),
							reason: Str::new_static("environment connection closed"),
						});
					}
					lifecycle.claim_terminal();
					cancel.cancel();
				},
				RequestState::Invocation(InvocationState::Worker { owner, id, cancel, .. }) => {
					self.authority.settle(&owner, &id);
					cancel.cancel();
				},
				RequestState::InvocationFinishing => {},
				RequestState::Exec { exec, cancel } => {
					let _ = exec_host.cancel(&exec);
					cancel.cancel();
					self.quotas.release_exec();
				},
				RequestState::ProcessAttach { cancel }
				| RequestState::BlobGet { cancel }
				| RequestState::DataStream { cancel }
				| RequestState::DocumentEvents { cancel, .. }
				| RequestState::LspEvents { cancel } => {
					cancel.cancel();
					self.quotas.release_stream();
				},
				RequestState::BlobPut(_) => {},
			}
		}
		self.invocation_ids.clear();
		for lease_id in self.document_leases.keys() {
			self
				.authority
				.release_lease(lease_id, self.connection_owner);
		}
		self.document_leases.clear();
	}
}

impl Drop for ConnectionState {
	fn drop(&mut self) {
		let exec_host = self.exec_host.clone();
		self.cancel_all(&exec_host);
	}
}

impl InvocationState {
	fn id(&self) -> &str {
		match self {
			Self::Native { id, .. } | Self::Worker { id, .. } => id,
		}
	}

	fn pending_admission_deadline(&self) -> Option<tokio::time::Instant> {
		match self {
			Self::Native { admission, .. } | Self::Worker { admission, .. } => {
				admission.pending_deadline()
			},
		}
	}

	fn expire_admission(
		&mut self,
		now: tokio::time::Instant,
	) -> Option<omp_proto::policy::v1::PolicyDenied> {
		match self {
			Self::Native { admission, .. } | Self::Worker { admission, .. } => admission.expire(now),
		}
	}
}

async fn reject_duplicate_open(
	connection: &ConnectionState,
	request_id: u64,
	responses: &flume::Sender<pb::ServerFrame>,
) -> bool {
	if connection.requests.contains_key(&request_id) {
		send_error(
			responses,
			request_id,
			pb::ProtocolErrorCode::AlreadyExists,
			"request_id is already open",
		)
		.await;
		true
	} else {
		false
	}
}

enum NativeForward {
	Continue,
	Terminal,
	Backpressure,
}

async fn spawn_native_invocation(
	request_id: u64,
	invocation_id: Str,
	name: Str,
	feed: omp_tool::InvocationFeed,
	deadline: Duration,
	params: IncomingParams<'static>,
	registry: Arc<Registry>,
	lifecycle: Arc<NativeLifecycle>,
	cancel: CancellationToken,
	responses: flume::Sender<pb::ServerFrame>,
	finished: flume::Sender<Finished>,
) {
	let (started, start) = flume::bounded(1);
	tokio::spawn(async move {
		let result = registry.invoke(&name, params);
		let _ = started.send(());
		match result {
			Ok(mut stream) => {
				let mut deadline = Box::pin(tokio::time::sleep(deadline));
				let mut cancel_grace: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
				let mut timed_out = false;
				let mut grace_expired = false;
				loop {
					if lifecycle.is_terminal() {
						break;
					}
					if let Some(grace) = cancel_grace.as_mut() {
						tokio::select! {
							biased;
							() = grace.as_mut() => {
								grace_expired = true;
								break;
							},
							event = stream.next() => {
								let reason = if timed_out {
									"native invocation ended without reporting timeout truth"
								} else {
									"native invocation ended without reporting cancellation truth"
								};
								if matches!(
									forward_native_event(
										event,
										true,
										reason,
										request_id,
										&invocation_id,
										&lifecycle,
										&responses,
									)
									.await,
									NativeForward::Terminal
								) {
									break;
								}
							},
						}
					} else {
						tokio::select! {
							biased;
							() = deadline.as_mut() => {
								let reason = Str::new_static("native invocation deadline exceeded");
								let _ = feed.interrupt(Interrupt {
									class: Str::new_static("deadline"),
									reason: reason.clone(),
								});
								if lifecycle.is_committed() {
									timed_out = true;
									cancel_grace = Some(Box::pin(tokio::time::sleep(
										NATIVE_CANCEL_GRACE,
									)));
								} else if lifecycle.claim_precommit_terminal() {
									send_abort_verdict(
										&responses,
										request_id,
										&invocation_id,
										omp_tool::Abort::Interrupted { reason },
									)
									.await;
									break;
								} else {
									break;
								}
							},
							() = cancel.cancelled() => {
								if lifecycle.is_committed() {
									cancel_grace = Some(Box::pin(tokio::time::sleep(
										NATIVE_CANCEL_GRACE,
									)));
								} else {
									break;
								}
							},
							event = stream.next() => {
								match forward_native_event(
									event,
									false,
									"",
									request_id,
									&invocation_id,
									&lifecycle,
									&responses,
								)
								.await
								{
									NativeForward::Continue => {},
									NativeForward::Terminal => break,
									NativeForward::Backpressure => {
										let _ = feed.interrupt(Interrupt {
											class: Str::new_static("backpressure"),
											reason: Str::new_static(
												"invocation response consumer stopped reading",
											),
										});
										if lifecycle.is_committed() {
											cancel_grace = Some(Box::pin(tokio::time::sleep(
												NATIVE_CANCEL_GRACE,
											)));
										} else {
											lifecycle.claim_terminal();
											break;
										}
									},
								}
							},
						}
					}
				}
				if grace_expired && lifecycle.is_committed() && lifecycle.claim_terminal() {
					drop(stream);
					let reason = if timed_out {
						Str::new_static(
							"native invocation exceeded its deadline and did not stop within grace",
						)
					} else {
						Str::new_static("native invocation did not stop within cancellation grace")
					};
					send_abort_verdict(
						&responses,
						request_id,
						&invocation_id,
						omp_tool::Abort::EffectsUnknown { reason },
					)
					.await;
				}
			},
			Err(error) => {
				if lifecycle.claim_terminal() {
					let _ = send_invocation_error(
						&responses,
						request_id,
						pb::ProtocolErrorCode::NotFound,
						&error.to_string(),
					)
					.await;
				}
			},
		}
		let _ = finished
			.send_async(Finished { request_id, invocation_id: Some(invocation_id) })
			.await;
	});
	let _ = start.recv_async().await;
}

async fn forward_native_event(
	event: Option<Result<ErasedEv, omp_tool::RegistryError>>,
	cancelling: bool,
	fallback_reason: &str,
	request_id: u64,
	invocation_id: &Str,
	lifecycle: &NativeLifecycle,
	responses: &flume::Sender<pb::ServerFrame>,
) -> NativeForward {
	match event {
		Some(Ok(ErasedEv::Update(_))) if cancelling => NativeForward::Continue,
		Some(Ok(ErasedEv::Update(_))) if lifecycle.is_terminal() => NativeForward::Terminal,
		Some(Ok(ErasedEv::Update(json))) => {
			if send_invocation_body(
				responses,
				request_id,
				server_frame::Body::Update(pb::Update {
					invocation_id: invocation_id.to_string(),
					json,
					props: Default::default(),
				}),
			)
			.await
			{
				NativeForward::Continue
			} else {
				NativeForward::Backpressure
			}
		},
		Some(Ok(ErasedEv::Done(outcome))) => {
			if lifecycle.claim_terminal() {
				let (json, is_error, useless) = erased_outcome_wire(outcome);
				send_invocation_terminal_body(
					responses,
					request_id,
					server_frame::Body::Verdict(pb::Verdict {
						invocation_id: invocation_id.to_string(),
						json,
						details_blob: None,
						parts: Vec::new(),
						is_error,
						useless,
						props: Default::default(),
					}),
				)
				.await;
			}
			NativeForward::Terminal
		},
		Some(Err(error)) if !cancelling => {
			if lifecycle.claim_terminal() {
				let _ = send_invocation_error(
					responses,
					request_id,
					pb::ProtocolErrorCode::Internal,
					&error.to_string(),
				)
				.await;
			}
			NativeForward::Terminal
		},
		None if !cancelling => {
			if lifecycle.claim_terminal() {
				let _ = send_invocation_stream_error(
					responses,
					request_id,
					invocation_id,
					"tool event stream closed without a terminal verdict",
				)
				.await;
			}
			NativeForward::Terminal
		},
		Some(Err(_)) | None => {
			if lifecycle.is_committed() && lifecycle.claim_terminal() {
				send_abort_verdict(
					responses,
					request_id,
					invocation_id,
					omp_tool::Abort::EffectsUnknown { reason: Str::from(fallback_reason) },
				)
				.await;
			}
			NativeForward::Terminal
		},
	}
}

fn spawn_worker_invocation(
	request_id: u64,
	invocation_id: Str,
	mut invocation: super::worker::WorkerInvocation,
	cancel: CancellationToken,
	interrupts: flume::Receiver<pb::Interrupt>,
	responses: flume::Sender<pb::ServerFrame>,
	finished: flume::Sender<Finished>,
) {
	tokio::spawn(async move {
		let mut cancel_requested = false;
		loop {
			let event = if cancel_requested {
				invocation.next().await.ok()
			} else {
				tokio::select! {
					biased;
					() = cancel.cancelled() => {
						invocation.cancel("environment invocation cancelled");
						cancel_requested = true;
						continue;
					},
					frame = interrupts.recv_async() => {
						if let Ok(frame) = frame {
							let _ = invocation.interrupt(frame);
						}
						continue;
					},
					event = invocation.next() => event.ok(),
				}
			};
			match event {
				Some(WorkerEvent::Update(_)) if cancel_requested => {},
				Some(WorkerEvent::Update(update)) => {
					if !send_invocation_body(
						&responses,
						request_id,
						server_frame::Body::Update(pb::Update {
							invocation_id: invocation_id.to_string(),
							json:          update.json,
							props:         Default::default(),
						}),
					)
					.await
					{
						invocation.cancel("invocation response consumer stopped reading");
						cancel_requested = true;
					}
				},
				Some(WorkerEvent::Pull(_)) => {
					let _ = send_invocation_error(
						&responses,
						request_id,
						pb::ProtocolErrorCode::Unsupported,
						"worker cursor pulls are unsupported on env/v1",
					)
					.await;
					invocation.cancel("worker requested an unsupported cursor pull");
					break;
				},
				Some(WorkerEvent::ProtocolError(error)) => {
					let _ = send_invocation_error(
						&responses,
						request_id,
						pb::ProtocolErrorCode::Internal,
						&error.message,
					)
					.await;
					break;
				},
				Some(WorkerEvent::Complete(complete)) => {
					let Ok((json, details_blob, is_error)) = worker_completion_json(&complete) else {
						send_abort_verdict(
							&responses,
							request_id,
							&invocation_id,
							omp_tool::Abort::EffectsUnknown {
								reason: Str::new_static("worker returned invalid structured result JSON"),
							},
						)
						.await;
						break;
					};
					send_invocation_terminal_body(
						&responses,
						request_id,
						server_frame::Body::Verdict(pb::Verdict {
							invocation_id: invocation_id.to_string(),
							json,
							parts: complete.parts,
							details_blob,
							is_error,
							useless: complete.useless,
							props: Default::default(),
						}),
					)
					.await;
					break;
				},
				Some(WorkerEvent::Aborted(abort)) => {
					let reason = if abort.effects_unknown {
						omp_tool::Abort::EffectsUnknown { reason: abort.reason }
					} else {
						omp_tool::Abort::Skipped { reason: abort.reason }
					};
					send_abort_verdict(&responses, request_id, &invocation_id, reason).await;
					break;
				},
				None => {
					let _ = send_invocation_stream_error(
						&responses,
						request_id,
						&invocation_id,
						"tool worker event stream closed without a terminal verdict",
					)
					.await;
					break;
				},
			}
		}
		let _ = finished
			.send_async(Finished { request_id, invocation_id: Some(invocation_id) })
			.await;
	});
}

async fn send_abort_verdict(
	responses: &flume::Sender<pb::ServerFrame>,
	request_id: u64,
	invocation_id: &Str,
	abort: omp_tool::Abort,
) {
	let verdict = CallOutcome::<serde_json::Value, serde_json::Value>::aborted(abort);
	let Ok(json) = serde_json::to_vec(&verdict) else {
		let _ = send_invocation_stream_error(
			responses,
			request_id,
			invocation_id,
			"failed to serialize invocation abort verdict",
		)
		.await;
		return;
	};
	send_invocation_terminal_body(
		responses,
		request_id,
		server_frame::Body::Verdict(pb::Verdict {
			invocation_id: invocation_id.to_string(),
			json:          Bytes::from(json),
			details_blob:  None,
			parts:         Vec::new(),
			is_error:      true,
			useless:       false,
			props:         Default::default(),
		}),
	)
	.await;
}

async fn send_policy_denied_verdict(
	responses: &flume::Sender<pb::ServerFrame>,
	request_id: u64,
	invocation_id: &Str,
	denied: omp_proto::policy::v1::PolicyDenied,
) {
	let reason = Str::from(denied.reason);
	let policy = omp_tool::PolicyDenied {
		reason:      reason.clone(),
		code:        (!denied.code.is_empty()).then(|| Str::from(denied.code)),
		decision_id: Str::from(denied.decision_id),
		rules:       Arc::from(
			denied
				.rules
				.into_iter()
				.map(|rule| Str::from(rule.as_str()))
				.collect::<Vec<_>>(),
		),
	};
	let verdict = CallOutcome::<serde_json::Value, serde_json::Value>::policy_denied(
		omp_tool::Abort::Skipped { reason },
		policy,
	);
	let Ok(json) = serde_json::to_vec(&verdict) else {
		let _ = send_invocation_stream_error(
			responses,
			request_id,
			invocation_id,
			"failed to serialize policy denial verdict",
		)
		.await;
		return;
	};
	send_invocation_terminal_body(
		responses,
		request_id,
		server_frame::Body::Verdict(pb::Verdict {
			invocation_id: invocation_id.to_string(),
			json:          Bytes::from(json),
			details_blob:  None,
			parts:         Vec::new(),
			is_error:      true,
			useless:       false,
			props:         Default::default(),
		}),
	)
	.await;
}

async fn send_invocation_error(
	responses: &flume::Sender<pb::ServerFrame>,
	request_id: u64,
	code: pb::ProtocolErrorCode,
	message: &str,
) -> bool {
	let body = server_frame::Body::Error(pb::ProtocolError {
		code:    code as i32,
		message: message.to_owned(),
		props:   Default::default(),
	});
	send_invocation_terminal_body(responses, request_id, body).await;
	true
}

async fn send_invocation_stream_error(
	responses: &flume::Sender<pb::ServerFrame>,
	request_id: u64,
	invocation_id: &str,
	message: &str,
) -> bool {
	let body = server_frame::Body::EventStreamError(pb::EventStreamError {
		stream:         pb::EventStreamKind::Invocation as i32,
		failure:        pb::EventStreamFailure::Closed as i32,
		invocation_id:  invocation_id.to_owned(),
		exec:           Bytes::new(),
		process_name:   String::new(),
		skipped_events: 0,
		message:        message.to_owned(),
		props:          Default::default(),
	});
	send_invocation_terminal_body(responses, request_id, body).await;
	true
}

fn spawn_exec(
	request_id: u64,
	run: super::exec::ExecRun,
	cancel: CancellationToken,
	responses: flume::Sender<pb::ServerFrame>,
	finished: flume::Sender<Finished>,
) {
	tokio::spawn(async move {
		let exec = Bytes::copy_from_slice(run.id());
		let mut terminal = false;
		loop {
			let event = tokio::select! {
				() = cancel.cancelled() => break,
				event = run.next_event() => event,
			};
			match event {
				Some(ExecEvent::Started { .. }) => {},
				Some(ExecEvent::Output(output)) => {
					send_body(&responses, request_id, server_frame::Body::Output(output)).await;
				},
				Some(ExecEvent::Exit(exit)) => {
					terminal = true;
					send_body(&responses, request_id, server_frame::Body::Exit(exit)).await;
					break;
				},
				None => break,
			}
		}
		if !terminal && !cancel.is_cancelled() {
			send_stream_error(
				&responses,
				request_id,
				pb::EventStreamKind::Exec,
				"",
				&exec,
				"",
				"exec event stream closed without ExitEvent",
			)
			.await;
		}
		let _ = finished
			.send_async(Finished { request_id, invocation_id: None })
			.await;
	});
}

fn spawn_process_attachment(
	request_id: u64,
	process_name: Str,
	events: flume::Receiver<ProcessEvent>,
	cancel: CancellationToken,
	responses: flume::Sender<pb::ServerFrame>,
	finished: flume::Sender<Finished>,
) {
	tokio::spawn(async move {
		loop {
			let event = tokio::select! {
				() = cancel.cancelled() => break,
				event = events.recv_async() => event.ok(),
			};
			match event {
				Some(ProcessEvent::Output(output)) => {
					send_body(&responses, request_id, server_frame::Body::ProcessOutput(output)).await;
				},
				Some(ProcessEvent::State(process)) => {
					send_body(
						&responses,
						request_id,
						server_frame::Body::ProcessState(pb::ProcessStateEvent {
							process: Some(process),
							props:   Default::default(),
						}),
					)
					.await;
				},
				None => {
					send_stream_error(
						&responses,
						request_id,
						pb::EventStreamKind::ProcessOutput,
						"",
						&[],
						&process_name,
						"named-process output stream closed",
					)
					.await;
					break;
				},
			}
		}
		let _ = finished
			.send_async(Finished { request_id, invocation_id: None })
			.await;
	});
}

struct WorkspaceSearchOwned {
	pattern: Str,
	case:    WorkspaceSearchCase,
	limit:   Option<u64>,
}

fn spawn_document_events(
	request_id: u64,
	events: DocumentEvents,
	cancel: CancellationToken,
	responses: flume::Sender<pb::ServerFrame>,
	finished: flume::Sender<Finished>,
) {
	tokio::spawn(async move {
		loop {
			let event = tokio::select! {
				() = cancel.cancelled() => break,
				event = events.next_event() => event,
			};
			match event {
				Ok(event) => {
					if responses
						.send_async(server_frame(
							request_id,
							server_frame::Body::DataEvent(pb::DataEvent {
								body:  Some(pb::data_event::Body::Document(event)),
								props: Default::default(),
							}),
						))
						.await
						.is_err()
					{
						break;
					}
				},
				Err(error) => {
					let _ = responses
						.send_async(server_frame(
							request_id,
							server_frame::Body::EventStreamError(pb::EventStreamError {
								stream:         pb::EventStreamKind::Document.into(),
								failure:        error.failure.into(),
								invocation_id:  String::new(),
								exec:           Bytes::new(),
								process_name:   String::new(),
								skipped_events: error.skipped_events,
								message:        error.message.to_string(),
								props:          Default::default(),
							}),
						))
						.await;
					break;
				},
			}
		}
		let _ = finished
			.send_async(Finished { request_id, invocation_id: None })
			.await;
	});
}

fn spawn_lsp_events(
	request_id: u64,
	events: LspEvents,
	cancel: CancellationToken,
	responses: flume::Sender<pb::ServerFrame>,
	finished: flume::Sender<Finished>,
) {
	tokio::spawn(async move {
		loop {
			let event = tokio::select! {
				() = cancel.cancelled() => break,
				event = events.next_event() => event,
			};
			match event {
				Ok(event) => {
					let body = match event {
						LspRegistryEvent::Event(event) => pb::data_event::Body::Lsp(event),
						LspRegistryEvent::Binding(event) => pb::data_event::Body::LspBinding(event),
					};
					if responses
						.send_async(server_frame(
							request_id,
							server_frame::Body::DataEvent(pb::DataEvent {
								body:  Some(body),
								props: Default::default(),
							}),
						))
						.await
						.is_err()
					{
						break;
					}
				},
				Err(error) => {
					let _ = responses
						.send_async(server_frame(
							request_id,
							server_frame::Body::EventStreamError(pb::EventStreamError {
								stream:         pb::EventStreamKind::LspRegistry.into(),
								failure:        error.failure.into(),
								invocation_id:  String::new(),
								exec:           Bytes::new(),
								process_name:   String::new(),
								skipped_events: error.skipped_events,
								message:        error.message.to_string(),
								props:          Default::default(),
							}),
						))
						.await;
					break;
				},
			}
		}
		let _ = finished
			.send_async(Finished { request_id, invocation_id: None })
			.await;
	});
}

fn spawn_workspace_walk(
	request_id: u64,
	workspace: WorkspaceHost,
	request: WalkRequest,
	cancel: CancellationToken,
	responses: flume::Sender<pb::ServerFrame>,
	finished: flume::Sender<Finished>,
) {
	tokio::task::spawn_blocking(move || {
		let mut emitted = 0_u64;
		let result = workspace.walk_stream(&request, &cancel, |entry| {
			if cancel.is_cancelled() {
				return ControlFlow::Break(());
			}
			let kind = match entry.file_type {
				FileType::File => document_pb::FileKind::RegularFile,
				FileType::Dir => document_pb::FileKind::Directory,
				FileType::Symlink => document_pb::FileKind::SymbolicLink,
			};
			let event = pb::data_event::Body::WalkEntry(pb::WalkEntry {
				path:     entry.relative_path.to_owned(),
				kind:     kind as i32,
				mtime_ms: entry.mtime,
				size:     entry.size,
				depth:    u64::try_from(entry.depth).unwrap_or(u64::MAX),
				props:    Default::default(),
			});
			if send_data_event_sync(&responses, request_id, event) {
				emitted += 1;
				ControlFlow::Continue(())
			} else {
				ControlFlow::Break(())
			}
		});
		match result {
			Ok(status) if !cancel.is_cancelled() => {
				let _ = send_data_event_sync(
					&responses,
					request_id,
					pb::data_event::Body::WalkComplete(pb::WalkComplete {
						scanned_entries:  emitted,
						filtered_entries: 0,
						limited_entries:  u64::from(status == WalkStatus::Stopped),
						cache_age_ms:     0,
						cached:           false,
						props:            Default::default(),
					}),
				);
			},
			Ok(_) => {},
			Err(error) => {
				send_workspace_stream_error_sync(
					&responses,
					request_id,
					pb::EventStreamKind::Walk,
					&error,
				);
			},
		}
		let _ = finished.send(Finished { request_id, invocation_id: None });
	});
}

fn spawn_workspace_search(
	request_id: u64,
	workspace: WorkspaceHost,
	request: WalkRequest,
	options: WorkspaceSearchOwned,
	cancel: CancellationToken,
	responses: flume::Sender<pb::ServerFrame>,
	finished: flume::Sender<Finished>,
) {
	tokio::task::spawn_blocking(move || {
		let borrowed = WorkspaceSearchOptions {
			pattern: options.pattern.as_str(),
			case:    options.case,
			limit:   options.limit,
		};
		let result = workspace.search_stream(&request, &borrowed, &cancel, |matched| {
			if cancel.is_cancelled() {
				return ControlFlow::Break(());
			}
			if send_data_event_sync(
				&responses,
				request_id,
				pb::data_event::Body::SearchMatch(pb::SearchMatchMsg {
					path:        matched.path.to_string(),
					line:        matched.line,
					byte_offset: matched.byte_offset,
					line_bytes:  matched.line_bytes,
					props:       Default::default(),
				}),
			) {
				ControlFlow::Continue(())
			} else {
				ControlFlow::Break(())
			}
		});
		match result {
			Ok(outcome) if !cancel.is_cancelled() => {
				let _ = send_data_event_sync(
					&responses,
					request_id,
					pb::data_event::Body::SearchComplete(pb::SearchComplete {
						files_scanned: outcome.files_scanned,
						matches:       outcome.matches,
						limited:       outcome.limited,
						props:         Default::default(),
					}),
				);
			},
			Ok(_) => {},
			Err(error) => {
				send_workspace_stream_error_sync(
					&responses,
					request_id,
					pb::EventStreamKind::Search,
					&error,
				);
			},
		}
		let _ = finished.send(Finished { request_id, invocation_id: None });
	});
}

fn send_data_event_sync(
	responses: &flume::Sender<pb::ServerFrame>,
	request_id: u64,
	body: pb::data_event::Body,
) -> bool {
	responses
		.send(checked_server_frame(
			request_id,
			server_frame::Body::DataEvent(pb::DataEvent {
				body:  Some(body),
				props: Default::default(),
			}),
		))
		.is_ok()
}

fn send_workspace_stream_error_sync(
	responses: &flume::Sender<pb::ServerFrame>,
	request_id: u64,
	kind: pb::EventStreamKind,
	error: &WorkspaceError,
) {
	let _ = responses.send(checked_server_frame(
		request_id,
		server_frame::Body::EventStreamError(pb::EventStreamError {
			stream:         kind as i32,
			failure:        pb::EventStreamFailure::Closed as i32,
			invocation_id:  String::new(),
			exec:           Bytes::new(),
			process_name:   String::new(),
			skipped_events: 0,
			message:        error.to_string(),
			props:          Default::default(),
		}),
	));
}

fn spawn_blob_get(
	request_id: u64,
	read: super::blobs::BlobRead,
	cancel: CancellationToken,
	responses: flume::Sender<pb::ServerFrame>,
	finished: flume::Sender<Finished>,
) {
	tokio::spawn(async move {
		if read.data.is_empty() {
			let send = send_body(
				&responses,
				request_id,
				server_frame::Body::BlobChunk(blob_pb::Chunk {
					data: Bytes::new(),
					hash: Bytes::copy_from_slice(&read.id.hash),
					size: Some(read.id.size),
				}),
			);
			tokio::select! {
				() = cancel.cancelled() => {},
				() = send => {},
			}
		}
		let mut offset = 0;
		while offset < read.data.len() {
			let first = offset == 0;
			let end = (offset + BLOB_CHUNK_BYTES).min(read.data.len());
			let send = send_body(
				&responses,
				request_id,
				server_frame::Body::BlobChunk(blob_pb::Chunk {
					data: read.data.slice(offset..end),
					hash: if first {
						Bytes::copy_from_slice(&read.id.hash)
					} else {
						Bytes::new()
					},
					size: first.then_some(read.id.size),
				}),
			);
			tokio::select! {
				() = cancel.cancelled() => break,
				() = send => offset = end,
			}
		}
		if !cancel.is_cancelled() {
			send_body(
				&responses,
				request_id,
				server_frame::Body::BlobGetComplete(pb::BlobGetComplete {
					hash:       Bytes::copy_from_slice(&read.id.hash),
					bytes_sent: read.data.len() as u64,
					props:      Default::default(),
				}),
			)
			.await;
		}
		let _ = finished
			.send_async(Finished { request_id, invocation_id: None })
			.await;
	});
}

fn worker_completion_json(
	complete: &super::worker::WorkerCompletion,
) -> Result<(Bytes, Option<omp_proto::thread::v1::Blob>, bool), Str> {
	let is_error = complete.kind != WorkerOutcomeKind::Ok;
	if let Some(blob) = &complete.details_blob {
		return Ok((Bytes::new(), Some(blob.clone()), is_error));
	}
	let details = complete
		.details_json
		.clone()
		.ok_or_else(|| Str::new_static("worker completion omitted structured details"))?;
	let json = match complete.kind {
		WorkerOutcomeKind::Ok | WorkerOutcomeKind::Faulted => {
			worker_verdict_json(details, is_error).map_err(|error| Str::from(error.to_string()))?
		},
		WorkerOutcomeKind::ArgsRejected => {
			let issue = complete
				.args_issue
				.as_ref()
				.ok_or_else(|| Str::new_static("worker omitted its argument issue"))?;
			let kind = issue
				.kind
				.parse()
				.map_err(|_| Str::new_static("worker argument issue kind is invalid"))?;
			let issue = ArgIssue {
				path: issue
					.path
					.iter()
					.map(|segment| ArgPath::Key(Str::from(segment.as_str())))
					.collect(),
				expected: Str::from(issue.expected.as_str()),
				kind,
				example: issue.example.as_deref().map(Str::from),
				found: issue.found.as_deref().map(Str::from),
			};
			Bytes::from(
				serde_json::to_vec(&CallOutcome::<serde_json::Value, serde_json::Value>::ArgsRejected(
					issue,
				))
				.map_err(|error| Str::from(error.to_string()))?,
			)
		},
		WorkerOutcomeKind::Aborted => {
			let abort: Abort =
				serde_json::from_slice(&details).map_err(|error| Str::from(error.to_string()))?;
			Bytes::from(
				serde_json::to_vec(&CallOutcome::<serde_json::Value, serde_json::Value>::aborted(
					abort,
				))
				.map_err(|error| Str::from(error.to_string()))?,
			)
		},
	};
	Ok((json, None, is_error))
}

fn worker_verdict_json(details: Bytes, is_error: bool) -> Result<Bytes, serde_json::Error> {
	let _: &serde_json::value::RawValue = serde_json::from_slice(&details)?;
	let prefix: &[u8] = if is_error {
		br#"{"kind":"faulted","value":"#
	} else {
		br#"{"kind":"ok","value":"#
	};
	let mut verdict = BytesMut::with_capacity(prefix.len() + details.len() + 1);
	verdict.extend_from_slice(prefix);
	verdict.extend_from_slice(&details);
	verdict.extend_from_slice(b"}");
	Ok(verdict.freeze())
}

fn erased_outcome_wire(outcome: ErasedOutcome) -> (Bytes, bool, bool) {
	match outcome {
		ErasedOutcome::Done { verdict, useless } => {
			let is_error =
				serde_json::from_slice::<CallOutcome<serde_json::Value, serde_json::Value>>(&verdict)
					.map_or(true, |verdict| !matches!(verdict, CallOutcome::Ok(_)));
			(verdict, is_error, useless)
		},
		ErasedOutcome::Detached(job) => {
			let json = serde_json::to_vec(
				&ToolTerminal::<serde_json::Value, serde_json::Value>::Detached(job),
			)
			.map(Bytes::from)
			.unwrap_or_default();
			(json, false, false)
		},
	}
}
async fn send_workspace_operation_error(
	responses: &flume::Sender<pb::ServerFrame>,
	request_id: u64,
	error: &WorkspaceOperationError,
) {
	match error {
		WorkspaceOperationError::Document(error) => {
			send_document_error(responses, request_id, error).await;
		},
		WorkspaceOperationError::Blob(error) => {
			send_blob_error(responses, request_id, error).await;
		},
		WorkspaceOperationError::WorktreeNotFound(_) => {
			send_error(responses, request_id, pb::ProtocolErrorCode::NotFound, &error.to_string())
				.await;
		},
		WorkspaceOperationError::CowUnsupported => {
			send_error(responses, request_id, pb::ProtocolErrorCode::Unsupported, &error.to_string())
				.await;
		},
		WorkspaceOperationError::OutsideRoot
		| WorkspaceOperationError::InvalidGeneration(_)
		| WorkspaceOperationError::InvalidWorktreeName => {
			send_error(
				responses,
				request_id,
				pb::ProtocolErrorCode::InvalidArgument,
				&error.to_string(),
			)
			.await;
		},
		WorkspaceOperationError::Workspace(_) | WorkspaceOperationError::Io(_) => {
			send_error(responses, request_id, pb::ProtocolErrorCode::Internal, &error.to_string())
				.await;
		},
	}
}
fn workspace_walk_request(
	workspace: &WorkspaceHost,
	request: &pb::WalkRequest,
) -> Result<WalkRequest, (pb::ProtocolErrorCode, String)> {
	let root = if request.root_uri.is_empty() {
		workspace.root().to_path_buf()
	} else {
		Url::parse(&request.root_uri)
			.map_err(|error| {
				(
					pb::ProtocolErrorCode::InvalidArgument,
					format!("walk root is not a valid URI: {error}"),
				)
			})?
			.to_file_path()
			.map_err(|()| {
				(pb::ProtocolErrorCode::InvalidArgument, "walk root is not a local file URI".to_owned())
			})?
	};
	if !request.exclude.is_empty() {
		return Err((
			pb::ProtocolErrorCode::Unsupported,
			"walk exclude globs are not implemented".to_owned(),
		));
	}
	let options = request.options.as_ref();
	let follow_links = match options
		.map(|options| pb::WalkFollowLinks::try_from(options.follow_links))
		.transpose()
		.map_err(|_| {
			(pb::ProtocolErrorCode::InvalidArgument, "walk follow_links value is invalid".to_owned())
		})?
		.unwrap_or(pb::WalkFollowLinks::Never)
	{
		pb::WalkFollowLinks::Unspecified | pb::WalkFollowLinks::Never => FollowLinks::Never,
		pb::WalkFollowLinks::Roots => FollowLinks::Roots,
		pb::WalkFollowLinks::Always => FollowLinks::Always,
	};
	let detail = match options
		.map(|options| pb::WalkDetail::try_from(options.detail))
		.transpose()
		.map_err(|_| {
			(pb::ProtocolErrorCode::InvalidArgument, "walk detail value is invalid".to_owned())
		})?
		.unwrap_or(pb::WalkDetail::Minimal)
	{
		pb::WalkDetail::Unspecified | pb::WalkDetail::Minimal => WalkDetail::Minimal,
		pb::WalkDetail::Full => WalkDetail::Full,
	};
	let order = match options
		.map(|options| pb::WalkOrder::try_from(options.order))
		.transpose()
		.map_err(|_| {
			(pb::ProtocolErrorCode::InvalidArgument, "walk order value is invalid".to_owned())
		})?
		.unwrap_or(pb::WalkOrder::Path)
	{
		pb::WalkOrder::Unspecified | pb::WalkOrder::Path => WalkOrder::Path,
		pb::WalkOrder::Native => WalkOrder::Unordered,
	};
	let directory_errors = match options
		.map(|options| pb::DirectoryErrorMode::try_from(options.directory_errors))
		.transpose()
		.map_err(|_| {
			(
				pb::ProtocolErrorCode::InvalidArgument,
				"walk directory_errors value is invalid".to_owned(),
			)
		})?
		.unwrap_or(pb::DirectoryErrorMode::SkipSkippable)
	{
		pb::DirectoryErrorMode::Unspecified | pb::DirectoryErrorMode::SkipSkippable => {
			DirectoryErrorMode::SkipSkippable
		},
		pb::DirectoryErrorMode::Visit => DirectoryErrorMode::Visit,
	};
	let options = options.cloned().unwrap_or_default();
	let mut walk = WalkRequest::from_options(root, WalkOptions {
		include_hidden: options.include_hidden,
		use_gitignore: options.use_gitignore,
		skip_git: options.skip_git,
		skip_node_modules: options.skip_node_modules,
		follow_links,
		detail,
		order,
		emit_root: options.emit_root,
		min_depth: usize::try_from(options.min_depth).unwrap_or(usize::MAX),
		max_depth: if options.max_depth == 0 {
			usize::MAX
		} else {
			usize::try_from(options.max_depth).unwrap_or(usize::MAX)
		},
		contents_first: options.contents_first,
		directory_errors,
		same_file_system: options.same_file_system,
		cache: options.cache,
	});
	if !request.include.is_empty() {
		let glob = CompiledWalkGlob::new(request.include.iter().cloned()).map_err(|error| {
			(pb::ProtocolErrorCode::InvalidArgument, format!("walk include glob is invalid: {error}"))
		})?;
		walk = walk.filter(WalkFilter::all().glob(glob));
	}
	if let Some(limit) = request.limit {
		walk = walk.limit(usize::try_from(limit).unwrap_or(usize::MAX));
	}
	Ok(walk)
}

async fn send_exec_error(
	responses: &flume::Sender<pb::ServerFrame>,
	request_id: u64,
	error: &ExecError,
) {
	let code = match error {
		ExecError::SessionNotFound | ExecError::RunNotFound | ExecError::ProcessNotFound(_) => {
			pb::ProtocolErrorCode::NotFound
		},
		ExecError::ProcessExists(_) => pb::ProtocolErrorCode::AlreadyExists,
		ExecError::UnsupportedSignal(_) => pb::ProtocolErrorCode::Unsupported,
		_ => pb::ProtocolErrorCode::Internal,
	};
	send_error(responses, request_id, code, &error.to_string()).await;
}

fn worker_operation_allowed(operation: &str) -> bool {
	operation.starts_with("omp.env.docs.")
		|| operation.starts_with("omp.env.find.")
		|| matches!(
			operation,
			"omp.env.blobs.stat"
				| "omp.env.blobs.get"
				| "omp.env.blobs.put"
				| "omp.env.blobs.commit_put"
		)
}

async fn send_blob_error(
	responses: &flume::Sender<pb::ServerFrame>,
	request_id: u64,
	error: &BlobError,
) {
	let code = match error {
		BlobError::InvalidHash
		| BlobError::HashMismatch
		| BlobError::SizeMismatch { .. }
		| BlobError::InvalidRange
		| BlobError::LengthOverflow => pb::ProtocolErrorCode::InvalidArgument,
		BlobError::Store(omp_storage::blob::Error::NotFound) => pb::ProtocolErrorCode::NotFound,
		BlobError::Store(_) | BlobError::Remove(_) | BlobError::FinalizeTask(_) => {
			pb::ProtocolErrorCode::Internal
		},
	};
	send_error(responses, request_id, code, &error.to_string()).await;
}

fn connection_lease_id(target: Option<&document_pb::DocumentTarget>) -> Option<&Bytes> {
	let document_pb::document_target::Target::LeaseId(lease_id) = target?.target.as_ref()? else {
		return None;
	};
	Some(lease_id)
}

fn connection_lease<'a>(
	connection: &'a ConnectionState,
	target: Option<&document_pb::DocumentTarget>,
) -> Option<&'a DocumentLease> {
	let document_pb::document_target::Target::LeaseId(lease_id) = target?.target.as_ref()? else {
		return None;
	};
	connection.document_leases.get(lease_id)
}

async fn send_document_error(
	responses: &flume::Sender<pb::ServerFrame>,
	request_id: u64,
	error: &DocumentError,
) {
	let code = match error {
		DocumentError::Protocol { code, .. } => {
			match document_pb::ProtocolErrorCode::try_from(*code) {
				Ok(document_pb::ProtocolErrorCode::InvalidArgument) => {
					pb::ProtocolErrorCode::InvalidArgument
				},
				Ok(document_pb::ProtocolErrorCode::NotFound) => pb::ProtocolErrorCode::NotFound,
				Ok(document_pb::ProtocolErrorCode::PermissionDenied) => {
					pb::ProtocolErrorCode::PermissionDenied
				},
				Ok(document_pb::ProtocolErrorCode::Unsupported) => pb::ProtocolErrorCode::Unsupported,
				Ok(document_pb::ProtocolErrorCode::AlreadyExists) => {
					pb::ProtocolErrorCode::AlreadyExists
				},

				Ok(document_pb::ProtocolErrorCode::Cancelled) => pb::ProtocolErrorCode::Cancelled,
				Ok(
					document_pb::ProtocolErrorCode::RevisionExpired
					| document_pb::ProtocolErrorCode::PreconditionFailed
					| document_pb::ProtocolErrorCode::ContentModified,
				) => pb::ProtocolErrorCode::PreconditionFailed,
				_ => pb::ProtocolErrorCode::Internal,
			}
		},
		DocumentError::Cancelled => pb::ProtocolErrorCode::Cancelled,
		DocumentError::Disconnected => pb::ProtocolErrorCode::Internal,
		DocumentError::MalformedResponse(_) => pb::ProtocolErrorCode::InvalidArgument,
		DocumentError::Wire(_) => pb::ProtocolErrorCode::Internal,
	};
	send_error(responses, request_id, code, &error.to_string()).await;
}

const fn frame_data_operation(body: &client_frame::Body) -> Option<(&'static str, &'static str)> {
	match body {
		client_frame::Body::OpenSession(_) => Some(("omp.env.sh.open_session", "env.exec")),
		client_frame::Body::CloseSession(_) => Some(("omp.env.sh.close_session", "env.exec")),
		client_frame::Body::Exec(_) => Some(("omp.env.sh.exec", "env.exec")),
		client_frame::Body::Stdin(_) => Some(("omp.env.sh.stdin", "env.exec")),
		client_frame::Body::Signal(_) => Some(("omp.env.sh.signal", "env.exec")),
		client_frame::Body::Resize(_) => Some(("omp.env.sh.resize", "env.exec")),
		client_frame::Body::StartProcess(_) => Some(("omp.env.proc.start", "env.process")),
		client_frame::Body::ListProcesses(_) => Some(("omp.env.proc.list", "env.process")),
		client_frame::Body::AttachOutput(_) => Some(("omp.env.proc.attach", "env.process")),
		client_frame::Body::SendInput(_) => Some(("omp.env.proc.send_input", "env.process")),
		client_frame::Body::SignalProcess(_) => Some(("omp.env.proc.signal", "env.process")),
		client_frame::Body::StopProcess(_) => Some(("omp.env.proc.stop", "env.process")),
		client_frame::Body::BlobStat(_) => Some(("omp.env.blobs.stat", "env.blob")),
		client_frame::Body::BlobGet(_) => Some(("omp.env.blobs.get", "env.blob")),
		client_frame::Body::BlobPutChunk(_) => Some(("omp.env.blobs.put", "env.blob")),
		client_frame::Body::BlobPutCommit(_) => Some(("omp.env.blobs.commit_put", "env.blob")),
		client_frame::Body::BlobDelete(_) => Some(("omp.env.blobs.delete", "env.blob")),
		_ => None,
	}
}

async fn authorize_data_operation(
	connection: &ConnectionState,
	scope: Option<&pb::InvocationScope>,
	operation: &'static str,
	capability: &'static str,
	responses: &flume::Sender<pb::ServerFrame>,
	request_id: u64,
) -> bool {
	let Some(spec) = omp_tool::operation_spec(operation) else {
		send_error(
			responses,
			request_id,
			pb::ProtocolErrorCode::Unsupported,
			"DATA operation has no canonical OperationSpec",
		)
		.await;
		return false;
	};
	if spec.authority != omp_tool::Authority::Environment {
		send_error(
			responses,
			request_id,
			pb::ProtocolErrorCode::PermissionDenied,
			"DATA operation is not Environment-authoritative",
		)
		.await;
		return false;
	}
	if !connection.grants(capability) {
		send_policy_error(responses, request_id, PolicyError::Denied { capability }).await;
		return false;
	}
	if scope.is_none() {
		return true;
	}
	if !spec
		.minimum_phase
		.has_reached(omp_core::InvocationPhase::EffectsAuthorized)
	{
		send_error(
			responses,
			request_id,
			pb::ProtocolErrorCode::Internal,
			"DATA OperationSpec does not enforce EFFECTS_AUTHORIZED",
		)
		.await;
		return false;
	}
	let Some(scope) = scope else {
		send_policy_error(responses, request_id, PolicyError::EffectsNotAuthorized).await;
		return false;
	};
	let Some(host) = &connection.host else {
		send_policy_error(responses, request_id, PolicyError::EffectsNotAuthorized).await;
		return false;
	};
	let worker_scope = connection
		.authority
		.is_worker_invocation(host, &scope.invocation_id);
	let credentials = DataAuthority {
		invocation_id:      &scope.invocation_id,
		effect_token:       &scope.effect_token,
		host_generation:    scope.host_generation,
		session_generation: scope.session_generation,
	};
	let result = if capability == "env.search" {
		connection
			.authority
			.validate_read(host, connection.connection_owner, credentials)
	} else {
		connection
			.authority
			.validate(host, connection.connection_owner, credentials, capability)
	};
	if let Err(error) = result {
		send_policy_error(responses, request_id, error).await;
		return false;
	}
	if worker_scope && !worker_operation_allowed(operation) {
		send_policy_error(responses, request_id, PolicyError::Denied { capability }).await;
		return false;
	}
	true
}

async fn send_policy_error(
	responses: &flume::Sender<pb::ServerFrame>,
	request_id: u64,
	error: PolicyError,
) {
	if let PolicyError::QuotaExceeded { quota, limit, used } = &error {
		let props = omp_proto::inference::v1::ValueMap {
			fields: BTreeMap::from([
				("quota".to_owned(), omp_proto::inference::v1::Value {
					kind: Some(omp_proto::inference::v1::value::Kind::String((*quota).to_owned())),
				}),
				("limit".to_owned(), omp_proto::inference::v1::Value {
					kind: Some(omp_proto::inference::v1::value::Kind::Int(*limit as i64)),
				}),
				("used".to_owned(), omp_proto::inference::v1::Value {
					kind: Some(omp_proto::inference::v1::value::Kind::Int(*used as i64)),
				}),
			]),
		};
		send_body(
			responses,
			request_id,
			server_frame::Body::Error(pb::ProtocolError {
				code:    pb::ProtocolErrorCode::ResourceExhausted.into(),
				message: format!("QuotaExceeded: quota={quota}"),
				props:   Some(props),
			}),
		)
		.await;
		return;
	}
	let (code, message) = match error {
		PolicyError::EffectsNotAuthorized => (
			pb::ProtocolErrorCode::Uncommitted,
			Str::new_static("omp.EffectsNotAuthorized: invocation has not reached EFFECTS_AUTHORIZED"),
		),
		PolicyError::Denied { capability } => (
			pb::ProtocolErrorCode::PermissionDenied,
			Str::from(format!(
				"Denied: effect envelope does not grant {capability}; escalation is not re-prompted"
			)),
		),
		PolicyError::InvalidEffectToken => (
			pb::ProtocolErrorCode::PermissionDenied,
			Str::new_static(
				"Denied: effect token is absent, mismatched, revoked, or connection-bound",
			),
		),
		PolicyError::StaleGeneration => (
			pb::ProtocolErrorCode::PreconditionFailed,
			Str::new_static("StaleGeneration: host or session generation is stale"),
		),
		PolicyError::LeaseNotOwned => (
			pb::ProtocolErrorCode::PermissionDenied,
			Str::new_static("Denied: document lease belongs to another connection"),
		),
		PolicyError::EnforcementUnavailable => (
			pb::ProtocolErrorCode::Unsupported,
			Str::new_static(
				"EnforcementUnavailable: sandbox ENFORCE is deferred; refusing instead of degrading",
			),
		),
		PolicyError::QuotaExceeded { .. } => unreachable!("quota errors returned above"),
	};
	send_error(responses, request_id, code, message.as_str()).await;
}
async fn send_unsupported_data(
	responses: &flume::Sender<pb::ServerFrame>,
	request_id: u64,
	operation: &str,
) {
	send_error(
		responses,
		request_id,
		pb::ProtocolErrorCode::Unsupported,
		&format!("{operation} are not implemented by this environment"),
	)
	.await;
}

async fn send_data_response(
	responses: &flume::Sender<pb::ServerFrame>,
	request_id: u64,
	body: pb::data_response::Body,
) {
	send_body(
		responses,
		request_id,
		server_frame::Body::Data(pb::DataResponse { body: Some(body), props: Default::default() }),
	)
	.await;
}

async fn send_stream_error(
	responses: &flume::Sender<pb::ServerFrame>,
	request_id: u64,
	kind: pb::EventStreamKind,
	invocation_id: &str,
	exec: &[u8],
	process_name: &str,
	message: &str,
) {
	send_body(
		responses,
		request_id,
		server_frame::Body::EventStreamError(pb::EventStreamError {
			stream:         kind as i32,
			failure:        pb::EventStreamFailure::Closed as i32,
			invocation_id:  invocation_id.to_owned(),
			exec:           Bytes::copy_from_slice(exec),
			process_name:   process_name.to_owned(),
			skipped_events: 0,
			message:        message.to_owned(),
			props:          Default::default(),
		}),
	)
	.await;
}

async fn send_error(
	responses: &flume::Sender<pb::ServerFrame>,
	request_id: u64,
	code: pb::ProtocolErrorCode,
	message: &str,
) {
	send_body(
		responses,
		request_id,
		server_frame::Body::Error(pb::ProtocolError {
			code:    code as i32,
			message: message.to_owned(),
			props:   Default::default(),
		}),
	)
	.await;
}

async fn send_body(
	responses: &flume::Sender<pb::ServerFrame>,
	request_id: u64,
	body: server_frame::Body,
) {
	let _ = responses
		.send_async(checked_server_frame(request_id, body))
		.await;
}

async fn send_invocation_body(
	responses: &flume::Sender<pb::ServerFrame>,
	request_id: u64,
	body: server_frame::Body,
) -> bool {
	matches!(
		tokio::time::timeout(
			INVOCATION_RESPONSE_SEND_GRACE,
			responses.send_async(checked_server_frame(request_id, body)),
		)
		.await,
		Ok(Ok(()))
	)
}

async fn send_invocation_terminal_body(
	responses: &flume::Sender<pb::ServerFrame>,
	request_id: u64,
	body: server_frame::Body,
) {
	let frame = checked_server_frame(request_id, body);
	let retry = frame.clone();
	if tokio::time::timeout(INVOCATION_RESPONSE_SEND_GRACE, responses.send_async(frame))
		.await
		.is_err()
	{
		let responses = responses.clone();
		tokio::spawn(async move {
			let _ = responses.send_async(retry).await;
		});
	}
}

fn checked_server_frame(request_id: u64, body: server_frame::Body) -> pb::ServerFrame {
	let mut frame = server_frame(request_id, body);
	if frame.encoded_len() > FRAME_LIMIT {
		frame = server_frame(
			request_id,
			server_frame::Body::Error(pb::ProtocolError {
				code:    pb::ProtocolErrorCode::Internal as i32,
				message: "environment response exceeds the configured frame limit".to_owned(),
				props:   Default::default(),
			}),
		);
	}
	frame
}

fn server_frame(request_id: u64, body: server_frame::Body) -> pb::ServerFrame {
	pb::ServerFrame { request_id, body: Some(body), props: Default::default() }
}

async fn read_server_frame<R>(
	reader: &mut R,
	scratch: &mut BytesMut,
) -> io::Result<Option<pb::ServerFrame>>
where
	R: AsyncRead + Unpin,
{
	let Some(length) = read_length(reader).await? else {
		return Ok(None);
	};
	if length > FRAME_LIMIT {
		return Err(io::Error::new(
			io::ErrorKind::InvalidData,
			"environment frame exceeds the configured limit",
		));
	}
	scratch.clear();
	scratch.resize(length, 0);
	reader.read_exact(scratch).await?;
	pb::ServerFrame::decode(&scratch[..])
		.map(Some)
		.map_err(io::Error::other)
}

async fn write_client_frame<W>(
	writer: &mut W,
	frame: &pb::ClientFrame,
	scratch: &mut BytesMut,
) -> io::Result<()>
where
	W: AsyncWrite + Unpin,
{
	if frame.encoded_len() > FRAME_LIMIT {
		return Err(io::Error::new(
			io::ErrorKind::InvalidData,
			"environment frame exceeds the configured limit",
		));
	}
	scratch.clear();
	frame
		.encode_length_delimited(&mut *scratch)
		.map_err(io::Error::other)?;
	writer.write_all(scratch).await?;
	writer.flush().await
}
async fn read_client_frame<R>(
	reader: &mut R,
	scratch: &mut BytesMut,
) -> io::Result<Option<pb::ClientFrame>>
where
	R: AsyncRead + Unpin,
{
	let Some(length) = read_length(reader).await? else {
		return Ok(None);
	};
	if length > FRAME_LIMIT {
		return Err(io::Error::new(
			io::ErrorKind::InvalidData,
			"environment frame exceeds the configured limit",
		));
	}
	scratch.clear();
	scratch.resize(length, 0);
	reader.read_exact(scratch).await?;
	pb::ClientFrame::decode(&scratch[..])
		.map(Some)
		.map_err(io::Error::other)
}

async fn write_server_frame<W>(
	writer: &mut W,
	frame: &pb::ServerFrame,
	scratch: &mut BytesMut,
) -> io::Result<()>
where
	W: AsyncWrite + Unpin,
{
	if frame.encoded_len() > FRAME_LIMIT {
		return Err(io::Error::new(
			io::ErrorKind::InvalidData,
			"environment frame exceeds the configured limit",
		));
	}
	scratch.clear();
	frame
		.encode_length_delimited(&mut *scratch)
		.map_err(io::Error::other)?;
	writer.write_all(scratch).await?;
	writer.flush().await
}

async fn read_length<R>(reader: &mut R) -> io::Result<Option<usize>>
where
	R: AsyncRead + Unpin,
{
	let mut value = 0_u64;
	for shift in (0..70).step_by(7) {
		let mut byte = [0_u8; 1];
		match reader.read_exact(&mut byte).await {
			Ok(_) => {},
			Err(error) if error.kind() == io::ErrorKind::UnexpectedEof && shift == 0 => {
				return Ok(None);
			},
			Err(error) => return Err(error),
		}
		let part = u64::from(byte[0] & 0x7f);
		if shift == 63 && part > 1 {
			return Err(io::Error::new(
				io::ErrorKind::InvalidData,
				"invalid environment frame length",
			));
		}
		value |= part << shift;
		if byte[0] & 0x80 == 0 {
			return usize::try_from(value).map(Some).map_err(io::Error::other);
		}
	}
	Err(io::Error::new(io::ErrorKind::InvalidData, "invalid environment frame length"))
}

/// Assembles and runs the standalone environment daemon with the production
/// built-in registry.
#[cfg(unix)]
pub async fn run(args: EnvdArgs) -> Result<(), EnvdError> {
	run_with_registry(args, Registry::new()).await
}

/// Assembles production dispatch plus caller-provided tool revisions.
#[cfg(unix)]
pub async fn run_with_registry(args: EnvdArgs, registry: Registry) -> Result<(), EnvdError> {
	let workspace = WorkspaceHost::open(&args.root)?;
	let root = workspace.root().to_path_buf();
	let data_dir = crate::cli::data_dir(None)
		.map_err(|error| io::Error::new(io::ErrorKind::NotFound, error.to_string()))?;
	let settings = crate::settings::Settings::load(&data_dir);
	let interrupt_grace = settings.runtime_durations().interrupt_grace;
	let state_dir = if let Some(path) = args.state_dir {
		path
	} else {
		crate::project_state::directory(&data_dir, &root)?
	};
	ensure_directory(&state_dir)?;
	let socket = args
		.socket
		.unwrap_or_else(|| crate::project_state::environment_socket(&state_dir));
	let docserver_socket = if let Some(socket) = args.docserver_socket {
		socket
	} else {
		let socket = crate::project_state::document_socket(&state_dir);
		ensure_document_socket_free(&socket).await?;
		socket
	};
	let (principal_authority, session_id, session_generation) =
		super::authenticated_runtime_identity()?;
	let mut ext_host_config = ExtHostConfig::current(
		principal_authority.principal().clone(),
		session_id.clone(),
		session_generation,
	)?;
	ext_host_config.interrupt_grace = interrupt_grace;
	let mut extension_bindings = Vec::new();
	if args.py_eval {
		let key = HostKey::new("workspace", "trusted", crate::envd::worker::PY_EVAL_MODULE);
		let binding = ExtensionDataBinding::built_in(
			&state_dir,
			key.clone(),
			session_id.as_str(),
			session_generation,
		);
		let mut digest = blake3::Hasher::new();
		digest.update(crate::build_id::current().as_bytes());
		digest.update(env!("CARGO_PKG_VERSION").as_bytes());
		digest.update(crate::envd::worker::PY_EVAL_MODULE.as_bytes());
		let provenance = omp_core::Provenance::new(
			Str::new_static("omp-first-party"),
			Str::new_static(crate::envd::worker::PY_EVAL_MODULE),
			Str::new_static(env!("CARGO_PKG_VERSION")),
			omp_core::ArtifactDigest::new(*digest.finalize().as_bytes()),
			Str::new_static("workspace"),
			Str::new_static("trusted"),
			1,
		);
		let manifest = crate::exthost::ExtensionManifest::py_eval(provenance, []);
		let mut spec = ExtHostSpec::new(key, manifest);
		spec.data_grants = binding.grants().clone();
		spec.data_socket = Some(binding.path().to_path_buf());
		ext_host_config.extensions.push(spec);
		extension_bindings.push(binding);
	}
	let (env_connections, env_connection_rx) = tokio::sync::watch::channel(0);
	let (doc_connections, doc_connection_rx) = tokio::sync::watch::channel(0);
	let server = Arc::new(
		EnvServer::open_project(
			&root,
			&state_dir,
			&docserver_socket,
			registry,
			ext_host_config,
			Some(doc_connections),
		)
		.await?,
	);
	let process_shutdown = CancellationToken::new();
	let signal = process_shutdown.clone();
	let signal_task = tokio::spawn(async move {
		let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
		match terminate.as_mut() {
			Ok(terminate) => {
				tokio::select! {
					_ = tokio::signal::ctrl_c() => {},
					_ = terminate.recv() => {},
				}
			},
			Err(_) => {
				let _ = tokio::signal::ctrl_c().await;
			},
		}
		signal.cancel();
	});
	let listener_shutdown = CancellationToken::new();
	let serve_shutdown = listener_shutdown.clone();
	let serve_socket = socket.clone();
	let serve_server = Arc::clone(&server);
	let mut serve_task = tokio::spawn(async move {
		serve_server
			.serve_uds(&serve_socket, serve_shutdown, Some(env_connections))
			.await
	});
	let mut extension_tasks = tokio::task::JoinSet::new();
	for binding in extension_bindings {
		let extension_server = Arc::clone(&server);
		let extension_shutdown = listener_shutdown.clone();
		extension_tasks.spawn(async move {
			extension_server
				.serve_extension_uds(binding, extension_shutdown)
				.await
		});
	}
	let idle_timeout = Duration::from_secs(args.idle_timeout);
	let idle = async move {
		if idle_timeout.is_zero() {
			std::future::pending::<()>().await;
		} else {
			wait_idle(env_connection_rx, doc_connection_rx, 1, idle_timeout).await;
		}
	};
	tokio::pin!(idle);
	tokio::select! {
		() = process_shutdown.cancelled() => {
			listener_shutdown.cancel();
			serve_task.await??;
		},
		() = &mut idle => {
			listener_shutdown.cancel();
			serve_task.await??;
		},
		result = &mut serve_task => {
			result??;
			tokio::select! {
				() = process_shutdown.cancelled() => {},
				() = &mut idle => {},
			}
		},
	}
	listener_shutdown.cancel();
	while let Some(result) = extension_tasks.join_next().await {
		result??;
	}
	signal_task.abort();
	Ok(())
}

#[cfg(unix)]
async fn wait_idle(
	mut env: tokio::sync::watch::Receiver<usize>,
	mut docs: tokio::sync::watch::Receiver<usize>,
	reserved_docs: usize,
	timeout: Duration,
) {
	let mut env_open = true;
	let mut docs_open = true;
	loop {
		while *env.borrow() != 0 || *docs.borrow() > reserved_docs {
			tokio::select! {
				result = env.changed(), if env_open => env_open = result.is_ok(),
				result = docs.changed(), if docs_open => docs_open = result.is_ok(),
				else => std::future::pending::<()>().await,
			}
		}
		let idle = tokio::time::sleep(timeout);
		tokio::pin!(idle);
		loop {
			tokio::select! {
				() = &mut idle => return,
				result = env.changed(), if env_open => {
					env_open = result.is_ok();
					if *env.borrow() != 0 || *docs.borrow() > reserved_docs {
						break;
					}
				},
				result = docs.changed(), if docs_open => {
					docs_open = result.is_ok();
					if *env.borrow() != 0 || *docs.borrow() > reserved_docs {
						break;
					}
				},
			}
		}
	}
}
/// Reports the transport limitation on platforms without a local IPC backend.
#[cfg(not(any(unix, windows)))]
pub async fn run(_args: EnvdArgs) -> Result<(), EnvdError> {
	Err(
		io::Error::new(io::ErrorKind::Unsupported, "envd requires a Unix-domain socket in Phase 1")
			.into(),
	)
}

#[cfg(unix)]
async fn connect_or_start_docserver(
	root: &Path,
	socket: &Path,
	connections: Option<tokio::sync::watch::Sender<usize>>,
) -> Result<(DocumentHost, Option<DocumentAuthority>), EnvdError> {
	if let Ok(stream) = tokio::net::UnixStream::connect(socket).await {
		let documents = DocumentHost::connect(stream).await?;
		if crate::build_id::is_stale(
			crate::build_id::current(),
			documents.hello().server_build.as_str(),
		) {
			tracing::warn!(
				socket = %socket.display(),
				"stale-build document daemon owns the socket and will be replaced once it drains"
			);
		}
		return Ok((documents, None));
	}
	if let Some(parent) = socket.parent() {
		ensure_directory(parent)?;
	}

	let shutdown = CancellationToken::new();
	let task_shutdown = shutdown.clone();
	let task_root = root.to_path_buf();
	let task_socket = socket.to_path_buf();
	let task = tokio::spawn(async move {
		omp_docserver::daemon::serve(
			task_root,
			omp_docserver::daemon::Transport::Socket(task_socket),
			omp_docserver::daemon::ServeOptions {
				lsp_config_paths: Vec::new(),
				shutdown: Some(task_shutdown),
				server_build: Str::from(crate::build_id::current()),
				connections,
			},
		)
		.await
	});
	let mut authority = DocumentAuthority { shutdown, task: Some(task) };
	let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
	loop {
		if let Some(result) = authority.finished_result().await {
			match result? {
				Ok(()) => return Err(EnvdError::DocserverExited),
				Err(error) => return Err(EnvdError::Document(Str::from(error.to_string()))),
			}
		}
		if let Ok(stream) = tokio::net::UnixStream::connect(socket).await {
			let documents = DocumentHost::connect(stream).await?;
			return Ok((documents, Some(authority)));
		}
		if tokio::time::Instant::now() >= deadline {
			return Err(
				io::Error::new(io::ErrorKind::TimedOut, "document-server hello timed out").into(),
			);
		}
		tokio::time::sleep(Duration::from_millis(25)).await;
	}
}

#[cfg(windows)]
async fn connect_or_start_docserver(
	root: &Path,
	socket: &Path,
	connections: Option<tokio::sync::watch::Sender<usize>>,
) -> Result<(DocumentHost, Option<DocumentAuthority>), EnvdError> {
	if let Ok(stream) = omp_docserver::windows::connect_owner_pipe(socket) {
		let documents = DocumentHost::connect(stream).await?;
		return Ok((documents, None));
	}
	let listener = omp_docserver::windows::OwnerPipeListener::bind(socket)?;
	let config = omp_docserver::ServerConfig::new(root)
		.map_err(|error| EnvdError::Document(Str::from(error.to_string())))?
		.with_server_build(crate::build_id::current());
	let environment = omp_docserver::Environment::new(config)
		.map_err(|error| EnvdError::Document(Str::from(error.to_string())))?;
	let shutdown = CancellationToken::new();
	let task_shutdown = shutdown.clone();
	let task = tokio::spawn(async move {
		omp_docserver::windows::serve_owner_pipe(
			environment,
			listener,
			omp_docserver::connection::ConnectionConfig::default(),
			task_shutdown,
			connections,
		)
		.await
	});
	let authority = DocumentAuthority { shutdown, task: Some(task) };
	let stream = omp_docserver::windows::connect_owner_pipe(socket)?;
	let documents = DocumentHost::connect(stream).await?;
	Ok((documents, Some(authority)))
}

/// Refuses standalone-daemon startup while another process serves the project
/// document authority.
///
/// A daemon must own its document authority: joining a foreign authority as a
/// client would chain daemon lifetimes across builds, keeping a draining
/// generation alive forever through the successor's own connection.
#[cfg(unix)]
async fn ensure_document_socket_free(socket: &Path) -> Result<(), EnvdError> {
	match tokio::net::UnixStream::connect(socket).await {
		Ok(_) => Err(EnvdError::DocumentAuthorityHeld),
		Err(_) => Ok(()),
	}
}

#[cfg(unix)]
fn ensure_directory(path: &Path) -> io::Result<()> {
	use std::os::unix::fs::PermissionsExt as _;

	std::fs::create_dir_all(path)?;
	std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}
#[cfg(all(test, unix))]
mod tests {
	use super::*;
	async fn test_connection(
		capabilities: &[&str],
	) -> (
		flume::Sender<pb::ClientFrame>,
		flume::Receiver<pb::ServerFrame>,
		tempfile::TempDir,
		tempfile::TempDir,
	) {
		let root = tempfile::tempdir().expect("workspace");
		let state = tempfile::tempdir().expect("state");
		let workspace = WorkspaceHost::open(root.path()).expect("workspace host");
		let document_config = omp_docserver::ServerConfig::new(root.path())
			.expect("document config")
			.with_server_build("envd-test");
		let document_environment =
			omp_docserver::Environment::new(document_config).expect("document environment");
		let (document_client, document_server) = tokio::io::duplex(64 * 1024);
		tokio::spawn(async move {
			let _ = omp_docserver::connection::serve_connection(
				document_environment,
				document_server,
				omp_docserver::connection::ConnectionConfig::default(),
			)
			.await;
		});
		let documents = DocumentHost::connect(document_client)
			.await
			.expect("document host");
		let hello = documents.hello().clone();
		let exec = ExecHost::new();
		let blobs = BlobHost::open(state.path().join("blobs")).expect("blob host");
		let sessions_index = Arc::new(
			SessionIndex::open(state.path().join("sessions.sqlite3")).expect("sessions index"),
		);
		let state_store =
			Arc::new(StateStore::open(state.path().join("state")).expect("state store"));
		let journal_external = ExternalJournalActor::spawn(
			Arc::clone(&sessions_index),
			Some(Arc::clone(&state_store)),
			blobs.clone(),
			Str::new_static("test-session"),
			Str::new_static("test-project"),
			Str::new_static("/test-project"),
		)
		.expect("external journal actor");
		let workspace_ops = WorkspaceOperations::open(
			workspace.clone(),
			documents.clone(),
			blobs.clone(),
			state.path().join("workspace-ops"),
		)
		.expect("workspace operations");
		let ext_hosts = ExtHostSupervisor::spawn(ExtHostConfig::new(
			PathBuf::from("unused"),
			omp_core::Principal::new(
				Str::new_static("test-principal"),
				Str::new_static("Test Principal"),
			),
			Str::new_static("test-session"),
			1,
		))
		.await
		.expect("empty extension supervisor");
		let server = Arc::new(EnvServer::new(
			ServerIdentity {
				workspace_id:   hello.workspace_id,
				root_uri:       hello.root_uri,
				server_epoch:   hello.server_epoch,
				server_version: Str::new_static("test"),
				server_build:   Str::new_static("envd-test"),
			},
			documents,
			None,
			exec,
			workspace,
			blobs.clone(),
			SiteMaterializer::open(state.path().join("ext"), blobs.store().clone())
				.expect("site materializer"),
			Arc::new(Registry::new()),
			workspace_ops,
			Arc::new(ext_hosts),
			Arc::new(SessionBridgeHost::new()),
			omp_tools::eval::EvalSessionControl::default(),
			sessions_index,
			journal_external,
			Arc::new(AuthorityTable::default()),
		));
		let host = HostKey::new("workspace", "sandboxed", "envd-test");
		let grants = Grants::supported(capabilities.iter().copied());
		server.authority.register_host(host.clone(), grants.clone());
		server
			.authority
			.open(host.clone(), Str::new_static("test-invocation"));
		server
			.authority
			.authorize(
				&host,
				"test-invocation",
				Bytes::from_static(b"test-effect-token"),
				grants.clone(),
				100,
				1,
				1,
			)
			.expect("authorize test invocation");
		let policy = ConnectionPolicy::extension(host, grants.iter());
		let (requests, request_rx) = flume::bounded(16);
		let (responses, response_rx) = flume::bounded(16);
		tokio::spawn(async move {
			server.serve_frames(request_rx, responses, policy).await;
		});
		requests
			.send_async(pb::ClientFrame {
				request_id: 0,
				body:       Some(client_frame::Body::Hello(pb::ClientHello {
					client:       "envd-test".to_owned(),
					schema_rev:   omp_proto::SCHEMA_REV,
					capabilities: capabilities
						.iter()
						.map(|capability| (*capability).to_owned())
						.collect(),
					client_id:    Bytes::new(),
					props:        Default::default(),
				})),
				props:      Default::default(),
				scope:      None,
			})
			.await
			.expect("send hello");
		assert!(matches!(
			response_rx.recv_async().await.expect("server hello").body,
			Some(server_frame::Body::Hello(_))
		));
		(requests, response_rx, root, state)
	}

	fn data_frame(request_id: u64, body: pb::data_request::Body) -> pb::ClientFrame {
		pb::ClientFrame {
			request_id,
			body: Some(client_frame::Body::Data(pb::DataRequest {
				body:  Some(body),
				props: Default::default(),
			})),
			scope: Some(pb::InvocationScope {
				invocation_id: "test-invocation".to_owned(),
				effect_token: Bytes::from_static(b"test-effect-token"),
				host_generation: 1,
				session_generation: 1,
				..Default::default()
			}),
			props: Default::default(),
		}
	}

	#[tokio::test]
	async fn extension_site_write_is_refused_even_with_grant() {
		let (requests, responses, root, _state) =
			test_connection(&["env.doc.read", "env.site"]).await;
		let path = root.path().join("sample.txt");
		std::fs::write(&path, b"hello document").expect("write document");
		let uri = Url::from_file_path(&path)
			.expect("document URI")
			.to_string();
		requests
			.send_async(data_frame(
				1,
				pb::data_request::Body::Document(pb::DocumentOp {
					op:    Some(pb::document_op::Op::Open(document_pb::OpenDocumentRequest {
						uri,
						language_id: "text".to_owned(),
					})),
					props: Default::default(),
				}),
			))
			.await
			.expect("send open");
		let opened = responses.recv_async().await.expect("open response");
		let Some(server_frame::Body::Data(pb::DataResponse {
			body:
				Some(pb::data_response::Body::Document(pb::DocumentResult {
					result: Some(pb::document_result::Result::Opened(opened)),
					..
				})),
			..
		})) = opened.body
		else {
			panic!("expected document open response");
		};
		let revision = opened.head.as_ref().and_then(|head| head.revision.clone());
		requests
			.send_async(data_frame(
				2,
				pb::data_request::Body::Document(pb::DocumentOp {
					op:    Some(pb::document_op::Op::Read(document_pb::ReadDocumentRequest {
						document: Some(document_pb::DocumentTarget {
							target: Some(document_pb::document_target::Target::LeaseId(opened.lease_id)),
						}),
						revision,
						selection: Some(document_pb::ReadSelection {
							selection: Some(document_pb::read_selection::Selection::Whole(
								document_pb::WholeDocument::default(),
							)),
						}),
					})),
					props: Default::default(),
				}),
			))
			.await
			.expect("send read");
		let read = responses.recv_async().await.expect("read response");
		assert!(matches!(
			read.body,
			Some(server_frame::Body::Data(pb::DataResponse {
				body: Some(pb::data_response::Body::Document(pb::DocumentResult {
					result: Some(pb::document_result::Result::Read(_)),
					..
				})),
				..
			}))
		));
		requests
			.send_async(data_frame(3, pb::data_request::Body::Site(pb::MaterializeSite::default())))
			.await
			.expect("send extension site write");
		let denied = responses.recv_async().await.expect("site refusal response");
		assert!(matches!(
			denied.body,
			Some(server_frame::Body::Error(pb::ProtocolError { code, .. }))
				if code == pb::ProtocolErrorCode::PermissionDenied as i32
		));
	}

	#[tokio::test]
	async fn data_walk_and_search_stream_incrementally_to_completion() {
		let (requests, responses, root, _state) = test_connection(&["env.walk", "env.search"]).await;
		std::fs::write(root.path().join("a.txt"), b"needle\n").expect("write first");
		std::fs::write(root.path().join("b.txt"), b"other needle\n").expect("write second");
		requests
			.send_async(data_frame(
				10,
				pb::data_request::Body::Walk(pb::WalkRequest {
					root_uri: String::new(),
					options:  None,
					include:  Vec::new(),
					exclude:  Vec::new(),
					limit:    None,
					props:    Default::default(),
				}),
			))
			.await
			.expect("send walk");
		let mut walk_entries = 0;
		loop {
			match responses.recv_async().await.expect("walk event").body {
				Some(server_frame::Body::DataEvent(pb::DataEvent {
					body: Some(pb::data_event::Body::WalkEntry(_)),
					..
				})) => walk_entries += 1,
				Some(server_frame::Body::DataEvent(pb::DataEvent {
					body: Some(pb::data_event::Body::WalkComplete(_)),
					..
				})) => break,
				other => panic!("unexpected walk frame: {other:?}"),
			}
		}
		assert!(walk_entries >= 2);
		requests
			.send_async(data_frame(
				11,
				pb::data_request::Body::Search(pb::SearchRequest {
					walk:           Some(pb::WalkRequest {
						root_uri: String::new(),
						options:  None,
						include:  Vec::new(),
						exclude:  Vec::new(),
						limit:    None,
						props:    Default::default(),
					}),
					pattern:        Bytes::from_static(b"needle"),
					case_sensitive: true,
					limit:          None,
					props:          Default::default(),
				}),
			))
			.await
			.expect("send search");
		let mut matches = 0;
		loop {
			match responses.recv_async().await.expect("search event").body {
				Some(server_frame::Body::DataEvent(pb::DataEvent {
					body: Some(pb::data_event::Body::SearchMatch(_)),
					..
				})) => matches += 1,
				Some(server_frame::Body::DataEvent(pb::DataEvent {
					body: Some(pb::data_event::Body::SearchComplete(_)),
					..
				})) => break,
				other => panic!("unexpected search frame: {other:?}"),
			}
		}
		assert_eq!(matches, 2);
	}

	#[test]
	fn connection_stream_state_and_cleanup_are_isolated() {
		let grants = Grants::supported(["env.search"]);
		let authority = Arc::new(AuthorityTable::default());
		let policy = ConnectionPolicy::in_process();
		let mut first =
			ConnectionState::new(ExecHost::new(), grants.clone(), Arc::clone(&authority), &policy);
		let second = ConnectionState::new(ExecHost::new(), grants, authority, &policy);
		let cancel = CancellationToken::new();
		first
			.requests
			.insert(41, RequestState::DataStream { cancel: cancel.clone() });
		assert!(!second.requests.contains_key(&41));
		let exec = first.exec_host.clone();
		first.cancel_all(&exec);
		let state = tempfile::tempdir().expect("binding state");
		let first_binding = ExtensionDataBinding::built_in(
			state.path(),
			HostKey::new("workspace", "trusted", "first"),
			"session",
			7,
		);
		let second_binding = ExtensionDataBinding::built_in(
			state.path(),
			HostKey::new("workspace", "trusted", "second"),
			"session",
			7,
		);
		assert_ne!(first_binding.path(), second_binding.path());
		assert!(first_binding.grants().contains("env.doc.read"));
		assert!(first_binding.grants().contains("env.search"));
		assert!(!first_binding.grants().contains("*"));
		assert!(cancel.is_cancelled());
		assert!(first.requests.is_empty());
	}

	#[tokio::test]
	async fn standalone_daemon_refuses_a_served_document_socket() {
		let scratch = tempfile::tempdir().expect("scratch socket directory");
		let socket = scratch.path().join("doc.sock");

		assert!(ensure_document_socket_free(&socket).await.is_ok(), "absent socket must be free");

		let listener = tokio::net::UnixListener::bind(&socket).expect("bind document socket");
		assert!(
			matches!(
				ensure_document_socket_free(&socket).await,
				Err(EnvdError::DocumentAuthorityHeld)
			),
			"live authority must refuse a second daemon"
		);

		// A stale socket file without a listener no longer refuses startup.
		drop(listener);
		tokio::time::timeout(Duration::from_secs(1), async {
			loop {
				if ensure_document_socket_free(&socket).await.is_ok() {
					break;
				}
				tokio::time::sleep(Duration::from_millis(1)).await;
			}
		})
		.await
		.expect("stale socket file did not become free");
	}
	#[tokio::test(start_paused = true)]
	async fn idle_wait_requires_one_continuous_quiet_window() {
		let (env_tx, env_rx) = tokio::sync::watch::channel(1);
		let (docs_tx, docs_rx) = tokio::sync::watch::channel(2);
		let busy = tokio::spawn(wait_idle(env_rx, docs_rx, 1, Duration::from_secs(10)));
		tokio::task::yield_now().await;
		tokio::time::advance(Duration::from_secs(20)).await;
		tokio::task::yield_now().await;
		assert!(!busy.is_finished(), "busy environment was considered idle");
		env_tx.send_replace(0);
		tokio::time::advance(Duration::from_secs(20)).await;
		tokio::task::yield_now().await;
		assert!(!busy.is_finished(), "external document client was considered idle");
		docs_tx.send_replace(1);
		tokio::task::yield_now().await;
		tokio::time::advance(Duration::from_secs(9)).await;
		tokio::task::yield_now().await;
		assert!(!busy.is_finished(), "idle wait resolved before its full window");
		tokio::time::advance(Duration::from_secs(1)).await;
		busy.await.expect("idle wait task");

		let (env_tx, env_rx) = tokio::sync::watch::channel(0);
		let (_docs_tx, docs_rx) = tokio::sync::watch::channel(1);
		let reset = tokio::spawn(wait_idle(env_rx, docs_rx, 1, Duration::from_secs(10)));
		tokio::task::yield_now().await;
		tokio::time::advance(Duration::from_secs(9)).await;
		env_tx.send_replace(1);
		tokio::task::yield_now().await;
		env_tx.send_replace(0);
		tokio::task::yield_now().await;
		tokio::time::advance(Duration::from_secs(9)).await;
		tokio::task::yield_now().await;
		assert!(!reset.is_finished(), "activity did not reset the idle window");
		tokio::time::advance(Duration::from_secs(1)).await;
		reset.await.expect("reset idle wait task");
	}

	#[tokio::test]
	async fn stale_document_authority_is_joined_without_replacement() {
		let root = tempfile::tempdir().expect("document workspace");
		let state = tempfile::tempdir().expect("document socket directory");
		let socket = state.path().join("document.sock");
		let shutdown = CancellationToken::new();
		let serve_shutdown = shutdown.clone();
		let serve_root = root.path().to_path_buf();
		let serve_socket = socket.clone();
		let task = tokio::spawn(async move {
			omp_docserver::daemon::serve(
				serve_root,
				omp_docserver::daemon::Transport::Socket(serve_socket),
				omp_docserver::daemon::ServeOptions {
					lsp_config_paths: Vec::new(),
					shutdown:         Some(serve_shutdown),
					server_build:     Str::new_static("stale-build"),
					connections:      None,
				},
			)
			.await
		});
		tokio::time::timeout(Duration::from_secs(2), async {
			loop {
				if let Ok(stream) = tokio::net::UnixStream::connect(&socket).await
					&& DocumentHost::connect(stream).await.is_ok()
				{
					break;
				}
				tokio::task::yield_now().await;
			}
		})
		.await
		.expect("stale document authority did not become ready");

		let (documents, authority) = connect_or_start_docserver(root.path(), &socket, None)
			.await
			.expect("join stale document authority");
		assert_eq!(documents.hello().server_build.as_str(), "stale-build");
		assert!(authority.is_none(), "joined authority was incorrectly claimed");

		drop(documents);
		shutdown.cancel();
		task
			.await
			.expect("stale authority task")
			.expect("stale authority shutdown");
	}
}
