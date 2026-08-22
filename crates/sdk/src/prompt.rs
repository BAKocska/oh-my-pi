//! Deterministic prompt compilation for SDK embedders.

use std::sync::Arc;

use omp_agent::{
	PromptError, PromptOut, PromptPatchSet, Props, RenderedPrompt, SlotAssembler, SlotClass,
	SlotDecl, SlotId, SlotRegistration, SlotSource,
};
use omp_core::Str;
use thiserror::Error;

use crate::SystemPromptCallback;

/// One immutable typed prompt contribution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptContribution {
	/// Destination prompt slot.
	pub slot:     SlotId,
	/// Stability band used for prompt-cache hashing.
	pub class:    SlotClass,
	/// Stable contributor identity.
	pub owner:    Str,
	/// Descending order within the slot.
	pub priority: i16,
	/// Contribution bytes.
	pub content:  Str,
}

#[derive(Clone)]
struct TextContribution(Str);

impl SlotSource for TextContribution {
	fn render(&self, _workspace: &Props, out: &mut dyn PromptOut) -> Result<(), PromptError> {
		out.write_str(&self.0);
		Ok(())
	}
}

/// Failure to compile an SDK prompt.
#[derive(Debug, Error)]
pub enum PromptPatchError {
	/// A callback returned different patches for identical immutable input.
	#[error("system prompt callback was nondeterministic for one snapshot")]
	CallbackVolatile,
	/// Native prompt validation or rendering failed.
	#[error(transparent)]
	Prompt(#[from] PromptError),
}

/// Builder for a canonical, hashed system prompt.
#[derive(Default)]
pub struct PromptCompiler {
	contributions: Vec<PromptContribution>,
	callback:      Option<SystemPromptCallback>,
}

impl PromptCompiler {
	/// Creates an empty compiler.
	pub const fn new() -> Self {
		Self { contributions: Vec::new(), callback: None }
	}

	/// Adds one immutable typed contribution.
	pub fn contribution(mut self, contribution: PromptContribution) -> Self {
		self.contributions.push(contribution);
		self
	}

	/// Installs the provider-system-prompt callback.
	pub fn callback(mut self, callback: SystemPromptCallback) -> Self {
		self.callback = Some(callback);
		self
	}

	/// Compiles canonical items and their prompt hash.
	pub fn compile(&self, workspace: &Props) -> Result<RenderedPrompt, PromptPatchError> {
		let registrations = self
			.contributions
			.iter()
			.map(|contribution| SlotRegistration {
				decl:   SlotDecl {
					slot:     contribution.slot,
					class:    contribution.class,
					owner:    contribution.owner.clone(),
					priority: contribution.priority,
				},
				source: Arc::new(TextContribution(contribution.content.clone())),
			})
			.collect();
		let mut assembler = SlotAssembler::new(registrations);
		if let Some(callback) = &self.callback {
			let first: PromptPatchSet = callback(workspace)?;
			let second = callback(workspace)?;
			if first != second {
				return Err(PromptPatchError::CallbackVolatile);
			}
			assembler = assembler.with_patches(first);
		}
		assembler
			.render_banded(workspace)
			.map(|(rendered, _)| rendered)
			.map_err(Into::into)
	}
}
