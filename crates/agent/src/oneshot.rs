//! Allocation-conscious completion choice resolution and pre-dispatch budgets.

use omp_core::Str;
use smallvec::SmallVec;
use thiserror::Error;

/// One completion request's Core-owned semantics.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CompletionRequest {
	/// Ordered strings to search in the provider emission.
	pub choices:        SmallVec<Str, 4>,
	/// Caller-owned fallback used only for failure or no-choice match.
	pub default:        Option<Str>,
	/// Maximum durable-receipt cost in micros of USD, if supplied.
	pub max_usd_micros: Option<u64>,
}

/// A completed constrained one-shot result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Completion {
	/// Raw provider text emission.
	pub text:      Str,
	/// Resolved ladder member, absent for free-text requests.
	pub choice:    Option<Str>,
	/// Whether this result came from the caller's fallback.
	pub fell_back: bool,
}

/// Completion failure when no caller fallback was supplied.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum CompletionError {
	/// A proposed request could not be funded before provider dispatch.
	#[error("completion budget exhausted before provider request")]
	BudgetExceeded,
	/// Provider execution failed.
	#[error("completion provider failed: {0}")]
	Provider(Str),
	/// No candidate occurred in a successful provider emission.
	#[error("completion emission matched no choice")]
	NoChoice,
}

/// Returns the ordered candidate with the earliest occurrence in `text`.
///
/// This uses one `str::find` per short choice and allocates nothing. Ties
/// retain the caller's earlier choice, making the choice ladder deterministic.
#[must_use]
pub fn select_choice<'a>(text: &str, choices: &'a [Str]) -> Option<&'a Str> {
	let mut selected: Option<(&Str, usize)> = None;
	for choice in choices {
		let Some(position) = text.find(choice.as_str()) else {
			continue;
		};
		if selected.is_none_or(|(_, earliest)| position < earliest) {
			selected = Some((choice, position));
		}
	}
	selected.map(|(choice, _)| choice)
}

/// Resolves one provider result after its budget was checked before dispatch.
///
/// A supplied default turns any provider failure or unmatched emission into a
/// journalable `fell_back` result. The journal owner records that fact; this
/// transport-neutral layer intentionally never owns a journal.
pub fn resolve_completion(
	request: &CompletionRequest,
	provider: Result<Str, CompletionError>,
) -> Result<Completion, CompletionError> {
	match provider {
		Ok(text) if request.choices.is_empty() => {
			Ok(Completion { text, choice: None, fell_back: false })
		},
		Ok(text) => match select_choice(text.as_str(), &request.choices) {
			Some(choice) => Ok(Completion { text, choice: Some(choice.clone()), fell_back: false }),
			None => fallback(request, text, CompletionError::NoChoice),
		},
		Err(error) => fallback(request, Str::new(""), error),
	}
}

fn fallback(
	request: &CompletionRequest,
	text: Str,
	error: CompletionError,
) -> Result<Completion, CompletionError> {
	match request.default.as_ref() {
		Some(choice) => Ok(Completion { text, choice: Some(choice.clone()), fell_back: true }),
		None => Err(error),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn leftmost_match_wins_the_choice_ladder() {
		let choices: smallvec::SmallVec<Str, 4> =
			smallvec::smallvec![Str::from("later"), Str::from("first")];
		assert_eq!(select_choice("first then later", &choices), Some(&choices[1]));
	}

	#[test]
	fn default_marks_fallback_for_provider_failure() {
		let request = CompletionRequest { default: Some(Str::from("unknown")), ..Default::default() };
		let completion =
			resolve_completion(&request, Err(CompletionError::Provider(Str::from("offline"))))
				.unwrap();
		assert!(completion.fell_back);
		assert_eq!(completion.choice.as_deref(), Some("unknown"));
	}
}
