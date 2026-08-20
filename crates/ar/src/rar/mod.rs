//! Bounded RAR 4.x/5.x indexing and member decompression.

mod rar4_decoder;
mod rar5_decoder;

use std::io::{Read, Seek, SeekFrom, Write};

use omp_core::Str;
use xutf::Utf16Le;

use self::{rar4_decoder::Rar4Decoder, rar5_decoder::Rar5Decoder};
use crate::{
	Entry, Error, Limits, Result,
	entry::Storage,
	path::{normalize, validate},
};

const RAR4_MARKER: &[u8; 7] = b"Rar!\x1a\x07\x00";
const RAR5_MARKER: &[u8; 8] = b"Rar!\x1a\x07\x01\x00";
const MAX_SFX_SCAN: u64 = 1024 * 1024 + RAR5_MARKER.len() as u64;
const MAX_RAR5_HEADER: u64 = 2 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum RarVersion {
	Rar4,
	Rar5,
}

impl RarVersion {
	const fn wire(self) -> u8 {
		match self {
			Self::Rar4 => 4,
			Self::Rar5 => 5,
		}
	}
}

struct Record {
	format:          RarVersion,
	path:            Str,
	data_start:      u64,
	packed_size:     u64,
	unpacked_size:   u64,
	method:          u8,
	version:         u8,
	dictionary_size: u64,
	solid:           bool,
	crc32:           Option<u32>,
	directory:       bool,
	mtime:           Option<u64>,
	mode:            Option<u32>,
	link:            Option<(Str, bool)>,
}

/// Returns whether bytes contain a RAR 1.5-5.x signature in the bounded SFX
/// prefix region.
pub fn is_header(bytes: &[u8]) -> bool {
	find_marker(bytes).is_some()
}

/// Indexes RAR headers without reading ordinary member payloads. Solid chains
/// are eagerly decoded into retained buffers because their dictionary state is
/// shared across members.
pub(crate) fn read_entries(
	source: &mut (impl Read + Seek),
	file_size: u64,
	limits: Limits,
	decoded: &mut Vec<Vec<u8>>,
) -> Result<Vec<Entry>> {
	if file_size > limits.archive_size {
		return Err(Error::ArchiveTooLarge { actual: file_size, limit: limits.archive_size });
	}
	let (format, marker) = scan_marker(source, file_size, limits)?;
	let records = match format {
		RarVersion::Rar5 => parse_rar5(source, file_size, marker, limits)?,
		RarVersion::Rar4 => parse_rar4(source, file_size, marker, limits)?,
	};
	let mut solid_buffers = vec![None; records.len()];
	decode_solid_chains(source, &records, limits, decoded, &mut solid_buffers)?;

	let mut entries = Vec::with_capacity(records.len());
	for (index, record) in records.into_iter().enumerate() {
		let storage = if record.directory {
			Storage::Synthetic
		} else if let Some((target_path, resolve_target)) = record.link {
			Storage::Link { target_path, resolve_target }
		} else if let Some(buffer) = solid_buffers[index] {
			Storage::Buffered { buffer, data_offset: 0, stored_size: record.unpacked_size }
		} else {
			Storage::Rar {
				data_offset:     record.data_start,
				packed_size:     record.packed_size,
				dictionary_size: record.dictionary_size,
				crc32:           record.crc32,
				format:          record.format.wire(),
				method:          record.method,
				version:         record.version,
			}
		};
		entries.push(Entry {
			path: record.path,
			directory: record.directory,
			size: record.unpacked_size,
			modified_unix_seconds: record.mtime,
			mode: record.mode,
			storage,
		});
	}
	Ok(entries)
}

/// Extracts and checksum-verifies one non-solid RAR member.
pub(crate) fn read_entry_to<W: Write>(
	source: &mut (impl Read + Seek),
	entry: &Entry,
	output: &mut W,
) -> Result<u64> {
	let Storage::Rar { data_offset, packed_size, dictionary_size, crc32, format, method, version } =
		&entry.storage
	else {
		return Err(Error::InvalidArchive("invalid RAR member storage"));
	};
	let packed_len = usize::try_from(*packed_size)
		.map_err(|_| Error::InvalidArchive("RAR packed size overflows platform limits"))?;
	let unpacked_len = usize::try_from(entry.size)
		.map_err(|_| Error::InvalidArchive("RAR unpacked size overflows platform limits"))?;
	let dictionary_len = usize::try_from(*dictionary_size)
		.map_err(|_| Error::InvalidArchive("RAR dictionary size overflows platform limits"))?;
	let packed = read_at(source, *data_offset, packed_len)?;
	let bytes = decode_member(
		&packed,
		unpacked_len,
		dictionary_len,
		*format,
		*method,
		*version,
		false,
		&mut Rar4Decoder::default(),
		&mut Rar5Decoder::default(),
	)?;
	verify_member(entry.path.as_str(), entry.size, *crc32, &bytes)?;
	output.write_all(&bytes)?;
	Ok(bytes.len() as u64)
}

