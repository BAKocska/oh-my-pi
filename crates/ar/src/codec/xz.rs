//! Bounded XZ container decoding with integrity and standard filter support.

use sha2::{Digest as _, Sha256};

use super::lzma::lzma2_decompress;
use crate::{Error, Limits, Result};

const XZ_MAGIC: &[u8; 6] = b"\xfd7zXZ\0";

fn invalid(message: &'static str) -> Error {
	Error::InvalidArchive(message)
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Result<u32> {
	let value = bytes
		.get(offset..offset + 4)
		.ok_or_else(|| invalid("truncated integer in XZ stream"))?;
	Ok(u32::from_le_bytes(value.try_into().expect("slice length")))
}

fn read_u32_be(bytes: &[u8], offset: usize) -> Result<u32> {
	let value = bytes
		.get(offset..offset + 4)
		.ok_or_else(|| invalid("truncated integer in XZ BCJ filter"))?;
	Ok(u32::from_be_bytes(value.try_into().expect("slice length")))
}

fn write_u32_le(bytes: &mut [u8], offset: usize, value: u32) {
	bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_u32_be(bytes: &mut [u8], offset: usize, value: u32) {
	bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

struct Cursor<'a> {
	bytes: &'a [u8],
	pos:   usize,
	limit: usize,
}

impl Cursor<'_> {
	fn read_varint(&mut self) -> Result<u64> {
		let mut value = 0_u64;
		for index in 0..9 {
			let byte = *self
				.bytes
				.get(self.pos)
				.filter(|_| self.pos < self.limit)
				.ok_or_else(|| invalid("truncated XZ variable-length integer"))?;
			self.pos += 1;
			if index > 0 && byte == 0 {
				return Err(invalid("non-canonical XZ variable-length integer"));
			}
			value |= u64::from(byte & 0x7f) << (index * 7);
			if byte & 0x80 == 0 {
				return Ok(value);
			}
		}
		Err(invalid("XZ variable-length integer is too long"))
	}
}

#[derive(Clone, Copy)]
struct Record {
	unpadded_size:     u64,
	uncompressed_size: u64,
}
fn padded_block_size(record: Record) -> Result<usize> {
	let size = record
		.unpadded_size
		.checked_add(3)
		.ok_or_else(|| invalid("XZ block size overflow"))?
		& !3;
	usize::try_from(size).map_err(|_| invalid("XZ block size is too large"))
}

struct Stream {
	start:       usize,
	index_start: usize,
	check_id:    u8,
	records:     Vec<Record>,
}

const fn check_size(check_id: u8) -> Option<usize> {
	match check_id {
		0 => Some(0),
		1 => Some(4),
		4 => Some(8),
		10 => Some(32),
		_ => None,
	}
}

fn parse_index(bytes: &[u8], start: usize, size: usize) -> Result<Vec<Record>> {
	let end = start
		.checked_add(size)
		.filter(|end| size >= 8 && *end <= bytes.len())
		.ok_or_else(|| invalid("invalid XZ index range"))?;
	if bytes[start] != 0 {
		return Err(invalid("missing XZ index indicator"));
	}
	if crc32fast::hash(&bytes[start..end - 4]) != read_u32_le(bytes, end - 4)? {
		return Err(invalid("XZ index CRC32 mismatch"));
	}
	let mut cursor = Cursor { bytes, pos: start + 1, limit: end - 4 };
	let count = cursor.read_varint()?;
	if count > size as u64 / 2 {
		return Err(invalid("impossible XZ index record count"));
	}
	let count = usize::try_from(count).map_err(|_| invalid("XZ index record count is too large"))?;
	let mut records = Vec::with_capacity(count);
	for _ in 0..count {
		let unpadded_size = cursor.read_varint()?;
		let uncompressed_size = cursor.read_varint()?;
		if unpadded_size == 0 {
			return Err(invalid("zero XZ unpadded block size"));
		}
		records.push(Record { unpadded_size, uncompressed_size });
	}
	if bytes[cursor.pos..cursor.limit]
		.iter()
		.any(|byte| *byte != 0)
	{
		return Err(invalid("non-zero XZ index padding"));
	}
	Ok(records)
}

fn discover_streams(bytes: &[u8]) -> Result<Vec<Stream>> {
	if bytes.is_empty() || bytes.len() & 3 != 0 {
		return Err(invalid("XZ stream size is not a multiple of four bytes"));
	}
	let mut streams = Vec::new();
	let mut end = bytes.len();
	while end > 0 {
		while end >= 4 && bytes[end - 4..end] == [0, 0, 0, 0] {
			end -= 4;
		}
		if end == 0 {
			return Err(invalid("XZ padding without a stream"));
		}
		if end < 24 {
			return Err(invalid("truncated XZ stream framing"));
		}
		let footer = end - 12;
		if bytes[footer + 10..footer + 12] != [0x59, 0x5a] {
			return Err(invalid("XZ footer magic mismatch"));
		}
		if crc32fast::hash(&bytes[footer + 4..footer + 10]) != read_u32_le(bytes, footer)? {
			return Err(invalid("XZ footer CRC32 mismatch"));
		}
		let flag0 = bytes[footer + 8];
		let flag1 = bytes[footer + 9];
		if flag0 != 0 || flag1 & 0xf0 != 0 {
			return Err(invalid("unsupported XZ stream flags"));
		}
		let check_id = flag1 & 0xf;
		if check_size(check_id).is_none() {
			return Err(Error::UnsupportedFeature("XZ integrity check"));
		}
		let index_size = (u64::from(read_u32_le(bytes, footer + 4)?) + 1)
			.checked_mul(4)
			.ok_or_else(|| invalid("invalid XZ backward index size"))?;
		let index_size =
			usize::try_from(index_size).map_err(|_| invalid("XZ backward index size is too large"))?;
		let index_start = footer
			.checked_sub(index_size)
			.ok_or_else(|| invalid("invalid XZ backward index size"))?;
		let records = parse_index(bytes, index_start, index_size)?;
		let mut blocks_size = 0_u64;
		for record in &records {
			blocks_size = blocks_size
				.checked_add(
					record
						.unpadded_size
						.checked_add(3)
						.ok_or_else(|| invalid("XZ block size overflow"))?
						& !3,
				)
				.ok_or_else(|| invalid("XZ block sizes are too large"))?;
		}
		let blocks_size =
			usize::try_from(blocks_size).map_err(|_| invalid("XZ block sizes are too large"))?;
		let start = index_start
			.checked_sub(blocks_size)
			.and_then(|value| value.checked_sub(12))
			.ok_or_else(|| invalid("invalid XZ header position"))?;
		if bytes.get(start..start + 6) != Some(XZ_MAGIC.as_slice()) {
			return Err(invalid("invalid XZ header position or magic"));
		}
		if bytes[start + 6] != flag0 || bytes[start + 7] != flag1 {
			return Err(invalid("XZ header and footer flags differ"));
		}
		if crc32fast::hash(&bytes[start + 6..start + 8]) != read_u32_le(bytes, start + 8)? {
			return Err(invalid("XZ header CRC32 mismatch"));
		}
		streams.push(Stream { start, index_start, check_id, records });
		end = start;
	}
	streams.reverse();
	Ok(streams)
}

struct Filter<'a> {
	id:         u64,
	properties: &'a [u8],
}

