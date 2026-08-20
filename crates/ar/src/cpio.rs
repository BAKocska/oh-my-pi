//! cpio container indexing for newc, CRC, odc, and old-binary streams.

use std::{
	collections::HashMap,
	io::{Read, Seek, SeekFrom, Write},
	str,
};

use omp_core::Str;

use crate::{
	Entry, Error, Limits, Result,
	entry::Storage,
	path::{normalize, parent, validate},
};

const NEWC_HEADER_SIZE: usize = 110;
const ODC_HEADER_SIZE: usize = 76;
const BINARY_HEADER_SIZE: usize = 26;
const TRAILER_NAME: &str = "TRAILER!!!";
const FILE_TYPE_MASK: u32 = 0o170000;
const FILE_TYPE_REGULAR: u32 = 0o100000;
const FILE_TYPE_DIRECTORY: u32 = 0o040000;
const FILE_TYPE_SYMLINK: u32 = 0o120000;
const FILE_TYPE_FIFO: u32 = 0o010000;

#[derive(Clone, Copy)]
struct Header {
	header_size: usize,
	alignment:   u64,
	inode:       u32,
	mode:        u32,
	nlink:       u32,
	mtime:       u32,
	file_size:   u32,
	dev_major:   u32,
	dev_minor:   u32,
	name_size:   u32,
	checksum:    Option<u32>,
}

struct Record {
	path:        Option<Str>,
	mode:        u32,
	mtime:       u64,
	nlink:       u32,
	inode:       u32,
	dev_major:   u32,
	dev_minor:   u32,
	file_size:   u64,
	data_offset: u64,
}

/// Returns whether bytes begin with a structurally plausible cpio header.
pub fn is_header(bytes: &[u8]) -> bool {
	let Ok(header) = parse_header(bytes) else {
		return false;
	};
	if header.mode > u16::MAX as u32 || header.name_size == 0 {
		return false;
	}
	let Some(name_end) = header.header_size.checked_add(header.name_size as usize) else {
		return false;
	};
	name_end <= bytes.len() && bytes[name_end - 1] == 0
}

/// Indexes a cpio stream without retaining ordinary member payloads.
pub(crate) fn read_entries(
	source: &mut (impl Read + Seek),
	file_size: u64,
	limits: Limits,
	_decoded: &mut Vec<Vec<u8>>,
) -> Result<Vec<Entry>> {
	read_entries_impl(source, file_size, limits, None)
}

/// Indexes an already-decoded cpio stream into a retained-buffer namespace.
pub(crate) fn read_entries_from_buffer(
	bytes: &[u8],
	limits: Limits,
	buffer: u32,
) -> Result<Vec<Entry>> {
	let mut cursor = std::io::Cursor::new(bytes);
	read_entries_impl(&mut cursor, bytes.len() as u64, limits, Some(buffer))
}

