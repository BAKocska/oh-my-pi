//! Compiles every `.proto` under `proto/` with protox + tonic-prost-build.
//!
//! protox is a pure-Rust protobuf compiler, so no system `protoc` install is
//! required. Generated Rust lands in `OUT_DIR` as one file per protobuf
//! package (`omp.thread.v1.rs`, ...) and is pulled in by `include!` in
//! `src/lib.rs`.
//!
//! Codegen choices:
//! - `bytes` fields decode into `Bytes` (O(1) clone, zero-copy slices) and
//!   serialize as lossless UTF-8-or-Base64 text.
//! - Maps are `BTreeMap` for deterministic serialization.
//! - Every type derives serde; well-known types (`google.protobuf.Struct`) are
//!   compiled locally instead of mapped to `prost-types` so the derives reach
//!   them too. This is Rust-native serde, not the proto3 JSON mapping (enums as
//!   ints, `snake_case` fields).
//!
//! Message bindings are always generated. The `tonic` feature additionally
//! emits gRPC client and server bindings into the same package files.

use std::{
	env, fs,
	path::{Path, PathBuf},
};

fn main() {
	let root = Path::new(std::env!("CARGO_MANIFEST_DIR")).join("proto");
	println!("cargo::rerun-if-changed={}", root.display());

	let mut protos = Vec::new();
	collect(&root, &mut protos);
	protos.sort();
	// Cargo only tracks the paths it is told about: emitting just `proto/`
	// misses edits to nested `.proto` files, so every collected source is
	// registered individually.
	for proto in &protos {
		println!("cargo::rerun-if-changed={}", proto.display());
	}
	// `src/lib.rs` includes `google.protobuf.rs` unconditionally so the serde
	// derives reach the well-known types; protox serves this file from its
	// embedded descriptor set even though it is not under `proto/`.
	protos.push(PathBuf::from("google/protobuf/struct.proto"));

	let fds = protox::compile(&protos, [&root]).expect("protox failed to compile .proto sources");
	let bytes_attributes = bytes_field_attributes(&fds);
	let generate_services = env::var_os("CARGO_FEATURE_TONIC").is_some();
	let mut builder = tonic_prost_build::configure()
		.build_client(generate_services)
		.build_server(generate_services)
		.bytes(".")
		.btree_map(".")
		.compile_well_known_types(true)
		.type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]")
		.message_attribute(".", "#[serde(default)]");
	for (path, attribute) in bytes_attributes {
		builder = builder.field_attribute(path, attribute);
	}
	builder
		.compile_fds(fds)
		.expect("tonic-prost-build failed to generate Rust code");
}

fn bytes_field_attributes(fds: &prost_types::FileDescriptorSet) -> Vec<(String, &'static str)> {
	let mut attributes = Vec::new();
	for file in &fds.file {
		let package = file.package.as_deref().unwrap_or_default();
		for message in &file.message_type {
			let name = message
				.name
				.as_deref()
				.expect("message descriptor missing name");
			let path = if package.is_empty() {
				format!(".{name}")
			} else {
				format!(".{package}.{name}")
			};
			collect_bytes_fields(message, &path, &mut attributes);
		}
	}
	attributes
}

fn collect_bytes_fields(
	message: &prost_types::DescriptorProto,
	message_path: &str,
	attributes: &mut Vec<(String, &'static str)>,
) {
	use prost_types::field_descriptor_proto::{Label, Type};

	let is_map = message
		.options
		.as_ref()
		.and_then(|options| options.map_entry)
		== Some(true);
	if is_map {
		if message.field.iter().any(|field| {
			field.name.as_deref() == Some("value") && field.r#type == Some(Type::Bytes as i32)
		}) {
			panic!(
				"protobuf map {message_path} has bytes values; add a dedicated bytes map serde adapter"
			);
		}
		return;
	}

	for field in &message.field {
		if field.r#type != Some(Type::Bytes as i32) {
			continue;
		}
		let field_name = field
			.name
			.as_deref()
			.expect("field descriptor missing name");
		let (field_path, attribute) = if field.proto3_optional == Some(true) {
			(format!("{message_path}.{field_name}"), "#[serde(with = \"crate::bytes_text::option\")]")
		} else if let Some(oneof_index) = field.oneof_index {
			let oneof_name = message.oneof_decl[oneof_index as usize]
				.name
				.as_deref()
				.expect("oneof descriptor missing name");
			(
				format!("{message_path}.{oneof_name}.{field_name}"),
				"#[serde(with = \"crate::bytes_text\")]",
			)
		} else if field.label == Some(Label::Repeated as i32) {
			(
				format!("{message_path}.{field_name}"),
				"#[serde(with = \"crate::bytes_text::repeated\")]",
			)
		} else {
			(format!("{message_path}.{field_name}"), "#[serde(with = \"crate::bytes_text\")]")
		};
		attributes.push((field_path, attribute));
	}

	for nested in &message.nested_type {
		let nested_name = nested
			.name
			.as_deref()
			.expect("nested message descriptor missing name");
		collect_bytes_fields(nested, &format!("{message_path}.{nested_name}"), attributes);
	}
}

/// Recursively gathers `.proto` files under `dir`.
fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
	for entry in fs::read_dir(dir).expect("proto/ directory missing") {
		let path = entry.expect("unreadable dir entry").path();
		if path.is_dir() {
			collect(&path, out);
		} else if path.extension().is_some_and(|ext| ext == "proto") {
			out.push(path);
		}
	}
}
