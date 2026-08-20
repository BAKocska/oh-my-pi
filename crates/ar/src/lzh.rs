//! Bounded LZH/LHA header indexing and member decompression.

use std::io::{Read, Seek, SeekFrom, Write};

use omp_core::Str;
use xutf::{TextBuf as _, Utf8, Utf16Le};

use crate::{
	Entry, Error, Limits, Result,
	entry::Storage,
	path::{normalize, validate},
};

pub(crate) const STORAGE_FORMAT: u8 = 0;

const METHOD_LH0: u8 = 0;
const METHOD_LH1: u8 = 1;
const METHOD_LH2: u8 = 2;
const METHOD_LH3: u8 = 3;
const METHOD_LH4: u8 = 4;
const METHOD_LH5: u8 = 5;
const METHOD_LH6: u8 = 6;
const METHOD_LH7: u8 = 7;
const METHOD_LHD: u8 = 8;
const METHOD_LZ4: u8 = 9;
const METHOD_LZ5: u8 = 10;
const METHOD_LZS: u8 = 11;

/// Returns whether bytes begin with a supported LZH header framing.
pub fn is_header(bytes: &[u8]) -> bool {
	bytes.len() >= 22 && method_code(&bytes[2..7]).is_some() && bytes[20] <= 2
}

/// Indexes LZH headers without materializing member payloads.
pub(crate) fn read_entries(
	source: &mut (impl Read + Seek),
	file_size: u64,
	limits: Limits,
	_decoded: &mut Vec<Vec<u8>>,
) -> Result<Vec<Entry>> {
	let mut entries = Vec::new();
	let mut offset = 0_u64;
	let mut parsed_count = 0_u64;
	let mut metadata_size = 0_u64;
	while offset < file_size {
		let marker = read_array::<1>(source, offset, file_size, "truncated LZH header")?[0];
		if marker == 0 {
			break;
		}
		let header = parse_header(source, offset, file_size, limits)?;
		metadata_size = metadata_size
			.checked_add(header.data_start - offset)
			.ok_or(Error::InvalidArchive("LZH index size overflow"))?;
		check_index_size(metadata_size, limits)?;
		parsed_count += 1;
		if parsed_count > limits.max_entries() {
			return Err(Error::TooManyEntries { actual: parsed_count, limit: limits.max_entries() });
		}
		if header.next_offset <= offset {
			return Err(Error::InvalidArchive("LZH header did not advance"));
		}
		offset = header.next_offset;
		let Some(path) = header.path else {
			continue;
		};
		let modified_unix_seconds = header.modified_unix_seconds;
		if header.method == METHOD_LHD {
			if header.mode.is_some_and(|mode| mode & 0xf000 == 0xa000) {
				let Some((link, target)) = path.split_once('|') else {
					return Err(Error::InvalidArchive("invalid LZH symbolic link"));
				};
				if link.is_empty() {
					return Err(Error::InvalidArchive("invalid LZH symbolic link"));
				}
				let (Some(link), Some(target)) = (normalize(link, false), normalize(target, false))
				else {
					continue;
				};
				validate(&link, limits)?;
				validate(&target, limits)?;
				entries.push(Entry {
					path: link,
					directory: false,
					size: 0,
					modified_unix_seconds,
					mode: header.mode,
					storage: Storage::Link { target_path: target, resolve_target: true },
				});
			} else {
				entries.push(Entry {
					path,
					directory: true,
					size: 0,
					modified_unix_seconds,
					mode: header.mode,
					storage: Storage::Synthetic,
				});
			}
			continue;
		}
		entries.push(Entry {
			path,
			directory: false,
			size: header.size,
			modified_unix_seconds,
			mode: header.mode,
			storage: Storage::LegacyDos {
				data_offset: header.data_start,
				stored_size: header.packed_size,
				format:      STORAGE_FORMAT,
				method:      header.method,
				checksum:    u32::from(header.crc),
			},
		});
	}
	if parsed_count == 0 {
		return Err(Error::InvalidArchive("LZH archive has no members"));
	}
	Ok(entries)
}

