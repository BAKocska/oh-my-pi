use std::{
	collections::HashMap,
	sync::{
		Arc, Weak,
		atomic::{AtomicU64, Ordering},
	},
};

use bytes::Bytes;
use flume::{Receiver, Sender};
use omp_core::{EnvPath, Str, sf};
use omp_proto::{
	blob::v1::{
		Chunk, DeleteRequest, DeleteResponse, GetRequest, PutResponse, StatRequest, StatResponse,
	},
	document::v1::{
		self as document, CommitTransactionRequest, DocumentEvent, DocumentHead,
		GetLspBindingsRequest, GetLspBindingsResponse, LspBindingEvent, LspEvent,
		OpenDocumentRequest, ReadDocumentRequest, ReadDocumentResponse, ReadSelection, Revision,
		TransactionCommitted, TransactionPartiallyCommitted, TransactionRejected,
		commit_transaction_response, document_target, read_document_response,
	},
	env::v1::{
		self as env_wire, Admission, AdmitInvocation, ArgText, ArgsCommitted, AttachOutput,
		BlobGetComplete, CancelRequest, ClientFrame, ClientHello, CloseSessionRequest,
		CloseSessionResponse, CommitBlobPut, CreateWorktree, DataEvent, DataRequest, DataResponse,
		DestroyWorktree, EventStreamError, EventStreamKind, ExecRequest, ExecStarted, ExitEvent,
		Interrupt, InvocationScope, InvokeAccepted, InvokeTool, ListProcesses, MaterializeSite,
		MergeWorktree, OpenSessionRequest, OpenSessionResponse, OutputAttached, OutputFrame,
		ProcessCommandAccepted, ProcessList, ProcessOutput, ProcessStarted, ProcessStateEvent,
		ProtocolError, ProtocolErrorCode, Retire, SearchComplete, SearchMatchMsg, SearchRequest,
		SendInput, ServerFrame, ServerHello, SignalProcess, SignalRequest, SiteMaterialized,
		StartProcess, StdinFrame, StopProcess, Update, Verdict, WalkComplete, WalkEntry, WalkRequest,
		WorktreeResult, cancel_request, client_frame, data_event, data_request, data_response,
		document_op, document_result, server_frame, worktree_op,
	},
};
use parking_lot::Mutex;
use thiserror::Error;
use url::Url;

use crate::{admit::Admitter, guard::RunGuard};

/// A client-side environment protocol failure.
#[derive(Debug, Error)]
pub enum ClientError {
	/// The frame transport closed before the operation completed.
	#[error("environment frame transport closed")]
	TransportClosed,
	/// All nonzero request identifiers have been consumed.
	#[error("environment request identifier space exhausted")]
	RequestIdExhausted,
	/// The server refused a DATA operation before effects were authorized.
	#[error("environment effects are not authorized: {0:?}")]
	EffectsNotAuthorized(ProtocolError),
	/// A correlated event stream permanently lost continuity.
	#[error("{0}")]
	StreamLost(StreamLost),
	/// The server rejected the request.
	#[error("environment protocol error: {0:?}")]
	Protocol(ProtocolError),
	/// The worker-scoped surface excludes this operation family.
	#[error("operation is unavailable on an invocation-scoped environment client")]
	ScopedOperationDenied,
	/// A typed environment path could not be resolved to a valid file URI.
	#[error("invalid environment path URI: {0}")]
	InvalidEnvPath(Str),
	/// A response did not have the body required by the typed operation.
	#[error("unexpected environment response while waiting for {expected}")]
	UnexpectedResponse {
		/// The response body expected by the operation.
		expected: &'static str,
	},
}

/// A terminal loss of event-stream continuity.
#[derive(Clone, Debug)]
pub struct StreamLost {
	/// The stream family whose ordered history is no longer contiguous.
	pub stream:          EventStreamKind,
	/// Number of events known to have been skipped.
	pub skipped:         u64,
	/// Server-supplied reason for the loss.
	pub reason:          Str,
	/// Resource-specific action required before work can safely continue.
	pub reopen_guidance: &'static str,
}

impl std::fmt::Display for StreamLost {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(
			formatter,
			"environment {:?} stream lost continuity after {} skipped events: {}; {}",
			self.stream, self.skipped, self.reason, self.reopen_guidance
		)
	}
}

/// Invocation-bound authority stamped on every scoped environment request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataScope {
	/// Stable invocation identifier shared across CONTROL, DATA, and the
	/// journal.
	pub invocation_id:      Str,
	/// Core-minted authorization token for this invocation.
	pub effect_token:       Bytes,
	/// Generation of the extension host issuing the request.
	pub host_generation:    u64,
	/// Generation of the owning session.
	pub session_generation: u64,
}

impl DataScope {
	/// Creates invocation-bound DATA authority.
	#[must_use]
	pub fn new(
		invocation_id: impl Into<Str>,
		effect_token: Bytes,
		host_generation: u64,
		session_generation: u64,
	) -> Self {
		Self {
			invocation_id: invocation_id.into(),
			effect_token,
			host_generation,
			session_generation,
		}
	}

	fn wire(&self) -> InvocationScope {
		InvocationScope {
			invocation_id: self.invocation_id.to_string(),
			effect_token: self.effect_token.clone(),
			host_generation: self.host_generation,
			session_generation: self.session_generation,
			..InvocationScope::default()
		}
	}
}

/// The client half of a transport-neutral bidirectional `env/v1` frame channel.
///
/// A UDS, mTLS, or other remote transport can decode frames into `incoming`
/// and encode frames received from `outgoing`. [`Self::in_process`] creates the
/// same boundary from flume channels for a colocated environment host.
#[derive(Clone, Debug)]
pub struct EnvClient {
	inner: Arc<ClientInner>,
}

/// Cloneable out-of-band control for one active environment execution.
///
/// Unlike [`ExecRun`], this handle does not own the execution stream or its
/// cancellation guard. It is safe to hand to a UI overlay: every operation is
/// resolved by the Environment from the opaque execution id and remains
/// generation-fenced by the invocation scope.
#[derive(Clone, Debug)]
pub struct ActiveExecControl {
	client: EnvClient,
	exec:   Bytes,
}

impl ActiveExecControl {
	/// Returns the opaque execution identity.
	#[must_use]
	pub fn exec_id(&self) -> &Bytes {
		&self.exec
	}

	/// Writes bytes to the active execution's stdin.
	pub async fn stdin(&self, data: Bytes) -> Result<bool, ClientError> {
		self
			.client
			.exec_live_control(env_wire::exec_session_op::Op::Stdin(StdinFrame {
				exec:  self.exec.clone(),
				input: Some(env_wire::stdin_frame::Input::Data(data)),
				props: None,
			}))
			.await
	}

	/// Closes the active execution's stdin.
	pub async fn eof(&self) -> Result<bool, ClientError> {
		self
			.client
			.exec_live_control(env_wire::exec_session_op::Op::Stdin(StdinFrame {
				exec:  self.exec.clone(),
				input: Some(env_wire::stdin_frame::Input::Eof(true)),
				props: None,
			}))
			.await
	}

	/// Resizes the active execution's pseudo-terminal.
	pub async fn resize(&self, rows: u32, columns: u32) -> Result<bool, ClientError> {
		self
			.client
			.exec_live_control(env_wire::exec_session_op::Op::Resize(env_wire::ResizeRequest {
				exec: self.exec.clone(),
				rows,
				columns,
				props: None,
			}))
			.await
	}

	/// Sends one named process-group signal.
	pub async fn signal(&self, signal: impl Into<String>) -> Result<bool, ClientError> {
		self
			.client
			.exec_live_control(env_wire::exec_session_op::Op::Signal(SignalRequest {
				exec:   self.exec.clone(),
				signal: signal.into(),
				props:  None,
			}))
			.await
	}

	/// Applies resource-owned graceful or forced cancellation.
	pub async fn cancel(
		&self,
		control: env_wire::ExecControlKind,
		grace_ms: u64,
	) -> Result<bool, ClientError> {
		Ok(self
			.client
			.exec_control(env_wire::ExecControlRequest {
				exec: self.exec.clone(),
				control: control as i32,
				grace_ms,
				wire_revision: omp_proto::SCHEMA_REV,
			})
			.await?
			.accepted)
	}
}
struct ClientInner {
	outgoing:     Sender<ClientFrame>,
	pending:      Mutex<HashMap<u64, Sender<ServerFrame>>>,
	hello_waiter: Mutex<Option<Sender<ServerFrame>>>,
	info:         Mutex<Option<ServerHello>>,
	events:       Receiver<ServerFrame>,
	next_id:      AtomicU64,
	cancel:       Sender<u64>,
	lease_close:  Sender<LeaseClose>,
	admitter:     Mutex<Option<Arc<dyn AdmissionDispatcher>>>,
}

trait AdmissionDispatcher: Send + Sync {
	fn dispatch(&self, client: Arc<ClientInner>, request_id: u64, query: AdmitInvocation);
}

struct AdmitterDispatcher<A> {
	admitter: Arc<A>,
	runtime:  tokio::runtime::Handle,
}
impl<A: Admitter> AdmissionDispatcher for AdmitterDispatcher<A> {
	fn dispatch(&self, client: Arc<ClientInner>, request_id: u64, query: AdmitInvocation) {
		let admitter = Arc::clone(&self.admitter);
		let invocation_id = query.invocation_id.clone();
		let task_client = Arc::clone(&client);
		drop(self.runtime.spawn(async move {
			let mut admission = admitter.admit(query).await;
			admission.invocation_id = invocation_id;
			let _ = task_client
				.outgoing
				.send_async(ClientFrame {
					request_id,
					body: Some(client_frame::Body::Admission(admission)),
					..ClientFrame::default()
				})
				.await;
		}));
	}
}

impl std::fmt::Debug for ClientInner {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter
			.debug_struct("ClientInner")
			.field("next_id", &self.next_id.load(Ordering::Relaxed))
			.finish_non_exhaustive()
	}
}

#[derive(Debug)]
struct LeaseClose {
	lease_id: Bytes,
	scope:    DataScope,
}

/// The server half of an in-process `env/v1` frame transport.
///
/// This type contains transport endpoints only. It does not implement or own
/// any environment resources.
#[derive(Debug)]
pub struct InProcessEnvTransport {
	requests:  Receiver<ClientFrame>,
	responses: Sender<ServerFrame>,
}

/// A correlated stream of raw server frames for one request.
#[derive(Debug)]
pub struct RequestStream {
	request_id: u64,
	receiver:   Receiver<ServerFrame>,
	client:     Weak<ClientInner>,
	finished:   bool,
}

/// One open tool invocation and its correlated event stream.
#[derive(Debug)]
pub struct Invocation {
	client: EnvClient,
	id:     Str,
	stream: RequestStream,
	guard:  Option<RunGuard>,
}

/// A typed event on a tool invocation stream.
#[derive(Debug)]
pub enum InvocationEvent {
	/// The environment accepted the invocation channel.
	Accepted(InvokeAccepted),
	/// Core must answer the environment's admission query before authorization.
	Admission(AdmitInvocation),
	/// Serialized typed progress from the executor.
	Update(Update),
	/// The terminal structured tool outcome and canonical model-facing parts.
	Verdict(Verdict),
}

