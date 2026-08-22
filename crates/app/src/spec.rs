//! Runtime symbol specification query surface.
//!
//! The canonical rows live beside [`omp_tool::OperationSpec`]. This module is a
//! thin application-facing view; it deliberately owns no copied metadata.

use std::path::Path;

use miette::IntoDiagnostic as _;
use omp_core::Str;
pub use omp_tool::{
	CallbackAbi, OperationSpec, PhaseLegalityRow, RuntimeDurationMetadata, RuntimeSymbolSpec,
	operation_spec, phase_legality_matrix, runtime_duration_metadata, runtime_symbols,
};

/// Typed system-prompt slots resolved at the command boundary.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PromptSlots {
	/// Replacement system instructions.
	pub system: Option<Str>,
	/// Instructions appended after the replacement/default system slot.
	pub append: Option<Str>,
}

impl PromptSlots {
	/// Materializes the ordered system message consumed by inference.
	pub fn combined(self) -> Option<Str> {
		match (self.system, self.append) {
			(Some(system), Some(append)) => Some(Str::from(format!("{system}\n\n{append}"))),
			(Some(system), None) => Some(system),
			(None, Some(append)) => Some(append),
			(None, None) => None,
		}
	}
}

/// Resolves explicit file-or-literal overrides, then native `SYSTEM.md` and
/// `APPEND_SYSTEM.md` discovery. Native `.omp` content precedes user and
/// foreign-compatible content roots.
pub fn resolve_prompt_slots(
	cwd: &Path,
	home: &Path,
	system: Option<&str>,
	append: Option<&str>,
) -> miette::Result<PromptSlots> {
	let (system, explicit_append) =
		crate::prompt_input::resolve_system_inputs(cwd, home, system, append)
			.map_err(|error| miette::miette!(error))?;
	let append = if explicit_append.is_some() {
		explicit_append
	} else {
		crate::prompt_input::discover_prompt_file(cwd, home, "APPEND_SYSTEM.md").into_diagnostic()?
	};
	Ok(PromptSlots { system, append })
}
