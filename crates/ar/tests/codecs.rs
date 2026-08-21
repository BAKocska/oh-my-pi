//! Bzip2 and ncompress stream-decoder contracts.

mod support;

use std::fmt::Write as _;

use omp_ar::{Archive, Error, Format, Limits};
use support::{fixtures::fixture_bytes, tar};

const BZIP_TAR: &[u8] = &[
	0x42, 0x5a, 0x68, 0x39, 0x31, 0x41, 0x59, 0x26, 0x53, 0x59, 0xae, 0x77, 0x3f, 0x97, 0x00, 0x00,
	0x37, 0x5b, 0x81, 0xca, 0x10, 0x40, 0x01, 0x7f, 0x80, 0x00, 0x80, 0x6e, 0x65, 0xdf, 0x60, 0x00,
	0x80, 0x08, 0x08, 0x20, 0x00, 0x74, 0x22, 0x46, 0x04, 0x68, 0x69, 0x93, 0xd1, 0x03, 0x23, 0x4d,
	0xa8, 0x25, 0x4d, 0x11, 0x90, 0x1a, 0x01, 0xa0, 0x68, 0x02, 0x2e, 0xf9, 0x10, 0x1b, 0x20, 0xa0,
	0x7c, 0x5f, 0x30, 0xea, 0xd1, 0xaa, 0x04, 0x56, 0x4b, 0x83, 0xa0, 0x8f, 0x68, 0xa9, 0x67, 0x58,
	0x18, 0xb0, 0x31, 0x62, 0xf6, 0x18, 0x2d, 0x2e, 0x17, 0x20, 0xea, 0x66, 0x00, 0xb4, 0x9c, 0x3b,
	0x10, 0x2a, 0x41, 0xef, 0xe5, 0x75, 0x8f, 0x92, 0x2a, 0x12, 0x3d, 0xaa, 0x9a, 0xd0, 0xce, 0xeb,
	0x11, 0x26, 0xc5, 0x8c, 0x59, 0x23, 0x1b, 0xf1, 0x77, 0x24, 0x53, 0x85, 0x09, 0x0a, 0xe7, 0x73,
	0xf9, 0x70,
];

#[test]
fn bzip2_decodes_level_one_and_level_nine_multiblock_streams() {
	for (name, length, expected_hash) in [
		(
			"bzip-level-1.txt.bz2",
			813_520,
			"81e63e3c3942b040fbcd62e0dcfcccfa97c95ede85c0d9df1e60d9452574728b",
		),
		(
			"bzip-level-9.txt.bz2",
			1_829_150,
			"63d3996fced6e1df4e9346ba5378620800a95ce77d51b5088863db96675a0912",
		),
	] {
		let compressed = fixture_bytes(&format!("codecs/{name}"));
		let mut archive = Archive::from_bytes_with_format(&compressed, Format::Bz2).unwrap();
		let decoded = archive.read("data").unwrap();
		assert_eq!(decoded.len(), length);
		assert_eq!(sha256_hex(&decoded), expected_hash);
	}
}

#[test]
fn bzip2_decodes_concatenated_streams() {
	let compressed = fixture_bytes("codecs/bzip-concatenated.bz2");
	let mut archive = Archive::from_bytes_with_format(&compressed, Format::Bz2).unwrap();
	let decoded = archive.read("data").unwrap();
	assert_eq!(
		decoded,
		b"first concatenated stream\nsecond concatenated stream\nwith another line\n"
	);
	assert_eq!(
		sha256_hex(&decoded),
		"47f7efe1fd83980f616b5e824ce079c39d83d70898a1acc7d3fa9e649d1c3cfd"
	);
}

#[test]
fn bzip2_rejects_truncation_bad_crc_and_output_overflow() {
	let compressed = fixture_bytes("codecs/bzip-level-1.txt.bz2");
	let truncated = &compressed[..compressed.len() - 1];
	assert!(matches!(
		archive_error(truncated, Format::Bz2, Limits::DEFAULT),
		Error::InvalidArchive("truncated bzip2 stream")
	));

	let mut bad_crc = compressed.clone();
	bad_crc[10] ^= 1;
	assert!(matches!(
		archive_error(&bad_crc, Format::Bz2, Limits::DEFAULT),
		Error::InvalidArchive("bzip2 block CRC mismatch")
	));

	assert!(matches!(
		archive_error(&compressed, Format::Bz2, Limits::DEFAULT.with_max_archive_size(1_000),),
		Error::ArchiveTooLarge { limit: 1_000, .. }
	));
}

