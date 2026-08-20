//! CPIO wire variants and RPM payload composition.

mod support;

use omp_ar::{Archive, Error, Format};
use support::fixtures::fixture_bytes;

const CPIO_VARIANTS: &[&str] =
	&["cpio-newc.cpio", "cpio-crc.cpio", "cpio-odc.cpio", "cpio-bin-le.cpio", "cpio-bin-be.cpio"];

#[test]
fn lists_and_extracts_every_cpio_wire_variant() {
	for name in CPIO_VARIANTS {
		let bytes = fixture_bytes(name);
		assert_eq!(Format::sniff(&bytes), Some(Format::Cpio), "{name}");
		let mut archive = Archive::from_bytes_with_format(&bytes, Format::Cpio).unwrap();
		for path in ["root.txt", "hard.txt", "dir", "dir/file.txt", "dir/link"] {
			assert!(archive.entry(path).is_some(), "{name}: missing {path}");
		}
		let directory = archive.entry("dir").unwrap();
		assert!(directory.is_directory(), "{name}");
		assert_eq!(directory.mode(), Some(0o040750), "{name}");
		let nested = archive.entry("dir/file.txt").unwrap();
		assert_eq!(nested.mode(), Some(0o100644), "{name}");
		assert_eq!(nested.modified_unix_seconds(), Some(1_700_000_000), "{name}");
		assert_eq!(archive.read("dir/file.txt").unwrap(), b"nested payload\n", "{name}");

		let root_link = archive.entry("root.txt").unwrap().is_link();
		let (member, link) = if root_link {
			("hard.txt", "root.txt")
		} else {
			("root.txt", "hard.txt")
		};
		assert_eq!(archive.read(member).unwrap(), b"root payload\n", "{name}");
		let linked = archive.entry(link).unwrap();
		assert!(linked.is_link(), "{name}");
		assert_eq!(linked.link_target(), Some(member), "{name}");
		assert_eq!(linked.size(), 13, "{name}");
		assert_eq!(archive.read(link).unwrap(), b"root payload\n", "{name}");

		let symbolic = archive.entry("dir/link").unwrap();
		assert!(symbolic.is_link(), "{name}");
		assert_eq!(symbolic.mode(), Some(0o120755), "{name}");
		assert_eq!(symbolic.link_target(), Some("root.txt"), "{name}");
		assert_eq!(archive.read("dir/link").unwrap(), b"root payload\n", "{name}");
	}
}

#[test]
fn reads_bsdtar_cpio_and_skips_special_nodes() {
	let bytes = fixture_bytes("cpio-bsdtar.cpio");
	let mut archive = Archive::from_bytes_with_format(&bytes, Format::Cpio).unwrap();
	assert_eq!(archive.read("dir/file.txt").unwrap(), b"nested payload\n");
	assert_eq!(archive.entry("dir/link").unwrap().link_target(), Some("root.txt"));

	let bytes = fixture_bytes("cpio-specials.cpio");
	let mut archive = Archive::from_bytes_with_format(&bytes, Format::Cpio).unwrap();
	for special in ["pipe", "tty", "socket"] {
		assert!(archive.entry(special).is_none());
	}
	assert_eq!(archive.read("kept.txt").unwrap(), b"kept\n");
}

#[test]
fn rejects_bad_crc_missing_trailer_and_unsafe_paths() {
	let mut crc = fixture_bytes("cpio-crc.cpio");
	let payload = crc
		.windows(b"nested payload\n".len())
		.position(|window| window == b"nested payload\n")
		.unwrap();
	crc[payload] ^= 0x20;
	assert!(matches!(
		Archive::from_bytes_with_format(&crc, Format::Cpio),
		Err(Error::InvalidArchive(_))
	));

	let bytes = fixture_bytes("cpio-newc.cpio");
	let trailer = bytes
		.windows(10)
		.position(|window| window == b"TRAILER!!!")
		.unwrap();
	assert!(matches!(
		Archive::from_bytes_with_format(&bytes[..trailer.saturating_sub(110)], Format::Cpio),
		Err(Error::InvalidArchive(_)) | Err(Error::Io(_))
	));

	let bytes = fixture_bytes("cpio-traversal.cpio");
	let mut archive = Archive::from_bytes_with_format(&bytes, Format::Cpio).unwrap();
	assert!(archive.entry("../escape.txt").is_none());
	assert!(archive.entry("escape.txt").is_none());
	assert_eq!(archive.read("safe.txt").unwrap(), b"safe\n");
}

#[test]
fn rpm_payload_compressors_extract_fully() {
	for compressor in ["gzip", "bzip2", "xz", "zstd", "lzma", "lzma-unknown"] {
		let name = format!("minimal-{compressor}.rpm");
		let bytes = fixture_bytes(&name);
		assert_eq!(Format::sniff(&bytes), Some(Format::Rpm), "{name}");
		let mut archive = Archive::from_bytes_with_format(&bytes, Format::Rpm)
			.unwrap_or_else(|error| panic!("{name}: {error}"));
		assert!(archive.entry("root.txt").is_some(), "{name}");
		assert_eq!(archive.read("dir/file.txt").unwrap(), b"nested payload\n", "{name}");
		let member = if archive.entry("root.txt").unwrap().is_link() {
			"hard.txt"
		} else {
			"root.txt"
		};
		assert_eq!(archive.read(member).unwrap(), b"root payload\n", "{name}");
	}
}

#[test]
fn rpm_uses_payload_magic_when_the_compressor_tag_is_unknown() {
	let mut bytes = fixture_bytes("minimal-gzip.rpm");
	let compressor = bytes
		.windows(5)
		.position(|window| window == b"gzip\0")
		.unwrap();
	bytes[compressor..compressor + 4].copy_from_slice(b"xxxx");
	let mut archive = Archive::from_bytes_with_format(&bytes, Format::Rpm).unwrap();
	assert_eq!(archive.read("dir/file.txt").unwrap(), b"nested payload\n");
}
