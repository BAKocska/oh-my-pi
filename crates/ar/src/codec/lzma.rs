//! Bounded LZMA1, LZMA2, and LZMA-alone decoding.

use crate::{Error, Limits, Result};

const TOP_VALUE: u32 = 1 << 24;
const BIT_MODEL_TOTAL: u16 = 1 << 11;
const MOVE_BITS: u16 = 5;
const LITERAL_SIZE: usize = 0x300;
const NUM_STATES: usize = 12;
const NUM_POS_STATES_MAX: usize = 16;
const MATCH_MIN_LEN: usize = 2;

fn invalid(message: &'static str) -> Error {
	Error::InvalidArchive(message)
}

struct RangeDecoder<'a> {
	bytes: &'a [u8],
	pos:   usize,
	range: u32,
	code:  u32,
}

impl<'a> RangeDecoder<'a> {
	fn new(bytes: &'a [u8]) -> Result<Self> {
		if bytes.len() < 5 || bytes[0] != 0 {
			return Err(invalid("invalid LZMA range-coded stream"));
		}
		let mut decoder = Self { bytes, pos: 0, range: u32::MAX, code: 0 };
		for _ in 0..5 {
			decoder.code = decoder.code.wrapping_shl(8) | u32::from(bytes[decoder.pos]);
			decoder.pos += 1;
		}
		Ok(decoder)
	}

	#[inline]
	fn normalize(&mut self) -> Result<()> {
		if self.range < TOP_VALUE {
			let byte = *self
				.bytes
				.get(self.pos)
				.ok_or_else(|| invalid("truncated LZMA range data"))?;
			self.pos += 1;
			self.range <<= 8;
			self.code = self.code.wrapping_shl(8) | u32::from(byte);
		}
		Ok(())
	}

	#[inline]
	fn decode_bit(&mut self, probabilities: &mut [u16], index: usize) -> Result<u32> {
		self.normalize()?;
		let probability = probabilities[index];
		let bound = (self.range >> 11).wrapping_mul(u32::from(probability));
		if self.code < bound {
			self.range = bound;
			probabilities[index] = probability + ((BIT_MODEL_TOTAL - probability) >> MOVE_BITS);
			Ok(0)
		} else {
			self.range = self.range.wrapping_sub(bound);
			self.code = self.code.wrapping_sub(bound);
			probabilities[index] = probability - (probability >> MOVE_BITS);
			Ok(1)
		}
	}

	fn decode_direct_bits(&mut self, count: usize) -> Result<u32> {
		let mut result = 0_u32;
		for _ in 0..count {
			self.normalize()?;
			self.range >>= 1;
			let bit = if self.code >= self.range {
				self.code = self.code.wrapping_sub(self.range);
				1
			} else {
				0
			};
			result = result.wrapping_shl(1) | bit;
		}
		Ok(result)
	}
}

fn probabilities(size: usize) -> Vec<u16> {
	vec![BIT_MODEL_TOTAL >> 1; size]
}

fn decode_tree(
	range: &mut RangeDecoder<'_>,
	probabilities: &mut [u16],
	offset: usize,
	bits: usize,
) -> Result<u32> {
	let mut symbol = 1_usize;
	for _ in 0..bits {
		symbol = (symbol << 1) | range.decode_bit(probabilities, offset + symbol)? as usize;
	}
	Ok((symbol - (1 << bits)) as u32)
}

fn decode_reverse_tree(
	range: &mut RangeDecoder<'_>,
	probabilities: &mut [u16],
	offset: usize,
	bits: usize,
) -> Result<u32> {
	let mut symbol = 1_usize;
	let mut result = 0_u32;
	for bit_index in 0..bits {
		let bit = range.decode_bit(probabilities, offset + symbol)?;
		symbol = (symbol << 1) | bit as usize;
		result |= bit << bit_index;
	}
	Ok(result)
}

struct LengthDecoder {
	choice: Vec<u16>,
	low:    Vec<u16>,
	mid:    Vec<u16>,
	high:   Vec<u16>,
}

impl LengthDecoder {
	fn new() -> Self {
		Self {
			choice: probabilities(2),
			low:    probabilities(NUM_POS_STATES_MAX << 3),
			mid:    probabilities(NUM_POS_STATES_MAX << 3),
			high:   probabilities(1 << 8),
		}
	}

