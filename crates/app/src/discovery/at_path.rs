//! Safe recursive expansion of `@path` references in instruction files.

use std::{
	collections::BTreeSet,
	env, fs, io,
	path::{Path, PathBuf},
};

const LEADING_PUNCTUATION: &[char] = &['`', '"', '\'', '(', '[', '{', '<'];
const TRAILING_PUNCTUATION: &[char] =
	&[')', ']', '}', '>', '.', ',', ';', ':', '!', '?', '"', '\'', '`'];
/// Maximum number of nested `@path` edges; the root is depth zero.
pub const MAX_AT_PATH_DEPTH: usize = 5;

/// Expands local `@path` references while preserving code and address syntax.
pub fn expand_at_paths(path: &Path) -> io::Result<String> {
	let mut visited = BTreeSet::new();
	expand(path, 0, &mut visited)
}
/// Expands local `@path` references in an already-parsed instruction field.
///
/// Relative references resolve beside `source`. The source itself is marked
/// visited so an instruction cannot recursively import its containing YAML
/// document.
pub fn expand_at_text(text: &str, source: &Path) -> io::Result<String> {
	let canonical = fs::canonicalize(source)?;
	let mut visited = BTreeSet::from([canonical.clone()]);
	expand_content(text, canonical.parent().unwrap_or(Path::new("")), 0, &mut visited)
}
/// Extracts unique `@path`, `@'path'`, and `@"path"` mentions in first-seen
/// order.
///
/// Mentions require a prose boundary, so email addresses and Git remotes do not
/// become filesystem reads. Unquoted surrounding punctuation is stripped.
pub fn extract_path_mentions(text: &str) -> Vec<String> {
	let mut mentions = Vec::new();
	let mut cursor = 0;
	while cursor < text.len() {
		let Some(relative) = text[cursor..].find('@') else {
			break;
		};
		let at = cursor + relative;
		let Some((raw, end, quoted)) = mention_token(text, at) else {
			cursor = at + 1;
			continue;
		};
		let cleaned = if quoted {
			raw.trim()
		} else {
			raw.trim()
				.trim_start_matches(LEADING_PUNCTUATION)
				.trim_end_matches(TRAILING_PUNCTUATION)
				.trim()
		};
		if !cleaned.is_empty()
			&& !cleaned.contains('@')
			&& !cleaned.contains("://")
			&& !mentions.iter().any(|existing| existing == cleaned)
		{
			mentions.push(cleaned.to_owned());
		}
		cursor = end.max(at + 1);
	}
	mentions
}

fn mention_token(text: &str, at: usize) -> Option<(&str, usize, bool)> {
	if text.as_bytes().get(at) != Some(&b'@') || !is_mention_boundary(text, at) {
		return None;
	}
	let start = at + 1;
	let first = text[start..].chars().next()?;
	if matches!(first, '\'' | '"') {
		let content_start = start + first.len_utf8();
		let relative_end = text[content_start..].find(first)?;
		let content_end = content_start + relative_end;
		return Some((&text[content_start..content_end], content_end + first.len_utf8(), true));
	}
	let end = text[start..]
		.find(|character: char| character.is_whitespace() || character == '@')
		.map_or(text.len(), |offset| start + offset);
	Some((&text[start..end], end, false))
}

fn is_mention_boundary(text: &str, at: usize) -> bool {
	text[..at].chars().next_back().is_none_or(|previous| {
		previous.is_whitespace() || matches!(previous, '(' | '[' | '{' | '<' | '"' | '\'' | '`')
	})
}

fn expand(path: &Path, depth: usize, visited: &mut BTreeSet<PathBuf>) -> io::Result<String> {
	if depth > MAX_AT_PATH_DEPTH {
		return fs::read_to_string(path);
	}
	let canonical = fs::canonicalize(path)?;
	if !visited.insert(canonical.clone()) {
		return Ok(String::new());
	}
	let content = fs::read_to_string(&canonical)?;
	let rendered =
		expand_content(&content, canonical.parent().unwrap_or(Path::new("")), depth, visited)?;
	visited.remove(&canonical);
	Ok(rendered)
}

