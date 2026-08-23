//! Canonical web-search tool over an application-owned inference backend.

use std::{
	error,
	fmt::{self, Display},
	sync::Arc,
};

use async_stream::stream;
use futures::Stream;
use omp_core::{Str, sf};
use omp_proto::inference::v1::{self as pb, search_request};
use omp_tool::{
	Abort, ArgIssue, ArgIssueKind, CommitError, Constraint, Effects, Ev, ExecEffects,
	IncomingParams, InferenceEffects, ParamError, Part, PromptCaps, Rev, Tool, ToolSpec,
	ToolTerminal, Usd,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Arguments accepted by `web_search@1`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Params {
	/// Google-style search query.
	#[schemars(with = "String")]
	pub query:              Str,
	/// Optional recency window: `day`, `week`, `month`, or `year`.
	#[serde(default)]
	pub recency:            Option<Recency>,
	/// Maximum results returned to the caller.
	#[serde(default)]
	pub limit:              Option<u32>,
	/// Maximum tokens in a synthesized answer.
	#[serde(default)]
	pub max_tokens:         Option<u32>,
	/// Synthesis sampling temperature.
	#[serde(default)]
	pub temperature:        Option<f64>,
	/// Provider retrieval count when distinct from `limit`.
	#[serde(default)]
	pub num_search_results: Option<u32>,
	/// Explicit provider pin; omitted uses configured automatic routing.
	#[serde(default)]
	#[schemars(with = "Option<String>")]
	pub provider:           Option<Str>,
}

/// Supported relative recency windows.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Recency {
	/// Previous day.
	Day,
	/// Previous week.
	Week,
	/// Previous month.
	Month,
	/// Previous year.
	Year,
}

/// Lossless canonical search response.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Payload {
	/// Canonical response returned by the inference facade.
	pub response: pb::SearchResponse,
}

/// Web search does not stream partial tool updates.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Update {}

/// Stable backend failure safe to project to the model.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BackendError {
	/// Stable error classification.
	pub code:    Str,
	/// Secret-free diagnostic.
	pub message: Str,
}

/// Search invocation failure.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fault {
	/// The application-owned inference backend rejected or failed the request.
	Search {
		/// Stable error classification.
		code:    Str,
		/// Secret-free diagnostic.
		message: Str,
	},
}

impl Display for Fault {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Search { code, message } => {
				write!(formatter, "web search failed ({code}): {message}")
			},
		}
	}
}

impl error::Error for Fault {}

/// Application-owned canonical search execution boundary.
///
/// Implementations must route through the one production inference facade;
/// tools never construct providers, credentials, or fallback registries.
pub trait SearchBackend: Send + Sync + 'static {
	/// Executes one canonical protobuf request.
	fn search(
		&self,
		request: pb::SearchRequest,
	) -> impl Future<Output = Result<pb::SearchResponse, BackendError>> + Send + '_;
}

/// Versioned `web_search` executor.
pub struct WebSearch<B> {
	backend: Arc<B>,
	spec:    ToolSpec,
}

/// Creates `web_search@1` over an application-owned inference backend.
pub fn tool<B: SearchBackend>(backend: Arc<B>) -> WebSearch<B> {
	WebSearch {
		backend,
		spec: ToolSpec {
			name:            sf!("web_search"),
			rev:             Rev { family: Default::default(), n: 1 },
			description:     sf!(
				"Searches the web through configured providers. Supports Google-style query \
				 directives, ordered automatic fallback, or an explicit provider pin."
			),
			schema:          omp_tool::schema::<Params>(),
			constraint:      Constraint::Schema {
				priority:       100,
				on_unsupported: omp_tool::Fallback::Unspecified,
			},
			effects:         Effects {
				documents: None,
				exec:      Some(ExecEffects { network: true, commands: Arc::default() }),
				inference: Some(InferenceEffects {
					max_requests: 1,
					max_usd:      Usd::from_nanos(u64::MAX),
				}),
				desktop:   None,
				subagents: 0,
			},
			projection_code: omp_tool::native_projection_code(
				env!("CARGO_PKG_NAME"),
				env!("CARGO_PKG_VERSION"),
				include_bytes!("web_search.rs"),
			)
			.into(),
		},
	}
}

