//! RPM lead/header parsing and compressed cpio payload indexing.

use std::io::{Read, Seek, SeekFrom, Write};

use flate2::read::MultiGzDecoder;

use crate::{Entry, Error, Limits, Result, codec, cpio};

const LEAD_SIZE: u64 = 96;
const HEADER_INTRO_SIZE: u64 = 16;
const INDEX_ENTRY_SIZE: u64 = 16;
const HEADER_MAGIC: u32 = 0x8ead_e801;
const SIGNATURE_TYPE_HEADER: u16 = 5;
const TAG_NAME: u32 = 1000;
const TAG_VERSION: u32 = 1001;
const TAG_PAYLOAD_FORMAT: u32 = 1124;
const TAG_PAYLOAD_COMPRESSOR: u32 = 1125;
const TAG_PAYLOAD_FLAGS: u32 = 1126;
const TYPE_STRING: u32 = 6;

#[derive(Clone, Copy)]
struct HeaderIntro {
	index_count: u32,
	data_size:   u32,
	body_size:   u64,
	total_size:  u64,
}

#[derive(Default)]
struct Metadata {
	name:               Option<String>,
	version:            Option<String>,
	payload_format:     Option<String>,
	payload_compressor: Option<String>,
}

/// Returns whether bytes begin with the RPM lead signature.
pub fn is_header(bytes: &[u8]) -> bool {
	bytes.starts_with(&[0xed, 0xab, 0xee, 0xdb])
}

/// Indexes an RPM's decoded cpio payload in a retained buffer.
pub(crate) fn read_entries(
	source: &mut (impl Read + Seek),
	file_size: u64,
	limits: Limits,
	decoded: &mut Vec<Vec<u8>>,
) -> Result<Vec<Entry>> {
	if file_size < LEAD_SIZE + HEADER_INTRO_SIZE {
		return Err(Error::InvalidArchive("truncated RPM lead and signature header"));
	}
	let initial = read_vec_at(
		source,
		0,
		(LEAD_SIZE + HEADER_INTRO_SIZE) as usize,
		file_size,
		"truncated RPM lead and signature header",
	)?;
	if !is_header(&initial) {
		return Err(Error::InvalidArchive("bad RPM lead magic"));
	}
	let major = initial[4];
	let package_type = u16::from_be_bytes([initial[6], initial[7]]);
	if major < 3 || package_type > 1 {
		return Err(Error::UnsupportedFeature("RPM lead version or package type"));
	}
	let signature_type = u16::from_be_bytes([initial[78], initial[79]]);
	if signature_type != SIGNATURE_TYPE_HEADER {
		return Err(Error::UnsupportedFeature("RPM non-header signature type"));
	}
	let signature_intro = parse_intro(&initial[LEAD_SIZE as usize..], limits)?;
	let signature_end = LEAD_SIZE
		.checked_add(signature_intro.total_size)
		.ok_or(Error::InvalidArchive("RPM signature header range overflows"))?;
	let main_offset = align(signature_end, 8)?;
	let signature_tail_size = main_offset
		.checked_sub(LEAD_SIZE + HEADER_INTRO_SIZE)
		.ok_or(Error::InvalidArchive("invalid RPM signature header size"))?;
	let signature_tail = read_vec_at(
		source,
		LEAD_SIZE + HEADER_INTRO_SIZE,
		usize::try_from(signature_tail_size)
			.map_err(|_| Error::InvalidArchive("RPM signature header does not fit this platform"))?,
		file_size,
		"truncated RPM signature header",
	)?;
	let body_size = usize::try_from(signature_intro.body_size)
		.map_err(|_| Error::InvalidArchive("RPM signature header does not fit this platform"))?;
	if signature_tail.len() < body_size {
		return Err(Error::InvalidArchive("truncated RPM signature header"));
	}
	validate_header_body(&signature_tail[..body_size], signature_intro)?;
	if signature_tail[body_size..].iter().any(|&byte| byte != 0) {
		return Err(Error::InvalidArchive("non-zero RPM signature alignment padding"));
	}

	let main_intro_bytes = read_vec_at(
		source,
		main_offset,
		HEADER_INTRO_SIZE as usize,
		file_size,
		"truncated RPM main header intro",
	)?;
	let main_intro = parse_intro(&main_intro_bytes, limits)?;
	let combined_index = signature_intro
		.total_size
		.checked_add(main_intro.total_size)
		.ok_or(Error::InvalidArchive("RPM header size overflows"))?;
	if combined_index > limits.index_size {
		return Err(Error::IndexTooLarge { actual: combined_index, limit: limits.index_size });
	}
	let main_body_offset = main_offset + HEADER_INTRO_SIZE;
	let main_body = read_vec_at(
		source,
		main_body_offset,
		usize::try_from(main_intro.body_size)
			.map_err(|_| Error::InvalidArchive("RPM main header does not fit this platform"))?,
		file_size,
		"truncated RPM main header",
	)?;
	let metadata = parse_main_header(&main_body, main_intro)?;
	if metadata
		.payload_format
		.as_deref()
		.is_some_and(|format| !format.eq_ignore_ascii_case("cpio"))
	{
		return Err(Error::UnsupportedFeature("RPM non-cpio payload format"));
	}

	let payload_offset = main_offset
		.checked_add(main_intro.total_size)
		.ok_or(Error::InvalidArchive("RPM payload offset overflows"))?;
	if payload_offset > file_size {
		return Err(Error::InvalidArchive("truncated RPM payload"));
	}
	let payload_size = file_size - payload_offset;
	if payload_size > limits.in_memory_size {
		return Err(Error::ArchiveTooLargeInMemory {
			actual: payload_size,
			limit:  limits.in_memory_size,
		});
	}
	let payload = read_vec_at(
		source,
		payload_offset,
		usize::try_from(payload_size)
			.map_err(|_| Error::InvalidArchive("RPM payload does not fit this platform"))?,
		file_size,
		"truncated RPM payload",
	)?;
	let cpio_bytes = decompress_payload(&payload, &metadata, limits)?;
	if cpio_bytes.len() as u64 > limits.in_memory_size {
		return Err(Error::ArchiveTooLargeInMemory {
			actual: cpio_bytes.len() as u64,
			limit:  limits.in_memory_size,
		});
	}
	let buffer = u32::try_from(decoded.len())
		.map_err(|_| Error::InvalidArchive("too many retained archive buffers"))?;
	let entries = cpio::read_entries_from_buffer(&cpio_bytes, limits, buffer)?;
	decoded.push(cpio_bytes);
	Ok(entries)
}