fn read_entries_impl(
	source: &mut (impl Read + Seek),
	file_size: u64,
	limits: Limits,
	buffer: Option<u32>,
) -> Result<Vec<Entry>> {
	if file_size > limits.archive_size {
		return Err(Error::ArchiveTooLarge { actual: file_size, limit: limits.archive_size });
	}

	let mut records = Vec::new();
	let mut offset = 0_u64;
	let mut metadata_size = 0_u64;
	let mut found_trailer = false;
	while offset < file_size {
		let mut prefix = [0_u8; 6];
		read_exact_at(source, offset, &mut prefix, file_size, "truncated CPIO header")?;
		let header_size = if matches!(&prefix[..2], [0xc7, 0x71] | [0x71, 0xc7]) {
			BINARY_HEADER_SIZE
		} else if &prefix == b"070701" || &prefix == b"070702" {
			NEWC_HEADER_SIZE
		} else if &prefix == b"070707" {
			ODC_HEADER_SIZE
		} else {
			return Err(Error::InvalidArchive("unsupported or corrupt CPIO magic"));
		};
		let mut header_bytes = [0_u8; NEWC_HEADER_SIZE];
		read_exact_at(
			source,
			offset,
			&mut header_bytes[..header_size],
			file_size,
			"truncated CPIO header",
		)?;
		let header = parse_header(&header_bytes[..header_size])?;
		if header.mode > u16::MAX as u32 {
			return Err(Error::InvalidArchive("CPIO mode exceeds 16 bits"));
		}
		if header.name_size == 0 {
			return Err(Error::InvalidArchive("CPIO name does not include a NUL terminator"));
		}
		let path_bytes = u64::from(header.name_size - 1);
		if path_bytes > limits.path_size {
			return Err(Error::PathTooLong { actual: path_bytes, limit: limits.path_size });
		}
		let member_size = u64::from(header.file_size);
		if member_size > limits.member_size {
			return Err(Error::MemberTooLarge {
				path:   Str::new("(CPIO entry)"),
				actual: member_size,
				limit:  limits.member_size,
			});
		}

		let name_start = offset
			.checked_add(header.header_size as u64)
			.ok_or(Error::InvalidArchive("CPIO name offset overflows"))?;
		let name_end = name_start
			.checked_add(u64::from(header.name_size))
			.ok_or(Error::InvalidArchive("CPIO name range overflows"))?;
		let data_offset = align(name_end, header.alignment)?;
		let data_end = data_offset
			.checked_add(member_size)
			.ok_or(Error::InvalidArchive("CPIO member range overflows"))?;
		let next_offset = align(data_end, header.alignment)?;
		if next_offset > file_size {
			return Err(Error::InvalidArchive("truncated CPIO member data"));
		}

		let name_len = usize::try_from(header.name_size)
			.map_err(|_| Error::InvalidArchive("CPIO name does not fit this platform"))?;
		let mut name = vec![0_u8; name_len];
		read_exact_at(source, name_start, &mut name, file_size, "truncated CPIO member name")?;
		if name.last() != Some(&0) || name[..name.len() - 1].contains(&0) {
			return Err(Error::InvalidArchive("invalid NUL termination in CPIO member name"));
		}
		validate_zero_range(source, name_end, data_offset, file_size, "non-zero CPIO name padding")?;
		validate_zero_range(source, data_end, next_offset, file_size, "non-zero CPIO data padding")?;
		metadata_size = metadata_size
			.checked_add(data_offset - offset)
			.ok_or(Error::InvalidArchive("CPIO index size overflows"))?;
		if metadata_size > limits.index_size {
			return Err(Error::IndexTooLarge { actual: metadata_size, limit: limits.index_size });
		}

		let raw_name = str::from_utf8(&name[..name.len() - 1]).ok();
		if raw_name == Some(TRAILER_NAME) {
			if member_size != 0 {
				return Err(Error::InvalidArchive("CPIO TRAILER!!! has non-empty data"));
			}
			found_trailer = true;
			offset = next_offset;
			break;
		}
		if let Some(expected) = header.checksum {
			let actual = checksum_range(source, data_offset, member_size)?;
			if actual != expected {
				return Err(Error::InvalidArchive("CPIO member has an invalid CRC checksum"));
			}
		}

		let file_type = header.mode & FILE_TYPE_MASK;
		if matches!(file_type, FILE_TYPE_DIRECTORY | FILE_TYPE_FIFO) && member_size != 0 {
			return Err(Error::InvalidArchive("CPIO directory or FIFO has non-empty data"));
		}
		if records.len() as u64 >= limits.entries {
			return Err(Error::TooManyEntries {
				actual: records.len() as u64 + 1,
				limit:  limits.entries,
			});
		}
		let normalized = raw_name.and_then(|raw| normalize(raw, false));
		if let Some(path) = normalized.as_deref() {
			validate(path, limits)?;
		}
		records.push(Record {
			path: normalized,
			mode: header.mode,
			mtime: u64::from(header.mtime),
			nlink: header.nlink,
			inode: header.inode,
			dev_major: header.dev_major,
			dev_minor: header.dev_minor,
			file_size: member_size,
			data_offset,
		});
		offset = next_offset;
	}
	if !found_trailer {
		return Err(Error::InvalidArchive("missing CPIO TRAILER!!! terminator"));
	}
	validate_zero_range(source, offset, file_size, file_size, "non-zero CPIO trailing padding")?;
	materialize_records(source, records, limits, buffer)
}

