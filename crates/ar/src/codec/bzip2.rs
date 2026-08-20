//! Bzip2 stream decoding with bounded output and integrity verification.

use crate::{Error, Limits, Result};

const BLOCK_MAGIC: [u8; 6] = [0x31, 0x41, 0x59, 0x26, 0x53, 0x59];
const STREAM_END_MAGIC: [u8; 6] = [0x17, 0x72, 0x45, 0x38, 0x50, 0x90];
const MAX_HUFFMAN_LENGTH: usize = 20;
const MAX_SELECTORS: usize = 18_002;
const GROUP_SIZE: usize = 50;

// Fixed randomization sequence from the original bzip2 format. Randomized
// blocks are obsolete, but remain part of the bitstream.
const RANDOM_NUMBERS: [u16; 512] = [
	619, 720, 127, 481, 931, 816, 813, 233, 566, 247, 985, 724, 205, 454, 863, 491, 741, 242, 949,
	214, 733, 859, 335, 708, 621, 574, 73, 654, 730, 472, 419, 436, 278, 496, 867, 210, 399, 680,
	480, 51, 878, 465, 811, 169, 869, 675, 611, 697, 867, 561, 862, 687, 507, 283, 482, 129, 807,
	591, 733, 623, 150, 238, 59, 379, 684, 877, 625, 169, 643, 105, 170, 607, 520, 932, 727, 476,
	693, 425, 174, 647, 73, 122, 335, 530, 442, 853, 695, 249, 445, 515, 909, 545, 703, 919, 874,
	474, 882, 500, 594, 612, 641, 801, 220, 162, 819, 984, 589, 513, 495, 799, 161, 604, 958, 533,
	221, 400, 386, 867, 600, 782, 382, 596, 414, 171, 516, 375, 682, 485, 911, 276, 98, 553, 163,
	354, 666, 933, 424, 341, 533, 870, 227, 730, 475, 186, 263, 647, 537, 686, 600, 224, 469, 68,
	770, 919, 190, 373, 294, 822, 808, 206, 184, 943, 795, 384, 383, 461, 404, 758, 839, 887, 715,
	67, 618, 276, 204, 918, 873, 777, 604, 560, 951, 160, 578, 722, 79, 804, 96, 409, 713, 940, 652,
	934, 970, 447, 318, 353, 859, 672, 112, 785, 645, 863, 803, 350, 139, 93, 354, 99, 820, 908,
	609, 772, 154, 274, 580, 184, 79, 626, 630, 742, 653, 282, 762, 623, 680, 81, 927, 626, 789,
	125, 411, 521, 938, 300, 821, 78, 343, 175, 128, 250, 170, 774, 972, 275, 999, 639, 495, 78,
	352, 126, 857, 956, 358, 619, 580, 124, 737, 594, 701, 612, 669, 112, 134, 694, 363, 992, 809,
	743, 168, 974, 944, 375, 748, 52, 600, 747, 642, 182, 862, 81, 344, 805, 988, 739, 511, 655,
	814, 334, 249, 515, 897, 955, 664, 981, 649, 113, 974, 459, 893, 228, 433, 837, 553, 268, 926,
	240, 102, 654, 459, 51, 686, 754, 806, 760, 493, 403, 415, 394, 687, 700, 946, 670, 656, 610,
	738, 392, 760, 799, 887, 653, 978, 321, 576, 617, 626, 502, 894, 679, 243, 440, 680, 879, 194,
	572, 640, 724, 926, 56, 204, 700, 707, 151, 457, 449, 797, 195, 791, 558, 945, 679, 297, 59, 87,
	824, 713, 663, 412, 693, 342, 606, 134, 108, 571, 364, 631, 212, 174, 643, 304, 329, 343, 97,
	430, 751, 497, 314, 983, 374, 822, 928, 140, 206, 73, 263, 980, 736, 876, 478, 430, 305, 170,
	514, 364, 692, 829, 82, 855, 953, 676, 246, 369, 970, 294, 750, 807, 827, 150, 790, 288, 923,
	804, 378, 215, 828, 592, 281, 565, 555, 710, 82, 896, 831, 547, 261, 524, 462, 293, 465, 502,
	56, 661, 821, 976, 991, 658, 869, 905, 758, 745, 193, 768, 550, 608, 933, 378, 286, 215, 979,
	792, 961, 61, 688, 793, 644, 986, 403, 106, 366, 905, 644, 372, 567, 466, 434, 645, 210, 389,
	550, 919, 135, 780, 773, 635, 389, 707, 100, 626, 958, 165, 504, 920, 176, 193, 713, 857, 265,
	203, 50, 668, 108, 645, 990, 626, 197, 510, 357, 358, 850, 858, 364, 936, 638,
];

