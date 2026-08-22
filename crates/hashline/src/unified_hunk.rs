//! Deterministic unified-hunk parsing and in-memory application.

use omp_core::Str;

/// One parsed unified hunk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnifiedHunk {
	/// Optional scoped context carried by the `@@` header.
	pub scope:    Option<Str>,
	/// Optional one-based source-line hint.
	pub old_line: Option<usize>,
	/// Rows consumed from the old document.
	pub old:      Vec<Str>,
	/// Rows emitted into the new document.
	pub new:      Vec<Str>,
	/// Whether the authored hunk explicitly targets EOF.
	pub eof:      bool,
}

/// A malformed or unsafe unified hunk.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum UnifiedHunkError {
	/// Content preceded the first hunk header.
	#[error("unified patch content at line {line} must follow an '@@' header")]
	MissingHeader { line: usize },
	/// One body row used an unknown prefix.
	#[error("invalid unified patch row at line {line}")]
	InvalidRow { line: usize },
	/// A hunk had no source or replacement rows.
	#[error("unified patch hunk at line {line} is empty")]
	EmptyHunk { line: usize },
	/// No bounded context variant matched.
	#[error("hunk {hunk} did not match the source")]
	NoMatch { hunk: usize },
	/// More than one equally valid location matched.
	#[error("hunk {hunk} is ambiguous at source lines {lines:?}")]
	Ambiguous { hunk: usize, lines: Vec<usize> },
	/// Create would overwrite an existing file without explicit permission.
	#[error("create operation would overwrite an existing file")]
	CreateOverwrite,
	/// Delete requires an existing source.
	#[error("delete operation requires an existing file")]
	DeleteMissing,
	/// Update requires an existing source.
	#[error("update operation requires an existing file")]
	UpdateMissing,
}

/// Parses `@@`-delimited unified hunks used inside a Codex update operation.
pub fn parse_unified_hunks(input: &str) -> Result<Vec<UnifiedHunk>, UnifiedHunkError> {
	let normalized = input.replace("\r\n", "\n").replace('\r', "\n");
	let mut hunks = Vec::new();
	let mut current: Option<(usize, UnifiedHunk)> = None;
	for (index, line) in normalized.lines().enumerate() {
		let line_number = index + 1;
		if line.starts_with("@@") {
			if let Some((start, hunk)) = current.take() {
				push_hunk(&mut hunks, start, hunk)?;
			}
			let header = line.trim_start_matches('@').trim();
			let (old_line, scope) = parse_header(header);
			current = Some((line_number, UnifiedHunk {
				scope,
				old_line,
				old: Vec::new(),
				new: Vec::new(),
				eof: false,
			}));
			continue;
		}
		if line == "*** End of File" {
			let Some((_, hunk)) = current.as_mut() else {
				return Err(UnifiedHunkError::MissingHeader { line: line_number });
			};
			hunk.eof = true;
			continue;
		}
		let Some((_, hunk)) = current.as_mut() else {
			if line.trim().is_empty() {
				continue;
			}
			return Err(UnifiedHunkError::MissingHeader { line: line_number });
		};
		let Some((prefix, body)) = line.split_at_checked(1) else {
			return Err(UnifiedHunkError::InvalidRow { line: line_number });
		};
		match prefix {
			" " => {
				hunk.old.push(Str::new(body));
				hunk.new.push(Str::new(body));
			},
			"-" => hunk.old.push(Str::new(body)),
			"+" => hunk.new.push(Str::new(body)),
			_ => return Err(UnifiedHunkError::InvalidRow { line: line_number }),
		}
	}
	if let Some((start, hunk)) = current {
		push_hunk(&mut hunks, start, hunk)?;
	}
	Ok(hunks)
}

fn push_hunk(
	hunks: &mut Vec<UnifiedHunk>,
	line: usize,
	hunk: UnifiedHunk,
) -> Result<(), UnifiedHunkError> {
	if hunk.old.is_empty() && hunk.new.is_empty() {
		return Err(UnifiedHunkError::EmptyHunk { line });
	}
	hunks.push(hunk);
	Ok(())
}

