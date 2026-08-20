//! Bounded 7z container indexing and eager solid-folder decoding.

use std::{
	collections::HashSet,
	io::{Read, Seek, SeekFrom, Write},
};

use omp_core::Str;
use xutf::{TextBuf as _, Utf8, Utf16Le};

use crate::{
	Entry, Error, Limits, Result,
	codec::{lzma_decompress, lzma2_decompress, x86_decode},
	entry::Storage,
	path::{normalize, validate},
};

const SIGNATURE: &[u8; 6] = b"7z\xbc\xaf'\x1c";
const FILETIME_EPOCH_SECONDS: u64 = 11_644_473_600;

const fn invalid(message: &'static str) -> Error {
	Error::InvalidArchive(message)
}

struct HeaderReader<'a> {
	bytes: &'a [u8],
	pos:   usize,
}

impl<'a> HeaderReader<'a> {
	const fn new(bytes: &'a [u8]) -> Self {
		Self { bytes, pos: 0 }
	}

	const fn remaining(&self) -> usize {
		self.bytes.len() - self.pos
	}

	fn read_byte(&mut self) -> Result<u8> {
		let value = *self
			.bytes
			.get(self.pos)
			.ok_or_else(|| invalid("truncated 7z header"))?;
		self.pos += 1;
		Ok(value)
	}

	fn read_bytes(&mut self, size: usize) -> Result<&'a [u8]> {
		let end = self
			.pos
			.checked_add(size)
			.filter(|end| *end <= self.bytes.len())
			.ok_or_else(|| invalid("truncated or invalid-sized 7z header data"))?;
		let result = &self.bytes[self.pos..end];
		self.pos = end;
		Ok(result)
	}

	fn read_u32(&mut self) -> Result<u32> {
		Ok(u32::from_le_bytes(self.read_bytes(4)?.try_into().expect("slice length")))
	}

	fn read_u64(&mut self) -> Result<u64> {
		Ok(u64::from_le_bytes(self.read_bytes(8)?.try_into().expect("slice length")))
	}

	fn read_number(&mut self) -> Result<u64> {
		let first = self.read_byte()?;
		let mut value = 0_u64;
		let mut mask = 0x80_u8;
		for index in 0..8 {
			if first & mask == 0 {
				return Ok(value | (u64::from(first & (mask - 1)) << (index * 8)));
			}
			value |= u64::from(self.read_byte()?) << (index * 8);
			mask >>= 1;
		}
		Ok(value)
	}

	fn read_usize(&mut self) -> Result<usize> {
		usize::try_from(self.read_number()?).map_err(|_| invalid("7z number is too large"))
	}

	fn skip_sized_property(&mut self, limits: Limits) -> Result<()> {
		let size = self.read_number()?;
		check_index_size(size, limits)?;
		self.read_bytes(usize::try_from(size).map_err(|_| invalid("7z property is too large"))?)?;
		Ok(())
	}
}

#[derive(Clone)]
struct Coder {
	id:           u64,
	properties:   Vec<u8>,
	input_start:  usize,
	output_start: usize,
	num_inputs:   usize,
	num_outputs:  usize,
}

#[derive(Clone, Copy)]
struct BindPair {
	input:  usize,
	output: usize,
}

#[derive(Clone)]
struct Folder {
	coders:         Vec<Coder>,
	bind_pairs:     Vec<BindPair>,
	packed_indices: Vec<usize>,
	unpack_sizes:   Vec<u64>,
	pack_offsets:   Vec<u64>,
	pack_sizes:     Vec<u64>,
	pack_crcs:      Vec<Option<u32>>,
	crc:            Option<u32>,
}

#[derive(Clone, Copy)]
struct Substream {
	folder_index: usize,
	offset:       u64,
	size:         u64,
	crc:          Option<u32>,
}

struct Streams {
	pack_position: u64,
	pack_sizes:    Vec<u64>,
	pack_crcs:     Vec<Option<u32>>,
	folders:       Vec<Folder>,
	substreams:    Vec<Substream>,
}

impl Streams {
	const fn empty() -> Self {
		Self {
			pack_position: 0,
			pack_sizes:    Vec::new(),
			pack_crcs:     Vec::new(),
			folders:       Vec::new(),
			substreams:    Vec::new(),
		}
	}
}

struct Digests {
	values: Vec<Option<u32>>,
}

struct FileMetadata {
	names:         Vec<String>,
	empty_streams: Vec<bool>,
	empty_files:   Vec<bool>,
	anti_files:    Vec<bool>,
	mtimes:        Vec<Option<u64>>,
	attributes:    Vec<Option<u32>>,
}

impl FileMetadata {
	const fn empty() -> Self {
		Self {
			names:         Vec::new(),
			empty_streams: Vec::new(),
			empty_files:   Vec::new(),
			anti_files:    Vec::new(),
			mtimes:        Vec::new(),
			attributes:    Vec::new(),
		}
	}
}

const fn check_index_size(size: u64, limits: Limits) -> Result<()> {
	if size > limits.max_index_size() {
		return Err(Error::IndexTooLarge { actual: size, limit: limits.max_index_size() });
	}
	Ok(())
}

