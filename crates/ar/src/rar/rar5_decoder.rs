// Decompression structure adapted from unrar5j by Stephane Bury
// (Copyright 2025, Apache License 2.0).

use crate::{Error, Result};

const MAIN_SIZE: usize = 306;
const DIST_SIZE: usize = 80;
const ALIGN_SIZE: usize = 16;
const LEN_SIZE: usize = 44;
const TABLE_SIZE: usize = MAIN_SIZE + DIST_SIZE + ALIGN_SIZE + LEN_SIZE;
const LEN_PLUS: [usize; 40] = [
	0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3,
	3, 3, 3, 3, 3, 3, 3, 3,
];
const MAX_MATCH: usize = 0x1004;
const MAX_FILTER_SIZE: usize = 1 << 22;

struct Bits {
	bytes:         Vec<u8>,
	packed_len:    usize,
	pos:           usize,
	bit:           u8,
	block_end:     usize,
	block_end_bit: u8,
}

impl Bits {
	fn new(packed: &[u8]) -> Self {
		let mut bytes = Vec::with_capacity(packed.len() + 16);
		bytes.extend_from_slice(packed);
		bytes.resize(packed.len() + 16, 0xff);
		Self { bytes, packed_len: packed.len(), pos: 0, bit: 0, block_end: 0, block_end_bit: 0 }
	}

	fn read(&mut self, count: usize) -> Result<usize> {
		if count > 31 {
			return invalid("invalid RAR5 bit count");
		}
		let mut value = 0usize;
		let mut left = count;
		while left > 0 {
			if self.pos >= self.packed_len {
				return invalid("truncated RAR5 compressed data");
			}
			let take = left.min(8 - usize::from(self.bit));
			value = (value << take)
				| usize::from(
					(self.bytes[self.pos] >> (8 - usize::from(self.bit) - take))
						& ((1u16 << take) - 1) as u8,
				);
			self.bit += take as u8;
			left -= take;
			if self.bit == 8 {
				self.bit = 0;
				self.pos += 1;
			}
		}
		Ok(value)
	}

	const fn align(&mut self) {
		if self.bit != 0 {
			self.pos += 1;
			self.bit = 0;
		}
	}

	fn byte(&mut self) -> Result<u8> {
		if self.bit != 0 {
			return invalid("unaligned RAR5 compressed data");
		}
		if self.pos >= self.packed_len {
			return invalid("truncated RAR5 compressed data");
		}
		let byte = self.bytes[self.pos];
		self.pos += 1;
		Ok(byte)
	}

	const fn at_block_end(&self) -> bool {
		self.pos > self.block_end || (self.pos == self.block_end && self.bit >= self.block_end_bit)
	}
}

struct Huffman {
	max_bits:     usize,
	size:         usize,
	counts:       Vec<usize>,
	first_code:   Vec<usize>,
	first_symbol: Vec<usize>,
	symbols:      Vec<usize>,
	ready:        bool,
}

impl Huffman {
	fn new(size: usize) -> Self {
		Self::with_max_bits(size, 15)
	}

	fn with_max_bits(size: usize, max_bits: usize) -> Self {
		Self {
			max_bits,
			size,
			counts: vec![0; max_bits + 1],
			first_code: vec![0; max_bits + 1],
			first_symbol: vec![0; max_bits + 1],
			symbols: vec![0; size],
			ready: false,
		}
	}

	fn build(&mut self, lengths: &[u8], offset: usize) -> Result<()> {
		self.counts.fill(0);
		for &length in lengths
			.get(offset..offset + self.size)
			.ok_or(Error::InvalidArchive("truncated RAR5 Huffman table"))?
		{
			let length = usize::from(length);
			if length > self.max_bits {
				return invalid("invalid RAR5 Huffman code length");
			}
			if length != 0 {
				self.counts[length] += 1;
			}
		}
		let mut code = 0usize;
		let mut symbols = 0usize;
		for bits in 1..=self.max_bits {
			code = (code + self.counts[bits - 1]) << 1;
			self.first_code[bits] = code;
			self.first_symbol[bits] = symbols;
			symbols += self.counts[bits];
		}
		if symbols != 0 && code + self.counts[self.max_bits] != 1 << self.max_bits {
			return invalid(match self.size {
				20 => "incomplete RAR5 level Huffman table",
				MAIN_SIZE => "incomplete RAR5 main Huffman table",
				DIST_SIZE => "incomplete RAR5 distance Huffman table",
				ALIGN_SIZE => "incomplete RAR5 alignment Huffman table",
				LEN_SIZE => "incomplete RAR5 length Huffman table",
				_ => "incomplete RAR5 Huffman table",
			});
		}
		let mut next = self.first_symbol.clone();
		for symbol in 0..self.size {
			let length = usize::from(lengths[offset + symbol]);
			if length != 0 {
				self.symbols[next[length]] = symbol;
				next[length] += 1;
			}
		}
		self.ready = symbols != 0;
		Ok(())
	}

