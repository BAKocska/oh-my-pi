//! RAR4/RAR5 indexing, extraction, filters, solid chains, and rejection paths.

mod support;

use std::{
	cell::RefCell,
	io::{self, Cursor, Read, Seek, SeekFrom},
	ops::Range,
	rc::Rc,
};

use omp_ar::{Archive, Error, Format, Limits};
use support::fixtures::fixture_bytes;

const HASH_HELLO: &str = "7f1620bec2523375e14494eade9ebdc362926b11904813809c503f2eb691aeff";
const HASH_DATA: &str = "10fc3c51a152e90e5b90319b601d92ccf37290ef53c35ff92507687d8a911a08";
const HASH_UNICODE: &str = "1dcc4cea49428fe3a0d2df11ba03ed049e3860511144de489f2e5a0cc53c989d";
const HASH_X86: &str = "2aad26e4b16a8535154aa7948ed00398f04104a65b1dfe34e89bd64235b6999d";

#[test]
fn rar5_store_lists_unicode_metadata_and_extracts() {
	let bytes = fixture_bytes("rar5-store.rar");
	assert_eq!(Format::sniff(&bytes), Some(Format::Rar));
	let mut archive = Archive::from_bytes_with_format(&bytes, Format::Rar).unwrap();
	assert_regular_hashes(&mut archive, false);
	for path in ["input/hello.txt", "input/nested/data.bin", "input/naive-☃.txt"] {
		let entry = archive.entry(path).unwrap();
		assert!(
			entry
				.modified_unix_seconds()
				.is_some_and(|time| time > 1_700_000_000)
		);
		assert_eq!(entry.mode().unwrap() & 0o170000, 0o100000);
	}
}

#[test]
fn indexing_stored_rar_does_not_read_member_payloads() {
	let bytes = fixture_bytes("rar5-store.rar");
	let payload = b"hello from rar\n";
	let payload_start = bytes
		.windows(payload.len())
		.position(|window| window == payload)
		.unwrap();
	let payload_range = payload_start..payload_start + payload.len();
	let reads = Rc::new(RefCell::new(Vec::new()));
	let reader = TrackingReader { cursor: Cursor::new(bytes.as_slice()), reads: Rc::clone(&reads) };
	let mut archive = Archive::with_format(reader, Format::Rar).unwrap();
	assert!(
		!reads
			.borrow()
			.iter()
			.any(|range| overlaps(range, &payload_range))
	);
	assert_eq!(sha256_hex(&archive.read("input/hello.txt").unwrap()), HASH_HELLO);
	assert!(
		reads
			.borrow()
			.iter()
			.any(|range| overlaps(range, &payload_range))
	);
}

#[test]
fn rar5_default_x86_and_solid_chains_extract() {
	let bytes = fixture_bytes("rar5-default.rar");
	let mut archive = Archive::from_bytes_with_format(&bytes, Format::Rar).unwrap();
	assert_regular_hashes(&mut archive, false);

	let bytes = fixture_bytes("rar5-solid.rar");
	let mut archive = Archive::from_bytes_with_format(&bytes, Format::Rar).unwrap();
	assert_eq!(sha256_hex(&archive.read("input/x86.bin").unwrap()), HASH_X86);
	assert_eq!(sha256_hex(&archive.read("input/hello.txt").unwrap()), HASH_HELLO);
	assert_eq!(sha256_hex(&archive.read("input/nested/data.bin").unwrap()), HASH_DATA);
	let bytes = fixture_bytes("rar5-x86-filter.rar");
	let mut archive = Archive::from_bytes_with_format(&bytes, Format::Rar).unwrap();
	assert_eq!(sha256_hex(&archive.read("input/x86.bin").unwrap()), HASH_X86);
}

