//! Post-hoc repair parsers for foreign append-only session transcripts.
//!
//! Foreign records are deliberately parsed line-by-line: damaged records become
//! diagnostics while later complete records retain their source line identity.

use omp_core::{Str, sf, time::parse_rfc3339};
use serde_json::Value;

use super::{
	Attribution, Block, BlockKind, CallId, Event, Kind, ModelId, ModelRef, Msg, ProviderId, Stop,
	Timing, Usage, UserBlock,
};

/// Supported foreign transcript dialects.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForeignFormat {
	/// Claude Code's JSONL session log.
	ClaudeCode,
	/// Codex CLI's rollout JSONL log.
	Codex,
}

impl ForeignFormat {
	const fn marker(self) -> &'static str {
		match self {
			Self::ClaudeCode => "claude_code",
			Self::Codex => "codex",
		}
	}
}

/// One recoverable problem found while scanning a foreign journal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportDiagnostic {
	/// One-based physical source line.
	pub line:   u64,
	/// Stable diagnostic category.
	pub code:   Str,
	/// Human-readable bounded explanation.
	pub reason: Str,
}

/// Parsed foreign content with its durable provenance marker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedEntry {
	/// One-based source line, stable even when preceding lines are malformed.
	pub source_line: u64,
	/// Source-native record identifier when present.
	pub source_id:   Option<Str>,
	/// Canonical message event.
	pub event:       Event,
}

/// Result of a post-hoc foreign journal scan.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportedTranscript {
	/// Canonical conversational entries in source order.
	pub entries:     Vec<ImportedEntry>,
	/// Malformed or unsupported records that were not imported.
	pub diagnostics: Vec<ImportDiagnostic>,
}

impl ImportedTranscript {
	/// Expands canonical messages into journal events and adjacent durable
	/// labels.
	///
	/// The label targets the imported message's physical event index, preserving
	/// provenance for assistant and tool-result entries whose message schema has
	/// no attribution field.
	#[must_use]
	pub fn into_events(self, format: ForeignFormat, first_event_index: u64) -> Vec<Event> {
		let mut events = Vec::with_capacity(self.entries.len().saturating_mul(2));
		for entry in self.entries {
			let target = first_event_index.saturating_add(events.len() as u64);
			let ts = entry.event.ts;
			events.push(entry.event);
			let id = entry
				.source_id
				.as_deref()
				.map_or_else(|| entry.source_line.to_string(), ToOwned::to_owned);
			events.push(Event {
				ts,
				kind: Kind::Label { target, label: Some(sf!("imported:{}:{id}", format.marker())) },
			});
		}
		events
	}
}

/// Parses a Claude Code or Codex JSONL journal without allowing one damaged
/// physical record to discard later complete messages.
#[must_use]
pub fn parse_foreign_jsonl(format: ForeignFormat, input: &str) -> ImportedTranscript {
	let mut output = ImportedTranscript::default();
	for (offset, line) in input.lines().enumerate() {
		let source_line = offset as u64 + 1;
		if line.trim().is_empty() {
			continue;
		}
		let value = match serde_json::from_str::<Value>(line) {
			Ok(value) => value,
			Err(error) => {
				output.diagnostics.push(ImportDiagnostic {
					line:   source_line,
					code:   sf!("invalid_json"),
					reason: Str::new(error.to_string()),
				});
				continue;
			},
		};
		match parse_record(format, source_line, &value) {
			Ok(Some(entry)) => output.entries.push(entry),
			Ok(None) => {},
			Err(reason) => output.diagnostics.push(ImportDiagnostic {
				line: source_line,
				code: sf!("invalid_message"),
				reason,
			}),
		}
	}
	output
}

fn parse_record(
	format: ForeignFormat,
	source_line: u64,
	value: &Value,
) -> Result<Option<ImportedEntry>, Str> {
	let object = value
		.as_object()
		.ok_or_else(|| sf!("record is not an object"))?;
	let source_id = object
		.get("uuid")
		.or_else(|| object.get("id"))
		.and_then(Value::as_str)
		.map(Str::new);
	let ts = parse_timestamp(object.get("timestamp")).unwrap_or_default();
	let message = match format {
		ForeignFormat::ClaudeCode => parse_claude(value),
		ForeignFormat::Codex => parse_codex(value),
	};
	Ok(message.map(|message| ImportedEntry {
		source_line,
		source_id,
		event: Event { ts, kind: Kind::Msg(message) },
	}))
}

