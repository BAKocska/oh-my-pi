//! Room identifiers, credentials, and AES-256-GCM frame sealing.

use ring::{
	aead::{self, Aad, LessSafeKey, Nonce, UnboundKey},
	rand::{SecureRandom as _, SystemRandom},
};
use thiserror::Error;

/// Number of bytes in a room identifier.
pub const ROOM_ID_BYTES: usize = 16;
/// Number of bytes in an AES-256 room key.
pub const ROOM_KEY_BYTES: usize = 32;
/// Number of bytes in a writable-guest token.
pub const WRITE_TOKEN_BYTES: usize = 16;
/// Number of nonce bytes prefixed to each sealed frame.
pub const NONCE_BYTES: usize = 12;
/// AES-GCM authentication tag length.
pub const TAG_BYTES: usize = 16;

/// A random relay room identifier.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RoomId([u8; ROOM_ID_BYTES]);

impl RoomId {
	/// Generates a cryptographically random room identifier.
	pub fn generate() -> Result<Self, CryptoError> {
		let mut bytes = [0; ROOM_ID_BYTES];
		SystemRandom::new()
			.fill(&mut bytes)
			.map_err(|_| CryptoError::Random)?;
		Ok(Self(bytes))
	}

	/// Returns the fixed-size identifier bytes.
	#[must_use]
	pub const fn as_bytes(&self) -> &[u8; ROOM_ID_BYTES] {
		&self.0
	}
}

/// A writable-guest bearer credential.
#[derive(Clone, Debug)]
pub struct WriteToken([u8; WRITE_TOKEN_BYTES]);

impl WriteToken {
	/// Generates a cryptographically random write token.
	pub fn generate() -> Result<Self, CryptoError> {
		let mut bytes = [0; WRITE_TOKEN_BYTES];
		SystemRandom::new()
			.fill(&mut bytes)
			.map_err(|_| CryptoError::Random)?;
		Ok(Self(bytes))
	}

	/// Imports an exact-width token.
	#[must_use]
	pub const fn from_bytes(bytes: [u8; WRITE_TOKEN_BYTES]) -> Self {
		Self(bytes)
	}

	/// Returns the token bytes for credential link construction.
	#[must_use]
	pub const fn as_bytes(&self) -> &[u8; WRITE_TOKEN_BYTES] {
		&self.0
	}

	/// Compares an untrusted candidate without data-dependent early exit.
	#[must_use]
	pub fn matches(&self, candidate: &[u8]) -> bool {
		let mut difference = candidate.len() ^ WRITE_TOKEN_BYTES;
		for (index, expected) in self.0.iter().copied().enumerate() {
			difference |= usize::from(expected ^ candidate.get(index).copied().unwrap_or(0));
		}
		difference == 0
	}
}

/// Opaque owner of a native AES-256-GCM key.
pub struct RoomKey(LessSafeKey);

impl RoomKey {
	/// Generates a cryptographically random AES-256-GCM room key.
	pub fn generate() -> Result<(Self, [u8; ROOM_KEY_BYTES]), CryptoError> {
		let mut raw = [0; ROOM_KEY_BYTES];
		SystemRandom::new()
			.fill(&mut raw)
			.map_err(|_| CryptoError::Random)?;
		let key = Self::from_bytes(raw)?;
		Ok((key, raw))
	}

	/// Imports an exact-width AES-256-GCM key and retains no export API.
	pub fn from_bytes(raw: [u8; ROOM_KEY_BYTES]) -> Result<Self, CryptoError> {
		let key = UnboundKey::new(&aead::AES_256_GCM, &raw).map_err(|_| CryptoError::InvalidKey)?;
		Ok(Self(LessSafeKey::new(key)))
	}

