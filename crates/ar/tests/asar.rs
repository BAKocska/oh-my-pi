//! Electron ASAR indexing, packed access, and unpacked sibling contracts.

use std::fs;

use omp_ar::{Archive, Error, Format, Limits};
use serde_json::{Value, json};
use tempfile::tempdir;

#[test]
fn lists_directories_and_reads_packed_members_and_links_lazily() {
	let bytes = fixture(
		json!({
			"docs": {"files": {
				"hello.txt": {"size": 5, "offset": "0"}
			}},
			"top.txt": {"size": 3, "offset": "5"},
			"alias.txt": {"link": "docs/hello.txt"}
		}),
		b"hellotop",
	);
	assert_eq!(Format::sniff(&bytes), Some(Format::Asar));
	assert_eq!(Format::from_path("bundle.ASAR"), Some(Format::Asar));
	let mut archive = Archive::from_bytes_with_format(&bytes, Format::Asar).unwrap();

	let root: Vec<_> = archive.list("").unwrap().into_iter().map(|entry| entry.path()).collect();
	assert_eq!(root, ["alias.txt", "docs", "top.txt"]);
	let docs: Vec<_> =
		archive.list("docs").unwrap().into_iter().map(|entry| entry.path()).collect();
	assert_eq!(docs, ["docs/hello.txt"]);
	assert_eq!(archive.read("docs/hello.txt").unwrap(), b"hello");
	assert_eq!(archive.read("top.txt").unwrap(), b"top");
	assert_eq!(archive.read("alias.txt").unwrap(), b"hello");
}

#[test]
fn reads_unpacked_members_from_the_adjacent_sibling_tree() {
	let directory = tempdir().unwrap();
	let archive_path = directory.path().join("bundle.asar");
	let unpacked_path = directory.path().join("bundle.asar.unpacked/assets");
	fs::create_dir_all(&unpacked_path).unwrap();
	fs::write(
		&archive_path,
		fixture(
			json!({
				"assets": {"files": {
					"external.txt": {"size": 8, "unpacked": true}
				}}
			}),
			b"",
		),
	)
	.unwrap();
	fs::write(unpacked_path.join("external.txt"), b"external").unwrap();

	let mut archive = Archive::open(&archive_path).unwrap();
	assert_eq!(archive.read("assets/external.txt").unwrap(), b"external");

	fs::remove_file(unpacked_path.join("external.txt")).unwrap();
	assert!(matches!(archive.read("assets/external.txt"), Err(Error::Io(_))));
}

#[test]
fn byte_backed_unpacked_members_fail_recoverably() {
	let bytes = fixture(json!({"external.txt": {"size": 1, "unpacked": true}}), b"");
	let mut archive = Archive::from_bytes_with_format(&bytes, Format::Asar).unwrap();
	assert!(matches!(archive.read("external.txt"), Err(Error::InvalidArchive(_))));
}

#[test]
fn rejects_unsafe_tree_names_and_link_targets() {
	for files in [
		json!({"..": {"size": 0, "offset": "0"}}),
		json!({"safe": {"link": "../outside"}}),
		json!({"nested/name": {"size": 0, "offset": "0"}}),
	] {
		let bytes = fixture(files, b"");
		assert!(matches!(
			Archive::from_bytes_with_format(&bytes, Format::Asar),
			Err(Error::UnsafePath(_))
		));
	}
}

#[test]
fn malformed_pickle_and_json_headers_are_recoverable_errors() {
	let valid = fixture(json!({"file": {"size": 0, "offset": "0"}}), b"");
	for bytes in [
		{
			let mut bytes = valid.clone();
			bytes[0] = 5;
			bytes
		},
		{
			let mut bytes = valid.clone();
			bytes[16] = b'!';
			bytes
		},
		{
			let mut bytes = valid.clone();
			bytes.truncate(16);
			bytes
		},
	] {
		assert!(matches!(
			Archive::from_bytes_with_format(&bytes, Format::Asar),
			Err(Error::InvalidArchive(_)) | Err(Error::Io(_))
		));
	}
}

#[test]
fn rejects_out_of_bounds_and_oversized_members() {
	let bytes = fixture(json!({"bad": {"size": 2, "offset": "0"}}), b"x");
	assert!(matches!(
		Archive::from_bytes_with_format(&bytes, Format::Asar),
		Err(Error::InvalidArchive(_))
	));

	let bytes = fixture(json!({"large": {"size": 5, "offset": "0"}}), b"large");
	let limits = Limits::DEFAULT.with_max_member_size(4);
	let mut archive =
		Archive::from_bytes_with_format_and_limits(&bytes, Format::Asar, limits).unwrap();
	assert!(matches!(archive.read("large"), Err(Error::MemberTooLarge { .. })));
}

fn fixture(files: Value, packed: &[u8]) -> Vec<u8> {
	let json = serde_json::to_vec(&json!({"files": files})).unwrap();
	let payload_size = 4_usize
		.checked_add(json.len())
		.and_then(|size| size.checked_add(1))
		.unwrap();
	let padded_payload_size = (payload_size + 3) & !3;
	let inner_size = 4 + padded_payload_size;

	let mut bytes = Vec::with_capacity(8 + inner_size + packed.len());
	push_u32(&mut bytes, 4);
	push_u32(&mut bytes, u32::try_from(inner_size).unwrap());
	push_u32(&mut bytes, u32::try_from(padded_payload_size).unwrap());
	push_u32(&mut bytes, u32::try_from(json.len()).unwrap());
	bytes.extend_from_slice(&json);
	bytes.resize(8 + inner_size, 0);
	bytes.extend_from_slice(packed);
	bytes
}

fn push_u32(output: &mut Vec<u8>, value: u32) {
	output.extend_from_slice(&value.to_le_bytes());
}