fn parse_intro(bytes: &[u8], limits: Limits) -> Result<HeaderIntro> {
	if bytes.len() != HEADER_INTRO_SIZE as usize || read_u32(bytes, 0)? != HEADER_MAGIC {
		return Err(Error::InvalidArchive("corrupt RPM header magic"));
	}
	if bytes[4..8].iter().any(|&byte| byte != 0) {
		return Err(Error::InvalidArchive("corrupt RPM header reserved bytes"));
	}
	let index_count = read_u32(bytes, 8)?;
	let data_size = read_u32(bytes, 12)?;
	if u64::from(index_count) > limits.entries {
		return Err(Error::TooManyEntries { actual: u64::from(index_count), limit: limits.entries });
	}
	let index_size = u64::from(index_count)
		.checked_mul(INDEX_ENTRY_SIZE)
		.ok_or(Error::InvalidArchive("RPM header index size overflows"))?;
	let body_size = index_size
		.checked_add(u64::from(data_size))
		.ok_or(Error::InvalidArchive("RPM header size overflows"))?;
	let total_size = HEADER_INTRO_SIZE + body_size;
	if total_size > limits.index_size {
		return Err(Error::IndexTooLarge { actual: total_size, limit: limits.index_size });
	}
	Ok(HeaderIntro { index_count, data_size, body_size, total_size })
}

