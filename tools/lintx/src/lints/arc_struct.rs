//! `arc-struct`: a struct whose fields are mostly `Arc`-wrapped usually wants
//! one shared inner (`struct Bla(Arc<BlaInner>)`) instead of per-field arcs.

use std::ops::Range;

use ra_ap_syntax::ast::{self, AstNode, HasName};

use crate::{
	fix::PathFix,
	lint::{Diagnosis, FileContext, Lint, RealtimeSink},
};

/// The lint: fires on ≥3 `Arc` fields covering at least half the struct.
pub struct ArcStruct;

/// One Arc-heavy struct.
pub struct Finding {
	span:  Range<usize>,
	name:  String,
	arced: usize,
	total: usize,
}

impl Diagnosis for Finding {
	fn span(&self) -> Range<usize> {
		self.span.clone()
	}

	fn message(&self) -> String {
		format!(
			"`{name}`: {}/{} fields are Arc-wrapped; consider `struct {name}(Arc<{name}Inner>)`",
			self.arced,
			self.total,
			name = self.name,
		)
	}

	fn autofixable(&self) -> bool {
		false
	}

	fn fix(self) -> Option<PathFix> {
		None
	}
}

impl Lint for ArcStruct {
	type Instance = Finding;

	const NAME: &'static str = "arc-struct";

	fn detect(&self, ctx: &FileContext<'_>, sink: &mut RealtimeSink<'_, Finding>) {
		for strukt in ctx
			.tree
			.syntax()
			.descendants()
			.filter_map(ast::Struct::cast)
		{
			let Some(ast::FieldList::RecordFieldList(fields)) = strukt.field_list() else {
				continue;
			};
			let mut total = 0usize;
			let mut arced = 0usize;
			for field in fields.fields() {
				total += 1;
				let Some(ast::Type::PathType(pt)) = field.ty() else {
					continue;
				};
				let tail = pt
					.path()
					.and_then(|p| p.segment())
					.and_then(|s| s.name_ref())
					.map(|n| n.text().to_string());
				if tail.as_deref() == Some("Arc") {
					arced += 1;
				}
			}
			if arced >= 3 && arced * 2 >= total {
				let range = strukt.syntax().text_range();
				sink.push(Finding {
					span: range.start().into()..range.end().into(),
					name: strukt
						.name()
						.map_or_else(|| "_".into(), |n| n.text().to_string()),
					arced,
					total,
				});
			}
		}
	}
}
