// Decompression structure adapted from unrar5j by Stephane Bury
// (Copyright 2025, Apache License 2.0).

use crate::{Error, Result};

const MAIN_SIZE: usize = 299;
const DIST_SIZE: usize = 60;
const LOW_DIST_SIZE: usize = 17;
const LEN_SIZE: usize = 28;
const TOTAL_SIZE: usize = MAIN_SIZE + DIST_SIZE + LOW_DIST_SIZE + LEN_SIZE;
const LEN_BASE: [usize; 28] = [
	0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 20, 24, 28, 32, 40, 48, 56, 64, 80, 96, 112, 128,
	160, 192, 224,
];
const LEN_BITS: [usize; 28] =
	[0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5];
const SHORT_DIST_BASE: [usize; 8] = [0, 4, 8, 16, 32, 64, 128, 192];
const SHORT_DIST_BITS: [usize; 8] = [2, 2, 3, 4, 5, 6, 6, 6];
const DIST_COUNTS: [usize; 19] = [4, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 14, 0, 12];

struct Bits {
	bytes:      Vec<u8>,
	packed_len: usize,
	pos:        usize,
	bit:        u8,
}

impl Bits {
	fn new(packed: &[u8]) -> Self {
		let mut bytes = Vec::with_capacity(packed.len() + 4);
		bytes.extend_from_slice(packed);
		bytes.resize(packed.len() + 4, 0);
		Self { bytes, packed_len: packed.len(), pos: 0, bit: 0 }
	}

