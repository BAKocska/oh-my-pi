//! Microsoft Cabinet container indexing and folder decompression.

mod lzx;

use std::io::{Read, Seek, SeekFrom, Write};

use flate2::{Decompress, FlushDecompress, Status};
use omp_core::Str;

use self::lzx::Decoder as LzxDecoder;
use crate::{Entry, Error, Limits, Result, entry::Storage, path};

const FIXED_HEADER_SIZE: u64 = 36;
const DATA_BLOCK_SIZE: usize = 8;
const MAX_DATA_OUTPUT: usize = 32 * 1024;
const ATTRIBUTE_READ_ONLY: u16 = 0x01;
const ATTRIBUTE_DIRECTORY: u16 = 0x10;
const ATTRIBUTE_EXECUTE: u16 = 0x40;
const ATTRIBUTE_UTF8_NAME: u16 = 0x80;

#[derive(Clone, Copy)]
struct FolderDescription {
	data_start:    u64,
	data_end:      u64,
	block_count:   u16,
	method:        u8,
	parameter:     u8,
	required_size: u64,
}

struct FileRecord {
	path:          Str,
	directory:     bool,
	size:          u64,
	folder_offset: u64,
	folder_index:  usize,
	modified:      Option<u64>,
	mode:          u32,
}

#[derive(Clone, Copy)]
struct RawBlock {
	folder_offset: u64,
	data_offset:   u64,
	size:          u64,
}

struct DecodedFolder {
	bytes:      Vec<u8>,
	raw_blocks: Vec<RawBlock>,
}

/// Returns whether bytes begin with the CAB signature.
pub fn is_header(bytes: &[u8]) -> bool {
	bytes.starts_with(b"MSCF")
}

