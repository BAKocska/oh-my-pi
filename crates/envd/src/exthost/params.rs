//! Invocation-owned parameter cursors and audited direct-filesystem CONTROL.

use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use bytes::Bytes;
use omp_core::{InvocationPhase, LifecyclePhase, Str};
use omp_proto::toolhost::v1;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::control::{
	AuditedDirectFilesystemRequest, ControlAuthority, ControlConnectionIdentity, ControlEffect,
	ControlProtocolError, ControlRequestContext, DirectFilesystemError, DirectFilesystemGrant,
	admit_direct_filesystem,
};

/// Maximum number of simultaneous value pulls for one invocation cursor.
pub const MAX_PENDING_PARAMETER_PULLS: usize = 1;
/// Exact manifest capability for the exceptional filesystem escape.
pub const DIRECT_FILESYSTEM_CAPABILITY: &str = "trusted.direct-filesystem";

/// One path component in a typed JSON argument cursor.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ParameterPathPart {
	/// Object member.
	Key(Str),
	/// Array index.
	Index(u64),
}

/// Closed parameter-cursor operation vocabulary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParameterOperation {
	/// Strictly finalize and decode all arguments.
	Args,
	/// Return exact provider-emitted bytes as UTF-8.
	Raw,
	/// Await effect authorization and return canonical argument text.
	Committed,
	/// Consume one structured invocation interrupt.
	NextInterrupt,
	/// Pull one value, string fragment, or collection.
	Pull,
	/// Advance one array element.
	ArrayNext,
	/// Advance one object member.
	ObjectNext,
}

impl ParameterOperation {
	fn parse(operation: &str) -> Option<Self> {
		Some(match operation {
			"omp.params.args" => Self::Args,
			"omp.params.raw" => Self::Raw,
			"omp.params.committed" => Self::Committed,
			"omp.params.next_interrupt" => Self::NextInterrupt,
			"omp.params.pull" => Self::Pull,
			"omp.params.array_next" => Self::ArrayNext,
			"omp.params.object_next" => Self::ObjectNext,
			_ => return None,
		})
	}

	const fn counts_as_pull(self) -> bool {
		!matches!(self, Self::NextInterrupt)
	}

	const fn minimum_phase(self) -> InvocationPhase {
		match self {
			Self::Committed => InvocationPhase::EffectsAuthorized,
			Self::Args
			| Self::Raw
			| Self::NextInterrupt
			| Self::Pull
			| Self::ArrayNext
			| Self::ObjectNext => InvocationPhase::Open,
		}
	}
}

/// Fully typed pull request handed to the live invocation feed.
#[derive(Clone, Debug, PartialEq)]
pub struct ParameterPullRequest {
	/// Host-issued invocation identity.
	pub invocation_id: Str,
	/// Closed cursor operation.
	pub operation:     ParameterOperation,
	/// Path into the canonical argument document.
	pub path:          Vec<ParameterPathPart>,
	/// Pull mode (`value`, `text`, `chunk`, and so on).
	pub mode:          Option<Str>,
	/// Alternate accepted spellings.
	pub aliases:       Vec<Str>,
	/// Host-defined coercion names.
	pub coercions:     Vec<Str>,
	/// Optional user-facing example.
	pub example:       Option<Str>,
	/// Optional expected shape.
	pub expected:      Option<Str>,
	/// Optional chunk byte offset.
	pub offset:        Option<u64>,
	/// Optional array or object enumeration index.
	pub index:         Option<u64>,
	/// Whether absence is legal.
	pub optional:      bool,
	/// Whether a loop-owned interrupt may preempt this pull.
	pub interruptible: bool,
}

/// Typed cursor response. The source owns issue, repair, interrupt, phase, and
/// commitment semantics; CONTROL only transports this already-authoritative
/// result.
#[derive(Clone, Debug, PartialEq)]
pub struct ParameterPullResult(pub Value);

