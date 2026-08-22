//! Transcript retention formatting, durable cursors, and idempotent suffix
//! commits.

use omp_core::Str;
use serde::{Deserialize, Serialize};
use strum::{Display, EnumString, IntoStaticStr};

use crate::{
	Result,
	store::{BankStore, RetainedWindow},
};

/// Journal-settled message role.
#[derive(
	Clone, Copy, Debug, Display, EnumString, Eq, IntoStaticStr, PartialEq, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
#[strum(ascii_case_insensitive, serialize_all = "lowercase")]
pub enum RetentionRole {
	/// User-authored input.
	User,
	/// Assistant response.
	Assistant,
	/// Durable tool outcome.
	Tool,
	/// System-authored settled context.
	System,
}

/// Owned settled message used by bounded shutdown retention.
#[derive(Clone, Debug)]
pub struct OwnedRetentionMessage {
	/// Stable journal item id.
	pub stable_id: Str,
	/// Message role.
	pub role:      RetentionRole,
	/// Settled textual content.
	pub content:   Str,
}
/// One journal-durable message eligible for retention.
#[derive(Clone, Copy)]
pub struct RetentionMessage<'a> {
	/// Stable journal item id used for idempotency metadata.
	pub stable_id: &'a str,
	/// Message role.
	pub role:      RetentionRole,
	/// Settled textual content.
	pub content:   &'a str,
}

/// Result of one retention decision.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RetentionOutcome {
	/// Whether a new durable episode was stored.
	pub stored_id:        Option<Str>,
	/// Highest covered user turn after the operation.
	pub retained_through: u64,
	/// User-only framed text supplied to extraction, when substantive.
	pub extraction_text:  Option<Str>,
	/// Marker-free text supplied to embeddings, when substantive.
	pub embedding_text:   Option<Str>,
}

/// Per-session retention coordinator backed by a durable cursor in the bank.
pub struct Retainer<'a> {
	store:                &'a BankStore,
	session_id:           &'a str,
	canonical_root:       &'a str,
	retain_every_n_turns: usize,
}

impl<'a> Retainer<'a> {
	/// Creates a coordinator. The turn interval is clamped to at least one.
	#[must_use]
	pub fn new(
		store: &'a BankStore,
		session_id: &'a str,
		canonical_root: &'a str,
		retain_every_n_turns: usize,
	) -> Self {
		Self { store, session_id, canonical_root, retain_every_n_turns: retain_every_n_turns.max(1) }
	}

