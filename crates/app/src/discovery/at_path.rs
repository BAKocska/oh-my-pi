//! Safe recursive expansion of `@path` references in instruction files.

use std::{
	collections::BTreeSet,
	env, fs, io,
	path::{Path, PathBuf},
};

/// Maximum number of nested `@path` edges; the root is depth zero.
pub const MAX_AT_PATH_DEPTH: usize = 5;

/// Expands local `@path` references while preserving code and address syntax.
pub fn expand_at_paths(path: &Path) -> io::Result<String> {
	let mut visited = BTreeSet::new();
	expand(path, 0, &mut visited)
}

fn expand(path: &Path, depth: usize, visited: &mut BTreeSet<PathBuf>) -> io::Result<String> {
	if depth > MAX_AT_PATH_DEPTH {
		return Ok(fs::read_to_string(path)?);
	}
	let canonical = fs::canonicalize(path)?;
	if !visited.insert(canonical.clone()) {
		return Ok(String::new());
	}
	let content = fs::read_to_string(&canonical)?;
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
		rendered.push_str(&expand_line(
			line,
			canonical.parent().unwrap_or(Path::new("")),
			depth,
			visited,
		)?);
	}
	visited.remove(&canonical);
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
		match target.and_then(|target| expand(&target, depth + 1, visited)) {
			Ok(expanded) => output.push_str(&expanded),
			Err(_) => {
				output.push('@');
				output.push_str(token);
			},
		}
		output.push_str(suffix);
		cursor = end;
	}
	Ok(output)
}

fn is_address_prefix(line: &str, at: usize) -> bool {
	let Some(previous) = line[..at].chars().next_back() else {
		return false;
	};
	previous.is_ascii_alphanumeric() || matches!(previous, '.' | '-' | '_')
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
	fn cycle_and_depth_are_bounded() {
		let tree = tempfile::tempdir().expect("tree");
		fs::write(tree.path().join("a"), "@b").expect("a");
		fs::write(tree.path().join("b"), "@a").expect("b");
		assert_eq!(expand_at_paths(&tree.path().join("a")).expect("expand"), "");
	}
}