	fn decode(&self, bits: &mut Bits) -> Result<usize> {
		if !self.ready {
			return invalid("empty RAR5 Huffman table");
		}
		let mut code = 0usize;
		for length in 1..=self.max_bits {
			code = (code << 1) | bits.read(1)?;
			if let Some(index) = code.checked_sub(self.first_code[length])
				&& index < self.counts[length]
			{
				return Ok(self.symbols[self.first_symbol[length] + index]);
			}
		}
		invalid("invalid RAR5 Huffman symbol")
	}
}

#[derive(Clone, Copy)]
struct Filter {
	kind:     u8,
	channels: usize,
	start:    usize,
	size:     usize,
}

/// Stateful RAR5 LZSS decoder. Reuse one instance for a solid chain.
pub struct Rar5Decoder {
	history:   Vec<u8>,
	reps:      [usize; 4],
	last_len:  usize,
	main:      Huffman,
	dist:      Huffman,
	align:     Huffman,
	length:    Huffman,
	use_align: bool,
}

impl Default for Rar5Decoder {
	fn default() -> Self {
		Self {
			history:   Vec::new(),
			reps:      [usize::MAX; 4],
			last_len:  0,
			main:      Huffman::new(MAIN_SIZE),
			dist:      Huffman::new(DIST_SIZE),
			align:     Huffman::new(ALIGN_SIZE),
			length:    Huffman::new(LEN_SIZE),
			use_align: false,
		}
	}
}

impl Rar5Decoder {
	pub(crate) fn reset(&mut self) {
		*self = Self::default();
	}