/// Parameter cursor rejection.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ParameterAuthorityError {
	/// Connection identity or generation changed.
	#[error("parameter authority belongs to a stale connection generation")]
	StaleGeneration,
	/// Request does not belong to the callback invocation.
	#[error("parameter cursor invocation does not match callback authority")]
	WrongInvocation,
	/// Operation is illegal at the current invocation phase.
	#[error("parameter cursor operation is illegal in the current invocation phase")]
	Phase,
	/// The sole pending pull lane is occupied.
	#[error("a parameter pull is already pending for this invocation")]
	Backpressure,
	/// Request arguments are malformed.
	#[error("parameter cursor request is malformed: {0}")]
	Invalid(Str),
	/// Invocation feed closed or refused the cursor request.
	#[error("parameter cursor source failed: {0}")]
	Source(Str),
}

impl ParameterAuthorityError {
	fn protocol(&self) -> ControlProtocolError {
		let code = match self {
			Self::StaleGeneration => "StaleGeneration",
			Self::WrongInvocation => "InvocationMismatch",
			Self::Phase => "InvalidPhase",
			Self::Backpressure => "ParamsMisuse",
			Self::Invalid(_) => "ParamsProtocol",
			Self::Source(_) => "ParamsProtocol",
		};
		ControlProtocolError::new(code, Str::from(self.to_string()))
			.retryable(matches!(self, Self::Backpressure))
	}
}

/// Existing invocation-feed boundary. It owns canonical arguments, repairs,
/// interrupts, commitment, and monotonic phases.
#[async_trait]
pub trait ParameterSource: Send + Sync + 'static {
	/// Executes one cursor request. Cancellation must stop any provider wait and
	/// release feed-side resources.
	async fn pull(
		&self,
		request: ParameterPullRequest,
		cancel: CancellationToken,
	) -> Result<ParameterPullResult, ParameterAuthorityError>;
}

/// Authoritative parameter cursor owner bound to one extension connection.
pub struct ParameterControlOwner {
	identity: Arc<ControlConnectionIdentity>,
	source:   Arc<dyn ParameterSource>,
	pending:  Arc<Mutex<BTreeSet<Str>>>,
}

impl ParameterControlOwner {
	/// Binds cursor requests to the live invocation source.
	pub fn new(identity: Arc<ControlConnectionIdentity>, source: Arc<dyn ParameterSource>) -> Self {
		Self { identity, source, pending: Arc::new(Mutex::new(BTreeSet::new())) }
	}

	fn validate(
		&self,
		context: &ControlRequestContext,
		operation: ParameterOperation,
		arguments: &serde_json::Map<String, Value>,
	) -> Result<Str, ParameterAuthorityError> {
		let connection = &context.connection;
		if connection.extension != self.identity.extension
			|| connection.artifact_digest != self.identity.artifact_digest
			|| connection.host_generation != self.identity.host_generation
			|| connection.session_generation != self.identity.session_generation
			|| connection.capabilities != self.identity.capabilities
		{
			return Err(ParameterAuthorityError::StaleGeneration);
		}
		let invocation = context
			.invocation
			.as_ref()
			.ok_or(ParameterAuthorityError::Phase)?;
		let requested = arguments
			.get("invocation_id")
			.and_then(Value::as_str)
			.filter(|value| !value.is_empty())
			.ok_or_else(|| {
				ParameterAuthorityError::Invalid(Str::new_static("invocation_id is required"))
			})?;
		if requested != invocation.invocation {
			return Err(ParameterAuthorityError::WrongInvocation);
		}
		if invocation.lifecycle != LifecyclePhase::Active
			|| !invocation.phase.allows_operation(operation.minimum_phase())
		{
			return Err(ParameterAuthorityError::Phase);
		}
		Ok(invocation.invocation.clone())
	}

