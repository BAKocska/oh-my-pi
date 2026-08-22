//! Lint and diagnosis contracts plus the erased engine plumbing.
//!
//! A [`Lint`] walks one parsed file and pushes typed findings into a
//! [`RealtimeSink`]; each finding implements [`Diagnosis`], which the sink
//! erases into a [`Diag`] the engine can render and (when a [`PathFix`] is
//! attached) feed to the fixer.

use std::marker::PhantomData;
use std::ops::Range;
use std::path::Path;

use ra_ap_syntax::{Edition, SourceFile};

use crate::fix::PathFix;

/// One parsed file handed to every lint.
pub struct FileContext<'a> {
	/// Path the file was read from, for rendering.
	pub path: &'a Path,
	/// Full source text.
	pub text: &'a str,
	/// Parsed syntax tree (errors tolerated; lints see what parsed).
	pub tree: SourceFile,
	line_starts: Vec<usize>,
}

impl<'a> FileContext<'a> {
	/// Parses `text` (edition 2024) and indexes line starts.
	pub fn new(path: &'a Path, text: &'a str) -> Self {
		let tree = SourceFile::parse(text, Edition::Edition2024).tree();
		let line_starts = std::iter::once(0)
			.chain(text.match_indices('\n').map(|(i, _)| i + 1))
			.collect();
		Self { path, text, tree, line_starts }
	}

	/// 1-based `(line, column)` of a byte offset.
	pub fn position(&self, offset: usize) -> (usize, usize) {
		let line = self.line_starts.partition_point(|&s| s <= offset);
		(line, offset - self.line_starts[line - 1] + 1)
	}

	/// Source text of a 1-based line, without its newline.
	pub fn line(&self, line: usize) -> &str {
		let start = self.line_starts[line - 1];
		let end = self.line_starts.get(line).map_or(self.text.len(), |s| s - 1);
		self.text[start..end].trim_end_matches('\r')
	}
}

/// A single finding a lint reports about one location.
pub trait Diagnosis {
	/// Byte span of the offending code.
	fn span(&self) -> Range<usize>;
	/// One-line description of the problem and the suggested direction.
	fn message(&self) -> String;
	/// Whether [`Diagnosis::fix`] yields a plan.
	fn autofixable(&self) -> bool;
	/// Planned rewrite, when one can be derived safely.
	fn fix(self) -> Option<PathFix>;
}

/// A rule that scans one file and reports typed findings.
pub trait Lint {
	/// Stable rule id shown in output (`long-path`, `mutex-arc`, …).
	const NAME: &'static str;
	/// Finding type this lint produces.
	type Instance: Diagnosis;
	/// Walk `ctx` and push every finding into `sink` as it is discovered.
	fn detect(&self, ctx: &FileContext<'_>, sink: &mut RealtimeSink<'_, Self::Instance>);
}

/// Streaming sink: findings are erased and forwarded the moment they are
/// pushed, so output appears while large trees are still being scanned.
pub struct RealtimeSink<'s, D: Diagnosis> {
	rule: &'static str,
	out: &'s mut dyn FnMut(Diag),
	_instance: PhantomData<D>,
}

impl<D: Diagnosis> RealtimeSink<'_, D> {
	/// Erase and forward one finding.
	pub fn push(&mut self, diagnosis: D) {
		let span = diagnosis.span();
		let message = diagnosis.message();
		let autofixable = diagnosis.autofixable();
		let fix = diagnosis.fix();
		(self.out)(Diag { rule: self.rule, span, message, autofixable, fix });
	}
}

/// Erased finding: what every lint's [`Diagnosis`] reduces to.
pub struct Diag {
	/// Owning rule id.
	pub rule: &'static str,
	/// Byte span of the offending code.
	pub span: Range<usize>,
	/// Rendered message.
	pub message: String,
	/// Whether a fix plan is attached.
	pub autofixable: bool,
	/// The fix plan, consumed by the fixer.
	pub fix: Option<PathFix>,
}

impl Diag {
	/// Pretty-print with the offending source line and a caret underline.
	pub fn render(&self, ctx: &FileContext<'_>) -> String {
		let (line, col) = ctx.position(self.span.start);
		let source = ctx.line(line);
		let note = if self.autofixable { "" } else { " (no autofix)" };
		// Caret line: mirror tabs so the underline aligns under any indent.
		let mut carets = String::with_capacity(source.len());
		for ch in source.chars().take(col - 1) {
			carets.push(if ch == '\t' { '\t' } else { ' ' });
		}
		let width = self.span.len().min(source.len().saturating_sub(col - 1)).max(1);
		carets.extend(std::iter::repeat_n('^', width));
		format!(
			"{}:{line}:{col}: [{}] {}{note}\n{line:>5} | {source}\n      | {carets}",
			ctx.path.display(),
			self.rule,
			self.message,
		)
	}
}

/// Object-safe form of [`Lint`] so the engine can hold a mixed rule set.
pub trait AnyLint {
	/// Run detection, forwarding erased findings to `out`.
	fn detect_erased(&self, ctx: &FileContext<'_>, out: &mut dyn FnMut(Diag));
}

impl<L: Lint> AnyLint for L {
	fn detect_erased(&self, ctx: &FileContext<'_>, out: &mut dyn FnMut(Diag)) {
		let mut sink = RealtimeSink { rule: L::NAME, out, _instance: PhantomData };
		self.detect(ctx, &mut sink);
	}
}
