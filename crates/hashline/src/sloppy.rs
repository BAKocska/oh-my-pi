//! Pi-compatible sloppy edit parsing and pure, atomic text transformation.

use omp_core::Str;

const OPEN: &str = "«";
const ALL: &str = "«*";
const REWRITE: &str = "»";
const GAP: &str = "…";
const SELECT_OPEN: &str = "⟪";
const SELECT_CLOSE: &str = "⟫";
const SELECT_DIVIDER: &str = "│";
const MAX_CANDIDATES: usize = 200;

/// One `[path]` section of a sloppy payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SloppySection {
	/// Authored path without brackets.
	pub path:  Str,
	/// Canonical operation body.
	pub input: Str,
}

/// Sloppy syntax, matching, or recovery failure.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SloppyError {
	/// Text appeared before the first path header.
	#[error("sloppy input must begin with a [path] header")]
	MissingPath,
	/// A section had no operations.
	#[error("sloppy section for {path} has no operations")]
	EmptySection { path: Str },
	/// An operation opener was malformed.
	#[error("operation {operation} has an invalid opener")]
	InvalidOpener { operation: usize },
	/// Match/rewrite structure was malformed.
	#[error("operation {operation} is malformed: {reason}")]
	Malformed { operation: usize, reason: &'static str },
	/// A pattern found no bounded exact or fuzzy match.
	#[error("operation {operation} did not match the source")]
	NoMatch { operation: usize },
	/// A unique operation matched multiple locations.
	#[error("operation {operation} is ambiguous at source lines {lines:?}; use «* or add context")]
	Ambiguous { operation: usize, lines: Vec<usize> },
	/// A backward re-emission references an unavailable operation.
	#[error("operation {operation} references unavailable deleted text »{reference}")]
	InvalidReference { operation: usize, reference: usize },
	/// Applying all selected ranges would overlap.
	#[error("operation {operation} selects overlapping matches")]
	Overlap { operation: usize },
	/// An operation parsed cleanly but changed no bytes.
	#[error("operation {operation} produced no change")]
	NoChange { operation: usize },
}

/// Splits a payload into path sections, dropping common foreign-envelope noise.
pub fn split_sloppy_sections(input: &str) -> Result<Vec<SloppySection>, SloppyError> {
	let normalized = input.replace("\r\n", "\n").replace('\r', "\n");
	let mut sections: Vec<SloppySection> = Vec::new();
	let mut path: Option<Str> = None;
	let mut body = String::new();
	for line in normalized.lines() {
		let trimmed = line.trim();
		if envelope_noise(trimmed) {
			continue;
		}
		if let Some(header) = trimmed
			.strip_prefix('[')
			.and_then(|line| line.strip_suffix(']'))
			&& !header.is_empty()
			&& !header.contains(['[', ']'])
		{
			if let Some(previous) = path.replace(Str::new(header)) {
				if body.trim().is_empty() {
					return Err(SloppyError::EmptySection { path: previous });
				}
				sections.push(SloppySection { path: previous, input: body.trim_matches('\n').into() });
				body.clear();
			}
			continue;
		}
		if path.is_none() {
			if trimmed.is_empty() {
				continue;
			}
			return Err(SloppyError::MissingPath);
		}
		body.push_str(line);
		body.push('\n');
	}
	if let Some(path) = path {
		if body.trim().is_empty() {
			return Err(SloppyError::EmptySection { path });
		}
		sections.push(SloppySection { path, input: body.trim_matches('\n').into() });
	}
	if sections.is_empty() {
		return Err(SloppyError::MissingPath);
	}
	Ok(sections)
}

fn envelope_noise(line: &str) -> bool {
	line == "***"
		|| line.starts_with("*** Begin")
		|| line.starts_with("*** End")
		|| line.starts_with("*** Abort")
		|| line.starts_with("*** Update File:")
		|| line.starts_with("*** Add File:")
		|| line.starts_with("*** Delete File:")
}

#[derive(Clone, Debug)]
struct Operation {
	all:     bool,
	ordinal: Option<usize>,
	pattern: String,
	rewrite: Rewrite,
}

#[derive(Clone, Debug)]
enum Rewrite {
	Explicit(String),
	LegacySelection { old: String, new: String },
	Inline(Vec<Selection>),
}

#[derive(Clone, Debug)]
struct Selection {
	old: String,
	new: String,
}