fn validate_header_body(body: &[u8], intro: HeaderIntro) -> Result<()> {
	if body.len() as u64 != intro.body_size {
		return Err(Error::InvalidArchive("truncated RPM header"));
	}
	let index_size = u64::from(intro.index_count) * INDEX_ENTRY_SIZE;
	for index in 0..intro.index_count {
		let record = usize::try_from(u64::from(index) * INDEX_ENTRY_SIZE)
			.expect("RPM header index is already platform-sized");
		let tag = read_u32(body, record)?;
		let data_type = read_u32(body, record + 4)?;
		let offset = read_u32(body, record + 8)?;
		let count = read_u32(body, record + 12)?;
		if offset > intro.data_size {
			return Err(Error::InvalidArchive("RPM tag points outside header data"));
		}
		let remaining = u64::from(intro.data_size - offset);
		let element_size = match data_type {
			0 => {
				if count != 0 {
					return Err(Error::InvalidArchive("RPM null tag contains values"));
				}
				continue;
			},
			1 | 2 | 7 => 1_u64,
			3 => 2,
			4 => 4,
			5 => 8,
			TYPE_STRING | 8 | 9 => {
				let string_count = if data_type == TYPE_STRING {
					if count != 1 {
						return Err(Error::InvalidArchive("RPM string tag has an invalid count"));
					}
					1
				} else {
					count
				};
				if u64::from(string_count) > remaining {
					return Err(Error::InvalidArchive("RPM string tag exceeds header data"));
				}
				let mut cursor = usize::try_from(index_size + u64::from(offset)).map_err(|_| {
					Error::InvalidArchive("RPM string offset does not fit this platform")
				})?;
				let limit = usize::try_from(index_size + u64::from(intro.data_size))
					.map_err(|_| Error::InvalidArchive("RPM header data does not fit this platform"))?;
				for _ in 0..string_count {
					while cursor < limit && body[cursor] != 0 {
						cursor += 1;
					}
					if cursor == limit {
						return Err(Error::InvalidArchive("RPM string tag is not NUL-terminated"));
					}
					cursor += 1;
				}
				continue;
			},
			_ => return Err(Error::InvalidArchive("RPM tag uses an unknown data type")),
		};
		if u64::from(offset) % element_size != 0 {
			return Err(Error::InvalidArchive("RPM tag data is misaligned"));
		}
		if u64::from(count)
			.checked_mul(element_size)
			.is_none_or(|size| size > remaining)
		{
			return Err(Error::InvalidArchive("RPM tag exceeds header data"));
		}
		let _ = tag;
	}
	Ok(())
}

fn parse_main_header(body: &[u8], intro: HeaderIntro) -> Result<Metadata> {
	validate_header_body(body, intro)?;
	let index_size = u64::from(intro.index_count) * INDEX_ENTRY_SIZE;
	let mut metadata = Metadata::default();
	for index in 0..intro.index_count {
		let record = usize::try_from(u64::from(index) * INDEX_ENTRY_SIZE)
			.expect("RPM header index is already platform-sized");
		let tag = read_u32(body, record)?;
		if !matches!(
			tag,
			TAG_NAME | TAG_VERSION | TAG_PAYLOAD_FORMAT | TAG_PAYLOAD_COMPRESSOR | TAG_PAYLOAD_FLAGS
		) {
			continue;
		}
		let data_type = read_u32(body, record + 4)?;
		let offset = read_u32(body, record + 8)?;
		let count = read_u32(body, record + 12)?;
		if data_type != TYPE_STRING || count != 1 {
			return Err(Error::InvalidArchive("RPM metadata tag must contain one string"));
		}
		let start = usize::try_from(index_size + u64::from(offset))
			.map_err(|_| Error::InvalidArchive("RPM metadata offset does not fit this platform"))?;
		let limit = usize::try_from(index_size + u64::from(intro.data_size))
			.map_err(|_| Error::InvalidArchive("RPM metadata does not fit this platform"))?;
		let relative_end = body[start..limit]
			.iter()
			.position(|&byte| byte == 0)
			.ok_or(Error::InvalidArchive("RPM metadata string is not NUL-terminated"))?;
		if relative_end > 4096 {
			return Err(Error::InvalidArchive("RPM metadata string is too large"));
		}
		let value = std::str::from_utf8(&body[start..start + relative_end])
			.map_err(|_| Error::InvalidArchive("RPM metadata string is not valid UTF-8"))?
			.to_owned();
		match tag {
			TAG_NAME => metadata.name = Some(value),
			TAG_VERSION => metadata.version = Some(value),
			TAG_PAYLOAD_FORMAT => metadata.payload_format = Some(value),
			TAG_PAYLOAD_COMPRESSOR => metadata.payload_compressor = Some(value),
			TAG_PAYLOAD_FLAGS => {},
			_ => unreachable!(),
		}
	}
	Ok(metadata)
}