fn scan_marker(
	source: &mut (impl Read + Seek),
	file_size: u64,
	limits: Limits,
) -> Result<(RarVersion, u64)> {
	let scan_limit = file_size.min(MAX_SFX_SCAN);
	let mut end = scan_limit.min(RAR5_MARKER.len() as u64);
	let mut probe = read_at(source, 0, usize_from_u64(end, "RAR signature scan")?)?;
	loop {
		if let Some((version, offset)) = find_marker(&probe) {
			return Ok((version, offset as u64));
		}
		if end >= scan_limit {
			return Err(Error::InvalidArchive("RAR signature not found"));
		}
		let next_end = scan_limit.min((end.saturating_mul(2)).max(64 * 1024));
		check_index_size(next_end, limits)?;
		let next = read_at(source, end, usize_from_u64(next_end - end, "RAR signature scan")?)?;
		probe.extend_from_slice(&next);
		end = next_end;
	}
}

fn find_marker(bytes: &[u8]) -> Option<(RarVersion, usize)> {
	let limit = bytes.len().min(MAX_SFX_SCAN as usize);
	for offset in 0..limit {
		if bytes.get(offset..offset + RAR5_MARKER.len()) == Some(RAR5_MARKER) {
			return Some((RarVersion::Rar5, offset));
		}
		if bytes.get(offset..offset + RAR4_MARKER.len()) == Some(RAR4_MARKER) {
			return Some((RarVersion::Rar4, offset));
		}
	}
	None
}

fn parse_rar5(
	source: &mut (impl Read + Seek),
	file_size: u64,
	marker: u64,
	limits: Limits,
) -> Result<Vec<Record>> {
	let mut records = Vec::new();
	let mut offset = marker + RAR5_MARKER.len() as u64;
	let mut metadata_size = offset;
	check_index_size(metadata_size, limits)?;
	let mut saw_main = false;
	while offset < file_size {
		let prefix_len = usize_from_u64((file_size - offset).min(16), "RAR5 header")?;
		if prefix_len < 5 {
			return invalid("truncated RAR5 header");
		}
		let prefix = read_at(source, offset, prefix_len)?;
		let mut size_cursor = 4usize;
		let header_size = read_vint(&prefix, &mut size_cursor, prefix.len(), "RAR5 header size")?;
		if header_size > MAX_RAR5_HEADER {
			return invalid("RAR5 header exceeds format limit");
		}
		let total_size = (size_cursor as u64)
			.checked_add(header_size)
			.ok_or(Error::InvalidArchive("RAR5 header size overflows"))?;
		let header_end = checked_end(offset, total_size, file_size, "RAR5 header")?;
		metadata_size = metadata_size
			.checked_add(total_size)
			.ok_or(Error::InvalidArchive("RAR metadata size overflows"))?;
		check_index_size(metadata_size, limits)?;
		let header = read_at(source, offset, usize_from_u64(total_size, "RAR5 header")?)?;
		let expected_crc = read_u32(&header, 0)?;
		if crc32fast::hash(&header[4..]) != expected_crc {
			return invalid("RAR5 header CRC32 mismatch");
		}
		let mut cursor = size_cursor;
		let header_len = header.len();
		let kind = read_vint(&header, &mut cursor, header_len, "RAR5 header type")?;
		let flags = read_vint(&header, &mut cursor, header_len, "RAR5 header flags")?;
		let extra_size = if flags & 1 != 0 {
			read_vint(&header, &mut cursor, header_len, "RAR5 extra area size")?
		} else {
			0
		};
		let data_size = if flags & 2 != 0 {
			read_vint(&header, &mut cursor, header_len, "RAR5 data size")?
		} else {
			0
		};
		if flags & 0x18 != 0 {
			return Err(Error::UnsupportedFeature("multi-volume RAR5 archive"));
		}
		let data_start = header_end;
		let data_end = checked_end(data_start, data_size, file_size, "RAR5 data area")?;
		if extra_size > (header.len() - cursor) as u64 {
			return invalid("invalid RAR5 extra area size");
		}
		let extra_start = header.len() - usize_from_u64(extra_size, "RAR5 extra area")?;

		match kind {
			4 => return Err(Error::Encrypted(Str::new("RAR5 headers"))),
			1 => {
				let archive_flags = read_vint(&header, &mut cursor, extra_start, "RAR5 archive flags")?;
				if archive_flags & 1 != 0 {
					return Err(Error::UnsupportedFeature("multi-volume RAR5 archive"));
				}
				if archive_flags & 8 != 0 {
					return Err(Error::UnsupportedFeature("RAR5 recovery record"));
				}
				if archive_flags & 2 != 0 {
					read_vint(&header, &mut cursor, extra_start, "RAR5 volume number")?;
				}
				saw_main = true;
			},
			2 | 3 => {
				if let Some(record) = parse_rar5_file(
					&header,
					&mut cursor,
					extra_start,
					data_start,
					data_size,
					kind,
					limits,
				)? {
					records.push(record);
					check_entry_count(records.len(), limits)?;
				}
			},
			5 => {
				let end_flags = read_vint(&header, &mut cursor, extra_start, "RAR5 end flags")?;
				if end_flags & 1 != 0 {
					return Err(Error::UnsupportedFeature("multi-volume RAR5 archive"));
				}
				break;
			},
			_ if flags & 4 != 0 => {},
			_ => return Err(Error::UnsupportedFeature("RAR5 header type")),
		}
		offset = data_end;
	}
	if !saw_main {
		return invalid("RAR5 main header is missing");
	}
	Ok(records)
}