fn delta_decode(bytes: &mut [u8], distance: usize) {
	let mut history = [0_u8; 256];
	let mut position = 0_usize;
	for byte in bytes {
		let value = byte.wrapping_add(history[(distance + position) & 0xff]);
		history[position] = value;
		*byte = value;
		position = position.wrapping_sub(1) & 0xff;
	}
}

pub(crate) fn x86_decode(bytes: &mut [u8], start_offset: u32) {
	const MASK_TO_BIT_NUMBER: [u32; 5] = [0, 1, 2, 2, 3];
	if bytes.len() < 5 {
		return;
	}
	let mut previous_mask = 0_u32;
	let mut previous_position = 0xffff_fffb_u32;
	let mut position = 0_usize;
	let limit = bytes.len() - 5;
	while position <= limit {
		let mut byte = bytes[position];
		if byte != 0xe8 && byte != 0xe9 {
			position += 1;
			continue;
		}
		let absolute_position = start_offset.wrapping_add(position as u32);
		let offset = absolute_position.wrapping_sub(previous_position);
		previous_position = absolute_position;
		if offset > 5 {
			previous_mask = 0;
		} else {
			for _ in 0..offset {
				previous_mask = (previous_mask & 0x77) << 1;
			}
		}
		byte = bytes[position + 4];
		if (byte == 0 || byte == 0xff) && previous_mask >> 1 <= 4 && previous_mask >> 1 != 3 {
			let mut source = u32::from_le_bytes(
				bytes[position + 1..position + 5]
					.try_into()
					.expect("slice length"),
			);
			let destination = loop {
				let destination = source.wrapping_sub(absolute_position).wrapping_sub(5);
				if previous_mask == 0 {
					break destination;
				}
				let bit_index = MASK_TO_BIT_NUMBER[(previous_mask >> 1) as usize] * 8;
				let test_byte = (destination >> (24 - bit_index)) as u8;
				if test_byte != 0 && test_byte != 0xff {
					break destination;
				}
				let low_mask = if bit_index == 0 {
					u32::MAX
				} else {
					(1_u32 << (32 - bit_index)) - 1
				};
				source = destination ^ low_mask;
			};
			bytes[position + 4] = !(((destination >> 24) as u8 & 1).wrapping_sub(1));
			bytes[position + 1..position + 4].copy_from_slice(&destination.to_le_bytes()[..3]);
			position += 5;
			previous_mask = 0;
		} else {
			position += 1;
			previous_mask |= 1;
			if byte == 0 || byte == 0xff {
				previous_mask |= 0x10;
			}
		}
	}
}

