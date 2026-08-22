//! Versioned read-only statistics API over the authoritative session index.

use std::{
	collections::BTreeMap,
	path::PathBuf,
	sync::Arc,
	time::{SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use http::{Method, Request, Response, StatusCode, header};
use http_body_util::Full;
use hyper::body::Incoming;
use omp_core::Str;
use omp_storage::{
	index::{
		SessionFilter, SessionIndex, SessionStatus, UsageBucket, UsageBucketWidth, UsageDimension,
		UsageQuery,
	},
	transcript::SessionId,
};
use serde_json::{Value, json};
use smallvec::SmallVec;

/// Host-owned verdict projection and detached-job authority.
pub mod verdict_authority {
	use std::{
		collections::{BTreeMap, BTreeSet},
		sync::Arc,
		time::Duration,
	};

	use async_trait::async_trait;
	use omp_agent::JobBoard;
	use omp_core::{InvocationPhase, Str};
	use omp_tool::{
		ArtifactLifetime, ExpectedArtifact, JobKind, JobMetadata, JobOwner, JobRef, JobStatus,
	};
	use parking_lot::Mutex;
	use serde::{Deserialize, Serialize};
	use serde_json::Value;
	use thiserror::Error;

	/// Maximum wall time allowed for a pure prompt projection callback.
	pub const PROMPT_PROJECTION_DEADLINE: Duration = Duration::from_millis(50);

	/// Authenticated extension incarnation allowed to use one verdict owner.
	#[derive(Clone, Debug, Eq, PartialEq)]
	pub struct VerdictAuthorityIdentity {
		/// Stable authenticated principal spelling.
		pub principal:          Str,
		/// Declaring extension identifier.
		pub extension:          Str,
		/// Verified extension artifact digest.
		pub artifact_digest:    Str,
		/// Active child incarnation.
		pub host_generation:    u64,
		/// Active session incarnation.
		pub session_generation: u64,
		/// Durable session receiving job settlement.
		pub session:            Str,
		/// Exact durable capability grants.
		pub capabilities:       Arc<BTreeSet<Str>>,
	}

	/// Core-authored authority for one verdict operation.
	#[derive(Clone, Copy, Debug)]
	pub struct VerdictCallContext<'a> {
		/// Authenticated connection identity.
		pub identity:  &'a VerdictAuthorityIdentity,
		/// Current invocation phase.
		pub phase:     InvocationPhase,
		/// Whether cancellation has already won.
		pub cancelled: bool,
	}

	/// Structured verdict owner failure.
	#[derive(Clone, Debug, Error, Eq, PartialEq)]
	pub enum VerdictAuthorityError {
		/// The request belongs to a stale or foreign connection.
		#[error("verdict request belongs to a stale or foreign connection")]
		Identity,
		/// The owning invocation was cancelled or settled.
		#[error("verdict request was cancelled")]
		Cancelled,
		/// The operation is illegal in the current invocation phase.
		#[error("verdict request is not legal in the current invocation phase")]
		Phase,
		/// The descriptor is not backed by a scoped Environment resource.
		#[error("invalid detached job: {0}")]
		InvalidJob(Str),
		/// The stable job id names a different durable descriptor.
		#[error("detached job id `{0}` is already bound to another descriptor")]
		JobConflict(Str),
		/// The authoritative board rejected admission.
		#[error("detached job admission failed: {0}")]
		JobAdmission(Str),
		/// The callback exceeded its host-owned deadline.
		#[error("verdict projection timed out")]
		ProjectionTimeout,
		/// The callback host rejected or lost the projection.
		#[error("verdict projection failed: {0}")]
		Projection(Str),
	}

	/// CONTROL-safe detached-job descriptor.
	///
	/// Only named Environment process generations are accepted. Extensions
	/// never receive a process handle or ambient process-listing authority.
	#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
	pub struct JobRegistration {
		/// Stable durable job identity.
		pub id:               Str,
		/// Environment process name.
		pub owner_name:       Str,
		/// Exact Environment process generation.
		pub owner_generation: u64,
		/// Human-readable expected artifact role.
		pub description:      Str,
		/// Expected artifact media type.
		pub media_type:       Option<Str>,
		/// Minimum artifact lifetime.
		pub lifetime:         ArtifactLifetime,
	}

	impl JobRegistration {
		fn into_job(self, session: Str) -> Result<JobRef, VerdictAuthorityError> {
			if self.id.is_empty() || self.owner_name.is_empty() || self.owner_generation == 0 {
				return Err(VerdictAuthorityError::InvalidJob(Str::new_static(
					"id, owner name, and owner generation must be present",
				)));
			}
			let now = now_ms();
			let mut metadata = JobMetadata::running(JobKind::Eval, self.description.clone(), now);
			metadata.status = JobStatus::Running;
			metadata.owner_session = Some(session);
			Ok(JobRef {
				id:       self.id,
				owner:    JobOwner::NamedProcess {
					name:       self.owner_name,
					generation: self.owner_generation,
				},
				metadata: Arc::new(metadata),
				artifact: ExpectedArtifact {
					description: self.description,
					media_type:  self.media_type,
					lifetime:    self.lifetime,
				},
			})
		}
	}

	/// Pure prompt projection request sent back to the declaring extension.
	#[derive(Clone, Debug, Serialize)]
	pub struct PromptProjectionRequest {
		/// Exact registered wire name.
		pub name:        Str,
		/// Exact revision family.
		pub family:      Str,
		/// Exact monotonic revision.
		pub revision:    u16,
		/// Canonical durable verdict body.
		pub verdict:     Value,
		/// Host-sealed prompt projection budget and dialect.
		pub prompt_caps: Value,
	}

	/// Callback transport used by Core to invoke the extension projector.
	#[async_trait]
	pub trait PromptProjectionDispatcher: Send + Sync + 'static {
		/// Dispatches `omp.verdicts.project` to the exact authenticated worker
		/// generation and returns canonical projected parts.
		async fn project(
			&self,
			identity: Arc<VerdictAuthorityIdentity>,
			request: PromptProjectionRequest,
		) -> Result<Value, VerdictAuthorityError>;
	}

	/// Identity-fenced owner for durable detached jobs and prompt projections.
	pub struct VerdictAuthority {
		identity:      Arc<VerdictAuthorityIdentity>,
		jobs:          JobBoard,
		dispatcher:    Arc<dyn PromptProjectionDispatcher>,
		registrations: Mutex<BTreeMap<Str, (JobRegistration, JobRef)>>,
	}

	impl VerdictAuthority {
		/// Binds one authority to an authenticated connection and session board.
		pub fn new(
			identity: Arc<VerdictAuthorityIdentity>,
			jobs: JobBoard,
			dispatcher: Arc<dyn PromptProjectionDispatcher>,
		) -> Self {
			Self { identity, jobs, dispatcher, registrations: Mutex::new(BTreeMap::new()) }
		}

		fn authorize(&self, context: VerdictCallContext<'_>) -> Result<(), VerdictAuthorityError> {
			if context.identity != self.identity.as_ref() {
				return Err(VerdictAuthorityError::Identity);
			}
			if context.cancelled || context.phase.is_terminal() {
				return Err(VerdictAuthorityError::Cancelled);
			}
			if !context
				.phase
				.allows_operation(InvocationPhase::EffectsAuthorized)
			{
				return Err(VerdictAuthorityError::Phase);
			}
			Ok(())
		}

		/// Idempotently installs one Environment-owned descriptor on the
		/// authoritative session job board.
		pub fn register_job(
			&self,
			context: VerdictCallContext<'_>,
			registration: JobRegistration,
		) -> Result<JobRef, VerdictAuthorityError> {
			self.authorize(context)?;
			let mut registrations = self.registrations.lock();
			if let Some((existing_registration, existing)) =
				registrations.get(registration.id.as_str()).cloned()
			{
				return if existing_registration == registration {
					Ok(existing)
				} else {
					Err(VerdictAuthorityError::JobConflict(registration.id))
				};
			}
			let registration_key = registration.clone();
			let job = registration.into_job(self.identity.session.clone())?;
			match self.jobs.try_register(job.clone()) {
				Ok(true) => {
					registrations.insert(job.id.clone(), (registration_key, job.clone()));
					Ok(job)
				},
				Ok(false) => Err(VerdictAuthorityError::JobConflict(job.id)),
				Err(error) => Err(VerdictAuthorityError::JobAdmission(Str::new(error.to_string()))),
			}
		}

		/// Dispatches one exact-revision prompt projection under the
		/// non-negotiable host deadline.
		pub async fn project_prompt(
			&self,
			context: VerdictCallContext<'_>,
			request: PromptProjectionRequest,
		) -> Result<Value, VerdictAuthorityError> {
			if context.identity != self.identity.as_ref() {
				return Err(VerdictAuthorityError::Identity);
			}
			if context.cancelled {
				return Err(VerdictAuthorityError::Cancelled);
			}
			if context.phase != InvocationPhase::Settled {
				return Err(VerdictAuthorityError::Phase);
			}
			if request.name.is_empty() {
				return Err(VerdictAuthorityError::Projection(Str::new_static(
					"projection requires an exact device wire name",
				)));
			}
			tokio::time::timeout(
				PROMPT_PROJECTION_DEADLINE,
				self.dispatcher.project(self.identity.clone(), request),
			)
			.await
			.map_err(|_| VerdictAuthorityError::ProjectionTimeout)?
		}
	}

	#[async_trait]
	impl omp_envd::exthost::VerdictControlOwner for VerdictAuthority {
		async fn register_job(
			&self,
			context: omp_envd::exthost::control::ControlRequestContext,
			mut arguments: serde_json::Map<String, Value>,
		) -> Result<Value, omp_envd::exthost::control::ControlProtocolError> {
			let descriptor = arguments
				.remove("job")
				.unwrap_or_else(|| Value::Object(arguments));
			let descriptor = descriptor.as_object().ok_or_else(|| {
				omp_envd::exthost::control::ControlProtocolError::new(
					"InvalidJob",
					"job descriptor must be an object",
				)
			})?;
			let owner_kind = descriptor
				.get("owner_kind")
				.and_then(Value::as_str)
				.unwrap_or_default();
			if owner_kind != "named_process" {
				return Err(omp_envd::exthost::control::ControlProtocolError::new(
					"JobOwnerDenied",
					"extensions may register only Environment-owned named process generations",
				));
			}
			let string = |name: &'static str| {
				descriptor
					.get(name)
					.and_then(Value::as_str)
					.filter(|value| !value.is_empty())
					.map(Str::new)
					.ok_or_else(|| {
						omp_envd::exthost::control::ControlProtocolError::new(
							"InvalidJob",
							format!("{name} must be a non-empty string"),
						)
					})
			};
			let lifetime = descriptor
				.get("lifetime")
				.and_then(Value::as_str)
				.unwrap_or("session")
				.parse::<ArtifactLifetime>()
				.map_err(|_| {
					omp_envd::exthost::control::ControlProtocolError::new(
						"InvalidJob",
						"lifetime must be ephemeral, session, or durable",
					)
				})?;
			let registration = JobRegistration {
				id: string("id")?,
				owner_name: string("owner_name")?,
				owner_generation: descriptor
					.get("owner_generation")
					.and_then(Value::as_u64)
					.filter(|generation| *generation != 0)
					.ok_or_else(|| {
						omp_envd::exthost::control::ControlProtocolError::new(
							"InvalidJob",
							"owner_generation must be a positive integer",
						)
					})?,
				description: string("description")?,
				media_type: descriptor
					.get("media_type")
					.and_then(Value::as_str)
					.map(Str::new),
				lifetime,
			};
			let phase = context
				.invocation
				.as_ref()
				.map(|invocation| invocation.phase)
				.ok_or_else(|| {
					omp_envd::exthost::control::ControlProtocolError::new(
						"InvalidPhase",
						"job registration requires a live invocation",
					)
				})?;
			let call = VerdictCallContext {
				identity: self.identity.as_ref(),
				phase,
				cancelled: phase.is_terminal(),
			};
			let job =
				VerdictAuthority::register_job(self, call, registration).map_err(control_error)?;
			let (owner_name, owner_generation) = match &job.owner {
				JobOwner::NamedProcess { name, generation } => (name.as_str(), *generation),
				JobOwner::AgentLoop { .. } => {
					return Err(omp_envd::exthost::control::ControlProtocolError::new(
						"JobOwnerDenied",
						"job owner escaped the named-process authority",
					));
				},
			};
			Ok(serde_json::json!({
				"id": job.id.as_str(),
				"owner_kind": "named_process",
				"owner_name": owner_name,
				"owner_generation": owner_generation,
				"description": job.artifact.description.as_str(),
				"media_type": job.artifact.media_type.as_deref(),
				"lifetime": job.artifact.lifetime.to_string(),
			}))
		}
	}

	fn control_error(
		error: VerdictAuthorityError,
	) -> omp_envd::exthost::control::ControlProtocolError {
		let code = match &error {
			VerdictAuthorityError::Identity => "StaleGeneration",
			VerdictAuthorityError::Cancelled => "Cancelled",
			VerdictAuthorityError::Phase => "InvalidPhase",
			VerdictAuthorityError::InvalidJob(_) => "InvalidJob",
			VerdictAuthorityError::JobConflict(_) => "JobConflict",
			VerdictAuthorityError::JobAdmission(_) => "JobAdmissionDenied",
			VerdictAuthorityError::ProjectionTimeout => "ProjectionTimeout",
			VerdictAuthorityError::Projection(_) => "ProjectionFailed",
		};
		omp_envd::exthost::control::ControlProtocolError::new(code, error.to_string())
	}

	fn now_ms() -> u64 {
		std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.unwrap_or_default()
			.as_millis() as u64
	}
}

