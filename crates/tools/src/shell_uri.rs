//! Quote-aware discovery and replacement of path-backed internal resource URIs.

use std::collections::BTreeMap;

use omp_core::Str;

/// Internal URI occurrence and its shell quoting context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Occurrence {
	/// Byte range in the original value.
	pub range: std::ops::Range<usize>,
	/// Exact registered URI spelling.
	pub uri:   Str,
	/// Quoting context at the start of the URI.
	pub quote: QuoteContext,
}

/// Shell context surrounding an internal URI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuoteContext {
	/// Unquoted shell text.
	Unquoted,
	/// Inside single quotes; substitutions are intentionally not expanded.
	Single,
	/// Inside double quotes.
	Double,
	/// Inside command substitution.
	Substitution,
}

/// Returns path-backed internal URIs in command order.
///
/// Single-quoted occurrences are retained for diagnostics but callers must not
/// materialize them because shell single quotes suppress substitution.
pub fn scan(input: &str) -> Vec<Occurrence> {
	let bytes = input.as_bytes();
	let mut found = Vec::new();
	let mut cursor = 0;
	let mut quote = QuoteContext::Unquoted;
	let mut substitution_depth = 0usize;
	while cursor < bytes.len() {
		match bytes[cursor] {
			b'\\' if quote != QuoteContext::Single => {
				cursor = (cursor + 2).min(bytes.len());
				continue;
			},
			b'\'' if quote != QuoteContext::Double => {
				quote = if quote == QuoteContext::Single {
					QuoteContext::Unquoted
				} else {
					QuoteContext::Single
				};
				cursor += 1;
				continue;
			},
			b'"' if quote != QuoteContext::Single => {
				quote = if quote == QuoteContext::Double {
					QuoteContext::Unquoted
				} else {
					QuoteContext::Double
				};
				cursor += 1;
				continue;
			},
			b'$' if quote != QuoteContext::Single && bytes.get(cursor + 1) == Some(&b'(') => {
				substitution_depth += 1;
				cursor += 2;
				continue;
			},
			b')' if substitution_depth != 0 && quote != QuoteContext::Single => {
				substitution_depth -= 1;
				cursor += 1;
				continue;
			},
			_ => {},
		}
		let tail = &input[cursor..];
		let Some(prefix) = PATH_SCHEMES
			.iter()
			.find(|prefix| tail.starts_with(**prefix))
		else {
			cursor += input[cursor..].chars().next().map_or(1, char::len_utf8);
			continue;
		};
		let end = uri_end(input, cursor + prefix.len(), quote);
		if end > cursor + prefix.len() {
			found.push(Occurrence {
				range: cursor..end,
				uri:   Str::new(&input[cursor..end]),
				quote: if substitution_depth != 0 && quote == QuoteContext::Unquoted {
					QuoteContext::Substitution
				} else {
					quote
				},
			});
		}
		cursor = end.max(cursor + prefix.len());
	}
	found
}

/// Replaces materialized URIs using shell-safe spelling for their quote
/// context. Missing mappings and single-quoted occurrences are left unchanged.
pub fn replace(input: &str, paths: &BTreeMap<Str, Str>) -> Str {
	let occurrences = scan(input);
	let mut output = String::with_capacity(input.len());
	let mut copied = 0;
	for occurrence in occurrences {
		let Some(path) = paths.get(&occurrence.uri) else {
			continue;
		};
		if occurrence.quote == QuoteContext::Single {
			continue;
		}
		output.push_str(&input[copied..occurrence.range.start]);
		match occurrence.quote {
			QuoteContext::Double => push_double_escaped(&mut output, path),
			QuoteContext::Unquoted | QuoteContext::Substitution => {
				push_single_quoted(&mut output, path)
			},
			QuoteContext::Single => unreachable!("single quotes are skipped"),
		}
		copied = occurrence.range.end;
	}
	if copied == 0 {
		return Str::new(input);
	}
	output.push_str(&input[copied..]);
	Str::new(output)
}

