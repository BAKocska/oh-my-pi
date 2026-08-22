//! Session attachment indexing and provider-boundary image policy.

use bytes::Bytes;
use omp_core::{Str, sf};
#[cfg(test)]
use omp_proto::thread::v1::Item;
use omp_proto::{
	inference::v1 as inference,
	thread::v1::{self as thread, Thread},
};
use thiserror::Error;

/// Hard upper bound for one transient input image before normalization.
pub const MAX_TRANSIENT_IMAGE_BYTES: usize = 20 * 1024 * 1024;
/// Conservative image-count floor for providers without a declared override.
pub const DEFAULT_PROVIDER_IMAGE_BUDGET: usize = 5;

/// One image addressable as `attachment://N` in the latest user message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Attachment {
	/// One-based display label.
	pub label: Str,
	/// Canonical session-local URI.
	pub uri:   Str,
	/// Immutable blob descriptor. Inline bytes may be absent when blob storage
	/// owns them.
	pub blob:  thread::Blob,
}

/// Latest-message attachment index used by tools and prompt projection.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AttachmentIndex {
	entries: Vec<Attachment>,
}

impl AttachmentIndex {
	/// Indexes image blobs from the latest user message containing at least one.
	#[must_use]
	pub fn from_thread(thread: &Thread) -> Self {
		let Some(blobs) = thread
			.items
			.iter()
			.rev()
			.find_map(|item| match item.kind.as_ref() {
				Some(thread::item::Kind::Message(message))
					if thread::Role::try_from(message.role) == Ok(thread::Role::User) =>
				{
					let blobs: Vec<_> = message
						.parts
						.iter()
						.filter_map(|part| match part.kind.as_ref() {
							Some(thread::part::Kind::Blob(blob)) if blob.mime.starts_with("image/") => {
								Some(blob.clone())
							},
							_ => None,
						})
						.collect();
					(!blobs.is_empty()).then_some(blobs)
				},
				_ => None,
			})
		else {
			return Self::default();
		};
		let entries = blobs
			.into_iter()
			.enumerate()
			.map(|(index, blob)| {
				let number = index + 1;
				Attachment { label: sf!("Image #{number}"), uri: sf!("attachment://{number}"), blob }
			})
			.collect();
		Self { entries }
	}

	/// Returns indexed attachments in positional order.
	#[must_use]
	pub fn entries(&self) -> &[Attachment] {
		&self.entries
	}

	/// Resolves an exact one-based attachment URI.
	pub fn resolve(&self, resource: &str) -> Result<&Attachment, AttachmentError> {
		let raw = resource
			.strip_prefix("attachment://")
			.ok_or(AttachmentError::InvalidUri)?;
		if raw.is_empty() || raw.bytes().any(|byte| !byte.is_ascii_digit()) {
			return Err(AttachmentError::InvalidUri);
		}
		let index = raw
			.parse::<usize>()
			.map_err(|_| AttachmentError::InvalidUri)?;
		self
			.entries
			.get(index.saturating_sub(1))
			.ok_or(AttachmentError::NotFound { index })
	}
}

/// Normalized image supplied by the environment-owned image processor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NormalizedAttachmentImage {
	/// Encoded bytes after any resizing or format conversion.
	pub bytes:      Bytes,
	/// Media type matching the encoded bytes.
	pub media_type: Str,
}

/// Attachment normalization failure.
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum AttachmentError {
	/// The URI is not an exact `attachment://N` address.
	#[error("invalid attachment URI")]
	InvalidUri,
	/// The one-based attachment position does not exist.
	#[error("attachment {index} does not exist")]
	NotFound {
		/// Missing one-based position.
		index: usize,
	},
	/// Inline input exceeded the hard pre-decode bound.
	#[error("transient image is too large: {bytes} bytes exceeds {max_bytes}")]
	TooLarge {
		/// Observed bytes.
		bytes:     usize,
		/// Enforced bytes.
		max_bytes: usize,
	},
}