/// Stable media type and schema revision returned by every JSON route.
pub const API_VERSION: &str = "omp.stats.v1";
/// Concrete response body used by the stats HTTP service.
pub type Body = Full<Bytes>;

/// Shared API dependencies.
pub struct StatsApi {
	index:     Arc<SessionIndex>,
	sync_lock: PathBuf,
}

impl StatsApi {
	/// Creates an API backed by the authoritative receipt index.
	pub fn new(index: Arc<SessionIndex>, sync_lock: PathBuf) -> Self {
		Self { index, sync_lock }
	}

	/// Produces the same overview envelope used by the HTTP route.
	pub fn overview_document(&self, range: &str) -> Result<Value, String> {
		let range = Range::parse(Some(&format!("range={range}"))).map_err(str::to_owned)?;
		let data = self.overview(range)?;
		Ok(json!({"version": API_VERSION, "data": data, "meta": {"range": range.label}}))
	}

	/// Routes one versioned API request.
	pub fn handle(&self, request: &Request<Incoming>) -> Response<Body> {
		let path = request.uri().path();
		if path == "/api/version" && request.method() == Method::GET {
			return json_response(StatusCode::OK, json!({"version": API_VERSION}));
		}
		if path == "/api/v1/stats/sync" && request.method() == Method::POST {
			return self.sync();
		}
		if request.method() != Method::GET || !path.starts_with("/api/v1/stats/") {
			return error_response(StatusCode::NOT_FOUND, "route_not_found", "unknown stats route");
		}
		let range = match Range::parse(request.uri().query()) {
			Ok(range) => range,
			Err(message) => return error_response(StatusCode::BAD_REQUEST, "invalid_range", message),
		};
		let route = &path[14..];
		let result = match route {
			"overview" => self.overview(range),
			"models" => self.grouped(range, UsageDimension::Model),
			"providers" => self.grouped(range, UsageDimension::Provider),
			"folders" => self.grouped(range, UsageDimension::Project),
			"costs" => self.costs(range),
			"timeseries" => self.timeseries(range),
			"recent" => self.recent(range, false),
			"errors" => self.recent(range, true),
			"tools" => self.tools(range),
			"behavior" | "gain" => {
				return error_response(
					StatusCode::NOT_IMPLEMENTED,
					"query_unavailable",
					"this statistics projection has no authoritative index",
				);
			},
			_ if route.starts_with("request/") => self.request(&route[8..]),
			_ => {
				return error_response(StatusCode::NOT_FOUND, "route_not_found", "unknown stats route");
			},
		};
		match result {
			Ok(data) => envelope_response(data, range),
			Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "query_failed", &error),
		}
	}

	fn overview(&self, range: Range) -> Result<Value, String> {
		let rows = self.query(range, SmallVec::new(), UsageBucketWidth::None)?;
		let total = rows.iter().fold(Totals::default(), |mut total, row| {
			total.add(row);
			total
		});
		Ok(json!({"overall": total.value()}))
	}

	fn grouped(&self, range: Range, dimension: UsageDimension) -> Result<Value, String> {
		let rows = self.query(range, SmallVec::from_buf([dimension]), UsageBucketWidth::None)?;
		Ok(json!({"rows": rows.iter().map(bucket_value).collect::<Vec<_>>() }))
	}

	fn costs(&self, range: Range) -> Result<Value, String> {
		let rows =
			self.query(range, SmallVec::from_buf([UsageDimension::Model]), UsageBucketWidth::None)?;
		Ok(json!({"rows": rows.iter().map(|row| json!({
			"model": key(row, UsageDimension::Model),
			"cost_nanos_usd": row.cost.nanos_usd,
			"cost_usd": row.cost.nanos_usd as f64 / 1_000_000_000.0,
			"estimated": row.cost.estimated,
		})).collect::<Vec<_>>() }))
	}

	fn timeseries(&self, range: Range) -> Result<Value, String> {
		let rows = self.query(range, SmallVec::new(), range.bucket)?;
		Ok(json!({"rows": rows.iter().map(bucket_value).collect::<Vec<_>>() }))
	}

	fn recent(&self, range: Range, errors_only: bool) -> Result<Value, String> {
		let page = self
			.index
			.list(&SessionFilter {
				since_ms: range.since_ms,
				until_ms: range.until_ms,
				limit: 100,
				..SessionFilter::default()
			})
			.map_err(|error| error.to_string())?;
		let rows = page
			.sessions
			.into_iter()
			.filter_map(|session| {
				if errors_only && session.status != SessionStatus::Error {
					return None;
				}
				Some(json!({
					"session_id": session.id.0.as_str(), "title": session.title.as_deref(),
					"project": session.project.as_str(), "kind": session.kind.to_string(),
					"status": session.status.to_string(), "updated_ms": session.updated_ms,
					"turns": session.turns, "entries": session.entries,
				}))
			})
			.collect::<Vec<_>>();
		Ok(json!({"rows": rows}))
	}

	fn tools(&self, range: Range) -> Result<Value, String> {
		let page = self
			.index
			.list(&SessionFilter {
				since_ms: range.since_ms,
				until_ms: range.until_ms,
				limit: 200,
				..SessionFilter::default()
			})
			.map_err(|error| error.to_string())?;
		let mut calls = 0_u64;
		let mut results = 0_u64;
		let mut errors = 0_u64;
		for session in page.sessions {
			let stats = self
				.index
				.session_statistics(&session.id, false)
				.map_err(|error| error.to_string())?;
			calls = calls.saturating_add(stats.tool_calls);
			results = results.saturating_add(stats.tool_results);
			errors = errors.saturating_add(stats.tool_errors);
		}
		Ok(json!({"rows": [{
			"tool": "all", "calls": calls, "results": results, "errors": errors,
		}]}))
	}

	fn request(&self, id: &str) -> Result<Value, String> {
		let Some((session, event)) = id.rsplit_once(':') else {
			return Err("request id must be SESSION:EVENT".to_owned());
		};
		let event_index = event
			.parse::<u64>()
			.map_err(|_| "request event is not an integer".to_owned())?;
		let receipt = self
			.index
			.receipt(&SessionId(Str::new(session)), event_index)
			.map_err(|error| error.to_string())?;
		Ok(receipt.map_or(Value::Null, |receipt| {
			json!({
				"id": id, "session_id": session, "event_index": event_index,
				"usage": usage_value(&receipt.usage), "cost_nanos_usd": receipt.cost.nanos_usd,
				"redacted": true,
			})
		}))
	}

	fn query(
		&self,
		range: Range,
		group_by: SmallVec<UsageDimension, 3>,
		bucket: UsageBucketWidth,
	) -> Result<Vec<UsageBucket>, String> {
		self
			.index
			.usage(&UsageQuery {
				since_ms: range.since_ms,
				until_ms: range.until_ms,
				group_by,
				bucket,
				include_subagents: true,
				..UsageQuery::default()
			})
			.map_err(|error| error.to_string())
	}

	/// Serializes manual synchronization through the same cross-process lock.
	pub fn sync_document(&self) -> Result<Value, &'static str> {
		let file = std::fs::OpenOptions::new()
			.write(true)
			.create_new(true)
			.open(&self.sync_lock)
			.map_err(|_| "another process is synchronizing statistics")?;
		drop(file);
		let _ = std::fs::remove_file(&self.sync_lock);
		Ok(json!({"version": API_VERSION, "data": {"processed": 0, "source": "write_time_index"}}))
	}

	fn sync(&self) -> Response<Body> {
		match self.sync_document() {
			Ok(document) => json_response(StatusCode::OK, document),
			Err(message) => error_response(StatusCode::CONFLICT, "sync_busy", message),
		}
	}
}

