//! Extension-host CONTROL request correlation and argument-stream fencing.

use std::collections::BTreeMap;

use bytes::Bytes;
use omp_agent::{
	JournalAuthor, JournalCustomEntry, JournalOperation, JournalQuery, JournalRequest,
	JournalRequestStamp, PendingCustomEntry,
	control::{ControlError as AgentControlError, ControlSender},
};
use omp_core::{Principal, Provenance, Str};
use omp_proto::{
	bounds::{
		PULL_ALIAS_MAX_COUNT, PULL_CHUNK_MAX_BYTES, PULL_EXPECTED_MAX_BYTES, PULL_NAME_MAX_BYTES,
		PULL_PATH_MAX_SEGMENTS,
	},
	env::v1::{ArgText, ArgsCommitted, Interrupt},
	thread::v1::{Part, part},
	toolhost::v1::{
		AdoptArtifact, AppendEntriesAtomic, AppendEntry, ArtifactRow, DeclareEntryKinds,
		EntryAppended, JournalHostEnvelope, JournalRow, JournalWorkerEnvelope, ListArtifacts,
		ListSessions, PinArtifact, PullReply, PullRequest, QueryJournal, QueryUsage, SessionRow,
		StatArtifact, StateCas, StateGet, StateScope, StateWatch, UsageReport, journal_host_envelope,
		journal_worker_envelope,
	},
};
use omp_storage::{
	blob::BlobRef,
	transcript::msg::{Content, UserBlock},
};
use serde_json::value::RawValue;
use thiserror::Error;

/// Maximum number of unresolved cursor pulls accepted for one invocation.
pub const MAX_PENDING_PULLS: usize = 1;

/// Correlation established by the host between environment and worker identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvocationCorrelation {
	/// Nonzero tool-host envelope request identifier.
	pub request_id:    u64,
	/// Environment-plane invocation identifier.
	pub invocation_id: Str,
	/// Worker-plane call identifier.
	pub call_id:       Str,
	/// Whether the registered declaration selected streaming arguments.
	pub streams_args:  bool,
}

#[derive(Debug)]
struct InvocationState {
	correlation: InvocationCorrelation,
	pull_open:   bool,
}

/// Typed protocol failures produced before a CONTROL frame is staged.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ControlError {
	/// Request id zero is reserved for registration, events, and health traffic.
	#[error("request id zero cannot identify an invocation")]
	ZeroRequestId,
	/// The environment invocation id is already live.
	#[error("invocation {0} is already mapped")]
	DuplicateInvocation(Str),
	/// A frame names no live invocation.
	#[error("invocation {0} is not live")]
	UnknownInvocation(Str),
	/// A frame's request id is stale or unknown.
	#[error("request id {0} is stale or unknown")]
	StaleRequest(u64),
	/// A worker call id does not match the request envelope.
	#[error("call id does not match request id {request_id}")]
	CallMismatch {
		/// Request envelope identifier.
		request_id: u64,
	},
	/// The declaration did not opt into speculative argument streaming.
	#[error("tool declaration did not enable streams_args")]
	StreamingNotDeclared,
	/// A second pull was attempted before the first pull completed.
	#[error("only one argument pull may be outstanding")]
	PullBusy,
	/// A pull or reply violated its declared allocation bound.
	#[error("argument pull violates the {field} bound")]
	PullBound {
		/// Name of the bounded field.
		field: &'static str,
	},
	/// A pull reply tried to complete when no pull was outstanding.
	#[error("pull reply has no outstanding pull")]
	NoOutstandingPull,
	/// The received CONTROL body is known but unsupported in this state.
	#[error("unsupported CONTROL frame: {0}")]
	Unsupported(&'static str),
}

/// Single-actor invocation map for multiplexed extension-host CONTROL traffic.
///
/// All mutation happens on the owning actor. The type deliberately contains no
/// lock: this preserves serialized callback entry unless an extension opts into
/// a separate concurrent host actor.
#[derive(Debug)]
pub struct HostRequestMap {
	next_request_id: u64,
	by_invocation:   BTreeMap<Str, u64>,
	by_request:      BTreeMap<u64, InvocationState>,
}

