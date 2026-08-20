//! Bounded lazy ISO 9660, Joliet, and Rock Ridge indexing.

use std::{
	cmp,
	collections::{HashMap, HashSet, VecDeque},
	io::{self, Read, Seek, SeekFrom, Write},
};

use omp_core::{Str, StrMut};
use smallvec::SmallVec;
use xutf::{TextBuf as _, Utf8, Utf16};

use crate::{
	Entry, Error, Limits, Result,
	entry::{IsoExtent, Storage},
	path::{normalize, parent, validate},
};

const SECTOR_SIZE: u64 = 2048;
const DESCRIPTOR_START: u64 = 16 * SECTOR_SIZE;
const MAX_DIRECTORY_DEPTH: u16 = 256;
const MAX_SUSP_CONTINUATIONS: usize = 32;

#[derive(Clone)]
struct IsoRecord {
	extent: u32,
	size: u32,
	extended_attribute_blocks: u8,
	modified_unix_seconds: Option<u64>,
	flags: u8,
	file_unit_blocks: u8,
	interleave_blocks: u8,
	identifier: Vec<u8>,
	system_use: Vec<u8>,
}

struct IsoVolume {
	root:            IsoRecord,
	joliet:          bool,
	rock_ridge_root: Option<IsoRecord>,
}

struct DirectoryWork {
	record:    IsoRecord,
	parent:    Str,
	depth:     u16,
	ancestors: SmallVec<(u32, u32), 16>,
}

#[derive(Default)]
struct SuspData {
	name:       Option<String>,
	mode:       Option<u32>,
	symlink:    Option<String>,
	relocation: Option<[u8; 2]>,
}

struct MetadataBudget {
	read:  u64,
	limit: u64,
}

impl MetadataBudget {
	const fn new(limits: Limits) -> Self {
		Self { read: 0, limit: limits.index_size }
	}

	fn read(
		&mut self,
		source: &mut (impl Read + Seek),
		file_size: u64,
		start: u64,
		size: u64,
		truncated: &'static str,
	) -> Result<Vec<u8>> {
		let end = checked_add(start, size, "ISO metadata range overflows")?;
		if end > file_size {
			return Err(Error::InvalidArchive(truncated));
		}
		let total = checked_add(self.read, size, "ISO metadata accounting overflows")?;
		if total > self.limit {
			return Err(Error::IndexTooLarge { actual: total, limit: self.limit });
		}
		let length = usize::try_from(size)
			.map_err(|_| Error::InvalidArchive("ISO metadata size does not fit this platform"))?;
		let mut bytes = vec![0_u8; length];
		source.seek(SeekFrom::Start(start))?;
		source
			.read_exact(&mut bytes)
			.map_err(|error| map_truncated(error, truncated))?;
		self.read = total;
		Ok(bytes)
	}
}

/// Returns whether bytes contain the ISO 9660 signature at sector 16.
pub fn is_header(bytes: &[u8]) -> bool {
	bytes.get((DESCRIPTOR_START + 1) as usize..(DESCRIPTOR_START + 6) as usize)
		== Some(b"CD001".as_slice())
}

/// Indexes an ISO 9660 image without materializing ordinary member payloads.
pub(crate) fn read_entries(
	source: &mut (impl Read + Seek),
	file_size: u64,
	limits: Limits,
	_decoded: &mut Vec<Vec<u8>>,
) -> Result<Vec<Entry>> {
	let mut budget = MetadataBudget::new(limits);
	let volume = read_volume(source, file_size, limits, &mut budget)?;
	let mut entries = Vec::new();
	let mut positions = HashMap::<Str, usize>::new();
	walk_tree(
		source,
		file_size,
		limits,
		&mut budget,
		&volume.root,
		volume.joliet,
		WalkMode::Index,
		&mut entries,
		&mut positions,
	)?;
	if let Some(root) = &volume.rock_ridge_root {
		walk_tree(
			source,
			file_size,
			limits,
			&mut budget,
			root,
			false,
			WalkMode::MergeRockRidge,
			&mut entries,
			&mut positions,
		)?;
	}
	Ok(entries)
}

