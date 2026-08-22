//! Preflight-bounded protobuf encoding and encrypted relay envelopes.

use omp_proto::collab::v1::{CollabFrame, RelayEnvelope};
use prost::Message as _;
use strum::IntoStaticStr;
use thiserror::Error;

use crate::{
	PROTOCOL_REVISION,
	crypto::{CryptoError, NONCE_BYTES, RoomKey, TAG_BYTES},
};

/// Largest plaintext collaboration frame accepted before encryption.
pub const FRAME_MAX_BYTES: usize = 1024 * 1024;
/// Largest outer relay envelope, including nonce, tag, and protobuf overhead.
pub const ENVELOPE_MAX_BYTES: usize = FRAME_MAX_BYTES + NONCE_BYTES + TAG_BYTES + 64;
/// Largest individual nested length-delimited field.
pub const FIELD_MAX_BYTES: usize = 512 * 1024;
/// Largest number of length-delimited fields in one message.
pub const LENGTH_DELIMITED_MAX_COUNT: usize = 4096;
/// Largest repetition count for one length-delimited field.
pub const REPEATED_MAX_COUNT: usize = 1024;
/// Largest collaboration-message nesting depth accepted before decoding.
pub const PROTOBUF_MAX_DEPTH: usize = 16;

/// Clear relay-visible routing metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RelayRoute {
	/// Zero means host/broadcast; positive values identify one relay peer.
	pub peer_id: u32,
}

/// Decoded routing metadata and authenticated frame.
#[derive(Debug)]
pub struct RoutedFrame {
	/// Cleartext relay route.
	pub route: RelayRoute,
	/// Authenticated inner protobuf frame.
	pub frame: CollabFrame,
}

/// Encodes, encrypts, and wraps one collaboration frame.
pub fn encode_envelope(
	key: &RoomKey,
	route: RelayRoute,
	frame: &CollabFrame,
) -> Result<Vec<u8>, CodecError> {
	ensure_revision("CollabFrame", frame.protocol_revision)?;
	let encoded_len = frame.encoded_len();
	if encoded_len > FRAME_MAX_BYTES {
		return Err(CodecError::FrameTooLarge { actual: encoded_len, limit: FRAME_MAX_BYTES });
	}
	let mut plaintext = Vec::with_capacity(encoded_len);
	frame
		.encode(&mut plaintext)
		.expect("Vec encoding is infallible");
	preflight(&plaintext, Node::CollabFrame, FRAME_MAX_BYTES)?;
	let sealed_frame = key.seal(&plaintext)?;
	let envelope = RelayEnvelope {
		protocol_revision: PROTOCOL_REVISION,
		peer_id:           route.peer_id,
		sealed_frame:      sealed_frame.into(),
	};
	let envelope_len = envelope.encoded_len();
	if envelope_len > ENVELOPE_MAX_BYTES {
		return Err(CodecError::EnvelopeTooLarge {
			actual: envelope_len,
			limit:  ENVELOPE_MAX_BYTES,
		});
	}
	Ok(envelope.encode_to_vec())
}

/// Preflights, decodes, authenticates, and preflights the inner frame.
pub fn decode_envelope(key: &RoomKey, encoded: &[u8]) -> Result<RoutedFrame, CodecError> {
	preflight(encoded, Node::RelayEnvelope, ENVELOPE_MAX_BYTES)?;
	let envelope = RelayEnvelope::decode(encoded).map_err(CodecError::DecodeEnvelope)?;
	ensure_revision("RelayEnvelope", envelope.protocol_revision)?;
	let plaintext = key.open(&envelope.sealed_frame)?;
	preflight(&plaintext, Node::CollabFrame, FRAME_MAX_BYTES)?;
	let frame = CollabFrame::decode(plaintext.as_slice()).map_err(CodecError::DecodeFrame)?;
	ensure_revision("CollabFrame", frame.protocol_revision)?;
	Ok(RoutedFrame { route: RelayRoute { peer_id: envelope.peer_id }, frame })
}

/// Refuses a malformed or over-bounds encoded collaboration frame before prost
/// allocation.
pub fn validate_collab_frame(encoded: &[u8]) -> Result<(), CodecError> {
	preflight(encoded, Node::CollabFrame, FRAME_MAX_BYTES)
}

fn ensure_revision(message: &'static str, actual: u32) -> Result<(), CodecError> {
	if actual == PROTOCOL_REVISION {
		Ok(())
	} else {
		Err(CodecError::UnsupportedRevision { message, actual, supported: PROTOCOL_REVISION })
	}
}