fn parse_operations(input: &str) -> Result<Vec<Operation>, SloppyError> {
	let lines = input.lines().collect::<Vec<_>>();
	let mut cursor = 0;
	let mut operations = Vec::new();
	while cursor < lines.len() {
		if lines[cursor].trim().is_empty() {
			cursor += 1;
			continue;
		}
		let opener = lines[cursor].trim();
		let (all, ordinal) = if opener == OPEN {
			(false, None)
		} else if opener == ALL {
			(true, None)
		} else if let Some(value) = opener
			.strip_prefix(OPEN)
			.and_then(|value| value.parse().ok())
		{
			(false, Some(value))
		} else {
			return Err(SloppyError::InvalidOpener { operation: operations.len() + 1 });
		};
		cursor += 1;
		let start = cursor;
		while cursor < lines.len() && lines[cursor].trim() != REWRITE && !is_opener(lines[cursor]) {
			cursor += 1;
		}
		let pattern = lines[start..cursor].join("\n");
		if pattern.is_empty() {
			return Err(SloppyError::Malformed {
				operation: operations.len() + 1,
				reason:    "MATCH must not be empty",
			});
		}
		let rewrite = if cursor < lines.len() && lines[cursor].trim() == REWRITE {
			cursor += 1;
			let start = cursor;
			while cursor < lines.len() && !is_opener(lines[cursor]) {
				cursor += 1;
			}
			let authored_rewrite = lines[start..cursor]
				.iter()
				.map(|line| line.trim_start_matches('＋'))
				.collect::<Vec<_>>()
				.join("\n");
			if pattern.contains(SELECT_OPEN) {
				let legacy = parse_legacy_selection(&pattern, operations.len() + 1)?;
				Rewrite::LegacySelection { old: legacy, new: authored_rewrite }
			} else {
				Rewrite::Explicit(authored_rewrite)
			}
		} else {
			let selections = parse_inline(&pattern, operations.len() + 1)?;
			if selections.is_empty() {
				return Err(SloppyError::Malformed {
					operation: operations.len() + 1,
					reason:    "operation needs a block rewrite or inline selection",
				});
			}
			Rewrite::Inline(selections)
		};
		operations.push(Operation { all, ordinal, pattern, rewrite });
	}
	Ok(operations)
}

fn parse_legacy_selection(pattern: &str, operation: usize) -> Result<String, SloppyError> {
	let Some(open) = pattern.find(SELECT_OPEN) else {
		return Err(SloppyError::Malformed { operation, reason: "selection is missing its opener" });
	};
	let after_open = &pattern[open + SELECT_OPEN.len()..];
	let Some(close) = after_open.find(SELECT_CLOSE) else {
		return Err(SloppyError::Malformed { operation, reason: "selection is missing its closer" });
	};
	let selected = &after_open[..close];
	if selected.contains(SELECT_DIVIDER)
		|| after_open[close + SELECT_CLOSE.len()..].contains(SELECT_OPEN)
	{
		return Err(SloppyError::Malformed {
			operation,
			reason: "block rewrite may accompany exactly one legacy current-text selection",
		});
	}
	Ok(selected.to_owned())
}

fn is_opener(line: &str) -> bool {
	let line = line.trim();
	line == OPEN
		|| line == ALL
		|| line
			.strip_prefix(OPEN)
			.is_some_and(|n| n.parse::<usize>().is_ok())
}

fn parse_inline(pattern: &str, operation: usize) -> Result<Vec<Selection>, SloppyError> {
	let mut remaining = pattern;
	let mut selections = Vec::new();
	while let Some(open) = remaining.find(SELECT_OPEN) {
		let after_open = &remaining[open + SELECT_OPEN.len()..];
		let Some(close) = after_open.find(SELECT_CLOSE) else {
			return Err(SloppyError::Malformed { operation, reason: "selection is missing ⟫" });
		};
		let selection = &after_open[..close];
		let Some((old, new)) = selection.split_once(SELECT_DIVIDER) else {
			return Err(SloppyError::Malformed { operation, reason: "selection is missing │" });
		};
		if new.contains(SELECT_DIVIDER) {
			return Err(SloppyError::Malformed {
				operation,
				reason: "selection has multiple │ markers",
			});
		}
		selections.push(Selection { old: old.to_owned(), new: new.to_owned() });
		remaining = &after_open[close + SELECT_CLOSE.len()..];
	}
	Ok(selections)
}

#[derive(Clone, Debug)]
struct Pattern {
	literals:     Vec<String>,
	leading_gap:  bool,
	trailing_gap: bool,
}

