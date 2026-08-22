//! Context usage rendering from the one anchored agent snapshot.

use std::fmt::Write as _;

use omp_agent::ContextSnapshot;
use omp_core::Str;

use super::command;

command!(context, 400, "context", [], "Show anchored context usage", [Context], true, none => |host| host.context());

/// Renders only fields from an already anchored snapshot; this function never
/// re-tokenizes or consults mutable chat state.
#[must_use]
pub fn render(snapshot: &ContextSnapshot) -> Str {
	let mut output = String::with_capacity(320);
	let _ = writeln!(
		output,
		"Context · turn {} · projection {}",
		snapshot.turn_id, snapshot.projection_revision
	);
	category(&mut output, "System", snapshot.system_tokens);
	category(&mut output, "Messages", snapshot.message_tokens);
	category(&mut output, "Tools", snapshot.tool_tokens);
	category(&mut output, "Buffers", snapshot.buffer_tokens);
	let _ = writeln!(output, "Unclassified/unavailable: {} tokens", snapshot.unclassified_tokens);
	let _ = writeln!(output, "Input: {} tokens", snapshot.input_tokens);
	let _ = writeln!(output, "Slack: {} tokens", snapshot.slack_tokens);
	let _ = writeln!(output, "Window: {} tokens", snapshot.window_tokens);
	match snapshot.snapcompact_savings {
		Some(tokens) => {
			let _ = write!(output, "Snapcompact savings: {tokens} tokens");
		},
		None => output.push_str("Snapcompact savings: unavailable"),
	}
	Str::from(output)
}

fn category(output: &mut String, label: &str, tokens: Option<u64>) {
	match tokens {
		Some(tokens) => {
			let _ = writeln!(output, "{label}: {tokens} tokens");
		},
		None => {
			let _ = writeln!(output, "{label}: unavailable");
		},
	}
}