/// Extracts and verifies one LZH member.
pub(crate) fn read_entry_to<W: Write>(
	source: &mut (impl Read + Seek),
	entry: &Entry,
	output: &mut W,
) -> Result<u64> {
	let Storage::LegacyDos { data_offset, stored_size, format, method, checksum } = entry.storage
	else {
		return Err(Error::InvalidArchive("invalid LZH member storage"));
	};
	if format != STORAGE_FORMAT {
		return Err(Error::InvalidArchive("invalid LZH member storage format"));
	}
	let packed_len = usize::try_from(stored_size)
		.map_err(|_| Error::InvalidArchive("LZH packed size does not fit this platform"))?;
	let out_len = usize::try_from(entry.size)
		.map_err(|_| Error::InvalidArchive("LZH member size does not fit this platform"))?;
	let mut packed = vec![0_u8; packed_len];
	source.seek(SeekFrom::Start(data_offset))?;
	source.read_exact(&mut packed)?;
	let bytes = match method {
		METHOD_LH0 | METHOD_LZ4 => {
			if packed_len != out_len {
				return Err(Error::SizeMismatch {
					path:     entry.path.clone(),
					expected: entry.size,
					actual:   stored_size,
				});
			}
			packed
		},
		METHOD_LH4 => decompress_lh_static(&packed, out_len, 1 << 12, 4, 13)?,
		METHOD_LH5 => decompress_lh_static(&packed, out_len, 1 << 13, 4, 14)?,
		METHOD_LH6 => decompress_lh_static(&packed, out_len, 1 << 15, 5, 16)?,
		METHOD_LH7 => decompress_lh_static(&packed, out_len, 1 << 16, 5, 17)?,
		METHOD_LZS => decompress_lzs(&packed, out_len)?,
		METHOD_LH1 => return Err(Error::UnsupportedFeature("LZH dynamic-Huffman method -lh1-")),
		METHOD_LH2 => return Err(Error::UnsupportedFeature("LZH dynamic-Huffman method -lh2-")),
		METHOD_LH3 => return Err(Error::UnsupportedFeature("LZH static-Huffman method -lh3-")),
		METHOD_LZ5 => return Err(Error::UnsupportedFeature("LZH LArc method -lz5-")),
		_ => return Err(Error::UnsupportedFeature("unknown LZH compression method")),
	};
	if bytes.len() != out_len {
		return Err(Error::SizeMismatch {
			path:     entry.path.clone(),
			expected: entry.size,
			actual:   bytes.len() as u64,
		});
	}
	let actual = u32::from(crc16_arc(&bytes));
	if actual != checksum {
		return Err(Error::ChecksumMismatch { path: entry.path.clone(), expected: checksum, actual });
	}
	output.write_all(&bytes)?;
	Ok(bytes.len() as u64)
}

struct ParsedHeader {
	method:                u8,
	packed_size:           u64,
	size:                  u64,
	data_start:            u64,
	next_offset:           u64,
	crc:                   u16,
	path:                  Option<Str>,
	modified_unix_seconds: Option<u64>,
	mode:                  Option<u32>,
}

#[derive(Default)]
struct ExtendedFields {
	filename:          Option<String>,
	directory:         Option<String>,
	unicode_filename:  Option<String>,
	unicode_directory: Option<String>,
	modified:          Option<u64>,
	mode:              Option<u32>,
	packed_size:       Option<u64>,
	size:              Option<u64>,
	common_crc:        Option<u16>,
	common_crc_offset: Option<u64>,
}