fn compile_pattern(operation: &Operation) -> Pattern {
	let source = match &operation.rewrite {
		Rewrite::Explicit(_) => operation.pattern.clone(),
		Rewrite::LegacySelection { .. } | Rewrite::Inline(_) => {
			strip_selection_desired(&operation.pattern)
		},
	};
	let leading_gap = source.starts_with(GAP);
	let trailing_gap = source.ends_with(GAP);
	Pattern {
		literals: source.split(GAP).map(ToOwned::to_owned).collect(),
		leading_gap,
		trailing_gap,
	}
}

fn strip_selection_desired(pattern: &str) -> String {
	let mut output = String::with_capacity(pattern.len());
	let mut remaining = pattern;
	while let Some(open) = remaining.find(SELECT_OPEN) {
		output.push_str(&remaining[..open]);
		let after_open = &remaining[open + SELECT_OPEN.len()..];
		let Some(close) = after_open.find(SELECT_CLOSE) else {
			output.push_str(&remaining[open..]);
			return output;
		};
		let selection = &after_open[..close];
		if let Some((old, _)) = selection.split_once(SELECT_DIVIDER) {
			output.push_str(old);
		} else {
			output.push_str(selection);
		}
		remaining = &after_open[close + SELECT_CLOSE.len()..];
	}
	output.push_str(remaining);
	output
}

#[derive(Clone, Debug)]
struct Candidate {
	start:    usize,
	end:      usize,
	captures: Vec<String>,
}

/// Applies one section atomically. The returned string is the complete new
/// source; errors never expose partially applied operations.
pub fn apply_sloppy(source: &str, input: &str) -> Result<String, SloppyError> {
	let operations = parse_operations(input)?;
	let mut content = source.to_owned();
	let mut removed = Vec::<Option<String>>::with_capacity(operations.len());
	for (index, operation) in operations.iter().enumerate() {
		let operation_number = index + 1;
		let pattern = compile_pattern(operation);
		let mut candidates = locate(&content, &pattern);
		if candidates.is_empty() {
			candidates = locate_fuzzy_lines(&content, &pattern);
		}
		if candidates.is_empty() {
			return Err(SloppyError::NoMatch { operation: operation_number });
		}
		let selected = if operation.all {
			candidates
		} else if let Some(ordinal) = operation.ordinal {
			let Some(candidate) = candidates.get(ordinal.saturating_sub(1)).cloned() else {
				return Err(SloppyError::NoMatch { operation: operation_number });
			};
			vec![candidate]
		} else if candidates.len() == 1 {
			candidates
		} else {
			return Err(SloppyError::Ambiguous {
				operation: operation_number,
				lines:     candidates
					.iter()
					.map(|candidate| line_at(&content, candidate.start))
					.collect(),
			});
		};
		if selected.windows(2).any(|pair| pair[0].end > pair[1].start) {
			return Err(SloppyError::Overlap { operation: operation_number });
		}
		let mut changed = false;
		let mut first_removed = None;
		for candidate in selected.into_iter().rev() {
			let replacement = render_rewrite(
				&content[candidate.start..candidate.end],
				&candidate.captures,
				&operation.rewrite,
				&removed,
				operation_number,
			)?;
			if replacement != content[candidate.start..candidate.end] {
				changed = true;
			}
			first_removed = Some(content[candidate.start..candidate.end].to_owned());
			content.replace_range(candidate.start..candidate.end, &replacement);
		}
		if !changed {
			return Err(SloppyError::NoChange { operation: operation_number });
		}
		removed.push(first_removed);
	}
	Ok(content)
}

fn locate(content: &str, pattern: &Pattern) -> Vec<Candidate> {
	let literals = pattern
		.literals
		.iter()
		.enumerate()
		.filter(|(_, literal)| !literal.is_empty())
		.collect::<Vec<_>>();
	if literals.is_empty() {
		return vec![Candidate {
			start:    content.len(),
			end:      content.len(),
			captures: Vec::new(),
		}];
	}
	let (_, first) = literals[0];
	let mut candidates = Vec::new();
	for (first_start, _) in content.match_indices(first).take(MAX_CANDIDATES) {
		let mut positions = vec![(first_start, first_start + first.len())];
		let mut cursor = first_start + first.len();
		let mut matched = true;
		for (_, literal) in literals.iter().skip(1) {
			if let Some(relative) = content[cursor..].find(literal.as_str()) {
				let start = cursor + relative;
				positions.push((start, start + literal.len()));
				cursor = start + literal.len();
			} else {
				matched = false;
				break;
			}
		}
		if !matched {
			continue;
		}
		let start = if pattern.leading_gap {
			line_start(content, first_start)
		} else {
			first_start
		};
		let end = if pattern.trailing_gap {
			line_end(content, cursor)
		} else {
			cursor
		};
		let mut captures = Vec::new();
		for pair in positions.windows(2) {
			captures.push(content[pair[0].1..pair[1].0].to_owned());
		}
		if pattern.leading_gap {
			captures.insert(0, content[start..first_start].to_owned());
		}
		if pattern.trailing_gap {
			captures.push(content[cursor..end].to_owned());
		}
		candidates.push(Candidate { start, end, captures });
	}
	candidates
}

