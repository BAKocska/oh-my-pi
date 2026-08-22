//! `mutex-arc`: `Mutex<Arc<T>>` / `RwLock<Option<Arc<T>>>` guard a swappable
//! handle — `arc_swap::ArcSwap` / `ArcSwapOption` do that lock-free.

use std::ops::Range;

use ra_ap_syntax::ast::{self, AstNode, HasGenericArgs};

use crate::fix::PathFix;
use crate::lint::{Diagnosis, FileContext, Lint, RealtimeSink};

/// The lint; no configuration.
pub struct MutexArc;

/// One lock-around-a-swappable-handle type.
pub struct Finding {
	span: Range<usize>,
	shown: String,
	replacement: &'static str,
}

impl Diagnosis for Finding {
	fn span(&self) -> Range<usize> {
		self.span.clone()
	}

	fn message(&self) -> String {
		format!("`{}`: lock around a swappable handle; consider `{}`", self.shown, self.replacement)
	}

	fn autofixable(&self) -> bool {
		false
	}

	fn fix(self) -> Option<PathFix> {
		None
	}
}

impl Lint for MutexArc {
	const NAME: &'static str = "mutex-arc";
	type Instance = Finding;

	fn detect(&self, ctx: &FileContext<'_>, sink: &mut RealtimeSink<'_, Finding>) {
		for pt in ctx.tree.syntax().descendants().filter_map(ast::PathType::cast) {
			let Some(lock) = tail_name(&pt) else { continue };
			if lock != "Mutex" && lock != "RwLock" {
				continue;
			}
			let Some(inner) = first_type_arg(&pt) else { continue };
			let replacement = match type_tail(&inner).as_deref() {
				Some("Arc") => "arc_swap::ArcSwap",
				Some("Option") => {
					let ast::Type::PathType(opt) = &inner else { continue };
					match first_type_arg(opt).as_ref().and_then(type_tail).as_deref() {
						Some("Arc") => "arc_swap::ArcSwapOption",
						_ => continue,
					}
				}
				_ => continue,
			};
			let range = pt.syntax().text_range();
			sink.push(Finding {
				span: range.start().into()..range.end().into(),
				shown: pt.syntax().text().to_string().chars().take(60).collect(),
				replacement,
			});
		}
	}
}

/// Tail identifier of a path type (`Mutex` for `parking_lot::Mutex<T>`).
fn tail_name(pt: &ast::PathType) -> Option<String> {
	pt.path()?.segment()?.name_ref().map(|n| n.text().to_string())
}

/// Tail identifier of a type's path (`Arc` for `std::sync::Arc<T>`).
fn type_tail(ty: &ast::Type) -> Option<String> {
	let ast::Type::PathType(pt) = ty else { return None };
	tail_name(pt)
}

/// First type argument of a path type's tail segment.
fn first_type_arg(pt: &ast::PathType) -> Option<ast::Type> {
	pt.path()?.segment()?.generic_arg_list()?.generic_args().find_map(|arg| match arg {
		ast::GenericArg::TypeArg(t) => t.ty(),
		_ => None,
	})
}