/// Extracts a multi-extent or interleaved ISO member.
pub(crate) fn read_entry_to<W: Write>(
	source: &mut (impl Read + Seek),
	entry: &Entry,
	output: &mut W,
) -> Result<u64> {
	let Storage::Iso { block_size, stored_size, extents } = &entry.storage else {
		return Err(Error::InvalidArchive("entry is not a ranged ISO member"));
	};
	let mut written = 0_u64;
	for extent in extents {
		let unit_bytes = u64::from(extent.file_unit_blocks)
			.checked_mul(*block_size)
			.ok_or(Error::InvalidArchive("ISO interleave unit size overflows"))?;
		let gap_bytes = u64::from(extent.interleave_blocks)
			.checked_mul(*block_size)
			.ok_or(Error::InvalidArchive("ISO interleave gap size overflows"))?;
		let mut remaining = extent.size;
		let mut position = extent.data_offset;
		if unit_bytes == 0 {
			copy_extent(source, position, remaining, output)?;
			written = checked_add(written, remaining, "ISO extracted size overflows")?;
			continue;
		}
		while remaining != 0 {
			let length = cmp::min(unit_bytes, remaining);
			copy_extent(source, position, length, output)?;
			written = checked_add(written, length, "ISO extracted size overflows")?;
			remaining -= length;
			position = position
				.checked_add(unit_bytes)
				.and_then(|position| position.checked_add(gap_bytes))
				.ok_or(Error::InvalidArchive("ISO interleaved member offset overflows"))?;
		}
	}
	if written != *stored_size || written != entry.size {
		return Err(Error::SizeMismatch {
			path:     entry.path.clone(),
			expected: entry.size,
			actual:   written,
		});
	}
	Ok(written)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WalkMode {
	Index,
	MergeRockRidge,
}

#[allow(
	clippy::too_many_arguments,
	reason = "the walker carries explicit source and policy bounds"
)]
fn walk_tree(
	source: &mut (impl Read + Seek),
	file_size: u64,
	limits: Limits,
	budget: &mut MetadataBudget,
	root: &IsoRecord,
	joliet: bool,
	mode: WalkMode,
	entries: &mut Vec<Entry>,
	positions: &mut HashMap<Str, usize>,
) -> Result<()> {
	let mut root_ancestors = SmallVec::new();
	root_ancestors.push((root.extent, root.size));
	let mut work = vec![DirectoryWork {
		record:    root.clone(),
		parent:    Str::new(""),
		depth:     0,
		ancestors: root_ancestors,
	}];
	let mut susp_skip = None;
	while let Some(directory) = work.pop() {
		if directory.depth > MAX_DIRECTORY_DEPTH {
			return Err(Error::InvalidArchive("ISO directory hierarchy exceeds 256 levels"));
		}
		let records = read_directory_records(source, file_size, limits, budget, &directory.record)?;
		if directory.depth == 0
			&& records
				.first()
				.is_some_and(|record| record.identifier.as_slice() == [0])
		{
			susp_skip = find_susp_skip(&records[0]);
		}
		let mut index = 0;
		while index < records.len() {
			let record = &records[index];
			if record.identifier.as_slice() == [0] || record.identifier.as_slice() == [1] {
				index += 1;
				continue;
			}

			let first = index;
			if record.flags & 0x80 != 0 {
				if record.flags & 0x02 != 0 {
					return Err(Error::UnsupportedFeature("multi-extent ISO directories"));
				}
				loop {
					index += 1;
					let next = records
						.get(index)
						.ok_or(Error::InvalidArchive("unterminated ISO multi-extent file"))?;
					if next.identifier != record.identifier {
						return Err(Error::InvalidArchive("non-contiguous ISO multi-extent file"));
					}
					if next.flags & !0x80 != record.flags & !0x80 {
						return Err(Error::InvalidArchive("inconsistent ISO multi-extent file flags"));
					}
					if next.flags & 0x80 == 0 {
						break;
					}
				}
			}
			let last = index;
			index += 1;

			let susp = parse_susp(record, susp_skip, source, file_size, limits, budget)?;
			if let Some(relocation) = susp.relocation {
				return Err(match &relocation {
					b"RE" => Error::UnsupportedFeature("Rock Ridge relocated directory (RE)"),
					b"CL" => Error::UnsupportedFeature("Rock Ridge relocated directory (CL)"),
					_ => Error::UnsupportedFeature("Rock Ridge relocated directory (PL)"),
				});
			}
			let raw_name = match susp.name {
				Some(name) => name,
				None => decode_identifier(&record.identifier, joliet)?,
			};
			check_path_bytes(raw_name.len() as u64, limits)?;
			let raw_path = join_member_path(directory.parent.as_str(), &raw_name)?;
			let Some(path) = normalize(raw_path.as_str(), false) else {
				continue;
			};
			validate(&path, limits)?;

			if let Some(target) = susp.symlink {
				check_path_bytes(target.len() as u64, limits)?;
				let (target_path, resolve_target) =
					resolve_link_target(path.as_str(), &target, limits)?;
				upsert_entry(
					Entry {
						path,
						directory: false,
						size: 0,
						modified_unix_seconds: record.modified_unix_seconds,
						mode: susp.mode,
						storage: Storage::Link { target_path, resolve_target },
					},
					limits,
					entries,
					positions,
				)?;
				continue;
			}

			if mode == WalkMode::MergeRockRidge {
				if let Some(position) = positions.get(&path).copied()
					&& susp.mode.is_some()
				{
					entries[position].mode = susp.mode;
				}
				if record.flags & 0x02 == 0 {
					continue;
				}
				push_directory_work(&directory, record, path, &mut work)?;
				continue;
			}

			if record.flags & 0x02 != 0 {
				upsert_entry(
					Entry {
						path:                  path.clone(),
						directory:             true,
						size:                  0,
						modified_unix_seconds: record.modified_unix_seconds,
						mode:                  susp.mode,
						storage:               Storage::Synthetic,
					},
					limits,
					entries,
					positions,
				)?;
				push_directory_work(&directory, record, path, &mut work)?;
				continue;
			}

			let mut total_size = 0_u64;
			let mut extents = SmallVec::<IsoExtent, 2>::new();
			for part in &records[first..=last] {
				total_size =
					checked_add(total_size, u64::from(part.size), "ISO member size overflows")?;
				extents.push(extent_for_record(part)?);
			}
			if total_size > limits.member_size {
				return Err(Error::MemberTooLarge {
					path:   path.clone(),
					actual: total_size,
					limit:  limits.member_size,
				});
			}
			// Zero-length members never read their extents, and writers record
			// junk locations for them (bsdtar's Joliet records for Rock Ridge
			// symlinks, empty files at unallocated blocks) — 7-Zip and bsdtar
			// both accept these, so validate extents only when bytes exist.
			if total_size == 0 {
				extents.clear();
			}
			for extent in &extents {
				let end = checked_add(extent.data_offset, extent.size, "ISO file extent overflows")?;
				if end > file_size {
					return Err(Error::InvalidArchive("ISO file extent extends past the image"));
				}
			}
			let storage = if extents.is_empty() {
				Storage::Raw { data_offset: 0, stored_size: 0 }
			} else if extents.len() == 1 && extents[0].file_unit_blocks == 0 {
				Storage::Raw { data_offset: extents[0].data_offset, stored_size: total_size }
			} else {
				Storage::Iso { block_size: SECTOR_SIZE, stored_size: total_size, extents }
			};
			upsert_entry(
				Entry {
					path,
					directory: false,
					size: total_size,
					modified_unix_seconds: record.modified_unix_seconds,
					mode: susp.mode,
					storage,
				},
				limits,
				entries,
				positions,
			)?;
		}
	}
	Ok(())
}