fn parse_rar5_file(
	header: &[u8],
	cursor: &mut usize,
	extra_start: usize,
	data_start: u64,
	data_size: u64,
	kind: u64,
	limits: Limits,
) -> Result<Option<Record>> {
	let file_flags = read_vint(header, cursor, extra_start, "RAR5 file flags")?;
	let unpacked_size = read_vint(header, cursor, extra_start, "RAR5 unpacked size")?;
	if file_flags & 8 != 0 {
		return Err(Error::UnsupportedFeature("RAR5 member with unknown unpacked size"));
	}
	let attributes = read_vint(header, cursor, extra_start, "RAR5 file attributes")?;
	let mut mtime = None;
	if file_flags & 2 != 0 {
		mtime = Some(u64::from(read_u32_at(header, cursor, extra_start, "RAR5 modification time")?));
	}
	let mut data_crc = None;
	if file_flags & 4 != 0 {
		data_crc = Some(read_u32_at(header, cursor, extra_start, "RAR5 data CRC32")?);
	}
	let compression = read_vint(header, cursor, extra_start, "RAR5 compression information")?;
	let mut version = (compression & 0x3f) as u8;
	if version == 1 && compression & 0x10_0000 != 0 {
		version = 0;
	}
	let solid = compression & 0x40 != 0;
	let method = ((compression >> 7) & 7) as u8;
	let dictionary_power = ((compression >> 10) & 0x1f) as u32;
	let mut dictionary_size = (128u64 * 1024)
		.checked_shl(dictionary_power)
		.ok_or(Error::InvalidArchive("RAR5 dictionary size overflows"))?;
	if compression & 0x3f == 1 {
		dictionary_size = dictionary_size
			.checked_add(dictionary_size * ((compression >> 15) & 0x1f) / 32)
			.ok_or(Error::InvalidArchive("RAR5 dictionary size overflows"))?;
	}
	if dictionary_size > limits.in_memory_size {
		return Err(Error::ArchiveTooLargeInMemory {
			actual: dictionary_size,
			limit:  limits.in_memory_size,
		});
	}
	let host_os = read_vint(header, cursor, extra_start, "RAR5 host OS")?;
	let name_size = read_vint(header, cursor, extra_start, "RAR5 file name size")?;
	if name_size > limits.path_size {
		return Err(Error::PathTooLong { actual: name_size, limit: limits.path_size });
	}
	let name_end = checked_slice_end(*cursor, name_size, extra_start, "RAR5 file name")?;
	let raw_path = std::str::from_utf8(&header[*cursor..name_end])
		.map_err(|_| Error::InvalidArchive("invalid RAR5 UTF-8 member name"))?;
	*cursor = name_end;
	let mut link_target = None;
	let mut extra_cursor = extra_start;
	while extra_cursor < header.len() {
		let record_size =
			read_vint(header, &mut extra_cursor, header.len(), "RAR5 extra record size")?;
		let record_end =
			checked_slice_end(extra_cursor, record_size, header.len(), "RAR5 extra record")?;
		let extra_type = read_vint(header, &mut extra_cursor, record_end, "RAR5 extra record type")?;
		if extra_type == 1 {
			return Err(Error::Encrypted(Str::new(raw_path)));
		}
		if extra_type == 3 {
			let time_flags = read_vint(header, &mut extra_cursor, record_end, "RAR5 time flags")?;
			if time_flags & 2 != 0 {
				mtime = if time_flags & 1 != 0 {
					Some(u64::from(read_u32_at(
						header,
						&mut extra_cursor,
						record_end,
						"RAR5 Unix modification time",
					)?))
				} else {
					let end =
						checked_slice_end(extra_cursor, 8, record_end, "RAR5 Windows modification time")?;
					let ticks =
						u64::from_le_bytes(header[extra_cursor..end].try_into().expect("eight bytes"));
					Some(
						(ticks / 10_000_000)
							.checked_sub(11_644_473_600)
							.ok_or(Error::InvalidArchive("RAR5 FILETIME predates Unix epoch"))?,
					)
				};
			}
		} else if extra_type == 5 {
			let redirect = read_vint(header, &mut extra_cursor, record_end, "RAR5 redirection type")?;
			read_vint(header, &mut extra_cursor, record_end, "RAR5 redirection flags")?;
			let target_size =
				read_vint(header, &mut extra_cursor, record_end, "RAR5 link target size")?;
			if target_size > limits.path_size {
				return Err(Error::PathTooLong { actual: target_size, limit: limits.path_size });
			}
			if !(1..=5).contains(&redirect) {
				return Err(Error::UnsupportedFeature("RAR5 redirection type"));
			}
			let target_end =
				checked_slice_end(extra_cursor, target_size, record_end, "RAR5 link target")?;
			let target = std::str::from_utf8(&header[extra_cursor..target_end])
				.map_err(|_| Error::InvalidArchive("invalid RAR5 UTF-8 link target"))?;
			link_target = Some(target.to_owned());
		}
		extra_cursor = record_end;
	}
	if kind == 3 {
		if raw_path == "RR" {
			return Err(Error::UnsupportedFeature("RAR5 recovery record"));
		}
		return Ok(None);
	}
	check_member_size(unpacked_size, raw_path, limits)?;
	check_decode_memory(data_size, unpacked_size, dictionary_size, method, limits)?;
	let Some(path) = normalize(raw_path, false) else {
		return Ok(None);
	};
	validate(path.as_str(), limits)?;
	let link = link_target.map(|target| canonical_link_target(path.as_str(), &target));
	Ok(Some(Record {
		format: RarVersion::Rar5,
		path,
		data_start,
		packed_size: data_size,
		unpacked_size,
		method,
		version,
		dictionary_size,
		solid,
		crc32: data_crc,
		directory: file_flags & 1 != 0,
		mtime,
		mode: (host_os == 1).then_some(attributes as u32),
		link,
	}))
}

