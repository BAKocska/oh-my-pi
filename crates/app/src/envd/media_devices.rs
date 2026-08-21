//! Harness-owned dynamic devices for media generation and AutoQA reports.

use std::{
	fmt,
	sync::Arc,
	time::{SystemTime, UNIX_EPOCH},
};

use async_stream::stream;
use futures::Stream;
use omp_core::{Str, sf};
use omp_storage::telemetry_index::{StoredIssue, TelemetryIndex};
use omp_tool::{
	Abort, ArgIssue, ArgIssueKind, CommitError, Constraint, Effects, Ev, IncomingParams, ParamError,
	Part, PromptCaps, Rev, Tool, ToolSpec, ToolTerminal,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Media generation arguments. Each device validates its required text field.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MediaParams {
	/// Image prompt, either plain text or a provider-neutral structured object.
	pub prompt:       Option<Value>,
	/// Speech text, required by `tts`.
	#[schemars(with = "Option<String>")]
	pub text:         Option<Str>,
	/// Requested provider, or the configured fallback order when absent.
	#[schemars(with = "Option<String>")]
	pub provider:     Option<Str>,
	/// Image aspect ratio such as `1:1` or `16:9`.
	#[schemars(with = "Option<String>")]
	pub aspect_ratio: Option<Str>,
	/// Optional source image artifact for image-to-image generation.
	#[schemars(with = "Option<String>")]
	pub input_image:  Option<Str>,
	/// Voice identifier for speech synthesis.
	#[schemars(with = "Option<String>")]
	pub voice:        Option<Str>,
	/// Requested output format.
	#[schemars(with = "Option<String>")]
	pub format:       Option<Str>,
}

/// A generated media artifact. Unavailable backends never fabricate one.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MediaPayload {
	/// Content-addressed artifact id.
	pub artifact_id: Str,
	/// Produced MIME type.
	pub media_type:  Str,
}

/// Stable structured media backend failure.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MediaFault {
	/// Machine-readable failure category.
	pub code:    Str,
	/// Backend that could not serve the request.
	pub backend: Str,
	/// Human-readable explanation.
	pub message: Str,
}

impl fmt::Display for MediaFault {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(formatter, "{}: {}", self.code, self.message)
	}
}
impl std::error::Error for MediaFault {}

/// Media devices do not currently stream previews.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum MediaUpdate {}

#[derive(Clone, Copy)]
enum MediaKind {
	Image,
	Speech,
}

/// Dyn-mounted media generator.
pub struct MediaDevice {
	spec: ToolSpec,
	kind: MediaKind,
}

/// Creates the `image_gen@1` dynamic device.
#[must_use]
pub fn image_gen() -> MediaDevice {
	media_device(
		"image_gen",
		"Generates images from structured prompts with provider, aspect-ratio, and format routing.",
		MediaKind::Image,
	)
}

/// Creates the `tts@1` dynamic device.
#[must_use]
pub fn tts() -> MediaDevice {
	media_device(
		"tts",
		"Synthesizes speech with local Kokoro or a configured remote voice backend.",
		MediaKind::Speech,
	)
}

fn media_device(name: &'static str, description: &'static str, kind: MediaKind) -> MediaDevice {
	MediaDevice {
		spec: ToolSpec {
			name:            sf!(name),
			rev:             Rev { family: Default::default(), n: 1 },
			description:     sf!(description),
			schema:          omp_tool::schema::<MediaParams>(),
			constraint:      Constraint::Schema {
				priority:       100,
				on_unsupported: omp_tool::Fallback::Unspecified,
			},
			effects:         Effects::empty(),
			projection_code: omp_tool::native_projection_code(
				env!("CARGO_PKG_NAME"),
				env!("CARGO_PKG_VERSION"),
				include_bytes!("media_devices.rs"),
			)
			.into_bytes(),
		},
		kind,
	}
}

impl Tool for MediaDevice {
	type Fault = MediaFault;
	type Params = MediaParams;
	type Payload = MediaPayload;
	type Update = MediaUpdate;

	fn spec(&self) -> &ToolSpec {
		&self.spec
	}

