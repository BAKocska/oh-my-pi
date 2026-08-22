//! Typed first-turn and revival prompt composition for subagents.

use std::{fmt::Write as _, path::Path};

use omp_agent::{AgentDefinition, AgentNode};
use omp_core::Str;
use serde_json::Value;

use super::settings::TaskEagerMode;

/// Model-family capabilities which affect delegation guidance.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ModelFamilyCapabilities {
	/// OpenAI Codex-style models benefit from explicit tool-call concurrency.
	pub codex_style:         bool,
	/// Model supports multiple independent tool calls in one assistant step.
	pub parallel_tool_calls: bool,
	/// Child result must terminate through the yield tool.
	pub structured_yield:    bool,
}

/// One peer projected into the generation-stamped IRC roster.
pub struct PromptPeer<'a> {
	/// Session-local display alias.
	pub name:     &'a str,
	/// Definition or root kind label.
	pub role:     &'a str,
	/// Current lifecycle label.
	pub status:   &'a str,
	/// Short current activity.
	pub activity: &'a str,
}

/// Complete immutable input for one child system prompt.
pub struct SubagentPromptInput<'a> {
	/// Selected agent definition.
	pub definition:        &'a AgentDefinition,
	/// Shared batch context, if any.
	pub shared_context:    Option<&'a str>,
	/// Active parent plan path.
	pub plan_path:         Option<&'a Path>,
	/// Exact active plan content.
	pub plan_content:      Option<&'a str>,
	/// Effective normal or isolated workspace root.
	pub workspace_root:    &'a Path,
	/// Effective normalized output schema.
	pub output_schema:     Option<&'a Value>,
	/// Stable display alias used by IRC for this loop.
	pub self_name:         &'a str,
	/// Resolved definition or root role for this loop.
	pub self_role:         &'a str,
	/// Whether spawn depth permits the loop to use the IRC bus.
	pub irc_enabled:       bool,
	/// IRC roster generation.
	pub roster_generation: u64,
	/// Peers visible to this child.
	pub peers:             &'a [PromptPeer<'a>],
	/// Model-family behavioral capabilities.
	pub capabilities:      ModelFamilyCapabilities,
	/// Whether plan mode attenuated this child.
	pub plan_mode:         bool,
	/// Live eager-delegation guidance inherited by this child.
	pub eager:             TaskEagerMode,
}

/// Composes the complete child prompt without creating a second policy owner.
pub fn compose(input: SubagentPromptInput<'_>) -> Str {
	let mut output = String::with_capacity(
		input.definition.description.len() + input.definition.prompt.len() + 1_024,
	);
	if !input.definition.description.is_empty() {
		output.push_str(input.definition.description.as_str());
		output.push_str("\n\n");
	}
	output.push_str(input.definition.prompt.as_str());
	if let Some(context) = input
		.shared_context
		.filter(|context| !context.trim().is_empty())
	{
		output.push_str("\n\n# Shared Context\n");
		output.push_str(context.trim());
	}
	let _ = write!(output, "\n\n# Runtime\nWorkspace root: `{}`\n", input.workspace_root.display());
	if let Some(path) = input.plan_path {
		let _ = writeln!(output, "Active plan: `{}`", path.display());
	}
	if let Some(plan) = input.plan_content.filter(|plan| !plan.trim().is_empty()) {
		output.push_str("\n## Active Plan\n");
		output.push_str(plan.trim());
		output.push('\n');
	}
	match input.eager {
		TaskEagerMode::Default => {},
		TaskEagerMode::Preferred => output.push_str(
			"\nDelegate independent specialist work when it reduces critical-path latency; keep \
			 shared mutations serialized.\n",
		),
		TaskEagerMode::Always => output.push_str(
			"\nOn the first turn, delegate at least one meaningful independent slice when spawn \
			 policy permits it.\n",
		),
	}
	if input.plan_mode {
		output.push_str(
			"\nPlan mode is read-only: inspect and return an executable plan. Do not mutate, spawn, \
			 or isolate work.\n",
		);
	}
	if let Some(schema) = input.output_schema {
		output.push_str(
			"\nReturn the terminal result through `yield` with complete data matching this effective \
			 JSON Schema:\n",
		);
		match serde_json::to_string(schema) {
			Ok(encoded) => output.push_str(&encoded),
			Err(_) => output.push_str("{}"),
		}
		output.push('\n');
	}
	if input.irc_enabled {
		let _ = writeln!(
			output,
			"\n# IRC\nYou are {} ({}) on roster generation {}.",
			input.self_name, input.self_role, input.roster_generation
		);
		output.push_str(
			"Ordinary sends are fire-and-forget. Await a reply only when blocked; reply with the \
			 received message id. Delivery receipts describe routing, not task completion.\n",
		);
		for peer in input.peers {
			let self_marker = if peer.name.eq_ignore_ascii_case(input.self_name) {
				", self"
			} else {
				""
			};
			let _ = writeln!(
				output,
				"- {} ({}, {}{}): {}",
				peer.name,
				peer.role,
				peer.status,
				self_marker,
				if peer.activity.is_empty() {
					"idle"
				} else {
					peer.activity
				}
			);
		}
	}
	if input.capabilities.codex_style {
		output.push_str(
			"\nFor independent lookups, issue tool calls together; keep dependent mutations ordered \
			 and verify the resulting state.\n",
		);
	} else if input.capabilities.parallel_tool_calls {
		output.push_str("\nUse parallel tool calls only for genuinely independent work.\n");
	}
	if input.capabilities.structured_yield {
		output.push_str(
			"Incremental yield paths accumulate until a terminal yield; never repeat assembled \
			 sections in the terminal payload.\n",
		);
	}
	Str::from(output)
}

/// Projects one live roster node without transferring scheduling authority.
pub fn peer_from_node(node: &AgentNode) -> (Str, Str, Str, Str) {
	(
		node.name.clone(),
		node
			.definition
			.clone()
			.unwrap_or_else(|| Str::new_static("main")),
		Str::from(node.status().to_string()),
		node.activity(),
	)
}