fn parse_rar4(
	source: &mut (impl Read + Seek),
	file_size: u64,
	marker: u64,
	limits: Limits,
) -> Result<Vec<Record>> {
	let mut records = Vec::new();
	let mut offset = marker + RAR4_MARKER.len() as u64;
	let mut metadata_size = offset;
	check_index_size(metadata_size, limits)?;
	let mut saw_main = false;
	while offset < file_size {
		if file_size - offset < 7 {
			return invalid("truncated RAR4 base header");
		}
		let base = read_at(source, offset, 7)?;
		let expected_crc = read_u16(&base, 0)?;
		let kind = base[2];
		let flags = read_u16(&base, 3)?;
		let header_size = u64::from(read_u16(&base, 5)?);
		if header_size < 7 {
			return invalid("invalid RAR4 header size");
		}
		let header_end = checked_end(offset, header_size, file_size, "RAR4 header")?;
		metadata_size = metadata_size
			.checked_add(header_size)
			.ok_or(Error::InvalidArchive("RAR metadata size overflows"))?;
		check_index_size(metadata_size, limits)?;
		let header = read_at(source, offset, usize_from_u64(header_size, "RAR4 header")?)?;
		if (crc32fast::hash(&header[2..]) & 0xffff) != u32::from(expected_crc) {
			return invalid("RAR4 header CRC mismatch");
		}
		let mut cursor = 7usize;
		let mut data_size = 0u64;
		if flags & 0x8000 != 0 {
			data_size =
				u64::from(read_u32_at(&header, &mut cursor, header.len(), "RAR4 additional size")?);
		}
		let data_start = header_end;
		let mut data_end = checked_end(data_start, data_size, file_size, "RAR4 data area")?;
		match kind {
			0x73 => {
				if flags & 1 != 0 {
					return Err(Error::UnsupportedFeature("multi-volume RAR4 archive"));
				}
				if flags & 0x40 != 0 {
					return Err(Error::UnsupportedFeature("RAR4 recovery record"));
				}
				if flags & 0x80 != 0 {
					return Err(Error::Encrypted(Str::new("RAR4 headers")));
				}
				saw_main = true;
			},
			0x74 => {
				let parsed = parse_rar4_file(
					source,
					&header,
					&mut cursor,
					flags,
					data_start,
					data_size,
					file_size,
					limits,
				)?;
				data_end = parsed.1;
				if let Some(record) = parsed.0 {
					records.push(record);
					check_entry_count(records.len(), limits)?;
				}
			},
			0x78 => return Err(Error::UnsupportedFeature("RAR4 recovery record")),
			0x7b => {
				if flags & 1 != 0 {
					return Err(Error::UnsupportedFeature("multi-volume RAR4 archive"));
				}
				break;
			},
			_ => {},
		}
		offset = data_end;
	}
	if !saw_main {
		return invalid("RAR4 main header is missing");
	}
	Ok(records)
}

