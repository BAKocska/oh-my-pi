use crate::{Error, Result};

const FRAME_SIZE: usize = 32 * 1024;
const MIN_MATCH: usize = 2;
const NUM_PRIMARY_LENGTHS: usize = 7;
const NUM_SECONDARY_LENGTHS: usize = 249;
const POSITION_SLOTS: [usize; 7] = [30, 32, 34, 36, 38, 42, 50];

struct BitReader<'a> {
	bytes:     &'a [u8],
	offset:    usize,
	word:      u16,
	remaining: u8,
}

impl<'a> BitReader<'a> {
	const fn new(bytes: &'a [u8]) -> Self {
		Self { bytes, offset: 0, word: 0, remaining: 0 }
	}

	fn read_bits(&mut self, count: u8) -> Result<u32> {
		let mut value = 0_u32;
		let mut needed = count;
		while needed > 0 {
			if self.remaining == 0 {
				let word = self
					.bytes
					.get(self.offset..self.offset + 2)
					.ok_or(Error::InvalidArchive("truncated CAB LZX bitstream"))?;
				self.word = u16::from_le_bytes([word[0], word[1]]);
				self.offset += 2;
				self.remaining = 16;
			}
			let take = needed.min(self.remaining);
			let mask = (1_u32 << take) - 1;
			value = (value << take) | (u32::from(self.word) >> (self.remaining - take) & mask);
			self.remaining -= take;
			needed -= take;
		}
		Ok(value)
	}

	const fn align_word(&mut self) {
		self.remaining = 0;
	}

	fn read_byte(&mut self) -> Result<u8> {
		if self.remaining != 0 {
			return Err(Error::InvalidArchive("misaligned CAB LZX byte stream"));
		}
		let byte = *self
			.bytes
			.get(self.offset)
			.ok_or(Error::InvalidArchive("truncated CAB LZX data"))?;
		self.offset += 1;
		Ok(byte)
	}

	fn read_u32_le(&mut self) -> Result<u32> {
		Ok(u32::from_le_bytes([
			self.read_byte()?,
			self.read_byte()?,
			self.read_byte()?,
			self.read_byte()?,
		]))
	}
}

struct HuffmanTable {
	counts:        [u32; 17],
	first_codes:   [u32; 17],
	first_symbols: [u32; 17],
	symbols:       Vec<u16>,
	empty:         bool,
}

impl HuffmanTable {
	fn new(lengths: &[u8], allow_empty: bool) -> Result<Self> {
		let mut counts = [0_u32; 17];
		let mut symbol_count = 0_usize;
		for &length in lengths {
			if length > 16 {
				return Err(Error::InvalidArchive("invalid CAB LZX Huffman code length"));
			}
			if length != 0 {
				counts[usize::from(length)] += 1;
				symbol_count += 1;
			}
		}
		let empty = symbol_count == 0;
		if empty && !allow_empty {
			return Err(Error::InvalidArchive("empty CAB LZX Huffman tree"));
		}

		let mut first_codes = [0_u32; 17];
		let mut first_symbols = [0_u32; 17];
		let mut code = 0_u32;
		let mut symbol_offset = 0_u32;
		for length in 1..=16 {
			code = (code + counts[length - 1]) * 2;
			if code + counts[length] > 1_u32 << length {
				return Err(Error::InvalidArchive("oversubscribed CAB LZX Huffman tree"));
			}
			first_codes[length] = code;
			first_symbols[length] = symbol_offset;
			symbol_offset += counts[length];
		}

		let mut symbols = vec![0_u16; symbol_count];
		let mut next = first_symbols;
		for (symbol, &length) in lengths.iter().enumerate() {
			if length != 0 {
				let slot = &mut next[usize::from(length)];
				symbols[*slot as usize] = u16::try_from(symbol)
					.map_err(|_| Error::InvalidArchive("CAB LZX tree has too many symbols"))?;
				*slot += 1;
			}
		}
		Ok(Self { counts, first_codes, first_symbols, symbols, empty })
	}

