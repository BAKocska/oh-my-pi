//! Bounded, deterministic JSON-tree previews for retained tool views.

use omp_core::{Str, StrMut};
use serde_json::Value;

use super::truncate::truncate_line;

const MAX_DEPTH_LIMIT: usize = 64;
const MAX_LINES_LIMIT: usize = 1_000;
const MAX_SCALAR_LIMIT: usize = 4_096;

/// Bounds for one JSON-tree preview.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JsonTreeBounds {
	/// Maximum nested object/array depth.
	pub max_depth:        usize,
	/// Maximum complete preview lines.
	pub max_lines:        usize,
	/// Maximum UTF-16 columns retained for a key or scalar.
	pub max_scalar_chars: usize,
}

impl Default for JsonTreeBounds {
	fn default() -> Self {
		Self { max_depth: 2, max_lines: 6, max_scalar_chars: 60 }
	}
}

/// One bounded preview and whether any node or scalar was omitted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JsonTreePreview {
	/// Complete tree lines in source object order.
	pub lines:     Vec<Str>,
	/// Whether a configured bound omitted data.
	pub truncated: bool,
}

/// Renders JSON as a bounded plain-text tree without terminal escapes.
pub fn preview(value: &Value, bounds: JsonTreeBounds) -> JsonTreePreview {
	let bounds = JsonTreeBounds {
		max_depth:        bounds.max_depth.min(MAX_DEPTH_LIMIT),
		max_lines:        bounds.max_lines.min(MAX_LINES_LIMIT),
		max_scalar_chars: bounds.max_scalar_chars.min(MAX_SCALAR_LIMIT),
	};
	let mut renderer = Renderer { bounds, lines: Vec::new(), truncated: false };
	match value {
		Value::Object(values) => {
			let visible = values
				.iter()
				.filter(|(key, _)| !matches!(key.as_str(), "intent" | "__partialJson"))
				.collect::<Vec<_>>();
			for (index, (key, value)) in visible.iter().enumerate() {
				renderer.node(value, Some(key), &mut Vec::new(), index + 1 == visible.len(), 1);
				if renderer.full() {
					renderer.truncated |= index + 1 < visible.len();
					break;
				}
			}
		},
		Value::Array(values) => {
			for (index, value) in values.iter().enumerate() {
				let label = format!("[{index}]");
				renderer.node(value, Some(&label), &mut Vec::new(), index + 1 == values.len(), 1);
				if renderer.full() {
					renderer.truncated |= index + 1 < values.len();
					break;
				}
			}
		},
		_ => renderer.node(value, None, &mut Vec::new(), true, 0),
	}
	JsonTreePreview { lines: renderer.lines, truncated: renderer.truncated }
}

struct Renderer {
	bounds:    JsonTreeBounds,
	lines:     Vec<Str>,
	truncated: bool,
}

impl Renderer {
	fn full(&self) -> bool {
		self.lines.len() >= self.bounds.max_lines
	}

	fn push(&mut self, line: Str) -> bool {
		if self.full() {
			self.truncated = true;
			return false;
		}
		self.lines.push(line);
		true
	}

	fn node(
		&mut self,
		value: &Value,
		key: Option<&str>,
		ancestors: &mut Vec<bool>,
		last: bool,
		depth: usize,
	) {
		if self.full() {
			self.truncated = true;
			return;
		}
		let mut prefix = StrMut::new("");
		for has_next in ancestors.iter().copied() {
			prefix.push_str(if has_next { "│  " } else { "   " });
		}
		prefix.push_str(if last { "└─ " } else { "├─ " });
		let key = key.unwrap_or("value");
		let key = truncate_line(key, self.bounds.max_scalar_chars);

		match value {
			Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
				let scalar = scalar(value, self.bounds.max_scalar_chars);
				prefix.push_str(key.text.as_ref());
				prefix.push_str(": ");
				prefix.push_str(&scalar);
				self.truncated |= key.was_truncated || scalar.ends_with('…');
				self.push(prefix.freeze());
			},
			Value::Array(values) => {
				prefix.push_str(key.text.as_ref());
				prefix.push_str(" []");
				self.truncated |= key.was_truncated;
				if !self.push(prefix.freeze()) {
					return;
				}
				if values.is_empty() {
					self.child_marker(ancestors, last, "[]");
				} else if depth >= self.bounds.max_depth {
					self.truncated = true;
					self.child_marker(ancestors, last, "…");
				} else {
					ancestors.push(!last);
					for (index, child) in values.iter().enumerate() {
						let label = format!("[{index}]");
						self.node(child, Some(&label), ancestors, index + 1 == values.len(), depth + 1);
						if self.full() {
							self.truncated |= index + 1 < values.len();
							break;
						}
					}
					ancestors.pop();
				}
			},
			Value::Object(values) => {
				prefix.push_str(key.text.as_ref());
				prefix.push_str(" {}");
				self.truncated |= key.was_truncated;
				if !self.push(prefix.freeze()) {
					return;
				}
				if values.is_empty() {
					self.child_marker(ancestors, last, "{}");
				} else if depth >= self.bounds.max_depth {
					self.truncated = true;
					self.child_marker(ancestors, last, "…");
				} else {
					ancestors.push(!last);
					for (index, (child_key, child)) in values.iter().enumerate() {
						self.node(
							child,
							Some(child_key),
							ancestors,
							index + 1 == values.len(),
							depth + 1,
						);
						if self.full() {
							self.truncated |= index + 1 < values.len();
							break;
						}
					}
					ancestors.pop();
				}
			},
		}
	}

	fn child_marker(&mut self, ancestors: &[bool], parent_last: bool, marker: &str) {
		let mut line = StrMut::new("");
		for has_next in ancestors.iter().copied() {
			line.push_str(if has_next { "│  " } else { "   " });
		}
		line.push_str(if parent_last {
			"   └─ "
		} else {
			"│  └─ "
		});
		line.push_str(marker);
		self.push(line.freeze());
	}
}

fn scalar(value: &Value, max_chars: usize) -> String {
	let rendered = serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned());
	truncate_line(&rendered, max_chars).text.into_owned()
}

#[cfg(test)]
mod tests {
	use serde_json::json;

	use super::*;

	#[test]
	fn preview_is_depth_line_and_scalar_bounded() {
		let value = json!({
			"intent": "hidden",
			"name": "a very long scalar value",
			"nested": { "items": [1, 2, { "deep": true }] },
			"tail": false
		});
		let preview = preview(&value, JsonTreeBounds {
			max_depth:        2,
			max_lines:        4,
			max_scalar_chars: 12,
		});
		assert_eq!(preview.lines.len(), 4);
		assert!(preview.truncated);
		assert!(preview.lines.iter().all(|line| !line.contains("hidden")));
	}

	#[test]
	fn zero_line_bound_emits_no_preview() {
		let preview = preview(&json!({ "value": 1 }), JsonTreeBounds {
			max_lines: 0,
			..JsonTreeBounds::default()
		});
		assert!(preview.lines.is_empty());
		assert!(preview.truncated);
	}
}