fn parse_header(
	source: &mut (impl Read + Seek),
	offset: u64,
	file_size: u64,
	limits: Limits,
) -> Result<ParsedHeader> {
	let prefix = read_array::<22>(source, offset, file_size, "truncated LZH header")?;
	let level = prefix[20];
	if level > 2 {
		return Err(Error::UnsupportedFeature("LZH header level 3"));
	}
	let method = method_code(&prefix[2..7]).ok_or(Error::InvalidArchive("invalid LZH method"))?;
	let mut packed_size = u64::from(u32_at(&prefix, 7));
	let mut size = u64::from(u32_at(&prefix, 11));
	let crc;
	let mut legacy_filename = None;
	let base_modified;
	let mut os_id = 0_u8;
	let mut fields = ExtendedFields::default();
	let mut header_bytes;
	let data_start;

	if level < 2 {
		let header_length = usize::from(prefix[0]);
		let minimum = if level == 0 { 22 } else { 25 };
		if header_length < minimum {
			return Err(Error::InvalidArchive("invalid LZH level-0/1 header size"));
		}
		let base_size = header_length
			.checked_add(2)
			.ok_or(Error::InvalidArchive("LZH header size overflow"))?;
		check_index_size(base_size as u64, limits)?;
		header_bytes = read_vec(source, offset, base_size, file_size, "truncated LZH header")?;
		let sum = header_bytes[2..]
			.iter()
			.fold(0_u8, |sum, byte| sum.wrapping_add(*byte));
		if sum != header_bytes[1] {
			return Err(Error::InvalidArchive("invalid LZH additive header checksum"));
		}
		let name_length = usize::from(header_bytes[21]);
		if 22_usize
			.checked_add(name_length)
			.and_then(|end| end.checked_add(2))
			.is_none_or(|end| end > base_size)
		{
			return Err(Error::InvalidArchive("invalid LZH filename length"));
		}
		if level == 1
			&& 25_usize
				.checked_add(name_length)
				.is_none_or(|end| end > base_size)
		{
			return Err(Error::InvalidArchive("invalid LZH level-1 filename length"));
		}
		check_path_size(name_length as u64, limits)?;
		legacy_filename = Some(decode_legacy(&header_bytes[22..22 + name_length]));
		crc = u16_at(&header_bytes, 22 + name_length);
		base_modified = dos_time_to_unix(u32_at(&header_bytes, 15));
		if level == 0 {
			let extension_start = 24 + name_length;
			if base_size.saturating_sub(extension_start) >= 12 {
				let extension = &header_bytes[extension_start..];
				if matches!(extension[0], 0x55 | 0x4b) && extension[1] == 0 {
					os_id = extension[0];
					fields.modified = Some(u64::from(u32_at(extension, 2)));
					fields.mode = Some(u32::from(u16_at(extension, extension.len() - 6)));
				}
			}
			data_start = offset + base_size as u64;
		} else {
			os_id = header_bytes[24 + name_length];
			let mut extension_size = usize::from(u16_at(&header_bytes, base_size - 2));
			let mut cursor = offset + base_size as u64;
			let mut total_extension_size = 0_u64;
			let mut extension_count = 0_u32;
			while extension_size != 0 {
				if extension_size < 3 {
					return Err(Error::InvalidArchive("invalid LZH extended header size"));
				}
				total_extension_size = total_extension_size
					.checked_add(extension_size as u64)
					.ok_or(Error::InvalidArchive("LZH extended header size overflow"))?;
				check_index_size(base_size as u64 + total_extension_size, limits)?;
				extension_count += 1;
				if extension_count > 65_535 {
					return Err(Error::InvalidArchive("too many LZH extended headers"));
				}
				let extension = read_vec(
					source,
					cursor,
					extension_size,
					file_size,
					"truncated LZH extended header",
				)?;
				let data_end = extension_size - 2;
				process_extended(
					extension[0],
					&extension[1..data_end],
					cursor + 1,
					&mut fields,
					limits,
				)?;
				extension_size = usize::from(u16_at(&extension, data_end));
				header_bytes.extend_from_slice(&extension);
				cursor += extension.len() as u64;
			}
			data_start = cursor;
			packed_size = match fields.packed_size {
				Some(value) => value,
				None => packed_size
					.checked_sub(total_extension_size)
					.ok_or(Error::InvalidArchive("invalid LZH level-1 packed size"))?,
			};
		}
	} else {
		let header_length = usize::from(u16_at(&prefix, 0));
		if header_length < 26 {
			return Err(Error::InvalidArchive("invalid LZH level-2 header size"));
		}
		check_index_size(header_length as u64, limits)?;
		header_bytes = read_vec(source, offset, header_length, file_size, "truncated LZH header")?;
		crc = u16_at(&header_bytes, 21);
		os_id = header_bytes[23];
		base_modified = Some(u64::from(u32_at(&header_bytes, 15)));
		let mut cursor = 24_usize;
		let mut extension_count = 0_u32;
		while cursor + 2 <= header_length {
			let extension_size = usize::from(u16_at(&header_bytes, cursor));
			if extension_size == 0 {
				cursor += 2;
				break;
			}
			if extension_size < 3 || cursor + extension_size > header_length {
				return Err(Error::InvalidArchive("invalid LZH extended header size"));
			}
			extension_count += 1;
			if extension_count > 65_535 {
				return Err(Error::InvalidArchive("too many LZH extended headers"));
			}
			process_extended(
				header_bytes[cursor + 2],
				&header_bytes[cursor + 3..cursor + extension_size],
				offset + cursor as u64 + 3,
				&mut fields,
				limits,
			)?;
			cursor += extension_size;
		}
		if cursor != header_length {
			return Err(Error::InvalidArchive("invalid LZH level-2 extended header chain"));
		}
		data_start = offset + header_length as u64;
		packed_size = fields.packed_size.unwrap_or(packed_size);
	}
	if level == 1 && os_id == 0x20 && method == METHOD_LH7 {
		return Err(Error::UnsupportedFeature("incompatible LHARK -lh7- variant"));
	}
	size = fields.size.unwrap_or(size);
	let modified = fields.modified.or(base_modified);
	if let (Some(expected), Some(common_offset)) = (fields.common_crc, fields.common_crc_offset) {
		let relative = usize::try_from(common_offset - offset)
			.map_err(|_| Error::InvalidArchive("LZH common CRC offset overflow"))?;
		let end = relative
			.checked_add(2)
			.ok_or(Error::InvalidArchive("LZH common CRC range overflow"))?;
		if end > header_bytes.len() || crc16_with_zero_range(&header_bytes, relative, end) != expected
		{
			return Err(Error::InvalidArchive("invalid LZH common header CRC-16"));
		}
	}
	let filename = fields
		.unicode_filename
		.or(fields.filename)
		.or(legacy_filename)
		.unwrap_or_default();
	let directory = fields
		.unicode_directory
		.or(fields.directory)
		.unwrap_or_default();
	let mut raw_path = directory;
	if !raw_path.is_empty() && !raw_path.ends_with(['/', '\\']) {
		raw_path.push('/');
	}
	raw_path.push_str(&filename);
	check_path_size(raw_path.len() as u64, limits)?;
	let path = normalize(&raw_path, false);
	if let Some(path) = &path {
		validate(path, limits)?;
	}
	let display_path = path.clone().unwrap_or_else(|| Str::new("<unsafe>"));
	let actual = size.max(packed_size);
	if actual > limits.max_member_size() {
		return Err(Error::MemberTooLarge {
			path: display_path,
			actual,
			limit: limits.max_member_size(),
		});
	}
	let next_offset = data_start
		.checked_add(packed_size)
		.ok_or(Error::InvalidArchive("LZH member range overflow"))?;
	if next_offset > file_size {
		return Err(Error::InvalidArchive("truncated LZH member data"));
	}
	Ok(ParsedHeader {
		method,
		packed_size,
		size,
		data_start,
		next_offset,
		crc,
		path,
		modified_unix_seconds: modified,
		mode: fields.mode,
	})
}