	fn request(
		operation: ParameterOperation,
		invocation_id: Str,
		arguments: &serde_json::Map<String, Value>,
	) -> Result<ParameterPullRequest, ParameterAuthorityError> {
		let strings = |name: &'static str| -> Result<Vec<Str>, ParameterAuthorityError> {
			arguments
				.get(name)
				.and_then(Value::as_array)
				.into_iter()
				.flatten()
				.map(|value| {
					value.as_str().map(Str::from).ok_or_else(|| {
						ParameterAuthorityError::Invalid(Str::from(format!(
							"{name} must contain strings"
						)))
					})
				})
				.collect()
		};
		let path = arguments
			.get("path")
			.and_then(Value::as_array)
			.into_iter()
			.flatten()
			.map(|part| {
				if let Some(key) = part.as_str() {
					Ok(ParameterPathPart::Key(Str::from(key)))
				} else if let Some(index) = part.as_u64() {
					Ok(ParameterPathPart::Index(index))
				} else {
					Err(ParameterAuthorityError::Invalid(Str::new_static(
						"path parts must be strings or non-negative integers",
					)))
				}
			})
			.collect::<Result<Vec<_>, _>>()?;
		if path.len() > 128 {
			return Err(ParameterAuthorityError::Invalid(Str::new_static(
				"parameter path exceeds 128 levels",
			)));
		}
		Ok(ParameterPullRequest {
			invocation_id,
			operation,
			path,
			mode: arguments.get("mode").and_then(Value::as_str).map(Str::from),
			aliases: strings("aliases")?,
			coercions: strings("coercions")?,
			example: arguments
				.get("example")
				.and_then(Value::as_str)
				.map(Str::from),
			expected: arguments
				.get("expected")
				.and_then(Value::as_str)
				.map(Str::from),
			offset: arguments.get("offset").and_then(Value::as_u64),
			index: arguments.get("index").and_then(Value::as_u64),
			optional: arguments
				.get("optional")
				.and_then(Value::as_bool)
				.unwrap_or(false),
			interruptible: arguments
				.get("interruptible")
				.and_then(Value::as_bool)
				.unwrap_or(false),
		})
	}
}

struct PendingPull {
	pending:       Arc<Mutex<BTreeSet<Str>>>,
	invocation_id: Str,
}

impl PendingPull {
	fn acquire(
		pending: &Arc<Mutex<BTreeSet<Str>>>,
		invocation_id: Str,
	) -> Result<Self, ParameterAuthorityError> {
		if !pending.lock().insert(invocation_id.clone()) {
			return Err(ParameterAuthorityError::Backpressure);
		}
		Ok(Self { pending: Arc::clone(pending), invocation_id })
	}
}

impl Drop for PendingPull {
	fn drop(&mut self) {
		self.pending.lock().remove(&self.invocation_id);
	}
}

struct CancelParameterRequest(CancellationToken);

impl Drop for CancelParameterRequest {
	fn drop(&mut self) {
		self.0.cancel();
	}
}

#[async_trait]
impl ControlAuthority for ParameterControlOwner {
	fn handles(&self, operation: &str) -> bool {
		ParameterOperation::parse(operation).is_some()
	}

	fn authorize(
		&self,
		context: &ControlRequestContext,
		operation: &str,
		arguments: &serde_json::Map<String, Value>,
	) -> Result<(), ControlProtocolError> {
		let operation = ParameterOperation::parse(operation).ok_or_else(|| {
			ControlProtocolError::new("UnknownOperation", "unknown parameter cursor operation")
		})?;
		self
			.validate(context, operation, arguments)
			.map(|_| ())
			.map_err(|error| error.protocol())
	}

	async fn request(
		&self,
		context: ControlRequestContext,
		operation: Str,
		arguments: serde_json::Map<String, Value>,
	) -> Result<Value, ControlProtocolError> {
		let operation = ParameterOperation::parse(operation.as_str()).ok_or_else(|| {
			ControlProtocolError::new("UnknownOperation", "unknown parameter cursor operation")
		})?;
		let invocation_id = self
			.validate(&context, operation, &arguments)
			.map_err(|error| error.protocol())?;
		let _pending = if operation.counts_as_pull() {
			Some(
				PendingPull::acquire(&self.pending, invocation_id.clone())
					.map_err(|error| error.protocol())?,
			)
		} else {
			None
		};
		let request =
			Self::request(operation, invocation_id, &arguments).map_err(|error| error.protocol())?;
		let cancel = CancellationToken::new();
		let _cancel_on_drop = CancelParameterRequest(cancel.clone());
		let result = self
			.source
			.pull(request, cancel)
			.await
			.map_err(|error| error.protocol())?;
		Ok(result.0)
	}