fn power_pc_decode(bytes: &mut [u8], start_offset: u32) {
	for index in (0..bytes.len().saturating_sub(3)).step_by(4) {
		if bytes[index] >> 2 != 0x12 || bytes[index + 3] & 3 != 1 {
			continue;
		}
		let source = (u32::from(bytes[index] & 3) << 24)
			| (u32::from(bytes[index + 1]) << 16)
			| (u32::from(bytes[index + 2]) << 8)
			| u32::from(bytes[index + 3] & 0xfc);
		let destination = source.wrapping_sub(start_offset).wrapping_sub(index as u32);
		bytes[index] = 0x48 | (destination >> 24) as u8 & 3;
		bytes[index + 1] = (destination >> 16) as u8;
		bytes[index + 2] = (destination >> 8) as u8;
		bytes[index + 3] = (bytes[index + 3] & 3) | destination as u8 & 0xfc;
	}
}

fn arm_decode(bytes: &mut [u8], start_offset: u32) {
	for index in (0..bytes.len().saturating_sub(3)).step_by(4) {
		if bytes[index + 3] != 0xeb {
			continue;
		}
		let source = (u32::from(bytes[index + 2]) << 18)
			| (u32::from(bytes[index + 1]) << 10)
			| (u32::from(bytes[index]) << 2);
		let destination = source
			.wrapping_sub(start_offset)
			.wrapping_sub(index as u32)
			.wrapping_sub(8)
			>> 2;
		bytes[index] = destination as u8;
		bytes[index + 1] = (destination >> 8) as u8;
		bytes[index + 2] = (destination >> 16) as u8;
	}
}