fn parse_claude(value: &Value) -> Option<Msg> {
	let kind = value
		.get("type")
		.and_then(Value::as_str)
		.unwrap_or_default();
	if !matches!(kind, "user" | "assistant") {
		return None;
	}
	let message = value.get("message").unwrap_or(value);
	let role = message.get("role").and_then(Value::as_str).unwrap_or(kind);
	if role == "assistant"
		&& let Some(parts) = message.get("content").and_then(Value::as_array)
	{
		let blocks = parts
			.iter()
			.filter_map(claude_assistant_block)
			.collect::<Vec<_>>();
		if !blocks.is_empty() {
			return Some(assistant_message(blocks, "anthropic", message.get("model")));
		}
	}
	parse_message(role, message.get("content"), "anthropic", message.get("model"))
}

fn parse_codex(value: &Value) -> Option<Msg> {
	let kind = value
		.get("type")
		.and_then(Value::as_str)
		.unwrap_or_default();
	let payload = value.get("payload").unwrap_or(value);
	if kind == "event_msg" {
		let event_kind = payload
			.get("type")
			.and_then(Value::as_str)
			.unwrap_or_default();
		let role = match event_kind {
			"user_message" => "user",
			"agent_message" => "assistant",
			_ => return None,
		};
		return parse_message(role, payload.get("message"), "openai", payload.get("model"));
	}
	if kind != "response_item" && kind != "message" {
		return None;
	}
	match payload
		.get("type")
		.and_then(Value::as_str)
		.unwrap_or("message")
	{
		"function_call" => {
			let id = payload
				.get("call_id")
				.or_else(|| payload.get("id"))
				.and_then(Value::as_str)
				.unwrap_or("imported-call");
			let name = payload
				.get("name")
				.and_then(Value::as_str)
				.unwrap_or("unknown");
			let args = payload.get("arguments").map_or_else(
				|| "{}".to_owned(),
				|value| {
					value
						.as_str()
						.map_or_else(|| value.to_string(), ToOwned::to_owned)
				},
			);
			return Some(assistant_message(
				vec![Block {
					kind: BlockKind::Tool {
						id:   CallId(Str::new(id)),
						name: Str::new(name),
						wire: None,
						args: Str::new(args),
					},
					re:   None,
				}],
				"openai",
				payload.get("model"),
			));
		},
		"function_call_output" => {
			let call = payload
				.get("call_id")
				.and_then(Value::as_str)
				.unwrap_or("imported-call");
			let output = payload.get("output").map_or_else(String::new, |value| {
				value
					.as_str()
					.map_or_else(|| value.to_string(), ToOwned::to_owned)
			});
			return Some(Msg::ToolResult {
				call:          CallId(Str::new(call)),
				tool:          sf!("unknown"),
				content:       vec![UserBlock::Text { text: Str::new(output) }],
				details:       None,
				error:         false,
				useless:       false,
				provider_meta: None,
			});
		},
		"message" => {},
		_ => return None,
	}
	let role = payload.get("role").and_then(Value::as_str)?;
	parse_message(role, payload.get("content"), "openai", payload.get("model"))
}

fn claude_assistant_block(part: &Value) -> Option<Block> {
	let object = part.as_object()?;
	let kind = match object.get("type").and_then(Value::as_str)? {
		"text" => BlockKind::Text { text: Str::new(object.get("text")?.as_str()?) },
		"thinking" => BlockKind::Think {
			text: Str::new(
				object
					.get("thinking")
					.or_else(|| object.get("text"))?
					.as_str()?,
			),
		},
		"tool_use" => {
			let input = object
				.get("input")
				.map_or_else(|| "{}".to_owned(), Value::to_string);
			BlockKind::Tool {
				id:   CallId(Str::new(object.get("id")?.as_str()?)),
				name: Str::new(object.get("name")?.as_str()?),
				wire: None,
				args: Str::new(input),
			}
		},
		_ => return None,
	};
	Some(Block { kind, re: None })
}

