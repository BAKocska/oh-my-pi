//! Canonical JSON Schema rendering for prompt templates.

use std::fmt::Write as _;

/// Renders a JSON Schema as compact TypeScript-like definitions and the
/// terminal yield envelope required by the agent protocol.
#[must_use]
pub fn render(schema: &serde_json::Value) -> String {
	let mut output = String::new();
	if let Some(definitions) = schema.get("$defs").and_then(serde_json::Value::as_object) {
		let mut definitions = definitions.iter().collect::<Vec<_>>();
		definitions.sort_unstable_by_key(|(name, _)| *name);
		for (name, definition) in definitions {
			let _ = writeln!(output, "type {name} = {};", render_type(definition));
		}
	}
	let root = render_type(schema);
	let _ = writeln!(output, "type ResultData = {root};");
	output.push_str(
		"type YieldEnvelope =\n  | { result: { data: ResultData } }\n  | { result: { error: string \
		 } };\n",
	);
	output
}

fn render_type(schema: &serde_json::Value) -> String {
	if let Some(reference) = schema.get("$ref").and_then(serde_json::Value::as_str) {
		return reference.rsplit('/').next().unwrap_or("unknown").to_owned();
	}
	if let Some(values) = schema.get("enum").and_then(serde_json::Value::as_array) {
		return values.iter().map(literal).collect::<Vec<_>>().join(" | ");
	}
	for combinator in ["oneOf", "anyOf"] {
		if let Some(values) = schema.get(combinator).and_then(serde_json::Value::as_array) {
			return values
				.iter()
				.map(render_type)
				.collect::<Vec<_>>()
				.join(" | ");
		}
	}
	match schema.get("type").and_then(serde_json::Value::as_str) {
		Some("object") | None if schema.get("properties").is_some() => {
			let required = schema
				.get("required")
				.and_then(serde_json::Value::as_array)
				.into_iter()
				.flatten()
				.filter_map(serde_json::Value::as_str)
				.collect::<std::collections::BTreeSet<_>>();
			let mut properties = schema
				.get("properties")
				.and_then(serde_json::Value::as_object)
				.into_iter()
				.flatten()
				.collect::<Vec<_>>();
			properties.sort_unstable_by_key(|(name, _)| *name);
			let fields = properties
				.into_iter()
				.map(|(name, value)| {
					format!(
						"{name}{}: {}",
						if required.contains(name.as_str()) {
							""
						} else {
							"?"
						},
						render_type(value)
					)
				})
				.collect::<Vec<_>>()
				.join("; ");
			format!("{{ {fields} }}")
		},
		Some("array") => format!(
			"Array<{}>",
			schema
				.get("items")
				.map_or_else(|| "unknown".to_owned(), render_type)
		),
		Some("string") => "string".to_owned(),
		Some("integer" | "number") => "number".to_owned(),
		Some("boolean") => "boolean".to_owned(),
		Some("null") => "null".to_owned(),
		_ => "unknown".to_owned(),
	}
}

fn literal(value: &serde_json::Value) -> String {
	match value {
		serde_json::Value::String(value) => serde_json::to_string(value).expect("string encoding"),
		_ => value.to_string(),
	}
}