fn arm_thumb_decode(bytes: &mut [u8], start_offset: u32) {
	let mut index = 0_usize;
	while index + 4 <= bytes.len() {
		if bytes[index + 1] & 0xf8 == 0xf0 && bytes[index + 3] & 0xf8 == 0xf8 {
			let source = ((u32::from(bytes[index + 1] & 7) << 19)
				| (u32::from(bytes[index]) << 11)
				| (u32::from(bytes[index + 3] & 7) << 8)
				| u32::from(bytes[index + 2]))
				<< 1;
			let destination = source
				.wrapping_sub(start_offset)
				.wrapping_sub(index as u32)
				.wrapping_sub(4)
				>> 1;
			bytes[index + 1] = 0xf0 | (destination >> 19) as u8 & 7;
			bytes[index] = (destination >> 11) as u8;
			bytes[index + 3] = 0xf8 | (destination >> 8) as u8 & 7;
			bytes[index + 2] = destination as u8;
			index += 4;
		} else {
			index += 2;
		}
	}
}

fn sparc_decode(bytes: &mut [u8], start_offset: u32) -> Result<()> {
	for index in (0..bytes.len().saturating_sub(3)).step_by(4) {
		if !((bytes[index] == 0x40 && bytes[index + 1] & 0xc0 == 0)
			|| (bytes[index] == 0x7f && bytes[index + 1] & 0xc0 == 0xc0))
		{
			continue;
		}
		let source = read_u32_be(bytes, index)? << 2;
		let mut destination = source.wrapping_sub(start_offset).wrapping_sub(index as u32) >> 2;
		destination = ((0_u32.wrapping_sub((destination >> 22) & 1) << 22) & 0x3fff_ffff)
			| (destination & 0x3f_ffff)
			| 0x4000_0000;
		write_u32_be(bytes, index, destination);
	}
	Ok(())
}

fn ia64_decode(bytes: &mut [u8], start_offset: u32) {
	const BRANCH_TABLE: [u8; 32] = [
		0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 4, 6, 6, 0, 0, 7, 7, 4, 4, 0, 0, 4, 4, 0,
		0,
	];
	for index in (0..bytes.len().saturating_sub(15)).step_by(16) {
		let mask = BRANCH_TABLE[usize::from(bytes[index] & 0x1f)];
		let mut bit_position = 5_usize;
		for slot in 0..3 {
			if mask >> slot & 1 != 0 {
				let byte_position = bit_position >> 3;
				let bit_offset = bit_position & 7;
				let mut instruction = 0_u64;
				for byte in 0..6 {
					instruction |= u64::from(bytes[index + byte_position + byte]) << (byte * 8);
				}
				let mut normalized = instruction >> bit_offset;
				if normalized >> 37 & 15 == 5 && normalized >> 9 & 7 == 0 {
					let source =
						((((normalized >> 13) & 0xf_ffff) | ((normalized >> 36) & 1) << 20) as u32) << 4;
					let destination = source.wrapping_sub(start_offset).wrapping_sub(index as u32) >> 4;
					normalized &= !(0x8f_ffff_u64 << 13);
					normalized |= u64::from(destination & 0xf_ffff) << 13;
					normalized |= u64::from(destination & 0x10_0000) << 16;
					instruction &= (1_u64 << bit_offset) - 1;
					instruction |= normalized << bit_offset;
					for byte in 0..6 {
						bytes[index + byte_position + byte] = (instruction >> (byte * 8)) as u8;
					}
				}
			}
			bit_position += 41;
		}
	}
}

fn arm64_decode(bytes: &mut [u8], start_offset: u32) -> Result<()> {
	for index in (0..bytes.len().saturating_sub(3)).step_by(4) {
		let pc = start_offset.wrapping_add(index as u32);
		let mut instruction = read_u32_le(bytes, index)?;
		if instruction >> 26 == 0x25 {
			instruction = 0x9400_0000 | instruction.wrapping_sub(pc >> 2) & 0x03ff_ffff;
			write_u32_le(bytes, index, instruction);
		} else if instruction & 0x9f00_0000 == 0x9000_0000 {
			let source = (instruction >> 29 & 3) | (instruction >> 3 & 0x001f_fffc);
			if source.wrapping_add(0x2_0000) & 0x1c_0000 != 0 {
				continue;
			}
			let destination = source.wrapping_sub(pc >> 12);
			instruction &= 0x9000_001f;
			instruction |= (destination & 3) << 29;
			instruction |= (destination & 0x3_fffc) << 3;
			instruction |= 0_u32.wrapping_sub(destination & 0x2_0000) & 0xe0_0000;
			write_u32_le(bytes, index, instruction);
		}
	}
	Ok(())
}

