//! Codex `apply_patch` envelope parsing.

use omp_core::Str;

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";
const ADD: &str = "*** Add File: ";
const DELETE: &str = "*** Delete File: ";
const UPDATE: &str = "*** Update File: ";
const MOVE: &str = "*** Move to: ";

/// One file operation decoded from an `apply_patch` envelope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ForeignPatchFile {
	/// Create a file from `+`-prefixed lines.
	Add { path: Str, content: Str },
	/// Remove a file.
	Delete { path: Str },
	/// Apply unified hunks, optionally moving the result.
	Update { path: Str, move_to: Option<Str>, hunks: Str },
}

impl ForeignPatchFile {
	/// Authored source path.
	pub fn path(&self) -> &str {
		match self {
			Self::Add { path, .. } | Self::Delete { path } | Self::Update { path, .. } => path,
		}
	}
}

/// A malformed `apply_patch` envelope.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ForeignPatchError {
	/// Strict parsing requires the opening sentinel.
	#[error("the first line of the patch must be '*** Begin Patch'")]
	MissingBegin,
	/// Strict parsing requires the closing sentinel.
	#[error("the last line of the patch must be '*** End Patch'")]
	MissingEnd,
	/// A file marker had no path.
	#[error("file operation at line {line} has an empty path")]
	EmptyPath { line: usize },
	/// Add-file bodies contain only `+` rows.
	#[error("add-file body at line {line} must contain only '+' rows")]
	InvalidAddBody { line: usize },
	/// Update-file bodies must contain a hunk.
	#[error("update-file hunk for {path} is empty at line {line}")]
	EmptyUpdate { path: Str, line: usize },
	/// The next top-level marker was not recognized.
	#[error("invalid apply_patch file marker at line {line}: {text}")]
	InvalidMarker { line: usize, text: Str },
}

/// Parses a complete Codex patch envelope.
pub fn parse_foreign_patch(input: &str) -> Result<Vec<ForeignPatchFile>, ForeignPatchError> {
	parse(input, false)
}

/// Parses the complete prefix of an in-progress Codex patch envelope.
///
/// Missing envelope terminators and an incomplete trailing hunk are tolerated;
/// malformed completed operations are still rejected.
pub fn parse_foreign_patch_streaming(
	input: &str,
) -> Result<Vec<ForeignPatchFile>, ForeignPatchError> {
	parse(input, true)
}

fn parse(input: &str, streaming: bool) -> Result<Vec<ForeignPatchFile>, ForeignPatchError> {
	let normalized = input.replace("\r\n", "\n").replace('\r', "\n");
	let trimmed = normalized.trim();
	if trimmed.is_empty() {
		return if streaming {
			Ok(Vec::new())
		} else {
			Err(ForeignPatchError::MissingBegin)
		};
	}
	let mut lines = trimmed.lines().collect::<Vec<_>>();
	if lines.len() >= 2 {
		let opener = lines[0].trim();
		let terminator = lines.last().copied().unwrap_or_default().trim();
		let heredoc = match opener {
			"<<EOF" => Some("EOF"),
			"<<'EOF'" => Some("EOF"),
			"<<\"EOF\"" => Some("EOF"),
			_ => None,
		};
		if heredoc == Some(terminator) {
			lines.remove(0);
			lines.pop();
		}
	}
	if lines.first().is_none_or(|line| line.trim() != BEGIN) {
		return if streaming {
			Ok(Vec::new())
		} else {
			Err(ForeignPatchError::MissingBegin)
		};
	}
	let has_end = lines.last().is_some_and(|line| line.trim() == END);
	if !has_end && !streaming {
		return Err(ForeignPatchError::MissingEnd);
	}
	let end = lines.len().saturating_sub(usize::from(has_end));
	let mut cursor = 1;
	let mut files = Vec::new();
	while cursor < end {
		if lines[cursor].trim().is_empty() {
			cursor += 1;
			continue;
		}
		let header = lines[cursor].trim();
		let line = cursor + 1;
		if let Some(path) = header.strip_prefix(ADD) {
			if path.is_empty() {
				return Err(ForeignPatchError::EmptyPath { line });
			}
			cursor += 1;
			let mut content = String::new();
			while cursor < end && !is_file_marker(lines[cursor]) {
				let body = lines[cursor];
				if body.trim().is_empty() {
					break;
				}
				let Some(body) = body.strip_prefix('+') else {
					if streaming && cursor + 1 == end {
						break;
					}
					return Err(ForeignPatchError::InvalidAddBody { line: cursor + 1 });
				};
				content.push_str(body);
				content.push('\n');
				cursor += 1;
			}
			files.push(ForeignPatchFile::Add { path: Str::new(path), content: content.into() });
			continue;
		}
		if let Some(path) = header.strip_prefix(DELETE) {
			if path.is_empty() {
				return Err(ForeignPatchError::EmptyPath { line });
			}
			files.push(ForeignPatchFile::Delete { path: Str::new(path) });
			cursor += 1;
			continue;
		}
		if let Some(path) = header.strip_prefix(UPDATE) {
			if path.is_empty() {
				return Err(ForeignPatchError::EmptyPath { line });
			}
			cursor += 1;
			let move_to = if cursor < end {
				lines[cursor].strip_prefix(MOVE).map(|path| {
					cursor += 1;
					Str::new(path)
				})
			} else {
				None
			};
			let body_start = cursor;
			while cursor < end && !is_file_marker(lines[cursor]) {
				cursor += 1;
			}
			if body_start == cursor && !streaming {
				return Err(ForeignPatchError::EmptyUpdate { path: Str::new(path), line: cursor + 1 });
			}
			files.push(ForeignPatchFile::Update {
				path: Str::new(path),
				move_to,
				hunks: lines[body_start..cursor].join("\n").into(),
			});
			continue;
		}
		if streaming {
			break;
		}
		return Err(ForeignPatchError::InvalidMarker { line, text: Str::new(header) });
	}
	Ok(files)
}

fn is_file_marker(line: &str) -> bool {
	let line = line.trim();
	line.starts_with(ADD) || line.starts_with(DELETE) || line.starts_with(UPDATE)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_heredoc_add_update_move_delete() {
		let patch = "<<'EOF'\n*** Begin Patch\n*** Add File: new.txt\n+one\n+two\n*** Update File: \
		             old.txt\n*** Move to: moved.txt\n@@ fn x\n-old\n+new\n*** End of File\n*** \
		             Delete File: gone.txt\n*** End Patch\nEOF";
		let files = parse_foreign_patch(patch).expect("valid envelope");
		assert_eq!(files.len(), 3);
		assert_eq!(files[0], ForeignPatchFile::Add {
			path:    "new.txt".into(),
			content: "one\ntwo\n".into(),
		});
		assert!(
			matches!(&files[1], ForeignPatchFile::Update { path, move_to: Some(dest), .. } if path == "old.txt" && dest == "moved.txt")
		);
		assert_eq!(files[2], ForeignPatchFile::Delete { path: "gone.txt".into() });
	}

	#[test]
	fn streaming_retains_complete_prefix() {
		let files =
			parse_foreign_patch_streaming("*** Begin Patch\n*** Add File: a\n+x\n*** Update File: b")
				.expect("streaming prefix");
		assert_eq!(files.len(), 2);
	}
}
