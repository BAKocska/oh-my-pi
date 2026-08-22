//! `long-path` / `relative-path`: inline qualified paths that must be
//! imported instead. One family, one module: candidate discovery, the
//! finding, and its fix heuristic all live here — the two lints differ only
//! in which paths they claim.
//!
//! - `long-path`: more segments than the limit (`std::sync::atomic::AtomicU32`).
//! - `relative-path`: any `crate::` / `super::` / `self::` inline path.

use std::ops::Range;

use ra_ap_syntax::SyntaxKind;
use ra_ap_syntax::ast::{self, AstNode};

use crate::fix::PathFix;
use crate::lint::{Diagnosis, FileContext, Lint, RealtimeSink};

/// The `long-path` lint; `max_segments` is the largest allowed inline
/// segment count (clippy default: 2).
pub struct LongPath {
	/// Paths with more segments than this are flagged.
	pub max_segments: usize,
}

/// The `relative-path` lint; no configuration.
pub struct RelativePath;

/// One inline qualified path that should be an import.
pub struct Finding {
	span: Range<usize>,
	rendered: String,
	segments: usize,
	relative: bool,
	fix: Option<PathFix>,
}

impl Finding {
	fn new(ctx: &FileContext<'_>, path: &ast::Path, names: Vec<String>) -> Self {
		let range = path.syntax().text_range();
		Self {
			span: range.start().into()..range.end().into(),
			fix: plan(ctx, path, &names),
			segments: names.len(),
			relative: is_relative_root(&names[0]),
			rendered: names.join("::"),
		}
	}
}

impl Diagnosis for Finding {
	fn span(&self) -> Range<usize> {
		self.span.clone()
	}

	fn message(&self) -> String {
		if self.relative {
			let root = self.rendered.split("::").next().unwrap_or_default();
			format!("`{}` uses a `{root}::` inline path; import it", self.rendered)
		} else {
			format!("`{}` has {} segments; import it", self.rendered, self.segments)
		}
	}

	fn autofixable(&self) -> bool {
		self.fix.is_some()
	}

	fn fix(self) -> Option<PathFix> {
		self.fix
	}
}

impl Lint for LongPath {
	const NAME: &'static str = "long-path";
	type Instance = Finding;

	fn detect(&self, ctx: &FileContext<'_>, sink: &mut RealtimeSink<'_, Finding>) {
		for (path, names) in inline_paths(ctx) {
			if names.len() > self.max_segments && !is_relative_root(&names[0]) {
				sink.push(Finding::new(ctx, &path, names));
			}
		}
	}
}

impl Lint for RelativePath {
	const NAME: &'static str = "relative-path";
	type Instance = Finding;

	fn detect(&self, ctx: &FileContext<'_>, sink: &mut RealtimeSink<'_, Finding>) {
		for (path, names) in inline_paths(ctx) {
			if is_relative_root(&names[0]) {
				sink.push(Finding::new(ctx, &path, names));
			}
		}
	}
}

/// Names importing which would shadow the std prelude — always keep a
/// qualifier for these instead (`io::Result`, not `Result`).
const PRELUDE: &[&str] = &[
	"Result", "Option", "Some", "None", "Ok", "Err", "Box", "String", "Vec", "Drop", "Send",
	"Sync", "Sized", "Unpin", "Clone", "Copy", "Default", "Eq", "Ord", "PartialEq", "PartialOrd",
	"Debug", "Hash", "Iterator", "IntoIterator", "Extend", "From", "Into", "TryFrom", "TryInto",
	"AsRef", "AsMut", "FnOnce", "FnMut", "Fn", "ToString", "ToOwned",
];

/// Decide what to import and what to keep at the use site, then let the
/// fixer construct the plan.
///
/// Heuristic: a CamelCase tail after a snake_case qualifier is a plain type —
/// import the full path, keep the tail. Anything else (assoc fns, consts,
/// enum variants, module fns) imports the parent and keeps the last two
/// segments, mirroring idiomatic `Error::new` / `consts::PI` call sites.
fn plan(ctx: &FileContext<'_>, path: &ast::Path, names: &[String]) -> Option<PathFix> {
	let last = names.last()?;
	let prev = names.get(names.len().checked_sub(2)?)?;
	let camel_tail = last.chars().next().is_some_and(char::is_uppercase)
		&& last.chars().any(char::is_lowercase);
	let snake_prev = prev.chars().next().is_some_and(char::is_lowercase);
	let (import_names, keep) = if camel_tail && snake_prev && !PRELUDE.contains(&last.as_str()) {
		(names, 1)
	} else {
		if names.len() < 3 {
			// e.g. `super::foo()` — importing `super` alone is meaningless.
			return None;
		}
		(&names[..names.len() - 1], 2)
	};
	PathFix::plan(ctx.text, path, import_names, keep)
}

/// Whether a path roots at the local crate tree rather than an external one.
fn is_relative_root(name: &str) -> bool {
	matches!(name, "crate" | "super" | "self")
}

/// Every topmost inline qualified path in the file with its segment names —
/// use items, visibility restrictions, and unanalyzable shapes excluded.
fn inline_paths<'c>(
	ctx: &'c FileContext<'_>,
) -> impl Iterator<Item = (ast::Path, Vec<String>)> + 'c {
	ctx.tree.syntax().descendants().filter_map(|node| {
		let path = ast::Path::cast(node)?;
		// Only topmost paths: a qualifier's parent is the outer PATH node.
		if path.syntax().parent()?.kind() == SyntaxKind::PATH {
			return None;
		}
		// Skip use items and visibility restrictions (`pub(in crate::x)`).
		if path
			.syntax()
			.ancestors()
			.any(|a| matches!(a.kind(), SyntaxKind::USE | SyntaxKind::VISIBILITY))
		{
			return None;
		}
		let names = segment_names(&path)?;
		(names.len() >= 2).then_some((path, names))
	})
}

/// Segment names of `path`, outermost-first, or `None` when the path has
/// shapes these lints won't touch (type anchors `<T as Tr>::`, generic args
/// in qualifiers).
fn segment_names(path: &ast::Path) -> Option<Vec<String>> {
	let mut names = Vec::new();
	let mut anchored = false;
	for seg in path.segments() {
		if seg.type_anchor().is_some() {
			anchored = true;
			continue;
		}
		if let Some(name) = seg.name_ref() {
			names.push(name.text().to_string());
		} else if seg.crate_token().is_some() {
			names.push("crate".into());
		} else if seg.super_token().is_some() {
			names.push("super".into());
		} else if seg.self_token().is_some() {
			names.push("self".into());
		} else {
			return None;
		}
	}
	if anchored && names.len() <= 1 {
		return None;
	}
	Some(names)
}
