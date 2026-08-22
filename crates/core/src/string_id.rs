/// Declares a branded string identifier with an owned form (`Name`, backed by
/// [`Str`](crate::Str)) and a zero-cost borrowed form (`Name<str>`), mirroring
/// `PathBuf`/`Path`. Owned form: storage — fields, map keys, moves. Borrowed
/// form: queries — brand a `&str` with `Name::from_ref` without allocating.
/// Consumers must depend on `serde`.
///
/// # Locked ownership convention
///
/// Bare `Name` is always the owned, `Str`-backed form. Only explicit
/// `Name<str>` is borrowed. Callers adapt to this convention; the macro must
/// never flip its default type parameter.
///
/// ```
/// omp_core::string_id!(/// Test id.
///     ModelKey);
/// fn lookup(_key: &ModelKey<str>) {}
/// let owned: ModelKey = ModelKey::new("m");
/// lookup(&owned);
/// lookup(ModelKey::from_ref("m"));
/// ```
///
/// Cross-brand misuse is rejected at compile time:
/// ```compile_fail
/// omp_core::string_id!(/// Test id.
///     ModelKey);
/// omp_core::string_id!(/// Test id.
///     RouteId);
/// fn lookup(_key: &ModelKey<str>) {}
/// let key: ModelKey = ModelKey::new("m");
/// lookup(&key);
/// lookup(RouteId::from_ref("r")); // RouteId is not a ModelKey
/// ```
#[macro_export]
macro_rules! string_id {
	($(#[$meta:meta])* $name:ident) => {
		$(#[$meta])*
		#[derive(Eq, Hash, Ord, PartialEq, PartialOrd)]
		#[repr(transparent)]
		pub struct $name<T: ?Sized = $crate::Str>(T);

		// LOCKED RULING: bare `$name` must remain the owned `Str` form. If the
		// default ever flips to `str`, `size_of::<$name>()` stops compiling.
		const _: () = assert!(
			::std::mem::size_of::<$name>() == ::std::mem::size_of::<$crate::Str>(),
			"bare string_id must remain the owned Str-backed form"
		);

		impl $name<$crate::Str> {
			/// Creates an identifier from stored text.
			#[inline]
			pub fn new(value: impl $crate::IntoStr) -> Self {
				Self(value.into_str())
			}

			/// Creates an empty identifier without allocating.
			#[inline]
			pub const fn empty() -> Self {
				Self($crate::Str::empty())
			}

			/// Creates an identifier from static text without allocating;
			/// `const` so identifiers can back `static` placeholders.
			#[inline]
			pub const fn new_static(value: &'static str) -> Self {
				Self($crate::sf!(value))
			}

			/// Borrows the underlying allocation-conscious string.
			#[inline]
			pub const fn as_inner(&self) -> &$crate::Str {
				&self.0
			}

			/// Returns the underlying allocation-conscious string.
			#[inline]
			pub fn into_inner(self) -> $crate::Str {
				self.0
			}
		}

		impl<T: ?Sized + AsRef<str>> $name<T> {
			/// Borrows the identifier as text.
			#[inline]
			pub fn as_str(&self) -> &str {
				self.0.as_ref()
			}
		}

		impl $name<str> {
			/// Brands borrowed text as this identifier without allocating.
			#[inline]
			pub const fn from_ref(value: &str) -> &Self {
				// SAFETY: `#[repr(transparent)]` over the inner text makes
				// `&str` and `&Self` layout-identical.
				unsafe { &*(value as *const str as *const Self) }
			}
		}

		impl Clone for $name<$crate::Str> {
			#[inline]
			fn clone(&self) -> Self {
				Self(self.0.clone())
			}
		}

		impl Default for $name<$crate::Str> {
			#[inline]
			fn default() -> Self {
				Self($crate::Str::default())
			}
		}

		impl ::std::ops::Deref for $name<$crate::Str> {
			type Target = $name<str>;

			#[inline]
			fn deref(&self) -> &$name<str> {
				$name::from_ref(self.0.as_str())
			}
		}

		impl ::std::ops::Deref for $name<str> {
			type Target = str;

			#[inline]
			fn deref(&self) -> &str {
				&self.0
			}
		}

		impl AsRef<str> for $name<$crate::Str> {
			#[inline]
			fn as_ref(&self) -> &str {
				self.0.as_str()
			}
		}

		impl AsRef<str> for $name<str> {
			#[inline]
			fn as_ref(&self) -> &str {
				&self.0
			}
		}

		impl ::std::borrow::Borrow<str> for $name<$crate::Str> {
			#[inline]
			fn borrow(&self) -> &str {
				self.0.as_str()
			}
		}

		impl ::std::borrow::Borrow<$name<str>> for $name<$crate::Str> {
			#[inline]
			fn borrow(&self) -> &$name<str> {
				self
			}
		}

		impl ::std::borrow::ToOwned for $name<str> {
			type Owned = $name<$crate::Str>;

			#[inline]
			fn to_owned(&self) -> $name<$crate::Str> {
				$name::<$crate::Str>::new(&self.0)
			}
		}

		impl<'a> From<&'a str> for &'a $name<str> {
			#[inline]
			fn from(value: &'a str) -> Self {
				$name::from_ref(value)
			}
		}

		impl From<$crate::Str> for $name<$crate::Str> {
			#[inline]
			fn from(value: $crate::Str) -> Self {
				Self(value)
			}
		}

		impl From<&str> for $name<$crate::Str> {
			#[inline]
			fn from(value: &str) -> Self {
				Self::new(value)
			}
		}

		impl From<String> for $name<$crate::Str> {
			#[inline]
			fn from(value: String) -> Self {
				Self::new(value)
			}
		}

		impl From<$name<$crate::Str>> for $crate::Str {
			#[inline]
			fn from(value: $name<$crate::Str>) -> Self {
				value.0
			}
		}

		impl From<&$name<str>> for $name<$crate::Str> {
			#[inline]
			fn from(value: &$name<str>) -> Self {
				::std::borrow::ToOwned::to_owned(value)
			}
		}

		impl From<&$name<$crate::Str>> for $name<$crate::Str> {
			#[inline]
			fn from(value: &$name<$crate::Str>) -> Self {
				value.clone()
			}
		}

		impl PartialEq<str> for $name<$crate::Str> {
			#[inline]
			fn eq(&self, other: &str) -> bool {
				self.0.as_str() == other
			}
		}

		impl PartialEq<&str> for $name<$crate::Str> {
			#[inline]
			fn eq(&self, other: &&str) -> bool {
				self.0.as_str() == *other
			}
		}

		impl PartialEq<$name<str>> for $name<$crate::Str> {
			#[inline]
			fn eq(&self, other: &$name<str>) -> bool {
				self.0.as_str() == &other.0
			}
		}

		impl PartialEq<$name<$crate::Str>> for $name<str> {
			#[inline]
			fn eq(&self, other: &$name<$crate::Str>) -> bool {
				&self.0 == other.0.as_str()
			}
		}

		impl<T: ?Sized + ::std::fmt::Display> ::std::fmt::Display for $name<T> {
			fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
				self.0.fmt(formatter)
			}
		}

		impl<T: ?Sized + ::std::fmt::Debug> ::std::fmt::Debug for $name<T> {
			fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
				::std::fmt::Debug::fmt(&self.0, formatter)
			}
		}

		impl<T: ?Sized + ::serde::Serialize> ::serde::Serialize for $name<T> {
			fn serialize<S: ::serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
				self.0.serialize(serializer)
			}
		}

		impl<'de> ::serde::Deserialize<'de> for $name<$crate::Str> {
			fn deserialize<D: ::serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
				Ok(Self($crate::Str::deserialize(deserializer)?))
			}
		}
	};
}