	pub(crate) fn decode(
		&mut self,
		packed: &[u8],
		unpacked_size: usize,
		dictionary_size: usize,
		solid: bool,
		version: u8,
	) -> Result<Vec<u8>> {
		if version > 1 {
			return Err(Error::UnsupportedFeature("RAR5 compression algorithm version"));
		}
		if !solid {
			self.reset();
		}
		if dictionary_size < 128 * 1024 {
			return invalid("invalid RAR5 dictionary size");
		}
		let prior_start = self.history.len().saturating_sub(dictionary_size);
		let prior = &self.history[prior_start..];
		let capacity = prior
			.len()
			.checked_add(unpacked_size)
			.and_then(|size| size.checked_add(MAX_MATCH))
			.ok_or(Error::InvalidArchive("RAR5 output size overflows"))?;
		let mut data = vec![0; capacity];
		data[..prior.len()].copy_from_slice(prior);
		let out_start = prior.len();
		let out_end = out_start + unpacked_size;
		let mut out_pos = out_start;
		let mut filters = Vec::new();
		let mut bits = Bits::new(packed);
		let mut last_block = false;

		self.read_tables(&mut bits, version, &mut last_block)?;
		while out_pos < out_end {
			if bits.at_block_end() {
				if bits.pos > bits.block_end
					|| (bits.pos == bits.block_end && bits.bit > bits.block_end_bit)
				{
					return invalid("RAR5 compressed block overread");
				}
				bits.align();
				if last_block {
					break;
				}
				self.read_tables(&mut bits, version, &mut last_block)?;
				continue;
			}
			let symbol = self.main.decode(&mut bits)?;
			if symbol < 256 {
				data[out_pos] = symbol as u8;
				out_pos += 1;
				continue;
			}
			if symbol == 256 {
				let bytes = (bits.read(2)? + 1) * 8;
				let mut start = 0usize;
				for shift in (0..bytes).step_by(8) {
					start |= bits.read(8)? << shift;
				}
				let size_bytes = (bits.read(2)? + 1) * 8;
				let mut size = 0usize;
				for shift in (0..size_bytes).step_by(8) {
					size |= bits.read(8)? << shift;
				}
				let kind = bits.read(3)? as u8;
				let channels = if kind == 0 { bits.read(5)? + 1 } else { 0 };
				if kind > 3 {
					return Err(Error::UnsupportedFeature("RAR5 filter type"));
				}
				let filter_start = out_pos - out_start + start;
				if size > MAX_FILTER_SIZE
					|| filter_start
						.checked_add(size)
						.is_none_or(|end| end > unpacked_size)
				{
					return invalid("invalid RAR5 filter range");
				}
				filters.push(Filter { kind, channels, start: filter_start, size });
				continue;
			}

			let mut distance = self.reps[0];
			let length;
			if symbol < 262 {
				if symbol >= 258 {
					let rep_index = symbol - 258;
					distance = self.reps[rep_index];
					for index in (1..=rep_index).rev() {
						self.reps[index] = self.reps[index - 1];
					}
					self.reps[0] = distance;
					let slot = self.length.decode(&mut bits)?;
					length = if slot >= 8 {
						slot_to_length(&mut bits, slot)?
					} else {
						slot
					} + 2;
					self.last_len = length;
				} else {
					length = self.last_len;
					if length == 0 {
						continue;
					}
				}
			} else {
				self.reps.copy_within(0..3, 1);
				let slot = symbol - 262;
				let mut match_length = if slot >= 8 {
					slot_to_length(&mut bits, slot)?
				} else {
					slot
				} + 2;
				let mut distance_slot = self.dist.decode(&mut bits)?;
				if distance_slot >= 4 {
					let extra_bits = (distance_slot - 2) >> 1;
					distance_slot = (2 | (distance_slot & 1)) << extra_bits;
					if extra_bits < 4 {
						distance_slot += bits.read(extra_bits)?;
					} else {
						match_length += LEN_PLUS.get(extra_bits).copied().unwrap_or(3);
						if self.use_align {
							distance_slot +=
								(bits.read(extra_bits - 4)? << 4) + self.align.decode(&mut bits)?;
						} else {
							distance_slot += bits.read(extra_bits)?;
						}
					}
				}
				distance = distance_slot + 1;
				self.reps[0] = distance;
				self.last_len = match_length;
				length = match_length;
			}
			if distance == 0 || distance > dictionary_size || distance > out_pos {
				return invalid("invalid RAR5 LZ distance");
			}
			if out_pos
				.checked_add(length)
				.is_none_or(|end| end > data.len())
			{
				return invalid("invalid RAR5 LZ match length");
			}
			for index in 0..length {
				data[out_pos + index] = data[out_pos + index - distance];
			}
			out_pos += length;
		}
		if out_pos != out_end {
			return invalid("RAR5 decompressed size mismatch");
		}
		let mut output = data[out_start..out_end].to_vec();
		apply_filters(&mut output, &filters);
		let history_start = out_end.saturating_sub(dictionary_size);
		self.history = data[history_start..out_end].to_vec();
		Ok(output)
	}