#[test]
fn rar5_sfx_and_symlink_are_supported() {
	let original = fixture_bytes("rar5-store.rar");
	let mut prefixed = vec![0xcc; 37];
	prefixed.extend_from_slice(&original);
	assert_eq!(Format::sniff(&prefixed), Some(Format::Rar));
	let mut archive = Archive::from_bytes_with_format(&prefixed, Format::Rar).unwrap();
	assert_eq!(sha256_hex(&archive.read("input/hello.txt").unwrap()), HASH_HELLO);

	let bytes = fixture_bytes("rar5-symlink.rar");
	let mut archive = Archive::from_bytes_with_format(&bytes, Format::Rar).unwrap();
	let link = archive.entry("input/links/hello-link").unwrap();
	assert!(link.is_link());
	assert_eq!(link.link_target(), Some("input/hello.txt"));
	assert_eq!(link.mode().unwrap() & 0o170000, 0o120000);
	assert_eq!(sha256_hex(&archive.read("input/links/hello-link").unwrap()), HASH_HELLO);
}

#[test]
fn rar4_store_default_and_symlink_extract() {
	for name in ["rar4-store.rar", "rar4-default.rar"] {
		let bytes = fixture_bytes(name);
		let mut archive = Archive::from_bytes_with_format(&bytes, Format::Rar).unwrap();
		assert_regular_hashes(&mut archive, false);
	}
	let bytes = fixture_bytes("rar4-symlink.rar");
	let mut archive = Archive::from_bytes_with_format(&bytes, Format::Rar).unwrap();
	let link = archive.entry("input/links/hello-link").unwrap();
	assert!(link.is_link());
	assert_eq!(link.link_target(), Some("input/hello.txt"));
	assert_eq!(link.mode().unwrap() & 0o170000, 0o120000);
	assert_eq!(sha256_hex(&archive.read("input/links/hello-link").unwrap()), HASH_HELLO);
}

#[test]
fn encrypted_volume_recovery_ppmd_and_methods_fail_precisely() {
	for name in [
		"rar5-password.rar",
		"rar5-header-password.rar",
		"rar4-password.rar",
		"rar4-header-password.rar",
	] {
		let bytes = fixture_bytes(name);
		assert!(matches!(
			Archive::from_bytes_with_format(&bytes, Format::Rar),
			Err(Error::Encrypted(_))
		));
	}
	for (name, feature) in [
		("rar5-volume.rar", "multi-volume RAR5 archive"),
		("rar5-recovery.rar", "RAR5 recovery record"),
		("rar4-recovery.rar", "RAR4 recovery record"),
		("rar4-unsupported.rar", "RAR4 compression method 0x36"),
	] {
		let bytes = fixture_bytes(name);
		assert!(matches!(
			Archive::from_bytes_with_format(&bytes, Format::Rar),
			Err(Error::UnsupportedFeature(actual)) if actual == feature
		));
	}

	let bytes = fixture_bytes("rar5-unsupported.rar");
	let mut archive = Archive::from_bytes_with_format(&bytes, Format::Rar).unwrap();
	assert!(matches!(
		archive.read("input/hello.txt"),
		Err(Error::UnsupportedFeature("RAR5 compression method 6"))
	));
	let bytes = fixture_bytes("rar4-ppm.rar");
	let mut archive = Archive::from_bytes_with_format(&bytes, Format::Rar).unwrap();
	assert!(matches!(
		archive.read("input/ppm.txt"),
		Err(Error::UnsupportedFeature("RAR4 PPMd compressed block"))
	));
}

#[test]
fn traversal_corruption_checksums_and_limits_fail_safely() {
	let bytes = fixture_bytes("rar5-traversal.rar");
	let mut archive = Archive::from_bytes_with_format(&bytes, Format::Rar).unwrap();
	assert!(archive.entry("../escape.txtxx").is_none());
	assert_eq!(sha256_hex(&archive.read("input/nested/data.bin").unwrap()), HASH_DATA);

	let original = fixture_bytes("rar5-store.rar");
	assert!(Archive::from_bytes_with_format(&original[..original.len() - 3], Format::Rar).is_err());
	let mut corrupt_header = original.clone();
	corrupt_header[20] ^= 1;
	assert!(matches!(
		Archive::from_bytes_with_format(&corrupt_header, Format::Rar),
		Err(Error::InvalidArchive("RAR5 header CRC32 mismatch"))
	));
	let mut corrupt_payload = original.clone();
	let payload = b"hello from rar\n";
	let offset = corrupt_payload
		.windows(payload.len())
		.position(|window| window == payload)
		.unwrap();
	corrupt_payload[offset] ^= 1;
	let mut archive = Archive::from_bytes_with_format(&corrupt_payload, Format::Rar).unwrap();
	assert!(matches!(archive.read("input/hello.txt"), Err(Error::ChecksumMismatch { .. })));

	let limits = Limits::DEFAULT.with_max_member_size(5);
	assert!(matches!(
		Archive::from_bytes_with_format_and_limits(&original, Format::Rar, limits),
		Err(Error::MemberTooLarge { .. })
	));
	let limits = Limits::DEFAULT.with_max_entries(1);
	assert!(matches!(
		Archive::from_bytes_with_format_and_limits(&original, Format::Rar, limits),
		Err(Error::TooManyEntries { .. })
	));
}