/// Indexes a CAB container, eagerly retaining each supported compressed folder.
pub(crate) fn read_entries(
	source: &mut (impl Read + Seek),
	file_size: u64,
	limits: Limits,
	decoded: &mut Vec<Vec<u8>>,
) -> Result<Vec<Entry>> {
	if file_size < FIXED_HEADER_SIZE {
		return Err(Error::InvalidArchive("truncated CAB CFHEADER"));
	}
	let fixed = read_exact_range(source, 0, FIXED_HEADER_SIZE, file_size)?;
	if !is_header(&fixed) {
		return Err(Error::InvalidArchive("CAB signature is missing"));
	}
	if le_u32(&fixed, 4)? != 0 || le_u32(&fixed, 12)? != 0 || le_u32(&fixed, 20)? != 0 {
		return Err(Error::InvalidArchive("reserved CAB CFHEADER fields are nonzero"));
	}
	let cabinet_size = u64::from(le_u32(&fixed, 8)?);
	if !(FIXED_HEADER_SIZE..=file_size).contains(&cabinet_size) {
		return Err(Error::InvalidArchive("declared CAB cabinet size is out of bounds"));
	}
	let file_table_offset = u64::from(le_u32(&fixed, 16)?);
	if !(FIXED_HEADER_SIZE..=cabinet_size).contains(&file_table_offset) {
		return Err(Error::InvalidArchive("CAB CFFILE table offset is out of bounds"));
	}
	if fixed[24] != 3 || fixed[25] != 1 {
		return Err(Error::UnsupportedFeature("CAB format version other than 1.3"));
	}
	let folder_count = u64::from(le_u16(&fixed, 26)?);
	let file_count = u64::from(le_u16(&fixed, 28)?);
	let flags = le_u16(&fixed, 30)?;
	if flags & 0x0003 != 0 {
		return Err(Error::UnsupportedFeature("multi-volume CAB archive"));
	}
	check_entry_count(folder_count + file_count, limits)?;
	if folder_count == 0 && file_count != 0 {
		return Err(Error::InvalidArchive("CAB files exist without a folder"));
	}

	let mut folder_reserve_size = 0_u64;
	let mut data_reserve_size = 0_usize;
	let mut folder_table_offset = FIXED_HEADER_SIZE;
	if flags & 0x0004 != 0 {
		let reserve_end = FIXED_HEADER_SIZE
			.checked_add(4)
			.ok_or(Error::InvalidArchive("CAB reserve header range overflows"))?;
		let reserve = read_exact_range(source, FIXED_HEADER_SIZE, reserve_end, cabinet_size)?;
		let header_reserve_size = u64::from(le_u16(&reserve, 0)?);
		folder_reserve_size = u64::from(reserve[2]);
		data_reserve_size = usize::from(reserve[3]);
		if header_reserve_size > 60_000 {
			return Err(Error::InvalidArchive("CAB CFHEADER reserve area exceeds 60000 bytes"));
		}
		folder_table_offset = reserve_end
			.checked_add(header_reserve_size)
			.ok_or(Error::InvalidArchive("CAB reserve area overflows"))?;
	}
	let folder_record_size = 8_u64 + folder_reserve_size;
	let folder_table_end = folder_table_offset
		.checked_add(
			folder_count
				.checked_mul(folder_record_size)
				.ok_or(Error::InvalidArchive("CAB CFFOLDER table size overflows"))?,
		)
		.ok_or(Error::InvalidArchive("CAB CFFOLDER table range overflows"))?;
	if folder_table_end > cabinet_size || folder_table_end > file_table_offset {
		return Err(Error::InvalidArchive("CAB CFFOLDER table is out of bounds"));
	}
	check_index_size(folder_table_end, limits)?;
	let header = read_exact_range(source, 0, folder_table_end, cabinet_size)?;
	let folder_capacity = usize::try_from(folder_count)
		.map_err(|_| Error::InvalidArchive("CAB folder count does not fit this platform"))?;
	let mut folders = Vec::with_capacity(folder_capacity);
	for index in 0..folder_capacity {
		let offset = usize::try_from(folder_table_offset + index as u64 * folder_record_size)
			.map_err(|_| Error::InvalidArchive("CAB CFFOLDER offset does not fit this platform"))?;
		let compression = le_u16(&header, offset + 6)?;
		folders.push(FolderDescription {
			data_start:    u64::from(le_u32(&header, offset)?),
			data_end:      cabinet_size,
			block_count:   le_u16(&header, offset + 4)?,
			method:        (compression & 0x000f) as u8,
			parameter:     (compression >> 8) as u8,
			required_size: 0,
		});
	}
	for index in 0..folders.len() {
		if folders[index].data_start < folder_table_end || folders[index].data_start > cabinet_size {
			return Err(Error::InvalidArchive("CAB CFFOLDER data offset is out of bounds"));
		}
		let start = folders[index].data_start;
		folders[index].data_end = folders
			.iter()
			.map(|candidate| candidate.data_start)
			.filter(|candidate| *candidate > start)
			.min()
			.unwrap_or(cabinet_size);
	}
	let first_data_offset = folders
		.iter()
		.map(|folder| folder.data_start)
		.min()
		.unwrap_or(cabinet_size);
	if file_table_offset > first_data_offset {
		return Err(Error::InvalidArchive("CAB CFFILE table overlaps folder data"));
	}
	let file_table_size = first_data_offset - file_table_offset;
	let index_size = folder_table_end
		.checked_add(file_table_size)
		.ok_or(Error::InvalidArchive("CAB index size overflows"))?;
	check_index_size(index_size, limits)?;
	let file_table = read_exact_range(source, file_table_offset, first_data_offset, cabinet_size)?;

	let file_capacity = usize::try_from(file_count)
		.map_err(|_| Error::InvalidArchive("CAB file count does not fit this platform"))?;
	let mut records = Vec::with_capacity(file_capacity);
	let mut position = 0_usize;
	for _ in 0..file_capacity {
		let fixed_end = position
			.checked_add(16)
			.ok_or(Error::InvalidArchive("CAB CFFILE offset overflows"))?;
		if fixed_end > file_table.len() {
			return Err(Error::InvalidArchive("truncated CAB CFFILE entry"));
		}
		let size = u64::from(le_u32(&file_table, position)?);
		let folder_offset = u64::from(le_u32(&file_table, position + 4)?);
		let folder_index = usize::from(le_u16(&file_table, position + 8)?);
		let date = le_u16(&file_table, position + 10)?;
		let time = le_u16(&file_table, position + 12)?;
		let attributes = le_u16(&file_table, position + 14)?;
		position = fixed_end;
		let relative_name_end = file_table[position..]
			.iter()
			.position(|&byte| byte == 0)
			.ok_or(Error::InvalidArchive("unterminated CAB CFFILE name"))?;
		let name_end = position + relative_name_end;
		let name_bytes = &file_table[position..name_end];
		if name_bytes.len() > 256 {
			return Err(Error::InvalidArchive("CAB CFFILE name exceeds 256 bytes"));
		}
		if name_bytes.len() as u64 > limits.max_path_size() {
			return Err(Error::PathTooLong {
				actual: name_bytes.len() as u64,
				limit:  limits.max_path_size(),
			});
		}
		position = name_end + 1;
		if folder_index >= 0xfffd {
			return Err(Error::UnsupportedFeature("multi-volume CAB continued file"));
		}
		if folder_index >= folders.len() {
			return Err(Error::InvalidArchive("CAB CFFILE references a missing folder"));
		}
		let raw_name = decode_name(name_bytes, attributes & ATTRIBUTE_UTF8_NAME != 0)?;
		if size > limits.max_member_size() {
			return Err(Error::MemberTooLarge {
				path:   raw_name.clone(),
				actual: size,
				limit:  limits.max_member_size(),
			});
		}
		let end = folder_offset
			.checked_add(size)
			.ok_or(Error::InvalidArchive("CAB CFFILE range overflows"))?;
		folders[folder_index].required_size = folders[folder_index].required_size.max(end);
		let Some(normalized) = path::normalize(raw_name.as_str(), false) else {
			continue;
		};
		path::validate(&normalized, limits)?;
		let directory = attributes & ATTRIBUTE_DIRECTORY != 0;
		records.push(FileRecord {
			path: normalized,
			directory,
			size,
			folder_offset,
			folder_index,
			modified: dos_timestamp(date, time)?,
			mode: mode_from_attributes(attributes, directory),
		});
	}
	for folder in &folders {
		check_in_memory_size(folder.required_size, limits)?;
	}

	let mut folder_storage = Vec::with_capacity(folders.len());
	let mut retained_size = decoded.iter().try_fold(0_u64, |total, bytes| {
		total
			.checked_add(bytes.len() as u64)
			.ok_or(Error::ArchiveTooLargeInMemory {
				actual: u64::MAX,
				limit:  limits.max_in_memory_size(),
			})
	})?;
	for folder in &folders {
		if folder.method == 2 || folder.method > 3 {
			folder_storage.push(None);
			continue;
		}
		let decoded_folder = decode_folder(source, *folder, data_reserve_size, limits)?;
		let needs_buffer = folder.method != 0
			|| records.iter().any(|record| {
				record.folder_index == folder_storage.len()
					&& !record.directory
					&& raw_member_range(&decoded_folder.raw_blocks, record.folder_offset, record.size)
						.is_none()
			});
		let buffer = if needs_buffer {
			retained_size = retained_size
				.checked_add(decoded_folder.bytes.len() as u64)
				.ok_or(Error::ArchiveTooLargeInMemory {
					actual: u64::MAX,
					limit:  limits.max_in_memory_size(),
				})?;
			check_in_memory_size(retained_size, limits)?;
			let index = u32::try_from(decoded.len())
				.map_err(|_| Error::InvalidArchive("too many retained CAB folder buffers"))?;
			decoded.push(decoded_folder.bytes);
			Some(index)
		} else {
			None
		};
		folder_storage.push(Some((buffer, decoded_folder.raw_blocks)));
	}

	let mut entries = Vec::with_capacity(records.len());
	for record in records {
		let storage = if record.directory {
			Storage::Synthetic
		} else if let Some((buffer, raw_blocks)) = &folder_storage[record.folder_index] {
			if let Some((data_offset, stored_size)) =
				raw_member_range(raw_blocks, record.folder_offset, record.size)
			{
				Storage::Raw { data_offset, stored_size }
			} else {
				Storage::Buffered {
					buffer:      buffer.ok_or(Error::InvalidArchive("CAB folder buffer is missing"))?,
					data_offset: record.folder_offset,
					stored_size: record.size,
				}
			}
		} else {
			let folder = folders[record.folder_index];
			Storage::CabUnsupported { method: folder.method, parameter: folder.parameter }
		};
		entries.push(Entry {
			path: record.path,
			directory: record.directory,
			size: if record.directory { 0 } else { record.size },
			modified_unix_seconds: record.modified,
			mode: Some(record.mode),
			storage,
		});
	}
	Ok(entries)
}

