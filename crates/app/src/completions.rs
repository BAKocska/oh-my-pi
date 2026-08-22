//! Static shell completion generation from the authoritative clap graph.

use std::io;

use clap::CommandFactory as _;
use clap_complete::{Shell, generate};

/// Writes a complete shell script for `omp`.
pub fn generate_script(shell: Shell, output: &mut dyn io::Write) {
	let mut command = crate::cli::OmpCli::command();
	generate(shell, &mut command, "omp", output);
}

/// Generates a shell script into owned bytes.
pub fn script(shell: Shell) -> Vec<u8> {
	let mut output = Vec::new();
	generate_script(shell, &mut output);
	output
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn every_supported_script_references_the_binary() {
		for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
			let output = String::from_utf8(script(shell)).expect("utf8");
			assert!(output.contains("omp"));
		}
	}
}