impl Default for HostRequestMap {
	fn default() -> Self {
		Self::new()
	}
}

impl HostRequestMap {
	/// Creates an empty map whose first invocation receives request id one.
	#[must_use]
	pub const fn new() -> Self {
		Self {
			next_request_id: 1,
			by_invocation:   BTreeMap::new(),
			by_request:      BTreeMap::new(),
		}
	}

	/// Establishes a live invocation mapping.
	///
	/// # Errors
	/// Returns [`ControlError::DuplicateInvocation`] if `invocation_id` is live.
	pub fn open(
		&mut self,
		invocation_id: Str,
		call_id: Str,
		streams_args: bool,
	) -> Result<InvocationCorrelation, ControlError> {
		if self.by_invocation.contains_key(&invocation_id) {
			return Err(ControlError::DuplicateInvocation(invocation_id));
		}
		let request_id = self.allocate_request_id();
		let correlation = InvocationCorrelation {
			request_id,
			invocation_id: invocation_id.clone(),
			call_id,
			streams_args,
		};
		self.by_invocation.insert(invocation_id, request_id);
		self.by_request.insert(request_id, InvocationState {
			correlation: correlation.clone(),
			pull_open:   false,
		});
		Ok(correlation)
	}

	/// Resolves and validates a forwarded `ArgText` frame.
	///
	/// # Errors
	/// Returns a typed stale or declaration error before the frame is staged.
	pub fn arg_text(&self, frame: &ArgText) -> Result<&InvocationCorrelation, ControlError> {
		let state = self.by_environment_id(frame.invocation_id.as_str())?;
		if !state.correlation.streams_args {
			return Err(ControlError::StreamingNotDeclared);
		}
		Ok(&state.correlation)
	}

	/// Resolves and validates a forwarded `ArgsCommitted` frame.
	///
	/// # Errors
	/// Returns a typed stale error before the frame is staged.
	pub fn args_committed(
		&self,
		frame: &ArgsCommitted,
	) -> Result<&InvocationCorrelation, ControlError> {
		self
			.by_environment_id(frame.invocation_id.as_str())
			.map(|state| &state.correlation)
	}

	/// Resolves and validates a forwarded `Interrupt` frame.
	///
	/// # Errors
	/// Returns a typed stale error before the frame is staged.
	pub fn interrupt(&self, frame: &Interrupt) -> Result<&InvocationCorrelation, ControlError> {
		self
			.by_environment_id(frame.invocation_id.as_str())
			.map(|state| &state.correlation)
	}

	/// Takes the sole outstanding pull slot after validating its request
	/// quartet.
	///
	/// # Errors
	/// Returns a stale, correlation, declaration, busy, or allocation-bound
	/// error.
	pub fn begin_pull(
		&mut self,
		request_id: u64,
		pull: &PullRequest,
	) -> Result<&InvocationCorrelation, ControlError> {
		if request_id == 0 {
			return Err(ControlError::ZeroRequestId);
		}
		validate_pull_bounds(pull)?;
		let state = self
			.by_request
			.get_mut(&request_id)
			.ok_or(ControlError::StaleRequest(request_id))?;
		if pull.call_id != state.correlation.call_id.as_str() {
			return Err(ControlError::CallMismatch { request_id });
		}
		if !state.correlation.streams_args {
			return Err(ControlError::StreamingNotDeclared);
		}
		if state.pull_open {
			return Err(ControlError::PullBusy);
		}
		state.pull_open = true;
		Ok(&state.correlation)
	}

