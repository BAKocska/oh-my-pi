//! JSON-shaped immutable template values with O(1) clone.

use omp_core::{IntoStr, Str};
use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};

/// A template value: the unit of data flowing from [`crate::Props`] through
/// expressions, filters, and emission.
///
/// Collections use persistent structures (`im`), so cloning a value — and
/// every props bag built from values — is O(1) structural sharing. Map
/// iteration is key-ordered, keeping every render deterministic.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Value {
	/// Absent/null. Renders as the empty string and is falsy.
	#[default]
	None,
	/// Boolean.
	Bool(bool),
	/// 64-bit signed integer.
	Int(i64),
	/// 64-bit float.
	Float(f64),
	/// String; inline up to 23 bytes, heap-shared above.
	Str(Str),
	/// Ordered list with O(1) clone.
	List(im::Vector<Self>),
	/// Key-ordered map with O(1) clone and deterministic iteration.
	Map(im::OrdMap<Str, Self>),
}

impl Value {
	/// Jinja-style truthiness: `none`, `false`, `0`, `0.0`, `""`, the empty
	/// list, and the empty map are falsy; everything else is truthy.
	pub fn is_truthy(&self) -> bool {
		match self {
			Self::None => false,
			Self::Bool(value) => *value,
			Self::Int(value) => *value != 0,
			Self::Float(value) => *value != 0.0,
			Self::Str(value) => !value.is_empty(),
			Self::List(value) => !value.is_empty(),
			Self::Map(value) => !value.is_empty(),
		}
	}

	/// Borrows the string payload when this value is a string.
	pub fn as_str(&self) -> Option<&str> {
		match self {
			Self::Str(value) => Some(value.as_str()),
			_ => None,
		}
	}

	/// Writes the display form used by `{{ }}` emission and string filters:
	/// `none` is empty, scalars use their natural form, and collections
	/// serialize as compact JSON.
	pub(crate) fn write_display(&self, out: &mut String) {
		use std::fmt::Write as _;
		match self {
			Self::None => {},
			Self::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
			Self::Int(value) => {
				let _ = write!(out, "{value}");
			},
			Self::Float(value) => {
				let _ = write!(out, "{value}");
			},
			Self::Str(value) => out.push_str(value),
			Self::List(_) | Self::Map(_) => out.push_str(
				&serde_json::to_string(self).expect("scribe values are always JSON-serializable"),
			),
		}
	}

	/// Returns the display form as an owned string.
	pub(crate) fn display(&self) -> Str {
		if let Self::Str(value) = self {
			return value.clone();
		}
		let mut out = String::new();
		self.write_display(&mut out);
		Str::from(out)
	}
}

impl Serialize for Value {
	fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		match self {
			Self::None => serializer.serialize_unit(),
			Self::Bool(value) => serializer.serialize_bool(*value),
			Self::Int(value) => serializer.serialize_i64(*value),
			Self::Float(value) => serializer.serialize_f64(*value),
			Self::Str(value) => serializer.serialize_str(value.as_str()),
			Self::List(items) => {
				let mut seq = serializer.serialize_seq(Some(items.len()))?;
				for item in items {
					seq.serialize_element(item)?;
				}
				seq.end()
			},
			Self::Map(entries) => {
				let mut map = serializer.serialize_map(Some(entries.len()))?;
				for (key, value) in entries {
					map.serialize_entry(key.as_str(), value)?;
				}
				map.end()
			},
		}
	}
}

impl From<bool> for Value {
	fn from(value: bool) -> Self {
		Self::Bool(value)
	}
}

impl From<i64> for Value {
	fn from(value: i64) -> Self {
		Self::Int(value)
	}
}

impl From<i32> for Value {
	fn from(value: i32) -> Self {
		Self::Int(i64::from(value))
	}
}

impl From<u32> for Value {
	fn from(value: u32) -> Self {
		Self::Int(i64::from(value))
	}
}

impl From<usize> for Value {
	fn from(value: usize) -> Self {
		Self::Int(value as i64)
	}
}

impl From<f64> for Value {
	fn from(value: f64) -> Self {
		Self::Float(value)
	}
}

impl From<&'static str> for Value {
	fn from(value: &'static str) -> Self {
		Self::Str(Str::new_static(value))
	}
}

