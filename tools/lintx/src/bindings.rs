//! Scope-aware model of the names a file already binds.
//!
//! Collected once per fix pass from use-tree leaves (aliases included, `as _`
//! excluded), local item definitions, and bare single-segment path usages.
//! The fixer admits a fix only when its ident is unbound and unused, or
//! already bound to the identical import.

use std::collections::{BTreeMap, BTreeSet};

use ra_ap_syntax::{
	SourceFile, SyntaxKind, SyntaxNode,
	ast::{self, AstNode, HasName},
};

use crate::scope::{self, ScopeKey};

/// Canonical path recorded for names bound by local item definitions; never
/// equal to a real import, so such names always refuse new imports.
const LOCAL_DEF: &str = "<def>";

/// What each scope of a file binds, plus every bare ident usage.
pub struct Bindings {
	by_scope: BTreeMap<ScopeKey, BTreeMap<String, String>>,
	bare:     BTreeSet<String>,
}

impl Bindings {
	/// Walk `file` once and record all bindings and bare usages.
	pub fn collect(file: &SourceFile) -> Self {
		let mut by_scope: BTreeMap<ScopeKey, BTreeMap<String, String>> = BTreeMap::new();
		let mut bare: BTreeSet<String> = BTreeSet::new();
		for node in file.syntax().descendants() {
			if let Some(use_item) = ast::Use::cast(node.clone()) {
				if let Some(tree) = use_item.use_tree() {
					collect_use_tree(&tree, "", by_scope.entry(scope::containing(&node)).or_default());
				}
			} else if let Some(path) = ast::Path::cast(node.clone()) {
				if path.qualifier().is_none()
					&& path
						.syntax()
						.parent()
						.is_some_and(|p| p.kind() != SyntaxKind::PATH)
					&& !path
						.syntax()
						.ancestors()
						.any(|a| a.kind() == SyntaxKind::USE)
					&& let Some(name) = path.segment().and_then(|s| s.name_ref())
				{
					bare.insert(name.text().to_string());
				}
			} else if let Some(name) = item_def_name(&node) {
				by_scope
					.entry(scope::containing(&node))
					.or_default()
					.entry(name)
					.or_insert_with(|| LOCAL_DEF.into());
			}
		}
		Self { by_scope, bare }
	}

	/// Innermost binding of `name` visible from `chain`, mirroring shadowing.
	/// Local item definitions answer with a marker that never equals an
	/// import path.
	pub fn visible(&self, chain: &[ScopeKey], name: &str) -> Option<&str> {
		chain
			.iter()
			.find_map(|s| self.by_scope.get(s).and_then(|names| names.get(name)))
			.map(String::as_str)
	}

	/// Whether any scope visible from `chain` binds `name` at all.
	pub fn binds(&self, chain: &[ScopeKey], name: &str) -> bool {
		self.visible(chain, name).is_some()
	}

	/// Whether `name` already occurs as a bare single-segment path anywhere
	/// in the file (glob import, prelude, or macro provenance — importing it
	/// could capture those usages).
	pub fn bare_used(&self, name: &str) -> bool {
		self.bare.contains(name)
	}

	/// Record an admitted import so later fixes in the same pass see it.
	pub fn record(&mut self, scope: ScopeKey, name: String, import: String) {
		self.by_scope.entry(scope).or_default().insert(name, import);
	}
}

/// Name introduced by a module-level item definition, if `node` is one.
fn item_def_name(node: &SyntaxNode) -> Option<String> {
	macro_rules! named {
		($($ty:ident),+) => {
			$(if let Some(item) = ast::$ty::cast(node.clone()) {
				return item.name().map(|n| n.text().to_string());
			})+
		};
	}
	named!(Struct, Enum, Union, Trait, TypeAlias, Fn, Const, Static, Module, MacroRules, MacroDef);
	None
}

/// Record every leaf of a use tree as `name` → canonical path.
fn collect_use_tree(tree: &ast::UseTree, prefix: &str, bound: &mut BTreeMap<String, String>) {
	let path_text = tree
		.path()
		.map(|p| p.syntax().text().to_string())
		.unwrap_or_default();
	let full = match (prefix.is_empty(), path_text.is_empty()) {
		(true, _) => path_text.clone(),
		(false, true) => prefix.to_string(),
		(false, false) => format!("{prefix}::{path_text}"),
	};
	if let Some(list) = tree.use_tree_list() {
		for child in list.use_trees() {
			collect_use_tree(&child, &full, bound);
		}
		return;
	}
	if tree.star_token().is_some() {
		return; // glob: the bare-usage check guards capture
	}
	let (name, canonical) = if let Some(alias) = tree.rename().and_then(|r| r.name()) {
		(alias.text().to_string(), full.clone())
	} else if tree.rename().is_some() {
		return; // `as _` imports a trait anonymously; it binds no name
	} else if full.ends_with("::self") || full == "self" {
		let canonical = full
			.trim_end_matches("::self")
			.trim_end_matches("self")
			.to_string();
		let name = canonical
			.rsplit("::")
			.next()
			.unwrap_or(&canonical)
			.to_string();
		(name, canonical)
	} else {
		(full.rsplit("::").next().unwrap_or(&full).to_string(), full.clone())
	};
	if !name.is_empty() && name != "_" {
		bound.insert(name, canonical.trim_start_matches("::").to_string());
	}
}
