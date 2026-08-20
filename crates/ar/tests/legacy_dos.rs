//! LZH/LHA and ARJ compatibility fixtures and rejection behavior.

mod support;

use std::io::Cursor;

use omp_ar::{Archive, Error, Format};
use support::fixtures::fixture_bytes;

fn open(name: &str, format: Format) -> Archive<Cursor<Vec<u8>>> {
	Archive::with_format(Cursor::new(fixture_bytes(name)), format)
		.unwrap_or_else(|error| panic!("fixture {name} opens: {error:?}"))
}

fn paths(archive: &Archive<Cursor<Vec<u8>>>) -> Vec<&str> {
	archive.entries().map(|entry| entry.path()).collect()
}

#[test]
fn lzh_sniffs_valid_header() {
	assert_eq!(Format::sniff(&fixture_bytes("lzh-level0-lh5.lzh")), Some(Format::Lzh));
	assert_ne!(Format::sniff(&[0_u8; 64]), Some(Format::Lzh));
}

#[test]
fn lzh_indexes_all_header_levels_and_metadata() {
	for name in ["lzh-level0-lh5.lzh", "lzh-level1-lh6.lzh", "lzh-level2-lh7.lzh"] {
		let mut archive = open(name, Format::Lzh);
		assert_eq!(paths(&archive), ["hello.txt", "nested", "nested/mode.sh", "nested/repeat.txt"]);
		assert!(
			archive
				.entry("nested")
				.is_some_and(|entry| entry.is_directory())
		);
		assert_eq!(
			archive
				.entry("hello.txt")
				.and_then(|entry| entry.modified_unix_seconds()),
			Some(1_700_000_000)
		);
		assert!(
			!archive
				.read("nested/repeat.txt")
				.expect("static-Huffman member decodes")
				.is_empty()
		);
	}
}

#[test]
fn lzh_static_huffman_methods_extract_identically() {
	let expected = open("lzh-level2-stored.lzh", Format::Lzh)
		.read("nested/repeat.txt")
		.expect("stored baseline reads");
	for name in
		["lzh-level0-lh4.lzh", "lzh-level0-lh5.lzh", "lzh-level1-lh6.lzh", "lzh-level2-lh7.lzh"]
	{
		assert_eq!(
			open(name, Format::Lzh)
				.read("nested/repeat.txt")
				.unwrap_or_else(|error| panic!("{name}: {error:?}")),
			expected,
			"{name}",
		);
	}
}

#[test]
fn lzh_decodes_larc_stored_and_lzs() {
	assert_eq!(
		open("lzh-lz4.lzh", Format::Lzh)
			.read("legacy/lz4.txt")
			.expect("-lz4- reads"),
		b"LArc legacy stream\n",
	);
	assert_eq!(
		open("lzh-lzs.lzh", Format::Lzh)
			.read("legacy/lzs.txt")
			.expect("-lzs- reads"),
		b"LArc legacy stream\n",
	);
}

#[test]
fn lzh_prefers_unicode_extensions_and_unix_metadata() {
	let mut archive = open("lzh-unicode-level2.lzh", Format::Lzh);
	let entries: Vec<_> = archive.entries().collect();
	assert_eq!(entries.len(), 2, "the parent directory is synthesized");
	let entry = archive
		.entry("unicode/雪.txt")
		.expect("Unicode member is indexed");
	assert_eq!(entry.mode(), Some(0x81a4));
	assert_eq!(entry.modified_unix_seconds(), Some(1_700_000_000));
	assert_eq!(
		archive
			.read("unicode/雪.txt")
			.expect("Unicode member reads"),
		b"Unicode LZH path\n"
	);
}

#[test]
fn lzh_drops_traversal_and_defers_precise_unsupported_error() {
	let traversal = open("lzh-traversal.lzh", Format::Lzh);
	assert_eq!(traversal.entries().count(), 0);

	let error = open("lzh-unsupported-lh1.lzh", Format::Lzh)
		.read("unsupported.txt")
		.expect_err("-lh1- is unsupported");
	assert!(matches!(error, Error::UnsupportedFeature("LZH dynamic-Huffman method -lh1-")));
}

