//! Verifies the protobuf JSON representation of byte fields.

use bytes::Bytes;
use omp_proto::omp::{
	document::v1::{DocumentEvent, DocumentTarget, document_target},
	inference::v1::ToolDef,
	telemetry::v1::ToolCall,
};
use serde_json::{Value, json};

#[test]
fn plain_utf8_and_empty_bytes_are_json_strings() {
	let schema = Bytes::from_static(br#"{"type":"object"}"#);
	let tool = ToolDef { schema_json: schema.clone(), ..Default::default() };
	let value = serde_json::to_value(&tool).unwrap();
	assert_eq!(value["schema_json"], json!(r#"{"type":"object"}"#));
	assert_eq!(
		serde_json::from_value::<ToolDef>(value)
			.unwrap()
			.schema_json,
		schema
	);

	let empty = ToolDef::default();
	let value = serde_json::to_value(&empty).unwrap();
	assert_eq!(value["schema_json"], json!(""));
	assert!(
		serde_json::from_value::<ToolDef>(value)
			.unwrap()
			.schema_json
			.is_empty()
	);
}

#[test]
fn binary_and_reserved_prefix_use_base64() {
	for bytes in [Bytes::from_static(&[0xff, 0x00]), Bytes::from_static(b"b64:literal")] {
		let tool = ToolDef { schema_json: bytes.clone(), ..Default::default() };
		let value = serde_json::to_value(&tool).unwrap();
		assert!(value["schema_json"].as_str().unwrap().starts_with("b64:"));
		assert_eq!(
			serde_json::from_value::<ToolDef>(value)
				.unwrap()
				.schema_json,
			bytes
		);
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