#[derive(Clone, Copy)]
struct Range {
	since_ms: Option<u64>,
	until_ms: Option<u64>,
	bucket:   UsageBucketWidth,
	label:    &'static str,
}

impl Range {
	fn parse(query: Option<&str>) -> Result<Self, &'static str> {
		let mut params = BTreeMap::new();
		for pair in query
			.unwrap_or_default()
			.split('&')
			.filter(|pair| !pair.is_empty())
		{
			let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
			params.insert(key, value);
		}
		let now = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.unwrap_or_default()
			.as_millis() as u64;
		let named = params.get("range").copied().unwrap_or("30d");
		let (mut since_ms, mut bucket, label) = match named {
			"24h" => (Some(now.saturating_sub(86_400_000)), UsageBucketWidth::Hour, "24h"),
			"7d" => (Some(now.saturating_sub(604_800_000)), UsageBucketWidth::Day, "7d"),
			"30d" => (Some(now.saturating_sub(2_592_000_000)), UsageBucketWidth::Day, "30d"),
			"90d" => (Some(now.saturating_sub(7_776_000_000)), UsageBucketWidth::Week, "90d"),
			"all" => (None, UsageBucketWidth::Month, "all"),
			_ => return Err("range must be 24h, 7d, 30d, 90d, or all"),
		};
		let mut until_ms = Some(now);
		if let Some(value) = params.get("since") {
			since_ms = Some(
				value
					.parse()
					.map_err(|_| "since must be epoch milliseconds")?,
			);
		}
		if let Some(value) = params.get("until") {
			until_ms = Some(
				value
					.parse()
					.map_err(|_| "until must be epoch milliseconds")?,
			);
		}
		if since_ms
			.zip(until_ms)
			.is_some_and(|(since, until)| since > until)
		{
			return Err("since must not be later than until");
		}
		if let Some(value) = params.get("bucket") {
			bucket = match *value {
				"none" => UsageBucketWidth::None,
				"hour" => UsageBucketWidth::Hour,
				"day" => UsageBucketWidth::Day,
				"week" => UsageBucketWidth::Week,
				"month" => UsageBucketWidth::Month,
				_ => return Err("bucket must be none, hour, day, week, or month"),
			};
		}
		Ok(Self { since_ms, until_ms, bucket, label })
	}
}