fn parse_rar4_file(
	source: &mut (impl Read + Seek),
	header: &[u8],
	cursor: &mut usize,
	flags: u16,
	data_start: u64,
	data_size: u64,
	file_size: u64,
	limits: Limits,
) -> Result<(Option<Record>, u64)> {
	let unpacked_low = u64::from(read_u32_at(header, cursor, header.len(), "RAR4 unpacked size")?);
	need(*cursor, 1, header.len(), "RAR4 host OS")?;
	let host_os = header[*cursor];
	*cursor += 1;
	let data_crc = read_u32_at(header, cursor, header.len(), "RAR4 data CRC32")?;
	let dos_time = read_u32_at(header, cursor, header.len(), "RAR4 modification time")?;
	need(*cursor, 2, header.len(), "RAR4 compression fields")?;
	let version = header[*cursor];
	let method_byte = header[*cursor + 1];
	*cursor += 2;
	let name_size = u64::from(read_u16_at(header, cursor, header.len(), "RAR4 file name size")?);
	let attributes = read_u32_at(header, cursor, header.len(), "RAR4 attributes")?;
	let mut packed_size = data_size;
	let mut unpacked_size = unpacked_low;
	if flags & 0x100 != 0 {
		packed_size = packed_size
			.checked_add(
				u64::from(read_u32_at(header, cursor, header.len(), "RAR4 high packed size")?) << 32,
			)
			.ok_or(Error::InvalidArchive("RAR4 packed size overflows"))?;
		unpacked_size = unpacked_size
			.checked_add(
				u64::from(read_u32_at(header, cursor, header.len(), "RAR4 high unpacked size")?) << 32,
			)
			.ok_or(Error::InvalidArchive("RAR4 unpacked size overflows"))?;
	}
	let data_end = checked_end(data_start, packed_size, file_size, "RAR4 file data")?;
	if flags & 3 != 0 {
		return Err(Error::UnsupportedFeature("multi-volume RAR4 member"));
	}
	if flags & 4 != 0 {
		return Err(Error::Encrypted(Str::new("RAR4 file data")));
	}
	if !(0x30..=0x35).contains(&method_byte) {
		return Err(Error::UnsupportedFeature("RAR4 compression method 0x36"));
	}
	if name_size > limits.path_size {
		return Err(Error::PathTooLong { actual: name_size, limit: limits.path_size });
	}
	let name_end = checked_slice_end(*cursor, name_size, header.len(), "RAR4 file name")?;
	let raw_path = if flags & 0x200 != 0 {
		decode_rar4_unicode_name(&header[*cursor..name_end])?
	} else {
		decode_latin1(&header[*cursor..name_end])
	};
	*cursor = name_end;
	if flags & 0x400 != 0 {
		need(*cursor, 8, header.len(), "RAR4 salt")?;
		*cursor += 8;
	}
	let mut mtime = Some(dos_time_seconds(dos_time));
	if flags & 0x1000 != 0 {
		let time_flags = read_u16_at(header, cursor, header.len(), "RAR4 extended time flags")?;
		for time_index in 0..4 {
			let mode = (time_flags >> ((3 - time_index) * 4)) & 15;
			if mode & 8 == 0 {
				continue;
			}
			let time_value = if time_index == 0 {
				dos_time
			} else {
				read_u32_at(header, cursor, header.len(), "RAR4 extended time")?
			};
			let mut precise = dos_time_seconds(time_value) + u64::from(mode & 4 != 0);
			let count = usize::from(mode & 3);
			need(*cursor, count, header.len(), "RAR4 extended time precision")?;
			let mut remainder = 0u32;
			for index in 0..count {
				remainder |= u32::from(header[*cursor + index]) << ((index + 3 - count) * 8);
			}
			*cursor += count;
			if remainder >= 10_000_000 {
				precise = precise.saturating_add(u64::from(remainder / 10_000_000));
			}
			if time_index == 0 {
				mtime = Some(precise);
			}
		}
	}
	check_member_size(unpacked_size, &raw_path, limits)?;
	let method = method_byte - 0x30;
	let dictionary_size = rar4_dictionary(flags);
	check_decode_memory(packed_size, unpacked_size, dictionary_size, method, limits)?;
	let Some(path) = normalize(&raw_path, false) else {
		return Ok((None, data_end));
	};
	validate(path.as_str(), limits)?;
	let directory = ((flags >> 5) & 7) == 7;
	let mode = (host_os == 3).then_some(attributes);
	let link = if mode.is_some_and(|mode| mode & 0xf000 == 0xa000) {
		if method_byte != 0x30 || packed_size != unpacked_size {
			return Err(Error::UnsupportedFeature("compressed RAR4 symlink"));
		}
		if packed_size > limits.path_size {
			return Err(Error::PathTooLong { actual: packed_size, limit: limits.path_size });
		}
		let target_bytes =
			read_at(source, data_start, usize_from_u64(packed_size, "RAR4 link target")?)?;
		if crc32fast::hash(&target_bytes) != data_crc {
			return Err(Error::ChecksumMismatch {
				path:     path.clone(),
				expected: data_crc,
				actual:   crc32fast::hash(&target_bytes),
			});
		}
		Some(canonical_link_target(path.as_str(), &decode_latin1(&target_bytes)))
	} else {
		None
	};
	Ok((
		Some(Record {
			format: RarVersion::Rar4,
			path,
			data_start,
			packed_size,
			unpacked_size,
			method,
			version,
			dictionary_size,
			solid: flags & 0x10 != 0,
			crc32: Some(data_crc),
			directory,
			mtime,
			mode,
			link,
		}),
		data_end,
	))
}