fn parse_header(header: &str) -> (Option<usize>, Option<Str>) {
	let (coordinates, scope) = if let Some((coordinates, scope)) = header.split_once("@@") {
		(coordinates.trim(), scope.trim())
	} else if header.starts_with('-') {
		(header, "")
	} else {
		("", header)
	};
	let old_line = coordinates
		.split_whitespace()
		.next()
		.filter(|part| part.starts_with('-'))
		.and_then(|part| part[1..].split(',').next())
		.and_then(|line| line.parse().ok());
	let scope = (!scope.is_empty()).then(|| Str::new(scope));
	(old_line, scope)
}

/// Applies a sequence of unified hunks to UTF-8 source text.
///
/// Matching is exact first, then indentation-adapted. Up to three common
/// context rows on either boundary may be removed as bounded recovery. Every
/// accepted variant must identify one location; ambiguity is never resolved by
/// first-match order.
pub fn apply_unified_hunks(
	source: &str,
	hunks: &[UnifiedHunk],
) -> Result<String, UnifiedHunkError> {
	let had_newline = source.ends_with('\n');
	let mut lines = source.lines().map(ToOwned::to_owned).collect::<Vec<_>>();
	let mut cursor = 0usize;
	for (hunk_index, hunk) in hunks.iter().enumerate() {
		let located = locate_hunk(&lines, hunk, cursor, hunk_index + 1)?;
		let old = &hunk.old[located.trim_front..hunk.old.len() - located.trim_back];
		let new = &hunk.new[located.trim_front..hunk.new.len() - located.trim_back];
		let replacement =
			adapt_indentation(old, &lines[located.start..located.start + old.len()], new);
		let end = located.start + old.len();
		lines.splice(located.start..end, replacement);
		cursor = located.start + new.len();
	}
	let mut output = lines.join("\n");
	if had_newline && !output.ends_with('\n') {
		output.push('\n');
	}
	Ok(output)
}

#[derive(Clone, Copy)]
struct Located {
	start:      usize,
	trim_front: usize,
	trim_back:  usize,
}

fn locate_hunk(
	lines: &[String],
	hunk: &UnifiedHunk,
	cursor: usize,
	hunk_number: usize,
) -> Result<Located, UnifiedHunkError> {
	let max_trim = hunk.old.len().min(hunk.new.len()).min(3);
	for total_trim in 0..=max_trim * 2 {
		for front in 0..=total_trim.min(max_trim) {
			let back = total_trim - front;
			if back > max_trim || front + back >= hunk.old.len() {
				continue;
			}
			if !shared_context(&hunk.old, &hunk.new, front, back) {
				continue;
			}
			let pattern = &hunk.old[front..hunk.old.len() - back];
			let mut matches = sequence_matches(lines, pattern, cursor, false);
			if matches.is_empty() {
				matches = sequence_matches(lines, pattern, cursor, true);
			}
			if let Some(scope) = &hunk.scope {
				matches.retain(|start| scope_before(lines, *start, scope));
			}
			if matches.len() == 1 {
				return Ok(Located { start: matches[0], trim_front: front, trim_back: back });
			}
			if matches.len() > 1 {
				if let Some(hint) = hunk.old_line.map(|line| line.saturating_sub(1)) {
					let nearest = matches
						.iter()
						.copied()
						.min_by_key(|start| start.abs_diff(hint));
					if let Some(nearest) = nearest
						&& matches
							.iter()
							.filter(|start| start.abs_diff(hint) == nearest.abs_diff(hint))
							.count() == 1
					{
						return Ok(Located { start: nearest, trim_front: front, trim_back: back });
					}
				}
				return Err(UnifiedHunkError::Ambiguous {
					hunk:  hunk_number,
					lines: matches.into_iter().map(|line| line + 1).collect(),
				});
			}
		}
	}
	Err(UnifiedHunkError::NoMatch { hunk: hunk_number })
}

fn shared_context(old: &[Str], new: &[Str], front: usize, back: usize) -> bool {
	old.iter().take(front).eq(new.iter().take(front))
		&& old.iter().rev().take(back).eq(new.iter().rev().take(back))
}