	/// Validates one streamed reply and releases the pull slot on its terminal
	/// fragment.
	///
	/// A reply carrying an issue is terminal even if an untrusted peer omitted
	/// `complete`; the host never leaves the linear cursor borrowed after
	/// failure.
	///
	/// # Errors
	/// Returns a stale, correlation, state, or allocation-bound error.
	pub fn accept_pull_reply(
		&mut self,
		request_id: u64,
		reply: &PullReply,
	) -> Result<bool, ControlError> {
		if reply.chunk.len() > PULL_CHUNK_MAX_BYTES {
			return Err(ControlError::PullBound { field: "PullReply.chunk" });
		}
		let state = self
			.by_request
			.get_mut(&request_id)
			.ok_or(ControlError::StaleRequest(request_id))?;
		if reply.call_id != state.correlation.call_id.as_str() {
			return Err(ControlError::CallMismatch { request_id });
		}
		if !state.pull_open {
			return Err(ControlError::NoOutstandingPull);
		}
		let terminal = reply.complete || reply.issue.is_some();
		if terminal {
			state.pull_open = false;
		}
		Ok(terminal)
	}

	/// Fuses and removes a terminal invocation mapping.
	///
	/// # Errors
	/// Returns a stale or correlation error if the terminal frame does not name
	/// the live request.
	pub fn fuse(
		&mut self,
		request_id: u64,
		call_id: &str,
	) -> Result<InvocationCorrelation, ControlError> {
		let state = self
			.by_request
			.get(&request_id)
			.ok_or(ControlError::StaleRequest(request_id))?;
		if state.correlation.call_id.as_str() != call_id {
			return Err(ControlError::CallMismatch { request_id });
		}
		let state = self
			.by_request
			.remove(&request_id)
			.expect("validated request remains in the single-owner map");
		self.by_invocation.remove(&state.correlation.invocation_id);
		Ok(state.correlation)
	}

	/// Returns the live correlation for an envelope request id.
	///
	/// # Errors
	/// Returns [`ControlError::StaleRequest`] for an unknown request.
	pub fn request(&self, request_id: u64) -> Result<&InvocationCorrelation, ControlError> {
		self
			.by_request
			.get(&request_id)
			.map(|state| &state.correlation)
			.ok_or(ControlError::StaleRequest(request_id))
	}

	fn by_environment_id(&self, invocation_id: &str) -> Result<&InvocationState, ControlError> {
		let request_id = self
			.by_invocation
			.get(invocation_id)
			.ok_or_else(|| ControlError::UnknownInvocation(Str::from(invocation_id)))?;
		self
			.by_request
			.get(request_id)
			.ok_or(ControlError::StaleRequest(*request_id))
	}

	fn allocate_request_id(&mut self) -> u64 {
		loop {
			let candidate = self.next_request_id;
			self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
			if candidate != 0 && !self.by_request.contains_key(&candidate) {
				return candidate;
			}
		}
	}
}

fn validate_pull_bounds(pull: &PullRequest) -> Result<(), ControlError> {
	if pull.path.len() > PULL_PATH_MAX_SEGMENTS {
		return Err(ControlError::PullBound { field: "PullRequest.path" });
	}
	if pull
		.path
		.iter()
		.any(|segment| segment.len() > PULL_NAME_MAX_BYTES)
	{
		return Err(ControlError::PullBound { field: "PullRequest.path segment" });
	}
	if pull
		.key
		.as_ref()
		.is_some_and(|key| key.len() > PULL_NAME_MAX_BYTES)
	{
		return Err(ControlError::PullBound { field: "PullRequest.key" });
	}
	if pull.aliases.len() > PULL_ALIAS_MAX_COUNT
		|| pull
			.aliases
			.iter()
			.any(|alias| alias.len() > PULL_NAME_MAX_BYTES)
	{
		return Err(ControlError::PullBound { field: "PullRequest.aliases" });
	}
	if pull
		.expected
		.as_ref()
		.is_some_and(|expected| expected.len() > PULL_EXPECTED_MAX_BYTES)
	{
		return Err(ControlError::PullBound { field: "PullRequest.expected" });
	}
	if usize::try_from(pull.chunk_bytes).map_or(true, |size| size > PULL_CHUNK_MAX_BYTES) {
		return Err(ControlError::PullBound { field: "PullRequest.chunk_bytes" });
	}
	Ok(())
}

