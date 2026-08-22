//! Bounded jq-like JSON subtree queries for internal resources.

use std::{fmt::Write as _, io};

use omp_core::{Str, StrMut};
use serde_json::Value;

/// Maximum accepted query text, in UTF-8 bytes.
pub const MAX_QUERY_BYTES: usize = 4 * 1024;
/// Maximum path components in one query.
pub const MAX_QUERY_DEPTH: usize = 64;
/// Maximum nesting allowed in an extracted JSON subtree.
pub const MAX_VALUE_DEPTH: usize = 128;

/// One parsed query component.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QueryToken {
	/// An object key.
	Key(Str),
	/// A zero-based array index.
	Index(usize),
}

/// A bounded JSON-query syntax or evaluation fault.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum QueryFault {
	/// Query text exceeds the parser ceiling.
	#[error("JSON query is {actual} bytes; the limit is {limit} bytes")]
	TooLong {
		/// Actual byte length.
		actual: usize,
		/// Accepted byte ceiling.
		limit:  usize,
	},
	/// Query contains too many path components.
	#[error("JSON query exceeds the maximum depth of {limit} components")]
	TooDeep {
		/// Accepted component ceiling.
		limit: usize,
	},
	/// A bracket was not closed.
	#[error("invalid JSON query at byte {offset}: missing closing ']' for bracket")]
	MissingBracket {
		/// Byte offset of the opening bracket.
		offset: usize,
	},
	/// A bracket contained no key or index.
	#[error("invalid JSON query at byte {offset}: empty brackets are not supported")]
	EmptyBracket {
		/// Byte offset of the opening bracket.
		offset: usize,
	},
	/// A quoted bracket key was not terminated.
	#[error("invalid JSON query at byte {offset}: unterminated quoted key")]
	UnterminatedQuote {
		/// Byte offset of the opening quote.
		offset: usize,
	},
	/// A quoted key contains an invalid escape.
	#[error(
		"invalid JSON query at byte {offset}: unsupported escape; use \\, \", \\' or a JSON escape"
	)]
	InvalidEscape {
		/// Byte offset of the backslash.
		offset: usize,
	},
	/// A token cannot begin at the reported byte.
	#[error(
		"invalid JSON query at byte {offset}: unexpected character {character:?}; expected '.', \
		 '[', or a key"
	)]
	Unexpected {
		/// Byte offset of the character.
		offset:    usize,
		/// Unexpected character.
		character: char,
	},
	/// An array index cannot fit in `usize`.
	#[error("invalid JSON query at byte {offset}: array index is too large")]
	IndexOverflow {
		/// Byte offset of the index.
		offset: usize,
	},
	/// Evaluation expected an object.
	#[error(
		"JSON query component {depth} selects key {key:?}, but the current value is not an object"
	)]
	ExpectedObject {
		/// One-based component number.
		depth: usize,
		/// Requested key.
		key:   Str,
	},
	/// Evaluation expected an array.
	#[error(
		"JSON query component {depth} selects index {index}, but the current value is not an array"
	)]
	ExpectedArray {
		/// One-based component number.
		depth: usize,
		/// Requested index.
		index: usize,
	},
	/// An object does not contain a requested key.
	#[error("JSON query component {depth}: object has no key {key:?}")]
	MissingKey {
		/// One-based component number.
		depth: usize,
		/// Requested key.
		key:   Str,
	},
	/// An array does not contain a requested index.
	#[error("JSON query component {depth}: index {index} is out of bounds for array length {len}")]
	IndexOutOfBounds {
		/// One-based component number.
		depth: usize,
		/// Requested index.
		index: usize,
		/// Array length.
		len:   usize,
	},
	/// The selected subtree is nested too deeply to render safely.
	#[error("selected JSON subtree exceeds the maximum depth of {limit}")]
	ValueTooDeep {
		/// Accepted nesting ceiling.
		limit: usize,
	},
	/// The rendered subtree exceeds the caller's byte ceiling.
	#[error("selected JSON subtree exceeds the {limit}-byte output limit")]
	OutputTooLarge {
		/// Accepted output ceiling.
		limit: usize,
	},
	/// Percent-decoding a path component failed.
	#[error("invalid percent escape in JSON path segment at byte {offset}")]
	InvalidPercentEscape {
		/// Byte offset in the complete path.
		offset: usize,
	},
	/// JSON serialization failed.
	#[error("selected JSON subtree could not be rendered")]
	Render,
}