fn read_volume(
	source: &mut (impl Read + Seek),
	file_size: u64,
	limits: Limits,
	budget: &mut MetadataBudget,
) -> Result<IsoVolume> {
	if file_size < DESCRIPTOR_START + SECTOR_SIZE {
		return Err(Error::InvalidArchive("truncated ISO volume descriptor set"));
	}
	let mut position = DESCRIPTOR_START;
	let mut primary = None;
	let mut joliet = None;
	let mut saw_high_sierra = false;
	let mut saw_udf = false;
	let mut terminated = false;
	while position < file_size {
		if budget.read.saturating_add(SECTOR_SIZE) > limits.index_size {
			return Err(Error::IndexTooLarge {
				actual: budget.read.saturating_add(SECTOR_SIZE),
				limit:  limits.index_size,
			});
		}
		let descriptor = budget.read(
			source,
			file_size,
			position,
			SECTOR_SIZE,
			"truncated ISO volume descriptor",
		)?;
		let kind = descriptor[0];
		if descriptor.get(1..6) == Some(b"CDROM".as_slice())
			|| descriptor.get(9..14) == Some(b"CDROM".as_slice())
		{
			saw_high_sierra = true;
		}
		if matches!(descriptor.get(1..6), Some(b"BEA01" | b"NSR02" | b"NSR03" | b"TEA01")) {
			saw_udf = true;
		}
		if descriptor.get(1..6) == Some(b"CD001".as_slice()) {
			if descriptor[6] != 1 && descriptor[6] != 2 {
				return Err(Error::InvalidArchive("unsupported ISO volume descriptor version"));
			}
			if kind == 1 {
				primary = Some(parse_volume_descriptor(&descriptor, false)?);
			}
			if kind == 2
				&& descriptor[88] == 0x25
				&& descriptor[89] == 0x2f
				&& matches!(descriptor[90], 0x40 | 0x43 | 0x45)
			{
				joliet = Some(parse_volume_descriptor(&descriptor, true)?);
			}
			if kind == 255 {
				terminated = true;
				break;
			}
		}
		position = checked_add(position, SECTOR_SIZE, "ISO descriptor offset overflows")?;
	}
	if primary.is_none() && joliet.is_none() {
		if saw_high_sierra {
			return Err(Error::UnsupportedFeature("High Sierra CD-ROM filesystem (not ISO 9660)"));
		}
		if saw_udf {
			return Err(Error::UnsupportedFeature("UDF-only image (no ISO 9660 volume descriptor)"));
		}
		return Err(Error::InvalidArchive("ISO primary volume descriptor not found"));
	}
	if !terminated {
		return Err(Error::InvalidArchive("ISO volume descriptor terminator not found"));
	}
	if let Some(mut joliet) = joliet {
		joliet.rock_ridge_root = primary.map(|volume| volume.root);
		return Ok(joliet);
	}
	primary.ok_or(Error::InvalidArchive("ISO primary volume descriptor not found"))
}