	fn reset(&mut self) {
		self.choice.fill(BIT_MODEL_TOTAL >> 1);
		self.low.fill(BIT_MODEL_TOTAL >> 1);
		self.mid.fill(BIT_MODEL_TOTAL >> 1);
		self.high.fill(BIT_MODEL_TOTAL >> 1);
	}

	fn decode(&mut self, range: &mut RangeDecoder<'_>, pos_state: usize) -> Result<usize> {
		if range.decode_bit(&mut self.choice, 0)? == 0 {
			return Ok(decode_tree(range, &mut self.low, pos_state << 3, 3)? as usize);
		}
		if range.decode_bit(&mut self.choice, 1)? == 0 {
			return Ok(8 + decode_tree(range, &mut self.mid, pos_state << 3, 3)? as usize);
		}
		Ok(16 + decode_tree(range, &mut self.high, 0, 8)? as usize)
	}
}

#[derive(Clone, Copy)]
struct LzmaProperties {
	lc:              usize,
	lp:              usize,
	pb:              usize,
	dictionary_size: usize,
}

fn parse_properties(properties: &[u8]) -> Result<LzmaProperties> {
	if properties.len() != 5 {
		return Err(invalid("invalid LZMA properties length"));
	}
	let mut packed = usize::from(properties[0]);
	if packed >= 9 * 5 * 5 {
		return Err(invalid("invalid LZMA properties"));
	}
	let lc = packed % 9;
	packed /= 9;
	let lp = packed % 5;
	let pb = packed / 5;
	let dictionary_size = u32::from_le_bytes(properties[1..5].try_into().expect("slice length"));
	Ok(LzmaProperties {
		lc,
		lp,
		pb,
		dictionary_size: usize::try_from(dictionary_size.max(4096)).unwrap_or(usize::MAX),
	})
}

struct LzmaDecoder {
	output:           Vec<u8>,
	output_pos:       usize,
	dictionary_size:  usize,
	dictionary_start: usize,
	processed:        usize,
	lc:               usize,
	lp:               usize,
	pb:               usize,
	state:            usize,
	rep0:             usize,
	rep1:             usize,
	rep2:             usize,
	rep3:             usize,
	literal:          Vec<u16>,
	is_match:         Vec<u16>,
	is_rep:           Vec<u16>,
	is_rep_g0:        Vec<u16>,
	is_rep_g1:        Vec<u16>,
	is_rep_g2:        Vec<u16>,
	is_rep0_long:     Vec<u16>,
	pos_slot:         Vec<u16>,
	pos_decoders:     Vec<u16>,
	align:            Vec<u16>,
	len:              LengthDecoder,
	rep_len:          LengthDecoder,
}

impl LzmaDecoder {
	fn new(output_size: usize, properties: LzmaProperties) -> Result<Self> {
		let mut decoder = Self {
			output:           vec![0; output_size],
			output_pos:       0,
			dictionary_size:  properties.dictionary_size,
			dictionary_start: 0,
			processed:        0,
			lc:               0,
			lp:               0,
			pb:               0,
			state:            0,
			rep0:             1,
			rep1:             1,
			rep2:             1,
			rep3:             1,
			literal:          Vec::new(),
			is_match:         probabilities(NUM_STATES * NUM_POS_STATES_MAX),
			is_rep:           probabilities(NUM_STATES),
			is_rep_g0:        probabilities(NUM_STATES),
			is_rep_g1:        probabilities(NUM_STATES),
			is_rep_g2:        probabilities(NUM_STATES),
			is_rep0_long:     probabilities(NUM_STATES * NUM_POS_STATES_MAX),
			pos_slot:         probabilities(4 << 6),
			pos_decoders:     probabilities(115),
			align:            probabilities(16),
			len:              LengthDecoder::new(),
			rep_len:          LengthDecoder::new(),
		};
		decoder.set_properties(properties.lc, properties.lp, properties.pb)?;
		decoder.reset_dictionary();
		Ok(decoder)
	}

	fn set_properties(&mut self, lc: usize, lp: usize, pb: usize) -> Result<()> {
		if lc > 8 || lp > 4 || pb > 4 || lc + lp > 12 {
			return Err(invalid("invalid LZMA properties"));
		}
		self.lc = lc;
		self.lp = lp;
		self.pb = pb;
		let literal_count = LITERAL_SIZE << (lc + lp);
		if self.literal.len() != literal_count {
			self.literal = probabilities(literal_count);
		}
		Ok(())
	}