	fn call<'c>(
		&'c self,
		mut incoming: IncomingParams<'c>,
	) -> impl Stream<Item = Ev<MediaUpdate, MediaPayload, MediaFault>> + Send + 'c {
		stream! {
			let params = match incoming.whole::<MediaParams>().await {
				Ok(params) => params,
				Err(error) => { yield media_param_event(error); return; },
			};
			let valid = match self.kind {
				MediaKind::Image => params.prompt.as_ref().is_some_and(|prompt| match prompt {
					Value::String(text) => !text.trim().is_empty(),
					Value::Object(fields) => !fields.is_empty(),
					_ => false,
				}),
				MediaKind::Speech => params.text.as_deref().is_some_and(|text| !text.trim().is_empty()),
			};
			if !valid {
				let field = match self.kind { MediaKind::Image => "prompt", MediaKind::Speech => "text" };
				yield media_done(Err(MediaFault { code: sf!("invalid_media_request"), backend: sf!("none"), message: Str::from(format!("{field} must not be empty")) }));
				return;
			}
			if let Err(error) = incoming.interruptable().committed().await { yield media_commit_event(error); return; }
			let (code, backend, message) = match self.kind {
				MediaKind::Image => (
					"image_backend_unavailable",
					params.provider.as_deref().unwrap_or("remote"),
					"no image-generation remote backend is configured",
				),
				MediaKind::Speech => (
					"tts_backend_unavailable",
					params.provider.as_deref().unwrap_or("kokoro"),
					"Kokoro requires configured model and voice artifacts; no remote speech backend is configured",
				),
			};
			yield media_done(Err(MediaFault { code: Str::from(code), backend: Str::from(backend), message: Str::from(message) }));
		}
	}

	fn prompt(&self, view: Result<&MediaPayload, &MediaFault>, _: &PromptCaps) -> Vec<Part> {
		vec![Part::Text {
			text: match view {
				Ok(payload) => Str::from(format!(
					"Generated {} artifact {}",
					payload.media_type, payload.artifact_id
				)),
				Err(fault) => {
					Str::from(serde_json::to_string(fault).unwrap_or_else(|_| fault.to_string()))
				},
			},
		}]
	}
}

/// Arguments accepted by `report_issue@1`.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReportParams {
	/// Session filing the report.
	#[schemars(with = "String")]
	pub session_id: Str,
	/// Device whose structured result was inconsistent.
	#[schemars(with = "String")]
	pub device:     Str,
	/// Observed device revision, required for exact `name@rev` attribution.
	#[schemars(with = "String")]
	pub rev:        Str,
	/// Structured verdict over the device result.
	pub verdict:    Value,
	/// User sharing disposition; defaults to `local_only`.
	#[schemars(with = "Option<String>")]
	pub consent:    Option<Str>,
}

/// Durable AutoQA filing result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReportPayload {
	/// Stable issue identifier.
	pub issue_id: Str,
	/// Exact reported `name@rev` identity.
	pub target:   Str,
	/// Firehose event kind written beside the issue.
	pub kind:     Str,
}

/// Dyn-mounted AutoQA issue recorder.
pub struct ReportIssue {
	spec:  ToolSpec,
	store: Arc<TelemetryIndex>,
}

/// Creates `report_issue@1` over the project AutoQA store.
#[must_use]
pub fn report_issue(store: Arc<TelemetryIndex>) -> ReportIssue {
	ReportIssue {
		spec: ToolSpec {
			name:            sf!("report_issue"),
			rev:             Rev { family: Default::default(), n: 1 },
			description:     sf!(
				"Records a structured AutoQA verdict against an exact device revision in the durable \
				 local issue store.",
			),
			schema:          omp_tool::schema::<ReportParams>(),
			constraint:      Constraint::Schema {
				priority:       255,
				on_unsupported: omp_tool::Fallback::Unspecified,
			},
			effects:         Effects::empty(),
			projection_code: omp_tool::native_projection_code(
				env!("CARGO_PKG_NAME"),
				env!("CARGO_PKG_VERSION"),
				include_bytes!("media_devices.rs"),
			)
			.into_bytes(),
		},
		store,
	}
}

impl Tool for ReportIssue {
	type Fault = MediaFault;
	type Params = ReportParams;
	type Payload = ReportPayload;
	type Update = MediaUpdate;

	fn spec(&self) -> &ToolSpec {
		&self.spec
	}