fn parse_volume_descriptor(descriptor: &[u8], joliet: bool) -> Result<IsoVolume> {
	both_endian_u32(descriptor, 80, "ISO volume space size has mismatched byte orders")?;
	both_endian_u16(descriptor, 120, "ISO volume set size has mismatched byte orders")?;
	both_endian_u16(descriptor, 124, "ISO volume sequence number has mismatched byte orders")?;
	let block_size =
		both_endian_u16(descriptor, 128, "ISO logical block size has mismatched byte orders")?;
	both_endian_u32(descriptor, 132, "ISO path table size has mismatched byte orders")?;
	if u64::from(block_size) != SECTOR_SIZE {
		return Err(Error::UnsupportedFeature("ISO 9660 logical block sizes other than 2048 bytes"));
	}
	let root =
		parse_record(descriptor, 156, descriptor.len() - 156, "invalid ISO root directory record")?;
	if root.flags & 0x02 == 0 || root.size == 0 || root.identifier.as_slice() != [0] {
		return Err(Error::InvalidArchive("invalid ISO root directory record"));
	}
	Ok(IsoVolume { root, joliet, rock_ridge_root: None })
}

fn read_directory_records(
	source: &mut (impl Read + Seek),
	file_size: u64,
	limits: Limits,
	budget: &mut MetadataBudget,
	record: &IsoRecord,
) -> Result<Vec<IsoRecord>> {
	let block = u64::from(record.extent)
		.checked_add(u64::from(record.extended_attribute_blocks))
		.ok_or(Error::InvalidArchive("ISO directory extent block overflows"))?;
	let start = block
		.checked_mul(SECTOR_SIZE)
		.ok_or(Error::InvalidArchive("ISO directory extent offset overflows"))?;
	let bytes = budget.read(
		source,
		file_size,
		start,
		u64::from(record.size),
		"truncated ISO directory extent",
	)?;
	let mut records = Vec::new();
	let mut offset = 0_usize;
	while offset < bytes.len() {
		let sector_remaining = SECTOR_SIZE as usize - offset % SECTOR_SIZE as usize;
		let length = bytes[offset] as usize;
		if length == 0 {
			offset = offset
				.checked_add(sector_remaining)
				.ok_or(Error::InvalidArchive("ISO directory offset overflows"))?;
			continue;
		}
		if length > sector_remaining || length > bytes.len() - offset {
			return Err(Error::InvalidArchive("ISO directory record crosses a logical block"));
		}
		records.push(parse_record(&bytes, offset, length, "invalid ISO member directory record")?);
		if records.len() as u64 > limits.entries {
			return Err(Error::TooManyEntries {
				actual: records.len() as u64,
				limit:  limits.entries,
			});
		}
		offset += length;
	}
	Ok(records)
}