	fn reset_dictionary(&mut self) {
		self.dictionary_start = self.output_pos;
		self.processed = 0;
		self.reset_state();
	}

	fn reset_state(&mut self) {
		self.state = 0;
		self.rep0 = 1;
		self.rep1 = 1;
		self.rep2 = 1;
		self.rep3 = 1;
		self.literal.fill(BIT_MODEL_TOTAL >> 1);
		self.is_match.fill(BIT_MODEL_TOTAL >> 1);
		self.is_rep.fill(BIT_MODEL_TOTAL >> 1);
		self.is_rep_g0.fill(BIT_MODEL_TOTAL >> 1);
		self.is_rep_g1.fill(BIT_MODEL_TOTAL >> 1);
		self.is_rep_g2.fill(BIT_MODEL_TOTAL >> 1);
		self.is_rep0_long.fill(BIT_MODEL_TOTAL >> 1);
		self.pos_slot.fill(BIT_MODEL_TOTAL >> 1);
		self.pos_decoders.fill(BIT_MODEL_TOTAL >> 1);
		self.align.fill(BIT_MODEL_TOTAL >> 1);
		self.len.reset();
		self.rep_len.reset();
	}

	fn append_uncompressed(&mut self, bytes: &[u8], reset_dictionary: bool) -> Result<()> {
		if reset_dictionary {
			self.reset_dictionary();
		}
		let end = self
			.output_pos
			.checked_add(bytes.len())
			.filter(|end| *end <= self.output.len())
			.ok_or_else(|| invalid("LZMA2 output exceeds its limit"))?;
		self.output[self.output_pos..end].copy_from_slice(bytes);
		self.output_pos = end;
		self.processed += bytes.len();
		Ok(())
	}

	fn assert_distance(&self, distance: usize) -> Result<()> {
		let available = self.output_pos - self.dictionary_start;
		if distance == 0 || distance > available || distance > self.dictionary_size {
			return Err(invalid("invalid LZMA match distance"));
		}
		Ok(())
	}

	fn copy_match(&mut self, distance: usize, length: usize, limit: usize) -> Result<()> {
		self.assert_distance(distance)?;
		if length > limit - self.output_pos {
			return Err(invalid("LZMA match exceeds its declared output size"));
		}
		for _ in 0..length {
			self.output[self.output_pos] = self.output[self.output_pos - distance];
			self.output_pos += 1;
		}
		self.processed += length;
		Ok(())
	}

