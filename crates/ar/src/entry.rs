//! Format-neutral indexed archive member metadata.

use omp_core::Str;
use smallvec::SmallVec;

/// Compression method recorded for a ZIP member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionMethod {
	/// Bytes are stored verbatim.
	Stored,
	/// Bytes use raw DEFLATE compression.
	Deflate,
	/// The method is retained for listing but cannot be decoded.
	Unsupported(u16),
}

impl CompressionMethod {
	pub(crate) const fn from_code(code: u16) -> Self {
		match code {
			0 => Self::Stored,
			8 => Self::Deflate,
			other => Self::Unsupported(other),
		}
	}

	/// Returns the ZIP wire-format method number.
	pub const fn code(self) -> u16 {
		match self {
			Self::Stored => 0,
			Self::Deflate => 8,
			Self::Unsupported(code) => code,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TarSparseExtent {
	pub(crate) offset: u64,
	pub(crate) length: u64,
}
/// One physical extent of an ISO 9660 file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsoExtent {
	pub(crate) data_offset:       u64,
	pub(crate) size:              u64,
	pub(crate) file_unit_blocks:  u8,
	pub(crate) interleave_blocks: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TarSparse {
	None,
	OldGnu(SmallVec<TarSparseExtent, 4>),
	Unsupported,
}

/// Where an indexed member's bytes live and how they are decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Storage {
	/// Directory synthesized from member paths; carries no bytes.
	Synthetic,
	/// ZIP member decoded from a local-header-relative range.
	Zip {
		compressed_size:     u64,
		crc32:               u32,
		method:              CompressionMethod,
		flags:               u16,
		local_header_offset: u64,
	},
	/// TAR member sliced from the (possibly decompressed) TAR stream.
	Tar { data_offset: u64, stored_size: u64, sparse: TarSparse },
	/// ASAR member sliced from the payload region, or an unpacked sibling.
	Asar { data_offset: u64, unpacked: bool },
	/// Symbolic link or directory alias resolved lazily by path rewriting.
	///
	/// `resolve_target` marks links that must be followed before the target
	/// kind is known (ASAR link records do not encode it); TAR keeps `false`
	/// because its reader classifies links while indexing.
	Link { target_path: Str, resolve_target: bool },
	/// ISO 9660 member assembled from multiple or interleaved physical extents.
	Iso { block_size: u64, stored_size: u64, extents: SmallVec<IsoExtent, 2> },
	/// Compressed LZH or ARJ member decoded from a source range.
	LegacyDos {
		data_offset: u64,
		stored_size: u64,
		format:      u8,
		method:      u8,
		checksum:    u32,
	},
	/// Non-solid RAR member decoded and checksum-verified on extraction.
	Rar {
		data_offset:     u64,
		packed_size:     u64,
		dictionary_size: u64,
		crc32:           Option<u32>,
		format:          u8,
		method:          u8,
		version:         u8,
	},
	/// CAB member whose folder compression is intentionally deferred because
	/// the method or parameter is unsupported.
	CabUnsupported { method: u8, parameter: u8 },
	/// Verbatim byte range in the original source (stored members of cpio,
	/// ar, ISO, and friends).
	Raw { data_offset: u64, stored_size: u64 },
	/// Range within one of the archive's retained decoded buffers
	/// (single-stream pseudo-archives, deb/rpm payloads, solid blocks).
	Buffered { buffer: u32, data_offset: u64, stored_size: u64 },
}

/// One normalized file, directory, or unresolved symbolic link in an archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
	pub(crate) path:                  Str,
	pub(crate) directory:             bool,
	pub(crate) size:                  u64,
	pub(crate) modified_unix_seconds: Option<u64>,
	pub(crate) mode:                  Option<u32>,
	pub(crate) storage:               Storage,
}

impl Entry {
	pub(crate) const fn synthetic_directory(path: Str) -> Self {
		Self {
			path,
			directory: true,
			size: 0,
			modified_unix_seconds: None,
			mode: None,
			storage: Storage::Synthetic,
		}
	}

	/// Returns the normalized archive-relative path.
	#[inline]
	pub fn path(&self) -> &str {
		self.path.as_str()
	}

	/// Returns the final component of the normalized path.
	#[inline]
	pub fn name(&self) -> &str {
		self.path.rsplit('/').next().unwrap_or(self.path.as_str())
	}

	/// Returns whether this entry represents a directory.
	#[inline]
	pub const fn is_directory(&self) -> bool {
		self.directory
	}

	/// Returns the declared logical size in bytes.
	#[inline]
	pub const fn size(&self) -> u64 {
		self.size
	}

	/// Returns the stored member size before decompression or sparse
	/// expansion.
	#[inline]
	pub const fn compressed_size(&self) -> u64 {
		match &self.storage {
			Storage::Zip { compressed_size, .. } => *compressed_size,
			Storage::Tar { stored_size, .. }
			| Storage::Iso { stored_size, .. }
			| Storage::LegacyDos { stored_size, .. }
			| Storage::Rar { packed_size: stored_size, .. }
			| Storage::Raw { stored_size, .. }
			| Storage::Buffered { stored_size, .. } => *stored_size,
			Storage::CabUnsupported { .. } => self.size,
			Storage::Asar { .. } => self.size,
			Storage::Synthetic | Storage::Link { .. } => 0,
		}
	}

	/// Returns the ZIP compression method, or `None` for other formats.
	#[inline]
	pub const fn zip_compression(&self) -> Option<CompressionMethod> {
		match &self.storage {
			Storage::Zip { method, .. } => Some(*method),
			_ => None,
		}
	}

	/// Returns the declared ZIP CRC-32, or `None` for other formats.
	#[inline]
	pub const fn crc32(&self) -> Option<u32> {
		match &self.storage {
			Storage::Zip { crc32, .. } => Some(*crc32),
			_ => None,
		}
	}

	/// Returns whether this ZIP member declares traditional encryption.
	#[inline]
	pub fn is_encrypted(&self) -> bool {
		matches!(&self.storage, Storage::Zip { flags, .. } if flags & 1 != 0)
	}

	/// Returns whether this entry is an unresolved symbolic-link node.
	#[inline]
	pub const fn is_link(&self) -> bool {
		matches!(&self.storage, Storage::Link { .. })
	}

	/// Returns an unresolved link target.
	#[inline]
	pub fn link_target(&self) -> Option<&str> {
		match &self.storage {
			Storage::Link { target_path, .. } => Some(target_path.as_str()),
			_ => None,
		}
	}

	/// Returns the member modification time as Unix seconds when recorded.
	#[inline]
	pub const fn modified_unix_seconds(&self) -> Option<u64> {
		self.modified_unix_seconds
	}

	/// Returns Unix permission/type bits when the container records them.
	#[inline]
	pub const fn mode(&self) -> Option<u32> {
		self.mode
	}
}
