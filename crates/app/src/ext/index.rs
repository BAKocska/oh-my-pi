//! Canonical signed native OMP extension index.

use std::{collections::BTreeSet, fs, path::Path};

use omp_core::Str;
use serde::{Deserialize, Serialize};

use super::{ExtensionCode, ExtensionError, trust::verify_signed_payload};

/// Current signed-index format.
pub const INDEX_VERSION: u32 = 1;

/// One explicit claim that an extension may shadow a bundled capability.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ShadowClaim {
	/// Capability family, such as `tool` or `agent`.
	pub kind: Str,
	/// Bundled capability name.
	pub name: Str,
}

/// One target-specific, hash-pinned wheel advertised by the index.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct IndexArtifact {
	/// Target triple, or `any` for a pure Python wheel.
	pub target:    Str,
	/// Artifact URL.
	pub url:       String,
	/// Wheel filename.
	pub file:      Str,
	/// Wheel compatibility tag.
	pub tag:       Str,
	/// Exact byte length.
	pub size:      u64,
	/// BLAKE3 digest prefixed by `b3:`.
	pub blake3:    Str,
	/// SHA-256 digest prefixed by `sha256:`.
	pub sha256:    Str,
	/// Publisher signature over both hashes and the capability digest.
	pub signature: Str,
}

/// An immutable extension release in the native index.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct IndexRelease {
	/// Exact PEP 440 release version.
	pub version:           Str,
	/// Canonical manifest BLAKE3 digest.
	pub manifest_digest:   Str,
	/// Digest of declared capabilities and hard-tool claims.
	pub capability_digest: Str,
	/// Whether index review/attestation completed.
	#[serde(default)]
	pub attested:          bool,
	/// Whether the release is yanked from new resolutions.
	#[serde(default)]
	pub yanked:            bool,
	/// Explicit bundled-name shadow claims.
	#[serde(default)]
	pub shadows:           Vec<ShadowClaim>,
	/// Target artifacts.
	pub artifacts:         Vec<IndexArtifact>,
}

/// One publisher-scoped extension identity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct IndexExtension {
	/// Stable extension identity.
	pub id:            Str,
	/// Python distribution name.
	pub distribution:  Str,
	/// Human-readable summary.
	#[serde(default)]
	pub description:   Str,
	/// Base64 Ed25519 publisher key.
	pub publisher_key: Str,
	/// Available immutable releases.
	pub releases:      Vec<IndexRelease>,
}

/// Signed canonical native index snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SignedIndex {
	/// Index format version.
	pub version:     u32,
	/// Stable index identity.
	pub name:        Str,
	/// RFC 3339 snapshot issuance time.
	pub issued_at:   Str,
	/// RFC 3339 snapshot expiry time.
	pub valid_until: Str,
	/// Extension catalog, sorted by id in canonical snapshots.
	pub extensions:  Vec<IndexExtension>,
	/// Detached Ed25519 signature from the configured index key.
	pub signature:   Str,
}

#[derive(Serialize)]
struct UnsignedIndex<'a> {
	version:     u32,
	name:        &'a Str,
	issued_at:   &'a Str,
	valid_until: &'a Str,
	extensions:  &'a [IndexExtension],
}

impl SignedIndex {
	/// Reads and validates a signed JSON index snapshot.
	pub fn read(path: &Path, index_key: &str) -> Result<Self, ExtensionError> {
		let bytes = fs::read(path)
			.map_err(|error| ExtensionError::new(ExtensionCode::EIntegrity, error.to_string()))?;
		let index: Self = serde_json::from_slice(&bytes)
			.map_err(|error| ExtensionError::new(ExtensionCode::EManifestParse, error.to_string()))?;
		index.verify(index_key)?;
		Ok(index)
	}

