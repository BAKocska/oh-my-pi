//! Unix ar, BSD archive, GNU archive, and COFF import-library indexing.

use std::io::{Read, Seek, SeekFrom, Write};

use omp_core::Str;

use crate::{
	Entry, Error, Limits, Result,
	entry::Storage,
	path::{normalize, validate},
};

const SIGNATURE: &[u8; 8] = b"!<arch>\n";
const HEADER_SIZE: usize = 60;
const FILE_TYPE_MASK: u32 = 0o170000;
const DIRECTORY_TYPE: u32 = 0o040000;

struct Header {
	raw_name:        String,
	physical_size:   u64,
	mtime:           Option<u64>,
	mode:            Option<u32>,
	bsd_name_length: Option<u64>,
}

struct Record {
	name:        String,
	name_bytes:  u64,
	data_offset: u64,
	size:        u64,
	mtime:       Option<u64>,
	mode:        Option<u32>,
}

/// Returns whether bytes begin with the Unix ar global signature.
pub fn is_header(bytes: &[u8]) -> bool {
	bytes.starts_with(SIGNATURE)
}

/// Indexes an ar stream using ranged metadata reads.
pub(crate) fn read_entries(
	source: &mut (impl Read + Seek),
	file_size: u64,
	limits: Limits,
	_decoded: &mut Vec<Vec<u8>>,
) -> Result<Vec<Entry>> {
	if file_size < SIGNATURE.len() as u64 {
		return Err(Error::InvalidArchive("invalid ar archive signature"));
	}
	let mut signature = [0_u8; SIGNATURE.len()];
	read_exact_at(source, 0, &mut signature, file_size, "truncated ar archive signature")?;
	if &signature != SIGNATURE {
		return Err(Error::InvalidArchive("invalid ar archive signature"));
	}

	let mut records = Vec::new();
	let mut long_names = None;
	let mut metadata_size = 0_u64;
	let mut position = SIGNATURE.len() as u64;
	while position < file_size {
		let mut bytes = [0_u8; HEADER_SIZE];
		read_exact_at(source, position, &mut bytes, file_size, "truncated ar member header")?;
		let header = parse_header(&bytes)?;
		metadata_size = metadata_size
			.checked_add(HEADER_SIZE as u64)
			.ok_or(Error::InvalidArchive("ar index size overflows"))?;
		check_index_size(metadata_size, limits)?;
		let payload_offset = position
			.checked_add(HEADER_SIZE as u64)
			.ok_or(Error::InvalidArchive("ar member offset overflows"))?;
		let payload_end = payload_offset
			.checked_add(header.physical_size)
			.ok_or(Error::InvalidArchive("ar member range overflows"))?;
		if payload_end > file_size {
			return Err(Error::InvalidArchive("truncated ar member data"));
		}

		let mut name = header.raw_name;
		let mut name_bytes = name.len() as u64;
		let mut data_offset = payload_offset;
		let mut size = header.physical_size;
		if let Some(length) = header.bsd_name_length {
			metadata_size = metadata_size
				.checked_add(length)
				.ok_or(Error::InvalidArchive("ar index size overflows"))?;
			check_index_size(metadata_size, limits)?;
			let length_usize = usize::try_from(length)
				.map_err(|_| Error::InvalidArchive("BSD ar member name does not fit this platform"))?;
			let mut encoded = vec![0_u8; length_usize];
			read_exact_at(
				source,
				payload_offset,
				&mut encoded,
				file_size,
				"truncated BSD ar member name",
			)?;
			let name_end = encoded
				.iter()
				.position(|&byte| byte == 0)
				.unwrap_or(encoded.len());
			if name_end == 0 {
				return Err(Error::InvalidArchive("empty BSD ar extended member name"));
			}
			name = String::from_utf8_lossy(&encoded[..name_end]).into_owned();
			name_bytes = name_end as u64;
			data_offset += length;
			size -= length;
		} else if name == "//" {
			metadata_size = metadata_size
				.checked_add(header.physical_size)
				.ok_or(Error::InvalidArchive("ar index size overflows"))?;
			check_index_size(metadata_size, limits)?;
			let table_size = usize::try_from(header.physical_size)
				.map_err(|_| Error::InvalidArchive("ar long-name table does not fit this platform"))?;
			let mut table = vec![0_u8; table_size];
			read_exact_at(
				source,
				payload_offset,
				&mut table,
				file_size,
				"truncated ar long-name table",
			)?;
			long_names = Some(table);
		}
		records.push(Record {
			name,
			name_bytes,
			data_offset,
			size,
			mtime: header.mtime,
			mode: header.mode,
		});
		if records.len() as u64 > limits.entries {
			return Err(Error::TooManyEntries {
				actual: records.len() as u64,
				limit:  limits.entries,
			});
		}
		position = payload_end
			.checked_add(header.physical_size & 1)
			.ok_or(Error::InvalidArchive("ar alignment offset overflows"))?;
		if position > file_size {
			return Err(Error::InvalidArchive("missing ar alignment byte"));
		}
	}
	materialize(records, long_names.as_deref(), limits)
}

