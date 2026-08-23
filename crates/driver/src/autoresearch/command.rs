//! `/autoresearch` parsing and completion contract.

use std::iter;

use omp_core::Str;

use super::engine::ClearTree;

/// Parsed autoresearch slash operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
	/// Enable phase one with an optional goal.
	Start {
		/// Goal text after the command.
		goal:       Option<Str>,
		/// Explicitly permit a non-Git workspace.
		unisolated: bool,
	},
	/// Disable hidden continuation while retaining history.
	Off,
	/// Clear session state and select tree handling.
	Clear(ClearTree),
}

/// Slash parse failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ParseError {
	/// Both mutually exclusive clear policies were supplied.
	#[error("--keep-tree and --reset-tree are mutually exclusive")]
	ConflictingClearTree,
	/// A clear-only flag appeared on another operation.
	#[error("tree policy flags are only valid with `/autoresearch clear`")]
	TreeFlagOutsideClear,
	/// An unknown flag was supplied.
	#[error("unknown /autoresearch flag")]
	UnknownFlag,
	/// `off` and `clear` do not accept trailing goal text.
	#[error("autoresearch subcommand does not accept trailing text")]
	UnexpectedArgument,
}

/// Parses text following `/autoresearch`.
pub fn parse(arguments: &str) -> Result<Command, ParseError> {
	let mut words = arguments.split_whitespace();
	let Some(first) = words.next() else {
		return Ok(Command::Start { goal: None, unisolated: false });
	};
	match first {
		"off" => {
			if words.next().is_some() {
				Err(ParseError::UnexpectedArgument)
			} else {
				Ok(Command::Off)
			}
		},
		"clear" => {
			let mut keep = false;
			let mut reset = false;
			for word in words {
				match word {
					"--keep-tree" => keep = true,
					"--reset-tree" => reset = true,
					value if value.starts_with('-') => return Err(ParseError::UnknownFlag),
					_ => return Err(ParseError::UnexpectedArgument),
				}
			}
			if keep && reset {
				Err(ParseError::ConflictingClearTree)
			} else {
				Ok(Command::Clear(if reset {
					ClearTree::Reset
				} else {
					ClearTree::Keep
				}))
			}
		},
		_ => {
			let mut unisolated = false;
			let mut goal = Vec::new();
			for word in iter::once(first).chain(words) {
				match word {
					"--unisolated" => unisolated = true,
					"--keep-tree" | "--reset-tree" => return Err(ParseError::TreeFlagOutsideClear),
					value if value.starts_with('-') => return Err(ParseError::UnknownFlag),
					value => goal.push(value),
				}
			}
			Ok(Command::Start {
				goal: (!goal.is_empty()).then(|| Str::from(goal.join(" "))),
				unisolated,
			})
		},
	}
}

/// Context-sensitive slash completions.
pub fn completions(arguments: &str) -> &'static [&'static str] {
	let trimmed = arguments.trim_start();
	if trimmed.starts_with("clear ") {
		&["--keep-tree", "--reset-tree"]
	} else if trimmed.contains(char::is_whitespace) {
		&[]
	} else {
		&["off", "clear", "--unisolated"]
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn clear_flags_are_explicit_and_exclusive() {
		assert_eq!(parse("clear --reset-tree"), Ok(Command::Clear(ClearTree::Reset)));
		assert_eq!(parse("clear --keep-tree"), Ok(Command::Clear(ClearTree::Keep)));
		assert_eq!(parse("clear --keep-tree --reset-tree"), Err(ParseError::ConflictingClearTree));
	}
}