const fn check_entry_count(count: u64, limits: Limits) -> Result<()> {
	if count > limits.max_entries() {
		return Err(Error::TooManyEntries { actual: count, limit: limits.max_entries() });
	}
	Ok(())
}

const fn check_memory_size(size: u64, limits: Limits) -> Result<()> {
	if size > limits.max_in_memory_size() {
		return Err(Error::ArchiveTooLargeInMemory {
			actual: size,
			limit:  limits.max_in_memory_size(),
		});
	}
	Ok(())
}

fn read_bool_vector(reader: &mut HeaderReader<'_>, count: usize) -> Result<Vec<bool>> {
	let mut result = Vec::with_capacity(count);
	let mut byte = 0_u8;
	let mut mask = 0_u8;
	for _ in 0..count {
		if mask == 0 {
			byte = reader.read_byte()?;
			mask = 0x80;
		}
		result.push(byte & mask != 0);
		mask >>= 1;
	}
	Ok(result)
}

fn read_defined_vector(reader: &mut HeaderReader<'_>, count: usize) -> Result<Vec<bool>> {
	if reader.read_byte()? != 0 {
		Ok(vec![true; count])
	} else {
		read_bool_vector(reader, count)
	}
}

fn read_digests(reader: &mut HeaderReader<'_>, count: usize) -> Result<Digests> {
	let defined = read_defined_vector(reader, count)?;
	let mut values = Vec::with_capacity(count);
	for is_defined in defined {
		values.push(if is_defined {
			Some(reader.read_u32()?)
		} else {
			None
		});
	}
	Ok(Digests { values })
}

fn expect_id(reader: &mut HeaderReader<'_>, expected: u64) -> Result<()> {
	if reader.read_number()? != expected {
		return Err(invalid("unexpected 7z header property ID"));
	}
	Ok(())
}

fn parse_folder(reader: &mut HeaderReader<'_>, limits: Limits) -> Result<Folder> {
	let coder_count = reader.read_number()?;
	if coder_count == 0 || coder_count > limits.max_entries() {
		return Err(invalid("invalid 7z folder coder count"));
	}
	let coder_count =
		usize::try_from(coder_count).map_err(|_| invalid("7z coder count is too large"))?;
	let mut coders = Vec::with_capacity(coder_count);
	let mut total_inputs = 0_usize;
	let mut total_outputs = 0_usize;
	for _ in 0..coder_count {
		let flags = reader.read_byte()?;
		if flags & 0xc0 != 0 {
			return Err(Error::UnsupportedFeature("7z coder alternatives or reserved flags"));
		}
		let id_size = usize::from(flags & 0x0f);
		if id_size == 0 || id_size > 8 {
			return Err(invalid("invalid 7z coder method ID size"));
		}
		let mut id = 0_u64;
		for byte in reader.read_bytes(id_size)? {
			id = id << 8 | u64::from(*byte);
		}
		let complex = flags & 0x10 != 0;
		let num_inputs = if complex { reader.read_usize()? } else { 1 };
		let num_outputs = if complex { reader.read_usize()? } else { 1 };
		if num_inputs == 0
			|| num_outputs == 0
			|| total_inputs.checked_add(num_inputs).is_none()
			|| total_outputs.checked_add(num_outputs).is_none()
			|| total_inputs + num_inputs > limits.max_entries() as usize
			|| total_outputs + num_outputs > limits.max_entries() as usize
		{
			return Err(invalid("invalid or excessive 7z coder stream count"));
		}
		let property_size = if flags & 0x20 != 0 {
			reader.read_number()?
		} else {
			0
		};
		check_index_size(property_size, limits)?;
		let property_size = usize::try_from(property_size)
			.map_err(|_| invalid("7z coder properties are too large"))?;
		coders.push(Coder {
			id,
			properties: reader.read_bytes(property_size)?.to_vec(),
			input_start: total_inputs,
			output_start: total_outputs,
			num_inputs,
			num_outputs,
		});
		total_inputs += num_inputs;
		total_outputs += num_outputs;
	}
	let bind_count = total_outputs
		.checked_sub(1)
		.filter(|count| *count <= total_inputs)
		.ok_or_else(|| invalid("invalid 7z folder bind-pair count"))?;
	let mut bind_pairs = Vec::with_capacity(bind_count);
	let mut used_inputs = HashSet::with_capacity(bind_count);
	let mut used_outputs = HashSet::with_capacity(bind_count);
	for _ in 0..bind_count {
		let input = reader.read_usize()?;
		let output = reader.read_usize()?;
		if input >= total_inputs
			|| output >= total_outputs
			|| !used_inputs.insert(input)
			|| !used_outputs.insert(output)
		{
			return Err(invalid("invalid 7z folder bind pair"));
		}
		bind_pairs.push(BindPair { input, output });
	}
	let packed_count = total_inputs - bind_count;
	let mut packed_indices = Vec::with_capacity(packed_count);
	if packed_count == 1 {
		for index in 0..total_inputs {
			if !used_inputs.contains(&index) {
				packed_indices.push(index);
			}
		}
	} else {
		for _ in 0..packed_count {
			let packed = reader.read_usize()?;
			if packed >= total_inputs || !used_inputs.insert(packed) {
				return Err(invalid("invalid 7z packed stream index"));
			}
			packed_indices.push(packed);
		}
	}
	if packed_indices.len() != packed_count {
		return Err(invalid("invalid 7z folder packed-stream map"));
	}
	Ok(Folder {
		coders,
		bind_pairs,
		packed_indices,
		unpack_sizes: Vec::new(),
		pack_offsets: Vec::new(),
		pack_sizes: Vec::new(),
		pack_crcs: Vec::new(),
		crc: None,
	})
}