/// One command running inside a server-owned exec session.
#[derive(Debug)]
pub struct ExecRun {
	client: EnvClient,
	scope:  Option<DataScope>,
	stream: RequestStream,
	guard:  Option<RunGuard>,
}

/// A typed event on an exec request stream.
#[derive(Debug)]
pub enum ExecEvent {
	/// The command was created and has an exec identifier.
	Started(ExecStarted),
	/// Ordered stdout, stderr, or PTY bytes.
	Output(OutputFrame),
	/// The terminal command status.
	Exit(ExitEvent),
}

/// A correlated named-process output attachment.
#[derive(Debug)]
pub struct ProcessAttachment {
	stream: RequestStream,
}

/// One event from a named-process output attachment.
#[derive(Debug)]
pub enum ProcessAttachmentEvent {
	/// The server established the attachment and identified its generation.
	Attached(OutputAttached),
	/// Ordered output from the attached process generation.
	Output(ProcessOutput),
	/// A lifecycle transition for the named process.
	State(ProcessStateEvent),
}

/// A streaming blob download.
#[derive(Debug)]
pub struct BlobDownload {
	stream: RequestStream,
}

/// One event from a blob download.
#[derive(Debug)]
pub enum BlobDownloadEvent {
	/// The next ordered bytes in the download.
	Chunk(Chunk),
	/// The successful terminal download marker.
	Complete(BlobGetComplete),
}

/// A streaming, correlated blob upload.
#[derive(Debug)]
pub struct BlobUpload {
	client:     EnvClient,
	scope:      Option<DataScope>,
	request_id: u64,
	stream:     RequestStream,
}

/// An invocation-scoped client whose public surface cannot invoke tools,
/// manage named processes, renegotiate the connection, or retire the server.
#[derive(Clone, Debug)]
pub struct WorkerEnvClient {
	client:           EnvClient,
	scope:            DataScope,
	last_transaction: Arc<Mutex<Option<TransactionId>>>,
}

/// The epoch-qualified identity of a document transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionId {
	/// Server epoch in which the transaction outcome is retained.
	pub server_epoch: Bytes,
	/// Caller-generated transaction identifier.
	pub txn_id:       Bytes,
}

/// An open, connection-owned document lease.
#[derive(Debug)]
pub struct DocumentLease {
	client:   EnvClient,
	scope:    DataScope,
	lease_id: Bytes,
	head:     DocumentHead,
	events:   DocumentEvents,
	released: bool,
}

/// A typed document read response.
#[derive(Debug)]
pub struct DocumentRead {
	response: ReadDocumentResponse,
}

/// A terminal document transaction outcome.
#[derive(Debug)]
pub enum TransactionOutcome {
	/// Every operation committed durably.
	Committed(TransactionCommitted),
	/// No operation committed; the supplied precondition or revision was
	/// rejected.
	Rejected(TransactionRejected),
	/// A durable prefix committed before a later operation failed.
	///
	/// Callers must not infer rollback.
	Partial(TransactionPartiallyCommitted),
}

/// Correlated events for one open document lease.
#[derive(Debug)]
pub struct DocumentEvents {
	stream: RequestStream,
}

/// One connection-wide LSP registry event.
#[derive(Debug)]
pub enum LspStreamEvent {
	/// Initial bindings returned by the subscription request.
	Bindings(GetLspBindingsResponse),
	/// A language-server notification.
	Event(LspEvent),
	/// A binding lifecycle change.
	Binding(LspBindingEvent),
}

/// Correlated connection-wide LSP events.
#[derive(Debug)]
pub struct LspEvents {
	stream: RequestStream,
}

/// One streaming workspace walk item.
#[derive(Debug)]
pub enum WalkEvent {
	/// One matching workspace entry.
	Entry(WalkEntry),
	/// Terminal walk accounting.
	Complete(WalkComplete),
}

/// A correlated streaming workspace walk.
#[derive(Debug)]
pub struct WalkStream {
	stream: RequestStream,
}

/// One streaming workspace search item.
#[derive(Debug)]
pub enum SearchEvent {
	/// One matching line.
	Match(SearchMatchMsg),
	/// Terminal search accounting.
	Complete(SearchComplete),
}

/// A correlated streaming workspace search.
#[derive(Debug)]
pub struct SearchStream {
	stream: RequestStream,
}

/// One item returned by a generic scoped DATA stream.
#[derive(Debug)]
pub enum DataStreamItem {
	/// A nonterminal event.
	Event(DataEvent),
	/// A terminal response.
	Response(DataResponse),
}

/// A generic scoped DATA request stream.
#[derive(Debug)]
pub struct DataStream {
	stream: RequestStream,
}

/// One typed DAP stream item.
#[derive(Debug)]
pub enum DapStreamEvent {
	/// The session was launched or attached.
	Session(document::DapSessionResponse),
	/// The action's terminal response.
	Action(document::DapActionResponse),
	/// Ordered adapter output.
	Output(document::DapOutput),
	/// Ordered adapter lifecycle or debugger event.
	Event(document::DapEvent),
}

/// A cancellable DAP launch, attach, or action stream.
#[derive(Debug)]
pub struct DapStream {
	stream: RequestStream,
}

/// One internal-resource completion item.
#[derive(Debug)]
pub enum ResourceCompletionEvent {
	/// One scored completion.
	Completion(env_wire::ResourceCompletion),
	/// Terminal completion accounting.
	Complete(env_wire::ResourceCompletionComplete),
}

/// A cancellable internal-resource completion stream.
#[derive(Debug)]
pub struct ResourceCompletionStream {
	stream: RequestStream,
}

/// One MCP subscription item.
#[derive(Debug)]
pub enum McpSubscriptionEvent {
	/// Server notification.
	Notification(env_wire::McpNotification),
	/// Lifecycle/status transition.
	Status(env_wire::McpServerStatus),
}

/// A cancellable MCP notification and status subscription.
#[derive(Debug)]
pub struct McpSubscription {
	stream: RequestStream,
}
impl EnvClient {
	/// Builds a client over decoded bidirectional frame channels.
	///
	/// `outgoing` carries client frames to the transport and `incoming` carries
	/// decoded server frames back. A dispatcher thread performs correlation;
	/// this client owns no async runtime or world resource.
	#[must_use]
	pub fn from_channels(outgoing: Sender<ClientFrame>, incoming: Receiver<ServerFrame>) -> Self {
		let (events_tx, events) = flume::unbounded();
		let (cancel, cancellations) = flume::unbounded();
		let (lease_close, lease_closes) = flume::unbounded();
		let inner = Arc::new(ClientInner {
			outgoing: outgoing.clone(),
			pending: Mutex::new(HashMap::new()),
			hello_waiter: Mutex::new(None),
			info: Mutex::new(None),
			events,
			next_id: AtomicU64::new(1),
			cancel,
			lease_close,
			admitter: Mutex::new(None),
		});
		let router = Arc::downgrade(&inner);
		let _ = std::thread::spawn(move || route_responses(router, incoming, events_tx));
		let _ = std::thread::spawn(move || route_cancellations(cancellations, outgoing));
		let closer = Arc::downgrade(&inner);
		let _ = std::thread::spawn(move || route_lease_closes(closer, lease_closes));
		Self { inner }
	}

	/// Creates an in-process client/server frame channel.
	///
	/// Capacity zero selects unbounded channels. A nonzero capacity applies
	/// backpressure to ordinary asynchronous frame sends; guard cancellation is
	/// first queued on a separate unbounded control channel so drop never
	/// blocks.
	#[must_use]
	pub fn in_process(capacity: usize) -> (Self, InProcessEnvTransport) {
		let (requests_tx, requests) = channel(capacity);
		let (responses, responses_rx) = channel(capacity);
		(Self::from_channels(requests_tx, responses_rx), InProcessEnvTransport {
			requests,
			responses,
		})
	}

	/// Performs the request-id-zero protocol handshake.
	pub async fn hello(&self, hello: ClientHello) -> Result<ServerHello, ClientError> {
		let (sender, receiver) = flume::bounded(1);
		{
			let mut slot = self.inner.hello_waiter.lock();
			if slot.is_some() {
				return Err(ClientError::UnexpectedResponse { expected: "a single in-flight hello" });
			}
			*slot = Some(sender);
		}
		let send = self
			.inner
			.outgoing
			.send_async(ClientFrame {
				request_id: 0,
				body: Some(client_frame::Body::Hello(hello)),
				..ClientFrame::default()
			})
			.await;
		if send.is_err() {
			self.inner.hello_waiter.lock().take();
			return Err(ClientError::TransportClosed);
		}
		let frame = receiver
			.recv_async()
			.await
			.map_err(|_| ClientError::TransportClosed)?;
		match frame.body {
			Some(server_frame::Body::Hello(response)) => {
				*self.inner.info.lock() = Some(response.clone());
				Ok(response)
			},
			Some(server_frame::Body::Error(error)) => Err(protocol_error(error)),
			_ => Err(ClientError::UnexpectedResponse { expected: "ServerHello" }),
		}
	}

	/// Returns the cached server handshake, if this client has completed one.
	#[must_use]
	pub fn info(&self) -> Option<ServerHello> {
		self.inner.info.lock().clone()
	}

	/// Installs the handler for server-initiated admission queries.
	///
	/// The handler must be installed from the async runtime that owns this
	/// client; the frame router schedules answers on that ambient runtime.
	pub fn set_admitter<A: Admitter>(&self, admitter: A) {
		*self.inner.admitter.lock() = Some(Arc::new(AdmitterDispatcher {
			admitter: Arc::new(admitter),
			runtime:  tokio::runtime::Handle::current(),
		}));
	}

	/// Restricts this connection to one invocation-bound worker authority.
	///
	/// The returned surface deliberately exposes no tool invocation, named
	/// process, blob deletion, hello, or retire operation.
	#[must_use]
	pub fn worker_scope(&self, scope: DataScope) -> WorkerEnvClient {
		WorkerEnvClient { client: self.clone(), scope, last_transaction: Arc::new(Mutex::new(None)) }
	}

	/// Asks the serving daemon to retire its listening socket.
	///
	/// The server stops accepting new connections and releases the endpoint.
	/// Existing connections, including this one, continue until closed or
	/// drained. This resolves once the server acknowledges with
	/// `RetireStarted`. Pre-change servers reject the request with a protocol
	/// error, which surfaces as [`ClientError::Protocol`].
	pub async fn retire(&self) -> Result<(), ClientError> {
		match self
			.one_shot(client_frame::Body::Retire(Retire::default()), None)
			.await?
		{
			server_frame::Body::RetireStarted(_) => Ok(()),
			_ => Err(ClientError::UnexpectedResponse { expected: "RetireStarted" }),
		}
	}

	/// Begins graceful Environment shutdown and waits for admission closure.
	pub async fn shutdown(
		&self,
		request: env_wire::ShutdownRequest,
	) -> Result<env_wire::ShutdownAcknowledged, ClientError> {
		match self
			.one_shot(client_frame::Body::Shutdown(request), None)
			.await?
		{
			server_frame::Body::ShutdownAcknowledged(acknowledged) => Ok(acknowledged),
			_ => Err(ClientError::UnexpectedResponse { expected: "ShutdownAcknowledged" }),
		}
	}

