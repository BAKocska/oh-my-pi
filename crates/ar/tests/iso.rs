//! ISO 9660, Joliet, and Rock Ridge interoperability and rejection contracts.

mod support;

use std::{
	cell::Cell,
	io::{Cursor, Read, Seek, SeekFrom},
	rc::Rc,
};

use omp_ar::{Archive, Error, Format, Limits};
use support::fixtures::fixture_bytes;

const BLOCK: usize = 2048;

#[test]
fn lists_joliet_tree_merges_rock_ridge_metadata_and_reads_links() {
	let bytes = fixture_bytes("rock-ridge-joliet.iso");
	assert_eq!(Format::sniff(&bytes[..40 * 1024]), Some(Format::Iso));
	let mut archive = Archive::from_bytes(&bytes).unwrap();
	let paths: Vec<_> = archive.entries().map(|entry| entry.path()).collect();
	assert_eq!(paths, [
		"empty-dir",
		"hello.txt",
		"nested",
		"nested/deep",
		"nested/deep/level",
		"nested/deep/level/three",
		"nested/deep/level/three/end.bin",
		"nested/hello-link",
		"nested/日本語-long-name.txt",
	]);
	assert!(archive.entry("empty-dir").unwrap().is_directory());
	assert!(
		archive
			.entry("nested/deep/level/three")
			.unwrap()
			.is_directory()
	);
	assert_eq!(archive.entry("hello.txt").unwrap().mode().unwrap() & 0o170000, 0o100000);
	let link = archive.entry("nested/hello-link").unwrap();
	assert_eq!(link.link_target(), Some("hello.txt"));
	assert_eq!(link.mode().unwrap() & 0o170000, 0o120000);
	assert!(link.modified_unix_seconds().is_some());
	assert_eq!(archive.read("hello.txt").unwrap(), b"hello from iso\n");
	assert_eq!(archive.read("nested/日本語-long-name.txt").unwrap(), b"unicode payload\n");
	assert_eq!(archive.read("nested/deep/level/three/end.bin").unwrap(), b"deep end\n");
	assert_eq!(archive.read("nested/hello-link").unwrap(), b"hello from iso\n");
}

#[test]
fn assembles_multi_extent_interleaved_and_extended_attribute_members() {
	let mut image = image_with_root(Vec::new(), 31);
	let mut directory = Vec::new();
	directory.extend(record(&[0], 20, BLOCK as u32, 0x02, 0, 0, 0, &[]));
	directory.extend(record(&[1], 20, BLOCK as u32, 0x02, 0, 0, 0, &[]));
	directory.extend(record(b"MULTI.BIN;1", 21, 3, 0x80, 0, 0, 0, &[]));
	directory.extend(record(b"MULTI.BIN;1", 22, 2, 0, 0, 0, 0, &[]));
	directory.extend(record(b"INTER.BIN;1", 23, 3000, 0, 0, 1, 1, &[]));
	directory.extend(record(b"XATTR.BIN;1", 26, 4, 0, 1, 0, 0, &[]));
	image[20 * BLOCK..20 * BLOCK + directory.len()].copy_from_slice(&directory);
	image[21 * BLOCK..21 * BLOCK + 3].copy_from_slice(b"abc");
	image[22 * BLOCK..22 * BLOCK + 2].copy_from_slice(b"de");
	image[23 * BLOCK..24 * BLOCK].fill(b'x');
	image[25 * BLOCK..25 * BLOCK + 952].fill(b'y');
	image[26 * BLOCK..27 * BLOCK].fill(b'a');
	image[27 * BLOCK..27 * BLOCK + 4].copy_from_slice(b"data");

	let mut archive = Archive::from_bytes_with_format(&image, Format::Iso).unwrap();
	assert_eq!(archive.read("MULTI.BIN").unwrap(), b"abcde");
	let interleaved = archive.read("INTER.BIN").unwrap();
	assert_eq!(interleaved.len(), 3000);
	assert!(interleaved[..2048].iter().all(|byte| *byte == b'x'));
	assert!(interleaved[2048..].iter().all(|byte| *byte == b'y'));
	assert_eq!(archive.read("XATTR.BIN").unwrap(), b"data");
}

