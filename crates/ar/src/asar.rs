//! Bounded read-only Electron ASAR indexing and member access.

use std::{
	io::{self, Read, Seek, SeekFrom, Write},
	path::Path,
};

use cap_std::{ambient_authority, fs::Dir};
use omp_core::{Str, StrMut};
use serde_json::{Map, Value};

use crate::{
	Entry, Error, Limits, Result,
	entry::Storage,
	path::{normalize, validate},
};

const PICKLE_PREFIX_SIZE: u64 = 8;
const INNER_PICKLE_PREFIX_SIZE: u64 = 8;
const JSON_OFFSET: u64 = PICKLE_PREFIX_SIZE + INNER_PICKLE_PREFIX_SIZE;

/// Returns whether bytes begin with a structurally plausible ASAR Pickle
/// header.
pub fn is_header(bytes: &[u8]) -> bool {
	let Some(prefix) = bytes.get(..16) else {
		return false;
	};
	let outer_payload = u32::from_le_bytes(prefix[0..4].try_into().expect("four bytes"));
	let inner_size = u32::from_le_bytes(prefix[4..8].try_into().expect("four bytes"));
	let inner_payload = u32::from_le_bytes(prefix[8..12].try_into().expect("four bytes"));
	let json_size = u32::from_le_bytes(prefix[12..16].try_into().expect("four bytes"));
	outer_payload == 4
		&& inner_size >= 12
		&& inner_size == inner_payload.saturating_add(4)
		&& json_size > 0
		&& u64::from(json_size).saturating_add(5) <= u64::from(inner_payload)
		&& bytes.get(16) == Some(&b'{')
}

pub(crate) fn read_entries(
	source: &mut (impl Read + Seek),
	file_size: u64,
	limits: Limits,
) -> Result<Vec<Entry>> {
	if file_size <= JSON_OFFSET {
		return Err(Error::InvalidArchive("truncated ASAR Pickle header"));
	}
	source.seek(SeekFrom::Start(0))?;
	let mut prefix = [0_u8; 17];
	source.read_exact(&mut prefix)?;
	if !is_header(&prefix) {
		return Err(Error::InvalidArchive("invalid ASAR Pickle header"));
	}

	let inner_size = u64::from(u32::from_le_bytes(prefix[4..8].try_into().expect("four bytes")));
	let inner_payload = u64::from(u32::from_le_bytes(prefix[8..12].try_into().expect("four bytes")));
	let json_size = u64::from(u32::from_le_bytes(prefix[12..16].try_into().expect("four bytes")));
	if inner_size
		!= inner_payload
			.checked_add(4)
			.ok_or(Error::InvalidArchive("ASAR Pickle header size overflow"))?
	{
		return Err(Error::InvalidArchive("inconsistent ASAR Pickle header sizes"));
	}
	if json_size > limits.index_size {
		return Err(Error::IndexTooLarge { actual: json_size, limit: limits.index_size });
	}
	let data_offset = PICKLE_PREFIX_SIZE
		.checked_add(inner_size)
		.ok_or(Error::InvalidArchive("ASAR data offset overflow"))?;
	if data_offset > file_size {
		return Err(Error::InvalidArchive("ASAR Pickle header extends past the archive"));
	}
	let json_end = JSON_OFFSET
		.checked_add(json_size)
		.ok_or(Error::InvalidArchive("ASAR JSON range overflow"))?;
	if json_end >= data_offset {
		return Err(Error::InvalidArchive("ASAR JSON extends past its Pickle payload"));
	}
	let terminator = read_byte_at(source, json_end)?;
	if terminator != 0 {
		return Err(Error::InvalidArchive("ASAR Pickle string is not NUL-terminated"));
	}
	validate_padding(source, json_end + 1, data_offset)?;

	let json_len = usize::try_from(json_size)
		.map_err(|_| Error::InvalidArchive("ASAR JSON size does not fit this platform"))?;
	let mut json = vec![0_u8; json_len];
	source.seek(SeekFrom::Start(JSON_OFFSET))?;
	source.read_exact(&mut json)?;
	let root: Value = serde_json::from_slice(&json)
		.map_err(|_| Error::InvalidArchive("ASAR header contains invalid JSON"))?;
	let files = root
		.as_object()
		.and_then(|root| root.get("files"))
		.and_then(Value::as_object)
		.ok_or(Error::InvalidArchive("ASAR JSON root has no files object"))?;

	let mut entries = Vec::new();
	walk_files(files, "", data_offset, file_size, limits, &mut entries)?;
	Ok(entries)
}

