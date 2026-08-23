//! Verifies the protobuf JSON representation of byte fields.

use bytes::Bytes;
use omp_proto::{
	omp::{
		document::v1::{DocumentEvent, DocumentTarget, document_target},
		inference::v1::{ToolDef, tool_def, tool_def::grammar},
		telemetry::v1::ToolCall,
	},
	prost::Message as _,
};
use serde_json::{Value, json};

#[test]
fn plain_utf8_and_empty_schema_bytes_are_json_strings() {
	let schema = Bytes::from_static(br#"{"type":"object"}"#);
	let tool = ToolDef {
		input: Some(tool_def::Input::JsonSchema(tool_def::JsonSchema {
			schema_json: schema.clone(),
			strict:      None,
		})),
		..Default::default()
	};
	let value = serde_json::to_value(&tool).unwrap();
	assert_eq!(value["input"]["JsonSchema"]["schema_json"], json!(r#"{"type":"object"}"#));
	let decoded = serde_json::from_value::<ToolDef>(value).unwrap();
	let Some(tool_def::Input::JsonSchema(decoded)) = decoded.input else {
		panic!("JSON Schema input");
	};
	assert_eq!(decoded.schema_json, schema);
	assert_eq!(decoded.strict, None);

	let empty = ToolDef {
		input: Some(tool_def::Input::JsonSchema(tool_def::JsonSchema::default())),
		..Default::default()
	};
	let value = serde_json::to_value(&empty).unwrap();
	assert_eq!(value["input"]["JsonSchema"]["schema_json"], json!(""));
	let decoded = serde_json::from_value::<ToolDef>(value).unwrap();
	let Some(tool_def::Input::JsonSchema(decoded)) = decoded.input else {
		panic!("JSON Schema input");
	};
	assert!(decoded.schema_json.is_empty());
}

#[test]
fn json_schema_strict_presence_survives_json_roundtrip() {
	for strict in [None, Some(false), Some(true)] {
		let tool = ToolDef {
			input: Some(tool_def::Input::JsonSchema(tool_def::JsonSchema {
				schema_json: Bytes::from_static(b"{}"),
				strict,
			})),
			..Default::default()
		};
		let value = serde_json::to_value(&tool).unwrap();
		assert_eq!(value["input"]["JsonSchema"]["strict"], strict.map_or(Value::Null, Value::Bool));
		let decoded = serde_json::from_value::<ToolDef>(value).unwrap();
		let Some(tool_def::Input::JsonSchema(decoded)) = decoded.input else {
			panic!("JSON Schema input");
		};
		assert_eq!(decoded.strict, strict);
	}
}

#[test]
fn grammar_tool_survives_binary_protocol_roundtrip_losslessly() {
	const EDIT_LARK: &str = "start: begin_patch op+ end_patch\ncontent_line: /[^§«»\\n][^\\n]*/ LF";
	let tool = ToolDef {
		name:        "edit".to_owned(),
		description: "Sparse edit".to_owned(),
		input:       Some(tool_def::Input::Grammar(tool_def::Grammar {
			syntax:               grammar::Syntax::Lark as i32,
			definition:           EDIT_LARK.to_owned(),
			fallback_schema_json: Bytes::from_static(br#"{"type":"object"}"#),
		})),
	};
	let decoded = ToolDef::decode(tool.encode_to_vec().as_slice()).expect("ToolDef decodes");
	let Some(tool_def::Input::Grammar(grammar)) = decoded.input else {
		panic!("grammar input");
	};
	assert_eq!(grammar.syntax, grammar::Syntax::Lark as i32);
	assert_eq!(grammar.definition, EDIT_LARK);
	assert_eq!(grammar.fallback_schema_json.as_ref(), br#"{"type":"object"}"#);
}

#[test]
fn binary_and_reserved_schema_prefix_use_base64() {
	for bytes in [Bytes::from_static(&[0xff, 0x00]), Bytes::from_static(b"b64:literal")] {
		let tool = ToolDef {
			input: Some(tool_def::Input::JsonSchema(tool_def::JsonSchema {
				schema_json: bytes.clone(),
				strict:      None,
			})),
			..Default::default()
		};
		let value = serde_json::to_value(&tool).unwrap();
		assert!(
			value["input"]["JsonSchema"]["schema_json"]
				.as_str()
				.unwrap()
				.starts_with("b64:")
		);
		let decoded = serde_json::from_value::<ToolDef>(value).unwrap();
		let Some(tool_def::Input::JsonSchema(decoded)) = decoded.input else {
			panic!("JSON Schema input");
		};
		assert_eq!(decoded.schema_json, bytes);
	}
}

#[test]
fn optional_bytes_preserve_none_and_some() {
	let none = ToolCall::default();
	let value = serde_json::to_value(&none).unwrap();
	assert_eq!(value["payload_json"], Value::Null);
	assert_eq!(
		serde_json::from_value::<ToolCall>(value)
			.unwrap()
			.payload_json,
		None
	);

	let payload = Bytes::from_static(br#"{"ok":true}"#);
	let some = ToolCall { payload_json: Some(payload.clone()), ..Default::default() };
	let value = serde_json::to_value(&some).unwrap();
	assert_eq!(value["payload_json"], json!(r#"{"ok":true}"#));
	assert_eq!(
		serde_json::from_value::<ToolCall>(value)
			.unwrap()
			.payload_json,
		Some(payload)
	);
}

#[test]
fn oneof_bytes_variant_round_trips_as_text() {
	let document_id = Bytes::from_static(b"doc-123");
	let target = DocumentTarget { target: Some(document_target::Target::DocumentId(document_id)) };
	let value = serde_json::to_value(&target).unwrap();
	assert_eq!(value["target"], json!({"DocumentId": "doc-123"}));
	assert_eq!(serde_json::from_value::<DocumentTarget>(value).unwrap(), target);
}

#[test]
fn repeated_bytes_round_trip_each_element() {
	let ids = vec![Bytes::from_static(b"first"), Bytes::from_static(&[0xff])];
	let event = DocumentEvent { invalidated_transaction_ids: ids.clone(), ..Default::default() };
	let value = serde_json::to_value(&event).unwrap();
	assert_eq!(value["invalidated_transaction_ids"], json!(["first", "b64:/w=="]));
	assert_eq!(
		serde_json::from_value::<DocumentEvent>(value)
			.unwrap()
			.invalidated_transaction_ids,
		ids
	);
}