	/// Materializes one immutable Python site tree through the installer-only
	/// environment connection.
	///
	/// The Environment makes the requested store references available, builds
	/// the complete symlink farm, and atomically replaces `sites/<site_key>`.
	/// Repeating a request with the same manifest returns the existing tree.
	///
	/// This method is intentionally absent from [`WorkerEnvClient`]: extension
	/// connections can read their site tree but cannot mutate any site or store.
	pub async fn materialize_site(
		&self,
		request: MaterializeSite,
	) -> Result<SiteMaterialized, ClientError> {
		let request =
			DataRequest { body: Some(data_request::Body::Site(request)), ..DataRequest::default() };
		match self
			.one_shot(client_frame::Body::Data(request), None)
			.await?
		{
			server_frame::Body::Data(DataResponse {
				body: Some(data_response::Body::Site(response)),
				..
			}) => Ok(response),
			_ => Err(ClientError::UnexpectedResponse { expected: "SiteMaterialized" }),
		}
	}

	/// Retrieves bounded, versioned host facts from the Environment authority.
	pub async fn host_info(
		&self,
		request: env_wire::HostInfoRequest,
	) -> Result<env_wire::HostInfo, ClientError> {
		let response = self
			.data_request_owned(DataRequest {
				body: Some(data_request::Body::HostInfo(request)),
				..DataRequest::default()
			})
			.await?;
		match response.body {
			Some(data_response::Body::HostInfo(result)) => Ok(result),
			_ => Err(ClientError::UnexpectedResponse { expected: "HostInfo" }),
		}
	}

	/// Retrieves the ordered canonical set of primary and granted roots.
	pub async fn workspace_roots(
		&self,
		request: env_wire::WorkspaceRootSetRequest,
	) -> Result<env_wire::WorkspaceRootSet, ClientError> {
		let response = self
			.data_request_owned(DataRequest {
				body: Some(data_request::Body::WorkspaceRoots(request)),
				..DataRequest::default()
			})
			.await?;
		match response.body {
			Some(data_response::Body::WorkspaceRoots(result)) => Ok(result),
			_ => Err(ClientError::UnexpectedResponse { expected: "WorkspaceRootSet" }),
		}
	}

	/// Returns Environment-owned MCP lifecycle status and definition epoch.
	pub async fn mcp_status(
		&self,
		request: env_wire::McpStatusRequest,
	) -> Result<env_wire::McpStatusResult, ClientError> {
		let result = self
			.mcp_request(env_wire::mcp_op::Op::Status(request))
			.await?;
		match result.result {
			Some(env_wire::mcp_result::Result::Status(status)) => Ok(status),
			_ => Err(ClientError::UnexpectedResponse { expected: "McpStatusResult" }),
		}
	}

	/// Executes one finite native MCP configuration operation.
	pub async fn mcp_config(
		&self,
		request: env_wire::McpConfigRequest,
	) -> Result<env_wire::McpConfigResult, ClientError> {
		let result = self
			.mcp_request(env_wire::mcp_op::Op::Config(request))
			.await?;
		match result.result {
			Some(env_wire::mcp_result::Result::Config(config)) => Ok(config),
			_ => Err(ClientError::UnexpectedResponse { expected: "McpConfigResult" }),
		}
	}

	/// Resets one MCP server at its exact definition epoch.
	pub async fn mcp_reset(
		&self,
		request: env_wire::McpResetRequest,
	) -> Result<env_wire::McpResetResult, ClientError> {
		let result = self
			.mcp_request(env_wire::mcp_op::Op::Reset(request))
			.await?;
		match result.result {
			Some(env_wire::mcp_result::Result::Reset(reset)) => Ok(reset),
			_ => Err(ClientError::UnexpectedResponse { expected: "McpResetResult" }),
		}
	}

	/// Resolves live transport headers without exposing credential ownership.
	pub async fn mcp_live_header(
		&self,
		request: env_wire::McpLiveHeaderRequest,
	) -> Result<env_wire::McpLiveHeader, ClientError> {
		let result = self
			.mcp_request(env_wire::mcp_op::Op::LiveHeader(request))
			.await?;
		match result.result {
			Some(env_wire::mcp_result::Result::LiveHeader(header)) => Ok(header),
			_ => Err(ClientError::UnexpectedResponse { expected: "McpLiveHeader" }),
		}
	}

	/// Reads one bounded MCP resource.
	pub async fn mcp_resource(
		&self,
		request: env_wire::McpResourceRequest,
	) -> Result<env_wire::McpResourceResult, ClientError> {
		let result = self
			.mcp_request(env_wire::mcp_op::Op::Resource(request))
			.await?;
		match result.result {
			Some(env_wire::mcp_result::Result::Resource(resource)) => Ok(resource),
			_ => Err(ClientError::UnexpectedResponse { expected: "McpResourceResult" }),
		}
	}

	/// Renders one bounded MCP prompt.
	pub async fn mcp_prompt(
		&self,
		request: env_wire::McpPromptRequest,
	) -> Result<env_wire::McpPromptResult, ClientError> {
		let result = self
			.mcp_request(env_wire::mcp_op::Op::Prompt(request))
			.await?;
		match result.result {
			Some(env_wire::mcp_result::Result::Prompt(prompt)) => Ok(prompt),
			_ => Err(ClientError::UnexpectedResponse { expected: "McpPromptResult" }),
		}
	}

	/// Invokes one MCP tool under Environment lifecycle and timeout authority.
	pub async fn mcp_invoke(
		&self,
		request: env_wire::McpInvokeRequest,
	) -> Result<env_wire::McpInvokeResult, ClientError> {
		let result = self
			.mcp_request(env_wire::mcp_op::Op::Invoke(request))
			.await?;
		match result.result {
			Some(env_wire::mcp_result::Result::Invoke(invoke)) => Ok(invoke),
			_ => Err(ClientError::UnexpectedResponse { expected: "McpInvokeResult" }),
		}
	}

	/// Opens a cancellable MCP notification/status subscription.
	pub async fn mcp_subscribe(
		&self,
		request: env_wire::McpSubscribeRequest,
	) -> Result<McpSubscription, ClientError> {
		let stream = self
			.open(
				client_frame::Body::Data(DataRequest {
					body: Some(data_request::Body::Mcp(env_wire::McpOp {
						op: Some(env_wire::mcp_op::Op::Subscribe(request)),
					})),
					..DataRequest::default()
				}),
				None,
			)
			.await?;
		Ok(McpSubscription { stream })
	}

	/// Creates an Environment-owned isolated worktree.
	pub async fn create_worktree(
		&self,
		request: CreateWorktree,
	) -> Result<WorktreeResult, ClientError> {
		self
			.worktree_request(worktree_op::Op::Create(request))
			.await
	}

	/// Destroys an Environment-owned isolated worktree.
	pub async fn destroy_worktree(
		&self,
		request: DestroyWorktree,
	) -> Result<WorktreeResult, ClientError> {
		self
			.worktree_request(worktree_op::Op::Destroy(request))
			.await
	}

	/// Produces the selected Environment-owned worktree disposition.
	pub async fn merge_worktree(
		&self,
		request: MergeWorktree,
	) -> Result<WorktreeResult, ClientError> {
		self.worktree_request(worktree_op::Op::Merge(request)).await
	}

	async fn data_request_owned(&self, request: DataRequest) -> Result<DataResponse, ClientError> {
		match self
			.one_shot(client_frame::Body::Data(request), None)
			.await?
		{
			server_frame::Body::Data(response) => Ok(response),
			_ => Err(ClientError::UnexpectedResponse { expected: "DataResponse" }),
		}
	}

	/// Creates a cloneable out-of-band control handle for one active execution.
	#[must_use]
	pub fn active_exec_control(&self, exec: Bytes) -> ActiveExecControl {
		ActiveExecControl { client: self.clone(), exec }
	}

	/// Applies typed control to one exec generation.
	pub async fn exec_control(
		&self,
		request: env_wire::ExecControlRequest,
	) -> Result<env_wire::ExecControlResult, ClientError> {
		let result = self
			.exec_session_request(env_wire::exec_session_op::Op::Control(request))
			.await?;
		match result.result {
			Some(env_wire::exec_session_result::Result::Controlled(controlled)) => Ok(controlled),
			_ => Err(ClientError::UnexpectedResponse { expected: "ExecControlResult" }),
		}
	}

	async fn exec_live_control(
		&self,
		op: env_wire::exec_session_op::Op,
	) -> Result<bool, ClientError> {
		let result = self.exec_session_request(op).await?;
		match result.result {
			Some(env_wire::exec_session_result::Result::Controlled(controlled)) => {
				Ok(controlled.accepted)
			},
			_ => Err(ClientError::UnexpectedResponse { expected: "ExecControlResult" }),
		}
	}

	async fn exec_session_request(
		&self,
		op: env_wire::exec_session_op::Op,
	) -> Result<env_wire::ExecSessionResult, ClientError> {
		let response = self
			.data_request_owned(DataRequest {
				body: Some(data_request::Body::ExecSession(env_wire::ExecSessionOp { op: Some(op) })),
				..DataRequest::default()
			})
			.await?;
		match response.body {
			Some(data_response::Body::ExecSession(result)) => Ok(result),
			_ => Err(ClientError::UnexpectedResponse { expected: "ExecSessionResult" }),
		}
	}

	async fn mcp_request(
		&self,
		op: env_wire::mcp_op::Op,
	) -> Result<env_wire::McpResult, ClientError> {
		let response = self
			.data_request_owned(DataRequest {
				body: Some(data_request::Body::Mcp(env_wire::McpOp { op: Some(op) })),
				..DataRequest::default()
			})
			.await?;
		match response.body {
			Some(data_response::Body::Mcp(result)) => Ok(result),
			_ => Err(ClientError::UnexpectedResponse { expected: "McpResult" }),
		}
	}

	async fn worktree_request(
		&self,
		operation: worktree_op::Op,
	) -> Result<WorktreeResult, ClientError> {
		let request = DataRequest {
			body: Some(data_request::Body::Worktree(omp_proto::env::v1::WorktreeOp {
				op: Some(operation),
				..Default::default()
			})),
			..Default::default()
		};
		match self
			.one_shot(client_frame::Body::Data(request), None)
			.await?
		{
			server_frame::Body::Data(DataResponse {
				body: Some(data_response::Body::Worktree(response)),
				..
			}) => Ok(response),
			_ => Err(ClientError::UnexpectedResponse { expected: "WorktreeResult" }),
		}
	}

	/// Returns the receiver for unsolicited request-id-zero server events.
	///
	/// Clones share one queue; callers should normally keep a single receiver
	/// and distribute events according to application policy.
	#[must_use]
	pub fn server_events(&self) -> Receiver<ServerFrame> {
		self.inner.events.clone()
	}

	/// Opens a tool invocation before its arguments have committed.
	pub async fn invoke(&self, request: InvokeTool) -> Result<Invocation, ClientError> {
		let id = Str::new(request.invocation_id.as_str());
		let (stream, guard) = self
			.open_guarded(client_frame::Body::InvokeTool(request), None)
			.await?;
		Ok(Invocation { client: self.clone(), id, stream, guard: Some(guard) })
	}

	/// Opens a persistent, server-owned exec session rooted at `cwd`.
	pub async fn open_session(
		&self,
		cwd: &EnvPath,
		mut request: OpenSessionRequest,
	) -> Result<OpenSessionResponse, ClientError> {
		request.cwd_uri = self.path_uri(cwd)?;
		self.open_session_scoped(request, None).await
	}