fn walk_files(
	files: &Map<String, Value>,
	parent: &str,
	data_offset: u64,
	file_size: u64,
	limits: Limits,
	entries: &mut Vec<Entry>,
) -> Result<()> {
	for (name, value) in files {
		if !is_safe_name(name) {
			return Err(Error::UnsafePath(Str::new(name)));
		}
		let path = join_path(parent, name)?;
		if normalize(path.as_str(), false).as_deref() != Some(path.as_str()) {
			return Err(Error::UnsafePath(path));
		}
		validate(&path, limits)?;
		let object = value
			.as_object()
			.ok_or(Error::InvalidArchive("ASAR file-tree node is not an object"))?;
		if let Some(children) = object.get("files") {
			if object.contains_key("link") || object.contains_key("size") {
				return Err(Error::InvalidArchive("ambiguous ASAR file-tree node"));
			}
			let children = children
				.as_object()
				.ok_or(Error::InvalidArchive("ASAR directory files value is not an object"))?;
			entries.push(Entry {
				path:                  path.clone(),
				directory:             true,
				size:                  0,
				modified_unix_seconds: None,
				storage:               Storage::Synthetic,
			});
			walk_files(children, path.as_str(), data_offset, file_size, limits, entries)?;
			continue;
		}
		if let Some(target) = object.get("link") {
			if object.contains_key("size") {
				return Err(Error::InvalidArchive("ambiguous ASAR link node"));
			}
			let target = target
				.as_str()
				.and_then(|target| normalize(target, false))
				.ok_or_else(|| Error::UnsafePath(Str::new(target.as_str().unwrap_or(""))))?;
			validate(&target, limits)?;
			entries.push(Entry {
				path,
				directory: false,
				size: 0,
				modified_unix_seconds: None,
				storage: Storage::AsarLink { target_path: target },
			});
			continue;
		}

		let size = object
			.get("size")
			.and_then(Value::as_u64)
			.ok_or(Error::InvalidArchive("ASAR file has no valid size"))?;
		let unpacked = match object.get("unpacked") {
			None | Some(Value::Bool(false)) => false,
			Some(Value::Bool(true)) => true,
			Some(_) => return Err(Error::InvalidArchive("ASAR unpacked flag is not boolean")),
		};
		let member_offset = if unpacked {
			0
		} else {
			let relative = object
				.get("offset")
				.and_then(Value::as_str)
				.ok_or(Error::InvalidArchive("packed ASAR file has no offset"))?
				.parse::<u64>()
				.map_err(|_| Error::InvalidArchive("packed ASAR file has an invalid offset"))?;
			let absolute = data_offset
				.checked_add(relative)
				.ok_or(Error::InvalidArchive("ASAR member offset overflow"))?;
			let end = absolute
				.checked_add(size)
				.ok_or(Error::InvalidArchive("ASAR member range overflow"))?;
			if end > file_size {
				return Err(Error::InvalidArchive("ASAR member extends past the archive"));
			}
			absolute
		};
		entries.push(Entry {
			path,
			directory: false,
			size,
			modified_unix_seconds: None,
			storage: Storage::Asar { data_offset: member_offset, unpacked },
		});
	}
	Ok(())
}

pub(crate) fn resolve_alias_path(entries: &[Entry], path: Str, limits: Limits) -> Result<Str> {
	let original = path.clone();
	let mut resolved = path;
	let mut rewrites = 0_u64;
	loop {
		let Some((end, target)) = find_alias(entries, resolved.as_str()) else {
			return Ok(resolved);
		};
		if rewrites == limits.link_depth {
			return Err(Error::LinkResolutionDepth { path: original, limit: limits.link_depth });
		}
		rewrites += 1;
		let suffix = resolved.get(end..).unwrap_or("").trim_start_matches('/');
		resolved = join_target(target, suffix, limits)?;
	}
}