fn process_extended(
	kind: u8,
	data: &[u8],
	absolute_data_offset: u64,
	fields: &mut ExtendedFields,
	limits: Limits,
) -> Result<()> {
	if matches!(kind, 0x01 | 0x02 | 0x44 | 0x45) {
		check_path_size(data.len() as u64, limits)?;
	}
	match kind {
		0x00 => {
			if data.len() < 2 {
				return Err(Error::InvalidArchive("invalid LZH common extended header"));
			}
			fields.common_crc = Some(u16_at(data, 0));
			fields.common_crc_offset = Some(absolute_data_offset);
		},
		0x01 => fields.filename = Some(decode_legacy(data)),
		0x02 => fields.directory = Some(decode_legacy(data).replace('\u{ff}', "/")),
		0x39 => return Err(Error::UnsupportedFeature("multi-volume LZH archives")),
		0x41 if data.len() >= 16 => {
			let ticks = u64::from(u32_at(data, 8)) | (u64::from(u32_at(data, 12)) << 32);
			fields.modified = filetime_to_unix(ticks);
		},
		0x42 => {
			if data.len() < 16 {
				return Err(Error::InvalidArchive("invalid LZH 64-bit size extended header"));
			}
			fields.packed_size = Some(u64_at(data, 0));
			fields.size = Some(u64_at(data, 8));
		},
		0x44 => fields.unicode_filename = Some(decode_utf16(data)?),
		0x45 => fields.unicode_directory = Some(decode_utf16(data)?.replace('\u{ff}', "/")),
		0x50 => {
			if data.len() < 2 {
				return Err(Error::InvalidArchive("invalid LZH Unix permissions header"));
			}
			fields.mode = Some(u32::from(u16_at(data, 0)));
		},
		0x54 => {
			if data.len() < 4 {
				return Err(Error::InvalidArchive("invalid LZH Unix timestamp header"));
			}
			fields.modified = Some(u64::from(u32_at(data, 0)));
		},
		_ => {},
	}
	Ok(())
}