#[test]
fn bzip2_formats_cover_sniffed_single_stream_and_tar_inner_paths() {
	let compressed = fixture_bytes("codecs/bzip-concatenated.bz2");
	let mut sniffed = Archive::from_bytes(&compressed).unwrap();
	assert_eq!(sniffed.format(), Format::TarBz2);
	assert_eq!(sniffed.read("data").unwrap().len(), 71);

	let mut explicit = Archive::from_bytes_with_format(&compressed, Format::Bz2).unwrap();
	assert_eq!(explicit.format(), Format::Bz2);
	assert_eq!(explicit.read("data").unwrap().len(), 71);

	let mut tar = Archive::from_bytes_with_format(BZIP_TAR, Format::TarBz2).unwrap();
	assert_eq!(tar.format(), Format::TarBz2);
	assert_eq!(tar.read("inner.txt").unwrap(), b"archive codec tar payload\n");
}

#[test]
fn ncompress_decodes_sixteen_bit_block_mode_stream() {
	let compressed = fixture_bytes("codecs/ncompress.bin.Z");
	let mut archive = Archive::from_bytes_with_format(&compressed, Format::Z).unwrap();
	let decoded = archive.read("data").unwrap();
	assert_eq!(decoded.len(), 180_000);
	assert_eq!(
		sha256_hex(&decoded),
		"35b19599038e534308e21e6c57cb60953ea6fb2e559ced2f870f76c3a16b2dc6"
	);
}

#[test]
fn ncompress_resets_after_clear_code_group() {
	let compressed =
		[0x1f, 0x9d, 0x89, 0x41, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x42, 0x00];
	let mut archive = Archive::from_bytes_with_format(&compressed, Format::Z).unwrap();
	assert_eq!(archive.read("data").unwrap(), b"AB");
}

#[test]
fn ncompress_rejects_invalid_headers_codes_padding_and_output_overflow() {
	assert!(matches!(
		archive_error(&[0x1f, 0x9d], Format::Z, Limits::DEFAULT),
		Error::InvalidArchive("truncated compress (.Z) header")
	));
	assert!(matches!(
		archive_error(&[0x1f, 0x9d, 0xa9], Format::Z, Limits::DEFAULT),
		Error::UnsupportedFeature("compress (.Z) reserved header flags")
	));
	assert!(matches!(
		archive_error(&[0x1f, 0x9d, 0x90, 0x01, 0x01], Format::Z, Limits::DEFAULT),
		Error::InvalidArchive("corrupt compress (.Z) dictionary code")
	));

	let compressed = fixture_bytes("codecs/ncompress.bin.Z");
	assert!(matches!(
		archive_error(&compressed[..compressed.len() - 1], Format::Z, Limits::DEFAULT),
		Error::InvalidArchive("non-zero compress (.Z) padding")
	));
	assert!(matches!(
		archive_error(&compressed, Format::Z, Limits::DEFAULT.with_max_archive_size(1_024),),
		Error::ArchiveTooLarge { limit: 1_024, .. }
	));
}

#[test]
fn ncompress_formats_cover_sniffed_single_stream_and_tar_inner_paths() {
	let compressed = fixture_bytes("codecs/ncompress.bin.Z");
	let mut sniffed = Archive::from_bytes(&compressed).unwrap();
	assert_eq!(sniffed.format(), Format::TarZ);
	assert_eq!(sniffed.read("data").unwrap().len(), 180_000);

	let mut explicit = Archive::from_bytes_with_format(&compressed, Format::Z).unwrap();
	assert_eq!(explicit.format(), Format::Z);
	assert_eq!(explicit.read("data").unwrap().len(), 180_000);

	let tar_bytes = tar::fixture(&[tar::TarMember::file("inner.txt", b"LZW tar payload\n")]);
	let compressed_tar = literal_compress_z(&tar_bytes);
	let mut tar = Archive::from_bytes_with_format(&compressed_tar, Format::TarZ).unwrap();
	assert_eq!(tar.format(), Format::TarZ);
	assert_eq!(tar.read("inner.txt").unwrap(), b"LZW tar payload\n");
}

fn archive_error(bytes: &[u8], format: Format, limits: Limits) -> Error {
	match Archive::from_bytes_with_format_and_limits(bytes, format, limits) {
		Ok(_) => panic!("malformed stream unexpectedly decoded"),
		Err(error) => error,
	}
}

fn literal_compress_z(bytes: &[u8]) -> Vec<u8> {
	let mut output = vec![0x1f, 0x9d, 0x90];
	let mut payload = Vec::new();
	let mut bit_position = 0_usize;
	let mut group_start = 0_usize;
	let mut width = 9_usize;
	let mut dictionary_head = 257_usize;

	for &byte in bytes {
		write_lsb_code(&mut payload, &mut bit_position, usize::from(byte), width);
		if dictionary_head < 1 << 16 {
			dictionary_head += 1;
			if dictionary_head > 1 << width && width < 16 {
				let group_bits = width * 8;
				bit_position =
					group_start + (bit_position - group_start).div_ceil(group_bits) * group_bits;
				payload.resize(bit_position.div_ceil(8), 0);
				group_start = bit_position;
				width += 1;
			}
		}
	}
	output.extend_from_slice(&payload);
	output
}

