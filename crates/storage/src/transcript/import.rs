//! Post-hoc repair parsers for foreign append-only session transcripts.
//!
//! Foreign records are deliberately parsed line-by-line: damaged records become
//! diagnostics while later complete records retain their source line identity.

use std::{
	fs::File,
	io::{BufRead as _, BufReader},
	path::{Path, PathBuf},
};

use omp_core::{Str, sf, time::parse_rfc3339};
use serde_json::Value;
use strum::IntoStaticStr;
use thiserror::Error;

use super::{
	Attribution, Block, BlockKind, CallId, Event, Header, Kind, ModelId, ModelRef, Msg, ProviderId,
	Stop, Timing, TitleSource, Usage, UserBlock, Writer, load, writer::JournalError,
};

/// Supported foreign transcript dialects.
#[derive(Clone, Copy, Debug, Eq, PartialEq, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum ForeignFormat {
	/// Claude Code's JSONL session log.
	ClaudeCode,
	/// Codex CLI's rollout JSONL log.
	Codex,
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
		let marker: &'static str = format.into();
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
				kind: Kind::Label { target, label: Some(sf!("imported:{marker}:{id}")) },
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

/// Durable audit facts from one pi v1/v2 migration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PiMigrationRecord {
	/// Source schema revision; absent headers are v1.
	pub source_version:  u32,
	/// Exact source path retained after migration.
	pub source_path:     PathBuf,
	/// Exact number of source bytes scanned.
	pub source_bytes:    u64,
	/// Unsupported legacy fields reported and omitted.
	pub dropped_fields:  Vec<Str>,
	/// Number of canonical v4 message/compaction events produced.
	pub imported_events: u64,
}

/// Completed pi legacy import.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PiImportReport {
	/// Durable migration audit facts.
	pub migration:   PiMigrationRecord,
	/// Per-line malformed/unsupported diagnostics.
	pub diagnostics: Vec<ImportDiagnostic>,
}

