//! Secret values with a non-serializing, redacted public surface.
//!
//! [`Secret`] provides callback-scoped access to byte secrets.
//! [`SecretString`] and [`SecretBox`] provide the import-compatible
//! [`ExposeSecret`] surface used when a direct secret borrow is required.

use std::fmt;

use zeroize::{Zeroize, Zeroizing};

/// Borrows the plaintext held by a secret wrapper.
///
/// Callers should keep the returned borrow short-lived and must not include
/// it in diagnostics.
pub trait ExposeSecret<T: ?Sized> {
	/// Returns a shared borrow of the wrapped plaintext.
	fn expose_secret(&self) -> &T;
}

/// An owned UTF-8 secret that is wiped on drop and redacted in diagnostics.
///
/// This type intentionally implements neither [`fmt::Display`] nor
/// serialization traits.
pub struct SecretString {
	inner: Zeroizing<String>,
}

impl From<String> for SecretString {
	fn from(secret: String) -> Self {
		Self { inner: Zeroizing::new(secret) }
	}
}

impl From<&str> for SecretString {
	fn from(secret: &str) -> Self {
		Self::from(secret.to_owned())
	}
}

impl Clone for SecretString {
	fn clone(&self) -> Self {
		Self::from(self.inner.as_str())
	}
}

impl ExposeSecret<str> for SecretString {
	fn expose_secret(&self) -> &str {
		self.inner.as_str()
	}
}

impl fmt::Debug for SecretString {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("SecretString([REDACTED])")
	}
}

/// An owned boxed secret that is wiped on drop and redacted in diagnostics.
///
/// `T` may be unsized, matching ordinary [`Box`] ownership.
pub struct SecretBox<T: Zeroize + ?Sized> {
	inner: Box<T>,
}

impl<T: Zeroize + ?Sized> SecretBox<T> {
	/// Wraps owned boxed secret material.
	#[must_use]
	pub const fn new(secret: Box<T>) -> Self {
		Self { inner: secret }
	}
}

impl<T: Zeroize + ?Sized> Clone for SecretBox<T>
where
	Box<T>: Clone,
{
	fn clone(&self) -> Self {
		Self::new(self.inner.clone())
	}
}

impl<T: Zeroize + ?Sized> ExposeSecret<T> for SecretBox<T> {
	fn expose_secret(&self) -> &T {
		&self.inner
	}
}

impl<T: Zeroize + ?Sized> fmt::Debug for SecretBox<T> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("SecretBox([REDACTED])")
	}
}

impl<T: Zeroize + ?Sized> Drop for SecretBox<T> {
	fn drop(&mut self) {
		self.inner.as_mut().zeroize();
	}
}

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
	use std::sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	};

	use zeroize::Zeroize;

	use super::{ExposeSecret, Secret, SecretBox, SecretString};

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

	#[test]
	fn string_diagnostics_redact_and_exposure_round_trips() {
		let material = "must-not-appear";
		let secret = SecretString::from(material);
		let debug = format!("{secret:?}");

		assert!(!debug.contains(material));
		assert_eq!(secret.expose_secret(), material);
	}

	#[test]
	fn string_clones_have_independent_storage() {
		let original = SecretString::from("independent");
		let cloned = original.clone();

		assert_eq!(cloned.expose_secret(), original.expose_secret());
		assert_ne!(cloned.expose_secret().as_ptr(), original.expose_secret().as_ptr());
		drop(original);
		assert_eq!(cloned.expose_secret(), "independent");
	}

	#[test]
	fn boxed_diagnostics_redact_and_exposure_round_trips() {
		let material = b"boxed-must-not-appear".to_vec();
		let secret = SecretBox::new(Box::new(material.clone()));
		let debug = format!("{secret:?}");

		assert!(!debug.contains("boxed-must-not-appear"));
		assert_eq!(secret.expose_secret(), &material);
	}

	#[test]
	fn boxed_clones_have_independent_storage() {
		let original = SecretBox::new(Box::new(b"independent".to_vec()));
		let cloned = original.clone();

		assert_eq!(cloned.expose_secret(), original.expose_secret());
		assert_ne!(cloned.expose_secret().as_ptr(), original.expose_secret().as_ptr());
		drop(original);
		assert_eq!(cloned.expose_secret(), b"independent");
	}

	#[derive(Clone)]
	struct DropProbe {
		zeroized: Arc<AtomicBool>,
	}

	impl Zeroize for DropProbe {
		fn zeroize(&mut self) {
			self.zeroized.store(true, Ordering::Relaxed);
		}
	}

	#[test]
	fn boxed_secret_zeroizes_on_drop() {
		let zeroized = Arc::new(AtomicBool::new(false));
		let secret = SecretBox::new(Box::new(DropProbe { zeroized: Arc::clone(&zeroized) }));

		drop(secret);

		assert!(zeroized.load(Ordering::Relaxed));
	}
}