fn final_folder_output(folder: &Folder) -> Result<usize> {
	let mut final_output = None;
	for coder in &folder.coders {
		for output in coder.output_start..coder.output_start + coder.num_outputs {
			if folder.bind_pairs.iter().any(|pair| pair.output == output) {
				continue;
			}
			if final_output.replace(output).is_some() {
				return Err(Error::UnsupportedFeature("7z folder with multiple final output streams"));
			}
		}
	}
	final_output.ok_or_else(|| invalid("7z folder has no final output stream"))
}

fn parse_pack_info(
	reader: &mut HeaderReader<'_>,
	streams: &mut Streams,
	limits: Limits,
) -> Result<()> {
	streams.pack_position = reader.read_number()?;
	let count = reader.read_number()?;
	check_entry_count(count, limits)?;
	let count = usize::try_from(count).map_err(|_| invalid("7z pack stream count is too large"))?;
	loop {
		match reader.read_number()? {
			0 => break,
			9 => {
				if !streams.pack_sizes.is_empty() {
					return Err(invalid("duplicate 7z pack sizes"));
				}
				streams.pack_sizes.reserve(count);
				for _ in 0..count {
					streams.pack_sizes.push(reader.read_number()?);
				}
			},
			10 => {
				if !streams.pack_crcs.is_empty() {
					return Err(invalid("duplicate 7z pack CRCs"));
				}
				streams.pack_crcs = read_digests(reader, count)?.values;
			},
			_ => reader.skip_sized_property(limits)?,
		}
	}
	if streams.pack_sizes.len() != count {
		return Err(invalid("7z PackInfo lacks complete sizes"));
	}
	if streams.pack_crcs.is_empty() {
		streams.pack_crcs.resize(count, None);
	}
	Ok(())
}

fn parse_unpack_info(
	reader: &mut HeaderReader<'_>,
	streams: &mut Streams,
	limits: Limits,
) -> Result<()> {
	expect_id(reader, 11)?;
	let count = reader.read_number()?;
	check_entry_count(count, limits)?;
	let count = usize::try_from(count).map_err(|_| invalid("7z folder count is too large"))?;
	if reader.read_byte()? != 0 {
		return Err(Error::UnsupportedFeature("external 7z folder definitions"));
	}
	streams.folders.reserve(count);
	for _ in 0..count {
		streams.folders.push(parse_folder(reader, limits)?);
	}
	expect_id(reader, 12)?;
	for folder in &mut streams.folders {
		let output_count = folder.coders.iter().try_fold(0_usize, |sum, coder| {
			sum.checked_add(coder.num_outputs)
				.ok_or_else(|| invalid("7z coder output count overflow"))
		})?;
		folder.unpack_sizes.reserve(output_count);
		for _ in 0..output_count {
			folder.unpack_sizes.push(reader.read_number()?);
		}
	}
	loop {
		match reader.read_number()? {
			0 => break,
			10 => {
				let digests = read_digests(reader, count)?;
				for (folder, digest) in streams.folders.iter_mut().zip(digests.values) {
					folder.crc = digest;
				}
			},
			_ => reader.skip_sized_property(limits)?,
		}
	}
	Ok(())
}

fn parse_substreams_info(
	reader: &mut HeaderReader<'_>,
	streams: &mut Streams,
	limits: Limits,
) -> Result<()> {
	let mut counts = vec![1_usize; streams.folders.len()];
	let mut sizes = None;
	let mut raw_digests = None;
	loop {
		match reader.read_number()? {
			0 => break,
			13 => {
				for count in &mut counts {
					*count = reader.read_usize()?;
				}
				let total = counts.iter().try_fold(0_u64, |sum, count| {
					sum.checked_add(*count as u64)
						.ok_or_else(|| invalid("7z substream count overflow"))
				})?;
				check_entry_count(total, limits)?;
			},
			9 => {
				let explicit_count = counts.iter().map(|count| count.saturating_sub(1)).sum();
				let mut values = Vec::with_capacity(explicit_count);
				for count in &counts {
					for _ in 1..*count {
						values.push(reader.read_number()?);
					}
				}
				sizes = Some(values);
			},
			10 => {
				let digest_count = counts
					.iter()
					.enumerate()
					.filter(|(index, count)| **count != 1 || streams.folders[*index].crc.is_none())
					.map(|(_, count)| *count)
					.sum();
				raw_digests = Some(read_digests(reader, digest_count)?);
			},
			_ => reader.skip_sized_property(limits)?,
		}
	}
	let mut size_index = 0_usize;
	let mut digest_index = 0_usize;
	for (folder_index, folder) in streams.folders.iter().enumerate() {
		let count = counts[folder_index];
		let folder_size = *folder
			.unpack_sizes
			.get(final_folder_output(folder)?)
			.ok_or_else(|| invalid("missing 7z folder unpack size"))?;
		let mut offset = 0_u64;
		for index in 0..count {
			let size = if index + 1 == count {
				folder_size
					.checked_sub(offset)
					.ok_or_else(|| invalid("invalid 7z substream size total"))?
			} else {
				let value = sizes
					.as_ref()
					.and_then(|values| values.get(size_index))
					.copied()
					.ok_or_else(|| invalid("7z SubStreamsInfo lacks sizes"))?;
				size_index += 1;
				value
			};
			let end = offset
				.checked_add(size)
				.filter(|end| *end <= folder_size)
				.ok_or_else(|| invalid("invalid 7z substream size total"))?;
			let crc = if count == 1 && folder.crc.is_some() {
				folder.crc
			} else {
				let value = raw_digests
					.as_ref()
					.and_then(|digests| digests.values.get(digest_index))
					.copied()
					.flatten();
				digest_index += 1;
				value
			};
			streams
				.substreams
				.push(Substream { folder_index, offset, size, crc });
			offset = end;
		}
		if offset != folder_size {
			return Err(invalid("invalid 7z substream sizes"));
		}
	}
	Ok(())
}

