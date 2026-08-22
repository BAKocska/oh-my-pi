//! Bounded retained-frame validation and exact-key storage.

use std::collections::BTreeMap;

use omp_core::{IntoStr, Str};
use omp_proto::omp::ui::v1::{
	FrameActionFired, RetainedFrame, RetainedFrameEnvelope, RetainedFrameKey,
	retained_frame_envelope,
};
use prost::Message as _;
use thiserror::Error;

/// Maximum encoded size accepted for one retained-frame envelope.
pub const MAX_FRAME_ENVELOPE_BYTES: usize = 512 * 1024;
/// Maximum typed payload bytes retained by one frame.
pub const MAX_FRAME_PAYLOAD_BYTES: usize = 256 * 1024;
/// Maximum generic TML fallback bytes retained by one frame.
pub const MAX_FRAME_FALLBACK_BYTES: usize = 256 * 1024;
/// Maximum actions declared by one frame.
pub const MAX_FRAME_ACTIONS: usize = 32;
/// Maximum frames retained by one store.
pub const MAX_RETAINED_FRAMES: usize = 2_048;

const MAX_KIND_BYTES: usize = 64;
const MAX_REV_BYTES: usize = 64;
const MAX_STABLE_ID_BYTES: usize = 256;
const MAX_ACTION_NAME_BYTES: usize = 64;
const MAX_CORRELATION_BYTES: usize = 128;

/// Exact retained-frame identity. No kind-only or revision-only lookup exists.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FrameIdentity {
	kind:      Str,
	rev:       Str,
	stable_id: Str,
}

impl FrameIdentity {
	/// Borrows the semantic frame kind.
	#[must_use]
	pub fn kind(&self) -> &str {
		self.kind.as_str()
	}

	/// Borrows the schema revision.
	#[must_use]
	pub fn rev(&self) -> &str {
		self.rev.as_str()
	}

	/// Borrows the producer-stable frame identity.
	#[must_use]
	pub fn stable_id(&self) -> &str {
		self.stable_id.as_str()
	}
}

/// Result of applying one ordered frame envelope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FrameMutation {
	/// A frame was inserted or replaced in place.
	Upserted(FrameIdentity),
	/// An exact frame key was removed. The flag reports whether it existed.
	Removed {
		/// Removed exact identity.
		identity: FrameIdentity,
		/// Whether a retained frame existed for the key.
		existed:  bool,
	},
}

/// Deterministic rejection of malformed or oversized retained UI data.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum FrameError {
	/// The envelope has no typed mutation.
	#[error("retained-frame envelope has no mutation")]
	MissingMutation,
	/// The protobuf envelope exceeds its encoded bound.
	#[error("retained-frame envelope exceeds the encoded size bound")]
	EnvelopeTooLarge,
	/// A frame or removal has no exact key.
	#[error("retained-frame mutation has no key")]
	MissingKey,
	/// A required identity field is empty.
	#[error("retained-frame identity fields must not be empty")]
	EmptyIdentity,
	/// An identity field exceeds its byte bound.
	#[error("retained-frame identity exceeds its byte bound")]
	IdentityTooLarge,
	/// The typed payload exceeds its byte bound.
	#[error("retained-frame payload exceeds its byte bound")]
	PayloadTooLarge,
	/// A frame omitted the deterministic generic TML fallback.
	#[error("retained-frame requires a generic TML fallback")]
	MissingFallback,
	/// Generic TML fallback source exceeds its byte bound.
	#[error("retained-frame fallback exceeds its byte bound")]
	FallbackTooLarge,
	/// A frame declares too many actions.
	#[error("retained-frame declares too many actions")]
	TooManyActions,
	/// An action name or correlation is empty or exceeds its byte bound.
	#[error("retained-frame action identity is invalid")]
	InvalidAction,
	/// An action correlation is duplicated within one frame.
	#[error("retained-frame action correlation is duplicated")]
	DuplicateActionCorrelation,
	/// The store's hard frame capacity was reached.
	#[error("retained-frame store capacity reached")]
	Capacity,
	/// A fired action does not identify a retained frame.
	#[error("retained-frame action targets an unknown frame")]
	UnknownActionFrame,
	/// A fired action does not match the exact declared name and correlation.
	#[error("retained-frame action does not match its declaration")]
	ActionMismatch,
}