fn validate_state_scope(scope: i32) -> Result<StateScope, JournalControlError> {
	let scope = StateScope::try_from(scope).map_err(|_| JournalControlError::InvalidStateScope)?;
	if scope == StateScope::Unspecified {
		Err(JournalControlError::InvalidStateScope)
	} else {
		Ok(scope)
	}
}

/// Core-authenticated identity attached to a journal CONTROL connection.
///
/// Neither principal nor provenance is decoded from worker frames.
#[derive(Clone, Debug)]
pub struct JournalConnectionIdentity {
	/// Authenticated principal.
	pub principal:          Principal,
	/// Authenticated extension provenance.
	pub provenance:         Provenance,
	/// Live extension-host generation.
	pub host_generation:    u64,
	/// Live session generation.
	pub session_generation: u64,
}

/// A journal/session/artifact request that must be served by its authoritative
/// read or artifact backend after host authentication.
#[derive(Debug)]
pub enum ExternalJournalRequest {
	/// Scoped journal scan.
	Query {
		/// Envelope correlation.
		request_id: u64,
		/// Authenticated extension namespace.
		extension:  Str,
		/// Worker query payload.
		query:      QueryJournal,
	},
	/// Authoritative sessions-index page.
	ListSessions {
		/// Envelope correlation.
		request_id: u64,
		/// Worker query payload.
		query:      ListSessions,
	},
	/// Authoritative sessions-index usage aggregation.
	QueryUsage {
		/// Envelope correlation.
		request_id: u64,
		/// Worker query payload.
		query:      QueryUsage,
	},
	/// Artifact adoption. The backend must stat `source_url` authoritatively
	/// before persisting identity and must not infer or trust a peer size.
	AdoptArtifact {
		/// Envelope correlation.
		request_id: u64,
		/// Authenticated durable request stamp.
		stamp:      JournalRequestStamp,
		/// Core-authenticated author; never decoded from the worker frame.
		author:     JournalAuthor,
		/// Worker request without any authorship fields.
		request:    AdoptArtifact,
	},
	/// Authoritative artifact metadata lookup.
	StatArtifact {
		/// Envelope correlation.
		request_id: u64,
		/// Worker query payload.
		request:    StatArtifact,
	},
	/// Authoritative artifact catalog page.
	ListArtifacts {
		/// Envelope correlation.
		request_id: u64,
		/// Worker query payload.
		request:    ListArtifacts,
	},
	/// Durable artifact pin or lifetime change.
	PinArtifact {
		/// Envelope correlation.
		request_id: u64,
		/// Authenticated durable request stamp.
		stamp:      JournalRequestStamp,
		/// Core-authenticated author; never decoded from the worker frame.
		author:     JournalAuthor,
		/// Worker request without any authorship fields.
		request:    PinArtifact,
	},
	/// Scoped state value lookup delegated to the authoritative state backend.
	StateGet {
		/// Envelope correlation.
		request_id: u64,
		/// Authenticated extension namespace owner.
		extension:  Str,
		/// Worker request without principal fields.
		request:    StateGet,
	},
	/// Durable scoped state compare-and-swap.
	StateCas {
		/// Envelope correlation.
		request_id: u64,
		/// Authenticated durable request stamp.
		stamp:      JournalRequestStamp,
		/// Core-authenticated author.
		author:     JournalAuthor,
		/// Worker request without principal fields.
		request:    StateCas,
	},
	/// Fused scoped state watch stream.
	StateWatch {
		/// Envelope correlation.
		request_id: u64,
		/// Authenticated extension namespace owner.
		extension:  Str,
		/// Worker request without principal fields.
		request:    StateWatch,
	},
}