/// Parse bounded dot/bracket query syntax.
pub fn parse_query(query: &str) -> Result<Vec<QueryToken>, QueryFault> {
	let input = query.trim();
	if input.len() > MAX_QUERY_BYTES {
		return Err(QueryFault::TooLong { actual: input.len(), limit: MAX_QUERY_BYTES });
	}
	let mut index = usize::from(input.starts_with('.'));
	let mut tokens = Vec::new();
	while index < input.len() {
		let byte = input.as_bytes()[index];
		if byte == b'.' {
			index += 1;
			if index == input.len() || matches!(input.as_bytes()[index], b'.' | b']') {
				return Err(QueryFault::Unexpected { offset: index - 1, character: '.' });
			}
			continue;
		}
		if byte == b'[' {
			let (token, next) = parse_bracket(input, index)?;
			push_token(&mut tokens, token)?;
			index = next;
			continue;
		}
		let start = index;
		while index < input.len() && is_key_byte(input.as_bytes()[index]) {
			index += 1;
		}
		if start == index {
			let character = input[index..].chars().next().expect("index is in bounds");
			return Err(QueryFault::Unexpected { offset: index, character });
		}
		push_token(&mut tokens, QueryToken::Key(Str::new(&input[start..index])))?;
	}
	Ok(tokens)
}

fn push_token(tokens: &mut Vec<QueryToken>, token: QueryToken) -> Result<(), QueryFault> {
	if tokens.len() == MAX_QUERY_DEPTH {
		return Err(QueryFault::TooDeep { limit: MAX_QUERY_DEPTH });
	}
	tokens.push(token);
	Ok(())
}

const fn is_key_byte(byte: u8) -> bool {
	byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
}

fn parse_bracket(input: &str, open: usize) -> Result<(QueryToken, usize), QueryFault> {
	let mut index = open + 1;
	while input.as_bytes().get(index) == Some(&b' ') {
		index += 1;
	}
	let Some(&first) = input.as_bytes().get(index) else {
		return Err(QueryFault::MissingBracket { offset: open });
	};
	if matches!(first, b'\'' | b'"') {
		return parse_quoted_bracket(input, index, first);
	}
	let close = input[index..]
		.find(']')
		.map(|relative| index + relative)
		.ok_or(QueryFault::MissingBracket { offset: open })?;
	let raw = input[index..close].trim();
	if raw.is_empty() {
		return Err(QueryFault::EmptyBracket { offset: open });
	}
	let token = if raw.bytes().all(|byte| byte.is_ascii_digit()) {
		QueryToken::Index(
			raw.parse()
				.map_err(|_| QueryFault::IndexOverflow { offset: index })?,
		)
	} else if raw.bytes().all(is_key_byte) {
		QueryToken::Key(Str::new(raw))
	} else {
		let character = raw
			.chars()
			.find(|character| !character.is_ascii_alphanumeric() && !matches!(character, '_' | '-'))
			.unwrap_or('?');
		return Err(QueryFault::Unexpected { offset: index, character });
	};
	Ok((token, close + 1))
}

fn parse_quoted_bracket(
	input: &str,
	quote_at: usize,
	quote: u8,
) -> Result<(QueryToken, usize), QueryFault> {
	let mut index = quote_at + 1;
	let mut key = StrMut::new("");
	while index < input.len() {
		let byte = input.as_bytes()[index];
		if byte == quote {
			let mut close = index + 1;
			while input.as_bytes().get(close) == Some(&b' ') {
				close += 1;
			}
			if input.as_bytes().get(close) != Some(&b']') {
				let character = input[close..].chars().next().unwrap_or(']');
				return Err(QueryFault::Unexpected { offset: close, character });
			}
			return Ok((QueryToken::Key(key.freeze()), close + 1));
		}
		if byte == b'\\' {
			let escape_at = index;
			index += 1;
			let Some(&escaped) = input.as_bytes().get(index) else {
				return Err(QueryFault::UnterminatedQuote { offset: quote_at });
			};
			match escaped {
				b'\\' => key.push('\\'),
				b'\'' if quote == b'\'' => key.push('\''),
				b'"' if quote == b'"' => key.push('"'),
				b'n' => key.push('\n'),
				b'r' => key.push('\r'),
				b't' => key.push('\t'),
				b'b' => key.push('\u{0008}'),
				b'f' => key.push('\u{000c}'),
				_ => return Err(QueryFault::InvalidEscape { offset: escape_at }),
			}
			index += 1;
			continue;
		}
		let character = input[index..].chars().next().expect("index is in bounds");
		key.push(character);
		index += character.len_utf8();
	}
	Err(QueryFault::UnterminatedQuote { offset: quote_at })
}