	/// Explicitly closes a persistent exec session.
	pub async fn close_session(
		&self,
		request: CloseSessionRequest,
	) -> Result<CloseSessionResponse, ClientError> {
		match self
			.one_shot(client_frame::Body::CloseSession(request), None)
			.await?
		{
			server_frame::Body::SessionClosed(response) => Ok(response),
			_ => Err(ClientError::UnexpectedResponse { expected: "CloseSessionResponse" }),
		}
	}

	/// Starts one guarded command inside a persistent session.
	pub async fn exec(&self, request: ExecRequest) -> Result<ExecRun, ClientError> {
		self.exec_scoped(request, None).await
	}

	/// Starts or replaces a server-owned named process rooted at `cwd`.
	pub async fn start_process(
		&self,
		cwd: &EnvPath,
		mut request: StartProcess,
	) -> Result<ProcessStarted, ClientError> {
		request.spec.get_or_insert_default().cwd_uri = self.path_uri(cwd)?;
		match self
			.one_shot(client_frame::Body::StartProcess(request), None)
			.await?
		{
			server_frame::Body::ProcessStarted(response) => Ok(response),
			_ => Err(ClientError::UnexpectedResponse { expected: "ProcessStarted" }),
		}
	}

	/// Lists the server-owned named processes visible to this environment.
	pub async fn list_processes(&self, request: ListProcesses) -> Result<ProcessList, ClientError> {
		match self
			.one_shot(client_frame::Body::ListProcesses(request), None)
			.await?
		{
			server_frame::Body::ProcessList(response) => Ok(response),
			_ => Err(ClientError::UnexpectedResponse { expected: "ProcessList" }),
		}
	}

	/// Attaches to ordered output and state events for one named-process
	/// generation.
	pub async fn attach_output(
		&self,
		request: AttachOutput,
	) -> Result<ProcessAttachment, ClientError> {
		let stream = self
			.open(client_frame::Body::AttachOutput(request), None)
			.await?;
		Ok(ProcessAttachment { stream })
	}

	/// Sends bytes or EOF to one generation of a server-owned named process.
	pub async fn send_process_input(
		&self,
		request: SendInput,
	) -> Result<ProcessCommandAccepted, ClientError> {
		self
			.process_command(client_frame::Body::SendInput(request))
			.await
	}

	/// Sends a signal to one generation of a server-owned named process.
	pub async fn signal_process(
		&self,
		request: SignalProcess,
	) -> Result<ProcessCommandAccepted, ClientError> {
		self
			.process_command(client_frame::Body::SignalProcess(request))
			.await
	}

	/// Stops one generation of a server-owned named process.
	pub async fn stop_process(
		&self,
		request: StopProcess,
	) -> Result<ProcessCommandAccepted, ClientError> {
		self
			.process_command(client_frame::Body::StopProcess(request))
			.await
	}

	/// Checks whether a content-addressed blob is present.
	pub async fn blob_stat(&self, request: StatRequest) -> Result<StatResponse, ClientError> {
		match self
			.one_shot(client_frame::Body::BlobStat(request), None)
			.await?
		{
			server_frame::Body::BlobStat(response) => Ok(response),
			_ => Err(ClientError::UnexpectedResponse { expected: "StatResponse" }),
		}
	}

	/// Starts a streaming blob download.
	pub async fn blob_get(&self, request: GetRequest) -> Result<BlobDownload, ClientError> {
		let stream = self
			.open(client_frame::Body::BlobGet(request), None)
			.await?;
		Ok(BlobDownload { stream })
	}

	/// Starts a streaming blob upload.
	///
	/// Call [`BlobUpload::send_chunk`] in order and finish with
	/// [`BlobUpload::commit`]. Dropping an uncommitted upload only abandons its
	/// client-side response route; blob visibility remains gated by the commit
	/// frame.
	pub fn blob_put(&self) -> Result<BlobUpload, ClientError> {
		let request_id = self.allocate_request_id()?;
		let stream = self.register(request_id);
		Ok(BlobUpload { client: self.clone(), scope: None, request_id, stream })
	}

	/// Deletes one content-addressed blob.
	pub async fn blob_delete(&self, request: DeleteRequest) -> Result<DeleteResponse, ClientError> {
		match self
			.one_shot(client_frame::Body::BlobDelete(request), None)
			.await?
		{
			server_frame::Body::BlobDeleted(response) => Ok(response),
			_ => Err(ClientError::UnexpectedResponse { expected: "DeleteResponse" }),
		}
	}

	async fn process_command(
		&self,
		body: client_frame::Body,
	) -> Result<ProcessCommandAccepted, ClientError> {
		match self.one_shot(body, None).await? {
			server_frame::Body::ProcessCommandAccepted(response) => Ok(response),
			_ => Err(ClientError::UnexpectedResponse { expected: "ProcessCommandAccepted" }),
		}
	}

	async fn one_shot(
		&self,
		body: client_frame::Body,
		scope: Option<&DataScope>,
	) -> Result<server_frame::Body, ClientError> {
		let mut stream = self.open(body, scope).await?;
		let frame = stream.next().await?.ok_or(ClientError::TransportClosed)?;
		stream.finish();
		response_body(frame)
	}

	async fn open(
		&self,
		body: client_frame::Body,
		scope: Option<&DataScope>,
	) -> Result<RequestStream, ClientError> {
		let request_id = self.allocate_request_id()?;
		let stream = self.register(request_id);
		if self.send(request_id, body, scope).await.is_err() {
			stream.unregister();
			return Err(ClientError::TransportClosed);
		}
		Ok(stream)
	}

	async fn open_guarded(
		&self,
		body: client_frame::Body,
		scope: Option<&DataScope>,
	) -> Result<(RequestStream, RunGuard), ClientError> {
		let request_id = self.allocate_request_id()?;
		let stream = self.register(request_id);
		let guard = RunGuard::new(request_id, self.inner.cancel.clone());
		if self.send(request_id, body, scope).await.is_err() {
			stream.unregister();
			guard.relinquish();
			return Err(ClientError::TransportClosed);
		}
		Ok((stream, guard))
	}

	fn register(&self, request_id: u64) -> RequestStream {
		let (sender, receiver) = flume::unbounded();
		self.inner.pending.lock().insert(request_id, sender);
		RequestStream { request_id, receiver, client: Arc::downgrade(&self.inner), finished: false }
	}

	async fn send(
		&self,
		request_id: u64,
		body: client_frame::Body,
		scope: Option<&DataScope>,
	) -> Result<(), ClientError> {
		self
			.inner
			.outgoing
			.send_async(ClientFrame {
				request_id,
				body: Some(body),
				scope: scope.map(DataScope::wire),
				..ClientFrame::default()
			})
			.await
			.map_err(|_| ClientError::TransportClosed)
	}

	async fn open_session_scoped(
		&self,
		request: OpenSessionRequest,
		scope: Option<&DataScope>,
	) -> Result<OpenSessionResponse, ClientError> {
		match self
			.one_shot(client_frame::Body::OpenSession(request), scope)
			.await?
		{
			server_frame::Body::SessionOpened(response) => Ok(response),
			_ => Err(ClientError::UnexpectedResponse { expected: "OpenSessionResponse" }),
		}
	}

	async fn exec_scoped(
		&self,
		request: ExecRequest,
		scope: Option<&DataScope>,
	) -> Result<ExecRun, ClientError> {
		let (stream, guard) = self
			.open_guarded(client_frame::Body::Exec(request), scope)
			.await?;
		Ok(ExecRun { client: self.clone(), scope: scope.cloned(), stream, guard: Some(guard) })
	}

	fn path_uri(&self, path: &EnvPath) -> Result<String, ClientError> {
		let path = path.as_str();
		if path.starts_with("file://") {
			let url = Url::parse(path)
				.map_err(|error| ClientError::InvalidEnvPath(Str::from(error.to_string())))?;
			if url.scheme() != "file" {
				return Err(ClientError::InvalidEnvPath(sf!(
					"environment paths must use the file scheme",
				)));
			}
			return Ok(url.to_string());
		}
		let mut url = if path.starts_with('/') {
			Url::parse("file:///")
				.map_err(|error| ClientError::InvalidEnvPath(Str::from(error.to_string())))?
		} else {
			let info = self.inner.info.lock();
			let root = info
				.as_ref()
				.map(|info| info.root_uri.as_str())
				.filter(|root| !root.is_empty())
				.ok_or(ClientError::UnexpectedResponse {
					expected: "ServerHello root_uri before resolving a relative EnvPath",
				})?;
			Url::parse(root)
				.map_err(|error| ClientError::InvalidEnvPath(Str::from(error.to_string())))?
		};
		if path.starts_with('/') {
			url.set_path(path);
		} else {
			let mut segments = url
				.path_segments_mut()
				.map_err(|()| ClientError::InvalidEnvPath(sf!("workspace root is not hierarchical")))?;
			segments.pop_if_empty();
			for component in path.split('/') {
				match component {
					"" | "." => {},
					".." => {
						return Err(ClientError::InvalidEnvPath(sf!(
							"relative environment paths cannot escape the workspace root",
						)));
					},
					component => {
						segments.push(component);
					},
				}
			}
		}
		url.set_query(None);
		url.set_fragment(None);
		Ok(url.to_string())
	}

	fn allocate_request_id(&self) -> Result<u64, ClientError> {
		self
			.inner
			.next_id
			.try_update(Ordering::Relaxed, Ordering::Relaxed, |request_id| request_id.checked_add(1))
			.map_err(|_| ClientError::RequestIdExhausted)
	}
}

impl WorkerEnvClient {
	/// Returns this worker client's immutable invocation authority.
	#[must_use]
	pub const fn scope(&self) -> &DataScope {
		&self.scope
	}

	/// Returns the cached server handshake, if negotiation completed.
	#[must_use]
	pub fn info(&self) -> Option<ServerHello> {
		self.client.info()
	}

	/// Performs one arbitrary scoped DATA request.
	pub async fn request(&self, request: DataRequest) -> Result<DataResponse, ClientError> {
		ensure_worker_data(&request)?;
		match self
			.client
			.one_shot(client_frame::Body::Data(request), Some(&self.scope))
			.await?
		{
			server_frame::Body::Data(response) => Ok(response),
			_ => Err(ClientError::UnexpectedResponse { expected: "DataResponse" }),
		}
	}

	/// Opens an arbitrary scoped DATA stream.
	pub async fn stream(&self, request: DataRequest) -> Result<DataStream, ClientError> {
		ensure_worker_data(&request)?;
		let stream = self
			.client
			.open(client_frame::Body::Data(request), Some(&self.scope))
			.await?;
		Ok(DataStream { stream })
	}

	/// Opens a connection-owned document lease and its correlated event stream.
	pub async fn open_document(
		&self,
		path: &EnvPath,
		language_id: Option<&str>,
	) -> Result<DocumentLease, ClientError> {
		let request = DataRequest {
			body: Some(data_request::Body::Document(omp_proto::env::v1::DocumentOp {
				op: Some(document_op::Op::Open(OpenDocumentRequest {
					uri:         self.client.path_uri(path)?,
					language_id: language_id.unwrap_or_default().to_owned(),
				})),
				..omp_proto::env::v1::DocumentOp::default()
			})),
			..DataRequest::default()
		};
		let mut stream = self
			.client
			.open(client_frame::Body::Data(request), Some(&self.scope))
			.await?;
		let frame = stream.next().await?.ok_or(ClientError::TransportClosed)?;
		let opened = document_result(response_data(frame)?, "OpenDocumentResponse", |result| {
			let document_result::Result::Opened(opened) = result else {
				return None;
			};
			Some(opened)
		})?;
		let head = opened
			.head
			.ok_or(ClientError::UnexpectedResponse { expected: "DocumentHead" })?;
		if opened.lease_id.is_empty() || !is_revision_pinned(&head) {
			return Err(ClientError::UnexpectedResponse {
				expected: "connection-owned lease id and revision-pinned DocumentHead",
			});
		}
		Ok(DocumentLease {
			client: self.client.clone(),
			scope: self.scope.clone(),
			lease_id: opened.lease_id,
			head,
			events: DocumentEvents { stream },
			released: false,
		})
	}