	fn read_tables(&mut self, bits: &mut Bits, version: u8, last_block: &mut bool) -> Result<()> {
		bits.align();
		let flags = bits.byte()?;
		let mut checksum = flags ^ bits.byte()?;
		let size_bytes = usize::from((flags >> 3) & 3);
		if size_bytes == 3 {
			return invalid("invalid RAR5 compressed block header");
		}
		let mut block_size = usize::from(bits.byte()?);
		checksum ^= block_size as u8;
		for index in 0..size_bytes {
			let byte = bits.byte()?;
			checksum ^= byte;
			block_size += usize::from(byte) << (8 * (index + 1));
		}
		if checksum != 0x5a {
			return invalid("RAR5 compressed block checksum mismatch");
		}
		let mut end_bits = usize::from(flags & 7) + 1;
		block_size += end_bits >> 3;
		if block_size == 0 {
			return invalid("empty RAR5 compressed block");
		}
		block_size -= 1;
		end_bits &= 7;
		bits.block_end = bits
			.pos
			.checked_add(block_size)
			.ok_or(Error::InvalidArchive("RAR5 block size overflows"))?;
		if bits.block_end > bits.packed_len {
			return invalid("truncated RAR5 compressed block");
		}
		bits.block_end_bit = end_bits as u8;
		*last_block = flags & 0x40 != 0;
		if flags & 0x80 == 0 {
			return Ok(());
		}

		let mut level_lengths = [0u8; 20];
		let mut index = 0usize;
		while index < level_lengths.len() {
			let length = bits.read(4)? as u8;
			if length == 15 {
				let zeros = bits.read(4)?;
				if zeros != 0 {
					index = level_lengths.len().min(index + zeros + 2);
					continue;
				}
			}
			level_lengths[index] = length;
			index += 1;
		}
		let mut level = Huffman::new(20);
		level.build(&level_lengths, 0)?;
		let table_length = if version == 1 {
			TABLE_SIZE
		} else {
			TABLE_SIZE - 16
		};
		let mut compact = vec![0u8; table_length];
		let mut index = 0usize;
		while index < table_length {
			let symbol = level.decode(bits)?;
			if symbol < 16 {
				compact[index] = symbol as u8;
				index += 1;
				continue;
			}
			let base = ((symbol - 16) & 1) * 4;
			let count = base * 2 + 3 + bits.read(base + 3)?;
			let value = if symbol < 18 {
				if index == 0 {
					return invalid("invalid RAR5 Huffman repeat");
				}
				compact[index - 1]
			} else {
				0
			};
			let end = table_length.min(index + count);
			compact[index..end].fill(value);
			index = end;
		}
		let mut lengths = vec![0u8; TABLE_SIZE];
		if version == 0 {
			lengths[..MAIN_SIZE + 64].copy_from_slice(&compact[..MAIN_SIZE + 64]);
			lengths[MAIN_SIZE + DIST_SIZE..].copy_from_slice(&compact[MAIN_SIZE + 64..]);
		} else {
			lengths.copy_from_slice(&compact);
		}
		self.main.build(&lengths, 0)?;
		self.dist.build(&lengths, MAIN_SIZE)?;
		self.align.build(&lengths, MAIN_SIZE + DIST_SIZE)?;
		self
			.length
			.build(&lengths, MAIN_SIZE + DIST_SIZE + ALIGN_SIZE)?;
		self.use_align = lengths[MAIN_SIZE + DIST_SIZE..MAIN_SIZE + DIST_SIZE + ALIGN_SIZE]
			.iter()
			.any(|&length| length != 4);
		Ok(())
	}
}

fn slot_to_length(bits: &mut Bits, slot: usize) -> Result<usize> {
	let count = (slot >> 2).saturating_sub(1);
	Ok(((4 | (slot & 3)) << count) + bits.read(count)?)
}

fn apply_filters(output: &mut [u8], filters: &[Filter]) {
	for filter in filters {
		let data = &mut output[filter.start..filter.start + filter.size];
		if filter.kind == 0 {
			let source = data.to_vec();
			let mut source_pos = 0usize;
			for channel in 0..filter.channels {
				let mut previous = 0u8;
				for pos in (channel..data.len()).step_by(filter.channels) {
					previous = previous.wrapping_sub(source[source_pos]);
					data[pos] = previous;
					source_pos += 1;
				}
			}
		} else if filter.kind == 1 || filter.kind == 2 {
			const FILE_SIZE: u32 = 1 << 24;
			let mut pos = 0usize;
			while pos + 4 < data.len() {
				let opcode = data[pos];
				pos += 1;
				if opcode != 0xe8 && (filter.kind == 1 || opcode != 0xe9) {
					continue;
				}
				let offset = ((filter.start + pos) as u32) & (FILE_SIZE - 1);
				let mut address = read_i32(data, pos);
				if (address as u32) < FILE_SIZE {
					address = address.wrapping_sub(offset as i32);
				} else if (address as u32) >= 0u32.wrapping_sub(offset) {
					address = address.wrapping_add(FILE_SIZE as i32);
				} else {
					pos += 4;
					continue;
				}
				write_i32(data, pos, address);
				pos += 4;
			}
		} else {
			for pos in (0..data.len().saturating_sub(3)).step_by(4) {
				if data[pos + 3] != 0xeb {
					continue;
				}
				let instruction = read_i32(data, pos);
				let address = (instruction & i32::MIN)
					| instruction.wrapping_sub(((filter.start + pos) >> 2) as i32) & 0x00ff_ffff;
				write_i32(data, pos, address);
			}
		}
	}
}

fn read_i32(bytes: &[u8], offset: usize) -> i32 {
	i32::from_le_bytes(
		bytes[offset..offset + 4]
			.try_into()
			.expect("checked filter range"),
	)
}

fn write_i32(bytes: &mut [u8], offset: usize, value: i32) {
	bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

const fn invalid<T>(reason: &'static str) -> Result<T> {
	Err(Error::InvalidArchive(reason))
}
