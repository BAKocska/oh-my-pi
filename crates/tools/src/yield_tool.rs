//! Subagent terminal and incremental structured-output submission.

use std::{
	error,
	fmt::{self, Display},
};

use async_stream::stream;
use futures::Stream;
use omp_core::{Str, sf};
use omp_tool::{
	Abort, ArgIssue, ArgIssueKind, CommitError, Constraint, Effects, Ev, IncomingParams, ParamError,
	Part, PromptCaps, Rev, Tool, ToolSpec, ToolTerminal,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Arguments accepted by `yield@1`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Params {
	/// Terminal label or non-empty incremental section path.
	#[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
	pub kind:   Option<YieldType>,
	/// Success/failure envelope. May be omitted only for terminal last-turn
	/// fallback.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub result: Option<ResultEnvelope>,
}

/// Terminal label or incremental section path.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum YieldType {
	/// Named terminal result.
	Terminal(Str),
	/// Non-empty incremental section path.
	Sections(Vec<Str>),
}

/// Structured success or failure.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Eq, Serialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum ResultEnvelope {
	/// Successful structured output.
	Data {
		/// Caller-schema-bound structured value.
		#[schemars(schema_with = "loose_record_schema")]
		data: Value,
	},
	/// Terminal failure description.
	Error {
		/// Human-readable failure.
		error: Str,
	},
}

/// Durable yield acknowledgement. The caller consumes the original argument
/// bytes for schema validation; this payload never substitutes for them.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Payload {
	/// Whether this is an incremental section.
	pub incremental:   bool,
	/// Whether finalization must consume the child's last assistant turn.
	pub use_last_turn: bool,
}

/// Yield does not stream updates.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Update {}

/// Loose object schema for `data`. Providers reject property schemas without
/// a `type` key, and strict validation of the caller schema happens
/// caller-side (`YieldPayloadValidator`), so the wire schema stays advisory.
fn loose_record_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
	schemars::json_schema!({
		"type": "object",
		"additionalProperties": true,
	})
}

/// Invalid yield envelope.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Fault {
	message: Str,
}
impl Display for Fault {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.message)
	}
}
impl error::Error for Fault {}

/// Yield executor. Caller-side `YieldPayloadValidator` validates original raw
/// call arguments against the caller schema using omp-tool argument machinery.
pub struct Yield {
	spec: ToolSpec,
}

/// Creates `yield@1`.
pub fn tool() -> Yield {
	Yield {
		spec: ToolSpec {
			name:            sf!("yield"),
			rev:             Rev { family: Default::default(), n: 1 },
			description:     sf!(
				"Submits terminal or incremental subagent output. Structured success uses \
				 `result.data`; failure uses `result.error`. A terminal typed yield may omit result \
				 to use the last assistant turn.",
			),
			schema:          omp_tool::schema::<Params>(),
			// Never strict: strict sampling forbids `additionalProperties: true`,
			// so an arbitrary caller-schema-bound `data` value cannot ride a
			// strict declaration. The caller-side validator owns the real check.
			constraint:      Constraint::None,
			effects:         Effects::empty(),
			projection_code: omp_tool::native_projection_code(
				env!("CARGO_PKG_NAME"),
				env!("CARGO_PKG_VERSION"),
				include_bytes!("yield_tool.rs"),
			)
			.into(),
		},
	}
}

impl Tool for Yield {
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
			let incremental = matches!(&params.kind, Some(YieldType::Sections(_)));
			if let Some(YieldType::Sections(parts)) = &params.kind
				&& (parts.is_empty() || parts.iter().any(|part| part.trim().is_empty()))
			{
				yield done(Err(Fault { message: sf!("type sections must be non-empty strings") }));
				return;
			}
			let use_last_turn = params.result.is_none();
			if use_last_turn && (params.kind.is_none() || incremental) {
				yield done(Err(Fault { message: sf!("result is required unless a terminal type requests last-turn fallback") }));
				return;
			}
			if let Err(error) = incoming.interruptable().committed().await { yield commit_event(error); return; }
			yield done(Ok(Payload { incremental, use_last_turn }));
		}
	}

	fn prompt(&self, view: Result<&Payload, &Fault>, _: &PromptCaps) -> Vec<Part> {
		vec![Part::Text {
			text: match view {
				Ok(payload) if payload.incremental => sf!("Incremental section accepted."),
				Ok(_) => sf!("Result accepted."),
				Err(fault) => Str::new(fault.to_string()),
			},
		}]
	}
}

const fn done(result: Result<Payload, Fault>) -> Ev<Update, Payload, Fault> {
	Ev::Done(ToolTerminal::Done { result, useless: false })
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
		example:  Some(sf!(r#"{{"result":{{"data":{{}}}}}}"#)),
		found:    Some(message),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn accepts_terminal_incremental_and_last_turn_envelopes() {
		let terminal: Params =
			serde_json::from_value(serde_json::json!({"result":{"data":{"ok":true}}})).unwrap();
		assert!(matches!(terminal.result, Some(ResultEnvelope::Data { .. })));
		let incremental: Params =
			serde_json::from_value(serde_json::json!({"type":["findings"],"result":{"data":[1,2]}}))
				.unwrap();
		assert!(matches!(incremental.kind, Some(YieldType::Sections(_))));
		let fallback: Params = serde_json::from_value(serde_json::json!({"type":"result"})).unwrap();
		assert!(fallback.result.is_none());
	}

	#[test]
	fn envelope_rejects_unknown_fields() {
		assert!(
			serde_json::from_value::<Params>(
				serde_json::json!({"result":{"data":1},"schemaOverridden":true})
			)
			.is_err()
		);
	}
	#[test]
	fn wire_schema_types_data_and_never_requests_strict_sampling() {
		// OpenAI-side validation rejects any property schema without a `type`
		// key, and strict sampling forbids `additionalProperties: true`.
		let yield_tool = tool();
		assert_eq!(yield_tool.spec().constraint, Constraint::None);
		let schema: Value = serde_json::from_slice(&yield_tool.spec().schema).unwrap();
		let data = &schema["properties"]["result"]["anyOf"][0]["anyOf"][0]["properties"]["data"];
		assert_eq!(data["type"], "object");
		assert_eq!(data["additionalProperties"], true);
	}
}