	/// Reads a revision or selection from an owned document lease.
	pub async fn read_document(
		&self,
		lease: &DocumentLease,
		revision: Option<Revision>,
		selection: Option<ReadSelection>,
	) -> Result<DocumentRead, ClientError> {
		let request = document_request(document_op::Op::Read(ReadDocumentRequest {
			document: Some(lease_target(lease)),
			revision,
			selection,
		}));
		let response = self.request(request).await?;
		let read = document_result(response, "ReadDocumentResponse", |result| {
			let document_result::Result::Read(read) = result else {
				return None;
			};
			Some(read)
		})?;
		let complete_head = read.head.as_ref().is_some_and(is_revision_pinned);
		if !complete_head || read.body.is_none() {
			return Err(ClientError::UnexpectedResponse {
				expected: "complete revision-pinned ReadDocumentResponse",
			});
		}
		Ok(DocumentRead { response: read })
	}

	/// Commits an idempotent document transaction.
	///
	/// The epoch-qualified transaction id is retained before transmission so a
	/// caller can reuse it after an ambiguous disconnect.
	pub async fn commit_transaction(
		&self,
		request: CommitTransactionRequest,
	) -> Result<TransactionOutcome, ClientError> {
		let server_epoch = self
			.client
			.inner
			.info
			.lock()
			.as_ref()
			.map(|info| info.server_epoch.clone())
			.filter(|epoch| !epoch.is_empty())
			.ok_or(ClientError::UnexpectedResponse {
				expected: "nonempty ServerHello.server_epoch before transaction",
			})?;
		let expected_transaction_id = request.transaction_id.clone();
		*self.last_transaction.lock() =
			Some(TransactionId { server_epoch, txn_id: expected_transaction_id.clone() });
		let response = self
			.request(document_request(document_op::Op::CommitTransaction(request)))
			.await?;
		let transaction = document_result(response, "CommitTransactionResponse", |result| {
			let document_result::Result::Transaction(transaction) = result else {
				return None;
			};
			Some(transaction)
		})?;
		match transaction.outcome {
			Some(commit_transaction_response::Outcome::Committed(committed))
				if committed.transaction_id == expected_transaction_id =>
			{
				Ok(TransactionOutcome::Committed(committed))
			},
			Some(commit_transaction_response::Outcome::Rejected(rejected))
				if rejected.transaction_id == expected_transaction_id =>
			{
				Ok(TransactionOutcome::Rejected(rejected))
			},
			Some(commit_transaction_response::Outcome::PartiallyCommitted(partial))
				if partial.transaction_id == expected_transaction_id =>
			{
				Ok(TransactionOutcome::Partial(partial))
			},
			Some(_) => Err(ClientError::UnexpectedResponse { expected: "matching transaction id" }),
			None => Err(ClientError::UnexpectedResponse { expected: "transaction outcome" }),
		}
	}

	/// Returns the epoch-qualified id of the most recently attempted
	/// transaction.
	#[must_use]
	pub fn last_transaction(&self) -> Option<TransactionId> {
		self.last_transaction.lock().clone()
	}

	/// Subscribes to LSP registry events after querying this lease's bindings.
	pub async fn lsp_events(&self, lease: &DocumentLease) -> Result<LspEvents, ClientError> {
		let request = document_request(document_op::Op::GetLspBindings(GetLspBindingsRequest {
			document: Some(lease_target(lease)),
		}));
		let stream = self
			.client
			.open(client_frame::Body::Data(request), Some(&self.scope))
			.await?;
		Ok(LspEvents { stream })
	}

	/// Reads the Environment-owned repository snapshot for a granted root.
	pub async fn repository_snapshot(
		&self,
		request: env_wire::RepositorySnapshotRequest,
	) -> Result<env_wire::RepositorySnapshot, ClientError> {
		let response = self
			.request(DataRequest {
				body: Some(data_request::Body::RepositorySnapshot(request)),
				..DataRequest::default()
			})
			.await?;
		match response.body {
			Some(data_response::Body::RepositorySnapshot(snapshot)) => Ok(snapshot),
			_ => Err(ClientError::UnexpectedResponse { expected: "RepositorySnapshot" }),
		}
	}

	/// Executes an attributed, approval-ticketed privileged write or unlink.
	pub async fn privileged_mutation(
		&self,
		request: env_wire::PrivilegedMutationIntent,
	) -> Result<env_wire::PrivilegedMutationResult, ClientError> {
		let response = self
			.request(DataRequest {
				body: Some(data_request::Body::PrivilegedMutation(request)),
				..DataRequest::default()
			})
			.await?;
		match response.body {
			Some(data_response::Body::PrivilegedMutation(result)) => Ok(result),
			_ => Err(ClientError::UnexpectedResponse { expected: "PrivilegedMutationResult" }),
		}
	}

	/// Materializes one internal resource as a leased Environment path.
	pub async fn materialize(
		&self,
		request: env_wire::MaterializeRequest,
	) -> Result<env_wire::MaterializationLease, ClientError> {
		let result = self
			.exec_session_request(env_wire::exec_session_op::Op::Materialize(request))
			.await?;
		match result.result {
			Some(env_wire::exec_session_result::Result::Materialized(lease)) => Ok(lease),
			_ => Err(ClientError::UnexpectedResponse { expected: "MaterializationLease" }),
		}
	}

	/// Releases a materialized Environment path lease.
	pub async fn release_materialization(
		&self,
		request: env_wire::ReleaseMaterialization,
	) -> Result<env_wire::MaterializationReleased, ClientError> {
		let result = self
			.exec_session_request(env_wire::exec_session_op::Op::ReleaseMaterialization(request))
			.await?;
		match result.result {
			Some(env_wire::exec_session_result::Result::MaterializationReleased(released)) => {
				Ok(released)
			},
			_ => Err(ClientError::UnexpectedResponse { expected: "MaterializationReleased" }),
		}
	}

	/// Applies typed control to one exec generation.
	pub async fn exec_control(
		&self,
		request: env_wire::ExecControlRequest,
	) -> Result<env_wire::ExecControlResult, ClientError> {
		let result = self
			.exec_session_request(env_wire::exec_session_op::Op::Control(request))
			.await?;
		match result.result {
			Some(env_wire::exec_session_result::Result::Controlled(controlled)) => Ok(controlled),
			_ => Err(ClientError::UnexpectedResponse { expected: "ExecControlResult" }),
		}
	}

	/// Negotiates backend capabilities for an exec session.
	pub async fn exec_capabilities(
		&self,
		request: env_wire::ExecCapabilitiesRequest,
	) -> Result<env_wire::ExecBackendCapabilities, ClientError> {
		let result = self
			.exec_session_request(env_wire::exec_session_op::Op::Capabilities(request))
			.await?;
		match result.result {
			Some(env_wire::exec_session_result::Result::Capabilities(capabilities)) => {
				Ok(capabilities)
			},
			_ => Err(ClientError::UnexpectedResponse { expected: "ExecBackendCapabilities" }),
		}
	}

	/// Reads revision-fenced final working-directory metadata.
	pub async fn exec_final_cwd(
		&self,
		request: env_wire::ExecFinalCwdRequest,
	) -> Result<env_wire::ExecFinalCwd, ClientError> {
		let result = self
			.exec_session_request(env_wire::exec_session_op::Op::FinalCwd(request))
			.await?;
		match result.result {
			Some(env_wire::exec_session_result::Result::FinalCwd(final_cwd)) => Ok(final_cwd),
			_ => Err(ClientError::UnexpectedResponse { expected: "ExecFinalCwd" }),
		}
	}

	/// Launches a revision-fenced DAP session and streams its typed events.
	pub async fn dap_launch(
		&self,
		request: document::DapLaunchRequest,
	) -> Result<DapStream, ClientError> {
		self
			.dap_stream(data_request::Body::DapLaunch(request))
			.await
	}

	/// Attaches a revision-fenced DAP session and streams its typed events.
	pub async fn dap_attach(
		&self,
		request: document::DapAttachRequest,
	) -> Result<DapStream, ClientError> {
		self
			.dap_stream(data_request::Body::DapAttach(request))
			.await
	}

	/// Applies one capability-tagged, revision-fenced DAP action.
	pub async fn dap_action(
		&self,
		request: document::DapActionRequest,
	) -> Result<DapStream, ClientError> {
		self
			.dap_stream(data_request::Body::DapAction(request))
			.await
	}

	/// Reads one bounded internal resource.
	pub async fn resource_read(
		&self,
		request: env_wire::ResourceReadRequest,
	) -> Result<env_wire::ResourceResult, ClientError> {
		self
			.resource_request(env_wire::resource_op::Op::Read(request))
			.await
	}

	/// Lists one bounded internal resource.
	pub async fn resource_list(
		&self,
		request: env_wire::ResourceListRequest,
	) -> Result<env_wire::ResourceResult, ClientError> {
		self
			.resource_request(env_wire::resource_op::Op::List(request))
			.await
	}

	/// Resolves one internal resource to canonical path metadata without bytes.
	pub async fn resource_path(
		&self,
		request: env_wire::ResourcePathRequest,
	) -> Result<env_wire::ResourceResult, ClientError> {
		self
			.resource_request(env_wire::resource_op::Op::Path(request))
			.await
	}

	/// Opens a cancellable, bounded internal-resource completion stream.
	pub async fn resource_complete(
		&self,
		request: env_wire::ResourceCompleteRequest,
	) -> Result<ResourceCompletionStream, ClientError> {
		let stream = self
			.client
			.open(
				client_frame::Body::Data(DataRequest {
					body: Some(data_request::Body::Resource(env_wire::ResourceOp {
						op: Some(env_wire::resource_op::Op::Complete(request)),
					})),
					..DataRequest::default()
				}),
				Some(&self.scope),
			)
			.await?;
		Ok(ResourceCompletionStream { stream })
	}

	async fn exec_session_request(
		&self,
		op: env_wire::exec_session_op::Op,
	) -> Result<env_wire::ExecSessionResult, ClientError> {
		let response = self
			.request(DataRequest {
				body: Some(data_request::Body::ExecSession(env_wire::ExecSessionOp { op: Some(op) })),
				..DataRequest::default()
			})
			.await?;
		match response.body {
			Some(data_response::Body::ExecSession(result)) => Ok(result),
			_ => Err(ClientError::UnexpectedResponse { expected: "ExecSessionResult" }),
		}
	}