#[test]
fn leaves_member_extents_unread_until_extraction() {
	let mut image = image_with_root(Vec::new(), 23);
	let file = record(b"LAZY.BIN;1", 21, 4, 0, 0, 0, 0, &[]);
	let directory_offset = 20 * BLOCK
		+ record(&[0], 20, BLOCK as u32, 0x02, 0, 0, 0, &[]).len()
		+ record(&[1], 20, BLOCK as u32, 0x02, 0, 0, 0, &[]).len();
	image[directory_offset..directory_offset + file.len()].copy_from_slice(&file);
	image[21 * BLOCK..21 * BLOCK + 4].copy_from_slice(b"lazy");
	let payload_read = Rc::new(Cell::new(false));
	let source = TrackingReader {
		inner:         Cursor::new(image),
		tracked_start: (21 * BLOCK) as u64,
		tracked_end:   (22 * BLOCK) as u64,
		touched:       Rc::clone(&payload_read),
	};
	let mut archive = Archive::with_format(source, Format::Iso).unwrap();
	assert!(!payload_read.get());
	assert_eq!(archive.read("LAZY.BIN").unwrap(), b"lazy");
	assert!(payload_read.get());
}

#[test]
fn follows_susp_continuations_and_rejects_continuation_cycles() {
	let continuation = susp_nm(b"continued.txt");
	let mut image = rock_ridge_continuation_image(&continuation);
	let archive = Archive::from_bytes_with_format(&image, Format::Iso).unwrap();
	assert!(archive.entry("continued.txt").is_some());

	let cyclic = susp_ce(30, 0, 28);
	image = rock_ridge_continuation_image(&cyclic);
	assert_error_contains(
		Archive::from_bytes_with_format(&image, Format::Iso),
		"cyclic ISO SUSP continuation",
	);
}

#[test]
fn rejects_truncation_endian_mismatch_relocation_and_directory_cycles() {
	let fixture = fixture_bytes("rock-ridge-joliet.iso");
	assert!(Archive::from_bytes_with_format(&fixture[..64 * 1024], Format::Iso).is_err());

	let mut endian = fixture.clone();
	let supplementary = (16..endian.len() / BLOCK)
		.map(|sector| sector * BLOCK)
		.find(|offset| endian[*offset] == 2 && &endian[*offset + 1..*offset + 6] == b"CD001")
		.unwrap();
	endian[supplementary + 130] ^= 1;
	assert_error_contains(
		Archive::from_bytes_with_format(&endian, Format::Iso),
		"logical block size has mismatched byte orders",
	);

	let mut relocation = fixture;
	let sl = relocation
		.windows(4)
		.position(|window| window == [b'S', b'L', 0x12, 1])
		.unwrap();
	relocation[sl] = b'C';
	assert_error_contains(
		Archive::from_bytes_with_format(&relocation, Format::Iso),
		"Rock Ridge relocated directory (CL)",
	);

	let loop_record = record(b"LOOP", 20, BLOCK as u32, 0x02, 0, 0, 0, &[]);
	let mut cyclic_directory = image_with_root(loop_record, 22);
	let dots = [
		record(&[0], 20, BLOCK as u32, 0x02, 0, 0, 0, &[]),
		record(&[1], 20, BLOCK as u32, 0x02, 0, 0, 0, &[]),
	]
	.concat();
	cyclic_directory[20 * BLOCK..20 * BLOCK + dots.len()].copy_from_slice(&dots);
	let loop_offset = 20 * BLOCK + dots.len();
	cyclic_directory[loop_offset..loop_offset + 38].copy_from_slice(&record(
		b"LOOP",
		20,
		BLOCK as u32,
		0x02,
		0,
		0,
		0,
		&[],
	));
	assert_error_contains(
		Archive::from_bytes_with_format(&cyclic_directory, Format::Iso),
		"cyclic ISO directory extent",
	);
}

#[test]
fn distinguishes_high_sierra_and_udf_only_images() {
	for (signature, offset, expected) in
		[(b"CDROM".as_slice(), 9, "High Sierra"), (b"NSR02".as_slice(), 1, "UDF-only")]
	{
		let mut image = vec![0_u8; 17 * BLOCK];
		image[16 * BLOCK + offset..16 * BLOCK + offset + signature.len()].copy_from_slice(signature);
		image[16 * BLOCK + offset + 5] = 1;
		assert_error_contains(Archive::from_bytes_with_format(&image, Format::Iso), expected);
	}
}