	async fn effect(
		&self,
		context: ControlRequestContext,
		_effect: ControlEffect,
	) -> Result<(), ControlProtocolError> {
		let operation = ParameterOperation::NextInterrupt;
		let invocation = context
			.invocation
			.as_ref()
			.ok_or_else(|| ParameterAuthorityError::Phase.protocol())?;
		let arguments = serde_json::Map::from_iter([(
			"invocation_id".to_owned(),
			Value::String(invocation.invocation.to_string()),
		)]);
		self
			.validate(&context, operation, &arguments)
			.map_err(|error| error.protocol())?;
		Err(ControlProtocolError::new(
			"UnsupportedEffect",
			"parameter authority accepts cursor requests only",
		))
	}
}

/// One direct-filesystem metadata result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DirectFilesystemStat {
	/// Portable file kind.
	pub kind:        Str,
	/// Byte length for regular files.
	pub size:        u64,
	/// Last-modified epoch milliseconds when available.
	pub modified_ms: Option<u64>,
	/// Whether the entry is read-only.
	pub readonly:    bool,
}

/// One direct-filesystem directory entry.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DirectFilesystemEntry {
	/// Absolute entry path.
	pub path: Str,
	/// Portable file kind.
	pub kind: Str,
}

/// Typed direct-filesystem operation result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DirectFilesystemOutput {
	/// Bounded file bytes.
	Bytes(Bytes),
	/// Metadata for one path.
	Stat(DirectFilesystemStat),
	/// Bounded directory listing.
	Entries(Vec<DirectFilesystemEntry>),
	/// Mutation completed without response bytes.
	Applied,
}

/// Direct-filesystem execution or audit failure.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum DirectFilesystemAuthorityError {
	/// Connection identity or generation changed.
	#[error("direct-filesystem authority belongs to a stale connection generation")]
	StaleGeneration,
	/// Exact manifest capability is absent.
	#[error("trusted direct-filesystem capability was not granted")]
	Capability,
	/// Invocation has not reached effects authorization.
	#[error("direct-filesystem operation is illegal in the current invocation phase")]
	Phase,
	/// Durable grant facts are malformed or stale.
	#[error("direct-filesystem durable grant is malformed or stale")]
	Grant,
	/// Request is malformed.
	#[error("direct-filesystem request is malformed: {0}")]
	Invalid(Str),
	/// Journal append failed before execution.
	#[error("direct-filesystem audit append failed: {0}")]
	Audit(Str),
	/// The explicitly privileged filesystem executor failed.
	#[error("direct-filesystem execution failed: {0}")]
	Execute(Str),
}

impl DirectFilesystemAuthorityError {
	fn protocol(&self) -> ControlProtocolError {
		let code = match self {
			Self::StaleGeneration => "StaleGeneration",
			Self::Capability | Self::Grant => "DirectFilesystemDenied",
			Self::Phase => "InvalidPhase",
			Self::Invalid(_) => "InvalidArguments",
			Self::Audit(_) => "DirectFilesystemAuditFailed",
			Self::Execute(_) => "DirectFilesystemFailed",
		};
		ControlProtocolError::new(code, Str::from(self.to_string()))
	}
}

/// Durable journal boundary. The request and immutable grant provenance are
/// appended before the exceptional filesystem executor runs.
#[async_trait]
pub trait DirectFilesystemJournal: Send + Sync + 'static {
	/// Appends an audit receipt and returns its durable identity.
	async fn append_request(
		&self,
		context: &ControlRequestContext,
		request: &AuditedDirectFilesystemRequest,
	) -> Result<Str, DirectFilesystemAuthorityError>;
}

/// Explicit privileged filesystem boundary. Ordinary Environment operations do
/// not implement this trait and cannot be reached through this owner.
#[async_trait]
pub trait DirectFilesystemExecutor: Send + Sync + 'static {
	/// Executes one already-admitted, already-audited request.
	async fn execute(
		&self,
		request: AuditedDirectFilesystemRequest,
		cancel: CancellationToken,
	) -> Result<DirectFilesystemOutput, DirectFilesystemAuthorityError>;
}

/// Audited direct-filesystem CONTROL owner for one trusted connection.
pub struct DirectFilesystemControlOwner {
	identity: Arc<ControlConnectionIdentity>,
	journal:  Arc<dyn DirectFilesystemJournal>,
	executor: Arc<dyn DirectFilesystemExecutor>,
}