fn parse_record(
	bytes: &[u8],
	offset: usize,
	available: usize,
	error: &'static str,
) -> Result<IsoRecord> {
	if available < 34
		|| offset
			.checked_add(available)
			.is_none_or(|end| end > bytes.len())
	{
		return Err(Error::InvalidArchive(error));
	}
	let length = bytes[offset] as usize;
	if length < 34 || length > available {
		return Err(Error::InvalidArchive(error));
	}
	let identifier_length = bytes[offset + 32] as usize;
	let padding = usize::from(identifier_length % 2 == 0);
	let system_use_offset = 33_usize
		.checked_add(identifier_length)
		.and_then(|value| value.checked_add(padding))
		.ok_or(Error::InvalidArchive(error))?;
	if identifier_length == 0 || system_use_offset > length {
		return Err(Error::InvalidArchive(error));
	}
	let extent = both_endian_u32(bytes, offset + 2, "ISO extent has mismatched byte orders")?;
	let size = both_endian_u32(bytes, offset + 10, "ISO data length has mismatched byte orders")?;
	both_endian_u16(bytes, offset + 28, "ISO volume sequence has mismatched byte orders")?;
	let file_unit_blocks = bytes[offset + 26];
	let interleave_blocks = bytes[offset + 27];
	if (file_unit_blocks == 0) != (interleave_blocks == 0) {
		return Err(Error::InvalidArchive("invalid ISO interleave configuration"));
	}
	Ok(IsoRecord {
		extent,
		size,
		extended_attribute_blocks: bytes[offset + 1],
		modified_unix_seconds: recording_time(&bytes[offset + 18..offset + 25]),
		flags: bytes[offset + 25],
		file_unit_blocks,
		interleave_blocks,
		identifier: bytes[offset + 33..offset + 33 + identifier_length].to_vec(),
		system_use: bytes[offset + system_use_offset..offset + length].to_vec(),
	})
}

fn find_susp_skip(record: &IsoRecord) -> Option<usize> {
	let bytes = &record.system_use;
	let mut offset = 0;
	while offset + 7 <= bytes.len() {
		let length = bytes[offset + 2] as usize;
		if length < 4 || offset + length > bytes.len() {
			return None;
		}
		if bytes.get(offset..offset + 2) == Some(b"SP".as_slice())
			&& length == 7
			&& bytes[offset + 4] == 0xbe
			&& bytes[offset + 5] == 0xef
		{
			return Some(bytes[offset + 6] as usize);
		}
		offset += length;
	}
	None
}