fn decode_solid_chains(
	source: &mut (impl Read + Seek),
	records: &[Record],
	limits: Limits,
	decoded: &mut Vec<Vec<u8>>,
	buffers: &mut [Option<u32>],
) -> Result<()> {
	let mut index = 0usize;
	let mut retained = decoded.iter().try_fold(0u64, |sum, bytes| {
		sum.checked_add(bytes.len() as u64)
			.ok_or(Error::InvalidArchive("decoded buffer size overflows"))
	})?;
	while index < records.len() {
		let starts_chain = records[index].solid
			|| records
				.get(index + 1)
				.is_some_and(|next| next.solid && next.format == records[index].format);
		if !starts_chain {
			index += 1;
			continue;
		}
		let mut end = index;
		while end + 1 < records.len()
			&& records[end + 1].solid
			&& records[end + 1].format == records[index].format
		{
			end += 1;
		}
		let mut rar4 = Rar4Decoder::default();
		let mut rar5 = Rar5Decoder::default();
		for current in index..=end {
			let record = &records[current];
			if record.directory {
				continue;
			}
			let packed = read_at(
				source,
				record.data_start,
				usize_from_u64(record.packed_size, "RAR packed member")?,
			)?;
			let bytes = decode_member(
				&packed,
				usize_from_u64(record.unpacked_size, "RAR unpacked member")?,
				usize_from_u64(record.dictionary_size, "RAR dictionary")?,
				record.format.wire(),
				record.method,
				record.version,
				record.solid,
				&mut rar4,
				&mut rar5,
			)?;
			verify_member(record.path.as_str(), record.unpacked_size, record.crc32, &bytes)?;
			if record.link.is_some() {
				continue;
			}
			retained = retained
				.checked_add(bytes.len() as u64)
				.ok_or(Error::InvalidArchive("decoded buffer size overflows"))?;
			if retained > limits.in_memory_size {
				return Err(Error::ArchiveTooLargeInMemory {
					actual: retained,
					limit:  limits.in_memory_size,
				});
			}
			let buffer = u32::try_from(decoded.len())
				.map_err(|_| Error::InvalidArchive("too many decoded RAR buffers"))?;
			decoded.push(bytes);
			buffers[current] = Some(buffer);
		}
		index = end + 1;
	}
	Ok(())
}

#[allow(clippy::too_many_arguments, reason = "wire-format decode state is explicit")]
fn decode_member(
	packed: &[u8],
	unpacked_size: usize,
	dictionary_size: usize,
	format: u8,
	method: u8,
	version: u8,
	solid: bool,
	rar4: &mut Rar4Decoder,
	rar5: &mut Rar5Decoder,
) -> Result<Vec<u8>> {
	if method == 0 {
		if packed.len() != unpacked_size {
			return invalid("stored RAR member size mismatch");
		}
		if solid {
			if format == 4 {
				rar4.reset();
			} else {
				rar5.reset();
			}
		}
		return Ok(packed.to_vec());
	}
	if method > 5 {
		return Err(Error::UnsupportedFeature(if format == 4 {
			"RAR4 compression method"
		} else {
			"RAR5 compression method 6"
		}));
	}
	if format == 5 {
		rar5.decode(packed, unpacked_size, dictionary_size, solid, version)
	} else {
		rar4.decode(packed, unpacked_size, dictionary_size, solid, version)
	}
}