#[test]
fn ignores_el_torito_boot_descriptors_in_hybrid_images() {
	let mut image = image_with_root(Vec::new(), 31);
	image[17 * BLOCK..18 * BLOCK].fill(0);
	image[17 * BLOCK] = 0;
	image[17 * BLOCK + 1..17 * BLOCK + 6].copy_from_slice(b"CD001");
	image[17 * BLOCK + 6] = 1;
	image[17 * BLOCK + 7..17 * BLOCK + 30].copy_from_slice(b"EL TORITO SPECIFICATION");
	image[18 * BLOCK] = 255;
	image[18 * BLOCK + 1..18 * BLOCK + 6].copy_from_slice(b"CD001");
	image[18 * BLOCK + 6] = 1;
	assert!(Archive::from_bytes_with_format(&image, Format::Iso).is_ok());
}

#[test]
fn enforces_metadata_entry_member_and_path_limits() {
	let bytes = fixture_bytes("rock-ridge-joliet.iso");
	assert!(matches!(
		Archive::from_bytes_with_format_and_limits(
			&bytes,
			Format::Iso,
			Limits::DEFAULT.with_max_index_size(2047),
		),
		Err(Error::IndexTooLarge { .. })
	));
	assert!(matches!(
		Archive::from_bytes_with_format_and_limits(
			&bytes,
			Format::Iso,
			Limits::DEFAULT.with_max_entries(2),
		),
		Err(Error::TooManyEntries { .. })
	));
	assert!(matches!(
		Archive::from_bytes_with_format_and_limits(
			&bytes,
			Format::Iso,
			Limits::DEFAULT.with_max_member_size(8),
		),
		Err(Error::MemberTooLarge { .. })
	));
	assert!(matches!(
		Archive::from_bytes_with_format_and_limits(
			&bytes,
			Format::Iso,
			Limits::DEFAULT.with_max_path_size(5),
		),
		Err(Error::PathTooLong { .. })
	));
}

fn rock_ridge_continuation_image(continuation: &[u8]) -> Vec<u8> {
	let sp = [b'S', b'P', 7, 1, 0xbe, 0xef, 0];
	let ce = susp_ce(30, 0, continuation.len() as u32);
	let mut directory = Vec::new();
	directory.extend(record(&[0], 20, BLOCK as u32, 0x02, 0, 0, 0, &sp));
	directory.extend(record(&[1], 20, BLOCK as u32, 0x02, 0, 0, 0, &[]));
	directory.extend(record(b"FILE.;1", 21, 1, 0, 0, 0, 0, &ce));
	let mut image = image_with_root(Vec::new(), 31);
	image[20 * BLOCK..20 * BLOCK + directory.len()].copy_from_slice(&directory);
	image[21 * BLOCK] = b'x';
	image[30 * BLOCK..30 * BLOCK + continuation.len()].copy_from_slice(continuation);
	image
}

fn image_with_root(extra_root_records: Vec<u8>, sectors: usize) -> Vec<u8> {
	let mut image = vec![0_u8; sectors * BLOCK];
	let mut pvd = vec![0_u8; BLOCK];
	pvd[0] = 1;
	pvd[1..6].copy_from_slice(b"CD001");
	pvd[6] = 1;
	both_u32(&mut pvd[80..88], sectors as u32);
	both_u16(&mut pvd[120..124], 1);
	both_u16(&mut pvd[124..128], 1);
	both_u16(&mut pvd[128..132], BLOCK as u16);
	both_u32(&mut pvd[132..140], 0);
	let root = record(&[0], 20, BLOCK as u32, 0x02, 0, 0, 0, &[]);
	pvd[156..156 + root.len()].copy_from_slice(&root);
	image[16 * BLOCK..17 * BLOCK].copy_from_slice(&pvd);
	image[17 * BLOCK] = 255;
	image[17 * BLOCK + 1..17 * BLOCK + 6].copy_from_slice(b"CD001");
	image[17 * BLOCK + 6] = 1;
	let mut directory = Vec::new();
	directory.extend(record(&[0], 20, BLOCK as u32, 0x02, 0, 0, 0, &[]));
	directory.extend(record(&[1], 20, BLOCK as u32, 0x02, 0, 0, 0, &[]));
	directory.extend(extra_root_records);
	image[20 * BLOCK..20 * BLOCK + directory.len()].copy_from_slice(&directory);
	image
}