fn parse_susp(
	record: &IsoRecord,
	skip: Option<usize>,
	source: &mut (impl Read + Seek),
	file_size: u64,
	limits: Limits,
	budget: &mut MetadataBudget,
) -> Result<SuspData> {
	let Some(skip) = skip.filter(|skip| *skip <= record.system_use.len()) else {
		return Ok(SuspData::default());
	};
	let mut areas = VecDeque::from([record.system_use[skip..].to_vec()]);
	let mut continuation_ranges = HashSet::new();
	let mut name = String::new();
	let mut has_name = false;
	let mut link_parts = Vec::<String>::new();
	let mut link_component_continues = false;
	let mut mode = None;
	let mut relocation = None;
	while let Some(area) = areas.pop_front() {
		let mut offset = 0;
		while offset + 4 <= area.len() {
			let signature = [area[offset], area[offset + 1]];
			let length = area[offset + 2] as usize;
			let version = area[offset + 3];
			if length < 4 || offset + length > area.len() {
				return Err(Error::InvalidArchive("malformed ISO SUSP record"));
			}
			if version != 1
				&& matches!(&signature, b"CE" | b"NM" | b"PX" | b"RE" | b"CL" | b"PL" | b"SL" | b"ST")
			{
				return Err(Error::InvalidArchive("unsupported ISO SUSP record version"));
			}
			let data = &area[offset + 4..offset + length];
			if &signature == b"ST" {
				break;
			}
			if &signature == b"CE" {
				if data.len() < 24 {
					return Err(Error::InvalidArchive("short ISO SUSP continuation record"));
				}
				let block = u64::from(both_endian_u32(
					data,
					0,
					"ISO SUSP continuation block has mismatched byte orders",
				)?);
				let block_offset = u64::from(both_endian_u32(
					data,
					8,
					"ISO SUSP continuation offset has mismatched byte orders",
				)?);
				let length = u64::from(both_endian_u32(
					data,
					16,
					"ISO SUSP continuation length has mismatched byte orders",
				)?);
				if length > limits.index_size {
					return Err(Error::IndexTooLarge { actual: length, limit: limits.index_size });
				}
				let start = block
					.checked_mul(SECTOR_SIZE)
					.and_then(|position| position.checked_add(block_offset))
					.ok_or(Error::InvalidArchive("ISO SUSP continuation offset overflows"))?;
				let end = checked_add(start, length, "ISO SUSP continuation range overflows")?;
				if !continuation_ranges.insert((start, end)) {
					return Err(Error::InvalidArchive("cyclic ISO SUSP continuation"));
				}
				if continuation_ranges.len() > MAX_SUSP_CONTINUATIONS {
					return Err(Error::InvalidArchive("too many ISO SUSP continuations"));
				}
				areas.push_back(budget.read(
					source,
					file_size,
					start,
					length,
					"truncated ISO SUSP continuation",
				)?);
			}
			if &signature == b"NM" && !data.is_empty() {
				has_name = true;
				if data[0] & 0x06 == 0 {
					name.push_str(&decode_utf8(&data[1..]));
				} else if data[0] & 0x02 != 0 {
					name.push('.');
				} else if data[0] & 0x04 != 0 {
					name.push_str("..");
				}
			}
			if &signature == b"PX" && data.len() >= 8 {
				mode = Some(both_endian_u32(data, 0, "Rock Ridge PX mode has mismatched byte orders")?);
			}
			if matches!(&signature, b"RE" | b"CL" | b"PL") {
				relocation = Some(signature);
			}
			if &signature == b"SL" && !data.is_empty() {
				let mut component_offset = 1;
				while component_offset < data.len() {
					if component_offset + 2 > data.len() {
						return Err(Error::InvalidArchive("malformed Rock Ridge SL component"));
					}
					let flags = data[component_offset];
					let component_length = data[component_offset + 1] as usize;
					component_offset += 2;
					if component_offset + component_length > data.len() {
						return Err(Error::InvalidArchive("malformed Rock Ridge SL component"));
					}
					let component = if flags & 0x08 != 0 {
						String::new()
					} else if flags & 0x04 != 0 {
						String::from("..")
					} else if flags & 0x02 != 0 {
						String::from(".")
					} else {
						decode_utf8(&data[component_offset..component_offset + component_length])
					};
					if link_component_continues && !link_parts.is_empty() {
						link_parts
							.last_mut()
							.expect("checked nonempty")
							.push_str(&component);
					} else {
						link_parts.push(component);
					}
					link_component_continues = flags & 0x01 != 0;
					component_offset += component_length;
				}
			}
			offset += length;
		}
	}
	let symlink = if link_parts.is_empty() {
		None
	} else if link_parts[0].is_empty() {
		Some(format!("/{}", link_parts[1..].join("/")))
	} else {
		Some(link_parts.join("/"))
	};
	Ok(SuspData { name: has_name.then_some(name), mode, symlink, relocation })
}

