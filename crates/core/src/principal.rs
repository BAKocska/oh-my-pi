//! Authenticated actor identity and extension provenance carried by durable
//! records.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use thiserror::Error;

use crate::{Str, hex};

/// The authenticated person acting through an omp daemon.
///
/// A principal is derived by the core from the authenticated connection. Its
/// identifier is intentionally redacted from [`Debug`] output so durable error
/// paths cannot accidentally disclose account identity.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Principal {
	id:      Str,
	display: Str,
}

impl Principal {
	/// Creates an authenticated principal from its stable identifier and safe
	/// human-readable display name.
	#[must_use]
	pub const fn new(id: Str, display: Str) -> Self {
		Self { id, display }
	}

	/// Returns the stable principal identifier.
	#[must_use]
	pub fn id(&self) -> &str {
		self.id.as_str()
	}

	/// Returns the human-readable principal name intended for UI surfaces.
	#[must_use]
	pub fn display(&self) -> &str {
		self.display.as_str()
	}
}

impl fmt::Debug for Principal {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("Principal")
			.field("id", &"[redacted]")
			.field("display", &self.display)
			.finish()
	}
}

/// A BLAKE3-256 digest identifying the exact extension artifact that acted.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArtifactDigest([u8; 32]);

impl ArtifactDigest {
	/// Creates an artifact digest from its raw BLAKE3-256 bytes.
	#[must_use]
	pub const fn new(bytes: [u8; 32]) -> Self {
		Self(bytes)
	}

	/// Returns the raw BLAKE3-256 digest bytes.
	#[must_use]
	pub const fn as_bytes(&self) -> &[u8; 32] {
		&self.0
	}

	/// Consumes the digest and returns its raw bytes.
	#[must_use]
	pub const fn into_bytes(self) -> [u8; 32] {
		self.0
	}
}

impl fmt::Display for ArtifactDigest {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("b3:")?;
		fmt::Display::fmt(&hex::encode(&self.0), formatter)
	}
}

impl fmt::Debug for ArtifactDigest {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		fmt::Display::fmt(self, formatter)
	}
}

/// Failure to parse an extension artifact digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ArtifactDigestError {
	/// The digest did not use the canonical `b3:` prefix.
	#[error("artifact digest must start with `b3:`")]
	MissingPrefix,
	/// The hexadecimal payload was not exactly 32 bytes.
	#[error("artifact digest must contain exactly 64 lowercase hexadecimal digits")]
	InvalidLength,
	/// The hexadecimal payload was not canonical lowercase hexadecimal.
	#[error("artifact digest contains a non-lowercase-hexadecimal character")]
	InvalidHex,
}

impl FromStr for ArtifactDigest {
	type Err = ArtifactDigestError;

	fn from_str(value: &str) -> Result<Self, Self::Err> {
		let encoded = value
			.strip_prefix("b3:")
			.ok_or(ArtifactDigestError::MissingPrefix)?;
		if encoded.len() != 64 {
			return Err(ArtifactDigestError::InvalidLength);
		}
		if !encoded
			.bytes()
			.all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
		{
			return Err(ArtifactDigestError::InvalidHex);
		}
		let bytes = <[u8; 32]>::try_from(hex::decode(encoded.as_bytes()))
			.map_err(|_| ArtifactDigestError::InvalidHex)?;
		Ok(Self(bytes))
	}
}

impl Serialize for ArtifactDigest {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		serializer.collect_str(self)
	}
}

impl<'de> Deserialize<'de> for ArtifactDigest {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>,
	{
		let value = Str::deserialize(deserializer)?;
		value.as_str().parse().map_err(D::Error::custom)
	}
}

/// Core-stamped identity of the exact extension incarnation that acted.
///
/// The seven fields are the publisher, extension id, version, artifact digest,
/// installation layer, trust tier, and host generation. Workers may observe
/// this value but must never be trusted to author it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Provenance {
	publisher:       Str,
	extension_id:    Str,
	version:         Str,
	artifact_digest: ArtifactDigest,
	layer:           Str,
	tier:            Str,
	generation:      u64,
}

impl Provenance {
	/// Creates provenance from core-authenticated extension installation facts.
	#[must_use]
	pub const fn new(
		publisher: Str,
		extension_id: Str,
		version: Str,
		artifact_digest: ArtifactDigest,
		layer: Str,
		tier: Str,
		generation: u64,
	) -> Self {
		Self { publisher, extension_id, version, artifact_digest, layer, tier, generation }
	}

	/// Returns the publisher key fingerprint.
	#[must_use]
	pub fn publisher(&self) -> &str {
		self.publisher.as_str()
	}

	/// Returns the dotted extension identifier.
	#[must_use]
	pub fn extension_id(&self) -> &str {
		self.extension_id.as_str()
	}

	/// Returns the exact extension version.
	#[must_use]
	pub fn version(&self) -> &str {
		self.version.as_str()
	}

	/// Returns the exact extension artifact digest.
	#[must_use]
	pub const fn artifact_digest(&self) -> ArtifactDigest {
		self.artifact_digest
	}

	/// Returns the installation layer.
	#[must_use]
	pub fn layer(&self) -> &str {
		self.layer.as_str()
	}

	/// Returns the conferred trust tier.
	#[must_use]
	pub fn tier(&self) -> &str {
		self.tier.as_str()
	}

	/// Returns the host incarnation generation.
	#[must_use]
	pub const fn generation(&self) -> u64 {
		self.generation
	}
}