const fn method_code(bytes: &[u8]) -> Option<u8> {
	match bytes {
		b"-lh0-" => Some(METHOD_LH0),
		b"-lh1-" => Some(METHOD_LH1),
		b"-lh2-" => Some(METHOD_LH2),
		b"-lh3-" => Some(METHOD_LH3),
		b"-lh4-" => Some(METHOD_LH4),
		b"-lh5-" => Some(METHOD_LH5),
		b"-lh6-" => Some(METHOD_LH6),
		b"-lh7-" => Some(METHOD_LH7),
		b"-lhd-" => Some(METHOD_LHD),
		b"-lz4-" => Some(METHOD_LZ4),
		b"-lz5-" => Some(METHOD_LZ5),
		b"-lzs-" => Some(METHOD_LZS),
		_ => None,
	}
}

pub(crate) fn decompress_lh_static(
	packed: &[u8],
	out_size: usize,
	dictionary_size: usize,
	position_bits: u8,
	position_symbols: usize,
) -> Result<Vec<u8>> {
	let mut reader = MsbBitReader::new(packed);
	let mut output = vec![0_u8; out_size];
	let mut output_position = 0_usize;
	let mut block_remaining = 0_u32;
	let mut commands = Huffman::single(0, 510)?;
	let mut positions = Huffman::single(0, position_symbols)?;
	while output_position < out_size {
		if block_remaining == 0 {
			block_remaining = reader.read(16)?;
			if block_remaining == 0 {
				return Err(Error::InvalidArchive("empty legacy Huffman block"));
			}
			let temporary = read_temporary_tree(&mut reader)?;
			commands = read_command_tree(&mut reader, &temporary)?;
			positions = read_position_tree(&mut reader, position_bits, position_symbols)?;
		}
		block_remaining -= 1;
		let symbol = usize::from(commands.decode(&mut reader)?);
		if symbol < 256 {
			output[output_position] = symbol as u8;
			output_position += 1;
			continue;
		}
		let length = symbol - 256 + 3;
		if length > out_size - output_position {
			return Err(Error::InvalidArchive("legacy Huffman match exceeds declared size"));
		}
		let position_code = usize::from(positions.decode(&mut reader)?);
		let distance = if position_code > 1 {
			let low_bits = position_code - 1;
			(1_usize << low_bits) + reader.read(low_bits as u8)? as usize
		} else {
			position_code
		};
		if distance >= dictionary_size || distance >= output_position {
			return Err(Error::InvalidArchive("legacy Huffman history distance is out of range"));
		}
		let source_start = output_position - distance - 1;
		for source_position in source_start..source_start + length {
			output[output_position] = output[source_position];
			output_position += 1;
		}
	}
	if block_remaining != 0 {
		return Err(Error::InvalidArchive("legacy Huffman block exceeds declared size"));
	}
	reader.assert_zero_padding()?;
	Ok(output)
}

