//! Scope model shared by fix planning and name-binding collection.
//!
//! A scope is identified by the byte offset of its module item list or block
//! statement list ([`ScopeKey::ROOT`] = file top level). Block scopes nest
//! visibly, but a module does *not* inherit its parent's imports — visibility
//! chains stop at the first module item list.

use ra_ap_syntax::{
	SyntaxKind, SyntaxNode,
	ast::{self, AstNode},
};

/// Opaque identity of one module or block scope within a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ScopeKey(usize);

impl ScopeKey {
	/// The file's top-level scope.
	pub const ROOT: Self = Self(0);
}

/// Whether `kind` opens a scope of its own.
fn is_scope(kind: SyntaxKind) -> bool {
	matches!(kind, SyntaxKind::ITEM_LIST | SyntaxKind::STMT_LIST)
}

/// Visibility chain for `node`, innermost first, ending at the first module
/// item list (or the file root).
pub fn chain(node: &SyntaxNode) -> Vec<ScopeKey> {
	let mut scopes = Vec::new();
	for a in node.ancestors() {
		match a.kind() {
			SyntaxKind::STMT_LIST => scopes.push(ScopeKey(a.text_range().start().into())),
			SyntaxKind::ITEM_LIST => {
				scopes.push(ScopeKey(a.text_range().start().into()));
				return scopes;
			},
			_ => {},
		}
	}
	scopes.push(ScopeKey::ROOT);
	scopes
}

/// Scope *containing* `item` (skips the item itself).
pub fn containing(item: &SyntaxNode) -> ScopeKey {
	item
		.ancestors()
		.skip(1)
		.find(|a| is_scope(a.kind()))
		.map_or(ScopeKey::ROOT, |a| ScopeKey(a.text_range().start().into()))
}

/// Start-of-line offset where a new `use` belongs for the module scope
/// enclosing `node`: after the scope's last existing `use`, else at the
/// scope's first item.
pub fn use_insertion_offset(node: &SyntaxNode, text: &str) -> usize {
	let scope_items: Vec<SyntaxNode> = node
		.ancestors()
		.find_map(|a| ast::ItemList::cast(a.clone()).map(|il| il.syntax().clone()))
		.or_else(|| node.ancestors().last())
		.map(|scope| {
			scope
				.children()
				.filter(|c| ast::Item::can_cast(c.kind()))
				.collect()
		})
		.unwrap_or_default();
	let last_use = scope_items
		.iter()
		.rev()
		.find(|c| c.kind() == SyntaxKind::USE);
	if let Some(u) = last_use {
		let end: usize = u.text_range().end().into();
		// Move to the start of the next line.
		return text[end..].find('\n').map_or(text.len(), |i| end + i + 1);
	}
	if let Some(first) = scope_items.first() {
		let start: usize = first.text_range().start().into();
		// Back up to the start of that line.
		return text[..start].rfind('\n').map_or(0, |i| i + 1);
	}
	0
}