	/// Seals bytes with a fresh nonce, returning `[12-byte
	/// nonce][ciphertext][tag]`.
	pub fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
		let mut nonce = [0; NONCE_BYTES];
		SystemRandom::new()
			.fill(&mut nonce)
			.map_err(|_| CryptoError::Random)?;
		let mut sealed = Vec::with_capacity(NONCE_BYTES + plaintext.len() + TAG_BYTES);
		sealed.extend_from_slice(&nonce);
		sealed.extend_from_slice(plaintext);
		let tag = self
			.0
			.seal_in_place_separate_tag(
				Nonce::assume_unique_for_key(nonce),
				Aad::empty(),
				&mut sealed[NONCE_BYTES..],
			)
			.map_err(|_| CryptoError::Seal)?;
		sealed.extend_from_slice(tag.as_ref());
		Ok(sealed)
	}

	/// Authenticates and opens a nonce-prefixed sealed frame.
	pub fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, CryptoError> {
		if sealed.len() < NONCE_BYTES + TAG_BYTES {
			return Err(CryptoError::SealedFrameTooShort { actual: sealed.len() });
		}
		let (nonce, ciphertext) = sealed.split_at(NONCE_BYTES);
		let nonce: [u8; NONCE_BYTES] = nonce.try_into().expect("split has exact nonce width");
		let mut plaintext = ciphertext.to_vec();
		let opened = self
			.0
			.open_in_place(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut plaintext)
			.map_err(|_| CryptoError::Tampered)?;
		let len = opened.len();
		plaintext.truncate(len);
		Ok(plaintext)
	}
}

/// Cryptographic frame failure. Authentication failures are terminal relay
/// faults.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CryptoError {
	/// The operating-system random source failed.
	#[error("secure random source failed")]
	Random,
	/// The AES key could not be imported.
	#[error("invalid AES-256-GCM room key")]
	InvalidKey,
	/// AES-GCM sealing failed.
	#[error("AES-256-GCM frame sealing failed")]
	Seal,
	/// A sealed frame omitted a complete nonce and authentication tag.
	#[error("sealed frame is {actual} bytes; minimum is 28")]
	SealedFrameTooShort {
		/// Observed frame length.
		actual: usize,
	},
	/// Authentication failed because the key or sealed bytes were altered.
	#[error("sealed frame authentication failed")]
	Tampered,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn seal_open_round_trip_and_tamper_fault() {
		let (key, raw) = RoomKey::generate().unwrap();
		assert_eq!(raw.len(), ROOM_KEY_BYTES);
		let mut sealed = key.seal(b"ordered protobuf frame").unwrap();
		assert_eq!(sealed.len(), NONCE_BYTES + 22 + TAG_BYTES);
		assert_eq!(key.open(&sealed).unwrap(), b"ordered protobuf frame");
		sealed[NONCE_BYTES] ^= 1;
		assert_eq!(key.open(&sealed), Err(CryptoError::Tampered));
		assert_eq!(key.open(&sealed[..20]), Err(CryptoError::SealedFrameTooShort { actual: 20 }));
	}

	#[test]
	fn token_check_requires_exact_timing_safe_match() {
		let token = WriteToken::from_bytes([7; WRITE_TOKEN_BYTES]);
		assert!(token.matches(&[7; WRITE_TOKEN_BYTES]));
		assert!(!token.matches(&[7; WRITE_TOKEN_BYTES - 1]));
		let mut wrong = [7; WRITE_TOKEN_BYTES];
		wrong[WRITE_TOKEN_BYTES - 1] = 8;
		assert!(!token.matches(&wrong));
	}
	#[test]
	fn generated_room_material_has_exact_widths() {
		let room_id = RoomId::generate().unwrap();
		let token = WriteToken::generate().unwrap();
		let (_, raw_key) = RoomKey::generate().unwrap();
		assert_eq!(room_id.as_bytes().len(), ROOM_ID_BYTES);
		assert_eq!(token.as_bytes().len(), WRITE_TOKEN_BYTES);
		assert_eq!(raw_key.len(), ROOM_KEY_BYTES);
	}
}