fn materialize_records(
	source: &mut (impl Read + Seek),
	records: Vec<Record>,
	limits: Limits,
	buffer: Option<u32>,
) -> Result<Vec<Entry>> {
	let mut groups = HashMap::<(u32, u32, u32), Vec<usize>>::new();
	for (index, record) in records.iter().enumerate() {
		if record.mode & FILE_TYPE_MASK == FILE_TYPE_REGULAR && record.nlink > 1 {
			groups
				.entry((record.dev_major, record.dev_minor, record.inode))
				.or_default()
				.push(index);
		}
	}
	let mut handled = vec![false; records.len()];
	let mut entries = Vec::with_capacity(records.len());
	for group in groups.values().filter(|group| group.len() >= 2) {
		for &index in group {
			handled[index] = true;
		}
		let payload_index = group
			.iter()
			.copied()
			.find(|&index| records[index].file_size != 0)
			.unwrap_or(group[0]);
		let Some(canonical_index) = group
			.iter()
			.copied()
			.find(|&index| {
				records[index].path.is_some()
					&& (index == payload_index || records[payload_index].path.is_none())
			})
			.or_else(|| {
				group
					.iter()
					.copied()
					.find(|&index| records[index].path.is_some())
			})
		else {
			continue;
		};
		let canonical_path = records[canonical_index]
			.path
			.clone()
			.expect("selected retained CPIO path");
		let payload = &records[payload_index];
		for &index in group {
			let record = &records[index];
			let Some(path) = record.path.clone() else {
				continue;
			};
			let storage = if index == canonical_index {
				member_storage(buffer, payload.data_offset, payload.file_size)
			} else {
				Storage::Link { target_path: canonical_path.clone(), resolve_target: false }
			};
			entries.push(Entry {
				path,
				directory: false,
				size: payload.file_size,
				modified_unix_seconds: Some(record.mtime),
				mode: Some(record.mode),
				storage,
			});
		}
	}

	for (index, record) in records.iter().enumerate() {
		if handled[index] {
			continue;
		}
		let Some(path) = record.path.clone() else {
			continue;
		};
		match record.mode & FILE_TYPE_MASK {
			FILE_TYPE_DIRECTORY => entries.push(Entry {
				path,
				directory: true,
				size: 0,
				modified_unix_seconds: Some(record.mtime),
				mode: Some(record.mode),
				storage: Storage::Synthetic,
			}),
			FILE_TYPE_SYMLINK => {
				let target = read_link_target(source, record, limits)?;
				entries.push(Entry {
					path,
					directory: false,
					size: 0,
					modified_unix_seconds: Some(record.mtime),
					mode: Some(record.mode),
					storage: target,
				});
			},
			FILE_TYPE_REGULAR => entries.push(Entry {
				path,
				directory: false,
				size: record.file_size,
				modified_unix_seconds: Some(record.mtime),
				mode: Some(record.mode),
				storage: member_storage(buffer, record.data_offset, record.file_size),
			}),
			_ => {},
		}
	}
	Ok(entries)
}

fn member_storage(buffer: Option<u32>, data_offset: u64, stored_size: u64) -> Storage {
	match buffer {
		Some(buffer) => Storage::Buffered { buffer, data_offset, stored_size },
		None => Storage::Raw { data_offset, stored_size },
	}
}

