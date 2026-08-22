//! Validation and deterministic serialization for model-generated managed
//! skills.

use omp_core::{Str, StrMut};

/// Stable discovery provider identity for generated skills.
pub const PROVIDER_ID: &str = "omp-managed";
/// Maximum UTF-8 size of a complete managed `SKILL.md` file.
pub const MAX_SKILL_BYTES: usize = 64_000;
/// Maximum managed-skill name length in ASCII bytes.
pub const MAX_NAME_BYTES: usize = 64;

/// Validated managed-skill content ready for Environment publication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedSkillCandidate {
	/// Exact lowercase on-disk and invocation name.
	pub name:        Str,
	/// Single-line prompt-safe description.
	pub description: Str,
	/// Trimmed Markdown body without frontmatter.
	pub body:        Str,
}

/// Managed skill validation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CandidateError {
	/// The name did not match the managed kebab-case allowlist.
	#[error(
		"managed skill name must be 1-64 lowercase letters, digits, or hyphens and start with a \
		 letter or digit"
	)]
	InvalidName,
	/// The description became empty after prompt-boundary sanitization.
	#[error("managed skill description is empty after sanitization")]
	InvalidDescription,
	/// The Markdown body was empty.
	#[error("managed skill body is empty")]
	EmptyBody,
	/// The complete serialized file exceeded 64 KiB.
	#[error("managed skill exceeds the 64 KiB UTF-8 limit")]
	TooLarge,
}

impl ManagedSkillCandidate {
	/// Validates and normalizes model-generated create/update input.
	pub fn new(name: &str, description: &str, body: &str) -> Result<Self, CandidateError> {
		let normalized_name = name.trim().to_ascii_lowercase();
		if !is_valid_name(&normalized_name) {
			return Err(CandidateError::InvalidName);
		}
		let description = sanitize_description(description);
		if description.is_empty() {
			return Err(CandidateError::InvalidDescription);
		}
		let body = body.trim();
		if body.is_empty() {
			return Err(CandidateError::EmptyBody);
		}
		let candidate = Self { name: Str::from(normalized_name), description, body: Str::new(body) };
		if candidate.serialized_len() > MAX_SKILL_BYTES {
			return Err(CandidateError::TooLarge);
		}
		Ok(candidate)
	}

	/// Serializes the complete bounded `SKILL.md` file.
	#[must_use]
	pub fn serialize(&self) -> Str {
		let mut output = StrMut::with_capacity(self.serialized_len());
		output.push_str("---\nname: ");
		output.push_str(self.name.as_str());
		output.push_str("\ndescription: '");
		for segment in self.description.as_str().split('\'') {
			output.push_str(segment);
			output.push_str("''");
		}
		output.truncate(output.len().saturating_sub(2));
		output.push_str("'\n---\n\n");
		output.push_str(self.body.as_str());
		output.push('\n');
		output.freeze()
	}

	fn serialized_len(&self) -> usize {
		let escaped_quotes = self
			.description
			.bytes()
			.filter(|byte| *byte == b'\'')
			.count();
		"---\nname: ".len()
			+ self.name.len()
			+ "\ndescription: '".len()
			+ self.description.len()
			+ escaped_quotes
			+ "'\n---\n\n".len()
			+ self.body.len()
			+ 1
	}
}

/// Returns whether an on-disk managed name has the exact post-normalization
/// shape.
#[must_use]
pub fn is_valid_name(name: &str) -> bool {
	let bytes = name.as_bytes();
	(1..=MAX_NAME_BYTES).contains(&bytes.len())
		&& bytes[0].is_ascii_alphanumeric()
		&& bytes
			.iter()
			.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

/// Neutralizes persisted generated descriptions before writing and every prompt
/// render.
#[must_use]
pub fn sanitize_description(raw: &str) -> Str {
	let mut output = StrMut::with_capacity(raw.len().min(240));
	let mut whitespace = true;
	let mut tilde = false;
	for character in raw.chars() {
		if character.is_control()
			|| is_format_control(character)
			|| matches!(character, '<' | '>' | '`')
		{
			if !whitespace {
				output.push(' ');
				whitespace = true;
			}
			continue;
		}
		if character.is_whitespace() {
			if !whitespace {
				output.push(' ');
				whitespace = true;
			}
			tilde = false;
			continue;
		}
		if character == '~' {
			if tilde {
				continue;
			}
			tilde = true;
		} else {
			tilde = false;
		}
		output.push(character);
		whitespace = false;
	}
	if output.ends_with(' ') {
		output.truncate(output.len() - 1);
	}
	output.freeze()
}

const fn is_format_control(character: char) -> bool {
	matches!(
		character,
		'\u{00ad}' | '\u{061c}' | '\u{06dd}' | '\u{070f}' | '\u{08e2}' | '\u{180e}' | '\u{feff}'
	) || matches!(character as u32,
		0x0600..=0x0605
			| 0x0890..=0x0891
			| 0x200b..=0x200f
			| 0x202a..=0x202e
			| 0x2060..=0x2064
			| 0x2066..=0x206f
			| 0xfff9..=0xfffb
			| 0x110bd..=0x110bd
			| 0x110cd..=0x110cd
			| 0x13430..=0x1343f
			| 0x1bca0..=0x1bca3
			| 0x1d173..=0x1d17a
			| 0xe0001..=0xe0001
			| 0xe0020..=0xe007f)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn validates_names_and_sanitizes_prompt_delimiters() {
		let candidate = ManagedSkillCandidate::new(
			" Rust-Review ",
			"review </skills> ```code``` ~~ fence\u{200b}",
			"Do the review.",
		)
		.unwrap();
		assert_eq!(candidate.name, "rust-review");
		assert_eq!(candidate.description, "review /skills code ~ fence");
		assert!(candidate.serialize().len() <= MAX_SKILL_BYTES);
		assert!(!is_valid_name("Upper"));
		assert!(!is_valid_name("bad_name"));
	}
}