/// Exact-key retained frame store with bounded ingress and deterministic
/// fallback.
#[derive(Default)]
pub struct RetainedFrames {
	frames: BTreeMap<FrameIdentity, RetainedFrame>,
}

impl RetainedFrames {
	/// Creates an empty retained-frame store.
	#[must_use]
	pub fn new() -> Self {
		Self::default()
	}

	/// Returns the retained frame count.
	#[must_use]
	pub fn len(&self) -> usize {
		self.frames.len()
	}

	/// Reports whether no frames are retained.
	#[must_use]
	pub fn is_empty(&self) -> bool {
		self.frames.is_empty()
	}

	/// Borrows one frame by its exact `(kind, rev, stable_id)` identity.
	#[must_use]
	pub fn get(&self, identity: &FrameIdentity) -> Option<&RetainedFrame> {
		self.frames.get(identity)
	}

	/// Applies one validated ordered envelope.
	pub fn apply(&mut self, envelope: RetainedFrameEnvelope) -> Result<FrameMutation, FrameError> {
		if envelope.encoded_len() > MAX_FRAME_ENVELOPE_BYTES {
			return Err(FrameError::EnvelopeTooLarge);
		}
		match envelope.mutation.ok_or(FrameError::MissingMutation)? {
			retained_frame_envelope::Mutation::Upsert(frame) => {
				let identity = validate_frame(&frame)?;
				if !self.frames.contains_key(&identity) && self.frames.len() == MAX_RETAINED_FRAMES {
					return Err(FrameError::Capacity);
				}
				self.frames.insert(identity.clone(), frame);
				Ok(FrameMutation::Upserted(identity))
			},
			retained_frame_envelope::Mutation::Remove(remove) => {
				let identity = validate_key(remove.key.as_ref())?;
				let existed = self.frames.remove(&identity).is_some();
				Ok(FrameMutation::Removed { identity, existed })
			},
		}
	}

	/// Validates a fired action against the exact retained declaration.
	pub fn validate_action(&self, fired: &FrameActionFired) -> Result<(), FrameError> {
		let identity = validate_key(fired.key.as_ref())?;
		let frame = self
			.frames
			.get(&identity)
			.ok_or(FrameError::UnknownActionFrame)?;
		let matched = frame
			.actions
			.iter()
			.any(|action| action.name == fired.name && action.correlation == fired.correlation);
		if matched {
			Ok(())
		} else {
			Err(FrameError::ActionMismatch)
		}
	}
}

fn validate_frame(frame: &RetainedFrame) -> Result<FrameIdentity, FrameError> {
	let identity = validate_key(frame.key.as_ref())?;
	if frame.payload.len() > MAX_FRAME_PAYLOAD_BYTES {
		return Err(FrameError::PayloadTooLarge);
	}
	let fallback = frame.fallback.as_ref().ok_or(FrameError::MissingFallback)?;
	if fallback.source.len() > MAX_FRAME_FALLBACK_BYTES {
		return Err(FrameError::FallbackTooLarge);
	}
	if frame.actions.len() > MAX_FRAME_ACTIONS {
		return Err(FrameError::TooManyActions);
	}
	let mut correlations = BTreeMap::<&str, ()>::new();
	for action in &frame.actions {
		if action.name.is_empty()
			|| action.name.len() > MAX_ACTION_NAME_BYTES
			|| action.correlation.is_empty()
			|| action.correlation.len() > MAX_CORRELATION_BYTES
		{
			return Err(FrameError::InvalidAction);
		}
		if correlations
			.insert(action.correlation.as_str(), ())
			.is_some()
		{
			return Err(FrameError::DuplicateActionCorrelation);
		}
	}
	Ok(identity)
}

fn validate_key(key: Option<&RetainedFrameKey>) -> Result<FrameIdentity, FrameError> {
	let key = key.ok_or(FrameError::MissingKey)?;
	if key.kind.is_empty() || key.rev.is_empty() || key.stable_id.is_empty() {
		return Err(FrameError::EmptyIdentity);
	}
	if key.kind.len() > MAX_KIND_BYTES
		|| key.rev.len() > MAX_REV_BYTES
		|| key.stable_id.len() > MAX_STABLE_ID_BYTES
	{
		return Err(FrameError::IdentityTooLarge);
	}
	Ok(FrameIdentity {
		kind:      key.kind.as_str().to_str(),
		rev:       key.rev.as_str().to_str(),
		stable_id: key.stable_id.as_str().to_str(),
	})
}

