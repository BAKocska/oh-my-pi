//! Authenticated actor identity and extension provenance carried by durable
//! records.

use std::{fmt, hash::{Hash, Hasher}, str::FromStr, sync::Arc};

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
#[derive(Clone)]
pub struct Provenance(Arc<ProvenanceData>);

#[derive(PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
struct ProvenanceData {
	publisher:       Str,
	extension_id:    Str,
	version:         Str,
	artifact_digest: ArtifactDigest,
	layer:           Str,
	tier:            Str,
	generation:      u64,
}

const _: () = assert!(
	std::mem::size_of::<Provenance>() <= 16,
	"Provenance must stay compact"
);

impl Provenance {
	/// Creates provenance from core-authenticated extension installation facts.
	#[must_use]
	pub fn new(
		publisher: Str,
		extension_id: Str,
		version: Str,
		artifact_digest: ArtifactDigest,
		layer: Str,
		tier: Str,
		generation: u64,
	) -> Self {
		Self(Arc::new(ProvenanceData {
			publisher,
			extension_id,
			version,
			artifact_digest,
			layer,
			tier,
			generation,
		}))
	}

	/// Returns the publisher key fingerprint.
	#[must_use]
	pub fn publisher(&self) -> &str {
		self.0.publisher.as_str()
	}

	/// Returns the dotted extension identifier.
	#[must_use]
	pub fn extension_id(&self) -> &str {
		self.0.extension_id.as_str()
	}

	/// Returns the exact extension version.
	#[must_use]
	pub fn version(&self) -> &str {
		self.0.version.as_str()
	}

	/// Returns the exact extension artifact digest.
	#[must_use]
	pub fn artifact_digest(&self) -> ArtifactDigest {
		self.0.artifact_digest
	}

	/// Returns the installation layer.
	#[must_use]
	pub fn layer(&self) -> &str {
		self.0.layer.as_str()
	}

	/// Returns the conferred trust tier.
	#[must_use]
	pub fn tier(&self) -> &str {
		self.0.tier.as_str()
	}

	/// Returns the host incarnation generation.
	#[must_use]
	pub fn generation(&self) -> u64 {
		self.0.generation
	}
}

impl fmt::Debug for Provenance {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("Provenance")
			.field("publisher", &self.0.publisher)
			.field("extension_id", &self.0.extension_id)
			.field("version", &self.0.version)
			.field("artifact_digest", &self.0.artifact_digest)
			.field("layer", &self.0.layer)
			.field("tier", &self.0.tier)
			.field("generation", &self.0.generation)
			.finish()
	}
}

impl PartialEq for Provenance {
	fn eq(&self, other: &Self) -> bool {
		self.0 == other.0
	}
}

impl Eq for Provenance {}

impl PartialOrd for Provenance {
	fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
		Some(self.cmp(other))
	}
}

impl Ord for Provenance {
	fn cmp(&self, other: &Self) -> std::cmp::Ordering {
		self.0.cmp(&other.0)
	}
}

impl Hash for Provenance {
	fn hash<H: Hasher>(&self, state: &mut H) {
		self.0.hash(state);
	}
}

impl Serialize for Provenance {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		self.0.serialize(serializer)
	}
}

impl<'de> Deserialize<'de> for Provenance {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>,
	{
		ProvenanceData::deserialize(deserializer).map(|data| Self(Arc::new(data)))
	}
}