	fn call<'c>(
		&'c self,
		mut incoming: IncomingParams<'c>,
	) -> impl Stream<Item = Ev<MediaUpdate, ReportPayload, MediaFault>> + Send + 'c {
		stream! {
			let params = match incoming.whole::<ReportParams>().await {
				Ok(params) => params,
				Err(error) => { yield report_param_event(error); return; },
			};
			if params.session_id.trim().is_empty() || params.device.trim().is_empty() || params.rev.trim().is_empty() || !params.verdict.is_object() {
				yield report_done(Err(MediaFault { code: sf!("invalid_issue_report"), backend: sf!("autoqa"), message: sf!("session_id, device, and rev are required and verdict must be an object") }));
				return;
			}
			if let Err(error) = incoming.interruptable().committed().await { yield report_commit_event(error); return; }
			let now = now_ms();
			let issue_id = Str::from(ulid::Ulid::generate().to_string());
			let consent = params.consent.unwrap_or_else(|| sf!("local_only"));
			let issue = StoredIssue { id: issue_id.clone(), session_id: params.session_id.clone(), device: params.device.clone(), rev: Some(params.rev.clone()), consent, created_at_ms: now };
			if let Err(error) = self.store.store_issue(&issue) {
				yield report_done(Err(store_fault(error.to_string()))); return;
			}
			let encoded = serde_json::json!({ "issue_id": issue_id.clone(), "device": params.device, "rev": params.rev.clone(), "verdict": params.verdict });
			if let Err(error) = self.store.append(issue.session_id.as_str(), "issue_report", now, encoded.to_string().as_bytes()) {
				yield report_done(Err(store_fault(error.to_string()))); return;
			}
			let target = format!("{}@{}", issue.device, params.rev);
			yield report_done(Ok(ReportPayload { issue_id, target: Str::from(target), kind: sf!("issue_report") }));
		}
	}

	fn prompt(&self, view: Result<&ReportPayload, &MediaFault>, _: &PromptCaps) -> Vec<Part> {
		vec![Part::Text {
			text: match view {
				Ok(payload) => {
					Str::from(format!("Filed AutoQA issue {} for {}.", payload.issue_id, payload.target))
				},
				Err(fault) => {
					Str::from(serde_json::to_string(fault).unwrap_or_else(|_| fault.to_string()))
				},
			},
		}]
	}
}

fn now_ms() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or_default()
		.as_millis()
		.try_into()
		.unwrap_or(u64::MAX)
}
fn store_fault(message: String) -> MediaFault {
	MediaFault {
		code:    sf!("autoqa_store_failed"),
		backend: sf!("sqlite"),
		message: Str::from(message),
	}
}
fn media_done(
	result: Result<MediaPayload, MediaFault>,
) -> Ev<MediaUpdate, MediaPayload, MediaFault> {
	Ev::Done(ToolTerminal::Done { result, useless: false })
}
fn report_done(
	result: Result<ReportPayload, MediaFault>,
) -> Ev<MediaUpdate, ReportPayload, MediaFault> {
	Ev::Done(ToolTerminal::Done { result, useless: false })
}
fn media_param_event(error: ParamError) -> Ev<MediaUpdate, MediaPayload, MediaFault> {
	map_param(error)
}
fn report_param_event(error: ParamError) -> Ev<MediaUpdate, ReportPayload, MediaFault> {
	map_param(error)
}
fn media_commit_event(error: CommitError) -> Ev<MediaUpdate, MediaPayload, MediaFault> {
	map_commit(error)
}
fn report_commit_event(error: CommitError) -> Ev<MediaUpdate, ReportPayload, MediaFault> {
	map_commit(error)
}
fn map_param<U, P, F>(error: ParamError) -> Ev<U, P, F> {
	match error {
		ParamError::Args(issue) => Ev::Args(*issue),
		ParamError::Interrupted(interrupt) => {
			Ev::Aborted(Abort::Interrupted { reason: interrupt.reason })
		},
		ParamError::Protocol(message) => Ev::Args(protocol_issue(message)),
	}
}
fn map_commit<U, P, F>(error: CommitError) -> Ev<U, P, F> {
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
		example:  None,
		found:    Some(message),
	}
}
#[cfg(test)]
mod tests {
	use tempfile::tempdir;

	use super::*;

	#[test]
	fn report_schema_is_closed_and_accepts_structured_verdicts() {
		assert!(
			serde_json::from_value::<ReportParams>(
				serde_json::json!({"session_id":"s","device":"read","rev":"2","verdict":{"ok":false}})
			)
			.is_ok()
		);
		assert!(
			serde_json::from_value::<ReportParams>(
				serde_json::json!({"session_id":"s","device":"read","rev":"2","verdict":{},"extra":true})
			)
			.is_err()
		);
	}

	#[test]
	fn report_store_round_trips_issue_rows() {
		let root = tempdir().unwrap();
		let store = TelemetryIndex::open(root.path(), &root.path().join("telemetry.sqlite")).unwrap();
		let issue = StoredIssue {
			id:            sf!("i"),
			session_id:    sf!("s"),
			device:        sf!("read"),
			rev:           Some(sf!("2")),
			consent:       sf!("local_only"),
			created_at_ms: 1,
		};
		store.store_issue(&issue).unwrap();
		assert_eq!(store.issue("i").unwrap(), Some(issue));
	}
}