/// Reports deferred unsupported CAB folder methods during member extraction.
pub(crate) fn read_entry_to<W: Write>(
	_source: &mut (impl Read + Seek),
	entry: &Entry,
	_output: &mut W,
) -> Result<u64> {
	match entry.storage {
		Storage::CabUnsupported { method: 2, parameter } => {
			Err(Error::UnsupportedCabQuantum { level: parameter })
		},
		Storage::CabUnsupported { method: 3, parameter } => {
			Err(Error::UnsupportedCabLzxWindow { bits: parameter })
		},
		Storage::CabUnsupported { method, parameter } => {
			Err(Error::UnsupportedCabCompression { method, parameter })
		},
		_ => Err(Error::InvalidArchive("CAB entry has unexpected storage")),
	}
}

fn decode_folder(
	source: &mut (impl Read + Seek),
	description: FolderDescription,
	data_reserve_size: usize,
	limits: Limits,
) -> Result<DecodedFolder> {
	let compressed_size = description
		.data_end
		.checked_sub(description.data_start)
		.ok_or(Error::InvalidArchive("CAB folder data range is reversed"))?;
	check_in_memory_size(compressed_size, limits)?;
	let bytes =
		read_exact_range(source, description.data_start, description.data_end, description.data_end)?;
	let mut position = 0_usize;
	let mut output_size = 0_usize;
	let mut raw_blocks = Vec::with_capacity(usize::from(description.block_count));
	for _ in 0..usize::from(description.block_count) {
		let payload_start = position
			.checked_add(DATA_BLOCK_SIZE + data_reserve_size)
			.ok_or(Error::InvalidArchive("CAB CFDATA header range overflows"))?;
		if payload_start > bytes.len() {
			return Err(Error::InvalidArchive("truncated CAB CFDATA header"));
		}
		let compressed = usize::from(le_u16(&bytes, position + 4)?);
		let uncompressed = usize::from(le_u16(&bytes, position + 6)?);
		if uncompressed == 0 {
			return Err(Error::UnsupportedFeature("multi-volume CAB split CFDATA block"));
		}
		if uncompressed > MAX_DATA_OUTPUT {
			return Err(Error::InvalidArchive("CAB CFDATA expands beyond 32768 bytes"));
		}
		let payload_end = payload_start
			.checked_add(compressed)
			.ok_or(Error::InvalidArchive("CAB CFDATA payload range overflows"))?;
		if payload_end > bytes.len() {
			return Err(Error::InvalidArchive("truncated CAB CFDATA payload"));
		}
		let expected_checksum = le_u32(&bytes, position)?;
		if expected_checksum != 0 {
			let payload_checksum = cab_checksum(&bytes[payload_start..payload_end], 0);
			let actual_checksum = cab_checksum(&bytes[position + 4..payload_start], payload_checksum);
			if actual_checksum != expected_checksum {
				return Err(Error::InvalidArchive("CAB CFDATA block checksum mismatch"));
			}
		}
		if description.method == 0 {
			raw_blocks.push(RawBlock {
				folder_offset: output_size as u64,
				data_offset:   description.data_start + payload_start as u64,
				size:          uncompressed as u64,
			});
		}
		output_size = output_size
			.checked_add(uncompressed)
			.ok_or(Error::InvalidArchive("CAB folder output size overflows"))?;
		check_in_memory_size(output_size as u64, limits)?;
		position = payload_end;
	}
	if (output_size as u64) < description.required_size {
		return Err(Error::InvalidArchive("CAB folder data is shorter than its file table declares"));
	}

	let mut output = vec![0_u8; output_size];
	let mut lzx = if description.method == 3 {
		Some(LzxDecoder::new(description.parameter)?)
	} else {
		None
	};
	position = 0;
	let mut output_position = 0_usize;
	for _ in 0..usize::from(description.block_count) {
		let compressed = usize::from(le_u16(&bytes, position + 4)?);
		let uncompressed = usize::from(le_u16(&bytes, position + 6)?);
		let payload_start = position + DATA_BLOCK_SIZE + data_reserve_size;
		let payload_end = payload_start + compressed;
		let payload = &bytes[payload_start..payload_end];
		let (history, tail) = output.split_at_mut(output_position);
		let destination = &mut tail[..uncompressed];
		match description.method {
			0 => {
				if compressed != uncompressed {
					return Err(Error::InvalidArchive("uncompressed CAB CFDATA sizes do not match"));
				}
				destination.copy_from_slice(payload);
			},
			1 => decode_mszip(payload, history, destination)?,
			3 => lzx
				.as_mut()
				.expect("LZX decoder exists for method 3")
				.decompress_frame(payload, destination)?,
			_ => return Err(Error::InvalidArchive("unsupported CAB folder reached decoder")),
		}
		output_position += uncompressed;
		position = payload_end;
	}
	Ok(DecodedFolder { bytes: output, raw_blocks })
}