fn locate_fuzzy_lines(content: &str, pattern: &Pattern) -> Vec<Candidate> {
	if pattern.leading_gap || pattern.trailing_gap || pattern.literals.len() != 1 {
		return Vec::new();
	}
	let expected = pattern.literals[0].lines().collect::<Vec<_>>();
	if expected.is_empty() {
		return Vec::new();
	}
	let starts = line_offsets(content);
	let actual = content.lines().collect::<Vec<_>>();
	let mut candidates = Vec::new();
	for row in 0..=actual.len().saturating_sub(expected.len()) {
		if expected
			.iter()
			.zip(&actual[row..row + expected.len()])
			.all(|(left, right)| left.trim() == right.trim())
		{
			let start = starts[row];
			let end = if row + expected.len() < starts.len() {
				starts[row + expected.len()].saturating_sub(1)
			} else {
				content.len()
			};
			candidates.push(Candidate { start, end, captures: Vec::new() });
		}
	}
	candidates
}

fn render_rewrite(
	matched: &str,
	captures: &[String],
	rewrite: &Rewrite,
	removed: &[Option<String>],
	operation: usize,
) -> Result<String, SloppyError> {
	match rewrite {
		Rewrite::Explicit(rewrite) => {
			let mut output = String::new();
			let mut capture = 0;
			for (line_index, line) in rewrite.lines().enumerate() {
				if let Some(reference) = line
					.trim()
					.strip_prefix(REWRITE)
					.and_then(|n| n.parse::<usize>().ok())
				{
					let Some(text) = reference
						.checked_sub(1)
						.and_then(|index| removed.get(index))
						.and_then(Option::as_ref)
					else {
						return Err(SloppyError::InvalidReference { operation, reference });
					};
					output.push_str(text);
				} else {
					let mut remaining = line;
					while let Some(gap) = remaining.find(GAP) {
						output.push_str(&remaining[..gap]);
						if let Some(value) = captures.get(capture) {
							output.push_str(value);
							capture += 1;
						} else {
							output.push_str(GAP);
						}
						remaining = &remaining[gap + GAP.len()..];
					}
					output.push_str(remaining);
				}
				if line_index + 1 < rewrite.lines().count() {
					output.push('\n');
				}
			}
			Ok(adapt_rewrite_indent(matched, &output))
		},
		Rewrite::LegacySelection { old, new } => {
			replace_selection_pattern(matched, old, new, operation)
		},
		Rewrite::Inline(selections) => {
			let mut output = matched.to_owned();
			let mut cursor = 0;
			for selection in selections {
				if selection.old.is_empty() {
					output.insert_str(cursor.min(output.len()), &selection.new);
					cursor = cursor.saturating_add(selection.new.len());
					continue;
				}
				let replacement =
					replace_selection_pattern(&output, &selection.old, &selection.new, operation)?;
				if replacement == output {
					return Err(SloppyError::NoChange { operation });
				}
				output = replacement;
				cursor = output.len();
			}
			Ok(output)
		},
	}
}

fn replace_selection_pattern(
	content: &str,
	old: &str,
	new: &str,
	operation: usize,
) -> Result<String, SloppyError> {
	let pattern = Pattern {
		literals:     old.split(GAP).map(ToOwned::to_owned).collect(),
		leading_gap:  old.starts_with(GAP),
		trailing_gap: old.ends_with(GAP),
	};
	let candidates = locate(content, &pattern);
	let [candidate] = candidates.as_slice() else {
		return if candidates.is_empty() {
			Err(SloppyError::NoMatch { operation })
		} else {
			Err(SloppyError::Ambiguous {
				operation,
				lines: candidates
					.iter()
					.map(|candidate| line_at(content, candidate.start))
					.collect(),
			})
		};
	};
	let explicit = Rewrite::Explicit(new.to_owned());
	let replacement = render_rewrite(
		&content[candidate.start..candidate.end],
		&candidate.captures,
		&explicit,
		&[],
		operation,
	)?;
	let mut output = content.to_owned();
	output.replace_range(candidate.start..candidate.end, &replacement);
	Ok(output)
}