	async fn resource_request(
		&self,
		op: env_wire::resource_op::Op,
	) -> Result<env_wire::ResourceResult, ClientError> {
		let response = self
			.request(DataRequest {
				body: Some(data_request::Body::Resource(env_wire::ResourceOp { op: Some(op) })),
				..DataRequest::default()
			})
			.await?;
		match response.body {
			Some(data_response::Body::Resource(env_wire::ResourceOpResult {
				result: Some(result),
			})) => Ok(result),
			_ => Err(ClientError::UnexpectedResponse { expected: "ResourceResult" }),
		}
	}

	async fn dap_stream(&self, body: data_request::Body) -> Result<DapStream, ClientError> {
		let stream = self
			.client
			.open(
				client_frame::Body::Data(DataRequest { body: Some(body), ..DataRequest::default() }),
				Some(&self.scope),
			)
			.await?;
		Ok(DapStream { stream })
	}

	/// Starts a streaming workspace walk rooted at a typed environment path.
	pub async fn walk(
		&self,
		root: &EnvPath,
		mut request: WalkRequest,
	) -> Result<WalkStream, ClientError> {
		request.root_uri = self.client.path_uri(root)?;
		let stream = self
			.client
			.open(
				client_frame::Body::Data(DataRequest {
					body: Some(data_request::Body::Walk(request)),
					..DataRequest::default()
				}),
				Some(&self.scope),
			)
			.await?;
		Ok(WalkStream { stream })
	}

	/// Starts a streaming workspace search rooted at a typed environment path.
	pub async fn search(
		&self,
		root: &EnvPath,
		mut request: SearchRequest,
	) -> Result<SearchStream, ClientError> {
		request.walk.get_or_insert_default().root_uri = self.client.path_uri(root)?;
		let stream = self
			.client
			.open(
				client_frame::Body::Data(DataRequest {
					body: Some(data_request::Body::Search(request)),
					..DataRequest::default()
				}),
				Some(&self.scope),
			)
			.await?;
		Ok(SearchStream { stream })
	}

	/// Opens a scoped exec session rooted at `cwd`.
	pub async fn open_session(
		&self,
		cwd: &EnvPath,
		mut request: OpenSessionRequest,
	) -> Result<OpenSessionResponse, ClientError> {
		request.cwd_uri = self.client.path_uri(cwd)?;
		self
			.client
			.open_session_scoped(request, Some(&self.scope))
			.await
	}

	/// Closes a scoped exec session.
	pub async fn close_session(
		&self,
		request: CloseSessionRequest,
	) -> Result<CloseSessionResponse, ClientError> {
		match self
			.client
			.one_shot(client_frame::Body::CloseSession(request), Some(&self.scope))
			.await?
		{
			server_frame::Body::SessionClosed(response) => Ok(response),
			_ => Err(ClientError::UnexpectedResponse { expected: "CloseSessionResponse" }),
		}
	}

	/// Starts one guarded command inside a scoped exec session.
	pub async fn exec(&self, request: ExecRequest) -> Result<ExecRun, ClientError> {
		self.client.exec_scoped(request, Some(&self.scope)).await
	}

	/// Checks whether a scoped content-addressed blob is present.
	pub async fn blob_stat(&self, request: StatRequest) -> Result<StatResponse, ClientError> {
		match self
			.client
			.one_shot(client_frame::Body::BlobStat(request), Some(&self.scope))
			.await?
		{
			server_frame::Body::BlobStat(response) => Ok(response),
			_ => Err(ClientError::UnexpectedResponse { expected: "StatResponse" }),
		}
	}

	/// Starts a scoped streaming blob download.
	pub async fn blob_get(&self, request: GetRequest) -> Result<BlobDownload, ClientError> {
		let stream = self
			.client
			.open(client_frame::Body::BlobGet(request), Some(&self.scope))
			.await?;
		Ok(BlobDownload { stream })
	}

	/// Starts a scoped streaming blob upload.
	pub fn blob_put(&self) -> Result<BlobUpload, ClientError> {
		let request_id = self.client.allocate_request_id()?;
		let stream = self.client.register(request_id);
		Ok(BlobUpload {
			client: self.client.clone(),
			scope: Some(self.scope.clone()),
			request_id,
			stream,
		})
	}
}

impl DocumentLease {
	/// Returns the opaque lease id, valid only on this connection.
	#[must_use]
	pub const fn id(&self) -> &Bytes {
		&self.lease_id
	}

	/// Returns the head pinned when this lease opened.
	#[must_use]
	pub const fn head(&self) -> &DocumentHead {
		&self.head
	}

	/// Returns the correlated event stream while retaining lease ownership.
	pub const fn events(&mut self) -> &mut DocumentEvents {
		&mut self.events
	}

	/// Explicitly closes this lease.
	pub async fn close(mut self) -> Result<(), ClientError> {
		let request = document_request(document_op::Op::Close(document::CloseDocumentRequest {
			lease_id: self.lease_id.clone(),
		}));
		self.events.stream.finish();
		let response = self
			.client
			.one_shot(client_frame::Body::Data(request), Some(&self.scope))
			.await?;
		let _ = document_result(response_data_body(response)?, "CloseDocumentResponse", |result| {
			let document_result::Result::Closed(closed) = result else {
				return None;
			};
			Some(closed)
		})?;
		self.released = true;
		Ok(())
	}
}

impl Drop for DocumentLease {
	fn drop(&mut self) {
		if self.released {
			return;
		}
		self.events.stream.finish();
		let _ = self
			.client
			.inner
			.lease_close
			.try_send(LeaseClose { lease_id: self.lease_id.clone(), scope: self.scope.clone() });
		self.released = true;
	}
}

impl DocumentRead {
	/// Returns the head from which this response was read.
	#[must_use]
	pub const fn head(&self) -> &DocumentHead {
		self
			.response
			.head
			.as_ref()
			.expect("DocumentRead is constructed only from a response containing a head")
	}

	/// Returns the complete content bytes, or `None` when the response contains
	/// requested slices.
	#[must_use]
	pub const fn content(&self) -> Option<&Bytes> {
		let Some(read_document_response::Body::Content(content)) = self.response.body.as_ref() else {
			return None;
		};
		Some(content)
	}

	/// Returns disjoint content slices when ranges were requested.
	#[must_use]
	pub const fn slices(&self) -> Option<&document::ContentSlices> {
		let Some(read_document_response::Body::Slices(slices)) = self.response.body.as_ref() else {
			return None;
		};
		Some(slices)
	}

	/// Consumes this wrapper and returns the canonical wire response.
	#[must_use]
	pub fn into_response(self) -> ReadDocumentResponse {
		self.response
	}
}

impl InProcessEnvTransport {
	/// Receives the next client frame asynchronously.
	pub async fn recv(&self) -> Result<ClientFrame, flume::RecvError> {
		self.requests.recv_async().await
	}

	/// Sends one server frame asynchronously.
	pub async fn send(&self, frame: ServerFrame) -> Result<(), Box<flume::SendError<ServerFrame>>> {
		self.responses.send_async(frame).await.map_err(Box::new)
	}

	/// Splits this transport into the server's receive and send endpoints.
	#[must_use]
	pub fn into_parts(self) -> (Receiver<ClientFrame>, Sender<ServerFrame>) {
		(self.requests, self.responses)
	}
}

impl RequestStream {
	/// Returns the correlation identifier carried by every frame in this stream.
	#[must_use]
	pub const fn request_id(&self) -> u64 {
		self.request_id
	}

	/// Waits for the next correlated server frame.
	pub async fn next(&mut self) -> Result<Option<ServerFrame>, ClientError> {
		if self.finished {
			return Ok(None);
		}
		if let Ok(frame) = self.receiver.recv_async().await {
			Ok(Some(frame))
		} else {
			self.finish();
			Err(ClientError::TransportClosed)
		}
	}

	/// Explicitly cancels this request and closes its local response route.
	///
	/// Unlike [`RunGuard`], a raw request stream is not cancelled on drop. This
	/// keeps the stream returned by detached work safe to discard; callers that
	/// own an ordinary long-lived request can cancel it explicitly here.
	pub fn cancel(mut self) {
		if let Some(client) = self.client.upgrade() {
			let _ = client.cancel.try_send(self.request_id);
		}
		self.finish();
	}

	fn finish(&mut self) {
		self.unregister();
		self.finished = true;
	}

	fn unregister(&self) {
		if let Some(client) = self.client.upgrade() {
			client.pending.lock().remove(&self.request_id);
		}
	}
}

impl Drop for RequestStream {
	fn drop(&mut self) {
		self.unregister();
	}
}

impl Invocation {
	/// Returns the invocation's logical identifier.
	#[must_use]
	pub fn invocation_id(&self) -> &str {
		&self.id
	}

	/// Returns the request-scoped cancellation guard.
	#[must_use]
	pub const fn guard(&self) -> &RunGuard {
		self
			.guard
			.as_ref()
			.expect("invocation guard exists until relinquished")
	}

	/// Relays one raw provider argument fragment without validation.
	pub async fn arg_text(&self, fragment: Str) -> Result<(), ClientError> {
		self
			.client
			.send(
				self.stream.request_id,
				client_frame::Body::ArgText(ArgText {
					invocation_id: self.id.to_string(),
					fragment: fragment.to_string(),
					..ArgText::default()
				}),
				None,
			)
			.await
	}

	/// Sends the exact committed argument bytes, authorizing effects env-side.
	pub async fn commit_args(
		&self,
		raw: Bytes,
		effect_token: Bytes,
		authorized_at_ms: u64,
		effects: Option<omp_proto::policy::v1::EffectEnvelope>,
	) -> Result<(), ClientError> {
		self
			.client
			.send(
				self.stream.request_id,
				client_frame::Body::ArgsCommitted(ArgsCommitted {
					invocation_id: self.id.to_string(),
					raw,
					effect_token,
					authorized_at_ms,
					effects,
					..ArgsCommitted::default()
				}),
				None,
			)
			.await
	}

	/// Answers the environment's admission query for this invocation.
	pub async fn admit(&self, mut admission: Admission) -> Result<(), ClientError> {
		admission.invocation_id = self.id.to_string();
		self
			.client
			.send(self.stream.request_id, client_frame::Body::Admission(admission), None)
			.await
	}

	/// Sends cooperative interrupt steering to this invocation only.
	pub async fn interrupt(&self, reason: Str) -> Result<(), ClientError> {
		self
			.client
			.send(
				self.stream.request_id,
				client_frame::Body::Interrupt(Interrupt {
					invocation_id: self.id.to_string(),
					reason: reason.to_string(),
					..Interrupt::default()
				}),
				None,
			)
			.await
	}

	/// Waits for the next typed invocation event.
	pub async fn next_event(&mut self) -> Result<Option<InvocationEvent>, ClientError> {
		let Some(frame) = self.stream.next().await? else {
			return Ok(None);
		};
		let body = match response_body(frame) {
			Ok(body) => body,
			Err(error) => {
				self.complete();
				return Err(error);
			},
		};
		match body {
			server_frame::Body::InvocationAccepted(event) => {
				Ok(Some(InvocationEvent::Accepted(event)))
			},
			server_frame::Body::AdmitInvocation(event) => Ok(Some(InvocationEvent::Admission(event))),
			server_frame::Body::Update(event) => Ok(Some(InvocationEvent::Update(event))),
			server_frame::Body::Verdict(event) => {
				self.complete();
				Ok(Some(InvocationEvent::Verdict(event)))
			},
			server_frame::Body::EventStreamError(event) => {
				let error = stream_lost(event);
				self.complete();
				Err(error)
			},
			_ => {
				self.complete();
				Err(ClientError::UnexpectedResponse { expected: "invocation event" })
			},
		}
	}