/// Result of dispatching one journal-domain worker envelope.
#[derive(Debug)]
pub enum JournalDispatch {
	/// Immediate host reply from the Agent Journal owner.
	Reply(JournalHostEnvelope),
	/// Current-session rows returned by the serialized Agent Journal owner.
	Rows {
		/// Envelope correlation.
		request_id: u64,
		/// Ordered authenticated raw rows.
		rows:       Vec<JournalCustomEntry>,
	},
	/// Authenticated request for the authoritative read/artifact backend.
	External(ExternalJournalRequest),
}

/// Typed rejection of a journal-domain CONTROL frame.
#[derive(Debug, Error)]
pub enum JournalControlError {
	/// Request id zero cannot correlate a journal command.
	#[error("journal CONTROL request id must be nonzero")]
	ZeroRequestId,
	/// Journal envelope omitted its body.
	#[error("journal CONTROL envelope has no body")]
	MissingBody,
	/// A durable request omitted its idempotency key.
	#[error("durable journal CONTROL request has no idempotency key")]
	MissingIdempotencyKey,
	/// A durable request was fenced by a stale host or session generation.
	#[error("durable journal CONTROL request carries stale generations")]
	StaleGeneration,
	/// JSON data was invalid before it reached the journal staging point.
	#[error("journal entry data_json is not one complete JSON value")]
	InvalidJson,
	/// A context part cannot be represented in extension journal context.
	#[error("journal context contains an unsupported or malformed part")]
	InvalidContext,
	/// An entry-kind declaration carried an invalid revision.
	#[error("entry-kind declaration revision is invalid")]
	InvalidRevision,
	/// A state request omitted its mandatory scope.
	#[error("state CONTROL request has unspecified scope")]
	InvalidStateScope,
	/// The serialized Agent Journal owner rejected the request.
	#[error(transparent)]
	Agent(#[from] AgentControlError),
}

const _: () = assert!(
	std::mem::size_of::<JournalControlError>() <= 128,
	"JournalControlError must stay compact"
);


/// Authenticated journal-domain CONTROL dispatcher for one extension.
///
/// Durable frames are generation-fenced and fully decoded before the one
/// mailbox message is sent, so an atomic group cannot be partially staged.
pub struct JournalControl {
	sender:             ControlSender,
	extension:          Str,
	granted_extensions: Vec<Str>,
	identity:           JournalConnectionIdentity,
}

impl JournalControl {
	/// Binds a journal dispatcher to core-authenticated connection identity.
	#[must_use]
	pub const fn new(
		sender: ControlSender,
		extension: Str,
		granted_extensions: Vec<Str>,
		identity: JournalConnectionIdentity,
	) -> Self {
		Self { sender, extension, granted_extensions, identity }
	}

