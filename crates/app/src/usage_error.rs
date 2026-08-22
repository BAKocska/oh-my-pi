//! Structured command-line usage failures.

use miette::Diagnostic;
use thiserror::Error;

/// A validation failure rendered without a stack trace and with the standard
/// help pointer used by every OMP command-line entry point.
#[derive(Clone, Debug, Diagnostic, Error, Eq, PartialEq)]
#[diagnostic(code(omp::cli::usage))]
#[error("{message}\nRun omp --help for available flags.")]
pub struct CliUsageError {
	message: String,
}

impl CliUsageError {
	/// Creates a usage error with the standard help pointer.
	pub fn new(message: impl Into<String>) -> Self {
		Self { message: message.into() }
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn includes_the_standard_help_pointer() {
		assert_eq!(
			CliUsageError::new("bad flag").to_string(),
			"bad flag\nRun omp --help for available flags."
		);
	}
}