	/// Explicitly leaves detached work owned by the environment service.
	///
	/// The returned stream can continue observing its terminal event, but its
	/// drop no longer requests cancellation.
	#[must_use]
	pub fn relinquish(mut self) -> RequestStream {
		if let Some(guard) = self.guard.take() {
			guard.relinquish();
		}
		self.stream
	}

	fn complete(&mut self) {
		if let Some(guard) = self.guard.take() {
			guard.relinquish();
		}
		self.stream.finish();
	}
}

impl ExecRun {
	/// Returns the request-scoped command cancellation guard.
	#[must_use]
	pub const fn guard(&self) -> &RunGuard {
		self
			.guard
			.as_ref()
			.expect("exec guard exists until relinquished")
	}

	/// Writes stdin bytes or EOF to this command.
	pub async fn stdin(&self, frame: StdinFrame) -> Result<(), ClientError> {
		self
			.client
			.send(self.stream.request_id, client_frame::Body::Stdin(frame), self.scope.as_ref())
			.await
	}

	/// Sends a signal to this command.
	pub async fn signal(&self, request: SignalRequest) -> Result<(), ClientError> {
		self
			.client
			.send(self.stream.request_id, client_frame::Body::Signal(request), self.scope.as_ref())
			.await
	}

	/// Resizes this command's PTY.
	pub async fn resize(
		&self,
		request: omp_proto::env::v1::ResizeRequest,
	) -> Result<(), ClientError> {
		self
			.client
			.send(self.stream.request_id, client_frame::Body::Resize(request), self.scope.as_ref())
			.await
	}

	/// Waits for the next typed command event.
	pub async fn next_event(&mut self) -> Result<Option<ExecEvent>, ClientError> {
		let Some(frame) = self.stream.next().await? else {
			return Ok(None);
		};
		let body = match response_body(frame) {
			Ok(body) => body,
			Err(error) => {
				self.complete();
				return Err(error);
			},
		};
		match body {
			server_frame::Body::ExecStarted(event) => Ok(Some(ExecEvent::Started(event))),
			server_frame::Body::Output(event) => Ok(Some(ExecEvent::Output(event))),
			server_frame::Body::Exit(event) => {
				self.complete();
				Ok(Some(ExecEvent::Exit(event)))
			},
			server_frame::Body::EventStreamError(event) => {
				let error = stream_lost(event);
				self.complete();
				Err(error)
			},
			_ => {
				self.complete();
				Err(ClientError::UnexpectedResponse { expected: "exec event" })
			},
		}
	}

	/// Explicitly leaves a detached command owned by the environment service.
	#[must_use]
	pub fn relinquish(mut self) -> RequestStream {
		if let Some(guard) = self.guard.take() {
			guard.relinquish();
		}
		self.stream
	}

	fn complete(&mut self) {
		if let Some(guard) = self.guard.take() {
			guard.relinquish();
		}
		self.stream.finish();
	}
}

impl ProcessAttachment {
	/// Waits for the next ordered attachment event.
	pub async fn next_event(&mut self) -> Result<Option<ProcessAttachmentEvent>, ClientError> {
		let Some(frame) = self.stream.next().await? else {
			return Ok(None);
		};
		let body = match response_body(frame) {
			Ok(body) => body,
			Err(error) => {
				self.stream.finish();
				return Err(error);
			},
		};
		match body {
			server_frame::Body::OutputAttached(event) => {
				Ok(Some(ProcessAttachmentEvent::Attached(event)))
			},
			server_frame::Body::ProcessOutput(event) => {
				Ok(Some(ProcessAttachmentEvent::Output(event)))
			},
			server_frame::Body::ProcessState(event) => Ok(Some(ProcessAttachmentEvent::State(event))),
			server_frame::Body::EventStreamError(event) => {
				let error = stream_lost(event);
				self.stream.finish();
				Err(error)
			},
			_ => {
				self.stream.finish();
				Err(ClientError::UnexpectedResponse { expected: "process attachment event" })
			},
		}
	}

	/// Stops the server-side output attachment.
	pub fn cancel(self) {
		self.stream.cancel();
	}
}

impl DocumentEvents {
	/// Waits for the next contiguous document event.
	pub async fn next_event(&mut self) -> Result<Option<DocumentEvent>, ClientError> {
		let Some(frame) = self.stream.next().await? else {
			return Ok(None);
		};
		match response_body(frame) {
			Ok(server_frame::Body::DataEvent(event)) => {
				if let Some(data_event::Body::Document(event)) = event.body {
					Ok(Some(event))
				} else {
					self.stream.finish();
					Err(ClientError::UnexpectedResponse { expected: "DocumentEvent" })
				}
			},
			Ok(server_frame::Body::EventStreamError(event)) => {
				let error = stream_lost(event);
				self.stream.finish();
				Err(error)
			},
			Ok(_) => {
				self.stream.finish();
				Err(ClientError::UnexpectedResponse { expected: "DocumentEvent" })
			},
			Err(error) => {
				self.stream.finish();
				Err(error)
			},
		}
	}
}

impl LspEvents {
	/// Waits for the initial bindings or next contiguous LSP event.
	pub async fn next_event(&mut self) -> Result<Option<LspStreamEvent>, ClientError> {
		let Some(frame) = self.stream.next().await? else {
			return Ok(None);
		};
		match response_body(frame) {
			Ok(server_frame::Body::Data(response)) => {
				match document_result(response, "GetLspBindingsResponse", |result| {
					let document_result::Result::LspBindings(bindings) = result else {
						return None;
					};
					Some(bindings)
				}) {
					Ok(bindings) => Ok(Some(LspStreamEvent::Bindings(bindings))),
					Err(error) => {
						self.stream.finish();
						Err(error)
					},
				}
			},
			Ok(server_frame::Body::DataEvent(event)) => match event.body {
				Some(data_event::Body::Lsp(event)) => Ok(Some(LspStreamEvent::Event(event))),
				Some(data_event::Body::LspBinding(event)) => Ok(Some(LspStreamEvent::Binding(event))),
				_ => {
					self.stream.finish();
					Err(ClientError::UnexpectedResponse { expected: "LSP registry event" })
				},
			},
			Ok(server_frame::Body::EventStreamError(event)) => {
				let error = stream_lost(event);
				self.stream.finish();
				Err(error)
			},
			Ok(_) => {
				self.stream.finish();
				Err(ClientError::UnexpectedResponse { expected: "LSP registry event" })
			},
			Err(error) => {
				self.stream.finish();
				Err(error)
			},
		}
	}
}

impl WalkStream {
	/// Waits for the next walk entry or terminal accounting event.
	pub async fn next_event(&mut self) -> Result<Option<WalkEvent>, ClientError> {
		let Some(frame) = self.stream.next().await? else {
			return Ok(None);
		};
		match response_body(frame) {
			Ok(server_frame::Body::DataEvent(event)) => match event.body {
				Some(data_event::Body::WalkEntry(event)) => Ok(Some(WalkEvent::Entry(event))),
				Some(data_event::Body::WalkComplete(event)) => {
					self.stream.finish();
					Ok(Some(WalkEvent::Complete(event)))
				},
				_ => {
					self.stream.finish();
					Err(ClientError::UnexpectedResponse { expected: "walk event" })
				},
			},
			Ok(server_frame::Body::EventStreamError(event)) => {
				let error = stream_lost(event);
				self.stream.finish();
				Err(error)
			},
			Ok(_) => {
				self.stream.finish();
				Err(ClientError::UnexpectedResponse { expected: "walk event" })
			},
			Err(error) => {
				self.stream.finish();
				Err(error)
			},
		}
	}

	/// Cancels the server-side walk.
	pub fn cancel(self) {
		self.stream.cancel();
	}
}

impl SearchStream {
	/// Waits for the next search match or terminal accounting event.
	pub async fn next_event(&mut self) -> Result<Option<SearchEvent>, ClientError> {
		let Some(frame) = self.stream.next().await? else {
			return Ok(None);
		};
		match response_body(frame) {
			Ok(server_frame::Body::DataEvent(event)) => match event.body {
				Some(data_event::Body::SearchMatch(event)) => Ok(Some(SearchEvent::Match(event))),
				Some(data_event::Body::SearchComplete(event)) => {
					self.stream.finish();
					Ok(Some(SearchEvent::Complete(event)))
				},
				_ => {
					self.stream.finish();
					Err(ClientError::UnexpectedResponse { expected: "search event" })
				},
			},
			Ok(server_frame::Body::EventStreamError(event)) => {
				let error = stream_lost(event);
				self.stream.finish();
				Err(error)
			},
			Ok(_) => {
				self.stream.finish();
				Err(ClientError::UnexpectedResponse { expected: "search event" })
			},
			Err(error) => {
				self.stream.finish();
				Err(error)
			},
		}
	}

	/// Cancels the server-side search.
	pub fn cancel(self) {
		self.stream.cancel();
	}
}

impl DataStream {
	/// Waits for the next event or terminal response.
	pub async fn next(&mut self) -> Result<Option<DataStreamItem>, ClientError> {
		let Some(frame) = self.stream.next().await? else {
			return Ok(None);
		};
		match response_body(frame) {
			Ok(server_frame::Body::DataEvent(event)) => Ok(Some(DataStreamItem::Event(event))),
			Ok(server_frame::Body::Data(response)) => {
				self.stream.finish();
				Ok(Some(DataStreamItem::Response(response)))
			},
			Ok(server_frame::Body::EventStreamError(event)) => {
				let error = stream_lost(event);
				self.stream.finish();
				Err(error)
			},
			Ok(_) => {
				self.stream.finish();
				Err(ClientError::UnexpectedResponse { expected: "DATA event or response" })
			},
			Err(error) => {
				self.stream.finish();
				Err(error)
			},
		}
	}

	/// Cancels the server-side DATA stream.
	pub fn cancel(self) {
		self.stream.cancel();
	}
}

impl DapStream {
	/// Waits for the next typed DAP session, action, output, or event frame.
	pub async fn next_event(&mut self) -> Result<Option<DapStreamEvent>, ClientError> {
		let Some(frame) = self.stream.next().await? else {
			return Ok(None);
		};
		match response_body(frame) {
			Ok(server_frame::Body::Data(DataResponse {
				body: Some(data_response::Body::DapSession(session)),
				..
			})) => {
				self.stream.finish();
				Ok(Some(DapStreamEvent::Session(session)))
			},
			Ok(server_frame::Body::Data(DataResponse {
				body: Some(data_response::Body::DapAction(action)),
				..
			})) => {
				self.stream.finish();
				Ok(Some(DapStreamEvent::Action(action)))
			},
			Ok(server_frame::Body::DataEvent(DataEvent {
				body: Some(data_event::Body::DapOutput(output)),
				..
			})) => Ok(Some(DapStreamEvent::Output(output))),
			Ok(server_frame::Body::DataEvent(DataEvent {
				body: Some(data_event::Body::DapEvent(event)),
				..
			})) => Ok(Some(DapStreamEvent::Event(event))),
			Ok(_) => Err(ClientError::UnexpectedResponse { expected: "typed DAP frame" }),
			Err(error) => {
				self.stream.finish();
				Err(error)
			},
		}
	}

