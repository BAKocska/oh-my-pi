//! `import-alias`: CamelCase import aliases hide the defining namespace.

use std::ops::Range;

use ra_ap_syntax::ast::{self, AstNode, HasName};

use crate::{
	fix::PathFix,
	lint::{Diagnosis, FileContext, Lint, RealtimeSink},
};

/// Rejects type-like `use … as Alias` bindings while permitting `_` and module
/// aliases.
pub struct ImportAlias;

/// One type-like import alias.
pub struct Finding {
	span:  Range<usize>,
	alias: String,
}

impl Diagnosis for Finding {
	fn span(&self) -> Range<usize> {
		self.span.clone()
	}

	fn message(&self) -> String {
		format!(
			"`as {}` hides the imported namespace; use the original name or its module",
			self.alias
		)
	}

	fn autofixable(&self) -> bool {
		false
	}

	fn fix(self) -> Option<PathFix> {
		None
	}
}

impl Lint for ImportAlias {
	type Instance = Finding;

	const NAME: &'static str = "import-alias";

	fn detect(&self, ctx: &FileContext<'_>, sink: &mut RealtimeSink<'_, Finding>) {
		for tree in ctx
			.tree
			.syntax()
			.descendants()
			.filter_map(ast::UseTree::cast)
		{
			let Some(alias) = tree.rename().and_then(|rename| rename.name()) else {
				continue;
			};
			let rendered = alias.text().to_string();
			if !is_type_alias(&rendered) {
				continue;
			}
			let range = alias.syntax().text_range();
			sink.push(Finding { span: range.start().into()..range.end().into(), alias: rendered });
		}
	}
}

fn is_type_alias(alias: &str) -> bool {
	alias.chars().next().is_some_and(char::is_uppercase)
}

#[cfg(test)]
mod tests {
	use super::is_type_alias;

	#[test]
	fn rejects_only_type_like_aliases() {
		assert!(is_type_alias("IoError"));
		assert!(is_type_alias("FmtResult"));
		assert!(!is_type_alias("pb"));
		assert!(!is_type_alias("std_thread"));
		assert!(!is_type_alias("_"));
	}
}
