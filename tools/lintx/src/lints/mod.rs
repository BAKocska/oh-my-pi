//! One module per lint (family); [`all`] assembles the rule set the engine
//! runs.

mod arc_struct;
mod inline_path;
mod mutex_arc;

use crate::lint::AnyLint;

/// Every lint, configured. `max_segments` is the `long-path` threshold.
pub fn all(max_segments: usize) -> Vec<Box<dyn AnyLint>> {
	vec![
		Box::new(inline_path::LongPath { max_segments }),
		Box::new(inline_path::RelativePath),
		Box::new(arc_struct::ArcStruct),
		Box::new(mutex_arc::MutexArc),
	]
}