impl DirectFilesystemControlOwner {
	/// Binds the exceptional escape to authenticated grant and journal owners.
	pub fn new(
		identity: Arc<ControlConnectionIdentity>,
		journal: Arc<dyn DirectFilesystemJournal>,
		executor: Arc<dyn DirectFilesystemExecutor>,
	) -> Self {
		Self { identity, journal, executor }
	}

	fn validate(
		&self,
		context: &ControlRequestContext,
	) -> Result<(), DirectFilesystemAuthorityError> {
		let connection = &context.connection;
		if connection.extension != self.identity.extension
			|| connection.artifact_digest != self.identity.artifact_digest
			|| connection.host_generation != self.identity.host_generation
			|| connection.session_generation != self.identity.session_generation
			|| connection.capabilities != self.identity.capabilities
		{
			return Err(DirectFilesystemAuthorityError::StaleGeneration);
		}
		if !self
			.identity
			.capabilities
			.contains(DIRECT_FILESYSTEM_CAPABILITY)
		{
			return Err(DirectFilesystemAuthorityError::Capability);
		}
		let invocation = context
			.invocation
			.as_ref()
			.ok_or(DirectFilesystemAuthorityError::Phase)?;
		if invocation.lifecycle != LifecyclePhase::Active
			|| !invocation
				.phase
				.allows_operation(InvocationPhase::EffectsAuthorized)
		{
			return Err(DirectFilesystemAuthorityError::Phase);
		}
		Ok(())
	}

	fn grant(
		context: &ControlRequestContext,
	) -> Result<DirectFilesystemGrant, DirectFilesystemAuthorityError> {
		#[derive(Deserialize)]
		struct Grant {
			extension_id:      Str,
			publisher:         Str,
			capability_digest: Str,
			grant_id:          Str,
			generation:        u64,
		}
		let value = context
			.invocation
			.as_ref()
			.and_then(|invocation| invocation.direct_filesystem.clone())
			.ok_or(DirectFilesystemAuthorityError::Grant)?;
		let grant: Grant =
			serde_json::from_value(value).map_err(|_| DirectFilesystemAuthorityError::Grant)?;
		if grant.extension_id != context.connection.extension
			|| grant.generation != context.connection.host_generation
			|| grant.grant_id.is_empty()
			|| grant.capability_digest.is_empty()
		{
			return Err(DirectFilesystemAuthorityError::Grant);
		}
		Ok(DirectFilesystemGrant {
			extension_id:      grant.extension_id,
			publisher:         grant.publisher,
			capability_digest: grant.capability_digest,
			grant_id:          grant.grant_id,
			generation:        grant.generation,
		})
	}

	fn request(
		context: &ControlRequestContext,
		arguments: &serde_json::Map<String, Value>,
	) -> Result<AuditedDirectFilesystemRequest, DirectFilesystemAuthorityError> {
		let authoritative = Self::grant(context)?;
		let offered = arguments
			.get("grant")
			.and_then(Value::as_object)
			.ok_or(DirectFilesystemAuthorityError::Grant)?;
		let offered_id = offered.get("grant_id").and_then(Value::as_str);
		let offered_generation = offered.get("generation").and_then(Value::as_u64);
		let offered_digest = offered.get("capability_digest").and_then(Value::as_str);
		let offered_extension = offered.get("extension_id").and_then(Value::as_str);
		let offered_publisher = offered.get("publisher").and_then(Value::as_str);
		if offered_id != Some(authoritative.grant_id.as_str())
			|| offered_generation != Some(authoritative.generation)
			|| offered_digest != Some(authoritative.capability_digest.as_str())
			|| offered_extension != Some(authoritative.extension_id.as_str())
			|| offered_publisher != Some(authoritative.publisher.as_str())
		{
			return Err(DirectFilesystemAuthorityError::Grant);
		}
		let operation = arguments
			.get("operation")
			.and_then(Value::as_str)
			.unwrap_or_default()
			.to_owned();
		let absolute_path = arguments
			.get("path")
			.and_then(Value::as_str)
			.unwrap_or_default()
			.to_owned();
		let data = match arguments.get("data") {
			None | Some(Value::Null) => Bytes::new(),
			Some(Value::Object(value)) => {
				let encoded = value.get("$bytes").and_then(Value::as_str).ok_or_else(|| {
					DirectFilesystemAuthorityError::Invalid(Str::new_static("data must be bytes"))
				})?;
				Bytes::from(omp_core::base64::decode(encoded).into_vec().map_err(|_| {
					DirectFilesystemAuthorityError::Invalid(Str::new_static("data has invalid base64"))
				})?)
			},
			Some(_) => {
				return Err(DirectFilesystemAuthorityError::Invalid(Str::new_static(
					"data must be bytes",
				)));
			},
		};
		let wire = v1::DirectFilesystemRequest {
			operation,
			absolute_path,
			data,
			grant_id: authoritative.grant_id.to_string(),
			capability_digest: Bytes::copy_from_slice(authoritative.capability_digest.as_bytes()),
			generation: authoritative.generation,
		};
		admit_direct_filesystem(wire, true, Some(&authoritative)).map_err(|error| match error {
			DirectFilesystemError::Undeclared | DirectFilesystemError::Ungranted => {
				DirectFilesystemAuthorityError::Grant
			},
			DirectFilesystemError::Operation => {
				DirectFilesystemAuthorityError::Invalid(Str::new_static("unsupported operation"))
			},
			DirectFilesystemError::RelativePath => {
				DirectFilesystemAuthorityError::Invalid(Str::new_static("path must be absolute"))
			},
			DirectFilesystemError::PayloadTooLarge => {
				DirectFilesystemAuthorityError::Invalid(Str::new_static("payload exceeds 1 MiB"))
			},
		})
	}
}

