//! Explicit AutoQA upload-consent presentation state.

use omp_core::{Str, sf};
use omp_tools::ask::{OptionItem, Question};

/// Redacted local report offered for optional delivery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConsentRequest {
	/// Durable local issue id.
	pub issue_id: Str,
	/// Exact `name@rev` target shown to the user.
	pub target:   Str,
	/// Exact target revision fenced by the eventual decision.
	pub revision: Str,
	/// Redacted payload summary; never raw prompt/provider bytes.
	pub summary:  Str,
}
pub use omp_storage::telemetry_index::{ConsentIntent, Decision};
/// Modal state for one AutoQA consent request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutoQaConsent {
	request: ConsentRequest,
}

impl AutoQaConsent {
	/// Opens a consent surface over one already-redacted local report.
	pub const fn new(request: ConsentRequest) -> Self {
		Self { request }
	}

	/// Borrows the exact revision-bound request shown to the user.
	pub const fn request(&self) -> &ConsentRequest {
		&self.request
	}

	/// Produces a durable host intent only from an explicit UI selection.
	pub fn decide(self, decision: Decision) -> ConsentIntent {
		ConsentIntent { issue_id: self.request.issue_id, revision: self.request.revision, decision }
	}

	/// Builds the explicit upload-or-local-only choice for this report.
	pub fn question(&self) -> Question {
		Question {
			id:          Str::new_static("autoqa-consent"),
			question:    sf!(
				"{}\n\nSend the redacted report for {} to AutoQA?",
				self.request.summary,
				self.request.target
			),
			header:      Some(Str::new_static("AutoQA consent")),
			options:     vec![
				OptionItem {
					label:       Str::new_static("Upload"),
					description: Some(Str::new_static(
						"Send only the displayed redacted report for this exact revision.",
					)),
					preview:     None,
				},
				OptionItem {
					label:       Str::new_static("Keep local"),
					description: Some(Str::new_static(
						"Retain the report locally without external delivery.",
					)),
					preview:     None,
				},
			],
			multi:       false,
			recommended: Some(1),
		}
	}
}
#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn decision_keeps_the_displayed_revision_fence() {
		let intent = AutoQaConsent::new(ConsentRequest {
			issue_id: sf!("qa-1"),
			target:   sf!("read@2"),
			revision: sf!("2"),
			summary:  sf!("Redacted report."),
		})
		.decide(Decision::Upload);
		assert_eq!(intent.issue_id, "qa-1");
		assert_eq!(intent.revision, "2");
		assert_eq!(intent.decision, Decision::Upload);
	}
}