fn decompress_lzs(packed: &[u8], out_size: usize) -> Result<Vec<u8>> {
	let mut reader = MsbBitReader::new(packed);
	let mut output = vec![0_u8; out_size];
	let mut history = [0x20_u8; 2048];
	let mut history_position = 2048 - 17;
	let mut output_position = 0_usize;
	while output_position < out_size {
		if reader.read(1)? != 0 {
			let value = reader.read(8)? as u8;
			output[output_position] = value;
			output_position += 1;
			history[history_position] = value;
			history_position = (history_position + 1) & 2047;
		} else {
			let position = reader.read(11)? as usize;
			let length = reader.read(4)? as usize + 2;
			if length > out_size - output_position {
				return Err(Error::InvalidArchive("LZH -lzs- match exceeds declared size"));
			}
			for index in 0..length {
				let value = history[(position + index) & 2047];
				output[output_position] = value;
				output_position += 1;
				history[history_position] = value;
				history_position = (history_position + 1) & 2047;
			}
		}
	}
	reader.assert_zero_padding()?;
	Ok(output)
}

struct MsbBitReader<'a> {
	bytes:    &'a [u8],
	position: usize,
}

impl<'a> MsbBitReader<'a> {
	const fn new(bytes: &'a [u8]) -> Self {
		Self { bytes, position: 0 }
	}

	fn read(&mut self, count: u8) -> Result<u32> {
		let count = usize::from(count);
		if count > 24 || self.position + count > self.bytes.len() * 8 {
			return Err(Error::InvalidArchive("truncated legacy compressed bitstream"));
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
				return Err(Error::InvalidArchive("non-zero trailing compressed bits"));
			}
		}
		Ok(())
	}
}

struct Huffman {
	counts:        [u16; 17],
	first_codes:   [u32; 17],
	first_symbols: [u16; 17],
	symbols:       Vec<u16>,
	single:        Option<u16>,
}

impl Huffman {
	const fn single(symbol: usize, symbol_count: usize) -> Result<Self> {
		if symbol >= symbol_count {
			return Err(Error::InvalidArchive("legacy Huffman symbol is out of range"));
		}
		Ok(Self {
			counts:        [0; 17],
			first_codes:   [0; 17],
			first_symbols: [0; 17],
			symbols:       Vec::new(),
			single:        Some(symbol as u16),
		})
	}