	fn decode(&self, reader: &mut BitReader<'_>) -> Result<usize> {
		if self.empty {
			return Err(Error::InvalidArchive("CAB LZX stream uses an empty Huffman tree"));
		}
		let mut code = 0_u32;
		for length in 1..=16 {
			code = code * 2 + reader.read_bits(1)?;
			let first = self.first_codes[length];
			if code >= first {
				let relative = code - first;
				if relative < self.counts[length] {
					return Ok(usize::from(
						self.symbols[self.first_symbols[length] as usize + relative as usize],
					));
				}
			}
		}
		Err(Error::InvalidArchive("invalid CAB LZX Huffman symbol"))
	}
}

fn read_code_lengths(
	reader: &mut BitReader<'_>,
	lengths: &mut [u8],
	first: usize,
	last: usize,
) -> Result<()> {
	let mut pretree_lengths = [0_u8; 20];
	for length in &mut pretree_lengths {
		*length = reader.read_bits(4)? as u8;
	}
	let pretree = HuffmanTable::new(&pretree_lengths, false)?;
	let mut index = first;
	while index < last {
		let symbol = pretree.decode(reader)?;
		if symbol == 17 || symbol == 18 {
			let (bits, base) = if symbol == 17 { (4, 4) } else { (5, 20) };
			let run = reader.read_bits(bits)? as usize + base;
			if index + run > last {
				return Err(Error::InvalidArchive("CAB LZX code-length run exceeds its tree"));
			}
			lengths[index..index + run].fill(0);
			index += run;
			continue;
		}
		if symbol == 19 {
			let run = reader.read_bits(1)? as usize + 4;
			if index + run > last {
				return Err(Error::InvalidArchive("CAB LZX code-length run exceeds its tree"));
			}
			let delta = pretree.decode(reader)? as u8;
			let length = (lengths[index] + 17 - delta) % 17;
			lengths[index..index + run].fill(length);
			index += run;
			continue;
		}
		lengths[index] = (usize::from(lengths[index]) + 17 - symbol) as u8 % 17;
		index += 1;
	}
	Ok(())
}

pub(super) struct Decoder {
	window:               Vec<u8>,
	position_base:        Vec<u32>,
	extra_bits:           Vec<u8>,
	main_lengths:         Vec<u8>,
	length_lengths:       [u8; NUM_SECONDARY_LENGTHS],
	main_table:           Option<HuffmanTable>,
	length_table:         Option<HuffmanTable>,
	aligned_table:        Option<HuffmanTable>,
	window_position:      usize,
	decoded_size:         u64,
	frame:                u32,
	r0:                   usize,
	r1:                   usize,
	r2:                   usize,
	header_read:          bool,
	intel_file_size:      i32,
	intel_started:        bool,
	block_type:           u8,
	block_length:         usize,
	block_remaining:      usize,
	uncompressed_padding: bool,
}

impl Decoder {
	pub(super) fn new(window_bits: u8) -> Result<Self> {
		if !(15..=21).contains(&window_bits) {
			return Err(Error::UnsupportedCabLzxWindow { bits: window_bits });
		}
		let slots = POSITION_SLOTS[usize::from(window_bits - 15)];
		let mut extra_bits = vec![0_u8; slots];
		let mut position_base = vec![0_u32; slots];
		for slot in 0..slots {
			extra_bits[slot] = if slot < 4 {
				0
			} else {
				((slot / 2) - 1).min(17) as u8
			};
			if slot > 0 {
				position_base[slot] = position_base[slot - 1] + (1_u32 << extra_bits[slot - 1]);
			}
		}
		Ok(Self {
			window: vec![0; 1_usize << window_bits],
			position_base,
			extra_bits,
			main_lengths: vec![0; 256 + slots * 8],
			length_lengths: [0; NUM_SECONDARY_LENGTHS],
			main_table: None,
			length_table: None,
			aligned_table: None,
			window_position: 0,
			decoded_size: 0,
			frame: 0,
			r0: 1,
			r1: 1,
			r2: 1,
			header_read: false,
			intel_file_size: 0,
			intel_started: false,
			block_type: 0,
			block_length: 0,
			block_remaining: 0,
			uncompressed_padding: false,
		})
	}

