//! Typed outcomes shared by every slash-command surface.

use omp_core::Str;

use super::registry::CommandProvenance;

/// A model-facing prompt produced by a command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptResult {
	/// Fully rendered prompt submitted as the next user message.
	pub text:       Str,
	/// Authority that produced the prompt.
	pub provenance: CommandProvenance,
}

/// A command that was handled without submitting a model prompt.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConsumedResult {
	/// Optional user-visible status. `None` is a deliberately silent success.
	pub status: Option<Str>,
	/// Whether the command scheduled a real agent turn after returning.
	pub agent_invoked: bool,
}

impl ConsumedResult {
	/// Creates a silent consumed result.
	pub const fn silent() -> Self {
		Self { status: None, agent_invoked: false }
	}

	/// Creates a consumed result with one status message.
	pub fn status(status: impl Into<Str>) -> Self {
		Self { status: Some(status.into()), agent_invoked: false }
	}
	/// Creates a consumed result for scheduled agent work.
	pub fn agent(status: impl Into<Str>) -> Self {
		Self { status: Some(status.into()), agent_invoked: true }
	}
}

/// Result of a recognized slash command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandResult {
	/// Submit the contained prompt to the model.
	Prompt(PromptResult),
	/// The command performed its work locally.
	Consumed(ConsumedResult),
	/// Exit the initiating client after any active turn is aborted.
	Exit,
}

/// Result of attempting roster dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DispatchResult {
	/// The input was not recognized and remains ordinary prompt text.
	Passthrough(Str),
	/// A recognized command produced a typed outcome.
	Handled(CommandResult),
}
