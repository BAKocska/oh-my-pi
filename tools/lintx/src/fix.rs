//! Fix planning and whole-file fix application.
//!
//! Individual lints *plan* fixes via [`PathFix::plan`]; application is a
//! file-global transaction over all lints' plans: overlap dropping, admission
//! against the file's [`Bindings`], and one back-to-front splice with grouped
//! `use` inserts. Scope semantics live in [`crate::scope`]; name resolution
//! in [`crate::bindings`].

use std::{
	collections::{BTreeMap, BTreeSet},
	ops::Range,
};

use ra_ap_syntax::ast::{self, AstNode, HasAttrs};

use crate::{
	bindings::Bindings,
	lint::{Diag, FileContext},
	scope::{self, ScopeKey},
};

/// A planned rewrite: replace `range` with `replacement` and import `import`.
pub struct PathFix {
	/// Byte range of the qualified path being shortened.
	range:       Range<usize>,
	/// Source text kept at the use site (tail segments, generics intact).
	replacement: String,
	/// Path the inserted `use` names.
	import:      String,
	/// Offset where the `use` line is inserted (start of a line).
	insert_at:   usize,
	/// Ident the fix brings into scope; used for collision checks.
	binds:       String,
	/// Visibility chain of the fix site, innermost first.
	scopes:      Vec<ScopeKey>,
}

impl PathFix {
	/// Plan a rewrite of `path`: keep its last `keep` segments at the use
	/// site and add `use <import_names>;`. Returns `None` when no safe plan
	/// exists:
	/// - the import path roots at a type or generic parameter
	///   (`SE::ErrorFormatter`) — `use` paths resolve from a crate root;
	/// - the path sits under a `#[cfg]`-gated ancestor — an inserted `use` would
	///   be unconditional while the usage is not.
	pub fn plan(text: &str, path: &ast::Path, import_names: &[String], keep: usize) -> Option<Self> {
		if import_names.len() < 2 {
			return None;
		}
		if import_names[0]
			.chars()
			.next()
			.is_some_and(char::is_uppercase)
		{
			return None;
		}
		if path.syntax().ancestors().any(|a| {
			ast::AnyHasAttrs::cast(a).is_some_and(|item| {
				item
					.attrs()
					.any(|attr| attr.simple_name().as_deref() == Some("cfg"))
			})
		}) {
			return None;
		}
		// Keep the kept segments' *source* text so turbofish/generics survive.
		let segs: Vec<ast::PathSegment> = path.segments().collect();
		let kept_start: usize = segs
			.get(segs.len().checked_sub(keep)?)?
			.syntax()
			.text_range()
			.start()
			.into();
		let range = usize::from(path.syntax().text_range().start())
			..usize::from(path.syntax().text_range().end());
		Some(Self {
			replacement: text[kept_start..range.end].to_string(),
			import: import_names.join("::"),
			insert_at: scope::use_insertion_offset(path.syntax(), text),
			binds: import_names.last()?.clone(),
			scopes: scope::chain(path.syntax()),
			range,
		})
	}
}

/// Apply all planned fixes in `diags` to `ctx`'s text. A fix is dropped when
/// its ident could capture an existing binding or bare usage, or when it
/// nests inside another fix (the next fixpoint pass picks it up). Returns the
/// new text and the number of fixes applied.
pub fn apply(ctx: &FileContext<'_>, diags: Vec<Diag>, own_crate: &str) -> (String, usize) {
	let mut bindings = Bindings::collect(&ctx.tree);
	let planned = drop_nested(diags.into_iter().filter_map(|d| d.fix).collect());
	let admitted = admit(planned, &mut bindings, own_crate);
	splice(ctx.text, admitted)
}

/// A fix whose range is contained in an earlier fix (nested path in generic
/// args) cannot be spliced in the same pass: keep the outer edit, defer the
/// inner. Runs before admission so a dropped fix records no binding.
fn drop_nested(mut planned: Vec<PathFix>) -> Vec<PathFix> {
	planned.sort_by_key(|f| f.range.start);
	let mut last_end = 0usize;
	planned.retain(|f| {
		if f.range.start < last_end {
			return false;
		}
		last_end = f.range.end;
		true
	});
	planned
}

/// Decide, per fix, whether it may bind its ident — and whether it needs a
/// `use` insert (`false` = the identical import is already visible).
fn admit(planned: Vec<PathFix>, bindings: &mut Bindings, own_crate: &str) -> Vec<(PathFix, bool)> {
	let mut admitted = Vec::with_capacity(planned.len());
	for mut fix in planned {
		// A self-referencing crate path must be imported via `crate::`.
		if !own_crate.is_empty() {
			if let Some(rest) = fix.import.strip_prefix(own_crate) {
				if let Some(rest) = rest.strip_prefix("::") {
					fix.import = format!("crate::{rest}");
				}
			}
		}
		// `use` paths resolve from a crate root: a first segment that is
		// itself a local binding (module def, alias) cannot be imported.
		let import_root = fix.import.split("::").next().unwrap_or_default();
		if !matches!(import_root, "crate" | "super" | "self" | "std" | "core" | "alloc")
			&& bindings.binds(&fix.scopes, import_root)
		{
			continue;
		}
		match bindings.visible(&fix.scopes, &fix.binds) {
			// Already bound to the same path in a visible scope: rewrite only.
			Some(existing) if existing == fix.import => admitted.push((fix, false)),
			// Bound to something else (import, alias, or local item): skip.
			Some(_) => {},
			None => {
				if bindings.bare_used(&fix.binds) {
					continue;
				}
				let scope = fix.scopes.last().copied().unwrap_or(ScopeKey::ROOT);
				bindings.record(scope, fix.binds.clone(), fix.import.clone());
				admitted.push((fix, true));
			},
		}
	}
	admitted
}

/// Materialize admitted fixes: path replacements plus grouped, deduped,
/// indent-matched `use` inserts, applied back-to-front over one buffer.
fn splice(text: &str, admitted: Vec<(PathFix, bool)>) -> (String, usize) {
	let mut inserts: BTreeMap<usize, BTreeSet<String>> = BTreeMap::new();
	for (f, needs_insert) in &admitted {
		if *needs_insert {
			inserts
				.entry(f.insert_at)
				.or_default()
				.insert(format!("use {};\n", f.import));
		}
	}

	let applied = admitted.len();
	let mut edits: Vec<(Range<usize>, String)> = admitted
		.into_iter()
		.map(|(f, _)| (f.range, f.replacement))
		.collect();
	for (at, lines) in inserts {
		// Match the indentation of the line we insert before.
		let indent: String = text[at..]
			.chars()
			.take_while(|c| *c == '\t' || *c == ' ')
			.collect();
		let block: String = lines.into_iter().map(|l| format!("{indent}{l}")).collect();
		edits.push((at..at, block));
	}
	edits.sort_by(|a, b| b.0.start.cmp(&a.0.start));
	let mut out = text.to_string();
	for (range, replacement) in edits {
		out.replace_range(range, &replacement);
	}
	(out, applied)
}
