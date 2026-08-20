//! Bounded ARJ header indexing and member decompression.

use std::io::{Read, Seek, SeekFrom, Write};

use omp_core::Str;

use crate::{
	Entry, Error, Limits, Result,
	entry::Storage,
	lzh,
	path::{normalize, validate},
};

const STORAGE_FORMAT: u8 = 1;
const SIGNATURE: [u8; 2] = [0x60, 0xea];
const MAX_BASIC_HEADER: usize = 2600;

/// Returns whether bytes begin with a CRC-framed ARJ main header.
pub fn is_header(bytes: &[u8]) -> bool {
	if bytes.len() < 34 || !bytes.starts_with(&SIGNATURE) {
		return false;
	}
	let size = usize::from(u16_at(bytes, 2));
	if !(30..=MAX_BASIC_HEADER).contains(&size) || bytes.len() < size + 8 {
		return false;
	}
	let body = &bytes[4..4 + size];
	usize::from(body[0]) >= 30
		&& usize::from(body[0]) <= size
		&& body[6] == 2
		&& crc32fast::hash(body) == u32_at(bytes, 4 + size)
}

/// Indexes ARJ basic and extended headers without materializing payloads.
pub(crate) fn read_entries(
	source: &mut (impl Read + Seek),
	file_size: u64,
	limits: Limits,
	_decoded: &mut Vec<Vec<u8>>,
) -> Result<Vec<Entry>> {
	let main = parse_block(source, 0, file_size, limits)?;
	if main.is_end {
		return Err(Error::InvalidArchive("ARJ archive is missing its main header"));
	}
	let first_header_size = usize::from(main.body[0]);
	if first_header_size < 30 || first_header_size > main.body.len() || main.body[6] != 2 {
		return Err(Error::InvalidArchive("invalid ARJ main header"));
	}
	let main_flags = main.body[4];
	if main_flags & 0x01 != 0 {
		return Err(Error::UnsupportedFeature("encrypted ARJ archives"));
	}
	if main_flags & 0x04 != 0 {
		return Err(Error::UnsupportedFeature("multi-volume ARJ archives"));
	}

	let mut entries = Vec::new();
	let mut offset = main.next_offset;
	let mut metadata_size = main.metadata_size;
	let mut parsed_count = 0_u64;
	loop {
		let block = parse_block(source, offset, file_size, limits)?;
		metadata_size = metadata_size
			.checked_add(block.metadata_size)
			.ok_or(Error::InvalidArchive("ARJ index size overflow"))?;
		check_index_size(metadata_size, limits)?;
		if block.is_end {
			break;
		}
		parsed_count += 1;
		if parsed_count > limits.max_entries() {
			return Err(Error::TooManyEntries { actual: parsed_count, limit: limits.max_entries() });
		}
		let first_header_size = usize::from(block.body[0]);
		if first_header_size < 30 || first_header_size > block.body.len() {
			return Err(Error::InvalidArchive("invalid ARJ local header"));
		}
		let host_os = block.body[3];
		let flags = block.body[4];
		let method = block.body[5];
		let file_type = block.body[6];
		if flags & 0x01 != 0 {
			return Err(Error::UnsupportedFeature("encrypted ARJ members"));
		}
		if flags & 0x0c != 0 {
			return Err(Error::UnsupportedFeature("multi-volume ARJ members"));
		}
		let packed_size = u64::from(u32_at(&block.body, 12));
		let size = u64::from(u32_at(&block.body, 16));
		let checksum = u32_at(&block.body, 20);
		let access_mode = u16_at(&block.body, 26);
		let (filename, next) =
			read_c_string(&block.body, first_header_size, block.body.len(), "filename")?;
		let _ = read_c_string(&block.body, next, block.body.len(), "comment")?;
		check_path_size((next - first_header_size - 1) as u64, limits)?;
		let raw_path = normalize_host_path(filename, host_os);
		check_path_size(raw_path.len() as u64, limits)?;
		let data_start = block.next_offset;
		let next_offset = data_start
			.checked_add(packed_size)
			.ok_or(Error::InvalidArchive("ARJ member range overflow"))?;
		if next_offset > file_size {
			return Err(Error::InvalidArchive("truncated ARJ member data"));
		}
		offset = next_offset;
		let path = normalize(&raw_path, false);
		let display_path = path.clone().unwrap_or_else(|| Str::new("<unsafe>"));
		let actual = size.max(packed_size);
		if actual > limits.max_member_size() {
			return Err(Error::MemberTooLarge {
				path: display_path,
				actual,
				limit: limits.max_member_size(),
			});
		}
		let Some(path) = path else {
			continue;
		};
		validate(&path, limits)?;
		let directory = file_type == 3;
		let mode = if matches!(host_os, 2 | 8) && access_mode != 0 {
			Some(u32::from(if directory { 0x4000 } else { 0x8000 } | (access_mode & 0x0fff)))
		} else {
			None
		};
		let raw_modified = u32_at(&block.body, 8);
		let modified_unix_seconds = if matches!(host_os, 2 | 8) {
			(raw_modified != 0).then_some(u64::from(raw_modified))
		} else {
			dos_time_to_unix(raw_modified)
		};
		entries.push(Entry {
			path,
			directory,
			size: if directory { 0 } else { size },
			modified_unix_seconds,
			mode,
			storage: if directory {
				Storage::Synthetic
			} else {
				Storage::LegacyDos {
					data_offset: data_start,
					stored_size: packed_size,
					format: STORAGE_FORMAT,
					method,
					checksum,
				}
			},
		});
	}
	if parsed_count == 0 {
		return Err(Error::InvalidArchive("ARJ archive has no members"));
	}
	Ok(entries)
}