fn sequence_matches(
	lines: &[String],
	pattern: &[Str],
	from: usize,
	indentation: bool,
) -> Vec<usize> {
	if pattern.is_empty() {
		return vec![from.min(lines.len())];
	}
	let mut matches = Vec::new();
	for start in from..=lines.len().saturating_sub(pattern.len()) {
		let actual = &lines[start..start + pattern.len()];
		let equal = if indentation {
			indentation_equivalent(pattern, actual)
		} else {
			pattern
				.iter()
				.map(Str::as_str)
				.eq(actual.iter().map(String::as_str))
		};
		if equal {
			matches.push(start);
		}
	}
	matches
}

fn indentation_equivalent(pattern: &[Str], actual: &[String]) -> bool {
	let mut delta = None;
	for (expected, found) in pattern.iter().zip(actual) {
		if expected.trim_start() != found.trim_start() {
			return false;
		}
		let expected_indent = expected.len() - expected.trim_start().len();
		let found_indent = found.len() - found.trim_start().len();
		let row_delta = found_indent as isize - expected_indent as isize;
		if let Some(delta) = delta {
			if delta != row_delta && !expected.trim().is_empty() {
				return false;
			}
		} else if !expected.trim().is_empty() {
			delta = Some(row_delta);
		}
	}
	true
}

fn adapt_indentation(old: &[Str], actual: &[String], new: &[Str]) -> Vec<String> {
	let delta = old
		.iter()
		.zip(actual)
		.find(|(expected, _)| !expected.trim().is_empty())
		.map_or(0, |(expected, found)| {
			(found.len() - found.trim_start().len()) as isize
				- (expected.len() - expected.trim_start().len()) as isize
		});
	new.iter()
		.map(|line| {
			if line.trim().is_empty() || delta == 0 {
				return line.to_string();
			}
			let indent = line.len() - line.trim_start().len();
			let adjusted = indent.saturating_add_signed(delta);
			let mut output = String::with_capacity(line.len().saturating_add_signed(delta));
			output.extend(std::iter::repeat_n(' ', adjusted));
			output.push_str(line.trim_start().as_str());
			output
		})
		.collect()
}

fn scope_before(lines: &[String], start: usize, scope: &str) -> bool {
	lines[..start]
		.iter()
		.rev()
		.take(200)
		.any(|line| line.trim() == scope.trim() || line.contains(scope.trim()))
}

/// Applies create/delete/update semantics without performing I/O.
pub fn apply_file_operation(
	existing: Option<&str>,
	operation: &crate::foreign_patch::ForeignPatchFile,
	allow_create_overwrite: bool,
) -> Result<Option<String>, UnifiedHunkError> {
	match operation {
		crate::foreign_patch::ForeignPatchFile::Add { content, .. } => {
			if existing.is_some() && !allow_create_overwrite {
				Err(UnifiedHunkError::CreateOverwrite)
			} else {
				Ok(Some(content.to_string()))
			}
		},
		crate::foreign_patch::ForeignPatchFile::Delete { .. } => existing
			.map(|_| None)
			.ok_or(UnifiedHunkError::DeleteMissing),
		crate::foreign_patch::ForeignPatchFile::Update { hunks, .. } => {
			let source = existing.ok_or(UnifiedHunkError::UpdateMissing)?;
			let parsed = parse_unified_hunks(hunks)?;
			apply_unified_hunks(source, &parsed).map(Some)
		},
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn adapts_indentation_and_preserves_newline() {
		let hunks = parse_unified_hunks("@@ fn x\n-  old\n+  new").expect("parse");
		assert_eq!(apply_unified_hunks("fn x\n    old\n", &hunks).expect("apply"), "fn x\n    new\n");
	}

	#[test]
	fn rejects_repeated_context_without_hint() {
		let hunks = parse_unified_hunks("@@\n-same\n+new").expect("parse");
		assert!(matches!(
			apply_unified_hunks("same\nsame\n", &hunks),
			Err(UnifiedHunkError::Ambiguous { .. })
		));
	}

	#[test]
	fn line_hint_disambiguates_repeated_context() {
		let hunks = parse_unified_hunks("@@ -2,1 +2,1 @@\n-same\n+new").expect("parse");
		assert_eq!(apply_unified_hunks("same\nsame\n", &hunks).expect("apply"), "same\nnew\n");
	}
}