#[allow(
	clippy::too_many_arguments,
	reason = "arguments map directly to ISO directory record fields"
)]
fn record(
	name: &[u8],
	extent: u32,
	size: u32,
	flags: u8,
	extended_attributes: u8,
	unit: u8,
	gap: u8,
	system_use: &[u8],
) -> Vec<u8> {
	let padding = usize::from(name.len() % 2 == 0);
	let length = 33 + name.len() + padding + system_use.len();
	let mut record = vec![0_u8; length];
	record[0] = length as u8;
	record[1] = extended_attributes;
	both_u32(&mut record[2..10], extent);
	both_u32(&mut record[10..18], size);
	record[18..25].copy_from_slice(&[124, 1, 2, 3, 4, 5, 0]);
	record[25] = flags;
	record[26] = unit;
	record[27] = gap;
	both_u16(&mut record[28..32], 1);
	record[32] = name.len() as u8;
	record[33..33 + name.len()].copy_from_slice(name);
	let system_use_offset = 33 + name.len() + padding;
	record[system_use_offset..].copy_from_slice(system_use);
	record
}

fn susp_nm(name: &[u8]) -> Vec<u8> {
	let mut record = vec![b'N', b'M', (5 + name.len()) as u8, 1, 0];
	record.extend_from_slice(name);
	record.extend_from_slice(&[b'S', b'T', 4, 1]);
	record
}

fn susp_ce(block: u32, offset: u32, length: u32) -> Vec<u8> {
	let mut record = vec![b'C', b'E', 28, 1];
	let start = record.len();
	record.resize(start + 24, 0);
	both_u32(&mut record[start..start + 8], block);
	both_u32(&mut record[start + 8..start + 16], offset);
	both_u32(&mut record[start + 16..start + 24], length);
	record
}

fn both_u16(field: &mut [u8], value: u16) {
	field[..2].copy_from_slice(&value.to_le_bytes());
	field[2..4].copy_from_slice(&value.to_be_bytes());
}

fn both_u32(field: &mut [u8], value: u32) {
	field[..4].copy_from_slice(&value.to_le_bytes());
	field[4..8].copy_from_slice(&value.to_be_bytes());
}

struct TrackingReader {
	inner:         Cursor<Vec<u8>>,
	tracked_start: u64,
	tracked_end:   u64,
	touched:       Rc<Cell<bool>>,
}

impl Read for TrackingReader {
	fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
		let start = self.inner.position();
		let requested_end = start.saturating_add(buffer.len() as u64);
		if start < self.tracked_end && requested_end > self.tracked_start {
			self.touched.set(true);
		}
		self.inner.read(buffer)
	}
}

impl Seek for TrackingReader {
	fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
		self.inner.seek(position)
	}
}

fn assert_error_contains<T>(result: omp_ar::Result<T>, expected: &str) {
	match result {
		Err(error) => assert!(
			error.to_string().contains(expected),
			"expected error containing {expected:?}, got {error:?}"
		),
		Ok(_) => panic!("operation unexpectedly succeeded"),
	}
}

#[test]
fn zero_length_symlink_records_with_junk_extents_are_accepted() {
	// bsdtar's Joliet record for a Rock Ridge symlink is a zero-length file
	// whose extent points at an unallocated block; 7-Zip and bsdtar accept
	// these, so indexing must tolerate junk extents on empty members and the
	// Rock Ridge merge must surface the link.
	let bytes = fixture_bytes("minimal-symlink.iso");
	let mut archive = Archive::from_bytes_with_format(&bytes, Format::Iso).unwrap();
	let link = archive.entry("link.txt").unwrap();
	assert!(link.is_link());
	assert_eq!(link.link_target(), Some("real.txt"));
	assert_eq!(archive.read("real.txt").unwrap(), b"target\n");
	assert_eq!(archive.read("link.txt").unwrap(), b"target\n");
}