fn verify_member(path: &str, expected_size: u64, crc32: Option<u32>, bytes: &[u8]) -> Result<()> {
	if bytes.len() as u64 != expected_size {
		return Err(Error::SizeMismatch {
			path:     Str::new(path),
			expected: expected_size,
			actual:   bytes.len() as u64,
		});
	}
	if let Some(expected) = crc32 {
		let actual = crc32fast::hash(bytes);
		if actual != expected {
			return Err(Error::ChecksumMismatch { path: Str::new(path), expected, actual });
		}
	}
	Ok(())
}

fn check_decode_memory(
	packed: u64,
	unpacked: u64,
	dictionary: u64,
	method: u8,
	limits: Limits,
) -> Result<()> {
	if method == 0 {
		return Ok(());
	}
	let actual = packed
		.checked_add(unpacked.saturating_mul(2))
		.and_then(|size| size.checked_add(dictionary.saturating_mul(2)))
		.and_then(|size| size.checked_add(8192))
		.ok_or(Error::InvalidArchive("RAR decode memory size overflows"))?;
	if actual > limits.in_memory_size {
		return Err(Error::ArchiveTooLargeInMemory { actual, limit: limits.in_memory_size });
	}
	Ok(())
}

fn check_member_size(size: u64, path: &str, limits: Limits) -> Result<()> {
	if size > limits.member_size {
		return Err(Error::MemberTooLarge {
			path:   Str::new(path),
			actual: size,
			limit:  limits.member_size,
		});
	}
	Ok(())
}

fn check_entry_count(count: usize, limits: Limits) -> Result<()> {
	if count as u64 > limits.entries {
		return Err(Error::TooManyEntries { actual: count as u64, limit: limits.entries });
	}
	Ok(())
}

fn check_index_size(size: u64, limits: Limits) -> Result<()> {
	if size > limits.index_size {
		return Err(Error::IndexTooLarge { actual: size, limit: limits.index_size });
	}
	Ok(())
}

fn canonical_link_target(record_path: &str, raw_target: &str) -> (Str, bool) {
	let portable = raw_target.replace('\\', "/");
	if portable.starts_with('/') {
		return (Str::new(&portable), false);
	}
	let mut parts: Vec<&str> = record_path.split('/').collect();
	parts.pop();
	for part in portable.split('/') {
		if part.is_empty() || part == "." {
			continue;
		}
		if part == ".." {
			if parts.pop().is_none() {
				return (Str::new(&portable), false);
			}
		} else {
			parts.push(part);
		}
	}
	(Str::new(&parts.join("/")), true)
}

fn decode_rar4_unicode_name(bytes: &[u8]) -> Result<String> {
	let Some(mut encoded) = bytes.iter().position(|&byte| byte == 0) else {
		return Ok(decode_latin1(bytes));
	};
	let legacy_end = encoded;
	encoded += 1;
	if encoded >= bytes.len() {
		return Ok(decode_latin1(&bytes[..legacy_end]));
	}
	let high = u16::from(bytes[encoded]) << 8;
	encoded += 1;
	let mut decoded_position = 0usize;
	let mut flags = 0u8;
	let mut flag_bits = 0u8;
	let mut output = Vec::<u16>::new();
	while encoded < bytes.len() {
		if flag_bits == 0 {
			flags = bytes[encoded];
			encoded += 1;
			flag_bits = 8;
			if encoded >= bytes.len() {
				break;
			}
		}
		match flags >> 6 {
			0 => {
				output.push(u16::from(bytes[encoded]));
				encoded += 1;
				decoded_position += 1;
			},
			1 => {
				output.push(high | u16::from(bytes[encoded]));
				encoded += 1;
				decoded_position += 1;
			},
			2 => {
				need(encoded, 2, bytes.len(), "RAR4 Unicode name")?;
				output.push(u16::from_le_bytes([bytes[encoded], bytes[encoded + 1]]));
				encoded += 2;
				decoded_position += 1;
			},
			_ => {
				let mut count = usize::from(bytes[encoded]);
				encoded += 1;
				if count & 0x80 != 0 {
					need(encoded, 1, bytes.len(), "RAR4 Unicode name correction")?;
					let correction = bytes[encoded];
					encoded += 1;
					count = (count & 0x7f) + 2;
					while count > 0 && decoded_position < bytes.len() {
						output.push(high | u16::from(bytes[decoded_position].wrapping_add(correction)));
						count -= 1;
						decoded_position += 1;
					}
				} else {
					count += 2;
					while count > 0 && decoded_position < bytes.len() {
						output.push(u16::from(bytes[decoded_position]));
						count -= 1;
						decoded_position += 1;
					}
				}
			},
		}
		flags <<= 2;
		flag_bits -= 2;
	}
	xutf::to_string::<Utf16Le>(&output)
		.map_err(|_| Error::InvalidArchive("invalid RAR4 Unicode member name"))
}

