//! Typed native plan approval projection.

use omp_agent::ApprovalSpec;
use omp_core::{Str, sf};
use serde::{Deserialize, Serialize};

use super::PlanArtifact;

/// Typed details forwarded through the native approval channel.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlanApprovalDetails {
	/// Canonical plan artifact URL.
	pub artifact: Str,
	/// Normalized plan title.
	pub title:    Str,
	/// Whether the artifact resolved at filing time.
	pub exists:   bool,
}

impl PlanApprovalDetails {
	/// Captures approval details from a resolved artifact.
	pub fn resolved(artifact: &PlanArtifact) -> Self {
		Self { artifact: artifact.url.clone(), title: artifact.title.clone(), exists: true }
	}

	/// Projects this proposal into the existing durable native approval route.
	pub fn approval_spec(&self) -> ApprovalSpec {
		ApprovalSpec {
			title:         sf!("Approve plan: {}", self.title),
			body:          sf!("Review the finalized plan at {} before execution.", self.artifact),
			subject:       self.artifact.clone(),
			kind:          sf!("plan"),
			scopes:        vec![sf!("once")],
			default:       None,
			route:         sf!("user"),
			approver:      None,
			timeout_ms:    0,
			unreachable:   sf!("deny"),
			require_human: true,
			pattern:       None,
			evidence:      vec![sf!("artifact={}", self.artifact), sf!("title={}", self.title)],
		}
	}
}

/// Native plan-review outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlanReviewDecision {
	/// Approve and hand off the plan.
	Approve,
	/// Reject without replacement guidance.
	Reject,
	/// Return user-authored revision guidance.
	Feedback(Str),
}
