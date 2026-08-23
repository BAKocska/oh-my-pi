//! 7z container integration coverage against reference-packer fixtures.

mod support;

use std::io;

use omp_ar::{Archive, Error, Format, Limits};
use support::fixtures::fixture_bytes;

fn open(name: &str) -> omp_ar::Result<Archive<io::Cursor<Vec<u8>>>> {
	Archive::with_format(io::Cursor::new(fixture_bytes(name)), Format::SevenZip)
}

#[test]
fn indexes_metadata_and_extracts_supported_coder_chains() {
	let expected_message = fixture_bytes("sevenzip-src/nested/message.txt");
	let expected_delta = fixture_bytes("sevenzip-src/delta.bin");
	let expected_x86 = fixture_bytes("sevenzip-src/x86.bin");
	let sniffed_bytes = fixture_bytes("sevenzip-default.7z");
	assert_eq!(Format::sniff(&sniffed_bytes), Some(Format::SevenZip));
	let mut sniffed = Archive::from_bytes(&sniffed_bytes).unwrap();
	assert_eq!(sniffed.read("nested/message.txt").unwrap(), expected_message);
	for name in ["sevenzip-copy.7z", "sevenzip-lzma.7z", "sevenzip-default.7z"] {
		let mut archive = open(name).unwrap();
		assert_eq!(archive.read("nested/message.txt").unwrap(), expected_message, "{name}");
	}
	let mut delta = open("sevenzip-delta.7z").unwrap();
	assert_eq!(delta.read("delta.bin").unwrap(), expected_delta);
	let mut bcj = open("sevenzip-bcj.7z").unwrap();
	assert_eq!(bcj.read("x86.bin").unwrap(), expected_x86);

	let archive = open("sevenzip-default.7z").unwrap();
	let message = archive.entry("nested/message.txt").unwrap();
	assert!(
		message
			.modified_unix_seconds()
			.is_some_and(|time| time > 1_700_000_000)
	);
	assert_eq!(message.mode().unwrap() & 0o170000, 0o100000);
	assert_eq!(archive.entry("nested").unwrap().mode().unwrap() & 0o170000, 0o040000);
}

#[test]
fn decodes_solid_members_and_resolves_unix_symlink() {
	let mut solid = open("sevenzip-solid.7z").unwrap();
	for path in ["alpha.txt", "beta.txt", "nested/message.txt"] {
		assert_eq!(solid.read(path).unwrap(), fixture_bytes(&format!("sevenzip-src/{path}")));
	}

	let mut links = open("sevenzip-symlink.7z").unwrap();
	let link = links.entry("link-to-message").unwrap();
	assert!(link.is_link());
	assert_eq!(link.link_target(), Some("nested/message.txt"));
	assert_eq!(link.mode().unwrap() & 0o170000, 0o120000);
	assert_eq!(
		links.read("link-to-message").unwrap(),
		fixture_bytes("sevenzip-src/nested/message.txt")
	);
}

#[test]
fn drops_traversal_and_reports_unsupported_coders_precisely() {
	let traversal = open("sevenzip-traversal.7z").unwrap();
	assert_eq!(traversal.entries().count(), 0);

	let Err(encrypted) = open("sevenzip-encrypted.7z") else {
		panic!("encrypted 7z unexpectedly opened");
	};
	assert!(matches!(encrypted, Error::UnsupportedFeature(feature) if feature.contains("7zAES")));
	let Err(ppmd) = open("sevenzip-ppmd.7z") else {
		panic!("PPMd 7z unexpectedly opened");
	};
	assert!(matches!(ppmd, Error::UnsupportedFeature(feature) if feature.contains("PPMd")));
}

#[test]
fn validates_framing_crc_and_limits() {
	let original = fixture_bytes("sevenzip-copy.7z");
	assert!(Archive::from_bytes_with_format(&original[..20], Format::SevenZip).is_err());
	let mut corrupt = original.clone();
	corrupt[32] ^= 0x80;
	assert!(Archive::from_bytes_with_format(&corrupt, Format::SevenZip).is_err());
	let limits = Limits::DEFAULT.with_max_member_size(1);
	assert!(matches!(
		Archive::from_bytes_with_format_and_limits(&original, Format::SevenZip, limits),
		Err(Error::MemberTooLarge { .. })
	));
}