	fn read(&mut self, count: usize) -> Result<usize> {
		if count > 32 {
			return invalid("invalid RAR4 bit count");
		}
		let mut value = 0usize;
		let mut left = count;
		while left > 0 {
			if self.pos >= self.packed_len {
				return invalid("truncated RAR4 compressed data");
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
}

struct Huffman {
	counts:       [usize; 16],
	first_code:   [usize; 16],
	first_symbol: [usize; 16],
	symbols:      Vec<usize>,
	ready:        bool,
}

impl Huffman {
	fn new(size: usize) -> Self {
		Self {
			counts:       [0; 16],
			first_code:   [0; 16],
			first_symbol: [0; 16],
			symbols:      vec![0; size],
			ready:        false,
		}
	}

	fn build(&mut self, lengths: &[u8], offset: usize, size: usize) -> Result<()> {
		let source = lengths
			.get(offset..offset + size)
			.ok_or(Error::InvalidArchive("truncated RAR4 Huffman table"))?;
		self.counts.fill(0);
		for &length in source {
			let length = usize::from(length & 15);
			if length != 0 {
				self.counts[length] += 1;
			}
		}
		let mut code = 0usize;
		let mut symbol_position = 0usize;
		for bits in 1..=15 {
			code = (code + self.counts[bits - 1]) << 1;
			self.first_code[bits] = code;
			self.first_symbol[bits] = symbol_position;
			symbol_position += self.counts[bits];
		}
		let mut next = self.first_symbol;
		for (symbol, &length) in source.iter().enumerate() {
			let length = usize::from(length & 15);
			if length != 0 {
				self.symbols[next[length]] = symbol;
				next[length] += 1;
			}
		}
		self.ready = symbol_position != 0;
		Ok(())
	}

	fn decode(&self, bits: &mut Bits) -> Result<usize> {
		if !self.ready {
			return invalid("empty RAR4 Huffman table");
		}
		let mut code = 0usize;
		for length in 1..=15 {
			code = (code << 1) | bits.read(1)?;
			if let Some(index) = code.checked_sub(self.first_code[length])
				&& index < self.counts[length]
			{
				return Ok(self.symbols[self.first_symbol[length] + index]);
			}
		}
		invalid("invalid RAR4 Huffman symbol")
	}
}

#[derive(Clone, Copy)]
struct PendingFilter {
	kind:        u8,
	start:       usize,
	size:        usize,
	channels:    usize,
	file_offset: u32,
}

/// Stateful decoder for the RAR 2.9 LZ/Huffman algorithm used by RAR3/4.
pub struct Rar4Decoder {
	history:               Vec<u8>,
	old_distances:         [usize; 4],
	last_distance:         usize,
	last_length:           usize,
	previous_low_distance: usize,
	low_distance_repeats:  usize,
	carried_lengths:       [u8; TOTAL_SIZE],
	filter_types:          Vec<u8>,
	filter_lengths:        Vec<usize>,
	last_filter:           usize,
}

impl Default for Rar4Decoder {
	fn default() -> Self {
		Self {
			history:               Vec::new(),
			old_distances:         [0; 4],
			last_distance:         0,
			last_length:           0,
			previous_low_distance: 0,
			low_distance_repeats:  0,
			carried_lengths:       [0; TOTAL_SIZE],
			filter_types:          Vec::new(),
			filter_lengths:        Vec::new(),
			last_filter:           0,
		}
	}
}

impl Rar4Decoder {
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
		if version < 29 {
			return Err(Error::UnsupportedFeature("RAR 1.5/2.0 compressed stream"));
		}
		if !solid {
			self.reset();
		}
		let prior_start = self.history.len().saturating_sub(dictionary_size);
		let prior = &self.history[prior_start..];
		let capacity = prior
			.len()
			.checked_add(unpacked_size)
			.and_then(|size| size.checked_add(260))
			.ok_or(Error::InvalidArchive("RAR4 output size overflows"))?;
		let mut output = vec![0; capacity];
		output[..prior.len()].copy_from_slice(prior);
		let output_start = prior.len();
		let output_end = output_start + unpacked_size;
		let mut output_position = output_start;
		let mut bits = Bits::new(packed);
		let mut main = Huffman::new(MAIN_SIZE);
		let mut distance_decoder = Huffman::new(DIST_SIZE);
		let mut low_distance_decoder = Huffman::new(LOW_DIST_SIZE);
		let mut length_decoder = Huffman::new(LEN_SIZE);
		let mut filters = Vec::new();

		self.read_tables(
			&mut bits,
			&mut main,
			&mut distance_decoder,
			&mut low_distance_decoder,
			&mut length_decoder,
		)?;
		while output_position < output_end {
			let symbol = main.decode(&mut bits)?;
			if symbol < 256 {
				output[output_position] = symbol as u8;
				output_position += 1;
			} else if symbol >= 271 {
				let slot = symbol - 271;
				let mut length = LEN_BASE[slot] + 3 + bits.read(LEN_BITS[slot])?;
				let distance_slot = distance_decoder.decode(&mut bits)?;
				let (dist_base, dist_bits) = distance_tables(distance_slot)?;
				let mut distance = dist_base + 1;
				if dist_bits != 0 {
					if distance_slot > 9 {
						if dist_bits > 4 {
							distance += bits.read(dist_bits - 4)? << 4;
						}
						if self.low_distance_repeats > 0 {
							self.low_distance_repeats -= 1;
							distance += self.previous_low_distance;
						} else {
							let low = low_distance_decoder.decode(&mut bits)?;
							if low == 16 {
								self.low_distance_repeats = 15;
								distance += self.previous_low_distance;
							} else {
								distance += low;
								self.previous_low_distance = low;
							}
						}
					} else {
						distance += bits.read(dist_bits)?;
					}
				}
				if distance >= 0x2000 {
					length += 1;
				}
				if distance >= 0x40000 {
					length += 1;
				}
				remember_distance(&mut self.old_distances, distance);
				self.last_distance = distance;
				self.last_length = length;
				copy_match(
					&mut output,
					&mut output_position,
					output_end,
					dictionary_size,
					distance,
					length,
				)?;
			} else if symbol == 256 {
				if bits.read(1)? != 0 {
					self.read_tables(
						&mut bits,
						&mut main,
						&mut distance_decoder,
						&mut low_distance_decoder,
						&mut length_decoder,
					)?;
				} else {
					if bits.read(1)? != 0 {
						self.read_tables(
							&mut bits,
							&mut main,
							&mut distance_decoder,
							&mut low_distance_decoder,
							&mut length_decoder,
						)?;
					}
					break;
				}
			} else if symbol == 257 {
				self.read_filter(
					&mut bits,
					output_position - output_start,
					unpacked_size,
					&mut filters,
				)?;
			} else if symbol == 258 {
				if self.last_length != 0 {
					copy_match(
						&mut output,
						&mut output_position,
						output_end,
						dictionary_size,
						self.last_distance,
						self.last_length,
					)?;
				}
			} else if symbol < 263 {
				let index = symbol - 259;
				let distance = self.old_distances[index];
				for pos in (1..=index).rev() {
					self.old_distances[pos] = self.old_distances[pos - 1];
				}
				self.old_distances[0] = distance;
				let slot = length_decoder.decode(&mut bits)?;
				let length = LEN_BASE[slot] + 2 + bits.read(LEN_BITS[slot])?;
				self.last_distance = distance;
				self.last_length = length;
				copy_match(
					&mut output,
					&mut output_position,
					output_end,
					dictionary_size,
					distance,
					length,
				)?;
			} else {
				let slot = symbol - 263;
				let distance = SHORT_DIST_BASE[slot] + 1 + bits.read(SHORT_DIST_BITS[slot])?;
				remember_distance(&mut self.old_distances, distance);
				self.last_distance = distance;
				self.last_length = 2;
				copy_match(
					&mut output,
					&mut output_position,
					output_end,
					dictionary_size,
					distance,
					2,
				)?;
			}
		}
		if output_position != output_end {
			return invalid("RAR4 decompressed size mismatch");
		}
		let mut extracted = output[output_start..output_end].to_vec();
		apply_filters(&mut extracted, &filters);
		self.history = output[output_end.saturating_sub(dictionary_size)..output_end].to_vec();
		Ok(extracted)
	}

	fn read_tables(
		&mut self,
		bits: &mut Bits,
		main: &mut Huffman,
		distance: &mut Huffman,
		low_distance: &mut Huffman,
		length: &mut Huffman,
	) -> Result<()> {
		bits.align();
		if bits.read(1)? != 0 {
			return Err(Error::UnsupportedFeature("RAR4 PPMd compressed block"));
		}
		let keep_previous = bits.read(1)? != 0;
		self.previous_low_distance = 0;
		self.low_distance_repeats = 0;
		if !keep_previous {
			self.carried_lengths.fill(0);
		}
		let lengths = read_length_table(bits, &self.carried_lengths, TOTAL_SIZE)?;
		let mut offset = 0usize;
		main.build(&lengths, offset, MAIN_SIZE)?;
		offset += MAIN_SIZE;
		distance.build(&lengths, offset, DIST_SIZE)?;
		offset += DIST_SIZE;
		low_distance.build(&lengths, offset, LOW_DIST_SIZE)?;
		offset += LOW_DIST_SIZE;
		length.build(&lengths, offset, LEN_SIZE)?;
		self.carried_lengths.copy_from_slice(&lengths);
		Ok(())
	}

	fn read_filter(
		&mut self,
		bits: &mut Bits,
		output_position: usize,
		unpacked_size: usize,
		filters: &mut Vec<PendingFilter>,
	) -> Result<()> {
		let first_byte = bits.read(8)? as u8;
		let mut size = usize::from(first_byte & 7) + 1;
		if size == 7 {
			size = bits.read(8)? + 7;
		} else if size == 8 {
			size = bits.read(16)?;
		}
		let mut code = vec![0u8; size];
		for byte in &mut code {
			*byte = bits.read(8)? as u8;
		}
		let mut vm = VmBits::new(&code);
		let filter_position = if first_byte & 0x80 != 0 {
			let encoded = vm.read_data()?;
			if encoded == 0 {
				self.filter_types.clear();
				self.filter_lengths.clear();
				0
			} else {
				encoded - 1
			}
		} else {
			self.last_filter
		};
		if filter_position > self.filter_types.len() {
			return invalid("invalid RarVM filter index");
		}
		self.last_filter = filter_position;
		let is_new = filter_position == self.filter_types.len();
		let mut start = vm.read_data()?;
		if first_byte & 0x40 != 0 {
			start = start
				.checked_add(258)
				.ok_or(Error::InvalidArchive("RAR4 filter start overflows"))?;
		}
		start = start
			.checked_add(output_position)
			.ok_or(Error::InvalidArchive("RAR4 filter start overflows"))?;
		let block_size = if first_byte & 0x20 != 0 {
			vm.read_data()?
		} else {
			self
				.filter_lengths
				.get(filter_position)
				.copied()
				.unwrap_or(0)
		};
		let mut registers = [0u32; 7];
		registers[3] = 0x3c000;
		registers[4] = block_size as u32;
		if first_byte & 0x10 != 0 {
			let mask = vm.read(7)?;
			for (register, value) in registers.iter_mut().enumerate() {
				if mask & (1 << register) != 0 {
					*value = vm.read_data()? as u32;
				}
			}
		}
		let kind = if is_new {
			let program_size = vm.read_data()?;
			if program_size > (code.len() * 8).saturating_sub(vm.position) / 8 {
				return invalid("truncated RarVM filter program");
			}
			let mut program = vec![0u8; program_size];
			for byte in &mut program {
				*byte = vm.read(8)? as u8;
			}
			let kind = identify_filter(&program);
			self.filter_types.push(kind);
			self.filter_lengths.push(block_size);
			kind
		} else {
			self.filter_lengths[filter_position] = block_size;
			self.filter_types[filter_position]
		};
		if !matches!(kind, 1 | 2 | 6) {
			return Err(Error::UnsupportedFeature("RAR4 custom RarVM filter program"));
		}
		if kind == 6 && registers[0] == 0 {
			return invalid("invalid RarVM delta channel count");
		}
		if start
			.checked_add(block_size)
			.is_none_or(|end| end > unpacked_size)
		{
			return invalid("invalid RarVM filter range");
		}
		filters.push(PendingFilter {
			kind,
			start,
			size: block_size,
			channels: registers[0] as usize,
			file_offset: registers[6],
		});
		Ok(())
	}
}

struct VmBits<'a> {
	bytes:    &'a [u8],
	position: usize,
}

impl<'a> VmBits<'a> {
	const fn new(bytes: &'a [u8]) -> Self {
		Self { bytes, position: 0 }
	}