/// Failure from a pi v1/v2 to v4 file migration.
#[derive(Debug, Error)]
pub enum PiImportError {
	/// Source or destination filesystem operation failed.
	#[error("pi session migration filesystem operation failed")]
	Io(#[from] std::io::Error),
	/// The source declared a schema newer than v2.
	#[error("unsupported pi session version {0}; importer accepts v1 and v2")]
	UnsupportedVersion(u32),
	/// Transcript encoding or validation failed.
	#[error(transparent)]
	Transcript(#[from] super::codec::Error),
	/// Atomic v4 journal publication failed.
	#[error(transparent)]
	Journal(#[from] JournalError),
}

/// Streams a pi v1/v2 JSONL source into a new validated v4 journal.
///
/// The source is never modified or removed. The destination is lazily
/// materialized as one atomic header-plus-migration/event group.
pub fn import_pi_file(
	source: &Path,
	destination: &Path,
	header: &Header,
) -> Result<PiImportReport, PiImportError> {
	let file = File::open(source)?;
	let mut reader = BufReader::new(file);
	let mut line = Vec::new();
	let mut events = Vec::new();
	let mut diagnostics = Vec::new();
	let mut dropped_fields = Vec::<Str>::new();
	let mut source_bytes = 0_u64;
	let mut source_version = 1_u32;
	let mut source_line = 0_u64;

	loop {
		line.clear();
		let read = reader.read_until(b'\n', &mut line)?;
		if read == 0 {
			break;
		}
		source_line = source_line.saturating_add(1);
		source_bytes =
			source_bytes.saturating_add(u64::try_from(read).expect("source line fits in u64"));
		if line.last() == Some(&b'\n') {
			line.pop();
		}
		if line.last() == Some(&b'\r') {
			line.pop();
		}
		if line.is_empty() {
			continue;
		}
		let value = match serde_json::from_slice::<Value>(&line) {
			Ok(value) => value,
			Err(error) => {
				diagnostics.push(ImportDiagnostic {
					line:   source_line,
					code:   sf!("invalid_json"),
					reason: Str::new(error.to_string()),
				});
				continue;
			},
		};
		let Some(object) = value.as_object() else {
			diagnostics.push(ImportDiagnostic {
				line:   source_line,
				code:   sf!("invalid_record"),
				reason: sf!("record is not an object"),
			});
			continue;
		};
		let record_type = object
			.get("type")
			.and_then(Value::as_str)
			.unwrap_or_default();
		if record_type == "session" {
			source_version = object
				.get("version")
				.and_then(Value::as_u64)
				.map_or(1, |version| u32::try_from(version).unwrap_or(u32::MAX));
			if source_version > 2 {
				return Err(PiImportError::UnsupportedVersion(source_version));
			}
			record_dropped_fields(
				object.keys().map(String::as_str),
				&["type", "version", "id", "timestamp", "cwd"],
				source_line,
				&mut dropped_fields,
				&mut diagnostics,
			);
			continue;
		}

		let ts = parse_timestamp(object.get("timestamp")).unwrap_or_default();
		let kind = match record_type {
			"message" => parse_pi_message(object.get("message")).map(Kind::Msg),
			"compaction" => {
				let summary = object
					.get("summary")
					.and_then(Value::as_str)
					.unwrap_or_default();
				let first_kept = object
					.get("firstKeptEntryIndex")
					.or_else(|| object.get("firstKeptEntryId"))
					.and_then(Value::as_u64)
					.unwrap_or_default();
				Some(Kind::Compact {
					summary: Str::new(summary),
					short: None,
					first_kept,
					tokens_before: object
						.get("tokensBefore")
						.and_then(Value::as_u64)
						.unwrap_or_default(),
					tokens_after: object.get("tokensAfter").and_then(Value::as_u64),
					method: Some(sf!("pi_legacy")),
					warning: None,
					superseded: Vec::new(),
					snapcompact: None,
				})
			},
			"title" => object
				.get("title")
				.and_then(Value::as_str)
				.map(|title| Kind::Title { title: Str::new(title), source: TitleSource::Imported }),
			_ => None,
		};
		if let Some(kind) = kind {
			let target = u64::try_from(events.len()).expect("import event count fits in u64");
			events.push(Event { ts, kind });
			events.push(Event {
				ts,
				kind: Kind::Label {
					target,
					label: Some(sf!("imported:pi:v{source_version}:line:{source_line}")),
				},
			});
		} else {
			diagnostics.push(ImportDiagnostic {
				line:   source_line,
				code:   sf!("unsupported_record"),
				reason: Str::new(record_type),
			});
		}
	}

	let imported_events = u64::try_from(events.len() / 2).expect("import count fits in u64");
	let migration_label = sf!(
		"migration:pi:v{source_version}:bytes:{source_bytes}:dropped:{}:source:{}",
		dropped_fields.len(),
		source.to_string_lossy()
	);
	events
		.insert(0, Event { ts: 0, kind: Kind::Label { target: 0, label: Some(migration_label) } });
	for event in &mut events[1..] {
		if let Kind::Label { target, .. } = &mut event.kind {
			*target = target.saturating_add(1);
		}
	}

	let mut writer = Writer::create_lazy(destination, header)?;
	writer.append_atomic(&events)?;
	drop(writer);
	load(destination)?;
	Ok(PiImportReport {
		migration: PiMigrationRecord {
			source_version,
			source_path: source.to_owned(),
			source_bytes,
			dropped_fields,
			imported_events,
		},
		diagnostics,
	})
}

fn record_dropped_fields<'a>(
	fields: impl IntoIterator<Item = &'a str>,
	supported: &[&str],
	line: u64,
	dropped: &mut Vec<Str>,
	diagnostics: &mut Vec<ImportDiagnostic>,
) {
	for field in fields {
		if supported.contains(&field) || dropped.iter().any(|known| known == field) {
			continue;
		}
		let field = Str::new(field);
		dropped.push(field.clone());
		diagnostics.push(ImportDiagnostic { line, code: sf!("unsupported_field"), reason: field });
	}
}

fn parse_pi_message(value: Option<&Value>) -> Option<Msg> {
	let message = value?.as_object()?;
	let role = message.get("role").and_then(Value::as_str)?;
	if role == "toolResult" {
		let call = message
			.get("toolCallId")
			.and_then(Value::as_str)
			.unwrap_or("imported-call");
		let tool = message
			.get("toolName")
			.and_then(Value::as_str)
			.unwrap_or("unknown");
		return Some(Msg::ToolResult {
			call:          CallId(Str::new(call)),
			tool:          Str::new(tool),
			content:       extract_text(message.get("content"))
				.into_iter()
				.map(|text| UserBlock::Text { text })
				.collect(),
			details:       None,
			error:         message
				.get("isError")
				.and_then(Value::as_bool)
				.unwrap_or(false),
			useless:       false,
			provider_meta: None,
		});
	}
	if role == "assistant"
		&& let Some(parts) = message.get("content").and_then(Value::as_array)
	{
		let blocks = parts
			.iter()
			.filter_map(|part| {
				let object = part.as_object()?;
				match object.get("type").and_then(Value::as_str)? {
					"text" => Some(Block {
						kind: BlockKind::Text { text: Str::new(object.get("text")?.as_str()?) },
						re:   None,
					}),
					"thinking" => Some(Block {
						kind: BlockKind::Think {
							text: Str::new(
								object
									.get("thinking")
									.or_else(|| object.get("text"))?
									.as_str()?,
							),
						},
						re:   None,
					}),
					"toolCall" => Some(Block {
						kind: BlockKind::Tool {
							id:   CallId(Str::new(object.get("id")?.as_str()?)),
							name: Str::new(object.get("name")?.as_str()?),
							wire: None,
							args: Str::new(
								object
									.get("arguments")
									.map_or_else(|| "{}".to_owned(), Value::to_string),
							),
						},
						re:   None,
					}),
					_ => None,
				}
			})
			.collect::<Vec<_>>();
		if !blocks.is_empty() {
			return Some(assistant_message(
				blocks,
				message
					.get("provider")
					.and_then(Value::as_str)
					.unwrap_or("imported"),
				message.get("model"),
			));
		}
	}
	parse_message(role, message.get("content"), "imported", message.get("model"))
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