	fn decode_chunk(
		&mut self,
		bytes: &[u8],
		output_size: usize,
		allow_end_marker: bool,
	) -> Result<bool> {
		let limit = self
			.output_pos
			.checked_add(output_size)
			.filter(|limit| *limit <= self.output.len())
			.ok_or_else(|| invalid("LZMA output exceeds its declared size"))?;
		let mut range = RangeDecoder::new(bytes)?;
		let pos_mask = (1 << self.pb) - 1;
		let literal_pos_mask = (1 << self.lp) - 1;

		while self.output_pos < limit || allow_end_marker {
			let pos_state = self.processed & pos_mask;
			if range.decode_bit(&mut self.is_match, (self.state << 4) + pos_state)? == 0 {
				let previous = if self.output_pos == self.dictionary_start {
					0
				} else {
					self.output[self.output_pos - 1]
				};
				let context = (((self.processed & literal_pos_mask) << self.lc)
					+ (usize::from(previous) >> (8 - self.lc)))
					* LITERAL_SIZE;
				let mut symbol = 1_usize;
				if self.state >= 7 {
					self.assert_distance(self.rep0)?;
					let mut match_byte = self.output[self.output_pos - self.rep0];
					while symbol < 0x100 {
						let match_bit = usize::from(match_byte >> 7);
						match_byte <<= 1;
						let bit = range
							.decode_bit(&mut self.literal, context + ((1 + match_bit) << 8) + symbol)?
							as usize;
						symbol = (symbol << 1) | bit;
						if match_bit != bit {
							break;
						}
					}
				}
				while symbol < 0x100 {
					symbol =
						(symbol << 1) | range.decode_bit(&mut self.literal, context + symbol)? as usize;
				}
				if self.output_pos >= limit {
					return Err(invalid("LZMA output exceeds its size limit before end marker"));
				}
				self.output[self.output_pos] = symbol as u8;
				self.output_pos += 1;
				self.processed += 1;
				self.state = if self.state < 4 {
					0
				} else if self.state < 10 {
					self.state - 3
				} else {
					self.state - 6
				};
				continue;
			}

			let length;
			if range.decode_bit(&mut self.is_rep, self.state)? == 1 {
				if range.decode_bit(&mut self.is_rep_g0, self.state)? == 0 {
					if range.decode_bit(&mut self.is_rep0_long, (self.state << 4) + pos_state)? == 0 {
						self.state = if self.state < 7 { 9 } else { 11 };
						self.copy_match(self.rep0, 1, limit)?;
						continue;
					}
				} else {
					let distance;
					if range.decode_bit(&mut self.is_rep_g1, self.state)? == 0 {
						distance = self.rep1;
					} else {
						if range.decode_bit(&mut self.is_rep_g2, self.state)? == 0 {
							distance = self.rep2;
						} else {
							distance = self.rep3;
							self.rep3 = self.rep2;
						}
						self.rep2 = self.rep1;
					}
					self.rep1 = self.rep0;
					self.rep0 = distance;
				}
				length = self.rep_len.decode(&mut range, pos_state)? + MATCH_MIN_LEN;
				self.state = if self.state < 7 { 8 } else { 11 };
			} else {
				self.rep3 = self.rep2;
				self.rep2 = self.rep1;
				self.rep1 = self.rep0;
				length = self.len.decode(&mut range, pos_state)? + MATCH_MIN_LEN;
				self.state = if self.state < 7 { 7 } else { 10 };
				let len_state = (length - MATCH_MIN_LEN).min(3);
				let slot = decode_tree(&mut range, &mut self.pos_slot, len_state << 6, 6)?;
				let distance = if slot < 4 {
					slot
				} else {
					let direct_bits = (slot >> 1) - 1;
					let mut distance = (2 | (slot & 1)) << direct_bits;
					if slot < 14 {
						distance = distance.wrapping_add(decode_reverse_tree(
							&mut range,
							&mut self.pos_decoders,
							(distance - slot) as usize,
							direct_bits as usize,
						)?);
					} else {
						distance = distance
							.wrapping_add(range.decode_direct_bits((direct_bits - 4) as usize)? << 4);
						distance =
							distance.wrapping_add(decode_reverse_tree(&mut range, &mut self.align, 0, 4)?);
						if distance == u32::MAX {
							if allow_end_marker {
								return Ok(true);
							}
							return Err(invalid("LZMA stream ended before its declared output size"));
						}
					}
					distance
				};
				self.rep0 = usize::try_from(distance.wrapping_add(1)).unwrap_or(usize::MAX);
			}
			self.copy_match(self.rep0, length, limit)?;
		}
		Ok(false)
	}

	fn finish(mut self) -> Vec<u8> {
		self.output.truncate(self.output_pos);
		self.output
	}
}

/// Decompresses a raw LZMA1 stream with standard five-byte properties.
pub(crate) fn lzma_decompress(
	properties: &[u8],
	bytes: &[u8],
	output_size: usize,
) -> Result<Vec<u8>> {
	let mut decoder = LzmaDecoder::new(output_size, parse_properties(properties)?)?;
	decoder.decode_chunk(bytes, output_size, false)?;
	if decoder.output_pos != output_size {
		return Err(invalid("LZMA output size mismatch"));
	}
	Ok(decoder.finish())
}

