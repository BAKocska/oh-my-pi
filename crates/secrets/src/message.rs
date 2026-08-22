//! Author-sensitive message transformation policy.

use strum::{Display, EnumString, IntoStaticStr};

use crate::obfuscator::SecretObfuscator;

/// Semantic origin of a textual message field.
#[derive(Clone, Copy, Debug, Display, EnumString, Eq, IntoStaticStr, PartialEq)]
#[strum(serialize_all = "snake_case")]
pub enum MessageTextKind {
	/// System instructions, which are never transformed.
	System,
	/// Tool schemas or grammar declarations, which are never transformed.
	Schema,
	/// Signed or hidden model reasoning, which is never transformed by the
	/// generic policy.
	Thinking,
	/// Operator-authored user text.
	User,
	/// Operator-attributed developer text.
	Developer,
	/// Tool-result text sent back to the provider.
	ToolResult,
	/// Persisted assistant text being replayed to a provider.
	AssistantReplay,
	/// Fresh provider-authored assistant text being restored locally.
	AssistantOutput,
	/// Model-authored tool arguments being restored locally.
	ToolArguments,
	/// Model-authored intent or summary text being restored locally.
	ModelMetadata,
	/// Binary or encoded media, which is never transformed.
	Binary,
}

/// Applies the outbound provider-bound policy to one textual field.
///
/// System instructions, schemas, thinking/signatures, binary content, and
/// non-operator developer instructions remain byte-identical. Assistant replay
/// is re-obfuscated because the journal is local raw truth.
#[must_use]
pub fn obfuscate_message_text(
	obfuscator: &mut SecretObfuscator,
	kind: MessageTextKind,
	text: &str,
) -> String {
	match kind {
		MessageTextKind::User
		| MessageTextKind::Developer
		| MessageTextKind::ToolResult
		| MessageTextKind::AssistantReplay => obfuscator.obfuscate(text),
		MessageTextKind::System
		| MessageTextKind::Schema
		| MessageTextKind::Thinking
		| MessageTextKind::AssistantOutput
		| MessageTextKind::ToolArguments
		| MessageTextKind::ModelMetadata
		| MessageTextKind::Binary => text.to_owned(),
	}
}

/// Applies inbound restoration only to provider/model-authored fields.
#[must_use]
pub fn restore_message_text(
	obfuscator: &SecretObfuscator,
	kind: MessageTextKind,
	text: &str,
) -> String {
	match kind {
		MessageTextKind::AssistantOutput
		| MessageTextKind::ToolArguments
		| MessageTextKind::ModelMetadata => obfuscator.deobfuscate(text),
		MessageTextKind::System
		| MessageTextKind::Schema
		| MessageTextKind::Thinking
		| MessageTextKind::User
		| MessageTextKind::Developer
		| MessageTextKind::ToolResult
		| MessageTextKind::AssistantReplay
		| MessageTextKind::Binary => text.to_owned(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::rule::{SecretKind, SecretMode, SecretRule};

	fn obfuscator() -> SecretObfuscator {
		let rule = SecretRule::new(
			SecretKind::Plain,
			SecretMode::Obfuscate,
			"message-secret",
			None,
			None,
			None,
		)
		.expect("rule");
		SecretObfuscator::new(vec![rule], "K".repeat(43))
	}

	#[test]
	fn outbound_skips_control_reasoning_and_binary_fields() {
		let mut obfuscator = obfuscator();
		for kind in [
			MessageTextKind::System,
			MessageTextKind::Schema,
			MessageTextKind::Thinking,
			MessageTextKind::Binary,
		] {
			assert_eq!(
				obfuscate_message_text(&mut obfuscator, kind, "message-secret"),
				"message-secret"
			);
		}
		assert_ne!(
			obfuscate_message_text(&mut obfuscator, MessageTextKind::User, "message-secret"),
			"message-secret"
		);
	}

	#[test]
	fn inbound_skips_operator_authored_literals() {
		let mut obfuscator = obfuscator();
		let token = obfuscate_message_text(&mut obfuscator, MessageTextKind::User, "message-secret");
		assert_eq!(restore_message_text(&obfuscator, MessageTextKind::User, &token), token);
		assert_eq!(
			restore_message_text(&obfuscator, MessageTextKind::AssistantOutput, &token),
			"message-secret"
		);
	}
}