	fn build(lengths: &[u8], symbol_count: usize) -> Result<Self> {
		let mut counts = [0_u16; 17];
		let mut maximum = 0_usize;
		for &length in lengths
			.get(..symbol_count)
			.ok_or(Error::InvalidArchive("short Huffman lengths"))?
		{
			if length > 16 {
				return Err(Error::InvalidArchive("legacy Huffman code is too long"));
			}
			if length != 0 {
				counts[usize::from(length)] += 1;
				maximum = maximum.max(usize::from(length));
			}
		}
		if maximum == 0 {
			return Err(Error::InvalidArchive("legacy Huffman table has no symbols"));
		}
		let mut first_codes = [0_u32; 17];
		let mut code = 0_u32;
		for length in 1..=16 {
			code = (code + u32::from(counts[length - 1])) * 2;
			if code + u32::from(counts[length]) > 1_u32 << length {
				return Err(Error::InvalidArchive("oversubscribed legacy Huffman table"));
			}
			first_codes[length] = code;
		}
		if first_codes[maximum] + u32::from(counts[maximum]) != 1_u32 << maximum {
			return Err(Error::InvalidArchive("incomplete legacy Huffman table"));
		}
		let mut first_symbols = [0_u16; 17];
		let mut total = 0_u16;
		for length in 1..=16 {
			first_symbols[length] = total;
			total += counts[length];
		}
		let mut symbols = Vec::with_capacity(usize::from(total));
		for length in 1..=16 {
			for (symbol, &symbol_length) in lengths[..symbol_count].iter().enumerate() {
				if usize::from(symbol_length) == length {
					symbols.push(symbol as u16);
				}
			}
		}
		Ok(Self { counts, first_codes, first_symbols, symbols, single: None })
	}

	fn decode(&self, reader: &mut MsbBitReader<'_>) -> Result<u16> {
		if let Some(symbol) = self.single {
			return Ok(symbol);
		}
		let mut code = 0_u32;
		for length in 1..=16 {
			code = (code << 1) | reader.read(1)?;
			let first = self.first_codes[length];
			let delta = code.wrapping_sub(first);
			if delta < u32::from(self.counts[length]) {
				let index = usize::from(self.first_symbols[length]) + delta as usize;
				return self
					.symbols
					.get(index)
					.copied()
					.ok_or(Error::InvalidArchive("invalid legacy Huffman code"));
			}
		}
		Err(Error::InvalidArchive("invalid legacy Huffman code"))
	}
}

fn read_code_length(reader: &mut MsbBitReader<'_>) -> Result<u8> {
	let mut length = reader.read(3)? as u8;
	if length == 7 {
		while reader.read(1)? != 0 {
			length += 1;
			if length > 16 {
				return Err(Error::InvalidArchive("legacy Huffman code is too long"));
			}
		}
	}
	Ok(length)
}

fn read_temporary_tree(reader: &mut MsbBitReader<'_>) -> Result<Huffman> {
	const SYMBOLS: usize = 19;
	let encoded = reader.read(5)? as usize;
	if encoded == 0 {
		return Huffman::single(reader.read(5)? as usize, SYMBOLS);
	}
	if encoded > SYMBOLS {
		return Err(Error::InvalidArchive("invalid temporary Huffman table size"));
	}
	let mut lengths = [0_u8; SYMBOLS];
	let mut index = 0_usize;
	while index < encoded {
		lengths[index] = read_code_length(reader)?;
		index += 1;
		if index == 3 {
			let skipped = reader.read(2)? as usize;
			if index + skipped > encoded {
				return Err(Error::InvalidArchive("invalid temporary Huffman table"));
			}
			index += skipped;
		}
	}
	Huffman::build(&lengths, SYMBOLS)
}

fn read_command_tree(reader: &mut MsbBitReader<'_>, temporary: &Huffman) -> Result<Huffman> {
	const SYMBOLS: usize = 510;
	let encoded = reader.read(9)? as usize;
	if encoded == 0 {
		return Huffman::single(reader.read(9)? as usize, SYMBOLS);
	}
	if encoded > SYMBOLS {
		return Err(Error::InvalidArchive("invalid command Huffman table size"));
	}
	let mut lengths = [0_u8; SYMBOLS];
	let mut index = 0_usize;
	while index < encoded {
		let code = temporary.decode(reader)?;
		if code <= 2 {
			let skipped = match code {
				0 => 1,
				1 => reader.read(4)? as usize + 3,
				_ => reader.read(9)? as usize + 20,
			};
			if index + skipped > encoded {
				return Err(Error::InvalidArchive("invalid command Huffman table"));
			}
			index += skipped;
		} else {
			lengths[index] = (code - 2) as u8;
			index += 1;
		}
	}
	Huffman::build(&lengths, SYMBOLS)
}

