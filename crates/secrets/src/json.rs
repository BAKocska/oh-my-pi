//! Recursive secret transforms for model-authored JSON values.

use serde_json::Value;

use crate::obfuscator::SecretObfuscator;

/// Recursively obfuscates string leaves in a model-authored JSON value.
///
/// Non-string leaves and object keys are retained byte-for-byte.
pub fn obfuscate_json(value: &mut Value, obfuscator: &mut SecretObfuscator) {
	map_strings(value, &mut |text| obfuscator.obfuscate(text));
}

/// Recursively restores string leaves in a model-authored JSON value.
pub fn deobfuscate_json(value: &mut Value, obfuscator: &SecretObfuscator) {
	map_strings(value, &mut |text| obfuscator.deobfuscate(text));
}

fn map_strings(value: &mut Value, transform: &mut impl FnMut(&str) -> String) {
	match value {
		Value::String(text) => {
			let mapped = transform(text);
			if mapped != *text {
				*text = mapped;
			}
		},
		Value::Array(values) => {
			for value in values {
				map_strings(value, transform);
			}
		},
		Value::Object(values) => {
			for value in values.values_mut() {
				map_strings(value, transform);
			}
		},
		Value::Null | Value::Bool(_) | Value::Number(_) => {},
	}
}

#[cfg(test)]
mod tests {
	use serde_json::json;

	use super::*;
	use crate::rule::{SecretKind, SecretMode, SecretRule};

	#[test]
	fn maps_nested_strings_without_touching_keys_or_scalars() {
		let rule = SecretRule::new(
			SecretKind::Plain,
			SecretMode::Obfuscate,
			"nested-secret",
			None,
			None,
			None,
		)
		.expect("rule");
		let mut obfuscator = SecretObfuscator::new(vec![rule], "K".repeat(43));
		let mut value = json!({"nested-secret": [1, {"value": "nested-secret"}]});
		obfuscate_json(&mut value, &mut obfuscator);
		assert_eq!(value["nested-secret"][0], 1);
		assert_ne!(value["nested-secret"][1]["value"], "nested-secret");
		deobfuscate_json(&mut value, &obfuscator);
		assert_eq!(value, json!({"nested-secret": [1, {"value": "nested-secret"}]}));
	}
}