/// Extracts and verifies one ARJ member.
pub(crate) fn read_entry_to<W: Write>(
	source: &mut (impl Read + Seek),
	entry: &Entry,
	output: &mut W,
) -> Result<u64> {
	let Storage::LegacyDos { data_offset, stored_size, format, method, checksum } = entry.storage
	else {
		return Err(Error::InvalidArchive("invalid ARJ member storage"));
	};
	if format != STORAGE_FORMAT {
		return Err(Error::InvalidArchive("invalid ARJ member storage format"));
	}
	let packed_len = usize::try_from(stored_size)
		.map_err(|_| Error::InvalidArchive("ARJ packed size does not fit this platform"))?;
	let out_len = usize::try_from(entry.size)
		.map_err(|_| Error::InvalidArchive("ARJ member size does not fit this platform"))?;
	let mut packed = vec![0_u8; packed_len];
	source.seek(SeekFrom::Start(data_offset))?;
	source.read_exact(&mut packed)?;
	let bytes = match method {
		0 => {
			if packed_len != out_len {
				return Err(Error::SizeMismatch {
					path:     entry.path.clone(),
					expected: entry.size,
					actual:   stored_size,
				});
			}
			packed
		},
		1..=3 => lzh::decompress_lh_static(&packed, out_len, 26_624, 5, 17)?,
		4 => decompress_method4(&packed, out_len)?,
		8 | 9 => {
			if out_len != 0 || packed_len != 0 {
				return Err(Error::InvalidArchive("ARJ no-data method has non-zero sizes"));
			}
			Vec::new()
		},
		_ => return Err(Error::UnsupportedFeature(arj_method_error(method))),
	};
	if bytes.len() != out_len {
		return Err(Error::SizeMismatch {
			path:     entry.path.clone(),
			expected: entry.size,
			actual:   bytes.len() as u64,
		});
	}
	if method != 8 {
		let actual = crc32fast::hash(&bytes);
		if actual != checksum {
			return Err(Error::ChecksumMismatch {
				path: entry.path.clone(),
				expected: checksum,
				actual,
			});
		}
	}
	output.write_all(&bytes)?;
	Ok(bytes.len() as u64)
}

fn arj_method_error(method: u8) -> &'static str {
	match method {
		5 => "ARJ compression method 5",
		6 => "ARJ compression method 6",
		7 => "ARJ compression method 7",
		10 => "ARJ compression method 10",
		_ => "unknown ARJ compression method",
	}
}

struct Block {
	body:          Vec<u8>,
	next_offset:   u64,
	metadata_size: u64,
	is_end:        bool,
}