fn find_alias<'a>(entries: &'a [Entry], path: &str) -> Option<(usize, &'a str)> {
	let mut end = path.len();
	while end > 0 {
		if let Ok(index) = entries.binary_search_by(|entry| entry.path().cmp(&path[..end]))
			&& let Storage::AsarLink { target_path } = &entries[index].storage
		{
			return Some((end, target_path.as_str()));
		}
		end = path[..end].rfind('/').unwrap_or(0);
	}
	None
}

fn join_target(target: &str, suffix: &str, limits: Limits) -> Result<Str> {
	let separator = usize::from(!target.is_empty() && !suffix.is_empty());
	let length = target
		.len()
		.checked_add(separator)
		.and_then(|length| length.checked_add(suffix.len()))
		.ok_or(Error::InvalidArchive("ASAR link path length overflow"))?;
	if length as u64 > limits.path_size {
		return Err(Error::PathTooLong { actual: length as u64, limit: limits.path_size });
	}
	let mut joined = StrMut::with_capacity(length);
	joined.push_str(target);
	if separator != 0 {
		joined.push('/');
	}
	joined.push_str(suffix);
	let joined = joined.freeze();
	validate(&joined, limits)?;
	Ok(joined)
}

pub(crate) fn read_entry_to<W: Write>(
	source: &mut (impl Read + Seek),
	unpacked_root: Option<&Path>,
	entry: &Entry,
	output: &mut W,
) -> Result<u64> {
	match &entry.storage {
		Storage::Asar { data_offset, unpacked: false } => {
			source.seek(SeekFrom::Start(*data_offset))?;
			copy_member(source, entry, output, false)
		},
		Storage::Asar { unpacked: true, .. } => {
			let root = unpacked_root.ok_or(Error::InvalidArchive(
				"unpacked ASAR member requires a filesystem archive source",
			))?;
			let directory = Dir::open_ambient_dir(root, ambient_authority())?;
			let mut file = directory.open(Path::new(entry.path()))?;
			copy_member(&mut file, entry, output, true)
		},
		Storage::AsarLink { target_path } => {
			Err(Error::UnreadableLink { path: entry.path.clone(), target: target_path.clone() })
		},
		_ => Err(Error::InvalidArchive("non-ASAR storage in ASAR reader")),
	}
}

fn copy_member(
	source: &mut impl Read,
	entry: &Entry,
	output: &mut impl Write,
	reject_trailing_bytes: bool,
) -> Result<u64> {
	let limit = entry
		.size()
		.saturating_add(u64::from(reject_trailing_bytes));
	let mut limited = source.take(limit);
	let actual = io::copy(&mut limited, output)?;
	if actual != entry.size() {
		return Err(Error::SizeMismatch { path: entry.path.clone(), expected: entry.size(), actual });
	}
	Ok(actual)
}

fn read_byte_at(source: &mut (impl Read + Seek), offset: u64) -> Result<u8> {
	source.seek(SeekFrom::Start(offset))?;
	let mut byte = [0_u8; 1];
	source.read_exact(&mut byte)?;
	Ok(byte[0])
}

fn validate_padding(source: &mut (impl Read + Seek), start: u64, end: u64) -> Result<()> {
	if start > end || end - start > 3 {
		return Err(Error::InvalidArchive("invalid ASAR Pickle string padding"));
	}
	source.seek(SeekFrom::Start(start))?;
	for _ in start..end {
		let mut byte = [0_u8; 1];
		source.read_exact(&mut byte)?;
		if byte[0] != 0 {
			return Err(Error::InvalidArchive("non-zero ASAR Pickle string padding"));
		}
	}
	Ok(())
}

fn is_safe_name(name: &str) -> bool {
	!name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
}

fn join_path(parent: &str, name: &str) -> Result<Str> {
	let length = parent
		.len()
		.checked_add(usize::from(!parent.is_empty()))
		.and_then(|length| length.checked_add(name.len()))
		.ok_or(Error::InvalidArchive("ASAR member path length overflow"))?;
	let mut path = StrMut::with_capacity(length);
	path.push_str(parent);
	if !parent.is_empty() {
		path.push('/');
	}
	path.push_str(name);
	Ok(path.freeze())
}