	/// Verifies version, canonical ordering, uniqueness, and the detached index
	/// signature. Signed bytes are canonical JSON of every field except
	/// `signature`.
	pub fn verify(&self, index_key: &str) -> Result<(), ExtensionError> {
		if self.version != INDEX_VERSION {
			return Err(ExtensionError::new(
				ExtensionCode::EManifestParse,
				"unsupported signed-index version",
			));
		}
		let mut previous: Option<&Str> = None;
		let mut ids = BTreeSet::new();
		for extension in &self.extensions {
			if extension.id.as_str().is_empty() || extension.publisher_key.as_str().is_empty() {
				return Err(ExtensionError::new(
					ExtensionCode::EManifestParse,
					"index extension has an empty identity or publisher key",
				));
			}
			if previous.is_some_and(|previous| previous >= &extension.id) || !ids.insert(&extension.id)
			{
				return Err(ExtensionError::new(
					ExtensionCode::EManifestParse,
					"index extensions are not uniquely sorted by id",
				));
			}
			previous = Some(&extension.id);
			let mut release_versions = BTreeSet::new();
			for release in &extension.releases {
				if !release_versions.insert(&release.version) {
					return Err(ExtensionError::new(
						ExtensionCode::EManifestParse,
						"index extension contains a duplicate release version",
					));
				}
				if release.artifacts.is_empty() {
					return Err(ExtensionError::new(
						ExtensionCode::ETargetMissing,
						"index release has no target artifacts",
					));
				}
				let mut targets = BTreeSet::new();
				for artifact in &release.artifacts {
					if !targets.insert(&artifact.target)
						|| !artifact.blake3.as_str().starts_with("b3:")
						|| !artifact.sha256.as_str().starts_with("sha256:")
						|| artifact.signature.as_str().is_empty()
					{
						return Err(ExtensionError::new(
							ExtensionCode::EManifestParse,
							"index release has duplicate targets or incomplete signed hashes",
						));
					}
				}
			}
		}
		let payload = serde_json::to_vec(&UnsignedIndex {
			version:     self.version,
			name:        &self.name,
			issued_at:   &self.issued_at,
			valid_until: &self.valid_until,
			extensions:  &self.extensions,
		})
		.map_err(|error| ExtensionError::new(ExtensionCode::ESig, error.to_string()))?;
		verify_signed_payload(index_key, &payload, self.signature.as_str())
	}

	/// Looks up one non-yanked exact release.
	#[must_use]
	pub fn release(&self, id: &str, version: &str) -> Option<(&IndexExtension, &IndexRelease)> {
		let extension = self
			.extensions
			.iter()
			.find(|extension| extension.id == id)?;
		let release = extension
			.releases
			.iter()
			.find(|release| release.version == version && !release.yanked)?;
		Some((extension, release))
	}

	/// Searches descriptions and identities in deterministic index order.
	pub fn search<'a>(
		&'a self,
		query: &'a str,
		capability_shadow: Option<&'a str>,
		attested_only: bool,
	) -> impl Iterator<Item = (&'a IndexExtension, &'a IndexRelease)> + 'a {
		let query = query.to_ascii_lowercase();
		self.extensions.iter().filter_map(move |extension| {
			if !extension.id.as_str().to_ascii_lowercase().contains(&query)
				&& !extension
					.description
					.as_str()
					.to_ascii_lowercase()
					.contains(&query)
			{
				return None;
			}
			let release = extension.releases.iter().rev().find(|release| {
				!release.yanked
					&& (!attested_only || release.attested)
					&& capability_shadow
						.is_none_or(|name| release.shadows.iter().any(|shadow| shadow.name == name))
			})?;
			Some((extension, release))
		})
	}
}

/// Requires every manifest shadow claim to have an exact user-configured
/// declaration. Index presence alone never changes built-in precedence.
pub fn validate_shadow_consent(
	release: &IndexRelease,
	configured: impl IntoIterator<Item = ShadowClaim>,
) -> Result<(), ExtensionError> {
	let configured: BTreeSet<(Str, Str)> = configured
		.into_iter()
		.map(|claim| (claim.kind, claim.name))
		.collect();
	if release
		.shadows
		.iter()
		.any(|claim| !configured.contains(&(claim.kind.clone(), claim.name.clone())))
	{
		return Err(ExtensionError::new(
			ExtensionCode::EConsent,
			"extension declares an unapproved built-in shadow",
		));
	}
	Ok(())
}