	pub(super) fn decompress_frame(&mut self, bytes: &[u8], output: &mut [u8]) -> Result<()> {
		let output_size = output.len();
		if output_size > FRAME_SIZE {
			return Err(Error::InvalidArchive("CAB LZX frame exceeds 32768 bytes"));
		}
		if output_size == 0 {
			return Ok(());
		}
		let mut reader = BitReader::new(bytes);
		if !self.header_read {
			if reader.read_bits(1)? != 0 {
				let high = reader.read_bits(16)?;
				let low = reader.read_bits(16)?;
				self.intel_file_size = ((high << 16) | low) as i32;
			}
			self.header_read = true;
		}

		let mut output_position = 0;
		while output_position < output_size {
			if self.block_remaining == 0 {
				self.read_block_header(&mut reader)?;
			}
			let run = self.block_remaining.min(output_size - output_position);
			let produced = self.decode_run(&mut reader, output, output_position, run)?;
			if produced != run {
				return Err(Error::InvalidArchive("CAB LZX block produced the wrong byte count"));
			}
			output_position += produced;
			self.block_remaining -= produced;
		}
		if self.block_remaining == 0 && self.block_type == 3 && self.uncompressed_padding {
			reader.read_byte()?;
			self.uncompressed_padding = false;
		}
		reader.align_word();
		self.translate_e8(output);
		self.frame += 1;
		Ok(())
	}

	fn read_block_header(&mut self, reader: &mut BitReader<'_>) -> Result<()> {
		if self.block_type == 3 && self.uncompressed_padding {
			reader.read_byte()?;
		}
		self.uncompressed_padding = false;
		self.block_type = reader.read_bits(3)? as u8;
		self.block_length = reader.read_bits(16)? as usize * 256 + reader.read_bits(8)? as usize;
		if self.block_length == 0 {
			return Err(Error::InvalidArchive("zero-length CAB LZX block"));
		}
		self.block_remaining = self.block_length;

		if self.block_type == 1 || self.block_type == 2 {
			if self.block_type == 2 {
				let mut aligned_lengths = [0_u8; 8];
				for length in &mut aligned_lengths {
					*length = reader.read_bits(3)? as u8;
				}
				self.aligned_table = Some(HuffmanTable::new(&aligned_lengths, false)?);
			}
			read_code_lengths(reader, &mut self.main_lengths, 0, 256)?;
			let main_len = self.main_lengths.len();
			read_code_lengths(reader, &mut self.main_lengths, 256, main_len)?;
			self.main_table = Some(HuffmanTable::new(&self.main_lengths, false)?);
			if self.main_lengths[0xe8] != 0 {
				self.intel_started = true;
			}
			read_code_lengths(reader, &mut self.length_lengths, 0, NUM_SECONDARY_LENGTHS)?;
			self.length_table = Some(HuffmanTable::new(&self.length_lengths, true)?);
			return Ok(());
		}
		if self.block_type == 3 {
			self.intel_started = true;
			reader.align_word();
			self.r0 = reader.read_u32_le()? as usize;
			self.r1 = reader.read_u32_le()? as usize;
			self.r2 = reader.read_u32_le()? as usize;
			if self.r0 == 0 || self.r1 == 0 || self.r2 == 0 {
				return Err(Error::InvalidArchive("invalid CAB LZX repeated offset"));
			}
			self.uncompressed_padding = self.block_length & 1 != 0;
			return Ok(());
		}
		Err(Error::InvalidArchive("unsupported CAB LZX block type"))
	}