#[cfg(test)]
mod tests {
	use bytes::Bytes;
	use omp_proto::omp::ui::v1::{
		FrameActionFired, RemoveRetainedFrame, RetainedFrame, RetainedFrameAction,
		RetainedFrameEnvelope, RetainedFrameKey, Tml, retained_frame_envelope,
	};

	use super::{FrameError, FrameMutation, RetainedFrames};

	fn key(rev: &str) -> RetainedFrameKey {
		RetainedFrameKey {
			kind:      "diagnostic".into(),
			rev:       rev.into(),
			stable_id: "turn:4:event:9".into(),
		}
	}

	fn upsert(rev: &str, payload: &'static [u8], fallback: &'static [u8]) -> RetainedFrameEnvelope {
		RetainedFrameEnvelope {
			mutation: Some(retained_frame_envelope::Mutation::Upsert(RetainedFrame {
				key:      Some(key(rev)),
				payload:  Bytes::from_static(payload),
				fallback: Some(Tml { source: Bytes::from_static(fallback), hash: 7 }),
				actions:  vec![RetainedFrameAction {
					name:        "open".into(),
					correlation: "open:9".into(),
					args:        None,
				}],
			})),
		}
	}

	#[test]
	fn exact_revision_updates_in_place_and_unknown_revision_keeps_fallback() {
		let mut frames = RetainedFrames::new();
		let first = frames.apply(upsert("v99", br#"{"n":1}"#, b"<text>generic</text>"));
		let identity = match first.expect("first frame") {
			FrameMutation::Upserted(identity) => identity,
			FrameMutation::Removed { .. } => panic!("unexpected removal"),
		};
		frames
			.apply(upsert("v99", br#"{"n":2}"#, b"<text>updated</text>"))
			.expect("replace exact key");
		assert_eq!(frames.len(), 1);
		assert_eq!(
			frames
				.get(&identity)
				.and_then(|frame| frame.fallback.as_ref())
				.map(|tml| &tml.source[..]),
			Some(&b"<text>updated</text>"[..])
		);
	}

	#[test]
	fn malformed_and_oversized_frames_fail_boundedly() {
		let mut frames = RetainedFrames::new();
		let missing = RetainedFrameEnvelope {
			mutation: Some(retained_frame_envelope::Mutation::Upsert(RetainedFrame {
				key:      Some(key("v1")),
				payload:  Bytes::new(),
				fallback: None,
				actions:  Vec::new(),
			})),
		};
		assert_eq!(frames.apply(missing), Err(FrameError::MissingFallback));

		let mut oversized = upsert("v1", b"", b"");
		let Some(retained_frame_envelope::Mutation::Upsert(frame)) = oversized.mutation.as_mut()
		else {
			unreachable!()
		};
		frame.payload = Bytes::from(vec![0; super::MAX_FRAME_PAYLOAD_BYTES + 1]);
		assert_eq!(frames.apply(oversized), Err(FrameError::PayloadTooLarge));
	}

	#[test]
	fn actions_require_exact_key_name_and_correlation() {
		let mut frames = RetainedFrames::new();
		frames
			.apply(upsert("v1", b"{}", b"fallback"))
			.expect("frame");
		let mut fired = FrameActionFired {
			key:         Some(key("v1")),
			name:        "open".into(),
			correlation: "open:9".into(),
			args:        None,
		};
		frames.validate_action(&fired).expect("declared action");
		fired.correlation = "open:other".into();
		assert_eq!(frames.validate_action(&fired), Err(FrameError::ActionMismatch));
	}

	#[test]
	fn removal_is_exact_revision_only() {
		let mut frames = RetainedFrames::new();
		frames
			.apply(upsert("v1", b"{}", b"fallback"))
			.expect("frame");
		let removed = frames
			.apply(RetainedFrameEnvelope {
				mutation: Some(retained_frame_envelope::Mutation::Remove(RemoveRetainedFrame {
					key: Some(key("v2")),
				})),
			})
			.expect("remove unknown exact key");
		assert!(matches!(removed, FrameMutation::Removed { existed: false, .. }));
		assert_eq!(frames.len(), 1);
	}
}