fn assistant_message(blocks: Vec<Block>, provider: &str, model: Option<&Value>) -> Msg {
	Msg::Assistant {
		content:     blocks,
		model:       ModelRef {
			provider: ProviderId(Str::new(provider)),
			api:      Str::new(provider),
			model:    ModelId(Str::new(model.and_then(Value::as_str).unwrap_or("imported"))),
		},
		stop:        Stop::EndTurn,
		usage:       Usage::default(),
		response_id: None,
		upstream:    None,
		ctx:         None,
		timing:      Timing::default(),
		disabled:    Vec::new(),
	}
}
fn parse_message(
	role: &str,
	content: Option<&Value>,
	provider: &'static str,
	model: Option<&Value>,
) -> Option<Msg> {
	let texts = extract_text(content);
	if texts.is_empty() {
		return None;
	}
	let imported = Some(Attribution { source: sf!("imported.{provider}"), id: None });
	match role {
		"user" => Some(Msg::User {
			content:     texts
				.into_iter()
				.map(|text| UserBlock::Text { text })
				.collect(),
			synthetic:   false,
			steering:    false,
			attribution: imported,
		}),
		"developer" | "system" => Some(Msg::Developer {
			content:     texts
				.into_iter()
				.map(|text| UserBlock::Text { text })
				.collect(),
			attribution: imported,
		}),
		"assistant" => Some(assistant_message(
			texts
				.into_iter()
				.map(|text| Block { kind: BlockKind::Text { text }, re: None })
				.collect(),
			provider,
			model,
		)),
		_ => None,
	}
}

fn extract_text(content: Option<&Value>) -> Vec<Str> {
	match content {
		Some(Value::String(text)) => vec![Str::new(text.as_str())],
		Some(Value::Array(parts)) => parts
			.iter()
			.flat_map(|part| {
				if let Some(text) = part.as_str() {
					return vec![Str::new(text)];
				}
				let Some(object) = part.as_object() else {
					return Vec::new();
				};
				let kind = object.get("type").and_then(Value::as_str).unwrap_or("text");
				if matches!(kind, "text" | "input_text" | "output_text") {
					return object
						.get("text")
						.and_then(Value::as_str)
						.map_or_else(Vec::new, |text| vec![Str::new(text)]);
				}
				if kind == "tool_result" {
					return extract_text(object.get("content"));
				}
				Vec::new()
			})
			.collect(),
		_ => Vec::new(),
	}
}

fn parse_timestamp(value: Option<&Value>) -> Option<u64> {
	match value? {
		Value::Number(number) => number.as_u64(),
		Value::String(value) => parse_rfc3339(value)
			.and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
			.and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok()),
		_ => None,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn claude_repairs_around_bad_lines_and_marks_provenance() {
		let input = concat!(
			r#"{"type":"user","uuid":"u1","timestamp":"2025-01-01T00:00:00Z","message":{"role":"user","content":"hello"}}"#,
			"\n{bad\n",
			r#"{"type":"assistant","uuid":"a1","message":{"role":"assistant","model":"claude-x","content":[{"type":"text","text":"hi"}]}}"#,
		);
		let parsed = parse_foreign_jsonl(ForeignFormat::ClaudeCode, input);
		assert_eq!(parsed.entries.len(), 2);
		assert_eq!(parsed.entries[1].source_line, 3);
		assert_eq!(parsed.diagnostics[0].code, "invalid_json");
		let events = parsed.into_events(ForeignFormat::ClaudeCode, 7);
		assert!(
			matches!(&events[1].kind, Kind::Label { target: 7, label: Some(label) } if label == "imported:claude_code:u1")
		);
	}

	#[test]
	fn codex_accepts_modern_response_items_and_legacy_events() {
		let input = concat!(
			r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"one"}]}}"#,
			"\n",
			r#"{"type":"event_msg","payload":{"type":"agent_message","message":"two"}}"#,
			"\n",
			r#"{"type":"response_item","payload":{"type":"function_call","call_id":"call-1","name":"read","arguments":"{\"path\":\"x\"}"}}"#,
		);
		let parsed = parse_foreign_jsonl(ForeignFormat::Codex, input);
		assert_eq!(parsed.entries.len(), 3);
		assert!(matches!(
			&parsed.entries[2].event.kind,
			Kind::Msg(Msg::Assistant { content, .. })
				if matches!(&content[0].kind, BlockKind::Tool { id, name, .. } if id.0 == "call-1" && name == "read")
		));
	}
}
