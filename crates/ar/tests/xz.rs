//! XZ and LZMA-alone integration coverage against reference-packer fixtures.

mod support;

use omp_ar::{Archive, Error, Format, Limits};
use support::fixtures::fixture_bytes;

fn decode(bytes: &[u8], format: Format) -> omp_ar::Result<Vec<u8>> {
	let mut archive = Archive::from_bytes_with_format(bytes, format)?;
	archive.read("data")
}

#[test]
fn decodes_integrity_checks_filters_and_multiblock_streams() {
	let text = fixture_bytes("xz-src/payload.txt");
	for name in ["xz-crc32.xz", "xz-crc64.xz", "xz-sha256.xz", "xz-multiblock.xz"] {
		assert_eq!(decode(&fixture_bytes(name), Format::Xz).unwrap(), text, "{name}");
	}
	assert_eq!(
		decode(&fixture_bytes("xz-delta.xz"), Format::Xz).unwrap(),
		fixture_bytes("sevenzip-src/delta.bin")
	);
	assert_eq!(
		decode(&fixture_bytes("xz-x86.xz"), Format::Xz).unwrap(),
		fixture_bytes("sevenzip-src/x86.bin")
	);
}

#[test]
fn handles_concatenated_streams_and_explicit_tar_xz_dispatch() {
	let stream = fixture_bytes("xz-crc32.xz");
	let expected = fixture_bytes("xz-src/payload.txt");
	let mut concatenated = stream.clone();
	concatenated.extend_from_slice(&stream);
	concatenated.extend_from_slice(&[0; 4]);
	concatenated.extend_from_slice(&stream);
	let mut tripled = expected.clone();
	tripled.extend_from_slice(&expected);
	tripled.extend_from_slice(&expected);
	assert_eq!(decode(&concatenated, Format::Xz).unwrap(), tripled);
	assert_eq!(Format::sniff(&stream), Some(Format::TarXz));
	let mut sniffed = Archive::from_bytes(&stream).unwrap();
	assert_eq!(sniffed.read("data").unwrap(), expected);
	assert_eq!(decode(&stream, Format::TarXz).unwrap(), expected);
}

#[test]
fn decodes_unknown_size_lzma_alone_and_honors_limits() {
	let bytes = fixture_bytes("lzma-alone.lzma");
	assert_eq!(&bytes[5..13], &[0xff; 8]);
	assert_eq!(decode(&bytes, Format::Lzma).unwrap(), fixture_bytes("xz-src/payload.txt"));
	let limits = Limits::DEFAULT.with_max_in_memory_size(10);
	let error = match Archive::from_bytes_with_format_and_limits(&bytes, Format::Lzma, limits) {
		Err(error) => error,
		Ok(_) => panic!("over-limit LZMA stream unexpectedly decoded"),
	};
	assert!(error.to_string().contains("size limit"));
}

#[test]
fn rejects_corruption_truncation_unsupported_filter_and_overflow() {
	let original = fixture_bytes("xz-crc32.xz");
	let mut corrupt = original.clone();
	let footer = corrupt.len() - 12;
	let backward = u32::from_le_bytes(corrupt[footer + 4..footer + 8].try_into().unwrap());
	let index_start = footer - (backward as usize + 1) * 4;
	corrupt[index_start - 1] ^= 1;
	assert!(decode(&corrupt, Format::Xz).is_err());
	assert!(decode(&original[..original.len() - 4], Format::Xz).is_err());

	let limits = Limits::DEFAULT.with_max_in_memory_size(10);
	assert!(matches!(
		Archive::from_bytes_with_format_and_limits(&original, Format::Xz, limits),
		Err(Error::ArchiveTooLargeInMemory { .. })
	));

	let mut unsupported = original.clone();
	let header_size = (usize::from(unsupported[12]) + 1) * 4;
	let header_end = 12 + header_size;
	let filter = (14..header_end - 4)
		.find(|index| unsupported[*index] == 0x21)
		.expect("fixture LZMA2 filter ID");
	unsupported[filter] = 0x22;
	let crc = crc32fast::hash(&unsupported[12..header_end - 4]);
	unsupported[header_end - 4..header_end].copy_from_slice(&crc.to_le_bytes());
	assert!(matches!(
		Archive::from_bytes_with_format(&unsupported, Format::Xz),
		Err(Error::UnsupportedFeature(feature)) if feature.contains("terminal filter")
	));
}
