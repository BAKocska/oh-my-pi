//! Post-render prompt canonicalization: HTML-comment stripping, blank-run
//! collapse, GFM table compaction, and RFC 2119 aliasing.
//!
//! Opt-in by design — [`crate::Template::render`] never applies these.
//! System-prompt assembly canonicalizes before hashing and paragraph dedup;
//! command templates do not. Fenced code blocks and inline code spans are
//! never rewritten.

/// Canonicalizes a whole prompt document.
///
/// Outside code fences: strips `<!-- -->` comments (including multi-line),
/// trims trailing whitespace, collapses blank-line runs to one, applies
/// [`canonicalize_text_line`] per line, and drops trailing newlines. Fenced
/// blocks (backtick or tilde fences) pass through untouched.
pub fn canonicalize_prompt(content: &str) -> String {
	let mut out = String::with_capacity(content.len());
	let mut in_fence = false;
	let mut in_comment = false;
	let mut blank = false;
	for raw_line in content.lines() {
		let trimmed = raw_line.trim_end();
		let fence = trimmed.trim_start();
		if fence.starts_with("```") || fence.starts_with("~~~") {
			in_fence = !in_fence;
			push_canonical_line(&mut out, raw_line, &mut blank);
			continue;
		}
		if in_fence {
			push_canonical_line(&mut out, raw_line, &mut blank);
			continue;
		}

		let mut line = String::with_capacity(trimmed.len());
		let mut rest = trimmed;
		loop {
			if in_comment {
				let Some(end) = rest.find("-->") else {
					break;
				};
				rest = &rest[end + 3..];
				in_comment = false;
			}
			let Some(start) = rest.find("<!--") else {
				line.push_str(rest);
				break;
			};
			line.push_str(&rest[..start]);
			rest = &rest[start + 4..];
			in_comment = true;
		}
		let line = canonicalize_text_line(line.trim_end());
		push_canonical_line(&mut out, &line, &mut blank);
	}
	while out.ends_with('\n') {
		out.pop();
	}
	out
}

fn push_canonical_line(out: &mut String, line: &str, blank: &mut bool) {
	if line.trim().is_empty() {
		*blank = !out.is_empty();
		return;
	}
	if *blank {
		out.push_str("\n\n");
	} else if !out.is_empty() {
		out.push('\n');
	}
	out.push_str(line);
	*blank = false;
}

/// Canonicalizes one line: GFM table rows get their cells trimmed and
/// separator cells compacted to `---`/`:---`/`---:`/`:---:`; every line then
/// goes through [`canonicalize_inline`].
pub fn canonicalize_text_line(line: &str) -> String {
	let trimmed = line.trim_start();
	let indent = &line[..line.len() - trimmed.len()];
	if trimmed.starts_with('|') && trimmed.ends_with('|') {
		let mut compact = String::with_capacity(line.len());
		compact.push_str(indent);
		for (index, cell) in trimmed.split('|').enumerate() {
			if index > 0 {
				compact.push('|');
			}
			let cell = cell.trim();
			if !cell.is_empty()
				&& cell
					.chars()
					.all(|character| matches!(character, '-' | ':' | ' '))
			{
				let left = cell.starts_with(':');
				let right = cell.ends_with(':');
				match (left, right) {
					(true, true) => compact.push_str(":---:"),
					(true, false) => compact.push_str(":---"),
					(false, true) => compact.push_str("---:"),
					(false, false) => compact.push_str("---"),
				}
			} else {
				compact.push_str(cell);
			}
		}
		return canonicalize_inline(&compact);
	}
	canonicalize_inline(line)
}

/// Aliases RFC 2119 phrasing (`MUST NOT` → `NEVER`, bold markers dropped)
/// and substitutes arrow/operator ASCII digraphs with Unicode, skipping
/// inline code spans.
pub fn canonicalize_inline(line: &str) -> String {
	let mut out = String::with_capacity(line.len());
	for (index, segment) in line.split('`').enumerate() {
		if index > 0 {
			out.push('`');
		}
		if index % 2 == 1 {
			out.push_str(segment);
			continue;
		}
		let segment = segment
			.replace("**MUST NOT**", "NEVER")
			.replace("**SHOULD NOT**", "AVOID")
			.replace("MUST NOT", "NEVER")
			.replace("SHOULD NOT", "AVOID")
			.replace("**MUST**", "MUST")
			.replace("**SHOULD**", "SHOULD")
			.replace("**REQUIRED**", "REQUIRED")
			.replace("**RECOMMENDED**", "RECOMMENDED")
			.replace("**MAY**", "MAY")
			.replace("**OPTIONAL**", "OPTIONAL")
			.replace("**NEVER**", "NEVER")
			.replace("**AVOID**", "AVOID")
			.replace("<->", "↔")
			.replace("->", "→")
			.replace("<-", "←")
			.replace("!=", "≠")
			.replace("<=", "≤")
			.replace(">=", "≥")
			.replace("...", "…");
		out.push_str(&segment);
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn canonicalization_never_rewrites_fenced_or_inline_code() {
		let input = "MUST NOT change `a -> b`.\n\n```text\nMUST NOT  \n\nx -> y\n```\n";
		assert_eq!(
			canonicalize_prompt(input),
			"NEVER change `a -> b`.\n\n```text\nMUST NOT  \n\nx -> y\n```"
		);
	}

	#[test]
	fn comments_are_stripped_and_blank_runs_collapse() {
		let input = "keep <!-- drop\nstill dropped --> tail\n\n\n\nnext";
		assert_eq!(canonicalize_prompt(input), "keep\n tail\n\nnext");
	}

	#[test]
	fn table_separators_compact_with_alignment_preserved() {
		assert_eq!(
			canonicalize_text_line("| :---- |  ----:  | :--: | ---- |"),
			"|:---|---:|:---:|---|"
		);
		assert_eq!(canonicalize_text_line("| a  |  b |"), "|a|b|");
	}
}