#[test]
fn lzh_rejects_truncation_and_member_crc_corruption() {
	let original = fixture_bytes("lzh-level2-stored.lzh");
	let error = Archive::with_format(Cursor::new(original[..12].to_vec()), Format::Lzh)
		.err()
		.expect("truncated LZH fails");
	assert!(matches!(error, Error::InvalidArchive(_)));

	let mut corrupted = original;
	let index = corrupted.len() - 2;
	corrupted[index] ^= 0x80;
	let mut archive =
		Archive::with_format(Cursor::new(corrupted), Format::Lzh).expect("headers remain valid");
	assert!(matches!(archive.read("nested/repeat.txt"), Err(Error::ChecksumMismatch { .. })));
}

#[test]
fn arj_sniffs_crc_valid_main_header() {
	assert_eq!(Format::sniff(&fixture_bytes("arj-method1.arj")), Some(Format::Arj));
	assert_ne!(Format::sniff(&[0_u8; 64]), Some(Format::Arj));
}

#[test]
fn arj_methods_zero_through_four_extract_identically() {
	let mut baseline = open("arj-method0.arj", Format::Arj);
	let expected = baseline
		.read("nested/repeat.txt")
		.expect("method 0 baseline reads");
	assert_eq!(baseline.read("hello.txt").expect("stored text reads"), b"Legacy archive hello.\n");
	for method in 0..=4 {
		let name = format!("arj-method{method}.arj");
		let mut archive = open(&name, Format::Arj);
		assert!(archive.entry("hello.txt").is_some());
		assert!(archive.entry("nested/mode.sh").is_some());
		assert_eq!(
			archive
				.read("nested/repeat.txt")
				.unwrap_or_else(|error| panic!("{name}: {error:?}")),
			expected
		);
	}
}

#[test]
fn arj_exposes_unix_mode_and_timestamp() {
	let archive = open("arj-method1.arj", Format::Arj);
	let hello = archive.entry("hello.txt").expect("hello is indexed");
	assert_eq!(hello.mode(), Some(0x81a4));
	assert_eq!(hello.modified_unix_seconds(), Some(1_700_000_000));
	assert_eq!(
		archive
			.entry("nested/mode.sh")
			.and_then(|entry| entry.mode()),
		Some(0x81ed)
	);
}

#[test]
fn arj_rejects_encryption_and_multivolume_members() {
	let encrypted =
		Archive::with_format(Cursor::new(fixture_bytes("arj-encrypted.arj")), Format::Arj)
			.err()
			.expect("encrypted member is rejected");
	assert!(matches!(encrypted, Error::UnsupportedFeature("encrypted ARJ members")));
	let multivolume =
		Archive::with_format(Cursor::new(fixture_bytes("arj-multivolume.arj")), Format::Arj)
			.err()
			.expect("multi-volume member is rejected");
	assert!(matches!(multivolume, Error::UnsupportedFeature("multi-volume ARJ members")));
}

#[test]
fn arj_defers_unknown_method_error_and_drops_traversal() {
	let error = open("arj-unsupported.arj", Format::Arj)
		.read("hello.txt")
		.expect_err("method 7 is unsupported");
	assert!(matches!(error, Error::UnsupportedFeature("ARJ compression method 7")));

	let traversal = open("arj-traversal.arj", Format::Arj);
	assert!(traversal.entry("hello.txt").is_none());
	assert!(traversal.entry("nested/mode.sh").is_some());
	assert!(traversal.entry("nested/repeat.txt").is_some());
}

#[test]
fn arj_rejects_truncation_and_header_crc_corruption() {
	let original = fixture_bytes("arj-method1.arj");
	let truncated = Archive::with_format(Cursor::new(original[..20].to_vec()), Format::Arj)
		.err()
		.expect("truncated ARJ fails");
	assert!(matches!(truncated, Error::InvalidArchive(_)));

	let mut corrupted = original;
	corrupted[12] ^= 1;
	let error = Archive::with_format(Cursor::new(corrupted), Format::Arj)
		.err()
		.expect("header CRC corruption fails");
	assert!(matches!(error, Error::InvalidArchive(_)));
}

#[test]
fn arj_verifies_member_crc() {
	let mut corrupted = fixture_bytes("arj-method0.arj");
	let data_byte = corrupted.len() - 5;
	corrupted[data_byte] ^= 0x80;
	let mut archive =
		Archive::with_format(Cursor::new(corrupted), Format::Arj).expect("headers remain valid");
	assert!(matches!(archive.read("nested/repeat.txt"), Err(Error::ChecksumMismatch { .. })));
}