fn write_lsb_code(output: &mut Vec<u8>, bit_position: &mut usize, code: usize, width: usize) {
	let end = *bit_position + width;
	output.resize(end.div_ceil(8), 0);
	for bit in 0..width {
		if code & (1 << bit) != 0 {
			let position = *bit_position + bit;
			output[position >> 3] |= 1 << (position & 7);
		}
	}
	*bit_position = end;
}

fn sha256_hex(bytes: &[u8]) -> String {
	let digest = sha256(bytes);
	let mut encoded = String::with_capacity(64);
	for byte in digest {
		write!(&mut encoded, "{byte:02x}").unwrap();
	}
	encoded
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
	let mut state = [
		0x6a09_e667_u32,
		0xbb67_ae85,
		0x3c6e_f372,
		0xa54f_f53a,
		0x510e_527f,
		0x9b05_688c,
		0x1f83_d9ab,
		0x5be0_cd19,
	];
	let mut chunks = bytes.chunks_exact(64);
	for chunk in &mut chunks {
		sha256_block(&mut state, chunk.try_into().unwrap());
	}
	let remainder = chunks.remainder();
	let mut final_blocks = [0_u8; 128];
	final_blocks[..remainder.len()].copy_from_slice(remainder);
	final_blocks[remainder.len()] = 0x80;
	let padded_len = if remainder.len() < 56 { 64 } else { 128 };
	final_blocks[padded_len - 8..padded_len]
		.copy_from_slice(&((bytes.len() as u64) * 8).to_be_bytes());
	for block in final_blocks[..padded_len].as_chunks::<64>().0 {
		sha256_block(&mut state, block);
	}
	let mut digest = [0_u8; 32];
	for (word, bytes) in state.into_iter().zip(digest.chunks_exact_mut(4)) {
		bytes.copy_from_slice(&word.to_be_bytes());
	}
	digest
}

fn sha256_block(state: &mut [u32; 8], block: &[u8; 64]) {
	const K: [u32; 64] = [
		0x428a_2f98,
		0x7137_4491,
		0xb5c0_fbcf,
		0xe9b5_dba5,
		0x3956_c25b,
		0x59f1_11f1,
		0x923f_82a4,
		0xab1c_5ed5,
		0xd807_aa98,
		0x1283_5b01,
		0x2431_85be,
		0x550c_7dc3,
		0x72be_5d74,
		0x80de_b1fe,
		0x9bdc_06a7,
		0xc19b_f174,
		0xe49b_69c1,
		0xefbe_4786,
		0x0fc1_9dc6,
		0x240c_a1cc,
		0x2de9_2c6f,
		0x4a74_84aa,
		0x5cb0_a9dc,
		0x76f9_88da,
		0x983e_5152,
		0xa831_c66d,
		0xb003_27c8,
		0xbf59_7fc7,
		0xc6e0_0bf3,
		0xd5a7_9147,
		0x06ca_6351,
		0x1429_2967,
		0x27b7_0a85,
		0x2e1b_2138,
		0x4d2c_6dfc,
		0x5338_0d13,
		0x650a_7354,
		0x766a_0abb,
		0x81c2_c92e,
		0x9272_2c85,
		0xa2bf_e8a1,
		0xa81a_664b,
		0xc24b_8b70,
		0xc76c_51a3,
		0xd192_e819,
		0xd699_0624,
		0xf40e_3585,
		0x106a_a070,
		0x19a4_c116,
		0x1e37_6c08,
		0x2748_774c,
		0x34b0_bcb5,
		0x391c_0cb3,
		0x4ed8_aa4a,
		0x5b9c_ca4f,
		0x682e_6ff3,
		0x748f_82ee,
		0x78a5_636f,
		0x84c8_7814,
		0x8cc7_0208,
		0x90be_fffa,
		0xa450_6ceb,
		0xbef9_a3f7,
		0xc671_78f2,
	];
	let mut schedule = [0_u32; 64];
	for (index, word) in block.as_chunks::<4>().0.iter().enumerate() {
		schedule[index] = u32::from_be_bytes(*word);
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
	let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
	for index in 0..64 {
		let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
		let choose = (e & f) ^ (!e & g);
		let temporary1 = h
			.wrapping_add(sum1)
			.wrapping_add(choose)
			.wrapping_add(K[index])
			.wrapping_add(schedule[index]);
		let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
		let majority = (a & b) ^ (a & c) ^ (b & c);
		let temporary2 = sum0.wrapping_add(majority);
		h = g;
		g = f;
		f = e;
		e = d.wrapping_add(temporary1);
		d = c;
		c = b;
		b = a;
		a = temporary1.wrapping_add(temporary2);
	}
	for (value, addition) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
		*value = value.wrapping_add(addition);
	}
}
