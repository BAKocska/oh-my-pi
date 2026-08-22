//! Historical artifact-name reservation for durable subagent aliases.

use std::path::Path;

use omp_agent::AgentTree;
use omp_core::Str;

/// Scans journal and output stems before the first new display-name allocation.
pub fn reserve_historical_stems(tree: &AgentTree, directory: &Path) -> std::io::Result<usize> {
	let mut stems = Vec::new();
	let entries = match std::fs::read_dir(directory) {
		Ok(entries) => entries,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
		Err(error) => return Err(error),
	};
	for entry in entries {
		let entry = entry?;
		if !entry.file_type()?.is_file() {
			continue;
		}
		let path = entry.path();
		if !matches!(path.extension().and_then(std::ffi::OsStr::to_str), Some("md" | "jsonl")) {
			continue;
		}
		if let Some(stem) = path.file_stem().and_then(std::ffi::OsStr::to_str)
			&& !stem.starts_with('.')
		{
			stems.push(Str::new(stem));
		}
	}
	stems.sort();
	stems.dedup();
	for stem in &stems {
		tree.reserve_historical_name(stem.as_str());
	}
	Ok(stems.len())
}

/// Normalizes a tiny-model one-line label when no caller name was supplied.
#[must_use]
pub fn normalize_generated_label(candidate: &str) -> Option<Str> {
	let line = candidate.lines().next()?.trim();
	if line.is_empty() {
		return None;
	}
	let mut output = String::with_capacity(line.len().min(32));
	for character in line.chars() {
		if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
			output.push(character);
		} else if character.is_whitespace() && !output.ends_with('-') {
			output.push('-');
		}
		if output.len() >= 32 {
			break;
		}
	}
	while output.ends_with('-') {
		output.pop();
	}
	(!output.is_empty()).then(|| Str::from(output))
}