fn assert_regular_hashes(archive: &mut Archive<Cursor<&[u8]>>, include_x86: bool) {
	for (path, expected) in [
		("input/hello.txt", HASH_HELLO),
		("input/nested/data.bin", HASH_DATA),
		("input/naive-☃.txt", HASH_UNICODE),
	] {
		assert_eq!(sha256_hex(&archive.read(path).unwrap()), expected, "{path}");
	}
	if include_x86 {
		assert_eq!(sha256_hex(&archive.read("input/x86.bin").unwrap()), HASH_X86);
	}
}

fn sha256_hex(bytes: &[u8]) -> String {
	const INITIAL: [u32; 8] = [
		0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
		0x5be0cd19,
	];
	const K: [u32; 64] = [
		0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
		0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
		0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
		0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
		0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
		0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
		0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
		0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
		0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
		0xc67178f2,
	];
	let bit_len = (bytes.len() as u64) * 8;
	let mut padded = bytes.to_vec();
	padded.push(0x80);
	while padded.len() % 64 != 56 {
		padded.push(0);
	}
	padded.extend_from_slice(&bit_len.to_be_bytes());
	let mut hash = INITIAL;
	for chunk in padded.as_chunks::<64>().0 {
		let mut schedule = [0u32; 64];
		for (index, word) in schedule[..16].iter_mut().enumerate() {
			*word = u32::from_be_bytes(chunk[index * 4..index * 4 + 4].try_into().unwrap());
		}
		for index in 16..64 {
			let s0 = schedule[index - 15].rotate_right(7)
				^ schedule[index - 15].rotate_right(18)
				^ (schedule[index - 15] >> 3);
			let s1 = schedule[index - 2].rotate_right(17)
				^ schedule[index - 2].rotate_right(19)
				^ (schedule[index - 2] >> 10);
			schedule[index] = schedule[index - 16]
				.wrapping_add(s0)
				.wrapping_add(schedule[index - 7])
				.wrapping_add(s1);
		}
		let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = hash;
		for index in 0..64 {
			let sum1 = h
				.wrapping_add(e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25))
				.wrapping_add((e & f) ^ (!e & g))
				.wrapping_add(K[index])
				.wrapping_add(schedule[index]);
			let sum0 = (a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22))
				.wrapping_add((a & b) ^ (a & c) ^ (b & c));
			h = g;
			g = f;
			f = e;
			e = d.wrapping_add(sum1);
			d = c;
			c = b;
			b = a;
			a = sum0.wrapping_add(sum1);
		}
		for (value, next) in hash.iter_mut().zip([a, b, c, d, e, f, g, h]) {
			*value = value.wrapping_add(next);
		}
	}
	let mut result = String::with_capacity(64);
	for value in hash {
		use std::fmt::Write as _;
		write!(result, "{value:08x}").unwrap();
	}
	result
}

struct TrackingReader<'a> {
	cursor: Cursor<&'a [u8]>,
	reads:  Rc<RefCell<Vec<Range<usize>>>>,
}

impl Read for TrackingReader<'_> {
	fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
		let start = self.cursor.position() as usize;
		let read = self.cursor.read(buffer)?;
		self.reads.borrow_mut().push(start..start + read);
		Ok(read)
	}
}

impl Seek for TrackingReader<'_> {
	fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
		self.cursor.seek(position)
	}
}

const fn overlaps(left: &Range<usize>, right: &Range<usize>) -> bool {
	left.start < right.end && right.start < left.end
}