#[derive(Default)]
struct Totals {
	requests:    u64,
	errors:      u64,
	input:       u64,
	output:      u64,
	cache_read:  u64,
	cache_write: u64,
	premium:     u64,
	cost:        u64,
	duration:    u64,
	sessions:    u64,
}

impl Totals {
	fn add(&mut self, row: &UsageBucket) {
		self.requests = self.requests.saturating_add(row.requests);
		self.errors = self.errors.saturating_add(row.errors);
		self.input = self.input.saturating_add(row.usage.input_tokens);
		self.output = self.output.saturating_add(row.usage.output_tokens);
		self.cache_read = self.cache_read.saturating_add(row.usage.cache_read_tokens);
		self.cache_write = self
			.cache_write
			.saturating_add(row.usage.cache_write_tokens);
		self.premium = self
			.premium
			.saturating_add(row.usage.premium_requests.unwrap_or_default());
		self.cost = self.cost.saturating_add(row.cost.nanos_usd);
		self.duration = self.duration.saturating_add(row.duration_ms);
		self.sessions = self.sessions.saturating_add(row.sessions);
	}

	fn value(&self) -> Value {
		json!({
			"requests": self.requests, "errors": self.errors,
			"input_tokens": self.input, "output_tokens": self.output,
			"cache_read_tokens": self.cache_read, "cache_write_tokens": self.cache_write,
			"premium_requests": self.premium, "cost_nanos_usd": self.cost,
			"cost_usd": self.cost as f64 / 1_000_000_000.0,
			"duration_ms": self.duration, "sessions": self.sessions,
		})
	}
}