#[cfg(test)]
mod tests {
	use std::{
		collections::{BTreeMap, hash_map::RandomState},
		hash::BuildHasher,
		mem::size_of,
	};

	use crate::Str;

	string_id!(TestId);
	string_id!(OtherId);

	#[test]
	fn map_supports_borrowed_brand_and_str_lookups() {
		let map: BTreeMap<TestId, u32> = BTreeMap::from([(TestId::new("x"), 42_u32)]);
		let borrowed: &TestId<str> = TestId::from_ref("x");

		assert_eq!(map.get(borrowed), Some(&42));
		assert_eq!(map.get("x"), Some(&42));
	}

	#[test]
	fn owned_derefs_to_explicit_borrowed_type() {
		fn lookup(_id: &TestId<str>) {}

		let owned: TestId = TestId::new("x");
		lookup(&owned);
		lookup(TestId::from_ref("x"));
	}

	#[test]
	fn owned_and_borrowed_hashes_match() {
		let state = RandomState::new();
		let owned: TestId = TestId::new("x");

		assert_eq!(state.hash_one(&owned), state.hash_one(TestId::from_ref("x")));
	}

	#[test]
	fn owned_layout_matches_str() {
		assert_eq!(size_of::<TestId>(), size_of::<Str>());
	}

	#[test]
	fn serde_wire_format_is_transparent() {
		let encoded = serde_json::to_string(&TestId::new("x")).unwrap();
		assert_eq!(encoded, "\"x\"");

		let decoded: TestId = serde_json::from_str(&encoded).unwrap();
		assert_eq!(decoded, "x");
	}

	#[test]
	fn debug_is_transparent() {
		assert_eq!(format!("{:?}", TestId::new("x")), format!("{:?}", Str::new("x")));
	}
}