fn decode_mszip(payload: &[u8], history: &[u8], output: &mut [u8]) -> Result<()> {
	if !payload.starts_with(b"CK") {
		return Err(Error::InvalidArchive("CAB MSZIP block is missing its CK signature"));
	}
	let dictionary = &history[history.len().saturating_sub(MAX_DATA_OUTPUT)..];
	let mut decompressor = Decompress::new(false);
	if !dictionary.is_empty() {
		decompressor
			.set_dictionary(dictionary)
			.map_err(|_| Error::InvalidArchive("CAB MSZIP dictionary setup failed"))?;
	}
	let status = decompressor
		.decompress(&payload[2..], output, FlushDecompress::Finish)
		.map_err(|_| Error::InvalidArchive("CAB MSZIP decompression failed"))?;
	if status != Status::StreamEnd || decompressor.total_out() != output.len() as u64 {
		return Err(Error::InvalidArchive("CAB MSZIP block produced the wrong byte count"));
	}
	Ok(())
}

fn raw_member_range(blocks: &[RawBlock], offset: u64, size: u64) -> Option<(u64, u64)> {
	let end = offset.checked_add(size)?;
	blocks.iter().find_map(|block| {
		let block_end = block.folder_offset.checked_add(block.size)?;
		(offset >= block.folder_offset && end <= block_end)
			.then(|| (block.data_offset + (offset - block.folder_offset), size))
	})
}