fn parse_block(
	source: &mut (impl Read + Seek),
	offset: u64,
	file_size: u64,
	limits: Limits,
) -> Result<Block> {
	let framing = read_array::<4>(source, offset, file_size, "truncated ARJ header signature")?;
	if framing[..2] != SIGNATURE {
		return Err(Error::InvalidArchive("invalid ARJ header signature"));
	}
	let body_size = usize::from(u16_at(&framing, 2));
	if body_size == 0 {
		return Ok(Block {
			body:          Vec::new(),
			next_offset:   offset + 4,
			metadata_size: 4,
			is_end:        true,
		});
	}
	if !(30..=MAX_BASIC_HEADER).contains(&body_size) {
		return Err(Error::InvalidArchive("invalid ARJ basic header size"));
	}
	check_index_size(body_size as u64 + 8, limits)?;
	let body_start = offset + 4;
	let body = read_vec(source, body_start, body_size, file_size, "truncated ARJ basic header")?;
	let expected = read_array::<4>(
		source,
		body_start + body_size as u64,
		file_size,
		"truncated ARJ basic header CRC",
	)?;
	if crc32fast::hash(&body) != u32::from_le_bytes(expected) {
		return Err(Error::InvalidArchive("invalid ARJ basic header CRC-32"));
	}
	let mut cursor = body_start + body_size as u64 + 4;
	let mut extension_count = 0_u32;
	loop {
		let size_bytes =
			read_array::<2>(source, cursor, file_size, "truncated ARJ extended header size")?;
		let extension_size = usize::from(u16::from_le_bytes(size_bytes));
		cursor += 2;
		if extension_size == 0 {
			break;
		}
		extension_count += 1;
		if extension_count > 65_535 {
			return Err(Error::InvalidArchive("too many ARJ extended headers"));
		}
		let metadata_size = cursor
			.checked_add(extension_size as u64 + 4)
			.and_then(|end| end.checked_sub(offset))
			.ok_or(Error::InvalidArchive("ARJ extended header range overflow"))?;
		check_index_size(metadata_size, limits)?;
		let extension =
			read_vec(source, cursor, extension_size, file_size, "truncated ARJ extended header")?;
		let expected = read_array::<4>(
			source,
			cursor + extension_size as u64,
			file_size,
			"truncated ARJ extended header CRC",
		)?;
		if crc32fast::hash(&extension) != u32::from_le_bytes(expected) {
			return Err(Error::InvalidArchive("invalid ARJ extended header CRC-32"));
		}
		cursor += extension_size as u64 + 4;
	}
	Ok(Block { body, next_offset: cursor, metadata_size: cursor - offset, is_end: false })
}

fn decompress_method4(packed: &[u8], out_size: usize) -> Result<Vec<u8>> {
	let mut reader = BitReader::new(packed);
	let mut output = vec![0_u8; out_size];
	let mut output_position = 0_usize;
	while output_position < out_size {
		let mut length_code = 0_usize;
		let mut length_width = 0_u8;
		while length_width < 7 {
			if reader.read(1)? == 0 {
				break;
			}
			length_code += 1_usize << length_width;
			length_width += 1;
		}
		if length_width != 0 {
			length_code += reader.read(length_width)? as usize;
		}
		if length_code == 0 {
			output[output_position] = reader.read(8)? as u8;
			output_position += 1;
			continue;
		}
		let length = length_code + 2;
		if length > out_size - output_position {
			return Err(Error::InvalidArchive("ARJ method-4 match exceeds declared size"));
		}
		let mut position_code = 0_usize;
		let mut position_width = 9_u8;
		while position_width < 13 {
			if reader.read(1)? == 0 {
				break;
			}
			position_code += 1_usize << position_width;
			position_width += 1;
		}
		position_code += reader.read(position_width)? as usize;
		if position_code >= 26_624 || position_code >= output_position {
			return Err(Error::InvalidArchive("ARJ method-4 history distance is out of range"));
		}
		let mut source_position = output_position - position_code - 1;
		for _ in 0..length {
			output[output_position] = output[source_position];
			output_position += 1;
			source_position += 1;
		}
	}
	reader.assert_zero_padding()?;
	Ok(output)
}

struct BitReader<'a> {
	bytes:    &'a [u8],
	position: usize,
}

impl<'a> BitReader<'a> {
	const fn new(bytes: &'a [u8]) -> Self {
		Self { bytes, position: 0 }
	}

	fn read(&mut self, count: u8) -> Result<u32> {
		let count = usize::from(count);
		if count > 24 || self.position + count > self.bytes.len() * 8 {
			return Err(Error::InvalidArchive("truncated ARJ method-4 bitstream"));
		}
		let mut value = 0_u32;
		for _ in 0..count {
			let position = self.position;
			self.position += 1;
			value = (value << 1) | u32::from((self.bytes[position >> 3] >> (7 - (position & 7))) & 1);
		}
		Ok(value)
	}

	fn assert_zero_padding(&mut self) -> Result<()> {
		while self.position < self.bytes.len() * 8 {
			if self.read(1)? != 0 {
				return Err(Error::InvalidArchive("non-zero trailing ARJ method-4 bits"));
			}
		}
		Ok(())
	}
}

fn read_c_string(
	bytes: &[u8],
	start: usize,
	end: usize,
	field: &'static str,
) -> Result<(String, usize)> {
	let Some(relative) = bytes
		.get(start..end)
		.ok_or(Error::InvalidArchive("invalid ARJ string range"))?
		.iter()
		.position(|byte| *byte == 0)
	else {
		return Err(Error::InvalidArchive(match field {
			"filename" => "ARJ filename is missing its terminator",
			_ => "ARJ comment is missing its terminator",
		}));
	};
	let terminator = start + relative;
	Ok((decode_legacy(&bytes[start..terminator]), terminator + 1))
}