fn riscv_decode(bytes: &mut [u8], start_offset: u32) -> Result<()> {
	if bytes.len() < 8 {
		return Ok(());
	}
	let mut index = 0_usize;
	while index <= bytes.len() - 8 {
		let first = bytes[index];
		if first == 0xef {
			let byte1 = bytes[index + 1];
			if byte1 & 0x0d == 0 {
				let address = ((u32::from(byte1 & 0xf0) << 13)
					| (u32::from(bytes[index + 2]) << 9)
					| (u32::from(bytes[index + 3]) << 1))
					.wrapping_sub(start_offset)
					.wrapping_sub(index as u32);
				bytes[index + 1] = (byte1 & 0x0f) | (address >> 8) as u8 & 0xf0;
				bytes[index + 2] = ((address >> 16) as u8 & 0x0f)
					| ((address >> 7) as u8 & 0x10)
					| ((address << 4) as u8 & 0xe0);
				bytes[index + 3] = ((address >> 4) as u8 & 0x7f) | ((address >> 13) as u8 & 0x80);
				index += 4;
				continue;
			}
		}
		if first & 0x7f != 0x17 {
			index += 2;
			continue;
		}
		let mut instruction = read_u32_le(bytes, index)?;
		let instruction2;
		if instruction & 0xe80 != 0 {
			let next = read_u32_le(bytes, index + 4)?;
			if ((instruction << 8) ^ next.wrapping_sub(3) & 0xf_8003) != 0 {
				index += 6;
				continue;
			}
			let address = (instruction & 0xffff_f000).wrapping_add(next >> 20);
			instruction = 0x17 | (2 << 7) | next << 12;
			instruction2 = address;
		} else {
			let register = instruction >> 27;
			if instruction.wrapping_sub(0x3117).wrapping_shl(18) >= register & 0x1d {
				index += 4;
				continue;
			}
			let address = read_u32_be(bytes, index + 4)?
				.wrapping_sub(start_offset)
				.wrapping_sub(index as u32);
			instruction2 = instruction >> 12 | address << 20;
			instruction = 0x17 | register << 7 | address.wrapping_add(0x800) & 0xffff_f000;
		}
		write_u32_le(bytes, index, instruction);
		write_u32_le(bytes, index + 4, instruction2);
		index += 8;
	}
	Ok(())
}

fn apply_filter(bytes: &mut [u8], filter: Filter<'_>) -> Result<()> {
	if filter.id == 3 {
		if filter.properties.len() != 1 {
			return Err(invalid("invalid XZ Delta filter properties"));
		}
		delta_decode(bytes, usize::from(filter.properties[0]) + 1);
		return Ok(());
	}
	if !filter.properties.is_empty() && filter.properties.len() != 4 {
		return Err(invalid("invalid XZ BCJ filter properties"));
	}
	let start_offset = if filter.properties.len() == 4 {
		u32::from_le_bytes(filter.properties.try_into().expect("slice length"))
	} else {
		0
	};
	let alignment = match filter.id {
		6 => 16,
		8 | 11 => 2,
		4 => 1,
		_ => 4,
	};
	if start_offset & (alignment - 1) != 0 {
		return Err(invalid("invalid XZ BCJ filter start offset"));
	}
	match filter.id {
		4 => x86_decode(bytes, start_offset),
		5 => power_pc_decode(bytes, start_offset),
		6 => ia64_decode(bytes, start_offset),
		7 => arm_decode(bytes, start_offset),
		8 => arm_thumb_decode(bytes, start_offset),
		9 => sparc_decode(bytes, start_offset)?,
		10 => arm64_decode(bytes, start_offset)?,
		11 => riscv_decode(bytes, start_offset)?,
		_ => return Err(Error::UnsupportedFeature("XZ filter")),
	}
	Ok(())
}