/// Apply a parsed query and return the selected borrowed subtree.
pub fn apply_query<'a>(value: &'a Value, tokens: &[QueryToken]) -> Result<&'a Value, QueryFault> {
	let mut current = value;
	for (offset, token) in tokens.iter().enumerate() {
		let depth = offset + 1;
		current = match token {
			QueryToken::Key(key) => current
				.as_object()
				.ok_or_else(|| QueryFault::ExpectedObject { depth, key: key.clone() })?
				.get(key.as_str())
				.ok_or_else(|| QueryFault::MissingKey { depth, key: key.clone() })?,
			QueryToken::Index(index) => {
				let values = current
					.as_array()
					.ok_or(QueryFault::ExpectedArray { depth, index: *index })?;
				values.get(*index).ok_or(QueryFault::IndexOutOfBounds {
					depth,
					index: *index,
					len: values.len(),
				})?
			},
		};
	}
	Ok(current)
}

/// Translate `/path/0/quoted%20key` into canonical dot/bracket query syntax.
pub fn path_to_query(path: &str) -> Result<Str, QueryFault> {
	if path.len() > MAX_QUERY_BYTES {
		return Err(QueryFault::TooLong { actual: path.len(), limit: MAX_QUERY_BYTES });
	}
	if path.is_empty() || path == "/" {
		return Ok(Str::new(""));
	}
	let mut query = StrMut::with_capacity(path.len());
	let mut path_offset = 0;
	let mut depth = 0;
	for segment in path.split('/') {
		let segment_offset = path_offset;
		path_offset += segment.len() + 1;
		if segment.is_empty() {
			continue;
		}
		if depth == MAX_QUERY_DEPTH {
			return Err(QueryFault::TooDeep { limit: MAX_QUERY_DEPTH });
		}
		depth += 1;
		let decoded = percent_decode(segment, segment_offset)?;
		if decoded.bytes().all(|byte| byte.is_ascii_digit()) {
			let _ = write!(query, "[{decoded}]");
		} else if decoded.bytes().all(is_key_byte) {
			query.push('.');
			query.push_str(&decoded);
		} else {
			query.push_str("['");
			for character in decoded.chars() {
				if matches!(character, '\\' | '\'') {
					query.push('\\');
				}
				query.push(character);
			}
			query.push_str("']");
		}
	}
	Ok(query.freeze())
}

fn percent_decode(segment: &str, base_offset: usize) -> Result<String, QueryFault> {
	let mut decoded = Vec::with_capacity(segment.len());
	let mut index = 0;
	while index < segment.len() {
		if segment.as_bytes()[index] != b'%' {
			decoded.push(segment.as_bytes()[index]);
			index += 1;
			continue;
		}
		let Some(hex) = segment.as_bytes().get(index + 1..index + 3) else {
			return Err(QueryFault::InvalidPercentEscape { offset: base_offset + index });
		};
		let high = hex_value(hex[0])
			.ok_or(QueryFault::InvalidPercentEscape { offset: base_offset + index })?;
		let low = hex_value(hex[1])
			.ok_or(QueryFault::InvalidPercentEscape { offset: base_offset + index })?;
		decoded.push((high << 4) | low);
		index += 3;
	}
	String::from_utf8(decoded).map_err(|error| QueryFault::InvalidPercentEscape {
		offset: base_offset + error.utf8_error().valid_up_to(),
	})
}

const fn hex_value(byte: u8) -> Option<u8> {
	match byte {
		b'0'..=b'9' => Some(byte - b'0'),
		b'a'..=b'f' => Some(byte - b'a' + 10),
		b'A'..=b'F' => Some(byte - b'A' + 10),
		_ => None,
	}
}