struct CancelDirectFilesystemRequest(CancellationToken);

impl Drop for CancelDirectFilesystemRequest {
	fn drop(&mut self) {
		self.0.cancel();
	}
}

#[async_trait]
impl ControlAuthority for DirectFilesystemControlOwner {
	fn handles(&self, operation: &str) -> bool {
		operation == "omp.direct_filesystem.request"
	}

	fn authorize(
		&self,
		context: &ControlRequestContext,
		operation: &str,
		arguments: &serde_json::Map<String, Value>,
	) -> Result<(), ControlProtocolError> {
		if operation != "omp.direct_filesystem.request" {
			return Err(ControlProtocolError::new(
				"UnknownOperation",
				"unknown direct-filesystem operation",
			));
		}
		self.validate(context).map_err(|error| error.protocol())?;
		Self::request(context, arguments).map_err(|error| error.protocol())?;
		Ok(())
	}

	async fn request(
		&self,
		context: ControlRequestContext,
		operation: Str,
		arguments: serde_json::Map<String, Value>,
	) -> Result<Value, ControlProtocolError> {
		self.authorize(&context, operation.as_str(), &arguments)?;
		let request = Self::request(&context, &arguments).map_err(|error| error.protocol())?;
		let receipt = self
			.journal
			.append_request(&context, &request)
			.await
			.map_err(|error| error.protocol())?;
		let cancel = CancellationToken::new();
		let _cancel_on_drop = CancelDirectFilesystemRequest(cancel.clone());
		let output = self
			.executor
			.execute(request, cancel)
			.await
			.map_err(|error| error.protocol())?;
		let data = match output {
			DirectFilesystemOutput::Bytes(bytes) => {
				json!({"$bytes": omp_core::base64::encode(&bytes)})
			},
			DirectFilesystemOutput::Stat(stat) => {
				serde_json::to_value(stat).map_err(direct_filesystem_serialization)?
			},
			DirectFilesystemOutput::Entries(entries) => {
				serde_json::to_value(entries).map_err(direct_filesystem_serialization)?
			},
			DirectFilesystemOutput::Applied => Value::Null,
		};
		Ok(json!({"data": data, "audit_receipt": receipt}))
	}

	async fn effect(
		&self,
		context: ControlRequestContext,
		_effect: ControlEffect,
	) -> Result<(), ControlProtocolError> {
		self.validate(&context).map_err(|error| error.protocol())?;
		Err(ControlProtocolError::new(
			"UnsupportedEffect",
			"direct-filesystem authority accepts requests only",
		))
	}
}

fn direct_filesystem_serialization(error: serde_json::Error) -> ControlProtocolError {
	ControlProtocolError::new("DirectFilesystemProtocol", Str::from(error.to_string()))
}