impl From<Str> for Value {
	fn from(value: Str) -> Self {
		Self::Str(value)
	}
}

impl From<String> for Value {
	fn from(value: String) -> Self {
		Self::Str(Str::from(value))
	}
}

impl From<im::Vector<Self>> for Value {
	fn from(value: im::Vector<Self>) -> Self {
		Self::List(value)
	}
}

impl From<im::OrdMap<Str, Self>> for Value {
	fn from(value: im::OrdMap<Str, Self>) -> Self {
		Self::Map(value)
	}
}

impl<T: Into<Self>> From<Vec<T>> for Value {
	fn from(value: Vec<T>) -> Self {
		Self::List(value.into_iter().map(Into::into).collect())
	}
}
impl<T: Into<Self>> FromIterator<T> for Value {
	fn from_iter<I: IntoIterator<Item = T>>(items: I) -> Self {
		Self::List(items.into_iter().map(Into::into).collect())
	}
}

impl<K: IntoStr, T: Into<Self>> FromIterator<(K, T)> for Value {
	fn from_iter<I: IntoIterator<Item = (K, T)>>(entries: I) -> Self {
		Self::Map(
			entries
				.into_iter()
				.map(|(key, value)| (key.into_str(), value.into()))
				.collect(),
		)
	}
}

impl<T: Into<Self>> From<Option<T>> for Value {
	fn from(value: Option<T>) -> Self {
		value.map_or(Self::None, Into::into)
	}
}

impl From<&serde_json::Value> for Value {
	fn from(value: &serde_json::Value) -> Self {
		match value {
			serde_json::Value::Null => Self::None,
			serde_json::Value::Bool(value) => Self::Bool(*value),
			serde_json::Value::Number(number) => number
				.as_i64()
				.map_or_else(|| Self::Float(number.as_f64().unwrap_or(0.0)), Self::Int),
			serde_json::Value::String(value) => Self::Str(Str::new(value)),
			serde_json::Value::Array(items) => Self::List(items.iter().map(Self::from).collect()),
			serde_json::Value::Object(entries) => Self::Map(
				entries
					.iter()
					.map(|(key, value)| (Str::new(key), Self::from(value)))
					.collect(),
			),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{list, map};

	#[test]
	fn truthiness_matches_the_documented_table() {
		for falsy in [
			Value::None,
			Value::Bool(false),
			Value::Int(0),
			Value::Float(0.0),
			Value::Str(Str::empty()),
			list![],
			map! {},
		] {
			assert!(!falsy.is_truthy(), "{falsy:?} must be falsy");
		}
		for truthy in [
			Value::Bool(true),
			Value::Int(-1),
			Value::Float(0.5),
			Value::from("x"),
			list![0],
			map! { "k" => Value::None },
		] {
			assert!(truthy.is_truthy(), "{truthy:?} must be truthy");
		}
	}

	#[test]
	fn json_round_trip_preserves_shape() {
		let json: serde_json::Value =
			serde_json::from_str(r#"{"b":true,"n":3,"f":1.5,"s":"x","l":[1,null],"m":{"k":"v"}}"#)
				.unwrap();
		let value = Value::from(&json);
		assert_eq!(value, map! {
			"b" => true,
			"n" => 3,
			"f" => 1.5,
			"s" => "x",
			"l" => list![Value::Int(1), Value::None],
			"m" => map! { "k" => "v" },
		});
		let back: serde_json::Value =
			serde_json::from_str(&serde_json::to_string(&value).unwrap()).unwrap();
		assert_eq!(back, json);
	}

	#[test]
	fn display_renders_scalars_and_compact_json_collections() {
		let mut out = String::new();
		Value::None.write_display(&mut out);
		Value::Int(3).write_display(&mut out);
		Value::from("|x").write_display(&mut out);
		list![1, "a"].write_display(&mut out);
		assert_eq!(out, "3|x[1,\"a\"]");
	}

	#[test]
	fn option_and_vec_conversions_lift_into_values() {
		assert_eq!(Value::from(None::<i64>), Value::None);
		assert_eq!(Value::from(Some("x")), Value::from("x"));
		assert_eq!(Value::from(vec![1i64, 2]), list![1, 2]);
	}
}