	fn decode_run(
		&mut self,
		reader: &mut BitReader<'_>,
		output: &mut [u8],
		output_start: usize,
		count: usize,
	) -> Result<usize> {
		if self.block_type == 3 {
			for index in 0..count {
				let value = reader.read_byte()?;
				self.write_byte(value, output, output_start + index);
			}
			return Ok(count);
		}
		if self.main_table.is_none() || self.length_table.is_none() {
			return Err(Error::InvalidArchive("missing CAB LZX decode trees"));
		}

		let mut produced = 0;
		while produced < count {
			let element = self
				.main_table
				.as_ref()
				.expect("checked above")
				.decode(reader)?;
			if element < 256 {
				self.write_byte(element as u8, output, output_start + produced);
				produced += 1;
				continue;
			}

			let match_element = element - 256;
			let mut match_length = match_element & NUM_PRIMARY_LENGTHS;
			if match_length == NUM_PRIMARY_LENGTHS {
				match_length += self
					.length_table
					.as_ref()
					.expect("checked above")
					.decode(reader)?;
			}
			match_length += MIN_MATCH;
			if match_length > count - produced || match_length > self.block_remaining - produced {
				return Err(Error::InvalidArchive("CAB LZX match crosses a frame or block boundary"));
			}

			let slot = match_element >> 3;
			let match_offset = if slot == 0 {
				self.r0
			} else if slot == 1 {
				std::mem::swap(&mut self.r1, &mut self.r0);
				self.r0
			} else if slot == 2 {
				std::mem::swap(&mut self.r2, &mut self.r0);
				self.r0
			} else {
				let (&base, &extra) = self
					.position_base
					.get(slot)
					.zip(self.extra_bits.get(slot))
					.ok_or(Error::InvalidArchive("CAB LZX position slot is out of range"))?;
				let mut offset = base as usize - 2;
				if self.block_type == 2 && extra >= 3 {
					if extra > 3 {
						offset += reader.read_bits(extra - 3)? as usize * 8;
					}
					offset += self
						.aligned_table
						.as_ref()
						.ok_or(Error::InvalidArchive("missing CAB LZX aligned tree"))?
						.decode(reader)?;
				} else if extra != 0 {
					offset += reader.read_bits(extra)? as usize;
				}
				self.r2 = self.r1;
				self.r1 = self.r0;
				self.r0 = offset;
				offset
			};

			if match_offset == 0
				|| match_offset
					> usize::try_from(self.decoded_size)
						.unwrap_or(usize::MAX)
						.min(self.window.len())
			{
				return Err(Error::InvalidArchive("CAB LZX match offset exceeds available history"));
			}
			for index in 0..match_length {
				let source =
					(self.window_position + self.window.len() - match_offset) % self.window.len();
				let value = self.window[source];
				self.write_byte(value, output, output_start + produced + index);
			}
			produced += match_length;
		}
		Ok(produced)
	}

	fn write_byte(&mut self, value: u8, output: &mut [u8], output_position: usize) {
		output[output_position] = value;
		self.window[self.window_position] = value;
		self.window_position = (self.window_position + 1) % self.window.len();
		self.decoded_size += 1;
	}

	fn translate_e8(&self, raw: &mut [u8]) {
		if !self.intel_started || self.intel_file_size == 0 || self.frame >= 32_768 || raw.len() <= 10
		{
			return;
		}
		let mut position = 0;
		let mut current = self.decoded_size as i64 - raw.len() as i64;
		let end = raw.len() - 10;
		while position < end {
			if raw[position] != 0xe8 {
				position += 1;
				current += 1;
				continue;
			}
			position += 1;
			let absolute =
				i32::from_le_bytes(raw[position..position + 4].try_into().expect("fixed range")) as i64;
			let file_size = i64::from(self.intel_file_size);
			if absolute >= -current && absolute < file_size {
				let relative = if absolute >= 0 {
					absolute - current
				} else {
					absolute + file_size
				};
				raw[position..position + 4].copy_from_slice(&(relative as i32).to_le_bytes());
			}
			position += 4;
			current += 5;
		}
	}
}