	fn read(&mut self, count: usize) -> Result<usize> {
		let mut value = 0usize;
		let mut left = count;
		while left > 0 {
			let byte_position = self.position >> 3;
			if byte_position >= self.bytes.len() {
				return invalid("truncated RarVM code");
			}
			let bit_position = self.position & 7;
			let take = left.min(8 - bit_position);
			value = (value << take)
				| usize::from(
					(self.bytes[byte_position] >> (8 - bit_position - take))
						& ((1u16 << take) - 1) as u8,
				);
			self.position += take;
			left -= take;
		}
		Ok(value)
	}

	fn read_data(&mut self) -> Result<usize> {
		match self.read(2)? {
			0 => self.read(4),
			1 => {
				let first = self.read(4)?;
				if first == 0 {
					Ok((self.read(8)? as u32 | 0xffff_ff00) as usize)
				} else {
					Ok(first * 16 + self.read(4)?)
				}
			},
			2 => self.read(16),
			_ => Ok((self.read(16)? << 16) + self.read(16)?),
		}
	}
}

fn distance_tables(slot: usize) -> Result<(usize, usize)> {
	let mut distance = 0usize;
	let mut current = 0usize;
	for (bits, &count) in DIST_COUNTS.iter().enumerate() {
		for _ in 0..count {
			if current == slot {
				return Ok((distance, bits));
			}
			distance += 1usize << bits;
			current += 1;
		}
	}
	invalid("invalid RAR4 distance slot")
}

fn remember_distance(distances: &mut [usize; 4], distance: usize) {
	distances.copy_within(0..3, 1);
	distances[0] = distance;
}

fn copy_match(
	output: &mut [u8],
	position: &mut usize,
	output_end: usize,
	dictionary_size: usize,
	distance: usize,
	length: usize,
) -> Result<()> {
	if distance == 0 || distance > dictionary_size || distance > *position {
		return invalid("invalid RAR4 LZ distance");
	}
	let length = length.min(output_end - *position);
	for index in 0..length {
		output[*position + index] = output[*position + index - distance];
	}
	*position += length;
	Ok(())
}

fn identify_filter(program: &[u8]) -> u8 {
	let checksum = crc32fast::hash(program);
	match (program.len(), checksum) {
		(53, 0xad57_6887) => 1,
		(57, 0x3cd7_e57e) => 2,
		(29, 0x0e06_077d) => 6,
		(120, 0x3769_893f) => 3,
		(149, 0x1c2c_5dc8) => 4,
		(216, 0xbc85_e701) => 5,
		(40, 0x46b9_c560) => 7,
		_ => 0,
	}
}

fn apply_filters(output: &mut [u8], filters: &[PendingFilter]) {
	for filter in filters {
		let data = &mut output[filter.start..filter.start + filter.size];
		if filter.kind == 6 {
			let source = data.to_vec();
			let mut source_position = 0usize;
			for channel in 0..filter.channels {
				let mut previous = 0u8;
				for position in (channel..data.len()).step_by(filter.channels) {
					previous = previous.wrapping_sub(source[source_position]);
					data[position] = previous;
					source_position += 1;
				}
			}
			continue;
		}
		let compare_opcode = if filter.kind == 2 { 0xe9 } else { 0xe8 };
		let mut position = 0usize;
		while position + 4 < data.len() {
			let opcode = data[position];
			position += 1;
			if opcode != 0xe8 && opcode != compare_opcode {
				continue;
			}
			let offset = (position as u32).wrapping_add(filter.file_offset);
			let address = read_i32(data, position);
			if address < 0 {
				if address.wrapping_add(offset as i32) >= 0 {
					write_i32(data, position, address.wrapping_add(0x1000000));
				}
			} else if address < 0x1000000 {
				write_i32(data, position, address.wrapping_sub(offset as i32));
			}
			position += 4;
		}
	}
}

fn read_length_table(bits: &mut Bits, previous: &[u8], size: usize) -> Result<Vec<u8>> {
	let mut pre_lengths = [0u8; 20];
	let mut index = 0usize;
	while index < pre_lengths.len() {
		let length = bits.read(4)? as u8;
		if length == 15 {
			let mut zero_count = bits.read(4)?;
			if zero_count != 0 {
				zero_count += 2;
				while zero_count > 0 && index < pre_lengths.len() {
					pre_lengths[index] = 0;
					index += 1;
					zero_count -= 1;
				}
				continue;
			}
		}
		pre_lengths[index] = length;
		index += 1;
	}
	let mut pre_table = Huffman::new(20);
	pre_table.build(&pre_lengths, 0, 20)?;
	let mut lengths = vec![0u8; size];
	let mut position = 0usize;
	while position < size {
		let symbol = pre_table.decode(bits)?;
		if symbol < 16 {
			lengths[position] = (symbol as u8).wrapping_add(previous[position]) & 15;
			position += 1;
		} else if symbol == 16 || symbol == 17 {
			let mut count =
				bits.read(if symbol == 16 { 3 } else { 7 })? + if symbol == 16 { 3 } else { 11 };
			let value = if position == 0 {
				0
			} else {
				lengths[position - 1]
			};
			while count > 0 && position < size {
				lengths[position] = value;
				position += 1;
				count -= 1;
			}
		} else {
			let mut count =
				bits.read(if symbol == 18 { 3 } else { 7 })? + if symbol == 18 { 3 } else { 11 };
			while count > 0 && position < size {
				lengths[position] = 0;
				position += 1;
				count -= 1;
			}
		}
	}
	Ok(lengths)
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