fn materialize(
	records: Vec<Record>,
	long_names: Option<&[u8]>,
	limits: Limits,
) -> Result<Vec<Entry>> {
	let mut entries = Vec::with_capacity(records.len());
	for mut record in records {
		if is_long_name_reference(&record.name) {
			let table = long_names
				.ok_or(Error::InvalidArchive("ar member references a missing long-name table"))?;
			let (name, length) = resolve_long_name(&record.name, table, limits)?;
			record.name = name;
			record.name_bytes = length;
		} else if !matches!(record.name.as_str(), "/" | "//" | "/SYM64/")
			&& record.name.ends_with('/')
		{
			record.name.pop();
			record.name_bytes = record.name.len() as u64;
		}
		if is_metadata_name(&record.name) {
			continue;
		}
		if record.name_bytes > limits.path_size {
			return Err(Error::PathTooLong { actual: record.name_bytes, limit: limits.path_size });
		}
		if record.size > limits.member_size {
			return Err(Error::MemberTooLarge {
				path:   Str::new(&record.name),
				actual: record.size,
				limit:  limits.member_size,
			});
		}
		let Some(path) = normalize(&record.name, false) else {
			continue;
		};
		validate(&path, limits)?;
		let directory = record
			.mode
			.is_some_and(|mode| mode & FILE_TYPE_MASK == DIRECTORY_TYPE);
		entries.push(Entry {
			path,
			directory,
			size: if directory { 0 } else { record.size },
			modified_unix_seconds: record.mtime,
			mode: record.mode,
			storage: if directory {
				Storage::Synthetic
			} else {
				Storage::Raw { data_offset: record.data_offset, stored_size: record.size }
			},
		});
	}
	Ok(entries)
}

fn parse_header(bytes: &[u8; HEADER_SIZE]) -> Result<Header> {
	if &bytes[58..60] != b"`\n" {
		return Err(Error::InvalidArchive("invalid ar archive member header"));
	}
	let raw_name = ascii_field(&bytes[..16])?.to_owned();
	let mtime = optional_number(ascii_field(&bytes[16..28])?, 10, "invalid ar modification time")?;
	optional_number(ascii_field(&bytes[28..34])?, 10, "invalid ar user id")?;
	optional_number(ascii_field(&bytes[34..40])?, 10, "invalid ar group id")?;
	let mode = optional_number(ascii_field(&bytes[40..48])?, 8, "invalid ar mode")?
		.map(|value| u32::try_from(value).map_err(|_| Error::InvalidArchive("ar mode overflows")))
		.transpose()?;
	let physical_size = optional_number(ascii_field(&bytes[48..58])?, 10, "invalid ar member size")?
		.ok_or(Error::InvalidArchive("invalid ar member size"))?;
	let bsd_name_length = if let Some(encoded) = raw_name.strip_prefix("#1/") {
		let length = required_decimal(encoded, "invalid BSD ar extended name length")?;
		if length == 0 || length > physical_size {
			return Err(Error::InvalidArchive("invalid BSD ar extended name length"));
		}
		Some(length)
	} else {
		None
	};
	Ok(Header { raw_name, physical_size, mtime, mode, bsd_name_length })
}