fn assign_pack_streams(streams: &mut Streams, source_size: u64) -> Result<()> {
	let mut absolute = 32_u64
		.checked_add(streams.pack_position)
		.ok_or_else(|| invalid("7z packed-stream offset overflow"))?;
	let mut offsets = Vec::with_capacity(streams.pack_sizes.len());
	for size in &streams.pack_sizes {
		let end = absolute
			.checked_add(*size)
			.filter(|end| *end <= source_size)
			.ok_or_else(|| invalid("invalid 7z packed-stream range"))?;
		offsets.push(absolute);
		absolute = end;
	}
	let mut pack_index = 0_usize;
	for folder in &mut streams.folders {
		for _ in &folder.packed_indices {
			if pack_index >= streams.pack_sizes.len() {
				return Err(invalid("invalid 7z folder-to-pack-stream mapping"));
			}
			folder.pack_offsets.push(offsets[pack_index]);
			folder.pack_sizes.push(streams.pack_sizes[pack_index]);
			folder.pack_crcs.push(streams.pack_crcs[pack_index]);
			pack_index += 1;
		}
	}
	if pack_index != streams.pack_sizes.len() {
		return Err(invalid("unused 7z packed streams"));
	}
	Ok(())
}

fn parse_streams_info(
	reader: &mut HeaderReader<'_>,
	source_size: u64,
	limits: Limits,
) -> Result<Streams> {
	let mut streams = Streams::empty();
	loop {
		match reader.read_number()? {
			0 => break,
			6 => parse_pack_info(reader, &mut streams, limits)?,
			7 => parse_unpack_info(reader, &mut streams, limits)?,
			8 => parse_substreams_info(reader, &mut streams, limits)?,
			_ => return Err(Error::UnsupportedFeature("7z StreamsInfo property")),
		}
	}
	if !streams.folders.is_empty() && streams.substreams.is_empty() {
		for (folder_index, folder) in streams.folders.iter().enumerate() {
			let size = *folder
				.unpack_sizes
				.get(final_folder_output(folder)?)
				.ok_or_else(|| invalid("missing 7z folder unpack size"))?;
			streams
				.substreams
				.push(Substream { folder_index, offset: 0, size, crc: folder.crc });
		}
	}
	assign_pack_streams(&mut streams, source_size)?;
	Ok(streams)
}

fn read_range(source: &mut (impl Read + Seek), offset: u64, size: u64) -> Result<Vec<u8>> {
	let size = usize::try_from(size).map_err(|_| invalid("7z source range is too large"))?;
	let mut bytes = vec![0; size];
	source.seek(SeekFrom::Start(offset))?;
	source.read_exact(&mut bytes)?;
	Ok(bytes)
}

const fn coder_error(id: u64) -> Error {
	match id {
		0x30401 => Error::UnsupportedFeature("7z coder PPMd"),
		0x303011b => Error::UnsupportedFeature("7z coder BCJ2 (multi-input folder)"),
		0x6f10701 => Error::UnsupportedFeature("encrypted 7z coder 7zAES"),
		0x40108 => Error::UnsupportedFeature("7z coder Deflate"),
		0x40109 => Error::UnsupportedFeature("7z coder Deflate64"),
		0x40202 => Error::UnsupportedFeature("7z coder BZip2"),
		0x3030205 => Error::UnsupportedFeature("7z coder BCJ PowerPC"),
		0x3030401 => Error::UnsupportedFeature("7z coder BCJ IA64"),
		0x3030501 => Error::UnsupportedFeature("7z coder BCJ ARM"),
		0x3030701 => Error::UnsupportedFeature("7z coder BCJ ARM Thumb"),
		0x3030805 => Error::UnsupportedFeature("7z coder BCJ SPARC"),
		0xa => Error::UnsupportedFeature("7z coder BCJ ARM64"),
		0xb => Error::UnsupportedFeature("7z coder BCJ RISC-V"),
		_ => Error::UnsupportedFeature("unknown 7z coder method"),
	}
}

