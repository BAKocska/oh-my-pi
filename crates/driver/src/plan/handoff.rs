//! Approved-plan reference supplied to task children.

use omp_core::Str;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{PlanArtifact, PlanState, PlanWorkflow, artifacts::canonical_url};

/// One approved plan authority passed to a task child.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OverallPlanReference {
	/// Canonical session-local artifact URL.
	pub artifact: Str,
	/// Human-facing normalized title.
	pub title:    Str,
	/// Approved execution topology.
	pub workflow: PlanWorkflow,
}

/// Refusal to hand a draft or mismatched plan to task execution.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum PlanHandoffError {
	/// Plan mode is still active, so the artifact is a draft.
	#[error("cannot hand off a draft while plan mode is active")]
	Draft,
	/// The resolved artifact does not match the approved state reference.
	#[error("resolved plan does not match the approved plan reference")]
	ReferenceMismatch,
}

impl OverallPlanReference {
	/// Resolves exactly one approved plan reference. Approval must disable plan
	/// mode before any task child can receive the plan.
	pub fn resolve(state: &PlanState, artifact: &PlanArtifact) -> Result<Self, PlanHandoffError> {
		if state.enabled {
			return Err(PlanHandoffError::Draft);
		}
		let state_url =
			canonical_url(state.artifact.as_str()).map_err(|_| PlanHandoffError::ReferenceMismatch)?;
		let artifact_url =
			canonical_url(artifact.url.as_str()).map_err(|_| PlanHandoffError::ReferenceMismatch)?;
		if state_url != artifact_url {
			return Err(PlanHandoffError::ReferenceMismatch);
		}
		Ok(Self {
			artifact: artifact_url,
			title:    artifact.title.clone(),
			workflow: state.workflow,
		})
	}
}