/// Normalizes inline images in the latest attachment-bearing user message.
///
/// The environment owns decoding and resizing; `normalize` is called only when
/// auto-resize is enabled. The agent enforces the hard byte bound before either
/// path and updates blob metadata atomically with returned bytes.
pub fn normalize_latest_inline_images<E>(
	thread: &mut Thread,
	auto_resize: bool,
	mut normalize: impl FnMut(Bytes) -> Result<Option<NormalizedAttachmentImage>, E>,
) -> Result<(), NormalizeAttachmentError<E>> {
	let Some(message) = thread
		.items
		.iter_mut()
		.rev()
		.find_map(|item| match item.kind.as_mut() {
			Some(thread::item::Kind::Message(message))
				if thread::Role::try_from(message.role) == Ok(thread::Role::User)
					&& message.parts.iter().any(is_image_part) =>
			{
				Some(message)
			},
			_ => None,
		})
	else {
		return Ok(());
	};
	for part in &mut message.parts {
		let Some(thread::part::Kind::Blob(blob)) = part.kind.as_mut() else {
			continue;
		};
		if !blob.mime.starts_with("image/") || blob.inline.is_empty() {
			continue;
		}
		if blob.inline.len() > MAX_TRANSIENT_IMAGE_BYTES {
			return Err(NormalizeAttachmentError::Policy(AttachmentError::TooLarge {
				bytes:     blob.inline.len(),
				max_bytes: MAX_TRANSIENT_IMAGE_BYTES,
			}));
		}
		if !auto_resize {
			continue;
		}
		if let Some(normalized) =
			normalize(blob.inline.clone()).map_err(NormalizeAttachmentError::Processor)?
		{
			blob.size = normalized.bytes.len() as u64;
			blob.inline = normalized.bytes;
			blob.mime = normalized.media_type.into();
		}
	}
	Ok(())
}

