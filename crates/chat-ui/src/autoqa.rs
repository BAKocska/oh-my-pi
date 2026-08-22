//! Explicit AutoQA upload-consent presentation state.

use omp_core::Str;

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

/// User-authored upload disposition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Decision {
	/// Keep the report private and terminally local-only.
	LocalOnly,
	/// Consent to upload this exact target revision.
	Upload,
}

/// Consent emitted by UI interaction rather than model arguments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConsentIntent {
	/// Durable local issue id.
	pub issue_id: Str,
	/// Revision observed by the confirmation surface.
	pub revision: Str,
	/// User-authored disposition.
	pub decision: Decision,
}

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
}