fn read_position_tree(
	reader: &mut MsbBitReader<'_>,
	position_bits: u8,
	symbol_count: usize,
) -> Result<Huffman> {
	let encoded = reader.read(position_bits)? as usize;
	if encoded == 0 {
		return Huffman::single(reader.read(position_bits)? as usize, symbol_count);
	}
	if encoded > symbol_count || symbol_count > 17 {
		return Err(Error::InvalidArchive("invalid position Huffman table size"));
	}
	let mut lengths = [0_u8; 17];
	for length in &mut lengths[..encoded] {
		*length = read_code_length(reader)?;
	}
	Huffman::build(&lengths, symbol_count)
}

const fn make_crc16_table() -> [u16; 256] {
	let mut table = [0_u16; 256];
	let mut index = 0;
	while index < 256 {
		let mut value = index as u16;
		let mut bit = 0;
		while bit < 8 {
			value = if value & 1 != 0 {
				(value >> 1) ^ 0xa001
			} else {
				value >> 1
			};
			bit += 1;
		}
		table[index] = value;
		index += 1;
	}
	table
}

const CRC16_TABLE: [u16; 256] = make_crc16_table();

fn crc16_arc(bytes: &[u8]) -> u16 {
	crc16_arc_seed(bytes, 0)
}

fn crc16_arc_seed(bytes: &[u8], mut value: u16) -> u16 {
	for byte in bytes {
		value = (value >> 8) ^ CRC16_TABLE[usize::from((value as u8) ^ byte)];
	}
	value
}

fn crc16_with_zero_range(bytes: &[u8], start: usize, end: usize) -> u16 {
	let value = crc16_arc_seed(&bytes[..start], 0);
	let value = crc16_arc_seed(&[0, 0][..end - start], value);
	crc16_arc_seed(&bytes[end..], value)
}

fn decode_legacy(bytes: &[u8]) -> String {
	let end = bytes
		.iter()
		.position(|byte| *byte == 0)
		.unwrap_or(bytes.len());
	let mut output = String::with_capacity(end);
	for &byte in &bytes[..end] {
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

fn decode_utf16(bytes: &[u8]) -> Result<String> {
	if bytes.len() & 1 != 0 {
		return Err(Error::InvalidArchive("odd LZH UTF-16 path length"));
	}
	let mut end = bytes.len();
	while end >= 2 && bytes[end - 2..end] == [0, 0] {
		end -= 2;
	}
	let units: Vec<u16> = bytes[..end]
		.as_chunks::<2>()
		.0
		.iter()
		.map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
		.collect();
	Ok(String::from_units(xutf::transcode::<Utf16Le, Utf8>(&units)))
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

const fn filetime_to_unix(ticks: u64) -> Option<u64> {
	(ticks / 10_000_000).checked_sub(11_644_473_600)
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

const fn check_index_size(actual: u64, limits: Limits) -> Result<()> {
	if actual > limits.max_index_size() {
		return Err(Error::IndexTooLarge { actual, limit: limits.max_index_size() });
	}
	Ok(())
}

const fn check_path_size(actual: u64, limits: Limits) -> Result<()> {
	if actual > limits.max_path_size() {
		return Err(Error::PathTooLong { actual, limit: limits.max_path_size() });
	}
	Ok(())
}

const fn u16_at(bytes: &[u8], offset: usize) -> u16 {
	u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
	u32::from_le_bytes(
		bytes[offset..offset + 4]
			.try_into()
			.expect("validated fixed-width field"),
	)
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
	u64::from_le_bytes(
		bytes[offset..offset + 8]
			.try_into()
			.expect("validated fixed-width field"),
	)
}