fn decode_folder(
	source: &mut (impl Read + Seek),
	folder: &Folder,
	limits: Limits,
) -> Result<Vec<u8>> {
	if folder.packed_indices.len() != 1
		|| folder.pack_offsets.len() != 1
		|| folder.pack_sizes.len() != 1
	{
		if folder.coders.iter().any(|coder| coder.id == 0x303011b) {
			return Err(coder_error(0x303011b));
		}
		return Err(Error::UnsupportedFeature("7z folder graph with multiple packed input streams"));
	}
	let mut bytes = read_range(source, folder.pack_offsets[0], folder.pack_sizes[0])?;
	if folder.pack_crcs[0].is_some_and(|expected| crc32fast::hash(&bytes) != expected) {
		return Err(invalid("7z packed-stream CRC32 mismatch"));
	}
	let mut input_index = folder.packed_indices[0];
	let mut visited = HashSet::with_capacity(folder.coders.len());
	loop {
		let coder_index = folder
			.coders
			.iter()
			.position(|coder| {
				input_index >= coder.input_start && input_index < coder.input_start + coder.num_inputs
			})
			.ok_or_else(|| invalid("invalid 7z folder binding graph"))?;
		if !visited.insert(coder_index) {
			return Err(invalid("cyclic 7z folder binding graph"));
		}
		let coder = &folder.coders[coder_index];
		if coder.num_inputs != 1 || coder.num_outputs != 1 {
			return Err(coder_error(coder.id));
		}
		let output_size = *folder
			.unpack_sizes
			.get(coder.output_start)
			.ok_or_else(|| invalid("missing 7z coder unpack size"))?;
		check_memory_size(output_size, limits)?;
		let output_size =
			usize::try_from(output_size).map_err(|_| invalid("7z coder output size is too large"))?;
		match coder.id {
			0 => {
				if !coder.properties.is_empty() || bytes.len() != output_size {
					return Err(invalid("invalid 7z Copy coder size or properties"));
				}
			},
			0x30101 => bytes = lzma_decompress(&coder.properties, &bytes, output_size)?,
			0x21 => {
				if coder.properties.len() != 1 {
					return Err(invalid("invalid 7z LZMA2 coder properties"));
				}
				bytes = lzma2_decompress(coder.properties[0], &bytes, output_size)?;
				if bytes.len() != output_size {
					return Err(invalid("7z LZMA2 coder output size mismatch"));
				}
			},
			3 => {
				if coder.properties.len() != 1 || bytes.len() != output_size {
					return Err(invalid("invalid 7z Delta coder properties or size"));
				}
				delta_decode(&mut bytes, usize::from(coder.properties[0]) + 1);
			},
			0x3030103 => {
				if (!coder.properties.is_empty() && coder.properties.len() != 4)
					|| bytes.len() != output_size
				{
					return Err(invalid("invalid 7z BCJ x86 coder properties or size"));
				}
				let start_offset = if coder.properties.len() == 4 {
					u32::from_le_bytes(
						coder
							.properties
							.as_slice()
							.try_into()
							.expect("slice length"),
					)
				} else {
					0
				};
				x86_decode(&mut bytes, start_offset);
			},
			_ => return Err(coder_error(coder.id)),
		}
		let Some(binding) = folder
			.bind_pairs
			.iter()
			.find(|pair| pair.output == coder.output_start)
		else {
			break;
		};
		input_index = binding.input;
	}
	if visited.len() != folder.coders.len() {
		return Err(Error::UnsupportedFeature("7z folder graph with disconnected coders"));
	}
	if folder
		.crc
		.is_some_and(|expected| crc32fast::hash(&bytes) != expected)
	{
		return Err(invalid("7z folder CRC32 mismatch"));
	}
	Ok(bytes)
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

fn decode_metadata_streams(
	source: &mut (impl Read + Seek),
	streams: &Streams,
	limits: Limits,
) -> Result<Vec<Vec<u8>>> {
	let mut buffers = Vec::with_capacity(streams.folders.len());
	let mut total = 0_u64;
	for folder in &streams.folders {
		let size = *folder
			.unpack_sizes
			.get(final_folder_output(folder)?)
			.ok_or_else(|| invalid("missing decoded 7z header size"))?;
		check_index_size(size, limits)?;
		total = total
			.checked_add(size)
			.ok_or_else(|| invalid("decoded 7z metadata size overflow"))?;
		check_memory_size(total, limits)?;
		buffers.push(decode_folder(source, folder, limits)?);
	}
	Ok(buffers)
}

fn select_property_bytes(reader: &mut HeaderReader<'_>, external: &[Vec<u8>]) -> Result<Vec<u8>> {
	match reader.read_byte()? {
		0 => {
			let bytes = reader.bytes[reader.pos..].to_vec();
			reader.pos = reader.bytes.len();
			Ok(bytes)
		},
		1 => {
			let index = reader.read_usize()?;
			external
				.get(index)
				.cloned()
				.ok_or_else(|| invalid("invalid 7z external property stream index"))
		},
		_ => Err(invalid("invalid 7z external-property flag")),
	}
}

fn decode_utf16_le(bytes: &[u8]) -> Result<String> {
	if bytes.len() & 1 != 0 {
		return Err(invalid("odd-length 7z UTF-16 file name"));
	}
	let mut units = Vec::with_capacity(bytes.len() / 2);
	for pair in bytes.as_chunks::<2>().0 {
		units.push(u16::from_le_bytes([pair[0], pair[1]]));
	}
	let mut index = 0;
	while index < units.len() {
		let unit = units[index];
		if (0xd800..=0xdbff).contains(&unit) {
			if !units
				.get(index + 1)
				.is_some_and(|next| (0xdc00..=0xdfff).contains(next))
			{
				return Err(invalid("invalid UTF-16 7z file name"));
			}
			index += 2;
		} else if (0xdc00..=0xdfff).contains(&unit) {
			return Err(invalid("invalid UTF-16 7z file name"));
		} else {
			index += 1;
		}
	}
	Ok(String::from_units(xutf::transcode::<Utf16Le, Utf8>(&units)))
}

fn parse_names(
	reader: &mut HeaderReader<'_>,
	count: usize,
	external: &[Vec<u8>],
	limits: Limits,
) -> Result<Vec<String>> {
	let property_bytes = select_property_bytes(reader, external)?;
	let mut values = HeaderReader::new(&property_bytes);
	let mut names = Vec::with_capacity(count);
	for _ in 0..count {
		let start = values.pos;
		while values.remaining() >= 2
			&& (values.bytes[values.pos] != 0 || values.bytes[values.pos + 1] != 0)
		{
			values.pos += 2;
		}
		let encoded_size = values.pos - start;
		if encoded_size as u64 > limits.max_path_size().saturating_mul(2) {
			return Err(Error::PathTooLong {
				actual: encoded_size as u64,
				limit:  limits.max_path_size().saturating_mul(2),
			});
		}
		if values.remaining() < 2 {
			return Err(invalid("unterminated 7z UTF-16 file-name table"));
		}
		let name = decode_utf16_le(&values.bytes[start..values.pos])?;
		values.pos += 2;
		if name.len() as u64 > limits.max_path_size() {
			return Err(Error::PathTooLong {
				actual: name.len() as u64,
				limit:  limits.max_path_size(),
			});
		}
		names.push(name);
	}
	Ok(names)
}

fn parse_times(
	reader: &mut HeaderReader<'_>,
	count: usize,
	external: &[Vec<u8>],
) -> Result<Vec<Option<u64>>> {
	let defined = read_defined_vector(reader, count)?;
	let property_bytes = select_property_bytes(reader, external)?;
	let mut values = HeaderReader::new(&property_bytes);
	let mut result = Vec::with_capacity(count);
	for is_defined in defined {
		if is_defined {
			let seconds = values.read_u64()? / 10_000_000;
			result.push(seconds.checked_sub(FILETIME_EPOCH_SECONDS));
		} else {
			result.push(None);
		}
	}
	Ok(result)
}

fn parse_attributes(
	reader: &mut HeaderReader<'_>,
	count: usize,
	external: &[Vec<u8>],
) -> Result<Vec<Option<u32>>> {
	let defined = read_defined_vector(reader, count)?;
	let property_bytes = select_property_bytes(reader, external)?;
	let mut values = HeaderReader::new(&property_bytes);
	let mut result = Vec::with_capacity(count);
	for is_defined in defined {
		result.push(if is_defined {
			Some(values.read_u32()?)
		} else {
			None
		});
	}
	Ok(result)
}

fn parse_files_info(
	reader: &mut HeaderReader<'_>,
	external: &[Vec<u8>],
	limits: Limits,
) -> Result<FileMetadata> {
	let count = reader.read_number()?;
	check_entry_count(count, limits)?;
	let count = usize::try_from(count).map_err(|_| invalid("7z file count is too large"))?;
	let mut metadata = FileMetadata {
		names:         vec![String::new(); count],
		empty_streams: vec![false; count],
		empty_files:   Vec::new(),
		anti_files:    Vec::new(),
		mtimes:        vec![None; count],
		attributes:    vec![None; count],
	};
	loop {
		let id = reader.read_number()?;
		if id == 0 {
			break;
		}
		let size = reader.read_number()?;
		check_index_size(size, limits)?;
		let size = usize::try_from(size).map_err(|_| invalid("7z file property is too large"))?;
		let mut property = HeaderReader::new(reader.read_bytes(size)?);
		match id {
			14 => metadata.empty_streams = read_bool_vector(&mut property, count)?,
			15 => {
				let empty_count = metadata
					.empty_streams
					.iter()
					.filter(|empty| **empty)
					.count();
				metadata.empty_files = read_bool_vector(&mut property, empty_count)?;
			},
			16 => {
				let empty_count = metadata
					.empty_streams
					.iter()
					.filter(|empty| **empty)
					.count();
				metadata.anti_files = read_bool_vector(&mut property, empty_count)?;
			},
			17 => metadata.names = parse_names(&mut property, count, external, limits)?,
			20 => metadata.mtimes = parse_times(&mut property, count, external)?,
			21 => metadata.attributes = parse_attributes(&mut property, count, external)?,
			25 => {
				if property.bytes[property.pos..].iter().any(|byte| *byte != 0) {
					return Err(invalid("non-zero 7z dummy property"));
				}
				property.pos = property.bytes.len();
			},
			_ => property.pos = property.bytes.len(),
		}
		if property.remaining() != 0 {
			return Err(invalid("invalid 7z file property length"));
		}
	}
	Ok(metadata)
}

fn parse_header(
	source: &mut (impl Read + Seek),
	bytes: &[u8],
	source_size: u64,
	limits: Limits,
) -> Result<(Streams, FileMetadata)> {
	let mut reader = HeaderReader::new(bytes);
	expect_id(&mut reader, 1)?;
	let mut additional = Vec::new();
	let mut streams = None;
	let mut files = None;
	loop {
		match reader.read_number()? {
			0 => break,
			2 => loop {
				if reader.read_number()? == 0 {
					break;
				}
				reader.skip_sized_property(limits)?;
			},
			3 => {
				let metadata_streams = parse_streams_info(&mut reader, source_size, limits)?;
				additional = decode_metadata_streams(source, &metadata_streams, limits)?;
			},
			4 => streams = Some(parse_streams_info(&mut reader, source_size, limits)?),
			5 => files = Some(parse_files_info(&mut reader, &additional, limits)?),
			_ => return Err(Error::UnsupportedFeature("7z header property")),
		}
	}
	if reader.remaining() != 0 {
		return Err(invalid("trailing 7z header data"));
	}
	Ok((streams.unwrap_or_else(Streams::empty), files.unwrap_or_else(FileMetadata::empty)))
}

fn canonical_link_target(record_path: &str, raw_target: &str) -> (Str, bool) {
	let portable = raw_target.replace('\\', "/");
	if portable.starts_with('/') {
		return (Str::new(portable), false);
	}
	let mut parts: Vec<&str> = record_path.split('/').collect();
	parts.pop();
	for part in portable.split('/') {
		if part.is_empty() || part == "." {
			continue;
		}
		if part == ".." {
			if parts.is_empty() {
				return (Str::new(portable), false);
			}
			parts.pop();
		} else {
			parts.push(part);
		}
	}
	(Str::new(parts.join("/")), true)
}

fn decode_data_folders(
	source: &mut (impl Read + Seek),
	streams: &Streams,
	limits: Limits,
	decoded: &mut Vec<Vec<u8>>,
) -> Result<Vec<u32>> {
	let mut total = decoded.iter().try_fold(0_u64, |sum, buffer| {
		sum.checked_add(buffer.len() as u64)
			.ok_or_else(|| invalid("decoded 7z size overflow"))
	})?;
	let mut indices = Vec::with_capacity(streams.folders.len());
	for folder in &streams.folders {
		let size = *folder
			.unpack_sizes
			.get(final_folder_output(folder)?)
			.ok_or_else(|| invalid("missing 7z folder output size"))?;
		total = total
			.checked_add(size)
			.ok_or_else(|| invalid("decoded 7z size overflow"))?;
		check_memory_size(total, limits)?;
		let buffer = decode_folder(source, folder, limits)?;
		let index =
			u32::try_from(decoded.len()).map_err(|_| invalid("too many decoded 7z buffers"))?;
		decoded.push(buffer);
		indices.push(index);
	}
	for stream in &streams.substreams {
		let folder_buffer = decoded
			.get(indices[stream.folder_index] as usize)
			.ok_or_else(|| invalid("missing decoded 7z folder"))?;
		let start =
			usize::try_from(stream.offset).map_err(|_| invalid("7z substream offset is too large"))?;
		let end = usize::try_from(stream.size)
			.ok()
			.and_then(|size| start.checked_add(size))
			.filter(|end| *end <= folder_buffer.len())
			.ok_or_else(|| invalid("invalid 7z substream range"))?;
		if stream
			.crc
			.is_some_and(|expected| crc32fast::hash(&folder_buffer[start..end]) != expected)
		{
			return Err(invalid("7z member CRC32 mismatch"));
		}
	}
	Ok(indices)
}

fn build_entries(
	streams: &Streams,
	files: FileMetadata,
	folder_buffers: &[u32],
	decoded: &[Vec<u8>],
	limits: Limits,
) -> Result<Vec<Entry>> {
	let mut entries = Vec::with_capacity(files.names.len());
	let mut empty_index = 0_usize;
	let mut stream_index = 0_usize;
	for file_index in 0..files.names.len() {
		let empty_stream = files
			.empty_streams
			.get(file_index)
			.copied()
			.unwrap_or(false);
		let empty_file = empty_stream && files.empty_files.get(empty_index).copied().unwrap_or(false);
		let anti = empty_stream && files.anti_files.get(empty_index).copied().unwrap_or(false);
		if empty_stream {
			empty_index += 1;
		}
		let stream = if empty_stream {
			None
		} else {
			let value = streams.substreams.get(stream_index).copied();
			stream_index += 1;
			Some(value.ok_or_else(|| invalid("invalid 7z file-to-substream mapping"))?)
		};
		let Some(path) = normalize(&files.names[file_index], false) else {
			continue;
		};
		if anti {
			continue;
		}
		validate(&path, limits)?;
		let attribute = files.attributes.get(file_index).copied().flatten();
		let mode = attribute.map(|value| value >> 16).filter(|mode| *mode != 0);
		let symlink = mode.is_some_and(|mode| mode & 0o170000 == 0o120000);
		let directory = empty_stream && !empty_file
			|| attribute.is_some_and(|value| value & 0x10 != 0)
			|| mode.is_some_and(|mode| mode & 0o170000 == 0o040000);
		let size = stream.map_or(0, |value| value.size);
		if size > limits.max_member_size() {
			return Err(Error::MemberTooLarge { path, actual: size, limit: limits.max_member_size() });
		}
		let storage = if symlink {
			let stream = stream.ok_or_else(|| invalid("7z symlink has no payload stream"))?;
			if stream.size > limits.max_path_size() {
				return Err(Error::PathTooLong { actual: stream.size, limit: limits.max_path_size() });
			}
			let buffer = decoded
				.get(folder_buffers[stream.folder_index] as usize)
				.ok_or_else(|| invalid("missing decoded 7z symlink folder"))?;
			let start = usize::try_from(stream.offset)
				.map_err(|_| invalid("7z symlink offset is too large"))?;
			let end = usize::try_from(stream.size)
				.ok()
				.and_then(|size| start.checked_add(size))
				.filter(|end| *end <= buffer.len())
				.ok_or_else(|| invalid("invalid 7z symlink payload range"))?;
			let raw_target = std::str::from_utf8(&buffer[start..end])
				.map_err(|_| invalid("invalid UTF-8 7z symlink target"))?;
			let (target_path, resolve_target) = canonical_link_target(&path, raw_target);
			Storage::Link { target_path, resolve_target }
		} else if directory {
			Storage::Synthetic
		} else if let Some(stream) = stream {
			Storage::Buffered {
				buffer:      folder_buffers[stream.folder_index],
				data_offset: stream.offset,
				stored_size: stream.size,
			}
		} else {
			Storage::Raw { data_offset: 0, stored_size: 0 }
		};
		entries.push(Entry {
			path,
			directory,
			size,
			modified_unix_seconds: files.mtimes.get(file_index).copied().flatten(),
			mode,
			storage,
		});
		check_entry_count(entries.len() as u64, limits)?;
	}
	if stream_index != streams.substreams.len() {
		return Err(invalid("unused 7z substreams"));
	}
	Ok(entries)
}

/// Returns whether bytes begin with the 7z signature.
pub fn is_header(bytes: &[u8]) -> bool {
	bytes.starts_with(SIGNATURE)
}

/// Indexes a 7z container, eagerly retaining each decoded solid folder.
pub(crate) fn read_entries(
	source: &mut (impl Read + Seek),
	file_size: u64,
	limits: Limits,
	decoded: &mut Vec<Vec<u8>>,
) -> Result<Vec<Entry>> {
	if file_size < 32 {
		return Err(invalid("truncated 7z signature header"));
	}
	let signature_header = read_range(source, 0, 32)?;
	if !is_header(&signature_header) {
		return Err(invalid("invalid 7z archive signature"));
	}
	if signature_header[6] != 0 {
		return Err(Error::UnsupportedFeature("7z major version"));
	}
	if crc32fast::hash(&signature_header[12..32])
		!= u32::from_le_bytes(signature_header[8..12].try_into().expect("slice length"))
	{
		return Err(invalid("7z start-header CRC32 mismatch"));
	}
	let mut start = HeaderReader::new(&signature_header[12..]);
	let next_offset = start.read_u64()?;
	let next_size = start.read_u64()?;
	let next_crc = start.read_u32()?;
	check_index_size(next_size, limits)?;
	let header_start = 32_u64
		.checked_add(next_offset)
		.ok_or_else(|| invalid("7z next-header offset overflow"))?;
	if header_start
		.checked_add(next_size)
		.as_ref()
		.is_none_or(|end| *end > file_size)
	{
		return Err(invalid("invalid 7z next-header range"));
	}
	let mut header = read_range(source, header_start, next_size)?;
	if crc32fast::hash(&header) != next_crc {
		return Err(invalid("7z next-header CRC32 mismatch"));
	}
	let first = HeaderReader::new(&header).read_number()?;
	if first == 23 {
		let mut encoded_reader = HeaderReader::new(&header);
		encoded_reader.read_number()?;
		let encoded_streams = parse_streams_info(&mut encoded_reader, file_size, limits)?;
		if encoded_reader.remaining() != 0 {
			return Err(invalid("trailing encoded 7z header data"));
		}
		let mut metadata = decode_metadata_streams(source, &encoded_streams, limits)?;
		if metadata.len() != 1 {
			return Err(invalid("encoded 7z header has multiple folders"));
		}
		header = metadata.pop().expect("one metadata folder");
		if HeaderReader::new(&header).read_number()? != 1 {
			return Err(invalid("invalid decoded 7z header marker"));
		}
	} else if first != 1 {
		return Err(Error::UnsupportedFeature("7z next-header type"));
	}
	let (streams, files) = parse_header(source, &header, file_size, limits)?;
	let folder_buffers = decode_data_folders(source, &streams, limits, decoded)?;
	build_entries(&streams, files, &folder_buffers, decoded, limits)
}

/// Rejects format-specific extraction; 7z members always use generic buffered
/// storage.
pub(crate) const fn read_entry_to<W: Write>(
	_source: &mut (impl Read + Seek),
	_entry: &Entry,
	_output: &mut W,
) -> Result<u64> {
	Err(invalid("7z member lacks buffered storage"))
}