	/// Retains only when the Pi-default user-turn interval has elapsed.
	pub fn retain_periodic(&self, messages: &[RetentionMessage<'_>]) -> Result<RetentionOutcome> {
		self.retain(messages, false)
	}

	/// Force-retains the unprocessed suffix, including a short final session
	/// window.
	pub fn retain_force(&self, messages: &[RetentionMessage<'_>]) -> Result<RetentionOutcome> {
		self.retain(messages, true)
	}

	fn retain(&self, messages: &[RetentionMessage<'_>], force: bool) -> Result<RetentionOutcome> {
		let cursor = self.store.retention_cursor(self.session_id)?;
		let user_turns = messages
			.iter()
			.filter(|message| message.role == RetentionRole::User)
			.count() as u64;
		if user_turns <= cursor || (!force && user_turns - cursor < self.retain_every_n_turns as u64)
		{
			return Ok(RetentionOutcome { retained_through: cursor, ..RetentionOutcome::default() });
		}
		let suffix = slice_unretained(messages, cursor);
		let Some(transcript) = format_durable_transcript(suffix) else {
			return Ok(RetentionOutcome { retained_through: cursor, ..RetentionOutcome::default() });
		};
		let extraction_text = format_extraction_text(suffix);
		let embedding_text = format_embedding_text(suffix);
		let ids = suffix
			.iter()
			.map(|message| message.stable_id)
			.collect::<Vec<_>>();
		let metadata = serde_json::json!({
			"session_id": self.session_id,
			"source_ids": ids,
			"message_count": suffix.len(),
			"retained_through_user_turn": user_turns,
			"primary_root": self.canonical_root,
		});
		let stored_id = self.store.retain_window(RetainedWindow {
			session_id:                 self.session_id,
			transcript:                 transcript.as_str(),
			embed_text:                 embedding_text.as_deref().unwrap_or(transcript.as_str()),
			metadata:                   &metadata,
			retained_through_user_turn: user_turns,
		})?;
		Ok(RetentionOutcome {
			stored_id,
			retained_through: user_turns,
			extraction_text,
			embedding_text,
		})
	}
}

/// Frames all substantive messages with explicit role/end markers.
#[must_use]
pub fn format_durable_transcript(messages: &[RetentionMessage<'_>]) -> Option<Str> {
	format_messages(messages.iter().copied(), true)
}

/// Frames only user-authored messages for fact/entity extraction.
#[must_use]
pub fn format_extraction_text(messages: &[RetentionMessage<'_>]) -> Option<Str> {
	format_messages(
		messages
			.iter()
			.copied()
			.filter(|message| message.role == RetentionRole::User),
		true,
	)
}

/// Formats every substantive message without protocol markers for embedding and
/// FTS.
#[must_use]
pub fn format_embedding_text(messages: &[RetentionMessage<'_>]) -> Option<Str> {
	format_messages(messages.iter().copied(), false)
}

/// Removes retention protocol markers from recalled episode content.
#[must_use]
pub fn strip_protocol_markers(content: &str) -> Str {
	let mut output = String::with_capacity(content.len());
	for line in content.lines() {
		let trimmed = line.trim();
		let marker = trimmed.starts_with("[role: ") && trimmed.ends_with(']')
			|| trimmed.starts_with("[user:end]")
			|| trimmed.starts_with("[assistant:end]")
			|| trimmed.starts_with("[tool:end]")
			|| trimmed.starts_with("[system:end]");
		if marker {
			continue;
		}
		if !output.is_empty() {
			output.push('\n');
		}
		output.push_str(line);
	}
	Str::new(output.trim())
}

fn slice_unretained<'a>(
	messages: &'a [RetentionMessage<'a>],
	cursor: u64,
) -> &'a [RetentionMessage<'a>] {
	if cursor == 0 {
		return messages;
	}
	let mut users = 0u64;
	for (index, message) in messages.iter().enumerate() {
		if message.role != RetentionRole::User {
			continue;
		}
		users += 1;
		if users > cursor {
			return &messages[index..];
		}
	}
	&[]
}

fn format_messages<'a>(
	messages: impl Iterator<Item = RetentionMessage<'a>>,
	markers: bool,
) -> Option<Str> {
	let mut output = String::new();
	for message in messages {
		let content = strip_memory_blocks(message.content);
		let content = content.trim();
		if !substantive(content) {
			continue;
		}
		if !output.is_empty() {
			output.push_str("\n\n");
		}
		if markers {
			let role: &'static str = message.role.into();
			output.push_str("[role: ");
			output.push_str(role);
			output.push_str("]\n");
			output.push_str(content);
			output.push('\n');
			output.push('[');
			output.push_str(role);
			output.push_str(":end]");
		} else {
			output.push_str(content);
		}
	}
	if output.trim().len() < 10 {
		None
	} else {
		Some(Str::new(output))
	}
}

fn strip_memory_blocks(content: &str) -> String {
	let mut output = String::with_capacity(content.len());
	let mut inside = false;
	for line in content.lines() {
		let trimmed = line.trim();
		if trimmed.starts_with("<memories>") {
			inside = true;
			continue;
		}
		if trimmed.ends_with("</memories>") {
			inside = false;
			continue;
		}
		if inside {
			continue;
		}
		if !output.is_empty() {
			output.push('\n');
		}
		output.push_str(line);
	}
	output
}

fn substantive(content: &str) -> bool {
	content.chars().any(char::is_alphanumeric)
}