const fn crc64_table() -> [u64; 256] {
	let mut table = [0_u64; 256];
	let mut index = 0;
	while index < 256 {
		let mut value = index as u64;
		let mut bit = 0;
		while bit < 8 {
			value = if value & 1 != 0 {
				(value >> 1) ^ 0xc96c_5795_d787_0f42
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

const CRC64_TABLE: [u64; 256] = crc64_table();

fn crc64(bytes: &[u8]) -> u64 {
	let mut value = u64::MAX;
	for byte in bytes {
		value = value >> 8 ^ CRC64_TABLE[((value as u8) ^ byte) as usize];
	}
	!value
}

fn verify_check(check_id: u8, output: &[u8], expected: &[u8]) -> Result<()> {
	match check_id {
		0 => Ok(()),
		1 if expected == crc32fast::hash(output).to_le_bytes() => Ok(()),
		1 => Err(invalid("XZ block CRC32 mismatch")),
		4 if expected == crc64(output).to_le_bytes() => Ok(()),
		4 => Err(invalid("XZ block CRC64 mismatch")),
		10 if expected == Sha256::digest(output).as_slice() => Ok(()),
		10 => Err(invalid("XZ block SHA-256 mismatch")),
		_ => Err(Error::UnsupportedFeature("XZ integrity check")),
	}
}

fn decode_block(bytes: &[u8], offset: usize, record: Record, check_id: u8) -> Result<Vec<u8>> {
	let first = *bytes
		.get(offset)
		.ok_or_else(|| invalid("missing XZ block header"))?;
	if first == 0 {
		return Err(invalid("missing XZ block header"));
	}
	let header_size = (usize::from(first) + 1) * 4;
	let header_end = offset
		.checked_add(header_size)
		.filter(|end| header_size >= 8 && *end <= bytes.len())
		.ok_or_else(|| invalid("truncated XZ block header"))?;
	if crc32fast::hash(&bytes[offset..header_end - 4]) != read_u32_le(bytes, header_end - 4)? {
		return Err(invalid("XZ block header CRC32 mismatch"));
	}
	let mut cursor = Cursor { bytes, pos: offset + 1, limit: header_end - 4 };
	let flags = bytes[cursor.pos];
	cursor.pos += 1;
	if flags & 0x3c != 0 {
		return Err(invalid("unsupported XZ block flags"));
	}
	let filter_count = usize::from(flags & 3) + 1;
	let declared_compressed = if flags & 0x40 != 0 {
		Some(cursor.read_varint()?)
	} else {
		None
	};
	let declared_uncompressed = if flags & 0x80 != 0 {
		Some(cursor.read_varint()?)
	} else {
		None
	};
	let mut filters = Vec::with_capacity(filter_count);
	for _ in 0..filter_count {
		let id = cursor.read_varint()?;
		let size = usize::try_from(cursor.read_varint()?)
			.map_err(|_| invalid("XZ filter properties are too large"))?;
		let end = cursor
			.pos
			.checked_add(size)
			.filter(|end| *end <= cursor.limit)
			.ok_or_else(|| invalid("truncated XZ filter properties"))?;
		filters.push(Filter { id, properties: &bytes[cursor.pos..end] });
		cursor.pos = end;
	}
	if bytes[cursor.pos..cursor.limit]
		.iter()
		.any(|byte| *byte != 0)
	{
		return Err(invalid("non-zero XZ block header padding"));
	}
	let integrity_size =
		check_size(check_id).ok_or(Error::UnsupportedFeature("XZ integrity check"))?;
	let compressed_size = record
		.unpadded_size
		.checked_sub(header_size as u64 + integrity_size as u64)
		.filter(|size| *size != 0)
		.ok_or_else(|| invalid("invalid XZ compressed block size"))?;
	if declared_compressed.is_some_and(|declared| declared != compressed_size) {
		return Err(invalid("XZ block compressed size mismatch"));
	}
	if declared_uncompressed.is_some_and(|declared| declared != record.uncompressed_size) {
		return Err(invalid("XZ block uncompressed size mismatch"));
	}
	let compressed_size = usize::try_from(compressed_size)
		.map_err(|_| invalid("XZ compressed block size is too large"))?;
	let compressed_start = header_end;
	let compressed_end = compressed_start
		.checked_add(compressed_size)
		.filter(|end| *end <= bytes.len())
		.ok_or_else(|| invalid("truncated XZ block data"))?;
	let padding_size = (4 - ((header_size + compressed_size) & 3)) & 3;
	let check_start = compressed_end
		.checked_add(padding_size)
		.filter(|start| {
			start
				.checked_add(integrity_size)
				.is_some_and(|end| end <= bytes.len())
		})
		.ok_or_else(|| invalid("truncated XZ block integrity check"))?;
	let terminal = filters
		.last()
		.ok_or_else(|| invalid("XZ block has no filters"))?;
	if terminal.id != 0x21 {
		return Err(if terminal.id == 0x22 {
			Error::UnsupportedFeature("XZ terminal filter ID 0x22 (LZMA2 required)")
		} else {
			Error::UnsupportedFeature("XZ terminal filter (LZMA2 required)")
		});
	}
	if terminal.properties.len() != 1 {
		return Err(invalid("invalid XZ LZMA2 filter properties"));
	}
	let output_size = usize::try_from(record.uncompressed_size)
		.map_err(|_| invalid("XZ decoded block size is too large"))?;
	let mut output = lzma2_decompress(
		terminal.properties[0],
		&bytes[compressed_start..compressed_end],
		output_size,
	)?;
	if output.len() != output_size {
		return Err(invalid("XZ decoded block size mismatch"));
	}
	for filter in filters[..filters.len() - 1].iter().rev() {
		apply_filter(&mut output, Filter { id: filter.id, properties: filter.properties })?;
	}
	if bytes[compressed_end..check_start]
		.iter()
		.any(|byte| *byte != 0)
	{
		return Err(invalid("non-zero XZ block padding"));
	}
	verify_check(check_id, &output, &bytes[check_start..check_start + integrity_size])?;
	let padded_end = offset
		.checked_add(padded_block_size(record)?)
		.ok_or_else(|| invalid("XZ block range overflow"))?;
	if check_start + integrity_size != padded_end {
		return Err(invalid("XZ block size does not match its index record"));
	}
	Ok(output)
}

/// Decompresses all concatenated `.xz` streams bounded by archive and memory
/// limits.
pub(crate) fn xz_decompress(bytes: &[u8], limits: Limits) -> Result<Vec<u8>> {
	let streams = discover_streams(bytes)?;
	let maximum = limits.max_archive_size().min(limits.max_in_memory_size());
	let mut total_size = 0_u64;
	for stream in &streams {
		for record in &stream.records {
			total_size = total_size
				.checked_add(record.uncompressed_size)
				.ok_or(Error::ArchiveTooLargeInMemory { actual: u64::MAX, limit: maximum })?;
			if total_size > maximum {
				return Err(Error::ArchiveTooLargeInMemory { actual: total_size, limit: maximum });
			}
		}
	}
	let capacity = usize::try_from(total_size)
		.map_err(|_| Error::ArchiveTooLargeInMemory { actual: total_size, limit: maximum })?;
	let mut output = Vec::with_capacity(capacity);
	for stream in streams {
		let mut block_position = stream.start + 12;
		for record in stream.records {
			let block = decode_block(bytes, block_position, record, stream.check_id)?;
			output.extend_from_slice(&block);
			block_position = block_position
				.checked_add(padded_block_size(record)?)
				.ok_or_else(|| invalid("XZ block position overflow"))?;
		}
		if block_position != stream.index_start {
			return Err(invalid("XZ blocks do not align with index"));
		}
	}
	Ok(output)
}
