//! Two-tool rewrite/review/approve ledger.

use omp_core::Str;

use super::types::{Action, Draft, Loss, Metrics};

/// Protocol violation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ProtocolError {
	/// Draft text was empty.
	#[error("rewrite text must not be empty")]
	EmptyDraft,
	/// A declared loss lacked content or justification.
	#[error("each rewrite loss needs non-empty content and reason")]
	InvalidLoss,
	/// Approval arrived before any rewrite.
	#[error("call rewrite before approve")]
	NoDraft,
	/// Approval arrived in the same turn as the newest rewrite.
	#[error("the newest draft must be reviewed in a separate turn before approval")]
	NotReviewed,
	/// A turn attempted to replace an already approved draft.
	#[error("an approved compression session is closed")]
	Closed,
}

/// Stateful protocol shared across isolated session turns.
#[derive(Clone, Debug)]
pub struct Protocol {
	source_words:  usize,
	source_tokens: usize,
	drafts:        Vec<Draft>,
	reviewed:      u32,
	approved:      bool,
	verdict:       Option<Str>,
}

impl Protocol {
	/// Creates a ledger for `source`.
	#[must_use]
	pub fn new(source: &str) -> Self {
		Self {
			source_words:  words(source),
			source_tokens: estimate_tokens(source),
			drafts:        Vec::new(),
			reviewed:      0,
			approved:      false,
			verdict:       None,
		}
	}

	/// Applies all tool calls from one model turn in exact order.
	///
	/// `approve` cannot certify a preceding same-turn `rewrite` because the
	/// command marks review only after the turn settles and renders the draft
	/// back to the model.
	pub fn apply_turn(&mut self, actions: Vec<Action>) -> Result<(), ProtocolError> {
		if self.approved {
			return Err(ProtocolError::Closed);
		}
		for action in actions {
			if self.approved {
				return Err(ProtocolError::Closed);
			}
			match action {
				Action::Rewrite { text, losses } => {
					if text.trim().is_empty() {
						return Err(ProtocolError::EmptyDraft);
					}
					if losses
						.iter()
						.any(|loss| loss.content.trim().is_empty() || loss.reason.trim().is_empty())
					{
						return Err(ProtocolError::InvalidLoss);
					}
					self.drafts.push(Draft {
						round: self
							.drafts
							.len()
							.try_into()
							.unwrap_or(u32::MAX)
							.saturating_add(1),
						text,
						losses,
					});
					self.approved = false;
					self.verdict = None;
				},
				Action::Approve { verdict } => {
					let draft = self.latest().ok_or(ProtocolError::NoDraft)?;
					if draft.round > self.reviewed {
						return Err(ProtocolError::NotReviewed);
					}
					self.approved = true;
					self.verdict = Some(verdict);
				},
			}
		}
		Ok(())
	}

	/// Marks `round` as rendered into a separate review turn.
	pub fn mark_reviewed(&mut self, round: u32) {
		self.reviewed = self.reviewed.max(round);
	}

	/// Newest draft.
	#[must_use]
	pub fn latest(&self) -> Option<&Draft> {
		self.drafts.last()
	}

	/// Whether a separately reviewed draft is accepted.
	#[must_use]
	pub const fn approved(&self) -> bool {
		self.approved
	}

	/// Approval verdict.
	#[must_use]
	pub fn verdict(&self) -> Option<&str> {
		self.verdict.as_deref()
	}

	/// Number of submitted drafts.
	#[must_use]
	pub fn rounds(&self) -> u32 {
		self.drafts.len().try_into().unwrap_or(u32::MAX)
	}

	/// Estimated source token count.
	#[must_use]
	pub const fn source_tokens(&self) -> usize {
		self.source_tokens
	}

	/// Source/draft word and token delta.
	#[must_use]
	pub fn metrics(&self, draft: &Draft) -> Metrics {
		let draft_tokens = estimate_tokens(draft.text.as_str());
		Metrics {
			source_words: self.source_words,
			draft_words: words(draft.text.as_str()),
			source_tokens: self.source_tokens,
			draft_tokens,
			ratio: if self.source_tokens == 0 {
				0.0
			} else {
				(self.source_tokens as f64 - draft_tokens as f64) / self.source_tokens as f64
			},
		}
	}
}

fn words(text: &str) -> usize {
	text.split_whitespace().count()
}

fn estimate_tokens(text: &str) -> usize {
	if text.is_empty() {
		return 0;
	}
	let words = words(text);
	let punctuation = text
		.chars()
		.filter(|character| character.is_ascii_punctuation())
		.count();
	let bytes = text.len().div_ceil(4);
	bytes.max(words.saturating_add(punctuation / 3))
}

/// Exactly the two advertised tool schemas.
#[must_use]
pub fn tool_schemas() -> [(&'static str, serde_json::Value); 2] {
	[
		(
			"rewrite",
			serde_json::json!({
				"type": "object",
				"additionalProperties": false,
				"required": ["text", "losses"],
				"properties": {
					"text": {"type": "string", "minLength": 1},
					"losses": {"type": "array", "items": {"type": "object", "additionalProperties": false, "required": ["content", "reason"], "properties": {"content": {"type": "string", "minLength": 1}, "reason": {"type": "string", "minLength": 1}}}}
				}
			}),
		),
		(
			"approve",
			serde_json::json!({
				"type": "object",
				"additionalProperties": false,
				"required": ["verdict"],
				"properties": {"verdict": {"type": "string", "minLength": 1}}
			}),
		),
	]
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn same_turn_self_approval_is_rejected() {
		let mut protocol = Protocol::new("long source prompt");
		let error = protocol
			.apply_turn(vec![
				Action::Rewrite {
					text:   "short".into(),
					losses: vec![Loss { content: "long".into(), reason: "redundant".into() }],
				},
				Action::Approve { verdict: "fine".into() },
			])
			.expect_err("same-turn approval must fail");
		assert_eq!(error, ProtocolError::NotReviewed);
	}
}
