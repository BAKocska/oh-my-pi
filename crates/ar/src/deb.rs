//! Debian binary package composition over Unix ar and inner tar streams.

use std::{
	io,
	io::{Cursor, Read, Seek, SeekFrom, Write},
	str,
};

use flate2::read::MultiGzDecoder;
use omp_core::sf;

use crate::{
	Entry, Error, Limits, Result, codec,
	entry::{Storage, TarSparse},
	path::{normalize, validate},
	tar, unix_ar,
};

const AR_SIGNATURE: &[u8; 8] = b"!<arch>\n";
const AR_HEADER_SIZE: usize = 60;
const DEBIAN_BINARY: &str = "debian-binary";

#[derive(Clone, Copy, PartialEq, Eq)]
enum TarKind {
	Control,
	Data,
}

#[derive(Clone, Copy)]
enum Compression {
	None,
	Gzip,
	Xz,
	Zstd,
	Bzip2,
	Lzma,
}

/// Returns whether the first ar member is `debian-binary`.
pub fn is_header(bytes: &[u8]) -> bool {
	first_member_name(bytes).as_deref() == Some(DEBIAN_BINARY)
}

/// Indexes the outer ar and eagerly retains decompressed control/data tar
/// streams.
pub(crate) fn read_entries(
	source: &mut (impl Read + Seek),
	file_size: u64,
	limits: Limits,
	decoded: &mut Vec<Vec<u8>>,
) -> Result<Vec<Entry>> {
	let probe_size = usize::try_from(
		file_size.min((AR_SIGNATURE.len() + AR_HEADER_SIZE) as u64 + limits.path_size),
	)
	.map_err(|_| Error::InvalidArchive("deb probe does not fit this platform"))?;
	let mut probe = vec![0_u8; probe_size];
	read_exact_at(source, 0, &mut probe, file_size, "truncated deb archive header")?;
	if !is_header(&probe) {
		return Err(Error::InvalidArchive("deb first member is not debian-binary"));
	}

	let outer = unix_ar::read_entries(source, file_size, limits, &mut Vec::new())?;
	if outer.first().map(Entry::path) != Some(DEBIAN_BINARY) {
		return Err(Error::InvalidArchive("deb first member is not debian-binary"));
	}
	let mut result = Vec::new();
	for entry in outer {
		let Some((kind, compression)) = classify_tar(entry.path()) else {
			if entry.path().starts_with("control.tar.") || entry.path().starts_with("data.tar.") {
				return Err(Error::UnsupportedFeature("deb tar compression"));
			}
			result.push(entry);
			continue;
		};
		let compressed = read_outer_member(source, &entry, file_size)?;
		let tar_bytes = decompress_tar(compressed, compression, limits)?;
		check_retained_size(decoded, tar_bytes.len() as u64, limits)?;
		let buffer = u32::try_from(decoded.len())
			.map_err(|_| Error::InvalidArchive("too many retained archive buffers"))?;
		let mut inner =
			tar::read_entries(&mut Cursor::new(tar_bytes.as_slice()), tar_bytes.len() as u64, limits)?;
		for inner_entry in &mut inner {
			map_inner_storage(inner_entry, buffer)?;
			if kind == TarKind::Control {
				prefix_control(inner_entry, limits)?;
			}
		}
		result.extend(inner);
		decoded.push(tar_bytes);
	}
	Ok(result)
}

fn classify_tar(path: &str) -> Option<(TarKind, Compression)> {
	let (kind, suffix) = if let Some(suffix) = path.strip_prefix("control.tar") {
		(TarKind::Control, suffix)
	} else {
		let suffix = path.strip_prefix("data.tar")?;
		(TarKind::Data, suffix)
	};
	let compression = match suffix {
		"" => Compression::None,
		".gz" => Compression::Gzip,
		".xz" => Compression::Xz,
		".zst" => Compression::Zstd,
		".bz2" => Compression::Bzip2,
		".lzma" => Compression::Lzma,
		_ => return None,
	};
	Some((kind, compression))
}

fn read_outer_member(
	source: &mut (impl Read + Seek),
	entry: &Entry,
	file_size: u64,
) -> Result<Vec<u8>> {
	let Storage::Raw { data_offset, stored_size } = entry.storage else {
		return Err(Error::InvalidArchive("invalid deb outer member storage"));
	};
	if entry.directory || stored_size != entry.size {
		return Err(Error::InvalidArchive("invalid deb outer member"));
	}
	let length = usize::try_from(stored_size)
		.map_err(|_| Error::InvalidArchive("deb outer member does not fit this platform"))?;
	let mut bytes = vec![0_u8; length];
	read_exact_at(source, data_offset, &mut bytes, file_size, "truncated deb outer member")?;
	Ok(bytes)
}