const CRC_TABLE: [u32; 256] = make_crc_table();

const fn make_crc_table() -> [u32; 256] {
	let mut table = [0_u32; 256];
	let mut index = 0;
	while index < table.len() {
		let mut value = (index as u32) << 24;
		let mut bit = 0;
		while bit < 8 {
			value = if value & 0x8000_0000 != 0 {
				(value << 1) ^ 0x04c1_1db7
			} else {
				value << 1
			};
			bit += 1;
		}
		table[index] = value;
		index += 1;
	}
	table
}

#[inline]
fn update_crc(crc: u32, byte: u8) -> u32 {
	CRC_TABLE[usize::from(((crc >> 24) as u8) ^ byte)] ^ (crc << 8)
}

struct BitReader<'a> {
	bytes:        &'a [u8],
	bit_position: usize,
}

impl<'a> BitReader<'a> {
	const fn new(bytes: &'a [u8]) -> Self {
		Self { bytes, bit_position: 0 }
	}

	const fn done(&self) -> bool {
		self.bit_position == self.bytes.len().saturating_mul(8)
	}

	fn read_bit(&mut self) -> Result<u32> {
		let byte_index = self.bit_position >> 3;
		let Some(&byte) = self.bytes.get(byte_index) else {
			return Err(Error::InvalidArchive("truncated bzip2 stream"));
		};
		let bit = u32::from((byte >> (7 - (self.bit_position & 7))) & 1);
		self.bit_position += 1;
		Ok(bit)
	}

	fn read_bits(&mut self, count: usize) -> Result<u32> {
		let mut value = 0_u32;
		for _ in 0..count {
			value = (value << 1) | self.read_bit()?;
		}
		Ok(value)
	}

	fn read_u32(&mut self) -> Result<u32> {
		Ok((self.read_bits(16)? << 16) | self.read_bits(16)?)
	}

	fn read_marker(&mut self) -> Result<[u8; 6]> {
		let mut marker = [0_u8; 6];
		for byte in &mut marker {
			*byte = self.read_bits(8)? as u8;
		}
		Ok(marker)
	}

