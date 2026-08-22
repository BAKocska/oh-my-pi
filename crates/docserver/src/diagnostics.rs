//! Canonical diagnostics shared by push, pull, checker, and tool surfaces.

use std::collections::{HashMap, HashSet};

use omp_core::Str;
use serde::{Deserialize, Serialize};

/// LSP-compatible zero-based position.
#[derive(
	Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
pub struct Position {
	/// Zero-based line.
	pub line:      u32,
	/// Zero-based UTF code-unit offset, in the server's negotiated encoding.
	pub character: u32,
}

/// LSP-compatible half-open range.
#[derive(
	Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
pub struct Range {
	/// Inclusive start.
	pub start: Position,
	/// Exclusive end.
	pub end:   Position,
}

/// Normalized diagnostic severity, ordered most to least severe.
#[derive(
	Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
	/// Compilation or analysis error.
	#[default]
	Error,
	/// Warning.
	Warning,
	/// Informational finding.
	Information,
	/// Hint.
	Hint,
}

impl Severity {
	/// Converts the LSP numeric severity, treating absent and unknown values as
	/// errors.
	pub const fn from_lsp(value: Option<u64>) -> Self {
		match value {
			Some(2) => Self::Warning,
			Some(3) => Self::Information,
			Some(4) => Self::Hint,
			_ => Self::Error,
		}
	}
}

/// A source-tagged diagnostic independent of its transport.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Diagnostic {
	/// Canonical file URI.
	pub uri:      Str,
	/// Zero-based range.
	pub range:    Range,
	/// Severity.
	pub severity: Severity,
	/// Human-readable message.
	pub message:  Str,
	/// Optional machine code.
	pub code:     Option<Str>,
	/// LSP server, checker, or linter name.
	pub source:   Str,
}

#[derive(Clone, Debug, Deserialize)]
struct LspRange {
	start: Position,
	end:   Position,
}

#[derive(Clone, Debug, Deserialize)]
struct LspDiagnostic {
	range:    LspRange,
	#[serde(default)]
	severity: Option<u64>,
	#[serde(default)]
	code:     Option<serde_json::Value>,
	message:  Str,
	#[serde(default)]
	source:   Option<Str>,
}

#[derive(Clone, Debug, Deserialize)]
struct PublishedDiagnostics {
	uri:         Str,
	#[serde(default)]
	version:     Option<i64>,
	#[serde(default)]
	diagnostics: Vec<LspDiagnostic>,
}

#[derive(Clone, Debug, Deserialize)]
struct PullDiagnostics {
	kind:  Str,
	#[serde(default)]
	items: Vec<LspDiagnostic>,
}

/// Parses an LSP push-diagnostics notification into canonical diagnostics.
pub fn parse_push(
	payload: &[u8],
	binding: &str,
) -> Result<(Str, Option<i64>, Vec<Diagnostic>), serde_json::Error> {
	let published: PublishedDiagnostics = serde_json::from_slice(payload)?;
	let diagnostics = normalize_lsp_items(&published.uri, binding, published.diagnostics);
	Ok((published.uri, published.version, diagnostics))
}

/// Parses a full LSP 3.17 pull-diagnostic report. Unchanged reports return
/// `None`.
pub fn parse_pull(
	uri: Str,
	payload: &[u8],
	binding: &str,
) -> Result<Option<Vec<Diagnostic>>, serde_json::Error> {
	let report: PullDiagnostics = serde_json::from_slice(payload)?;
	if report.kind.as_str() != "full" {
		return Ok(None);
	}
	Ok(Some(normalize_lsp_items(&uri, binding, report.items)))
}

fn normalize_lsp_items(uri: &Str, binding: &str, items: Vec<LspDiagnostic>) -> Vec<Diagnostic> {
	items
		.into_iter()
		.map(|item| Diagnostic {
			uri:      uri.clone(),
			range:    Range { start: item.range.start, end: item.range.end },
			severity: Severity::from_lsp(item.severity),
			message:  item.message,
			code:     item.code.and_then(code_string),
			source:   item.source.unwrap_or_else(|| Str::from(binding)),
		})
		.collect()
}

fn code_string(value: serde_json::Value) -> Option<Str> {
	match value {
		serde_json::Value::String(value) => Some(Str::from(value)),
		serde_json::Value::Number(value) => Some(Str::from(value.to_string())),
		_ => None,
	}
}

/// Removes diagnostics from orphan TypeScript files when their code requires a
/// project.
pub fn filter_orphan_typescript(diagnostics: &mut Vec<Diagnostic>, has_project_root: bool) {
	if has_project_root {
		return;
	}
	const ORPHAN_CODES: [&str; 7] = ["1375", "1378", "2307", "2580", "2591", "2792", "2867"];
	diagnostics.retain(|diagnostic| {
		let typescript = diagnostic.source.eq_ignore_ascii_case("typescript")
			|| diagnostic
				.source
				.to_ascii_lowercase()
				.contains("typescript");
		!typescript
			|| diagnostic
				.code
				.as_deref()
				.is_none_or(|code| !ORPHAN_CODES.contains(&code))
	});
}

/// Deduplicates cross-source findings by range and message, preserving all
/// source names.
pub fn normalize(mut diagnostics: Vec<Diagnostic>) -> Vec<Diagnostic> {
	let mut positions = HashMap::<(Str, Range, Str), usize>::new();
	let mut output = Vec::<Diagnostic>::with_capacity(diagnostics.len());
	for diagnostic in diagnostics.drain(..) {
		let key = (diagnostic.uri.clone(), diagnostic.range, diagnostic.message.clone());
		if let Some(index) = positions.get(&key).copied() {
			let existing = &mut output[index];
			if !existing
				.source
				.split(",")
				.any(|source| source.trim() == diagnostic.source.as_str())
			{
				let mut sources = existing.source.as_str().split(", ").collect::<HashSet<_>>();
				sources.insert(diagnostic.source.as_str());
				let mut sources = sources.into_iter().collect::<Vec<_>>();
				sources.sort_unstable();
				existing.source = Str::from(sources.join(", "));
			}
			continue;
		}
		positions.insert(key, output.len());
		output.push(diagnostic);
	}
	output.sort_by(|left, right| {
		left
			.severity
			.cmp(&right.severity)
			.then_with(|| left.uri.cmp(&right.uri))
			.then_with(|| left.range.cmp(&right.range))
			.then_with(|| left.message.cmp(&right.message))
	});
	output
}
