//! Secret bytes with a non-serializing, redacted public surface.

use std::fmt;

use zeroize::Zeroize;

/// Opaque secret bytes that redact in diagnostics and are wiped on drop.
///
/// The bytes can only be borrowed for the duration of [`Self::expose`]. This
/// type intentionally implements neither `Serialize` nor `Deserialize`; wire
/// sealing owns the only serialization boundary.
pub struct Secret {
	bytes: Vec<u8>,
}

impl Secret {
	/// Wraps owned secret bytes.
	#[must_use]
	pub const fn new(bytes: Vec<u8>) -> Self {
		Self { bytes }
	}

	/// Borrows the secret bytes only for the duration of `f`.
	pub fn expose<T>(&self, f: impl FnOnce(&[u8]) -> T) -> T {
		f(&self.bytes)
	}

	/// Returns the secret length without exposing its contents.
	#[must_use]
	pub const fn len(&self) -> usize {
		self.bytes.len()
	}

	/// Returns whether this secret is empty without exposing its contents.
	#[must_use]
	pub const fn is_empty(&self) -> bool {
		self.bytes.is_empty()
	}
}

impl From<Vec<u8>> for Secret {
	fn from(bytes: Vec<u8>) -> Self {
		Self::new(bytes)
	}
}

impl fmt::Debug for Secret {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("Secret(<redacted>)")
	}
}

impl fmt::Display for Secret {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("<redacted>")
	}
}

impl Drop for Secret {
	fn drop(&mut self) {
		self.bytes.zeroize();
	}
}

#[cfg(test)]
mod tests {
	use super::Secret;

	#[test]
	fn diagnostics_never_expose_secret_bytes() {
		let secret = Secret::from(b"must-not-appear".to_vec());
		assert_eq!(format!("{secret:?}"), "Secret(<redacted>)");
		assert_eq!(secret.to_string(), "<redacted>");
	}

	#[test]
	fn exposure_is_scoped_to_the_callback() {
		let secret = Secret::from(b"value".to_vec());
		assert_eq!(secret.expose(|bytes| bytes.len()), 5);
	}
}