	/// Cancels the server-side DAP request or subscription.
	pub fn cancel(self) {
		self.stream.cancel();
	}
}

impl ResourceCompletionStream {
	/// Waits for the next completion or terminal accounting frame.
	pub async fn next_event(&mut self) -> Result<Option<ResourceCompletionEvent>, ClientError> {
		let Some(frame) = self.stream.next().await? else {
			return Ok(None);
		};
		match response_body(frame) {
			Ok(server_frame::Body::DataEvent(DataEvent {
				body: Some(data_event::Body::ResourceCompletion(completion)),
				..
			})) => Ok(Some(ResourceCompletionEvent::Completion(completion))),
			Ok(server_frame::Body::DataEvent(DataEvent {
				body: Some(data_event::Body::ResourceCompletionComplete(complete)),
				..
			})) => {
				self.stream.finish();
				Ok(Some(ResourceCompletionEvent::Complete(complete)))
			},
			Ok(_) => Err(ClientError::UnexpectedResponse { expected: "resource completion frame" }),
			Err(error) => {
				self.stream.finish();
				Err(error)
			},
		}
	}

	/// Cancels the server-side completion request.
	pub fn cancel(self) {
		self.stream.cancel();
	}
}

impl McpSubscription {
	/// Waits for the next MCP notification or lifecycle transition.
	pub async fn next_event(&mut self) -> Result<Option<McpSubscriptionEvent>, ClientError> {
		let Some(frame) = self.stream.next().await? else {
			return Ok(None);
		};
		match response_body(frame) {
			Ok(server_frame::Body::DataEvent(DataEvent {
				body: Some(data_event::Body::McpNotification(notification)),
				..
			})) => Ok(Some(McpSubscriptionEvent::Notification(notification))),
			Ok(server_frame::Body::DataEvent(DataEvent {
				body: Some(data_event::Body::McpStatus(status)),
				..
			})) => Ok(Some(McpSubscriptionEvent::Status(status))),
			Ok(_) => Err(ClientError::UnexpectedResponse { expected: "MCP subscription frame" }),
			Err(error) => {
				self.stream.finish();
				Err(error)
			},
		}
	}

	/// Cancels the MCP subscription.
	pub fn cancel(self) {
		self.stream.cancel();
	}
}

impl BlobDownload {
	/// Waits for the next ordered chunk or terminal completion marker.
	pub async fn next_event(&mut self) -> Result<Option<BlobDownloadEvent>, ClientError> {
		let Some(frame) = self.stream.next().await? else {
			return Ok(None);
		};
		let body = match response_body(frame) {
			Ok(body) => body,
			Err(error) => {
				self.stream.finish();
				return Err(error);
			},
		};
		match body {
			server_frame::Body::BlobChunk(chunk) => Ok(Some(BlobDownloadEvent::Chunk(chunk))),
			server_frame::Body::BlobGetComplete(complete) => {
				self.stream.finish();
				Ok(Some(BlobDownloadEvent::Complete(complete)))
			},
			server_frame::Body::EventStreamError(event) => {
				let error = stream_lost(event);
				self.stream.finish();
				Err(error)
			},
			_ => {
				self.stream.finish();
				Err(ClientError::UnexpectedResponse { expected: "blob chunk or completion" })
			},
		}
	}

	/// Stops this download before its completion marker.
	pub fn cancel(self) {
		self.stream.cancel();
	}
}

impl BlobUpload {
	/// Returns the correlation identifier shared by every upload frame.
	#[must_use]
	pub const fn request_id(&self) -> u64 {
		self.request_id
	}

	/// Sends the next ordered blob chunk.
	pub async fn send_chunk(&self, chunk: Chunk) -> Result<(), ClientError> {
		self
			.client
			.send(self.request_id, client_frame::Body::BlobPutChunk(chunk), self.scope.as_ref())
			.await
	}

	/// Cancels this upload without making its staged bytes visible.
	pub fn abort(self) {
		self.stream.cancel();
	}

	/// Commits the upload and waits for its content identity.
	pub async fn commit(mut self) -> Result<PutResponse, ClientError> {
		self
			.client
			.send(
				self.request_id,
				client_frame::Body::BlobPutCommit(CommitBlobPut::default()),
				self.scope.as_ref(),
			)
			.await?;
		let frame = self
			.stream
			.next()
			.await?
			.ok_or(ClientError::TransportClosed)?;
		self.stream.finish();
		match response_body(frame)? {
			server_frame::Body::BlobPut(response) => Ok(response),
			server_frame::Body::EventStreamError(event) => Err(stream_lost(event)),
			_ => Err(ClientError::UnexpectedResponse { expected: "PutResponse" }),
		}
	}
}

fn response_body(frame: ServerFrame) -> Result<server_frame::Body, ClientError> {
	match frame.body {
		Some(server_frame::Body::Error(error)) => Err(protocol_error(error)),
		Some(body) => Ok(body),
		None => Err(ClientError::UnexpectedResponse { expected: "nonempty server frame" }),
	}
}

const fn protocol_error(error: ProtocolError) -> ClientError {
	if error.code == ProtocolErrorCode::Uncommitted as i32 {
		ClientError::EffectsNotAuthorized(error)
	} else {
		ClientError::Protocol(error)
	}
}

fn response_data(frame: ServerFrame) -> Result<DataResponse, ClientError> {
	response_data_body(response_body(frame)?)
}

fn response_data_body(body: server_frame::Body) -> Result<DataResponse, ClientError> {
	match body {
		server_frame::Body::Data(response) => Ok(response),
		server_frame::Body::EventStreamError(event) => Err(stream_lost(event)),
		_ => Err(ClientError::UnexpectedResponse { expected: "DataResponse" }),
	}
}

fn document_request(operation: document_op::Op) -> DataRequest {
	DataRequest {
		body: Some(data_request::Body::Document(omp_proto::env::v1::DocumentOp {
			op: Some(operation),
			..omp_proto::env::v1::DocumentOp::default()
		})),
		..DataRequest::default()
	}
}

fn document_result<T>(
	response: DataResponse,
	expected: &'static str,
	select: impl FnOnce(document_result::Result) -> Option<T>,
) -> Result<T, ClientError> {
	let Some(data_response::Body::Document(document)) = response.body else {
		return Err(ClientError::UnexpectedResponse { expected });
	};
	document
		.result
		.and_then(select)
		.ok_or(ClientError::UnexpectedResponse { expected })
}

fn lease_target(lease: &DocumentLease) -> document::DocumentTarget {
	document::DocumentTarget {
		target: Some(document_target::Target::LeaseId(lease.lease_id.clone())),
	}
}

fn is_revision_pinned(head: &DocumentHead) -> bool {
	head
		.document
		.as_ref()
		.is_some_and(|document| !document.id.is_empty() && !document.uri.is_empty())
		&& head
			.revision
			.as_ref()
			.is_some_and(|revision| revision.content_hash.len() == 32)
}

fn stream_lost(event: EventStreamError) -> ClientError {
	let stream = EventStreamKind::try_from(event.stream).unwrap_or(EventStreamKind::Unspecified);
	let reopen_guidance = match stream {
		EventStreamKind::Document => "discard the closed lease and reopen the document",
		EventStreamKind::LspRegistry => {
			"reconnect, reopen documents, and re-query LSP bindings before revision-sensitive work"
		},
		EventStreamKind::Walk => "restart the walk from a new request",
		EventStreamKind::Search => "restart the search from a new request",
		EventStreamKind::Invocation => "restart the invocation from its durable boundary",
		EventStreamKind::Exec => "open a new command and do not infer missing output",
		EventStreamKind::ProcessOutput | EventStreamKind::ProcessState => {
			"reattach and resume after the last observed sequence"
		},
		EventStreamKind::Dap => "reopen the DAP session before issuing another action",
		EventStreamKind::ResourceCompletion => "restart completion from a fresh catalog revision",
		EventStreamKind::McpNotification => "resubscribe from the last observed sequence",
		EventStreamKind::Unspecified => "reopen the resource before continuing",
	};
	ClientError::StreamLost(StreamLost {
		stream,
		skipped: event.skipped_events,
		reason: Str::from(event.message),
		reopen_guidance,
	})
}

const fn ensure_worker_data(request: &DataRequest) -> Result<(), ClientError> {
	match request.body.as_ref() {
		Some(
			data_request::Body::Document(_)
			| data_request::Body::Walk(_)
			| data_request::Body::Search(_)
			| data_request::Body::RepositorySnapshot(_)
			| data_request::Body::PrivilegedMutation(_)
			| data_request::Body::ExecSession(_)
			| data_request::Body::DapLaunch(_)
			| data_request::Body::DapAttach(_)
			| data_request::Body::DapAction(_)
			| data_request::Body::Resource(_),
		) => Ok(()),
		_ => Err(ClientError::ScopedOperationDenied),
	}
}

fn channel<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
	if capacity == 0 {
		flume::unbounded()
	} else {
		flume::bounded(capacity)
	}
}

fn route_responses(
	client: Weak<ClientInner>,
	incoming: Receiver<ServerFrame>,
	events: Sender<ServerFrame>,
) {
	while let Ok(frame) = incoming.recv() {
		let Some(client) = client.upgrade() else {
			break;
		};
		if frame.request_id == 0 {
			let is_hello_response = matches!(
				frame.body.as_ref(),
				Some(server_frame::Body::Hello(_) | server_frame::Body::Error(_))
			);
			if is_hello_response && let Some(waiter) = client.hello_waiter.lock().take() {
				let _ = waiter.send(frame);
			} else {
				let _ = events.send(frame);
			}
			continue;
		}
		if let Some(server_frame::Body::AdmitInvocation(query)) = frame.body.as_ref()
			&& let Some(admitter) = client.admitter.lock().clone()
		{
			admitter.dispatch(Arc::clone(&client), frame.request_id, query.clone());
			continue;
		}
		let target = client.pending.lock().get(&frame.request_id).cloned();
		if let Some(target) = target {
			let _ = target.send(frame);
		}
	}
	if let Some(client) = client.upgrade() {
		client.pending.lock().clear();
		client.hello_waiter.lock().take();
	}
}

fn route_cancellations(cancellations: Receiver<u64>, outgoing: Sender<ClientFrame>) {
	while let Ok(request_id) = cancellations.recv() {
		let frame = ClientFrame {
			request_id: 0,
			body: Some(client_frame::Body::Cancel(CancelRequest {
				target: Some(cancel_request::Target::TargetRequestId(request_id)),
				..CancelRequest::default()
			})),
			..ClientFrame::default()
		};
		if outgoing.send(frame).is_err() {
			break;
		}
	}
}

fn route_lease_closes(client: Weak<ClientInner>, closes: Receiver<LeaseClose>) {
	while let Ok(close) = closes.recv() {
		let Some(client) = client.upgrade() else {
			break;
		};
		let Ok(request_id) =
			client
				.next_id
				.try_update(Ordering::Relaxed, Ordering::Relaxed, |request_id| {
					request_id.checked_add(1)
				})
		else {
			continue;
		};
		let request = document_request(document_op::Op::Close(document::CloseDocumentRequest {
			lease_id: close.lease_id,
		}));
		let frame = ClientFrame {
			request_id,
			body: Some(client_frame::Body::Data(request)),
			scope: Some(close.scope.wire()),
			..ClientFrame::default()
		};
		if client.outgoing.send(frame).is_err() {
			break;
		}
	}
}