fn ascii_field(bytes: &[u8]) -> Result<&str> {
	let end = bytes
		.iter()
		.rposition(|&byte| byte != b' ')
		.map_or(0, |index| index + 1);
	if bytes[..end]
		.iter()
		.any(|&byte| !(0x20..=0x7e).contains(&byte))
	{
		return Err(Error::InvalidArchive("invalid ar archive header field"));
	}
	std::str::from_utf8(&bytes[..end])
		.map_err(|_| Error::InvalidArchive("invalid ar archive header field"))
}

fn optional_number(value: &str, radix: u32, message: &'static str) -> Result<Option<u64>> {
	if value.is_empty() || value == "-1" {
		return Ok(None);
	}
	if value.bytes().any(|byte| match radix {
		8 => !(b'0'..=b'7').contains(&byte),
		_ => !byte.is_ascii_digit(),
	}) {
		return Err(Error::InvalidArchive(message));
	}
	u64::from_str_radix(value, radix)
		.map(Some)
		.map_err(|_| Error::InvalidArchive(message))
}

fn required_decimal(value: &str, message: &'static str) -> Result<u64> {
	if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
		return Err(Error::InvalidArchive(message));
	}
	value.parse().map_err(|_| Error::InvalidArchive(message))
}

fn is_long_name_reference(name: &str) -> bool {
	name
		.strip_prefix('/')
		.is_some_and(|offset| !offset.is_empty() && offset.bytes().all(|byte| byte.is_ascii_digit()))
}

fn resolve_long_name(reference: &str, table: &[u8], limits: Limits) -> Result<(String, u64)> {
	let offset = required_decimal(
		reference.strip_prefix('/').unwrap_or_default(),
		"invalid ar long-name reference",
	)?;
	let offset = usize::try_from(offset)
		.map_err(|_| Error::InvalidArchive("ar long-name offset does not fit this platform"))?;
	if offset >= table.len() {
		return Err(Error::InvalidArchive("ar long-name offset is outside its table"));
	}
	let relative_end = table[offset..]
		.iter()
		.position(|&byte| byte == 0 || byte == b'\n')
		.ok_or(Error::InvalidArchive("unterminated ar long name"))?;
	let mut end = offset + relative_end;
	if table[end] == b'\n' && end > offset && table[end - 1] == b'/' {
		end -= 1;
	}
	if end == offset {
		return Err(Error::InvalidArchive("empty ar long name"));
	}
	let length = (end - offset) as u64;
	if length > limits.path_size {
		return Err(Error::PathTooLong { actual: length, limit: limits.path_size });
	}
	Ok((String::from_utf8_lossy(&table[offset..end]).into_owned(), length))
}

fn is_metadata_name(name: &str) -> bool {
	matches!(name, "/" | "//" | "/SYM64/" | "__.SYMDEF" | "__.SYMDEF SORTED")
}

fn check_index_size(actual: u64, limits: Limits) -> Result<()> {
	if actual > limits.index_size {
		return Err(Error::IndexTooLarge { actual, limit: limits.index_size });
	}
	Ok(())
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

/// `Raw` storage is served by the archive core.
pub(crate) fn read_entry_to<W: Write>(
	_source: &mut (impl Read + Seek),
	_entry: &Entry,
	_output: &mut W,
) -> Result<u64> {
	Err(Error::InvalidArchive("entry is not an ar member"))
}