	/// Dispatches one worker journal envelope.
	///
	/// Query rows are returned as [`ExternalJournalRequest`] so the app can
	/// stream each authoritative backend row into `JournalHostEnvelope` and set
	/// that row's `terminal` bit; no intermediate collection is required.
	///
	/// # Errors
	/// Rejects missing correlation, stale generations, malformed declarations,
	/// JSON, or context before any journal bytes are staged.
	pub async fn dispatch(
		&self,
		request_id: u64,
		envelope: JournalWorkerEnvelope,
		ts: u64,
	) -> Result<JournalDispatch, JournalControlError> {
		if request_id == 0 {
			return Err(JournalControlError::ZeroRequestId);
		}
		let body = envelope.body.ok_or(JournalControlError::MissingBody)?;
		match body {
			journal_worker_envelope::Body::DeclareEntryKinds(declare) => {
				self.declare(declare).await?;
				Ok(JournalDispatch::Reply(appended_reply(Vec::new())))
			},
			journal_worker_envelope::Body::AppendEntry(entry) => {
				let stamp = self.entry_stamp(request_id, &entry)?;
				let operation = JournalOperation::Append(pending_entry(entry)?);
				let reply = self
					.sender
					.journal(JournalRequest { ts, stamp, author: self.author(), operation })
					.await?;
				Ok(JournalDispatch::Reply(appended_reply(reply.indexes)))
			},
			journal_worker_envelope::Body::AppendEntriesAtomic(group) => {
				let (stamp, entries) = self.atomic(request_id, group)?;
				let reply = self
					.sender
					.journal(JournalRequest {
						ts,
						stamp,
						author: self.author(),
						operation: JournalOperation::AppendAtomic(entries),
					})
					.await?;
				Ok(JournalDispatch::Reply(appended_reply(reply.indexes)))
			},
			journal_worker_envelope::Body::QueryJournal(query) => {
				if !query.session.is_empty() {
					return Ok(JournalDispatch::External(ExternalJournalRequest::Query {
						request_id,
						extension: self.extension.clone(),
						query,
					}));
				}
				let kinds = if query.kinds.is_empty() {
					vec![None]
				} else {
					query
						.kinds
						.iter()
						.map(|kind| Some(Str::from(kind.as_str())))
						.collect()
				};
				let limit = query.limit.map(|limit| limit as usize);
				let queries = kinds
					.into_iter()
					.map(|kind| JournalQuery {
						caller_extension: self.extension.clone(),
						granted_extensions: self.granted_extensions.clone(),
						kind,
						rev: None,
						since: query.since_index,
						limit: query.until_index.map_or(limit, |_| None),
						live: query.live_only,
					})
					.collect();
				let mut rows = self.sender.query(queries).await?;
				if let Some(until) = query.until_index {
					rows.retain(|row| row.index <= until);
					if let Some(limit) = limit
						&& rows.len() > limit
					{
						rows.drain(..rows.len() - limit);
					}
				}
				Ok(JournalDispatch::Rows { request_id, rows })
			},
			journal_worker_envelope::Body::ListSessions(query) => {
				Ok(JournalDispatch::External(ExternalJournalRequest::ListSessions {
					request_id,
					query,
				}))
			},
			journal_worker_envelope::Body::QueryUsage(query) => {
				Ok(JournalDispatch::External(ExternalJournalRequest::QueryUsage { request_id, query }))
			},
			journal_worker_envelope::Body::AdoptArtifact(request) => {
				let stamp = self.durable_stamp(
					request_id,
					request.idempotency_key.as_str(),
					request.host_generation,
					request.session_generation,
				)?;
				Ok(JournalDispatch::External(ExternalJournalRequest::AdoptArtifact {
					request_id,
					stamp,
					author: self.author(),
					request,
				}))
			},
			journal_worker_envelope::Body::StatArtifact(request) => {
				Ok(JournalDispatch::External(ExternalJournalRequest::StatArtifact {
					request_id,
					request,
				}))
			},
			journal_worker_envelope::Body::ListArtifacts(request) => {
				Ok(JournalDispatch::External(ExternalJournalRequest::ListArtifacts {
					request_id,
					request,
				}))
			},
			journal_worker_envelope::Body::PinArtifact(request) => {
				let stamp = self.durable_stamp(
					request_id,
					request.idempotency_key.as_str(),
					request.host_generation,
					request.session_generation,
				)?;
				Ok(JournalDispatch::External(ExternalJournalRequest::PinArtifact {
					request_id,
					stamp,
					author: self.author(),
					request,
				}))
			},
			journal_worker_envelope::Body::StateGet(request) => {
				validate_state_scope(request.scope)?;
				Ok(JournalDispatch::External(ExternalJournalRequest::StateGet {
					request_id,
					extension: self.extension.clone(),
					request,
				}))
			},
			journal_worker_envelope::Body::StateCas(request) => {
				validate_state_scope(request.scope)?;
				let stamp = self.durable_stamp(
					request_id,
					request.idempotency_key.as_str(),
					request.host_generation,
					request.session_generation,
				)?;
				Ok(JournalDispatch::External(ExternalJournalRequest::StateCas {
					request_id,
					stamp,
					author: self.author(),
					request,
				}))
			},
			journal_worker_envelope::Body::StateWatch(request) => {
				validate_state_scope(request.scope)?;
				Ok(JournalDispatch::External(ExternalJournalRequest::StateWatch {
					request_id,
					extension: self.extension.clone(),
					request,
				}))
			},
		}
	}

