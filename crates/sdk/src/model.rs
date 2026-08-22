//! Credential-blind semantic model planning.

use std::collections::BTreeMap;

use omp_core::Str;
use omp_llm_catalog::{
	CandidateProvenance, Catalog, ModelRole, ModelScope, SelectionCandidate, SelectionError,
	candidate_plan, pick_default,
};
pub use omp_llm_inference::recovery::dialect::{Dialect, DialectEvent, DialectStage, ToolEnvelope};
use thiserror::Error;

/// Ordered credential-blind fallback plan for one session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelPlan {
	candidates: Box<[SelectionCandidate]>,
}

impl ModelPlan {
	/// Returns candidates in dispatch order.
	pub fn candidates(&self) -> &[SelectionCandidate] {
		&self.candidates
	}

	pub(crate) fn clamp_thinking(&mut self, max_rank: u8) {
		const LEVELS: &[&str] = &["", "minimal", "low", "medium", "high", "xhigh", "max"];
		for candidate in &mut self.candidates {
			let Some(selected) = &mut candidate.selected else {
				continue;
			};
			let Some(thinking) = &selected.thinking else {
				continue;
			};
			let rank = LEVELS
				.iter()
				.position(|level| *level == thinking.as_str())
				.map(|rank| rank as u8);
			if thinking == "auto" || rank.is_some_and(|rank| rank > max_rank) {
				selected.thinking =
					(max_rank != 0).then(|| Str::new_static(LEVELS[usize::from(max_rank)]));
			}
		}
	}
}

/// Failure to compile model selectors or scope patterns.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ModelPlanError {
	/// Catalog selection rejected a selector or scope.
	#[error(transparent)]
	Selection(#[from] SelectionError),
}

/// Resolves an ordered selector chain without consulting credential state.
/// Resolves the catalog's preferred available model.
pub fn default_model_plan(catalog: &Catalog) -> Option<ModelPlan> {
	let selected = pick_default(catalog.models(), catalog.routes(), &BTreeMap::new())?;
	let selector = Str::new(selected.model.as_str());
	Some(ModelPlan {
		candidates: Box::new([SelectionCandidate {
			selector,
			selected: Some(selected),
			provenance: CandidateProvenance::Catalog,
		}]),
	})
}

/// Resolves an ordered selector chain without consulting credential state.
pub fn resolve_model_plan(
	catalog: &Catalog,
	selectors: &[Str],
	roles: &[ModelRole],
	enabled_patterns: &[Str],
) -> Result<ModelPlan, ModelPlanError> {
	let scope = if enabled_patterns.is_empty() {
		None
	} else {
		Some(ModelScope::compile(enabled_patterns)?)
	};
	let candidates = candidate_plan(
		catalog.models(),
		catalog.routes(),
		catalog.aliases(),
		roles,
		&BTreeMap::new(),
		selectors,
		scope.as_ref(),
	)?;
	Ok(ModelPlan { candidates: candidates.into_boxed_slice() })
}