fn extent_for_record(record: &IsoRecord) -> Result<IsoExtent> {
	let block = u64::from(record.extent)
		.checked_add(u64::from(record.extended_attribute_blocks))
		.ok_or(Error::InvalidArchive("ISO file extent block overflows"))?;
	let data_offset = block
		.checked_mul(SECTOR_SIZE)
		.ok_or(Error::InvalidArchive("ISO file extent offset overflows"))?;
	Ok(IsoExtent {
		data_offset,
		size: u64::from(record.size),
		file_unit_blocks: record.file_unit_blocks,
		interleave_blocks: record.interleave_blocks,
	})
}

fn push_directory_work(
	parent_work: &DirectoryWork,
	record: &IsoRecord,
	path: Str,
	work: &mut Vec<DirectoryWork>,
) -> Result<()> {
	let key = (record.extent, record.size);
	if parent_work.ancestors.contains(&key) {
		return Err(Error::InvalidArchive("cyclic ISO directory extent"));
	}
	let depth = parent_work
		.depth
		.checked_add(1)
		.ok_or(Error::InvalidArchive("ISO directory depth overflows"))?;
	let mut ancestors = parent_work.ancestors.clone();
	ancestors.push(key);
	work.push(DirectoryWork { record: record.clone(), parent: path, depth, ancestors });
	Ok(())
}

fn upsert_entry(
	entry: Entry,
	limits: Limits,
	entries: &mut Vec<Entry>,
	positions: &mut HashMap<Str, usize>,
) -> Result<()> {
	if let Some(position) = positions.get(&entry.path).copied() {
		let existing = &entries[position];
		if existing.directory && !entry.directory || existing.directory == entry.directory {
			entries[position] = entry;
		}
		return Ok(());
	}
	let actual = entries.len() as u64 + 1;
	if actual > limits.entries {
		return Err(Error::TooManyEntries { actual, limit: limits.entries });
	}
	positions.insert(entry.path.clone(), entries.len());
	entries.push(entry);
	Ok(())
}

fn resolve_link_target(path: &str, target: &str, limits: Limits) -> Result<(Str, bool)> {
	if target.starts_with('/') {
		return Ok((Str::new(target), false));
	}
	let mut parts = SmallVec::<&str, 16>::new();
	for component in parent(path)
		.split('/')
		.filter(|component| !component.is_empty())
	{
		parts.push(component);
	}
	for component in target.split('/') {
		match component {
			"" | "." => {},
			".." if parts.pop().is_none() => return Ok((Str::new(target), false)),
			".." => {},
			component => parts.push(component),
		}
	}
	let length = parts.iter().map(|part| part.len()).sum::<usize>() + parts.len().saturating_sub(1);
	let mut resolved = StrMut::with_capacity(length);
	for (index, part) in parts.iter().enumerate() {
		if index != 0 {
			resolved.push('/');
		}
		resolved.push_str(part);
	}
	let resolved = resolved.freeze();
	let Some(resolved) = normalize(resolved.as_str(), true) else {
		return Ok((Str::new(target), false));
	};
	validate(&resolved, limits)?;
	Ok((resolved, true))
}

fn join_member_path(parent: &str, name: &str) -> Result<Str> {
	let separator = usize::from(!parent.is_empty());
	let length = parent
		.len()
		.checked_add(separator)
		.and_then(|length| length.checked_add(name.len()))
		.ok_or(Error::InvalidArchive("ISO member path length overflows"))?;
	let mut path = StrMut::with_capacity(length);
	path.push_str(parent);
	if separator != 0 {
		path.push('/');
	}
	path.push_str(name);
	Ok(path.freeze())
}

fn decode_identifier(identifier: &[u8], joliet: bool) -> Result<String> {
	let mut name = if joliet {
		if identifier.len() % 2 != 0 {
			return Err(Error::InvalidArchive("Joliet identifier has an odd byte length"));
		}
		let mut units = Vec::with_capacity(identifier.len() / 2);
		for bytes in identifier.chunks_exact(2) {
			units.push(u16::from_be_bytes([bytes[0], bytes[1]]));
		}
		let bytes = xutf::transcode::<Utf16, Utf8>(&units);
		String::from_units(bytes)
	} else {
		decode_utf8(identifier)
	};
	if name.ends_with(";1") {
		name.truncate(name.len() - 2);
	}
	Ok(name)
}