fn expand_content(
	content: &str,
	base: &Path,
	depth: usize,
	visited: &mut BTreeSet<PathBuf>,
) -> io::Result<String> {
	let mut fenced = None::<&str>;
	let mut rendered = String::with_capacity(content.len());
	for line in content.split_inclusive('\n') {
		let trimmed = line.trim_start();
		if let Some(marker) = fenced {
			rendered.push_str(line);
			if trimmed.starts_with(marker) {
				fenced = None;
			}
			continue;
		}
		if trimmed.starts_with("```") {
			fenced = Some("```");
			rendered.push_str(line);
			continue;
		}
		if trimmed.starts_with("~~~") {
			fenced = Some("~~~");
			rendered.push_str(line);
			continue;
		}
		rendered.push_str(&expand_line(line, base, depth, visited)?);
	}
	Ok(rendered)
}

fn expand_line(
	line: &str,
	base: &Path,
	depth: usize,
	visited: &mut BTreeSet<PathBuf>,
) -> io::Result<String> {
	let mut output = String::with_capacity(line.len());
	let mut cursor = 0;
	let bytes = line.as_bytes();
	let mut inline_code = false;
	while cursor < bytes.len() {
		if bytes[cursor] == b'`' {
			inline_code = !inline_code;
			output.push('`');
			cursor += 1;
			continue;
		}
		if inline_code || bytes[cursor] != b'@' || is_address_prefix(line, cursor) {
			let character = line[cursor..]
				.chars()
				.next()
				.expect("cursor is in the line");
			output.push(character);
			cursor += character.len_utf8();
			continue;
		}
		let start = cursor + 1;
		let mut end = start;
		while end < bytes.len() && !bytes[end].is_ascii_whitespace() {
			end += 1;
		}
		let token = line[start..end].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}']);
		if token.is_empty()
			|| token.contains('@')
			|| token.contains("://")
			|| token.ends_with(".git") && token.contains('/')
		{
			output.push('@');
			cursor += 1;
			continue;
		}
		let suffix = &line[start + token.len()..end];
		let target = resolve_path(base, token);
		if let Ok(expanded) = target.and_then(|target| expand(&target, depth + 1, visited)) {
			output.push_str(&expanded)
		} else {
			output.push('@');
			output.push_str(token);
		}
		output.push_str(suffix);
		cursor = end;
	}
	Ok(output)
}

fn is_address_prefix(line: &str, at: usize) -> bool {
	!is_mention_boundary(line, at)
}

fn resolve_path(base: &Path, token: &str) -> io::Result<PathBuf> {
	if token == "~" {
		return env::var_os("HOME")
			.map(PathBuf::from)
			.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is unset"));
	}
	if let Some(rest) = token.strip_prefix("~/") {
		return env::var_os("HOME")
			.map(|home| PathBuf::from(home).join(rest))
			.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is unset"));
	}
	let path = Path::new(token);
	Ok(if path.is_absolute() {
		path.to_path_buf()
	} else {
		base.join(path)
	})
}

#[cfg(test)]
mod tests {
	use std::fs;

	use super::*;
	#[test]
	fn protects_code_addresses_and_punctuation() {
		let tree = tempfile::tempdir().expect("tree");
		fs::write(tree.path().join("child.md"), "child").expect("child");
		fs::write(
			tree.path().join("root.md"),
			"@child.md. user@example.test git@github.com:a/b `@child.md`\n```\n@child.md\n```\n",
		)
		.expect("root");
		assert_eq!(
			expand_at_paths(&tree.path().join("root.md")).expect("expand"),
			"child. user@example.test git@github.com:a/b `@child.md`\n```\n@child.md\n```\n"
		);
	}

	#[test]
	fn extracts_quoted_paths_and_strips_balanced_punctuation() {
		assert_eq!(
			extract_path_mentions(
				r#"Read @src/main.rs, @"docs/design notes.md" and (@'other notes.txt'). Ignore me@example.test and git@github.com:a/b."#,
			),
			["src/main.rs", "docs/design notes.md", "other notes.txt"]
		);
	}

	#[test]
	fn mention_extraction_preserves_first_seen_order_and_deduplicates() {
		assert_eq!(extract_path_mentions("@b.rs @a.rs @b.rs https://example/@not-a-path"), [
			"b.rs", "a.rs"
		]);
	}

	#[test]
	fn cycle_and_depth_are_bounded() {
		let tree = tempfile::tempdir().expect("tree");
		fs::write(tree.path().join("a"), "@b").expect("a");
		fs::write(tree.path().join("b"), "@a").expect("b");
		assert_eq!(expand_at_paths(&tree.path().join("a")).expect("expand"), "");
	}
}