	fn align_to_byte(&mut self) -> Result<()> {
		while self.bit_position & 7 != 0 {
			if self.read_bit()? != 0 {
				return Err(Error::InvalidArchive("non-zero bzip2 stream padding"));
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

	fn ensure(&self, additional: usize) -> Result<()> {
		let actual = (self.bytes.len() as u64)
			.checked_add(additional as u64)
			.ok_or(Error::ArchiveTooLarge { actual: u64::MAX, limit: self.limit })?;
		if actual > self.limit {
			return Err(Error::ArchiveTooLarge { actual, limit: self.limit });
		}
		Ok(())
	}

	fn push(&mut self, byte: u8) -> Result<()> {
		self.ensure(1)?;
		self.bytes.push(byte);
		Ok(())
	}

	fn push_repeated(&mut self, byte: u8, count: usize) -> Result<()> {
		self.ensure(count)?;
		self.bytes.resize(self.bytes.len() + count, byte);
		Ok(())
	}
}

struct HuffmanTable {
	counts:      [u16; MAX_HUFFMAN_LENGTH + 1],
	first_codes: [u32; MAX_HUFFMAN_LENGTH + 1],
	offsets:     [u16; MAX_HUFFMAN_LENGTH + 1],
	symbols:     Vec<u16>,
}

impl HuffmanTable {
	fn new(lengths: &[u8]) -> Result<Self> {
		let mut counts = [0_u16; MAX_HUFFMAN_LENGTH + 1];
		for &length in lengths {
			if !(1..=MAX_HUFFMAN_LENGTH as u8).contains(&length) {
				return Err(Error::InvalidArchive("invalid bzip2 Huffman code length"));
			}
			counts[usize::from(length)] += 1;
		}

		let mut first_codes = [0_u32; MAX_HUFFMAN_LENGTH + 1];
		let mut offsets = [0_u16; MAX_HUFFMAN_LENGTH + 1];
		let mut code = 0_u32;
		let mut offset = 0_u16;
		for length in 1..=MAX_HUFFMAN_LENGTH {
			code = (code + u32::from(counts[length - 1])) * 2;
			if code + u32::from(counts[length]) > 1_u32 << length {
				return Err(Error::InvalidArchive("oversubscribed bzip2 Huffman table"));
			}
			first_codes[length] = code;
			offsets[length] = offset;
			offset += counts[length];
		}

		let mut symbols = vec![0_u16; lengths.len()];
		let mut next = offsets;
		for (symbol, &length) in lengths.iter().enumerate() {
			let slot = &mut next[usize::from(length)];
			symbols[usize::from(*slot)] = symbol as u16;
			*slot += 1;
		}
		Ok(Self { counts, first_codes, offsets, symbols })
	}

	fn decode(&self, reader: &mut BitReader<'_>) -> Result<usize> {
		let mut code = 0_u32;
		for length in 1..=MAX_HUFFMAN_LENGTH {
			code = (code << 1) | reader.read_bit()?;
			let first = self.first_codes[length];
			if code >= first {
				let relative = code - first;
				if relative < u32::from(self.counts[length]) {
					let index = usize::from(self.offsets[length]) + relative as usize;
					return Ok(usize::from(self.symbols[index]));
				}
			}
		}
		Err(Error::InvalidArchive("invalid bzip2 Huffman code"))
	}
}

fn read_stream_header(reader: &mut BitReader<'_>) -> Result<usize> {
	if reader.read_bits(8)? != 0x42 || reader.read_bits(8)? != 0x5a || reader.read_bits(8)? != 0x68 {
		return Err(Error::InvalidArchive("invalid bzip2 stream header"));
	}
	let level = reader.read_bits(8)?;
	if !(0x31..=0x39).contains(&level) {
		return Err(Error::InvalidArchive("invalid bzip2 block-size level"));
	}
	Ok((level as usize - 0x30) * 100_000)
}

fn read_huffman_tables(
	reader: &mut BitReader<'_>,
	used_bytes: &[u8],
) -> Result<(Vec<u8>, Vec<HuffmanTable>)> {
	let group_count = reader.read_bits(3)? as usize;
	if !(2..=6).contains(&group_count) {
		return Err(Error::InvalidArchive("invalid bzip2 Huffman group count"));
	}
	let selector_count = reader.read_bits(15)? as usize;
	if !(1..=MAX_SELECTORS).contains(&selector_count) {
		return Err(Error::InvalidArchive("invalid bzip2 selector count"));
	}

	let mut selectors = Vec::with_capacity(selector_count);
	let mut selector_mtf = [0_u8; 6];
	for (index, value) in selector_mtf[..group_count].iter_mut().enumerate() {
		*value = index as u8;
	}
	for _ in 0..selector_count {
		let mut position = 0;
		while reader.read_bit()? != 0 {
			position += 1;
			if position >= group_count {
				return Err(Error::InvalidArchive("invalid bzip2 selector MTF value"));
			}
		}
		let selector = selector_mtf[position];
		selectors.push(selector);
		selector_mtf.copy_within(..position, 1);
		selector_mtf[0] = selector;
	}

	let alpha_size = used_bytes.len() + 2;
	let mut tables = Vec::with_capacity(group_count);
	for _ in 0..group_count {
		let mut lengths = vec![0_u8; alpha_size];
		let mut length = reader.read_bits(5)? as i32;
		for value in &mut lengths {
			while reader.read_bit()? != 0 {
				length += if reader.read_bit()? == 0 { 1 } else { -1 };
				if !(1..=MAX_HUFFMAN_LENGTH as i32).contains(&length) {
					return Err(Error::InvalidArchive("invalid bzip2 Huffman code length"));
				}
			}
			if !(1..=MAX_HUFFMAN_LENGTH as i32).contains(&length) {
				return Err(Error::InvalidArchive("invalid bzip2 Huffman code length"));
			}
			*value = length as u8;
		}
		tables.push(HuffmanTable::new(&lengths)?);
	}
	Ok((selectors, tables))
}

fn next_symbol(
	reader: &mut BitReader<'_>,
	selectors: &[u8],
	tables: &[HuffmanTable],
	selector_index: &mut usize,
	group_remaining: &mut usize,
	table_index: &mut usize,
) -> Result<usize> {
	if *group_remaining == 0 {
		let Some(&selector) = selectors.get(*selector_index) else {
			return Err(Error::InvalidArchive("bzip2 block exhausted its Huffman selectors"));
		};
		*table_index = usize::from(selector);
		*selector_index += 1;
		*group_remaining = GROUP_SIZE;
	}
	*group_remaining -= 1;
	tables[*table_index].decode(reader)
}

fn decode_block_data(
	reader: &mut BitReader<'_>,
	block_size_limit: usize,
	used_bytes: &[u8],
	selectors: &[u8],
	tables: &[HuffmanTable],
) -> Result<Vec<u8>> {
	let mut mtf = [0_u8; 256];
	mtf[..used_bytes.len()].copy_from_slice(used_bytes);
	let mut block = Vec::with_capacity(block_size_limit);
	let mut selector_index = 0;
	let mut group_remaining = 0;
	let mut table_index = 0;
	let end_symbol = used_bytes.len() + 1;

	let mut symbol = next_symbol(
		reader,
		selectors,
		tables,
		&mut selector_index,
		&mut group_remaining,
		&mut table_index,
	)?;
	while symbol != end_symbol {
		if symbol == 0 || symbol == 1 {
			let mut run_length = 0_usize;
			let mut power = 1_usize;
			loop {
				run_length = run_length
					.checked_add(if symbol == 0 {
						power
					} else {
						power.saturating_mul(2)
					})
					.ok_or(Error::InvalidArchive("invalid bzip2 RLE run length"))?;
				if run_length > block_size_limit || power > block_size_limit {
					return Err(Error::InvalidArchive("invalid bzip2 RLE run length"));
				}
				power = power.saturating_mul(2);
				symbol = next_symbol(
					reader,
					selectors,
					tables,
					&mut selector_index,
					&mut group_remaining,
					&mut table_index,
				)?;
				if symbol != 0 && symbol != 1 {
					break;
				}
			}
			let needed = block
				.len()
				.checked_add(run_length)
				.ok_or(Error::InvalidArchive("bzip2 block exceeds its block-size level"))?;
			if needed > block_size_limit {
				return Err(Error::InvalidArchive("bzip2 block exceeds its block-size level"));
			}
			block.resize(needed, mtf[0]);
			if symbol == end_symbol {
				break;
			}
		}

		let Some(position) = symbol.checked_sub(1) else {
			return Err(Error::InvalidArchive("invalid bzip2 MTF symbol"));
		};
		if position >= used_bytes.len() {
			return Err(Error::InvalidArchive("invalid bzip2 MTF symbol"));
		}
		let byte = mtf[position];
		mtf.copy_within(..position, 1);
		mtf[0] = byte;
		if block.len() == block_size_limit {
			return Err(Error::InvalidArchive("bzip2 block exceeds its block-size level"));
		}
		block.push(byte);
		symbol = next_symbol(
			reader,
			selectors,
			tables,
			&mut selector_index,
			&mut group_remaining,
			&mut table_index,
		)?;
	}
	Ok(block)
}

fn inverse_bwt(block: &[u8], original_pointer: usize) -> Result<Vec<u8>> {
	if block.is_empty() || original_pointer >= block.len() {
		return Err(Error::InvalidArchive("invalid bzip2 BWT origin pointer"));
	}
	let mut counts = [0_usize; 257];
	for &byte in block {
		counts[usize::from(byte) + 1] += 1;
	}
	for index in 1..counts.len() {
		counts[index] += counts[index - 1];
	}
	let mut positions = [0_usize; 256];
	positions.copy_from_slice(&counts[..256]);
	let mut next = vec![0_u32; block.len()];
	for (index, &byte) in block.iter().enumerate() {
		let position = &mut positions[usize::from(byte)];
		next[*position] = index as u32;
		*position += 1;
	}
	let mut decoded = vec![0_u8; block.len()];
	let mut position = next[original_pointer] as usize;
	for byte in &mut decoded {
		*byte = block[position];
		position = next[position] as usize;
	}
	Ok(decoded)
}

fn append_rle1(decoded: &[u8], randomized: bool, output: &mut BoundedOutput) -> Result<u32> {
	let mut crc = u32::MAX;
	let mut previous = 0_u8;
	let mut has_previous = false;
	let mut repetitions = 0_usize;
	let mut random_index = 0_usize;
	let mut random_remaining = 0_u16;

	for &encoded_byte in decoded {
		let mut byte = encoded_byte;
		if randomized {
			if random_remaining == 0 {
				random_remaining = RANDOM_NUMBERS[random_index];
				random_index = (random_index + 1) & 511;
			}
			random_remaining -= 1;
			if random_remaining == 1 {
				byte ^= 1;
			}
		}

		if repetitions == 4 {
			output.push_repeated(previous, usize::from(byte))?;
			for _ in 0..byte {
				crc = update_crc(crc, previous);
			}
			repetitions = 0;
			continue;
		}
		output.push(byte)?;
		crc = update_crc(crc, byte);
		if has_previous && byte == previous {
			repetitions += 1;
		} else {
			previous = byte;
			has_previous = true;
			repetitions = 1;
		}
	}
	if repetitions == 4 {
		return Err(Error::InvalidArchive("truncated bzip2 RLE run"));
	}
	Ok(crc ^ u32::MAX)
}

/// Decompresses concatenated bzip2 streams bounded by `limits.archive_size`.
pub fn bzip2_decompress(bytes: &[u8], limits: Limits) -> Result<Vec<u8>> {
	let mut reader = BitReader::new(bytes);
	let mut output = BoundedOutput::new(limits.archive_size, bytes.len());

	loop {
		let block_size_limit = read_stream_header(&mut reader)?;
		let mut combined_crc = 0_u32;
		loop {
			let marker = reader.read_marker()?;
			if marker == STREAM_END_MAGIC {
				let expected = reader.read_u32()?;
				if combined_crc != expected {
					return Err(Error::InvalidArchive("bzip2 stream CRC mismatch"));
				}
				reader.align_to_byte()?;
				break;
			}
			if marker != BLOCK_MAGIC {
				return Err(Error::InvalidArchive("invalid bzip2 block marker"));
			}

			let expected_block_crc = reader.read_u32()?;
			let randomized = reader.read_bit()? != 0;
			let original_pointer = reader.read_bits(24)? as usize;
			let used_ranges = reader.read_bits(16)? as u16;
			let mut used = [0_u8; 256];
			let mut used_count = 0;
			for range in 0..16 {
				if used_ranges & (0x8000_u16 >> range) == 0 {
					continue;
				}
				for low in 0..16 {
					if reader.read_bit()? != 0 {
						used[used_count] = (range * 16 + low) as u8;
						used_count += 1;
					}
				}
			}
			if used_count == 0 {
				return Err(Error::InvalidArchive("bzip2 block has an empty symbol map"));
			}
			let used_bytes = &used[..used_count];
			let (selectors, tables) = read_huffman_tables(&mut reader, used_bytes)?;
			let block =
				decode_block_data(&mut reader, block_size_limit, used_bytes, &selectors, &tables)?;
			let decoded = inverse_bwt(&block, original_pointer)?;
			let actual_block_crc = append_rle1(&decoded, randomized, &mut output)?;
			if actual_block_crc != expected_block_crc {
				return Err(Error::InvalidArchive("bzip2 block CRC mismatch"));
			}
			combined_crc = combined_crc.rotate_left(1) ^ actual_block_crc;
		}
		if reader.done() {
			return Ok(output.bytes);
		}
	}
}
