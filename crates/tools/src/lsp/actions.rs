//! Standard method and raw-parameter construction for LSP actions.

use serde_json::{Map, Value, json};

use super::Action;

/// Returns the standard JSON-RPC method for an action.
pub const fn method(action: Action) -> Option<&'static str> {
	match action {
		Action::Definition => Some("textDocument/definition"),
		Action::TypeDefinition => Some("textDocument/typeDefinition"),
		Action::Implementation => Some("textDocument/implementation"),
		Action::References => Some("textDocument/references"),
		Action::Hover => Some("textDocument/hover"),
		Action::Symbols => Some("textDocument/documentSymbol"),
		Action::Rename => Some("textDocument/rename"),
		Action::CodeActions => Some("textDocument/codeAction"),
		Action::Diagnostics => Some("textDocument/diagnostic"),
		_ => None,
	}
}

/// Adds textDocument and position to raw object parameters only when absent.
pub fn auto_parameters(
	params: Option<Value>,
	uri: Option<&str>,
	line: Option<u32>,
	character: Option<u32>,
) -> Value {
	let mut object = match params {
		Some(Value::Object(object)) => object,
		_ => Map::new(),
	};
	if !object.contains_key("textDocument")
		&& let Some(uri) = uri
	{
		object.insert("textDocument".into(), json!({ "uri": uri }));
	}
	if !object.contains_key("position")
		&& let Some(line) = line
	{
		object.insert(
			"position".into(),
			json!({ "line": line.saturating_sub(1), "character": character.unwrap_or(0) }),
		);
	}
	Value::Object(object)
}
