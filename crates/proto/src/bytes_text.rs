//! Lossless text-oriented serde adapters for protobuf byte fields.
//!
//! Valid UTF-8 bytes serialize as their literal text unless that text starts
//! with `b64:`. Non-UTF-8 bytes and UTF-8 beginning with that reserved prefix
//! serialize as `b64:` followed by standard padded Base64. Deserialization
//! reverses that rule, so every byte sequence has one deterministic encoding.

use std::fmt;

use bytes::Bytes;
use omp_core::base64;
use serde::{
	Deserialize, Deserializer, Serialize, Serializer,
	de::{Unexpected, Visitor},
	ser::SerializeSeq,
};

/// Serializes one byte field using the lossless text encoding.
pub fn serialize<S>(value: &Bytes, serializer: S) -> Result<S::Ok, S::Error>
where
	S: Serializer,
{
	TextBytes(value).serialize(serializer)
}

/// Deserializes one byte field from the lossless text encoding.
pub fn deserialize<'de, D>(deserializer: D) -> Result<Bytes, D::Error>
where
	D: Deserializer<'de>,
{
	deserializer.deserialize_str(BytesVisitor)
}

/// Serde adapters for an optional protobuf byte field.
pub mod option {
	use super::*;

	/// Serializes an optional byte field using the lossless text encoding.
	pub fn serialize<S>(value: &Option<Bytes>, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		match value {
			Some(value) => serializer.serialize_some(&TextBytes(value)),
			None => serializer.serialize_none(),
		}
	}

	/// Deserializes an optional byte field from the lossless text encoding.
	pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Bytes>, D::Error>
	where
		D: Deserializer<'de>,
	{
		Option::<ByteString>::deserialize(deserializer).map(|value| value.map(|value| value.0))
	}
}

/// Serde adapters for a repeated protobuf byte field.
pub mod repeated {
	use super::*;

	/// Serializes repeated byte fields using the lossless text encoding.
	pub fn serialize<S>(values: &[Bytes], serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		let mut sequence = serializer.serialize_seq(Some(values.len()))?;
		for value in values {
			sequence.serialize_element(&TextBytes(value))?;
		}
		sequence.end()
	}

	/// Deserializes repeated byte fields from the lossless text encoding.
	pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<Bytes>, D::Error>
	where
		D: Deserializer<'de>,
	{
		Vec::<ByteString>::deserialize(deserializer)
			.map(|values| values.into_iter().map(|value| value.0).collect())
	}
}

struct TextBytes<'a>(&'a Bytes);

impl Serialize for TextBytes<'_> {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		match std::str::from_utf8(self.0) {
			Ok(text) if !text.starts_with("b64:") => serializer.serialize_str(text),
			_ => serializer.collect_str(&Base64Bytes(self.0)),
		}
	}
}

struct Base64Bytes<'a>(&'a [u8]);

impl fmt::Display for Base64Bytes<'_> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(formatter, "b64:{}", base64::encode(self.0))
	}
}

struct ByteString(Bytes);

impl<'de> Deserialize<'de> for ByteString {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>,
	{
		deserializer.deserialize_str(BytesVisitor).map(Self)
	}
}

struct BytesVisitor;

impl Visitor<'_> for BytesVisitor {
	type Value = Bytes;

	fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("a text or b64:-prefixed byte string")
	}

	fn visit_str<E>(self, text: &str) -> Result<Self::Value, E>
	where
		E: serde::de::Error,
	{
		if let Some(encoded) = text.strip_prefix("b64:") {
			return base64::decode(encoded).into_bytes().map_err(|_| {
				E::invalid_value(Unexpected::Str(text), &"valid padded standard Base64 after b64:")
			});
		}
		Ok(Bytes::copy_from_slice(text.as_bytes()))
	}
}