impl<B: SearchBackend> Tool for WebSearch<B> {
	type Fault = Fault;
	type Params = Params;
	type Payload = Payload;
	type Update = Update;

	fn spec(&self) -> &ToolSpec {
		&self.spec
	}

	fn call<'c>(
		&'c self,
		mut incoming: IncomingParams<'c>,
	) -> impl Stream<Item = Ev<Update, Payload, Fault>> + Send + 'c {
		stream! {
			let params = match incoming.whole::<Params>().await {
				Ok(value) => value,
				Err(error) => { yield param_event(error); return; }
			};
			if let Err(error) = incoming.interruptable().committed().await {
				yield commit_event(error);
				return;
			}
			let request = into_request(params);
			let result = self.backend.search(request).await
				.map(|response| Payload { response })
				.map_err(|error| Fault::Search { code: error.code, message: error.message });
			yield Ev::Done(ToolTerminal::Done { result, useless: false });
		}
	}

	fn prompt(&self, view: Result<&Payload, &Fault>, caps: &PromptCaps) -> Vec<Part> {
		let text = match view {
			Ok(payload) => render_response(&payload.response),
			Err(fault) => fault.to_string(),
		};
		let mut end = text.len().min(caps.maximum_text_bytes as usize);
		while end != 0 && !text.is_char_boundary(end) {
			end -= 1;
		}
		(end != 0)
			.then(|| Part::Text { text: Str::new(&text[..end]) })
			.into_iter()
			.collect()
	}
}

fn into_request(params: Params) -> pb::SearchRequest {
	pb::SearchRequest {
		query: params.query.to_string(),
		limit: params.limit.unwrap_or(0),
		recency: params.recency.map_or(0, |recency| match recency {
			Recency::Day => search_request::Recency::Day as i32,
			Recency::Week => search_request::Recency::Week as i32,
			Recency::Month => search_request::Recency::Month as i32,
			Recency::Year => search_request::Recency::Year as i32,
		}),
		engine: params
			.provider
			.as_deref()
			.map_or_else(String::new, ToString::to_string),
		max_tokens: params.max_tokens.unwrap_or(0),
		temperature: params.temperature,
		num_search_results: params.num_search_results.unwrap_or(0),
		..Default::default()
	}
}

fn render_response(response: &pb::SearchResponse) -> String {
	let mut output = String::new();
	if !response.answer.is_empty() {
		output.push_str(&response.answer);
		output.push_str("\n\n");
	}
	if !response.sources.is_empty() {
		output.push_str("## Sources\n\n");
		for (index, source) in response.sources.iter().enumerate() {
			output.push_str(&format!("{}. [{}]({})", index + 1, source.title, source.url));
			if !source.snippet.is_empty() {
				output.push_str(" — ");
				output.push_str(&source.snippet);
			}
			output.push('\n');
		}
	}
	for warning in &response.warnings {
		output.push_str("\n> ");
		output.push_str(warning);
	}
	output
}

fn param_event(error: ParamError) -> Ev<Update, Payload, Fault> {
	match error {
		ParamError::Args(issue) => Ev::Args(*issue),
		ParamError::Interrupted(interrupt) => {
			Ev::Aborted(Abort::Interrupted { reason: interrupt.reason })
		},
		ParamError::Protocol(message) => Ev::Args(protocol_issue(message)),
	}
}

fn commit_event(error: CommitError) -> Ev<Update, Payload, Fault> {
	match error {
		CommitError::Aborted => Ev::Aborted(Abort::InputDropped),
		CommitError::Interrupted(interrupt) => {
			Ev::Aborted(Abort::Interrupted { reason: interrupt.reason })
		},
		CommitError::Protocol(message) => Ev::Args(protocol_issue(message)),
	}
}

fn protocol_issue(message: Str) -> ArgIssue {
	ArgIssue {
		path:     Vec::new(),
		expected: sf!("one committed JSON argument object"),
		kind:     ArgIssueKind::Protocol,
		example:  Some(sf!(r#"{{"query":"rust async traits"}} "#)),
		found:    Some(message),
	}
}