fn adapt_rewrite_indent(matched: &str, rewrite: &str) -> String {
	let indent = matched
		.lines()
		.find(|line| !line.trim().is_empty())
		.map_or("", |line| &line[..line.len() - line.trim_start().len()]);
	if indent.is_empty() {
		return rewrite.to_owned();
	}
	rewrite
		.lines()
		.enumerate()
		.map(|(index, line)| {
			if index == 0 || line.trim().is_empty() || line.starts_with([' ', '\t']) {
				line.to_owned()
			} else {
				format!("{indent}{line}")
			}
		})
		.collect::<Vec<_>>()
		.join("\n")
}

fn line_offsets(content: &str) -> Vec<usize> {
	let mut offsets = vec![0];
	offsets.extend(content.match_indices('\n').map(|(index, _)| index + 1));
	offsets
}

fn line_at(content: &str, offset: usize) -> usize {
	content[..offset.min(content.len())]
		.bytes()
		.filter(|byte| *byte == b'\n')
		.count()
		+ 1
}

fn line_start(content: &str, offset: usize) -> usize {
	content[..offset].rfind('\n').map_or(0, |index| index + 1)
}

fn line_end(content: &str, offset: usize) -> usize {
	content[offset..]
		.find('\n')
		.map_or(content.len(), |index| offset + index)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn splits_and_strips_envelopes() {
		let sections =
			split_sloppy_sections("*** Begin Patch\n[a]\n«\nx\n»\ny\n[b]\n«\nz⟪z│q⟫\n*** End Patch")
				.expect("sections");
		assert_eq!(sections.len(), 2);
		assert_eq!(sections[0].path, "a");
	}

	#[test]
	fn legacy_selection_replaces_only_selected_text() {
		let source = "const timeout = readConfig().timeout ?? 1000;\nrun(timeout);\n";
		let input = format!(
			"{OPEN}\ntimeout = \
			 {GAP}{SELECT_OPEN}1000{SELECT_CLOSE}{GAP}\nrun(timeout)\n{REWRITE}\n5000"
		);
		assert_eq!(
			apply_sloppy(source, &input).expect("legacy selection"),
			"const timeout = readConfig().timeout ?? 5000;\nrun(timeout);\n"
		);
	}

	#[test]
	fn inline_replacements_apply_each_selection() {
		let source = "const timeout = 1000;\nconst retries = 3;\n";
		let input = format!(
			"{OPEN}\nconst timeout = {SELECT_OPEN}1000{SELECT_DIVIDER}5000{SELECT_CLOSE};\nconst \
			 retries = {SELECT_OPEN}3{SELECT_DIVIDER}5{SELECT_CLOSE};"
		);
		assert_eq!(
			apply_sloppy(source, &input).expect("inline selections"),
			"const timeout = 5000;\nconst retries = 5;\n"
		);
	}

	#[test]
	fn inline_selection_reemits_local_gap() {
		let source = "const value = oldCall(options);\nreport(value);\n";
		let input = format!(
			"{OPEN}\nconst value = {SELECT_OPEN}oldCall({GAP}){SELECT_DIVIDER}newCall({GAP}) ?? \
			 fallback{SELECT_CLOSE};\nreport(value)"
		);
		assert_eq!(
			apply_sloppy(source, &input).expect("selection gap"),
			"const value = newCall(options) ?? fallback;\nreport(value);\n"
		);
	}

	#[test]
	fn block_gap_and_inline_edits_apply() {
		let source = "fn x() {\n    old\n    keep\n}\n";
		let input = "«\nfn x() {\n…\n}\n»\nfn x() {\n…\n    newer\n}\n«\n⟪newer│newest⟫";
		let changed = apply_sloppy(source, input).expect("apply");
		assert!(changed.contains("newest"));
	}

	#[test]
	fn unique_is_required_unless_all() {
		assert!(matches!(apply_sloppy("x\nx\n", "«\nx\n»\ny"), Err(SloppyError::Ambiguous { .. })));
		assert_eq!(apply_sloppy("x\nx\n", "«*\nx\n»\ny").expect("all"), "y\ny\n");
	}

	#[test]
	fn backward_reference_reemits_deleted_text() {
		let changed = apply_sloppy("a\nb\n", "«\na\n»\nA\n«\nb\n»\n»1").expect("apply");
		assert_eq!(changed, "A\na\n");
	}

	#[test]
	fn failures_are_atomic() {
		let source = "a\nb\n";
		let error =
			apply_sloppy(source, "«\na\n»\nA\n«\nmissing\n»\nM").expect_err("second op fails");
		assert!(matches!(error, SloppyError::NoMatch { operation: 2 }));
		assert_eq!(source, "a\nb\n");
	}
}
