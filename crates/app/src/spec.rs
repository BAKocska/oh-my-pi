//! Runtime symbol specification query surface.
//!
//! The canonical rows live beside [`omp_tool::OperationSpec`]. This module is a
//! thin application-facing view; it deliberately owns no copied metadata.

use std::path::{Path, PathBuf};

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
	#[must_use]
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
	let roots = crate::discovery::native::discover_roots(cwd, home, 32);
	let native_system = roots
		.project
		.iter()
		.map(|root| root.join("SYSTEM.md"))
		.chain(std::iter::once(roots.user.join("SYSTEM.md")));
	let native_append = roots
		.project
		.iter()
		.map(|root| root.join("APPEND_SYSTEM.md"))
		.chain(std::iter::once(roots.user.join("APPEND_SYSTEM.md")));
	let foreign_system = [cwd.join("SYSTEM.md"), cwd.join(".claude/CLAUDE.md")];
	Ok(PromptSlots {
		system: resolve_explicit(system)?
			.or(read_first(native_system)?)
			.or(read_first(foreign_system.into_iter())?),
		append: resolve_explicit(append)?.or(read_first(native_append)?),
	})
}

fn resolve_explicit(value: Option<&str>) -> miette::Result<Option<Str>> {
	let Some(value) = value else {
		return Ok(None);
	};
	let path = PathBuf::from(value);
	if path.is_file() {
		return std::fs::read_to_string(path)
			.map(Str::from)
			.map(Some)
			.into_diagnostic();
	}
	Ok(Some(Str::new(value)))
}

fn read_first(paths: impl IntoIterator<Item = PathBuf>) -> miette::Result<Option<Str>> {
	for path in paths {
		if path.is_file() {
			return std::fs::read_to_string(path)
				.map(Str::from)
				.map(Some)
				.into_diagnostic();
		}
	}
	Ok(None)
}