fn key(row: &UsageBucket, dimension: UsageDimension) -> Option<&str> {
	row.key
		.iter()
		.find_map(|(candidate, value)| (*candidate == dimension).then_some(value.as_str()))
}
fn usage_value(usage: &omp_proto::omp::inference::v1::Usage) -> Value {
	json!({
		"input_tokens": usage.input_tokens, "output_tokens": usage.output_tokens,
		"cache_read_tokens": usage.cache_read_tokens, "cache_write_tokens": usage.cache_write_tokens,
		"total_tokens": usage.total_tokens, "context_tokens": usage.context_tokens,
		"premium_requests": usage.premium_requests, "reasoning_tokens": usage.reasoning_tokens,
	})
}
fn bucket_value(row: &UsageBucket) -> Value {
	json!({
		"key": row.key.iter().map(|(dimension, value)| (dimension.to_string(), value.as_str())).collect::<BTreeMap<_, _>>(),
		"start_ms": row.start_ms, "requests": row.requests, "errors": row.errors,
		"duration_ms": row.duration_ms, "sessions": row.sessions,
		"usage": usage_value(&row.usage), "cost_nanos_usd": row.cost.nanos_usd,
	})
}
fn envelope_response(data: Value, range: Range) -> Response<Body> {
	json_response(
		StatusCode::OK,
		json!({"version": API_VERSION, "data": data, "meta": {"range": range.label}}),
	)
}
fn error_response(status: StatusCode, code: &str, message: &str) -> Response<Body> {
	json_response(
		status,
		json!({"version": API_VERSION, "error": {"code": code, "message": message}}),
	)
}
fn json_response(status: StatusCode, value: Value) -> Response<Body> {
	let bytes = serde_json::to_vec(&value)
		.unwrap_or_else(|_| b"{\"error\":{\"code\":\"serialization_failed\"}}".to_vec());
	Response::builder()
		.status(status)
		.header(header::CONTENT_TYPE, "application/json; charset=utf-8")
		.body(Full::new(Bytes::from(bytes)))
		.unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
}