	async fn declare(&self, declaration: DeclareEntryKinds) -> Result<(), JournalControlError> {
		let declarations = declaration
			.kinds
			.into_iter()
			.map(|kind| {
				omp_agent::EntryKindDecl::parse(
					Str::from(kind.name),
					kind.rev.as_str(),
					kind.display,
					kind.has_projection,
					None,
				)
				.map_err(|_| JournalControlError::InvalidRevision)
			})
			.collect::<Result<Vec<_>, _>>()?;
		self
			.sender
			.declare_entry_kinds(self.extension.clone(), declarations)
			.await?;
		Ok(())
	}

	fn atomic(
		&self,
		request_id: u64,
		group: AppendEntriesAtomic,
	) -> Result<(JournalRequestStamp, Vec<PendingCustomEntry>), JournalControlError> {
		let stamp = self.durable_stamp(
			request_id,
			group.idempotency_key.as_str(),
			group.host_generation,
			group.session_generation,
		)?;
		let mut entries = Vec::with_capacity(group.entries.len());
		for entry in group.entries {
			self.generations(entry.host_generation, entry.session_generation)?;
			entries.push(pending_entry(entry)?);
		}
		Ok((stamp, entries))
	}

	fn entry_stamp(
		&self,
		request_id: u64,
		entry: &AppendEntry,
	) -> Result<JournalRequestStamp, JournalControlError> {
		self.durable_stamp(
			request_id,
			entry.idempotency_key.as_str(),
			entry.host_generation,
			entry.session_generation,
		)
	}

	fn durable_stamp(
		&self,
		request_id: u64,
		idempotency_key: &str,
		host_generation: u64,
		session_generation: u64,
	) -> Result<JournalRequestStamp, JournalControlError> {
		if idempotency_key.is_empty() {
			return Err(JournalControlError::MissingIdempotencyKey);
		}
		self.generations(host_generation, session_generation)?;
		Ok(JournalRequestStamp {
			request_id: Str::from(request_id.to_string()),
			idempotency_key: Str::from(idempotency_key),
			host_generation,
			session_generation,
		})
	}

	const fn generations(
		&self,
		host_generation: u64,
		session_generation: u64,
	) -> Result<(), JournalControlError> {
		if host_generation == self.identity.host_generation
			&& session_generation == self.identity.session_generation
		{
			Ok(())
		} else {
			Err(JournalControlError::StaleGeneration)
		}
	}

	fn author(&self) -> JournalAuthor {
		JournalAuthor {
			principal:  self.identity.principal.clone(),
			provenance: self.identity.provenance.clone(),
		}
	}
}

/// Wraps one streamed journal row for a correlated host reply.
#[must_use]
pub const fn journal_row_reply(body: journal_host_envelope::Body) -> JournalHostEnvelope {
	JournalHostEnvelope { body: Some(body), props: None }
}

fn appended_reply(indexes: Vec<u64>) -> JournalHostEnvelope {
	journal_row_reply(journal_host_envelope::Body::EntryAppended(EntryAppended {
		indexes,
		terminal: true,
		props: None,
	}))
}

fn pending_entry(entry: AppendEntry) -> Result<PendingCustomEntry, JournalControlError> {
	let data = serde_json::from_slice::<Box<RawValue>>(&entry.data_json)
		.map_err(|_| JournalControlError::InvalidJson)?;
	let context = (!entry.context.is_empty())
		.then(|| {
			entry
				.context
				.into_iter()
				.map(user_block)
				.collect::<Result<Content, _>>()
		})
		.transpose()?;
	Ok(PendingCustomEntry {
		kind: Str::from(entry.kind),
		rev: Str::from(entry.rev),
		data: Some(data),
		context,
		display: entry.display,
	})
}

/// Encodes current-session journal rows as a fused CONTROL reply stream.
///
/// An empty result emits one terminal sentinel row so an empty query cannot be
/// mistaken for a dropped stream.
pub fn journal_rows(
	rows: &[JournalCustomEntry],
) -> impl DoubleEndedIterator<Item = JournalHostEnvelope> + '_ {
	let terminal = rows.last().map(|row| row.index);
	let sentinel = rows.is_empty().then(|| {
		journal_row_reply(journal_host_envelope::Body::JournalRow(JournalRow {
			terminal: true,
			..Default::default()
		}))
	});
	rows
		.iter()
		.map(move |row| {
			let entry = &row.entry;
			let context = entry
				.context()
				.into_iter()
				.flatten()
				.map(proto_part)
				.collect();
			journal_row_reply(journal_host_envelope::Body::JournalRow(JournalRow {
				index: row.index,
				kind: entry.kind().into(),
				rev: entry.rev().unwrap_or_default().into(),
				data_json: entry
					.data()
					.map_or_else(Bytes::new, |data| Bytes::copy_from_slice(data.get().as_bytes())),
				context,
				terminal: terminal == Some(row.index),
				props: None,
			}))
		})
		.chain(sentinel)
}