/// Decompresses a stateful LZMA2 stream under an exact output ceiling.
pub(crate) fn lzma2_decompress(
	dictionary_property: u8,
	bytes: &[u8],
	limit: usize,
) -> Result<Vec<u8>> {
	if dictionary_property > 40 {
		return Err(invalid("unsupported LZMA2 dictionary property"));
	}
	let dictionary_size = if dictionary_property == 40 {
		u32::MAX
	} else {
		(2 | (u32::from(dictionary_property) & 1)) << (u32::from(dictionary_property) / 2 + 11)
	};
	let mut decoder = LzmaDecoder::new(limit, LzmaProperties {
		lc:              3,
		lp:              0,
		pb:              2,
		dictionary_size: usize::try_from(dictionary_size).unwrap_or(usize::MAX),
	})?;
	let mut position = 0_usize;
	let mut need_dictionary_reset = true;
	let mut properties_set = false;

	while position < bytes.len() {
		let control = bytes[position];
		position += 1;
		if control == 0 {
			if position != bytes.len() {
				return Err(invalid("trailing data after LZMA2 end marker"));
			}
			return Ok(decoder.finish());
		}
		let take = |position: &mut usize| -> Result<u8> {
			let value = *bytes
				.get(*position)
				.ok_or_else(|| invalid("truncated LZMA2 chunk header"))?;
			*position += 1;
			Ok(value)
		};
		if control < 0x80 {
			if control != 1 && control != 2 {
				return Err(invalid("invalid LZMA2 control byte"));
			}
			if control == 2 && need_dictionary_reset {
				return Err(invalid("LZMA2 dictionary was not initialized"));
			}
			let unpack_size =
				(usize::from(take(&mut position)?) << 8) + usize::from(take(&mut position)?) + 1;
			let end = position
				.checked_add(unpack_size)
				.filter(|end| *end <= bytes.len())
				.ok_or_else(|| invalid("invalid LZMA2 uncompressed chunk size"))?;
			decoder.append_uncompressed(&bytes[position..end], control == 1)?;
			position = end;
			if control == 1 {
				need_dictionary_reset = false;
				properties_set = false;
			}
			continue;
		}

		let unpack_size = ((usize::from(control & 0x1f)) << 16)
			+ (usize::from(take(&mut position)?) << 8)
			+ usize::from(take(&mut position)?)
			+ 1;
		let pack_size =
			(usize::from(take(&mut position)?) << 8) + usize::from(take(&mut position)?) + 1;
		let resets_dictionary = control >= 0xe0;
		let resets_state = control >= 0xa0;
		let sets_properties = control >= 0xc0;
		if need_dictionary_reset && !resets_dictionary {
			return Err(invalid("LZMA2 dictionary was not initialized"));
		}
		if !properties_set && !sets_properties {
			return Err(invalid("LZMA2 properties were not initialized"));
		}
		if sets_properties {
			let mut property = usize::from(take(&mut position)?);
			if property >= 9 * 5 * 5 {
				return Err(invalid("invalid LZMA2 properties"));
			}
			let lc = property % 9;
			property /= 9;
			let lp = property % 5;
			let pb = property / 5;
			if lc + lp > 4 {
				return Err(invalid("invalid LZMA2 literal properties"));
			}
			decoder.set_properties(lc, lp, pb)?;
			properties_set = true;
		}
		let end = position
			.checked_add(pack_size)
			.filter(|end| *end <= bytes.len())
			.ok_or_else(|| invalid("invalid LZMA2 chunk size"))?;
		if unpack_size > limit - decoder.output_pos {
			return Err(invalid("LZMA2 output exceeds its limit"));
		}
		if resets_dictionary {
			decoder.reset_dictionary();
		} else if resets_state {
			decoder.reset_state();
		}
		decoder.decode_chunk(&bytes[position..end], unpack_size, false)?;
		position = end;
		need_dictionary_reset = false;
	}
	Err(invalid("truncated LZMA2 stream without end marker"))
}

/// Decompresses one LZMA-alone stream bounded by `limits.archive_size`.
pub(crate) fn lzma_alone_decompress(bytes: &[u8], limits: Limits) -> Result<Vec<u8>> {
	if bytes.len() < 13 {
		return Err(invalid("truncated LZMA-alone header"));
	}
	let size_bytes: [u8; 8] = bytes[5..13].try_into().expect("slice length");
	let declared = u64::from_le_bytes(size_bytes);
	let maximum = limits.max_archive_size().min(limits.max_in_memory_size());
	let output_size = if declared == u64::MAX {
		usize::try_from(maximum)
			.map_err(|_| Error::ArchiveTooLargeInMemory { actual: maximum, limit: maximum })?
	} else {
		if declared > maximum {
			return Err(Error::ArchiveTooLargeInMemory { actual: declared, limit: maximum });
		}
		usize::try_from(declared)
			.map_err(|_| Error::ArchiveTooLargeInMemory { actual: declared, limit: maximum })?
	};
	if declared != u64::MAX {
		return lzma_decompress(&bytes[..5], &bytes[13..], output_size);
	}
	let mut decoder = LzmaDecoder::new(output_size, parse_properties(&bytes[..5])?)?;
	if !decoder.decode_chunk(&bytes[13..], output_size, true)? {
		return Err(invalid("LZMA-alone output exceeds its size limit before end marker"));
	}
	Ok(decoder.finish())
}
