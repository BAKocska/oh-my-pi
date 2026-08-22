//! Bounded model projections for debugger snapshots.

use std::fmt::Write as _;

use omp_core::{Str, encoding::base64};
use serde_json::Value;
use xutf::{Encoding as _, Utf8};

use crate::{debug::Action, render::truncate::truncate_head_bytes};

const MAX_ROWS: usize = 100;
const MAX_OUTPUT_BYTES: usize = 32 * 1024;

/// Formats one structured debug result with stable bounds.
pub fn render(action: Action, data: &Value) -> Str {
	match action {
		Action::Sessions => sessions(data),
		Action::StackTrace => stack(data),
		Action::Scopes | Action::Variables => variables(data),
		Action::ReadMemory => memory(data),
		Action::Disassemble => disassembly(data),
		Action::Output => output(data),
		Action::Continue | Action::Pause | Action::StepOver | Action::StepIn | Action::StepOut => {
			stop(data)
		},
		_ => structured(data),
	}
}

fn sessions(data: &Value) -> Str {
	let rows = data.as_array().map(Vec::as_slice).unwrap_or_default();
	let mut text = String::from("SESSION\tADAPTER\tSTATE\tREVISION\n");
	for row in rows.iter().take(MAX_ROWS) {
		let _ = writeln!(
			text,
			"{}\t{}\t{}\t{}",
			string(row, "id"),
			string(row, "adapter"),
			string(row, "state"),
			row.get("revision")
				.and_then(Value::as_u64)
				.unwrap_or_default(),
		);
	}
	truncation(&mut text, rows.len());
	Str::from(text)
}

fn stop(data: &Value) -> Str {
	let mut text = String::new();
	let _ = writeln!(
		text,
		"{}: {} (thread {})",
		string(data, "state"),
		string(data, "reason"),
		data
			.get("thread_id")
			.and_then(Value::as_i64)
			.unwrap_or_default(),
	);
	if let Some(frame) = data.get("frame") {
		let source = frame
			.get("source")
			.and_then(|source| source.get("path"))
			.and_then(Value::as_str)
			.unwrap_or("<unknown>");
		let _ = writeln!(
			text,
			"#{} {} at {}:{}:{}",
			frame.get("id").and_then(Value::as_i64).unwrap_or_default(),
			string(frame, "name"),
			source,
			frame
				.get("line")
				.and_then(Value::as_u64)
				.unwrap_or_default(),
			frame
				.get("column")
				.and_then(Value::as_u64)
				.unwrap_or_default(),
		);
	}
	Str::from(text)
}

fn stack(data: &Value) -> Str {
	let rows = data
		.get("stackFrames")
		.and_then(Value::as_array)
		.map(Vec::as_slice)
		.unwrap_or_default();
	let mut text = String::from("FRAME\tNAME\tSOURCE\tLINE:COLUMN\n");
	for frame in rows.iter().take(MAX_ROWS) {
		let source = frame
			.get("source")
			.and_then(|source| source.get("path"))
			.and_then(Value::as_str)
			.unwrap_or("<unknown>");
		let _ = writeln!(
			text,
			"{}\t{}\t{}\t{}:{}",
			frame.get("id").and_then(Value::as_i64).unwrap_or_default(),
			string(frame, "name"),
			source,
			frame
				.get("line")
				.and_then(Value::as_u64)
				.unwrap_or_default(),
			frame
				.get("column")
				.and_then(Value::as_u64)
				.unwrap_or_default(),
		);
	}
	truncation(&mut text, rows.len());
	Str::from(text)
}

fn variables(data: &Value) -> Str {
	let rows = data
		.get("variables")
		.or_else(|| data.get("scopes"))
		.and_then(Value::as_array)
		.map(Vec::as_slice)
		.unwrap_or_default();
	let mut text = String::from("NAME\tTYPE\tVALUE\tREFERENCE\n");
	for row in rows.iter().take(MAX_ROWS) {
		let value = row.get("value").and_then(Value::as_str).unwrap_or_default();
		let _ = writeln!(
			text,
			"{}\t{}\t{}\t{}",
			string(row, "name"),
			string(row, "type"),
			value.replace(['\r', '\n', '\t'], " "),
			row.get("variablesReference")
				.and_then(Value::as_i64)
				.unwrap_or_default(),
		);
	}
	truncation(&mut text, rows.len());
	Str::from(text)
}

fn memory(data: &Value) -> Str {
	let address = data.get("address").and_then(Value::as_str).unwrap_or("0");
	let encoded = data.get("data").and_then(Value::as_str).unwrap_or_default();
	let Ok(bytes) = base64::decode(encoded).into_vec() else {
		return Str::from("memory response contained invalid base64");
	};
	let mut text = String::new();
	for (line, chunk) in bytes.chunks(16).take(MAX_ROWS).enumerate() {
		let _ = write!(text, "{}+{:04x}  ", address, line * 16);
		for byte in chunk {
			let _ = write!(text, "{byte:02x} ");
		}
		for _ in chunk.len()..16 {
			text.push_str("   ");
		}
		text.push(' ');
		for byte in chunk {
			text.push(if byte.is_ascii_graphic() || *byte == b' ' {
				char::from(*byte)
			} else {
				'.'
			});
		}
		text.push('\n');
	}
	if bytes.len() > MAX_ROWS * 16 {
		let _ = writeln!(text, "... {} bytes omitted", bytes.len() - MAX_ROWS * 16);
	}
	Str::from(text)
}

fn disassembly(data: &Value) -> Str {
	let rows = data
		.get("instructions")
		.and_then(Value::as_array)
		.map(Vec::as_slice)
		.unwrap_or_default();
	let mut text = String::from("ADDRESS\tBYTES\tINSTRUCTION\tSOURCE\n");
	for row in rows.iter().take(MAX_ROWS) {
		let source = row
			.get("location")
			.and_then(|location| location.get("path"))
			.and_then(Value::as_str)
			.unwrap_or_default();
		let _ = writeln!(
			text,
			"{}\t{}\t{}\t{}:{}",
			string(row, "address"),
			string(row, "instructionBytes"),
			string(row, "instruction"),
			source,
			row.get("line").and_then(Value::as_u64).unwrap_or_default(),
		);
	}
	truncation(&mut text, rows.len());
	Str::from(text)
}

fn output(data: &Value) -> Str {
	let value = data
		.get("output")
		.and_then(Value::as_str)
		.unwrap_or_else(|| data.as_str().unwrap_or_default());
	if value.len() <= MAX_OUTPUT_BYTES {
		return Str::new(value);
	}
	let start = value.len() - MAX_OUTPUT_BYTES;
	let start = {
		let mut remaining = value.as_bytes();
		let mut boundary = 0;
		while boundary < start {
			Utf8::decode(&mut remaining);
			boundary = value.len() - remaining.len();
		}
		boundary
	};
	Str::from(format!("[older output omitted]\n{}", &value[start..]))
}

fn structured(data: &Value) -> Str {
	let mut text = serde_json::to_string_pretty(data).unwrap_or_default();
	if text.len() > MAX_OUTPUT_BYTES {
		text = truncate_head_bytes(&text, MAX_OUTPUT_BYTES).text.to_owned();
		text.push_str("\n... response truncated");
	}
	Str::from(text)
}

fn string<'a>(value: &'a Value, field: &str) -> &'a str {
	value.get(field).and_then(Value::as_str).unwrap_or_default()
}

fn truncation(text: &mut String, count: usize) {
	if count > MAX_ROWS {
		let _ = writeln!(text, "... {} rows omitted", count - MAX_ROWS);
	}
}