fn normalize_host_path(mut path: String, host_os: u8) -> String {
	path = path.replace('\\', "/");
	if host_os == 1 {
		path = path.replace('>', "/");
	} else if host_os == 4 {
		path = path.replace(':', "/");
	}
	path
}

fn decode_legacy(bytes: &[u8]) -> String {
	let mut output = String::with_capacity(bytes.len());
	for &byte in bytes {
		let character = match byte {
			0x80 => '\u{20ac}',
			0x82 => '\u{201a}',
			0x83 => '\u{0192}',
			0x84 => '\u{201e}',
			0x85 => '\u{2026}',
			0x86 => '\u{2020}',
			0x87 => '\u{2021}',
			0x88 => '\u{02c6}',
			0x89 => '\u{2030}',
			0x8a => '\u{0160}',
			0x8b => '\u{2039}',
			0x8c => '\u{0152}',
			0x8e => '\u{017d}',
			0x91 => '\u{2018}',
			0x92 => '\u{2019}',
			0x93 => '\u{201c}',
			0x94 => '\u{201d}',
			0x95 => '\u{2022}',
			0x96 => '\u{2013}',
			0x97 => '\u{2014}',
			0x98 => '\u{02dc}',
			0x99 => '\u{2122}',
			0x9a => '\u{0161}',
			0x9b => '\u{203a}',
			0x9c => '\u{0153}',
			0x9e => '\u{017e}',
			0x9f => '\u{0178}',
			_ => char::from(byte),
		};
		output.push(character);
	}
	output
}

fn dos_time_to_unix(value: u32) -> Option<u64> {
	if value == 0 {
		return None;
	}
	let year = 1980 + ((value >> 25) & 0x7f) as i64;
	let month = ((value >> 21) & 0x0f) as i64;
	let day = ((value >> 16) & 0x1f) as i64;
	let hour = (value >> 11) & 0x1f;
	let minute = (value >> 5) & 0x3f;
	let second = (value & 0x1f) * 2;
	if !(1..=12).contains(&month)
		|| !(1..=31).contains(&day)
		|| hour > 23
		|| minute > 59
		|| second > 59
	{
		return None;
	}
	let days = days_from_civil(year, month, day);
	u64::try_from(days * 86_400 + i64::from(hour * 3600 + minute * 60 + second)).ok()
}

fn days_from_civil(mut year: i64, month: i64, day: i64) -> i64 {
	year -= i64::from(month <= 2);
	let era = year.div_euclid(400);
	let year_of_era = year - era * 400;
	let adjusted_month = month + if month > 2 { -3 } else { 9 };
	let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
	let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
	era * 146_097 + day_of_era - 719_468
}

fn read_array<const N: usize>(
	source: &mut (impl Read + Seek),
	offset: u64,
	file_size: u64,
	message: &'static str,
) -> Result<[u8; N]> {
	if offset
		.checked_add(N as u64)
		.is_none_or(|end| end > file_size)
	{
		return Err(Error::InvalidArchive(message));
	}
	let mut bytes = [0_u8; N];
	source.seek(SeekFrom::Start(offset))?;
	source.read_exact(&mut bytes)?;
	Ok(bytes)
}

fn read_vec(
	source: &mut (impl Read + Seek),
	offset: u64,
	length: usize,
	file_size: u64,
	message: &'static str,
) -> Result<Vec<u8>> {
	if offset
		.checked_add(length as u64)
		.is_none_or(|end| end > file_size)
	{
		return Err(Error::InvalidArchive(message));
	}
	let mut bytes = vec![0_u8; length];
	source.seek(SeekFrom::Start(offset))?;
	source.read_exact(&mut bytes)?;
	Ok(bytes)
}

fn check_index_size(actual: u64, limits: Limits) -> Result<()> {
	if actual > limits.max_index_size() {
		return Err(Error::IndexTooLarge { actual, limit: limits.max_index_size() });
	}
	Ok(())
}

fn check_path_size(actual: u64, limits: Limits) -> Result<()> {
	if actual > limits.max_path_size() {
		return Err(Error::PathTooLong { actual, limit: limits.max_path_size() });
	}
	Ok(())
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
	u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
	u32::from_le_bytes(
		bytes[offset..offset + 4]
			.try_into()
			.expect("validated fixed-width field"),
	)
}