/// Marks the last sessions-index row terminal, or emits one empty terminal
/// sentinel when the authoritative page is empty.
pub fn session_rows(
	rows: impl IntoIterator<Item = SessionRow>,
) -> impl Iterator<Item = JournalHostEnvelope> {
	fuse_rows(rows, |mut row, terminal| {
		row.terminal = terminal;
		journal_host_envelope::Body::SessionRow(row)
	})
}

/// Marks the last usage row terminal, or emits one empty terminal sentinel.
pub fn usage_rows(
	rows: impl IntoIterator<Item = UsageReport>,
) -> impl Iterator<Item = JournalHostEnvelope> {
	fuse_rows(rows, |mut row, terminal| {
		row.terminal = terminal;
		journal_host_envelope::Body::UsageReport(row)
	})
}

/// Marks the last artifact row terminal, or emits one empty terminal sentinel.
pub fn artifact_rows(
	rows: impl IntoIterator<Item = ArtifactRow>,
) -> impl Iterator<Item = JournalHostEnvelope> {
	fuse_rows(rows, |mut row, terminal| {
		row.terminal = terminal;
		journal_host_envelope::Body::ArtifactRow(row)
	})
}

fn fuse_rows<T: Default>(
	rows: impl IntoIterator<Item = T>,
	mut wrap: impl FnMut(T, bool) -> journal_host_envelope::Body,
) -> impl Iterator<Item = JournalHostEnvelope> {
	let mut rows = rows.into_iter().peekable();
	let mut emitted = false;
	std::iter::from_fn(move || {
		if let Some(row) = rows.next() {
			emitted = true;
			let terminal = rows.peek().is_none();
			return Some(journal_row_reply(wrap(row, terminal)));
		}
		if emitted {
			return None;
		}
		emitted = true;
		Some(journal_row_reply(wrap(T::default(), true)))
	})
}

fn proto_part(block: &UserBlock) -> Part {
	match block {
		UserBlock::Text { text } => Part { kind: Some(part::Kind::Text(text.to_string())) },
		UserBlock::Image { blob } => Part {
			kind: Some(part::Kind::Blob(omp_proto::thread::v1::Blob {
				hash: Bytes::copy_from_slice(&blob.hash),
				size: blob.size,
				..Default::default()
			})),
		},
	}
}

fn user_block(part: Part) -> Result<UserBlock, JournalControlError> {
	match part.kind {
		Some(part::Kind::Text(text)) => Ok(UserBlock::Text { text: Str::from(text) }),
		Some(part::Kind::Blob(blob)) => {
			let hash = <[u8; 32]>::try_from(blob.hash.as_ref())
				.map_err(|_| JournalControlError::InvalidContext)?;
			Ok(UserBlock::Image { blob: BlobRef { hash, size: blob.size } })
		},
		Some(part::Kind::Thinking(_) | part::Kind::Fallback(_) | part::Kind::ServerTool(_))
		| None => Err(JournalControlError::InvalidContext),
	}
}