fn decode_latin1(bytes: &[u8]) -> String {
	let mut output = String::with_capacity(bytes.len());
	for &byte in bytes {
		output.push(char::from(byte));
	}
	output
}

fn rar4_dictionary(flags: u16) -> u64 {
	let index = u32::from((flags >> 5) & 7);
	if index == 7 {
		4 * 1024 * 1024
	} else {
		(64 * 1024u64) << index
	}
}

fn dos_time_seconds(value: u32) -> u64 {
	let second = i64::from(value & 0x1f) * 2;
	let minute = i64::from((value >> 5) & 0x3f);
	let hour = i64::from((value >> 11) & 0x1f);
	let day = i64::from((value >> 16) & 0x1f);
	let month = i64::from((value >> 21) & 0xf);
	let year = i64::from((value >> 25) & 0x7f) + 1980;
	let days = days_from_civil(year, month, day);
	(days * 86_400 + hour * 3600 + minute * 60 + second).max(0) as u64
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
	let year = year - i64::from(month <= 2);
	let era = year.div_euclid(400);
	let year_of_era = year - era * 400;
	let shifted_month = month + if month > 2 { -3 } else { 9 };
	let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
	let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
	era * 146_097 + day_of_era - 719_468
}

fn read_vint(bytes: &[u8], cursor: &mut usize, end: usize, _what: &'static str) -> Result<u64> {
	let mut value = 0u64;
	let mut shift = 0u32;
	for _ in 0..10 {
		if *cursor >= end {
			return invalid("truncated RAR vint");
		}
		let byte = bytes[*cursor];
		*cursor += 1;
		let part = u64::from(byte & 0x7f)
			.checked_shl(shift)
			.ok_or(Error::InvalidArchive("RAR vint is too large"))?;
		value = value
			.checked_add(part)
			.ok_or(Error::InvalidArchive("RAR vint is too large"))?;
		if byte & 0x80 == 0 {
			return Ok(value);
		}
		shift += 7;
	}
	invalid("RAR vint is too long")
}

fn read_at(source: &mut (impl Read + Seek), offset: u64, size: usize) -> Result<Vec<u8>> {
	source.seek(SeekFrom::Start(offset))?;
	let mut bytes = vec![0; size];
	source.read_exact(&mut bytes)?;
	Ok(bytes)
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
	let end = offset
		.checked_add(2)
		.ok_or(Error::InvalidArchive("RAR integer offset overflows"))?;
	let raw = bytes
		.get(offset..end)
		.ok_or(Error::InvalidArchive("truncated RAR integer"))?;
	Ok(u16::from_le_bytes(raw.try_into().expect("two bytes")))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
	let end = offset
		.checked_add(4)
		.ok_or(Error::InvalidArchive("RAR integer offset overflows"))?;
	let raw = bytes
		.get(offset..end)
		.ok_or(Error::InvalidArchive("truncated RAR integer"))?;
	Ok(u32::from_le_bytes(raw.try_into().expect("four bytes")))
}

fn read_u16_at(bytes: &[u8], cursor: &mut usize, end: usize, what: &'static str) -> Result<u16> {
	need(*cursor, 2, end, what)?;
	let value = read_u16(bytes, *cursor)?;
	*cursor += 2;
	Ok(value)
}

fn read_u32_at(bytes: &[u8], cursor: &mut usize, end: usize, what: &'static str) -> Result<u32> {
	need(*cursor, 4, end, what)?;
	let value = read_u32(bytes, *cursor)?;
	*cursor += 4;
	Ok(value)
}

fn checked_end(start: u64, size: u64, limit: u64, _what: &'static str) -> Result<u64> {
	let end = start
		.checked_add(size)
		.ok_or(Error::InvalidArchive("RAR range overflows"))?;
	if end > limit {
		return invalid("truncated RAR data range");
	}
	Ok(end)
}

fn checked_slice_end(start: usize, size: u64, limit: usize, what: &'static str) -> Result<usize> {
	let size = usize_from_u64(size, what)?;
	let end = start
		.checked_add(size)
		.ok_or(Error::InvalidArchive("RAR slice range overflows"))?;
	if end > limit {
		return invalid("truncated RAR header field");
	}
	Ok(end)
}

fn need(start: usize, size: usize, end: usize, _what: &'static str) -> Result<()> {
	if start
		.checked_add(size)
		.is_none_or(|field_end| field_end > end)
	{
		return invalid("truncated RAR header field");
	}
	Ok(())
}

fn usize_from_u64(value: u64, _what: &'static str) -> Result<usize> {
	usize::try_from(value).map_err(|_| Error::InvalidArchive("RAR size overflows platform limits"))
}

fn invalid<T>(reason: &'static str) -> Result<T> {
	Err(Error::InvalidArchive(reason))
}
