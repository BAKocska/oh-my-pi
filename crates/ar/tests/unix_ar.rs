//! Unix ar dialects, COFF import libraries, and Debian package composition.

mod support;

use omp_ar::{Archive, Error, Format};
use support::fixtures::fixture_bytes;

#[test]
fn reads_bsd_short_extended_and_darwin_names() {
	let bytes = fixture_bytes("unix-bsdtar.a");
	assert_eq!(Format::sniff(&bytes), Some(Format::Ar));
	let mut archive = Archive::from_bytes_with_format(&bytes, Format::Ar).unwrap();
	let paths: Vec<_> = archive.entries().map(|entry| entry.path()).collect();
	assert_eq!(paths, ["deep.txt", "short.txt", "this-is-a-very-long-member-name.txt"]);
	assert_eq!(archive.read("short.txt").unwrap(), b"short member\n");
	assert_eq!(archive.read("deep.txt").unwrap(), b"nested member\n");
	assert_eq!(archive.read("this-is-a-very-long-member-name.txt").unwrap(), b"long member\n");
	let long = archive
		.entry("this-is-a-very-long-member-name.txt")
		.unwrap();
	assert_eq!(long.mode(), Some(0o100644));
	assert!(long.modified_unix_seconds().is_some_and(|time| time > 0));

	let bytes = fixture_bytes("unix-bsd.a");
	let mut archive = Archive::from_bytes_with_format(&bytes, Format::Ar).unwrap();
	let paths: Vec<_> = archive.entries().map(|entry| entry.path()).collect();
	assert_eq!(paths, ["short.txt", "this-is-a-very-long-member-name.txt"]);
	assert!(
		archive
			.entries()
			.all(|entry| !entry.path().contains("SYMDEF"))
	);
	assert!(
		archive
			.read("short.txt")
			.unwrap()
			.starts_with(b"short member\n")
	);
}

#[test]
fn resolves_gnu_long_names_and_accepts_blank_optional_numbers() {
	let bytes = fixture_bytes("unix-gnu.a");
	let mut archive = Archive::from_bytes_with_format(&bytes, Format::Ar).unwrap();
	let paths: Vec<_> = archive.entries().map(|entry| entry.path()).collect();
	assert_eq!(paths, ["nested", "nested/really-long-file-name.txt", "short.txt"]);
	let long = archive.entry("nested/really-long-file-name.txt").unwrap();
	assert_eq!(long.mode(), Some(0o100644));
	assert_eq!(long.modified_unix_seconds(), Some(1_700_000_000));
	assert_eq!(archive.read("nested/really-long-file-name.txt").unwrap(), b"gnu long\n");

	let mut bytes = fixture_bytes("unix-gnu.a");
	let short_header = 192;
	bytes[short_header + 16..short_header + 28].fill(b' ');
	bytes[short_header + 40..short_header + 48].fill(b' ');
	let mut archive = Archive::from_bytes_with_format(&bytes, Format::Ar).unwrap();
	let short = archive.entry("short.txt").unwrap();
	assert_eq!(short.modified_unix_seconds(), None);
	assert_eq!(short.mode(), None);
	assert_eq!(archive.read("short.txt").unwrap(), b"short gnu\n");
}

#[test]
fn lists_coff_import_members_and_omits_linker_tables() {
	let bytes = fixture_bytes("windows-import.lib");
	let mut archive = Archive::from_bytes_with_format(&bytes, Format::Ar).unwrap();
	let paths: Vec<_> = archive.entries().map(|entry| entry.path()).collect();
	assert_eq!(paths, ["short.obj", "very-long-windows-object-name.obj"]);
	assert_eq!(archive.read("short.obj").unwrap(), b"COFF-short");
	assert_eq!(archive.read("very-long-windows-object-name.obj").unwrap(), b"COFF-long");
}

#[test]
fn rejects_truncated_malformed_and_missing_long_name_metadata() {
	let original = fixture_bytes("unix-gnu.a");
	assert!(matches!(
		Archive::from_bytes_with_format(&original[..original.len() - 1], Format::Ar),
		Err(Error::InvalidArchive(_)) | Err(Error::Io(_))
	));

	let mut bad_header = original.clone();
	bad_header[8 + 58] = 0;
	assert!(matches!(
		Archive::from_bytes_with_format(&bad_header, Format::Ar),
		Err(Error::InvalidArchive(_))
	));

	let mut missing_table = original;
	missing_table[8 + 60 + 4..8 + 60 + 4 + 9].copy_from_slice(b"not-table");
	assert!(matches!(
		Archive::from_bytes_with_format(&missing_table, Format::Ar),
		Err(Error::InvalidArchive(_))
	));
}

#[test]
fn deb_exposes_version_control_and_data_members() {
	let bytes = fixture_bytes("tiny.deb");
	assert_eq!(Format::sniff(&bytes), Some(Format::Deb));
	let mut archive = Archive::from_bytes_with_format(&bytes, Format::Deb).unwrap();
	let paths: Vec<_> = archive.entries().map(|entry| entry.path()).collect();
	assert_eq!(paths, [
		"control",
		"control/control",
		"control/escape-link",
		"control/missing-link",
		"control/postinst",
		"debian-binary",
		"usr",
		"usr/share",
		"usr/share/demo",
		"usr/share/demo/hello.txt",
	]);
	assert_eq!(archive.read("debian-binary").unwrap(), b"2.0\n");
	assert!(
		archive
			.read("control/control")
			.unwrap()
			.windows(14)
			.any(|window| window == b"Package: tiny\n")
	);
	assert_eq!(archive.entry("control/postinst").unwrap().mode().unwrap() & 0o777, 0o755);
	assert_eq!(
		archive.entry("control/missing-link").unwrap().link_target(),
		Some("control/missing-target")
	);
	assert_eq!(archive.entry("control/escape-link").unwrap().link_target(), Some("../../outside"));
	assert_eq!(archive.read("usr/share/demo/hello.txt").unwrap(), b"hello from deb\n");
}

#[test]
fn deb_inner_compressors_extract_fully() {
	for name in
		["tiny-uncompressed.deb", "tiny-bz2.deb", "tiny-xz.deb", "tiny-zst.deb", "tiny-lzma.deb"]
	{
		let bytes = fixture_bytes(name);
		let mut archive = Archive::from_bytes_with_format(&bytes, Format::Deb)
			.unwrap_or_else(|error| panic!("{name}: {error}"));
		assert!(
			archive
				.read("control/control")
				.unwrap()
				.windows(14)
				.any(|window| window == b"Package: tiny\n"),
			"{name}"
		);
		assert_eq!(archive.read("usr/share/demo/hello.txt").unwrap(), b"hello from deb\n", "{name}");
	}
}

#[test]
fn deb_requires_the_version_member_first() {
	let bytes = fixture_bytes("unix-gnu.a");
	assert_ne!(Format::sniff(&bytes), Some(Format::Deb));
	assert!(matches!(
		Archive::from_bytes_with_format(&bytes, Format::Deb),
		Err(Error::InvalidArchive(_))
	));
}