fn read_link_target(
	source: &mut (impl Read + Seek),
	record: &Record,
	limits: Limits,
) -> Result<Storage> {
	if record.file_size > limits.path_size {
		return Err(Error::PathTooLong { actual: record.file_size, limit: limits.path_size });
	}
	let size = usize::try_from(record.file_size)
		.map_err(|_| Error::InvalidArchive("CPIO symlink target does not fit this platform"))?;
	let mut bytes = vec![0_u8; size];
	read_exact_at(
		source,
		record.data_offset,
		&mut bytes,
		u64::MAX,
		"truncated CPIO symlink target",
	)?;
	let raw = str::from_utf8(&bytes)
		.map_err(|_| Error::InvalidArchive("CPIO symlink target is not valid UTF-8"))?;
	if raw.contains('\0') {
		return Err(Error::InvalidArchive("CPIO symlink target contains NUL"));
	}
	let portable = raw.replace('\\', "/");
	if portable.starts_with('/') {
		return Ok(Storage::Link { target_path: Str::new(&portable), resolve_target: false });
	}
	let joined = if parent(record.path.as_deref().unwrap_or("")).is_empty() {
		portable.clone()
	} else {
		format!("{}/{}", parent(record.path.as_deref().unwrap_or("")), portable)
	};
	match normalize_link_path(&joined) {
		Some(target) => {
			validate(&target, limits)?;
			Ok(Storage::Link { target_path: target, resolve_target: true })
		},
		None => Ok(Storage::Link { target_path: Str::new(&portable), resolve_target: false }),
	}
}

fn normalize_link_path(path: &str) -> Option<Str> {
	let mut components = Vec::new();
	for component in path.split('/') {
		match component {
			"" | "." => {},
			".." => {
				components.pop()?;
			},
			_ => components.push(component),
		}
	}
	normalize(&components.join("/"), false)
}

fn parse_header(bytes: &[u8]) -> Result<Header> {
	if bytes.len() >= 2 && matches!(&bytes[..2], [0xc7, 0x71] | [0x71, 0xc7]) {
		if bytes.len() < BINARY_HEADER_SIZE {
			return Err(Error::InvalidArchive("truncated old-binary CPIO header"));
		}
		let little = bytes[0] == 0xc7;
		let word = |offset: usize| -> u16 {
			let pair = [bytes[offset], bytes[offset + 1]];
			if little {
				u16::from_le_bytes(pair)
			} else {
				u16::from_be_bytes(pair)
			}
		};
		let words32 = |offset: usize| u32::from(word(offset)) << 16 | u32::from(word(offset + 2));
		return Ok(Header {
			header_size: BINARY_HEADER_SIZE,
			alignment:   2,
			dev_major:   0,
			dev_minor:   u32::from(word(2)),
			inode:       u32::from(word(4)),
			mode:        u32::from(word(6)),
			nlink:       u32::from(word(12)),
			mtime:       words32(16),
			name_size:   u32::from(word(20)),
			file_size:   words32(22),
			checksum:    None,
		});
	}
	if bytes.len() < 6 {
		return Err(Error::InvalidArchive("truncated CPIO magic"));
	}
	if &bytes[..6] == b"070701" || &bytes[..6] == b"070702" {
		if bytes.len() < NEWC_HEADER_SIZE {
			return Err(Error::InvalidArchive("truncated new-ASCII CPIO header"));
		}
		let field = |index: usize| parse_digits(&bytes[6 + index * 8..14 + index * 8], 16);
		let checksum = field(12)?;
		if &bytes[..6] == b"070701" && checksum != 0 {
			return Err(Error::InvalidArchive("newc CPIO checksum field is non-zero"));
		}
		return Ok(Header {
			header_size: NEWC_HEADER_SIZE,
			alignment:   4,
			inode:       field(0)?,
			mode:        field(1)?,
			nlink:       field(4)?,
			mtime:       field(5)?,
			file_size:   field(6)?,
			dev_major:   field(7)?,
			dev_minor:   field(8)?,
			name_size:   field(11)?,
			checksum:    (&bytes[..6] == b"070702").then_some(checksum),
		});
	}
	if &bytes[..6] == b"070707" {
		if bytes.len() < ODC_HEADER_SIZE {
			return Err(Error::InvalidArchive("truncated portable-ASCII CPIO header"));
		}
		return Ok(Header {
			header_size: ODC_HEADER_SIZE,
			alignment:   1,
			dev_major:   0,
			dev_minor:   parse_digits(&bytes[6..12], 8)?,
			inode:       parse_digits(&bytes[12..18], 8)?,
			mode:        parse_digits(&bytes[18..24], 8)?,
			nlink:       parse_digits(&bytes[36..42], 8)?,
			mtime:       parse_digits(&bytes[48..59], 8)?,
			name_size:   parse_digits(&bytes[59..65], 8)?,
			file_size:   parse_digits(&bytes[65..76], 8)?,
			checksum:    None,
		});
	}
	Err(Error::InvalidArchive("unsupported or corrupt CPIO magic"))
}