/// Replaces materialized URIs as raw path values outside shell source text.
///
/// Environment values and the dedicated `cwd` parameter are transported as
/// data, so shell quoting there would become part of the path.
pub fn replace_plain(input: &str, paths: &BTreeMap<Str, Str>) -> Str {
	let occurrences = scan(input);
	let mut output = String::with_capacity(input.len());
	let mut copied = 0;
	for occurrence in occurrences {
		let Some(path) = paths.get(&occurrence.uri) else {
			continue;
		};
		output.push_str(&input[copied..occurrence.range.start]);
		output.push_str(path);
		copied = occurrence.range.end;
	}
	if copied == 0 {
		return Str::new(input);
	}
	output.push_str(&input[copied..]);
	Str::new(output)
}

const PATH_SCHEMES: [&str; 7] =
	["artifact://", "memory://", "agent://", "local://", "skill://", "rule://", "plan://"];

fn uri_end(input: &str, start: usize, quote: QuoteContext) -> usize {
	let bytes = input.as_bytes();
	let mut cursor = start;
	while let Some(&byte) = bytes.get(cursor) {
		let terminal = match quote {
			QuoteContext::Single => byte == b'\'',
			QuoteContext::Double => byte == b'"',
			QuoteContext::Unquoted | QuoteContext::Substitution => {
				byte.is_ascii_whitespace()
					|| matches!(
						byte,
						b'\'' | b'"' | b';' | b'|' | b'&' | b'<' | b'>' | b'(' | b'$' | b'\\'
							| b')'
					)
			},
		};
		if terminal {
			break;
		}
		cursor += 1;
	}
	while cursor > start && matches!(bytes[cursor - 1], b',' | b'.' | b':' | b']' | b'}') {
		cursor -= 1;
	}
	cursor
}

fn push_single_quoted(output: &mut String, path: &str) {
	output.push('\'');
	for part in path.split('\'') {
		if !output.ends_with('\'') {
			output.push_str("'\\''");
		}
		output.push_str(part);
	}
	output.push('\'');
}

fn push_double_escaped(output: &mut String, path: &str) {
	for character in path.chars() {
		if matches!(character, '"' | '\\' | '$' | '`') {
			output.push('\\');
		}
		output.push(character);
	}
}

#[cfg(test)]
mod tests {
	use std::collections::BTreeMap;

	use omp_core::Str;

	use super::{QuoteContext, replace, replace_plain, scan};

	#[test]
	fn scans_command_substitution_and_quotes_without_false_suffixes() {
		let found = scan("read artifact://a:raw; echo \"local://x y\" 'skill://literal'");
		assert_eq!(found.len(), 3);
		assert_eq!(found[0].uri, "artifact://a:raw");
		assert_eq!(found[1].uri, "local://x y");
		assert_eq!(found[2].quote, QuoteContext::Single);
	}

	#[test]
	fn replacement_quotes_environment_paths_and_preserves_literals() {
		let paths = BTreeMap::from([(Str::new("local://x"), Str::new("/tmp/a b"))]);
		assert_eq!(replace("cat local://x 'local://x'", &paths), "cat '/tmp/a b' 'local://x'");
	}

	#[test]
	fn unquoted_internal_uris_stop_before_shell_syntax() {
		let paths = BTreeMap::from([(Str::new("local://x.md"), Str::new("/tmp/a b"))]);
		for (command, expected) in [
			("read local://x.md;echo hi", "read '/tmp/a b';echo hi"),
			("cat local://x.md&&echo hi", "cat '/tmp/a b'&&echo hi"),
			("cat local://x.md|head", "cat '/tmp/a b'|head"),
			("cat local://x.md<input", "cat '/tmp/a b'<input"),
			("cat local://x.md>output", "cat '/tmp/a b'>output"),
			("cat local://x.md(subshell)", "cat '/tmp/a b'(subshell)"),
			("cat local://x.md$HOME", "cat '/tmp/a b'$HOME"),
			(r"cat local://x.md\ suffix", r"cat '/tmp/a b'\ suffix"),
			("cat local://x.md)", "cat '/tmp/a b')"),
		] {
			assert_eq!(replace(command, &paths), expected, "{command}");
		}
	}

	#[test]
	fn data_replacement_does_not_embed_shell_quotes() {
		let paths = BTreeMap::from([(Str::new("local://x"), Str::new("/tmp/a b"))]);
		assert_eq!(replace_plain("local://x", &paths), "/tmp/a b");
	}
}