fn decompress_payload(payload: &[u8], metadata: &Metadata, limits: Limits) -> Result<Vec<u8>> {
	let declared = metadata.payload_compressor.as_deref().map(str::trim);
	let recognized = declared.and_then(|method| {
		let lower = method.to_ascii_lowercase();
		matches!(
			lower.as_str(),
			"gzip" | "gz" | "bzip2" | "bzip" | "xz" | "lzma" | "zstd" | "zstdio" | "none"
		)
		.then_some(lower)
	});
	let method = recognized.unwrap_or_else(|| {
		if codec::is_gzip(payload) {
			"gzip".to_owned()
		} else if codec::is_bzip2(payload) {
			"bzip2".to_owned()
		} else if codec::is_xz(payload) {
			"xz".to_owned()
		} else if codec::is_zstd(payload) {
			"zstd".to_owned()
		} else if sniff_lzma_alone(payload) {
			"lzma".to_owned()
		} else if cpio::is_header(payload) {
			"none".to_owned()
		} else {
			"unsupported".to_owned()
		}
	});
	let codec_limits = limits.with_max_archive_size(limits.in_memory_size);
	match method.as_str() {
		"gzip" | "gz" => gzip_decompress(payload, limits.in_memory_size),
		"bzip2" | "bzip" => codec::bzip2_decompress(payload, codec_limits),
		"xz" => codec::xz_decompress(payload, codec_limits),
		"zstd" | "zstdio" => codec::zstd_decompress(payload, codec_limits),
		"lzma" => {
			if !sniff_lzma_alone(payload) {
				return Err(Error::InvalidArchive("malformed RPM LZMA payload"));
			}
			codec::lzma_alone_decompress(payload, codec_limits)
		},
		"none" => {
			if !cpio::is_header(payload) {
				return Err(Error::InvalidArchive("invalid uncompressed RPM CPIO payload"));
			}
			Ok(payload.to_vec())
		},
		_ => Err(Error::UnsupportedFeature("RPM payload compressor")),
	}
}

fn gzip_decompress(bytes: &[u8], max_output: u64) -> Result<Vec<u8>> {
	let mut output = Vec::new();
	MultiGzDecoder::new(bytes)
		.take(max_output.saturating_add(1))
		.read_to_end(&mut output)?;
	if output.len() as u64 > max_output {
		return Err(Error::ArchiveTooLargeInMemory {
			actual: output.len() as u64,
			limit:  max_output,
		});
	}
	Ok(output)
}

fn sniff_lzma_alone(bytes: &[u8]) -> bool {
	if bytes.len() < 13 || bytes[0] > 224 {
		return false;
	}
	let dictionary = u32::from_le_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
	if dictionary < 4096 {
		return false;
	}
	let rounded = dictionary.checked_next_power_of_two().unwrap_or(0);
	dictionary == rounded || dictionary == rounded.saturating_sub(rounded / 4)
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
	let slice = bytes
		.get(offset..offset + 4)
		.ok_or(Error::InvalidArchive("truncated RPM header field"))?;
	Ok(u32::from_be_bytes(slice.try_into().expect("four-byte RPM field")))
}

fn align(value: u64, alignment: u64) -> Result<u64> {
	let remainder = value % alignment;
	if remainder == 0 {
		Ok(value)
	} else {
		value
			.checked_add(alignment - remainder)
			.ok_or(Error::InvalidArchive("RPM alignment offset overflows"))
	}
}

fn read_vec_at(
	source: &mut (impl Read + Seek),
	offset: u64,
	length: usize,
	file_size: u64,
	message: &'static str,
) -> Result<Vec<u8>> {
	let end = offset
		.checked_add(length as u64)
		.ok_or(Error::InvalidArchive(message))?;
	if end > file_size {
		return Err(Error::InvalidArchive(message));
	}
	let mut bytes = vec![0_u8; length];
	source.seek(SeekFrom::Start(offset))?;
	source.read_exact(&mut bytes).map_err(|error| {
		if error.kind() == std::io::ErrorKind::UnexpectedEof {
			Error::InvalidArchive(message)
		} else {
			error.into()
		}
	})?;
	Ok(bytes)
}

/// `Buffered` storage is served by the archive core.
pub(crate) const fn read_entry_to<W: Write>(
	_source: &mut (impl Read + Seek),
	_entry: &Entry,
	_output: &mut W,
) -> Result<u64> {
	Err(Error::InvalidArchive("entry is not an RPM member"))
}