fn decode_utf8(bytes: &[u8]) -> String {
	String::from_units(xutf::transcode::<Utf8, Utf8>(bytes))
}

fn both_endian_u16(bytes: &[u8], offset: usize, mismatch: &'static str) -> Result<u16> {
	let little = u16::from_le_bytes(
		bytes
			.get(offset..offset + 2)
			.ok_or(Error::InvalidArchive("truncated ISO both-endian field"))?
			.try_into()
			.expect("two-byte slice"),
	);
	let big = u16::from_be_bytes(
		bytes
			.get(offset + 2..offset + 4)
			.ok_or(Error::InvalidArchive("truncated ISO both-endian field"))?
			.try_into()
			.expect("two-byte slice"),
	);
	if little != big {
		return Err(Error::InvalidArchive(mismatch));
	}
	Ok(little)
}

fn both_endian_u32(bytes: &[u8], offset: usize, mismatch: &'static str) -> Result<u32> {
	let little = u32::from_le_bytes(
		bytes
			.get(offset..offset + 4)
			.ok_or(Error::InvalidArchive("truncated ISO both-endian field"))?
			.try_into()
			.expect("four-byte slice"),
	);
	let big = u32::from_be_bytes(
		bytes
			.get(offset + 4..offset + 8)
			.ok_or(Error::InvalidArchive("truncated ISO both-endian field"))?
			.try_into()
			.expect("four-byte slice"),
	);
	if little != big {
		return Err(Error::InvalidArchive(mismatch));
	}
	Ok(little)
}

fn recording_time(bytes: &[u8]) -> Option<u64> {
	let [year, month, day, hour, minute, second, zone] = *<&[u8; 7]>::try_from(bytes).ok()?;
	let year = i64::from(year) + 1900;
	let month = i64::from(month);
	let day = i64::from(day);
	let hour = i64::from(hour);
	let minute = i64::from(minute);
	let second = i64::from(second.min(59));
	let zone = i64::from(zone as i8);
	if !(1..=12).contains(&month)
		|| !(1..=31).contains(&day)
		|| hour > 23
		|| minute > 59
		|| bytes[5] > 60
		|| !(-48..=52).contains(&zone)
	{
		return None;
	}
	let adjusted_year = year - i64::from(month <= 2);
	let era = if adjusted_year >= 0 {
		adjusted_year
	} else {
		adjusted_year - 399
	} / 400;
	let year_of_era = adjusted_year - era * 400;
	let shifted_month = month + if month > 2 { -3 } else { 9 };
	let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
	let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
	let days = era * 146_097 + day_of_era - 719_468;
	let seconds = days
		.checked_mul(86_400)?
		.checked_add(hour * 3600 + minute * 60 + second)?
		.checked_sub(zone * 15 * 60)?;
	u64::try_from(seconds).ok()
}

fn check_path_bytes(actual: u64, limits: Limits) -> Result<()> {
	if actual > limits.path_size {
		return Err(Error::PathTooLong { actual, limit: limits.path_size });
	}
	Ok(())
}

fn checked_add(left: u64, right: u64, error: &'static str) -> Result<u64> {
	left.checked_add(right).ok_or(Error::InvalidArchive(error))
}

fn copy_extent<W: Write>(
	source: &mut (impl Read + Seek),
	position: u64,
	length: u64,
	output: &mut W,
) -> Result<()> {
	source.seek(SeekFrom::Start(position))?;
	let copied = io::copy(&mut source.take(length), output)?;
	if copied != length {
		return Err(Error::InvalidArchive("truncated ISO member extent"));
	}
	Ok(())
}

fn map_truncated(error: io::Error, message: &'static str) -> Error {
	if error.kind() == io::ErrorKind::UnexpectedEof {
		Error::InvalidArchive(message)
	} else {
		Error::Io(error)
	}
}