fn read_exact_range(
	source: &mut (impl Read + Seek),
	start: u64,
	end: u64,
	cabinet_size: u64,
) -> Result<Vec<u8>> {
	if end < start || end > cabinet_size {
		return Err(Error::InvalidArchive("CAB metadata range is out of bounds"));
	}
	let length = usize::try_from(end - start)
		.map_err(|_| Error::InvalidArchive("CAB range does not fit this platform"))?;
	let mut bytes = vec![0_u8; length];
	source.seek(SeekFrom::Start(start))?;
	source.read_exact(&mut bytes)?;
	Ok(bytes)
}

fn le_u16(bytes: &[u8], offset: usize) -> Result<u16> {
	let value = bytes
		.get(offset..offset + 2)
		.ok_or(Error::InvalidArchive("truncated CAB integer field"))?;
	Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn le_u32(bytes: &[u8], offset: usize) -> Result<u32> {
	let value = bytes
		.get(offset..offset + 4)
		.ok_or(Error::InvalidArchive("truncated CAB integer field"))?;
	Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn cab_checksum(bytes: &[u8], initial: u32) -> u32 {
	let mut checksum = initial;
	let mut chunks = bytes.chunks_exact(4);
	for chunk in &mut chunks {
		checksum ^= u32::from_le_bytes(chunk.try_into().expect("four-byte chunk"));
	}
	let remaining = chunks.remainder();
	let remainder = match remaining {
		[a, b, c] => u32::from(*a) << 16 | u32::from(*b) << 8 | u32::from(*c),
		[a, b] => u32::from(*a) << 8 | u32::from(*b),
		[a] => u32::from(*a),
		[] => 0,
		_ => unreachable!("chunks_exact remainder is shorter than four bytes"),
	};
	checksum ^ remainder
}

fn decode_name(bytes: &[u8], utf8: bool) -> Result<Str> {
	if utf8 {
		return std::str::from_utf8(bytes)
			.map(Str::new)
			.map_err(|_| Error::InvalidArchive("CAB file name is not valid UTF-8"));
	}
	let mut output = String::with_capacity(bytes.len());
	for &byte in bytes {
		output.push(cp1252(byte));
	}
	Ok(output.into())
}

const fn cp1252(byte: u8) -> char {
	match byte {
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
		0x81 | 0x8d | 0x8f | 0x90 | 0x9d => '\u{fffd}',
		_ => byte as char,
	}
}

fn dos_timestamp(date: u16, time: u16) -> Result<Option<u64>> {
	if date == 0 && time == 0 {
		return Ok(None);
	}
	let year = 1980 + i64::from(date >> 9);
	let month = i64::from(date >> 5 & 0x0f);
	let day = i64::from(date & 0x1f);
	let hour = u64::from(time >> 11);
	let minute = u64::from(time >> 5 & 0x3f);
	let second = u64::from(time & 0x1f) * 2;
	if !(1..=12).contains(&month)
		|| !(1..=31).contains(&day)
		|| hour > 23
		|| minute > 59
		|| second > 59
	{
		return Err(Error::InvalidArchive("CAB file has an invalid DOS timestamp"));
	}
	let days = days_from_civil(year, month, day);
	let seconds = u64::try_from(days)
		.map_err(|_| Error::InvalidArchive("CAB DOS timestamp predates Unix time"))?
		* 86_400
		+ hour * 3600
		+ minute * 60
		+ second;
	Ok(Some(seconds))
}

const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
	let adjusted_year = year - if month <= 2 { 1 } else { 0 };
	let era = adjusted_year.div_euclid(400);
	let year_of_era = adjusted_year - era * 400;
	let adjusted_month = month + if month > 2 { -3 } else { 9 };
	let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
	let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
	era * 146_097 + day_of_era - 719_468
}

const fn mode_from_attributes(attributes: u16, directory: bool) -> u32 {
	if directory {
		return 0o040_755;
	}
	let mut permissions = if attributes & ATTRIBUTE_READ_ONLY != 0 {
		0o444
	} else {
		0o644
	};
	if attributes & ATTRIBUTE_EXECUTE != 0 {
		permissions |= 0o111;
	}
	0o100_000 | permissions
}

const fn check_entry_count(actual: u64, limits: Limits) -> Result<()> {
	if actual > limits.max_entries() {
		return Err(Error::TooManyEntries { actual, limit: limits.max_entries() });
	}
	Ok(())
}

const fn check_index_size(actual: u64, limits: Limits) -> Result<()> {
	if actual > limits.max_index_size() {
		return Err(Error::IndexTooLarge { actual, limit: limits.max_index_size() });
	}
	Ok(())
}

const fn check_in_memory_size(actual: u64, limits: Limits) -> Result<()> {
	if actual > limits.max_in_memory_size() {
		return Err(Error::ArchiveTooLargeInMemory { actual, limit: limits.max_in_memory_size() });
	}
	Ok(())
}