/// Collaboration wire decoding and protocol refusal.
#[derive(Debug, Error)]
pub enum CodecError {
	/// An encoded plaintext frame exceeds its pre-encryption ceiling.
	#[error("collaboration frame is {actual} bytes; limit is {limit}")]
	FrameTooLarge {
		/// Actual byte count.
		actual: usize,
		/// Accepted byte count.
		limit:  usize,
	},
	/// An outer envelope exceeds its transport ceiling.
	#[error("relay envelope is {actual} bytes; limit is {limit}")]
	EnvelopeTooLarge {
		/// Actual byte count.
		actual: usize,
		/// Accepted byte count.
		limit:  usize,
	},
	/// A length-delimited field exceeds the allocation bound.
	#[error("{message} field {field} is {actual} bytes; limit is {limit}")]
	FieldTooLarge {
		/// Containing protobuf message.
		message: &'static str,
		/// Field number.
		field:   u32,
		/// Declared field length.
		actual:  usize,
		/// Accepted field length.
		limit:   usize,
	},
	/// One message contains too many allocating fields.
	#[error("{message} has {actual} length-delimited fields; limit is {limit}")]
	TooManyFields {
		/// Containing message.
		message: &'static str,
		/// Observed count.
		actual:  usize,
		/// Accepted count.
		limit:   usize,
	},
	/// A repeated field contains too many elements.
	#[error("{message} field {field} has {actual} values; limit is {limit}")]
	TooManyRepeated {
		/// Containing message.
		message: &'static str,
		/// Field number.
		field:   u32,
		/// Observed count.
		actual:  usize,
		/// Accepted count.
		limit:   usize,
	},
	/// Known nested messages exceed the stack/decode depth bound.
	#[error("collaboration protobuf nesting depth is {actual}; limit is {limit}")]
	TooDeep {
		/// First rejected depth.
		actual: usize,
		/// Accepted depth.
		limit:  usize,
	},
	/// The protobuf wire encoding is malformed.
	#[error("malformed collaboration protobuf at byte {offset}")]
	Malformed {
		/// Byte offset at which parsing failed.
		offset: usize,
	},
	/// A peer requested a revision this binary does not implement.
	#[error("{message} protocol revision {actual} is unsupported; expected {supported}")]
	UnsupportedRevision {
		/// Refusing message type.
		message:   &'static str,
		/// Received revision.
		actual:    u32,
		/// Sole supported revision.
		supported: u32,
	},
	/// The outer protobuf could not be decoded after preflight.
	#[error("relay envelope protobuf decode failed")]
	DecodeEnvelope(#[source] prost::DecodeError),
	/// The authenticated inner protobuf could not be decoded after preflight.
	#[error("collaboration frame protobuf decode failed")]
	DecodeFrame(#[source] prost::DecodeError),
	/// Encryption or authentication failed.
	#[error("collaboration frame cryptography failed")]
	Crypto(#[from] CryptoError),
}

#[derive(Clone, Copy, IntoStaticStr)]
enum Node {
	RelayEnvelope,
	CollabFrame,
	Hello,
	Welcome,
	SessionHeader,
	Bye,
	#[strum(serialize = "ErrorMessage")]
	Error,
	#[strum(serialize = "SnapshotChunk")]
	Snapshot,
	#[strum(serialize = "JournalRecord")]
	Journal,
	#[strum(serialize = "CompactionRecord")]
	Compaction,
	#[strum(serialize = "ModelChangeRecord")]
	ModelChange,
	#[strum(serialize = "ThinkingLevelChangeRecord")]
	ThinkingChange,
	#[strum(serialize = "CustomRecord")]
	Custom,
	StreamEvent,
	ToolExecution,
	Notice,
	#[strum(serialize = "SessionStateUpdate")]
	SessionState,
	ModelMetadata,
	ContextUsage,
	Participant,
	#[strum(serialize = "RegistrySnapshot")]
	Registry,
	AgentSummary,
	#[strum(serialize = "PromptRequest")]
	Prompt,
	#[strum(serialize = "ImageAttachment")]
	Image,
	#[strum(serialize = "AbortRequest")]
	Abort,
	AgentCommand,
	UiRequest,
	SelectSpec,
	SelectOption,
	EditorSpec,
	UiRequestEnd,
	UiResponse,
	TranscriptRequest,
	TranscriptChunk,
	BusEvent,
	#[strum(serialize = "OpaqueImportedMessage")]
	Opaque,
}

impl Node {
	const fn child(self, field: u32) -> Option<Self> {
		match (self, field) {
			(Self::CollabFrame, 10) => Some(Self::Hello),
			(Self::CollabFrame, 11) => Some(Self::Welcome),
			(Self::CollabFrame, 12) => Some(Self::Bye),
			(Self::CollabFrame, 13) => Some(Self::Error),
			(Self::CollabFrame, 15) => Some(Self::Opaque),
			(Self::CollabFrame, 20) => Some(Self::Snapshot),
			(Self::CollabFrame, 21) => Some(Self::Journal),
			(Self::CollabFrame, 22) => Some(Self::StreamEvent),
			(Self::CollabFrame, 23) => Some(Self::SessionState),
			(Self::CollabFrame, 24) => Some(Self::Registry),
			(Self::CollabFrame, 30) => Some(Self::Prompt),
			(Self::CollabFrame, 31) => Some(Self::Abort),
			(Self::CollabFrame, 32) => Some(Self::AgentCommand),
			(Self::CollabFrame, 33) => Some(Self::UiResponse),
			(Self::CollabFrame, 34) => Some(Self::TranscriptRequest),
			(Self::CollabFrame, 40) => Some(Self::UiRequest),
			(Self::CollabFrame, 41) => Some(Self::UiRequestEnd),
			(Self::CollabFrame, 42) => Some(Self::TranscriptChunk),
			(Self::CollabFrame, 43) => Some(Self::BusEvent),
			(Self::Welcome, 2) => Some(Self::SessionHeader),
			(Self::Welcome, 3) => Some(Self::SessionState),
			(Self::Welcome, 4) => Some(Self::Registry),
			(Self::Snapshot, 1) => Some(Self::Journal),
			(Self::Journal, 4) => Some(Self::Opaque),
			(Self::Journal, 5) => Some(Self::Compaction),
			(Self::Journal, 6) => Some(Self::ModelChange),
			(Self::Journal, 7) => Some(Self::ThinkingChange),
			(Self::Journal, 8) => Some(Self::Custom),
			(Self::StreamEvent, 2) => Some(Self::Opaque),
			(Self::StreamEvent, 3) => Some(Self::ToolExecution),
			(Self::StreamEvent, 4) => Some(Self::Notice),
			(Self::SessionState, 6) => Some(Self::ModelMetadata),
			(Self::SessionState, 8) => Some(Self::ContextUsage),
			(Self::SessionState, 9) => Some(Self::Participant),
			(Self::Registry, 1) => Some(Self::AgentSummary),
			(Self::Prompt, 2) => Some(Self::Image),
			(Self::UiRequest, 3) => Some(Self::SelectSpec),
			(Self::UiRequest, 4) => Some(Self::EditorSpec),
			(Self::SelectSpec, 1) => Some(Self::SelectOption),
			_ => None,
		}
	}
}

fn preflight(encoded: &[u8], node: Node, maximum: usize) -> Result<(), CodecError> {
	if encoded.len() > maximum {
		return if matches!(node, Node::RelayEnvelope) {
			Err(CodecError::EnvelopeTooLarge { actual: encoded.len(), limit: maximum })
		} else {
			Err(CodecError::FrameTooLarge { actual: encoded.len(), limit: maximum })
		};
	}
	scan_message(encoded, node, 0, 0)
}

fn scan_message(encoded: &[u8], node: Node, depth: usize, base: usize) -> Result<(), CodecError> {
	if depth > PROTOBUF_MAX_DEPTH {
		return Err(CodecError::TooDeep { actual: depth, limit: PROTOBUF_MAX_DEPTH });
	}
	if matches!(node, Node::Opaque) {
		return Ok(());
	}
	let mut cursor = 0;
	let mut length_count = 0;
	let mut occurrences = [0_u16; 64];
	while cursor < encoded.len() {
		let key_offset = cursor;
		let key = read_varint(encoded, &mut cursor, base)?;
		let field = u32::try_from(key >> 3)
			.map_err(|_| CodecError::Malformed { offset: base + key_offset })?;
		if field == 0 {
			return Err(CodecError::Malformed { offset: base + key_offset });
		}
		match key & 7 {
			0 => {
				read_varint(encoded, &mut cursor, base)?;
			},
			1 => advance(encoded, &mut cursor, 8, base)?,
			2 => {
				length_count += 1;
				if length_count > LENGTH_DELIMITED_MAX_COUNT {
					return Err(CodecError::TooManyFields {
						message: node.into(),
						actual:  length_count,
						limit:   LENGTH_DELIMITED_MAX_COUNT,
					});
				}
				let length_offset = cursor;
				let length = usize::try_from(read_varint(encoded, &mut cursor, base)?)
					.map_err(|_| CodecError::Malformed { offset: base + length_offset })?;
				let limit = if matches!(node, Node::RelayEnvelope) && field == 3 {
					FRAME_MAX_BYTES + NONCE_BYTES + TAG_BYTES
				} else {
					FIELD_MAX_BYTES
				};
				if length > limit {
					return Err(CodecError::FieldTooLarge {
						message: node.into(),
						field,
						actual: length,
						limit,
					});
				}
				if let Ok(slot) = usize::try_from(field)
					&& let Some(count) = occurrences.get_mut(slot)
				{
					*count = count.saturating_add(1);
					if usize::from(*count) > REPEATED_MAX_COUNT {
						return Err(CodecError::TooManyRepeated {
							message: node.into(),
							field,
							actual: usize::from(*count),
							limit: REPEATED_MAX_COUNT,
						});
					}
				}
				let start = cursor;
				advance(encoded, &mut cursor, length, base)?;
				if matches!(node, Node::SelectSpec) && field == 4 {
					let count = count_packed_varints(&encoded[start..cursor], base + start)?;
					if count > REPEATED_MAX_COUNT {
						return Err(CodecError::TooManyRepeated {
							message: node.into(),
							field,
							actual: count,
							limit: REPEATED_MAX_COUNT,
						});
					}
				}

				if let Some(child) = node.child(field) {
					scan_message(&encoded[start..cursor], child, depth + 1, base + start)?;
				}
			},
			5 => advance(encoded, &mut cursor, 4, base)?,
			_ => return Err(CodecError::Malformed { offset: base + key_offset }),
		}
	}
	Ok(())
}

fn read_varint(encoded: &[u8], cursor: &mut usize, base: usize) -> Result<u64, CodecError> {
	let start = *cursor;
	let mut value = 0_u64;
	for shift in (0..70).step_by(7) {
		let byte = *encoded
			.get(*cursor)
			.ok_or(CodecError::Malformed { offset: base + start })?;
		*cursor += 1;
		if shift == 63 && byte > 1 {
			break;
		}
		value |= u64::from(byte & 0x7f) << shift;
		if byte & 0x80 == 0 {
			return Ok(value);
		}
	}
	Err(CodecError::Malformed { offset: base + start })
}

fn count_packed_varints(encoded: &[u8], base: usize) -> Result<usize, CodecError> {
	let mut cursor = 0;
	let mut count = 0;
	while cursor < encoded.len() {
		read_varint(encoded, &mut cursor, base)?;
		count += 1;
	}
	Ok(count)
}
fn advance(
	encoded: &[u8],
	cursor: &mut usize,
	count: usize,
	base: usize,
) -> Result<(), CodecError> {
	let next = cursor
		.checked_add(count)
		.filter(|next| *next <= encoded.len())
		.ok_or(CodecError::Malformed { offset: base + *cursor })?;
	*cursor = next;
	Ok(())
}
#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn refuses_declared_field_before_allocation() {
		// CollabFrame field 10 (Hello), declaring 524,289 bytes with no payload.
		let encoded = [0x52, 0x81, 0x80, 0x20];
		assert!(matches!(
			validate_collab_frame(&encoded),
			Err(CodecError::FieldTooLarge { message: "CollabFrame", field: 10, .. })
		));
	}

	#[test]
	fn envelope_round_trip_and_revision_refusal() {
		let (key, _) = RoomKey::generate().unwrap();
		let frame =
			CollabFrame { protocol_revision: PROTOCOL_REVISION, sequence: 9, ..Default::default() };
		let encoded = encode_envelope(&key, RelayRoute { peer_id: 7 }, &frame).unwrap();
		let decoded = decode_envelope(&key, &encoded).unwrap();
		assert_eq!(decoded.route.peer_id, 7);
		assert_eq!(decoded.frame.sequence, 9);
		let old = CollabFrame { protocol_revision: 3, ..Default::default() };
		assert!(matches!(
			encode_envelope(&key, RelayRoute { peer_id: 0 }, &old),
			Err(CodecError::UnsupportedRevision { actual: 3, .. })
		));
	}
	#[test]
	fn refuses_packed_repetition_before_decode() {
		let mut encoded = vec![0x22, 0x81, 0x08];
		encoded.resize(3 + REPEATED_MAX_COUNT + 1, 0);
		assert!(matches!(
			scan_message(&encoded, Node::SelectSpec, 0, 0),
			Err(CodecError::TooManyRepeated {
				message: "SelectSpec",
				field: 4,
				actual,
				..
			}) if actual == REPEATED_MAX_COUNT + 1
		));
	}
}
