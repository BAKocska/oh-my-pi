//! Bounded subagent result projection with durable full-output persistence.

use std::{fs, path::Path};

use omp_agent::SubagentDisposition;
use omp_core::{Str, sf};
use thiserror::Error;

/// Maximum caller-returned output bytes.
pub const MAX_OUTPUT_BYTES: usize = 500_000;
/// Maximum caller-returned output lines.
pub const MAX_OUTPUT_LINES: usize = 5_000;
/// Maximum retained disposition preview characters.
pub const MAX_PREVIEW_CHARS: usize = 5_000;
/// Maximum flattened cancellation salvage characters.
pub const MAX_CANCELLATION_SALVAGE_CHARS: usize = 500;

/// Durable output write failure.
#[derive(Debug, Error)]
pub enum OutputError {
	/// The output parent directory could not be created.
	#[error("subagent artifact directory could not be created")]
	CreateDirectory(#[source] std::io::Error),
	/// The complete output could not be written.
	#[error("subagent artifact could not be written")]
	Write(#[source] std::io::Error),
	/// The temporary artifact could not be atomically published.
	#[error("subagent artifact could not be published")]
	Publish(#[source] std::io::Error),
}

/// Persists complete output atomically and returns a bounded disposition.
pub fn persist_bounded(
	path: &Path,
	artifact_uri: Str,
	full: &str,
	workspace: Option<Str>,
	cancelled: bool,
) -> Result<SubagentDisposition, OutputError> {
	let parent = path.parent().unwrap_or_else(|| Path::new("."));
	fs::create_dir_all(parent).map_err(OutputError::CreateDirectory)?;
	let temporary = path.with_extension("tmp");
	fs::write(&temporary, full.as_bytes()).map_err(OutputError::Write)?;
	fs::rename(&temporary, path).map_err(OutputError::Publish)?;
	let (bounded, output_truncated) = bounded_tail(full);
	let preview = if cancelled {
		flattened_salvage(bounded.as_str())
	} else {
		truncate_chars(bounded.as_str(), MAX_PREVIEW_CHARS)
	};
	let preview_truncated = preview.len() < bounded.len();
	Ok(SubagentDisposition {
		artifact_uri: Some(artifact_uri),
		preview: (!preview.is_empty()).then_some(preview),
		truncated: output_truncated || preview_truncated,
		workspace,
	})
}

fn bounded_tail(full: &str) -> (Str, bool) {
	let mut start = full.len().saturating_sub(MAX_OUTPUT_BYTES);
	while start < full.len() && !full.is_char_boundary(start) {
		start += 1;
	}
	if start != 0
		&& let Some(newline) = full[start..].find('\n')
	{
		start += newline + 1;
	}
	let tail = &full[start..];
	let line_count = tail
		.as_bytes()
		.iter()
		.filter(|byte| **byte == b'\n')
		.count()
		+ 1;
	if line_count > MAX_OUTPUT_LINES {
		let skip = line_count - MAX_OUTPUT_LINES;
		let mut seen = 0;
		for (index, byte) in tail.bytes().enumerate() {
			if byte == b'\n' {
				seen += 1;
				if seen == skip {
					let offset = index + 1;
					return (Str::new(&tail[offset..]), true);
				}
			}
		}
	}
	(Str::new(tail), start != 0)
}

fn flattened_salvage(value: &str) -> Str {
	let mut flat = String::with_capacity(value.len().min(MAX_CANCELLATION_SALVAGE_CHARS));
	for word in value.split_whitespace() {
		if !flat.is_empty() {
			flat.push(' ');
		}
		flat.push_str(word);
		if flat.chars().count() >= MAX_CANCELLATION_SALVAGE_CHARS {
			break;
		}
	}
	truncate_chars(&flat, MAX_CANCELLATION_SALVAGE_CHARS)
}

fn truncate_chars(value: &str, limit: usize) -> Str {
	let Some((offset, _)) = value.char_indices().nth(limit) else {
		return Str::new(value);
	};
	let mut output = Str::new(&value[..offset]);
	if !output.is_empty() {
		output = sf!("{}...", output);
	}
	output
}
