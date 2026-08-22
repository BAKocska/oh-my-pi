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
	#[must_use]
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
			"behavior" | "gain" => Ok(json!({"rows": [], "available": false})),
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