/// Render a selected subtree, emitting JSON primitives without string quotes.
pub fn render_value(value: &Value, max_bytes: usize) -> Result<Str, QueryFault> {
	ensure_value_depth(value, 0)?;
	let rendered = match value {
		Value::Null => Str::new_static("null"),
		Value::Bool(value) => Str::new_static(if *value { "true" } else { "false" }),
		Value::Number(value) => Str::new(value.to_string()),
		Value::String(value) => Str::new(value),
		Value::Array(_) | Value::Object(_) => {
			let mut output = BoundedJson::new(max_bytes);
			if serde_json::to_writer_pretty(&mut output, value).is_err() {
				return Err(if output.overflow {
					QueryFault::OutputTooLarge { limit: max_bytes }
				} else {
					QueryFault::Render
				});
			}
			Str::new(String::from_utf8(output.bytes).map_err(|_| QueryFault::Render)?)
		},
	};
	if rendered.len() > max_bytes {
		return Err(QueryFault::OutputTooLarge { limit: max_bytes });
	}
	Ok(rendered)
}

struct BoundedJson {
	bytes:    Vec<u8>,
	limit:    usize,
	overflow: bool,
}

impl BoundedJson {
	fn new(limit: usize) -> Self {
		Self { bytes: Vec::with_capacity(limit.min(4_096)), limit, overflow: false }
	}
}

impl io::Write for BoundedJson {
	fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
		if self.bytes.len().saturating_add(bytes.len()) > self.limit {
			self.overflow = true;
			return Err(io::Error::new(io::ErrorKind::FileTooLarge, "JSON output limit exceeded"));
		}
		self.bytes.extend_from_slice(bytes);
		Ok(bytes.len())
	}

	fn flush(&mut self) -> io::Result<()> {
		Ok(())
	}
}

fn ensure_value_depth(value: &Value, depth: usize) -> Result<(), QueryFault> {
	if depth > MAX_VALUE_DEPTH {
		return Err(QueryFault::ValueTooDeep { limit: MAX_VALUE_DEPTH });
	}
	match value {
		Value::Array(values) => {
			for value in values {
				ensure_value_depth(value, depth + 1)?;
			}
		},
		Value::Object(values) => {
			for value in values.values() {
				ensure_value_depth(value, depth + 1)?;
			}
		},
		Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {},
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use serde_json::json;

	use super::*;

	#[test]
	fn parses_pi_dot_bracket_and_quoted_key_table() {
		let cases = [
			(".foo.bar[0]", vec![
				QueryToken::Key("foo".into()),
				QueryToken::Key("bar".into()),
				QueryToken::Index(0),
			]),
			("foo['special-key']", vec![
				QueryToken::Key("foo".into()),
				QueryToken::Key("special-key".into()),
			]),
			("[\"space key\"]", vec![QueryToken::Key("space key".into())]),
			("[bare-key]", vec![QueryToken::Key("bare-key".into())]),
		];
		for (input, expected) in cases {
			assert_eq!(parse_query(input).unwrap(), expected, "{input}");
		}
	}

	#[test]
	fn translates_paths_and_extracts_subtrees() {
		let query = path_to_query("/foo/bar/0/special%20key").unwrap();
		assert_eq!(query, ".foo.bar[0]['special key']");
		let value = json!({"foo": {"bar": [{"special key": "answer"}]}});
		let tokens = parse_query(&query).unwrap();
		assert_eq!(render_value(apply_query(&value, &tokens).unwrap(), 32).unwrap(), "answer");
	}

	#[test]
	fn reports_actionable_faults_and_depth_bounds() {
		assert!(matches!(parse_query(".foo["), Err(QueryFault::MissingBracket { offset: 4 })));
		assert!(matches!(parse_query(".foo[]"), Err(QueryFault::EmptyBracket { offset: 4 })));
		let too_deep = ".x".repeat(MAX_QUERY_DEPTH + 1);
		assert!(matches!(parse_query(&too_deep), Err(QueryFault::TooDeep { .. })));
		let value = json!({"items": [1]});
		let tokens = parse_query(".items[2]").unwrap();
		assert!(matches!(
			apply_query(&value, &tokens),
			Err(QueryFault::IndexOutOfBounds { index: 2, len: 1, .. })
		));
	}

	#[test]
	fn rejects_excessive_value_depth_and_output() {
		let mut value = Value::Null;
		for _ in 0..=MAX_VALUE_DEPTH {
			value = Value::Array(vec![value]);
		}
		assert!(matches!(render_value(&value, usize::MAX), Err(QueryFault::ValueTooDeep { .. })));
		assert!(matches!(
			render_value(&json!({"large": "payload"}), 4),
			Err(QueryFault::OutputTooLarge { limit: 4 })
		));
	}
}