fn decompress_tar(bytes: Vec<u8>, compression: Compression, limits: Limits) -> Result<Vec<u8>> {
	let codec_limits = limits.with_max_archive_size(limits.in_memory_size);
	let output = match compression {
		Compression::None => bytes,
		Compression::Gzip => gzip_decompress(&bytes, limits.in_memory_size)?,
		Compression::Xz => codec::xz_decompress(&bytes, codec_limits)?,
		Compression::Zstd => codec::zstd_decompress(&bytes, codec_limits)?,
		Compression::Bzip2 => codec::bzip2_decompress(&bytes, codec_limits)?,
		Compression::Lzma => codec::lzma_alone_decompress(&bytes, codec_limits)?,
	};
	if output.len() as u64 > limits.in_memory_size {
		return Err(Error::ArchiveTooLargeInMemory {
			actual: output.len() as u64,
			limit:  limits.in_memory_size,
		});
	}
	Ok(output)
}

fn gzip_decompress(bytes: &[u8], max_output: u64) -> Result<Vec<u8>> {
	let mut output = Vec::new();
	MultiGzDecoder::new(bytes)
		.take(max_output.saturating_add(1))
		.read_to_end(&mut output)?;
	if output.len() as u64 > max_output {
		return Err(Error::ArchiveTooLargeInMemory {
			actual: output.len() as u64,
			limit:  max_output,
		});
	}
	Ok(output)
}

fn check_retained_size(decoded: &[Vec<u8>], additional: u64, limits: Limits) -> Result<()> {
	let retained = decoded.iter().try_fold(0_u64, |total, bytes| {
		total
			.checked_add(bytes.len() as u64)
			.ok_or(Error::InvalidArchive("decoded buffer size overflows"))
	})?;
	let actual = retained
		.checked_add(additional)
		.ok_or(Error::InvalidArchive("decoded buffer size overflows"))?;
	if actual > limits.in_memory_size {
		return Err(Error::ArchiveTooLargeInMemory { actual, limit: limits.in_memory_size });
	}
	Ok(())
}

fn map_inner_storage(entry: &mut Entry, buffer: u32) -> Result<()> {
	let replacement = match &entry.storage {
		Storage::Tar { data_offset, stored_size, sparse: TarSparse::None } => {
			if *stored_size != entry.size {
				return Err(Error::InvalidArchive("deb tar member has inconsistent stored size"));
			}
			Some(Storage::Buffered { buffer, data_offset: *data_offset, stored_size: *stored_size })
		},
		Storage::Tar { .. } => return Err(Error::UnsupportedFeature("sparse deb tar member")),
		Storage::Synthetic | Storage::Link { .. } => None,
		_ => return Err(Error::InvalidArchive("unexpected deb inner tar storage")),
	};
	if let Some(storage) = replacement {
		entry.storage = storage;
	}
	Ok(())
}

fn prefix_control(entry: &mut Entry, limits: Limits) -> Result<()> {
	entry.path = sf!("control/{}", entry.path);
	validate(&entry.path, limits)?;
	if let Storage::Link { target_path, .. } = &mut entry.storage {
		let target = target_path.as_str();
		let prefix = if target.is_empty() {
			Some(sf!("control"))
		} else {
			normalize(target, false)
				.filter(|normalized| normalized.as_str() == target)
				.map(|normalized| sf!("control/{normalized}"))
		};
		if let Some(prefixed) = prefix {
			validate(&prefixed, limits)?;
			*target_path = prefixed;
		}
	}
	Ok(())
}

fn first_member_name(bytes: &[u8]) -> Option<String> {
	if !bytes.starts_with(AR_SIGNATURE) || bytes.len() < AR_SIGNATURE.len() + AR_HEADER_SIZE {
		return None;
	}
	let header = &bytes[AR_SIGNATURE.len()..AR_SIGNATURE.len() + AR_HEADER_SIZE];
	if &header[58..60] != b"`\n" {
		return None;
	}
	let end = header[..16]
		.iter()
		.rposition(|&byte| byte != b' ')
		.map_or(0, |index| index + 1);
	if header[..end]
		.iter()
		.any(|&byte| !(0x20..=0x7e).contains(&byte))
	{
		return None;
	}
	let raw = str::from_utf8(&header[..end]).ok()?;
	if let Some(length) = raw.strip_prefix("#1/") {
		let length: usize = length.parse().ok()?;
		if length == 0 || AR_SIGNATURE.len() + AR_HEADER_SIZE + length > bytes.len() {
			return None;
		}
		let encoded = &bytes[AR_SIGNATURE.len() + AR_HEADER_SIZE..][..length];
		let name_end = encoded
			.iter()
			.position(|&byte| byte == 0)
			.unwrap_or(encoded.len());
		return Some(String::from_utf8_lossy(&encoded[..name_end]).into_owned());
	}
	Some(raw.strip_suffix('/').unwrap_or(raw).to_owned())
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
		if error.kind() == io::ErrorKind::UnexpectedEof {
			Error::InvalidArchive(message)
		} else {
			error.into()
		}
	})
}

/// `Raw` and `Buffered` storage are served by the archive core.
pub(crate) const fn read_entry_to<W: Write>(
	_source: &mut (impl Read + Seek),
	_entry: &Entry,
	_output: &mut W,
) -> Result<u64> {
	Err(Error::InvalidArchive("entry is not a deb member"))
}
