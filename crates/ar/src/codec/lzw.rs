//! ncompress `.Z` LZW decoding with bounded output.

use crate::{Error, Limits, Result};

const CLEAR_CODE: usize = 256;
const FIRST_BLOCK_CODE: usize = 257;
const MIN_BITS: usize = 9;
const MAX_BITS: usize = 16;

struct LsbCodeReader<'a> {
	bytes:        &'a [u8],
	bit_position: usize,
	group_start:  usize,
}

impl<'a> LsbCodeReader<'a> {
	const fn new(bytes: &'a [u8]) -> Self {
		Self { bytes, bit_position: 0, group_start: 0 }
	}

	const fn total_bits(&self) -> usize {
		self.bytes.len().saturating_mul(8)
	}

	const fn remaining_bits(&self) -> usize {
		self.total_bits().saturating_sub(self.bit_position)
	}

	fn read(&mut self, width: usize) -> Option<usize> {
		if self.remaining_bits() < width {
			return None;
		}
		let mut value = 0_usize;
		for bit in 0..width {
			let position = self.bit_position + bit;
			value |= usize::from((self.bytes[position >> 3] >> (position & 7)) & 1) << bit;
		}
		self.bit_position += width;
		Some(value)
	}

	fn align_code_group(&mut self, width: usize) -> Result<()> {
		let group_bits = width * 8;
		let consumed = self.bit_position - self.group_start;
		let groups = consumed.div_ceil(group_bits);
		let aligned = self
			.group_start
			.checked_add(groups * group_bits)
			.ok_or(Error::InvalidArchive("truncated compress (.Z) code group"))?;
		if aligned > self.total_bits() {
			return Err(Error::InvalidArchive("truncated compress (.Z) code group"));
		}
		self.bit_position = aligned;
		self.group_start = aligned;
		Ok(())
	}

	fn assert_final_padding(&self) -> Result<()> {
		for position in self.bit_position..self.total_bits() {
			if (self.bytes[position >> 3] >> (position & 7)) & 1 != 0 {
				return Err(Error::InvalidArchive("non-zero compress (.Z) padding"));
			}
		}
		Ok(())
	}
}

struct BoundedOutput {
	limit: u64,
	bytes: Vec<u8>,
}

impl BoundedOutput {
	fn new(limit: u64, input_size: usize) -> Self {
		let initial = limit.min((input_size.saturating_mul(2).clamp(64, 64 * 1024)) as u64);
		Self { limit, bytes: Vec::with_capacity(usize::try_from(initial).unwrap_or(64 * 1024)) }
	}

	fn append_reversed(&mut self, stack: &[u8], length: usize) -> Result<()> {
		let actual = (self.bytes.len() as u64)
			.checked_add(length as u64)
			.ok_or(Error::ArchiveTooLarge { actual: u64::MAX, limit: self.limit })?;
		if actual > self.limit {
			return Err(Error::ArchiveTooLarge { actual, limit: self.limit });
		}
		self.bytes.reserve(length);
		for &byte in stack[..length].iter().rev() {
			self.bytes.push(byte);
		}
		Ok(())
	}
}

/// Decompresses one ncompress `.Z` stream bounded by `limits.archive_size`.
pub fn lzw_decompress(bytes: &[u8], limits: Limits) -> Result<Vec<u8>> {
	if bytes.len() < 3 {
		return Err(Error::InvalidArchive("truncated compress (.Z) header"));
	}
	if bytes[..2] != [0x1f, 0x9d] {
		return Err(Error::InvalidArchive("invalid compress (.Z) header"));
	}

	let flags = bytes[2];
	if flags & 0x60 != 0 {
		return Err(Error::UnsupportedFeature("compress (.Z) reserved header flags"));
	}
	let max_bits = usize::from(flags & 0x1f);
	if !(MIN_BITS..=MAX_BITS).contains(&max_bits) {
		return Err(Error::InvalidArchive("invalid compress (.Z) maximum code width"));
	}
	let block_mode = flags & 0x80 != 0;
	let dictionary_limit = 1_usize << max_bits;
	let mut parents = vec![0_u16; dictionary_limit];
	let mut suffixes = vec![0_u8; dictionary_limit];
	let mut stack = vec![0_u8; dictionary_limit];
	let mut reader = LsbCodeReader::new(&bytes[3..]);
	let mut output = BoundedOutput::new(limits.archive_size, bytes.len());

	let mut width = MIN_BITS;
	let mut dictionary_head = if block_mode { FIRST_BLOCK_CODE } else { 256 };
	let mut needs_previous_suffix = false;

	loop {
		let Some(code) = reader.read(width) else {
			reader.assert_final_padding()?;
			return Ok(output.bytes);
		};
		if code >= dictionary_head {
			return Err(Error::InvalidArchive("corrupt compress (.Z) dictionary code"));
		}
		if block_mode && code == CLEAR_CODE {
			reader.align_code_group(width)?;
			width = MIN_BITS;
			dictionary_head = FIRST_BLOCK_CODE;
			needs_previous_suffix = false;
			continue;
		}

		let mut current = code;
		let mut stack_length = 0;
		while current >= 256 {
			if current >= dictionary_head || stack_length >= stack.len() - 1 {
				return Err(Error::InvalidArchive("corrupt compress (.Z) dictionary chain"));
			}
			stack[stack_length] = suffixes[current];
			stack_length += 1;
			current = usize::from(parents[current]);
		}
		stack[stack_length] = current as u8;
		stack_length += 1;

		if needs_previous_suffix {
			suffixes[dictionary_head - 1] = current as u8;
			if code == dictionary_head - 1 {
				stack[0] = current as u8;
			}
		}
		output.append_reversed(&stack, stack_length)?;

		if dictionary_head < dictionary_limit {
			needs_previous_suffix = true;
			parents[dictionary_head] = code as u16;
			dictionary_head += 1;
			if dictionary_head > 1_usize << width && width < max_bits {
				reader.align_code_group(width)?;
				width += 1;
			}
		} else {
			needs_previous_suffix = false;
		}
	}
}