/// Failure from attachment policy or the injected image processor.
#[derive(Debug, Error)]
pub enum NormalizeAttachmentError<E> {
	/// Agent-owned size and addressing policy failed.
	#[error(transparent)]
	Policy(#[from] AttachmentError),
	/// Environment-owned image normalization failed.
	#[error("image normalization failed")]
	Processor(E),
}

/// Returns the conservative per-request image count for a provider.
#[must_use]
pub fn provider_image_budget(provider: Option<&str>) -> usize {
	match provider {
		Some("anthropic" | "amazon-bedrock" | "openrouter") => 90,
		Some("openai" | "openai-codex" | "google" | "google-vertex" | "google-gemini-cli") => 200,
		Some("umans") => 10,
		_ => DEFAULT_PROVIDER_IMAGE_BUDGET,
	}
}

/// Drops oldest transient images until the provider count limit is met.
///
/// Tool-result messages that lose their only part receive a visible omission
/// marker so provider message shape remains valid.
#[must_use]
pub fn clamp_provider_images(thread: &mut Thread, provider: Option<&str>) -> usize {
	let budget = provider_image_budget(provider);
	let total = thread
		.items
		.iter()
		.filter_map(|item| item.kind.as_ref())
		.map(|kind| match kind {
			thread::item::Kind::Message(message) => message
				.parts
				.iter()
				.filter(|part| is_image_part(part))
				.count(),
			thread::item::Kind::ToolResult(result) => result
				.parts
				.iter()
				.filter(|part| is_image_part(part))
				.count(),
			thread::item::Kind::ToolCall(_) => 0,
		})
		.sum::<usize>();
	let mut remaining = total.saturating_sub(budget);
	let dropped = remaining;
	for item in &mut thread.items {
		if remaining == 0 {
			break;
		}
		match item.kind.as_mut() {
			Some(thread::item::Kind::Message(message)) => {
				message.parts.retain(|part| {
					if remaining != 0 && is_image_part(part) {
						remaining -= 1;
						false
					} else {
						true
					}
				});
			},
			Some(thread::item::Kind::ToolResult(result)) => {
				result.parts.retain(|part| {
					if remaining != 0 && is_image_part(part) {
						remaining -= 1;
						false
					} else {
						true
					}
				});
				if result.parts.is_empty() {
					result.parts.push(thread::Part {
						kind: Some(thread::part::Kind::Text(
							"[image omitted: provider image limit]".to_owned(),
						)),
					});
				}
			},
			Some(thread::item::Kind::ToolCall(_)) | None => {},
		}
	}
	dropped
}

/// Replaces image parts with a hidden text companion for a text-only model.
///
/// `descriptions` are positional against [`AttachmentIndex`]. Missing
/// descriptions produce a bounded neutral marker rather than exposing binary
/// data to a model that cannot consume it.
#[must_use]
pub fn describe_images_for_text_model(
	thread: &mut Thread,
	model_accepts_images: bool,
	descriptions: &[Str],
) -> usize {
	if model_accepts_images {
		return 0;
	}
	let mut ordinal = 0_usize;
	let mut replaced = 0_usize;
	for item in &mut thread.items {
		let Some(thread::item::Kind::Message(message)) = item.kind.as_mut() else {
			continue;
		};
		let mut parts = Vec::with_capacity(message.parts.len());
		for part in message.parts.drain(..) {
			if is_image_part(&part) {
				let number = ordinal + 1;
				let description = descriptions.get(ordinal).map_or_else(
					|| sf!("[Image #{number}: description unavailable]"),
					|text| sf!("[Image #{number}: {}]", text.as_str()),
				);
				parts.push(thread::Part { kind: Some(thread::part::Kind::Text(description.into())) });
				ordinal += 1;
				replaced += 1;
			} else {
				parts.push(part);
			}
		}
		message.parts = parts;
		if replaced != 0 {
			item
				.props
				.get_or_insert_default()
				.fields
				.insert("omp/hidden-image-description".to_owned(), inference::Value {
					kind: Some(inference::value::Kind::Bool(true)),
				});
		}
	}
	replaced
}

fn is_image_part(part: &thread::Part) -> bool {
	matches!(part.kind.as_ref(), Some(thread::part::Kind::Blob(blob)) if blob.mime.starts_with("image/"))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn image(byte: u8) -> thread::Part {
		thread::Part {
			kind: Some(thread::part::Kind::Blob(thread::Blob {
				hash: vec![byte; 32].into(),
				mime: "image/png".to_owned(),
				size: 1,
				inline: Bytes::from(vec![byte]),
				..Default::default()
			})),
		}
	}

	fn user(parts: Vec<thread::Part>) -> Item {
		Item {
			seq:           0,
			created_at_ms: 0,
			kind:          Some(thread::item::Kind::Message(thread::Message {
				role: thread::Role::User as i32,
				parts,
			})),
			props:         None,
		}
	}

	#[test]
	fn latest_user_images_are_one_based_and_exactly_addressed() {
		let thread = Thread { items: vec![user(vec![image(1)]), user(vec![image(2), image(3)])] };
		let index = AttachmentIndex::from_thread(&thread);
		assert_eq!(index.entries().len(), 2);
		assert_eq!(index.resolve("attachment://2").unwrap().label, "Image #2");
		assert_eq!(index.resolve("attachment://0"), Err(AttachmentError::NotFound { index: 0 }));
		assert_eq!(index.resolve("attachment://2?q=x"), Err(AttachmentError::InvalidUri));
	}

	#[test]
	fn provider_clamp_drops_oldest_images() {
		let mut parts = Vec::new();
		for byte in 0..7 {
			parts.push(image(byte));
		}
		let mut thread = Thread { items: vec![user(parts)] };
		assert_eq!(clamp_provider_images(&mut thread, Some("unknown")), 2);
		let index = AttachmentIndex::from_thread(&thread);
		assert_eq!(index.entries().len(), 5);
		assert_eq!(index.entries()[0].blob.inline, Bytes::from_static(&[2]));
	}

	#[test]
	fn descriptions_are_only_applied_for_text_models() {
		let original = Thread { items: vec![user(vec![image(1)])] };
		let mut vision = original.clone();
		assert_eq!(describe_images_for_text_model(&mut vision, true, &["chart".into()]), 0);
		assert_eq!(vision, original);
		let mut text = original;
		assert_eq!(describe_images_for_text_model(&mut text, false, &["chart".into()]), 1);
		assert!(
			text.items[0]
				.props
				.as_ref()
				.unwrap()
				.fields
				.contains_key("omp/hidden-image-description")
		);
	}
}