fn parse_digits(bytes: &[u8], radix: u32) -> Result<u32> {
	let mut value = 0_u32;
	for &byte in bytes {
		let digit = match byte {
			b'0'..=b'9' => u32::from(byte - b'0'),
			b'a'..=b'f' if radix == 16 => u32::from(byte - b'a') + 10,
			b'A'..=b'F' if radix == 16 => u32::from(byte - b'A') + 10,
			_ => return Err(Error::InvalidArchive("invalid numeric field in CPIO header")),
		};
		if digit >= radix {
			return Err(Error::InvalidArchive("invalid numeric field in CPIO header"));
		}
		value = value
			.checked_mul(radix)
			.and_then(|value| value.checked_add(digit))
			.ok_or(Error::InvalidArchive("numeric CPIO header field overflows"))?;
	}
	Ok(value)
}

fn align(value: u64, alignment: u64) -> Result<u64> {
	let remainder = value % alignment;
	if remainder == 0 {
		Ok(value)
	} else {
		value
			.checked_add(alignment - remainder)
			.ok_or(Error::InvalidArchive("CPIO alignment overflows"))
	}
}

fn read_exact_at(
	source: &mut (impl Read + Seek),
	offset: u64,
	bytes: &mut [u8],
	file_size: u64,
	message: &'static str,
) -> Result<()> {
	let end = offset
		.checked_add(bytes.len() as u64)
		.ok_or(Error::InvalidArchive(message))?;
	if end > file_size {
		return Err(Error::InvalidArchive(message));
	}
	source.seek(SeekFrom::Start(offset))?;
	source.read_exact(bytes).map_err(|error| {
		if error.kind() == std::io::ErrorKind::UnexpectedEof {
			Error::InvalidArchive(message)
		} else {
			error.into()
		}
	})
}

fn validate_zero_range(
	source: &mut (impl Read + Seek),
	start: u64,
	end: u64,
	file_size: u64,
	message: &'static str,
) -> Result<()> {
	if end < start || end > file_size {
		return Err(Error::InvalidArchive(message));
	}
	let mut offset = start;
	let mut buffer = [0_u8; 8192];
	while offset < end {
		let count = usize::try_from((end - offset).min(buffer.len() as u64))
			.expect("zero-padding chunk is bounded by its fixed buffer");
		read_exact_at(source, offset, &mut buffer[..count], file_size, message)?;
		if buffer[..count].iter().any(|&byte| byte != 0) {
			return Err(Error::InvalidArchive(message));
		}
		offset += count as u64;
	}
	Ok(())
}

fn checksum_range(source: &mut (impl Read + Seek), offset: u64, size: u64) -> Result<u32> {
	source.seek(SeekFrom::Start(offset))?;
	let mut remaining = size;
	let mut checksum = 0_u32;
	let mut buffer = [0_u8; 8192];
	while remaining != 0 {
		let count = usize::try_from(remaining.min(buffer.len() as u64))
			.expect("checksum chunk is bounded by its fixed buffer");
		source.read_exact(&mut buffer[..count])?;
		for &byte in &buffer[..count] {
			checksum = checksum.wrapping_add(u32::from(byte));
		}
		remaining -= count as u64;
	}
	Ok(checksum)
}

/// `Raw` and `Buffered` storage are served by the archive core.
pub(crate) fn read_entry_to<W: Write>(
	_source: &mut (impl Read + Seek),
	_entry: &Entry,
	_output: &mut W,
) -> Result<u64> {
	Err(Error::InvalidArchive("entry is not a CPIO member"))
}
