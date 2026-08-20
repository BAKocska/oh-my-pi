//! Content-addressed storage for binary payloads.
//!
//! Blobs are addressed by their BLAKE3-256 digest, which deduplicates payloads
//! across sessions, makes writes idempotent, and gives references the same
//! meaning on every machine. Files live at `<root>/blobs/<hh>/<hh>/
//! <full-64-hex>`; the two fanout levels use the first two digest bytes so that
//! a single directory does not accumulate millions of entries.
//!
//! New blobs are written to `<root>/tmp`, flushed with `fsync`, and atomically
//! renamed into their final location, so readers never observe a
//! partially-written blob. [`BlobStore::get`] verifies length only by default.
//! Call [`BlobStore::verify`] when a full digest check is required.
//!
//! Blob-producing transactions intentionally finish before the journal entry
//! that makes them reachable. This put-before-journal ordering can leave an
//! unreferenced blob after a crash, but never a journal reference to a missing
//! blob.

use std::{
	fmt,
	fs::{self, File, OpenOptions},
	io::{self, Read, Write},
	path::{Path, PathBuf},
	sync::atomic::{AtomicU64, Ordering},
};

use bytes::Bytes;
use omp_core::encoding::hex::{self, ArrayStr};
use serde::{
	Deserialize, Deserializer, Serialize, Serializer,
	de::{self, Visitor},
	ser::SerializeStruct,
};
use thiserror::Error as ThisError;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const COPY_BUFFER_SIZE: usize = 64 * 1024;

/// A stable reference to a content-addressed blob.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct BlobRef {
	/// The BLAKE3-256 digest of the blob contents.
	pub hash: [u8; 32],
	/// The blob length in bytes.
	pub size: u64,
}

impl BlobRef {
	/// Returns the digest as 64 lowercase hexadecimal characters in stack
	/// storage.
	#[must_use]
	pub const fn to_hex(&self) -> ArrayStr<32> {
		hex::encode_n(&self.hash)
	}

	/// Parses a 64-character lowercase hexadecimal digest with the supplied byte
	/// length.
	///
	/// # Errors
	///
	/// Returns [`Error::BadHex`] when `hash` is not exactly 64 lowercase
	/// hexadecimal characters.
	pub fn parse_hex(hash: &str, size: u64) -> Result<Self, Error> {
		Ok(Self { hash: parse_hash(hash)?, size })
	}
}

impl fmt::Display for BlobRef {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(self.to_hex().as_str())
	}
}

impl Serialize for BlobRef {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		let hash = self.to_hex();
		let mut state = serializer.serialize_struct("BlobRef", 2)?;
		state.serialize_field("h", hash.as_str())?;
		state.serialize_field("n", &self.size)?;
		state.end()
	}
}

impl<'de> Deserialize<'de> for BlobRef {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>,
	{
		#[derive(Deserialize)]
		struct WireRef {
			#[serde(rename = "h", deserialize_with = "deserialize_hash")]
			hash: [u8; 32],
			#[serde(rename = "n")]
			size: u64,
		}

		let wire = WireRef::deserialize(deserializer)?;
		Ok(Self { hash: wire.hash, size: wire.size })
	}
}

/// Errors produced by blob reference parsing and blob-store operations.
#[derive(Debug, ThisError)]
pub enum Error {
	/// An underlying filesystem or stream operation failed.
	#[error(transparent)]
	Io(#[from] io::Error),
	/// A blob's stored length differs from the referenced length.
	#[error("corrupt blob: expected {expected} bytes, found {actual} bytes")]
	Corrupt {
		/// The byte length recorded by the reference.
		expected: u64,
		/// The byte length found on disk.
		actual:   u64,
	},
	/// A digest was not exactly 64 lowercase hexadecimal characters.
	#[error("invalid BLAKE3 hash hex")]
	BadHex,
	/// The referenced blob does not exist.
	#[error("blob not found")]
	NotFound,
}

/// A filesystem-backed, content-addressed blob store.
#[derive(Clone, Debug)]
pub struct BlobStore {
	root: PathBuf,
}

impl BlobStore {
	/// Opens a store rooted at `root`, creating its blob and temporary
	/// directories when absent.
	///
	/// # Errors
	///
	/// Returns [`Error::Io`] when the directory hierarchy cannot be created.
	pub fn open(root: impl Into<PathBuf>) -> Result<Self, Error> {
		let store = Self { root: root.into() };
		fs::create_dir_all(store.blobs_dir())?;
		fs::create_dir_all(store.tmp_dir())?;
		Ok(store)
	}

	/// Returns the filesystem root that owns this blob namespace.
	#[must_use]
	pub fn root(&self) -> &Path {
		&self.root
	}

	/// Stores an in-memory blob and returns its content-derived reference.
	///
	/// This uses the same staged, single-pass placement authority as
	/// [`Self::put_reader`] and [`Self::begin_put`]. If the digest is already
	/// present, the operation succeeds without rewriting the file.
	///
	/// # Errors
	///
	/// Returns [`Error::Io`] when the input length cannot be represented or a
	/// filesystem operation fails.
	pub fn put(&self, data: &[u8]) -> Result<BlobRef, Error> {
		self.put_reader(data)
	}

	/// Streams a blob from `reader` into the store while computing its digest.
	///
	/// The reader is consumed once using one recycled fixed-size scratch
	/// allocation. Bytes pass through [`BlobStage`], so reader-driven and
	/// serializer-driven writes share hashing, synchronization, atomic
	/// placement, deduplication, and cleanup.
	///
	/// # Errors
	///
	/// Returns [`Error::Io`] when reading, writing, synchronizing, or renaming
	/// fails.
	pub fn put_reader(&self, mut reader: impl Read) -> Result<BlobRef, Error> {
		let mut stage = self.begin_put()?;
		let mut buffer = vec![0_u8; COPY_BUFFER_SIZE].into_boxed_slice();

		loop {
			let read = match reader.read(&mut buffer) {
				Ok(0) => break,
				Ok(read) => read,
				Err(error) if error.kind() == io::ErrorKind::Interrupted => {
					continue;
				},
				Err(error) => {
					return Err(error.into());
				},
			};
			stage.write_all(&buffer[..read])?;
		}

		stage.finish()
	}

	/// Starts a store-owned streaming blob transaction.
	///
	/// Write already-encoded bytes into the returned [`BlobStage`] and call
	/// [`BlobStage::finish`] to synchronize and atomically adopt them. Dropping
	/// the stage, including while unwinding from a serializer error, removes its
	/// temporary file. Finalization deliberately precedes any journal record
	/// that makes the returned reference reachable.
	///
	/// # Errors
	///
	/// Returns [`Error::Io`] when a temporary staging file cannot be created.
	pub fn begin_put(&self) -> Result<BlobStage, Error> {
		let (file, temporary) = self.create_temp()?;
		Ok(BlobStage {
			store: self.clone(),
			file: Some(file),
			temporary,
			hasher: blake3::Hasher::new(),
			size: 0,
			failed: false,
		})
	}

	/// Reads a blob, checking that its stored byte length matches the reference.
	///
	/// This deliberately does not recompute the digest; use [`Self::verify`] for
	/// full content verification.
	///
	/// # Errors
	///
	/// Returns [`Error::NotFound`] when the blob is absent, [`Error::Corrupt`]
	/// when its length is wrong, or [`Error::Io`] for another read failure.
	pub fn get(&self, reference: &BlobRef) -> Result<Bytes, Error> {
		let data = fs::read(self.path(reference)).map_err(map_read_error)?;
		let actual = usize_to_u64(data.len())?;
		if actual != reference.size {
			return Err(Error::Corrupt { expected: reference.size, actual });
		}
		Ok(Bytes::from(data))
	}

	/// Returns whether the referenced blob path currently exists as a file.
	#[must_use]
	pub fn has(&self, reference: &BlobRef) -> bool {
		self.path(reference).is_file()
	}

	/// Returns the sharded filesystem path for a blob reference.
	///
	/// The layout is
	/// `<root>/blobs/<first-byte-hex>/<second-byte-hex>/<full-64-hex>`.
	#[must_use]
	pub fn path(&self, reference: &BlobRef) -> PathBuf {
		let hash = reference.to_hex();
		self
			.blobs_dir()
			.join(&hash[..2])
			.join(&hash[2..4])
			.join(hash.as_str())
	}

	/// Fully verifies that a blob's byte length and BLAKE3 digest match its
	/// reference.
	///
	/// # Errors
	///
	/// Returns [`Error::NotFound`] when the blob is absent or [`Error::Io`] when
	/// it cannot be read.
	pub fn verify(&self, reference: &BlobRef) -> Result<bool, Error> {
		let mut file = File::open(self.path(reference)).map_err(map_read_error)?;
		let mut hasher = blake3::Hasher::new();
		let mut size = 0_u64;
		let mut buffer = vec![0_u8; COPY_BUFFER_SIZE].into_boxed_slice();

		loop {
			let read = match file.read(&mut buffer) {
				Ok(0) => break,
				Ok(read) => read,
				Err(error) if error.kind() == io::ErrorKind::Interrupted => {
					continue;
				},
				Err(error) => {
					return Err(error.into());
				},
			};
			hasher.update(&buffer[..read]);
			size = size
				.checked_add(usize_to_u64(read)?)
				.ok_or_else(|| io::Error::other("blob length exceeds u64"))?;
		}

		Ok(size == reference.size && hasher.finalize().as_bytes() == &reference.hash)
	}

	fn blobs_dir(&self) -> PathBuf {
		self.root.join("blobs")
	}

	fn tmp_dir(&self) -> PathBuf {
		self.root.join("tmp")
	}

	fn prepare_destination(destination: &Path) -> Result<(), Error> {
		let parent = destination
			.parent()
			.ok_or_else(|| io::Error::other("blob destination has no parent"))?;
		fs::create_dir_all(parent)?;
		Ok(())
	}

	fn create_temp(&self) -> Result<(File, TemporaryPath), Error> {
		let directory = self.tmp_dir();
		fs::create_dir_all(&directory)?;
		loop {
			let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
			let name = format!("{}-{sequence:016x}.blob", std::process::id());
			let path = directory.join(name);
			match OpenOptions::new().write(true).create_new(true).open(&path) {
				Ok(file) => return Ok((file, TemporaryPath::new(path))),
				Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {},
				Err(error) => return Err(error.into()),
			}
		}
	}

	fn commit(mut temporary: TemporaryPath, destination: &Path) -> Result<(), Error> {
		match fs::rename(temporary.path(), destination) {
			Ok(()) => {
				temporary.disarm();
				Ok(())
			},
			Err(error)
				if error.kind() == io::ErrorKind::AlreadyExists && destination.try_exists()? =>
			{
				Ok(())
			},
			Err(error) => Err(error.into()),
		}
	}
}

/// A store-owned, single-pass writer for one content-addressed blob.
///
/// Each successful write is incorporated into the blob's BLAKE3 digest and
/// byte length exactly once. The temporary content is removed unless
/// [`Self::finish`] successfully adopts it.
pub struct BlobStage {
	store:     BlobStore,
	file:      Option<File>,
	temporary: TemporaryPath,
	hasher:    blake3::Hasher,
	size:      u64,
	failed:    bool,
}

impl BlobStage {
	/// Synchronizes and atomically adopts the staged bytes, returning their
	/// exact content-derived reference.
	///
	/// An existing blob with the same digest is a successful deduplication. On
	/// any error, the unadopted temporary file is removed.
	///
	/// # Errors
	///
	/// Returns [`Error::Io`] when the stage has failed, or when synchronizing,
	/// preparing the destination, or atomically placing the blob fails.
	pub fn finish(mut self) -> Result<BlobRef, Error> {
		if self.failed {
			return Err(io::Error::other("blob stage previously failed").into());
		}

		let file = self.file.take().expect("blob stage file is present");
		file.sync_all()?;
		drop(file);

		let reference = BlobRef { hash: *self.hasher.finalize().as_bytes(), size: self.size };
		let destination = self.store.path(&reference);
		if destination.try_exists()? {
			return Ok(reference);
		}

		BlobStore::prepare_destination(&destination)?;
		BlobStore::commit(self.temporary, &destination)?;
		Ok(reference)
	}

	const fn file(&mut self) -> &mut File {
		self.file.as_mut().expect("blob stage file is present")
	}
}

impl Write for BlobStage {
	fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
		let written = match self.file().write(buffer) {
			Ok(written) => written,
			Err(error) => {
				self.failed = true;
				return Err(error);
			},
		};
		self.hasher.update(&buffer[..written]);
		self.size = if let Some(size) = u64::try_from(written)
			.ok()
			.and_then(|written| self.size.checked_add(written))
		{
			size
		} else {
			self.failed = true;
			return Err(io::Error::other("blob length exceeds u64"));
		};
		Ok(written)
	}

	fn write_all(&mut self, mut buffer: &[u8]) -> io::Result<()> {
		while !buffer.is_empty() {
			match self.write(buffer) {
				Ok(0) => {
					self.failed = true;
					return Err(io::Error::new(io::ErrorKind::WriteZero, "failed to write staged blob"));
				},
				Ok(written) => buffer = &buffer[written..],
				Err(error) => return Err(error),
			}
		}
		Ok(())
	}

	fn flush(&mut self) -> io::Result<()> {
		match self.file().flush() {
			Ok(()) => Ok(()),
			Err(error) => {
				self.failed = true;
				Err(error)
			},
		}
	}
}

struct TemporaryPath {
	path: Option<PathBuf>,
}

impl TemporaryPath {
	const fn new(path: PathBuf) -> Self {
		Self { path: Some(path) }
	}

	fn path(&self) -> &Path {
		self.path.as_deref().expect("temporary path is armed")
	}

	fn disarm(&mut self) {
		self.path = None;
	}
}

impl Drop for TemporaryPath {
	fn drop(&mut self) {
		if let Some(path) = self.path.take() {
			let _ = fs::remove_file(path);
		}
	}
}

fn parse_hash(hash: &str) -> Result<[u8; 32], Error> {
	if hash.len() != 64
		|| !hash
			.bytes()
			.all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
	{
		return Err(Error::BadHex);
	}
	hex::decode(hash)
		.into_array::<32>()
		.map_err(|_| Error::BadHex)
}

fn deserialize_hash<'de, D>(deserializer: D) -> Result<[u8; 32], D::Error>
where
	D: Deserializer<'de>,
{
	struct HashVisitor;

	impl Visitor<'_> for HashVisitor {
		type Value = [u8; 32];

		fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
			formatter.write_str("64 lowercase hexadecimal characters")
		}

		fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
		where
			E: de::Error,
		{
			parse_hash(value).map_err(E::custom)
		}
	}

	deserializer.deserialize_str(HashVisitor)
}

fn usize_to_u64(value: usize) -> Result<u64, Error> {
	u64::try_from(value).map_err(|_| io::Error::other("blob length exceeds u64").into())
}

fn map_read_error(error: io::Error) -> Error {
	if error.kind() == io::ErrorKind::NotFound {
		Error::NotFound
	} else {
		Error::Io(error)
	}
}

#[cfg(test)]
mod tests {

	use tempfile::tempdir;

	use super::{BlobRef, BlobStore, Error};

	#[test]
	fn put_get_round_trip() {
		let directory = tempdir().unwrap();
		let store = BlobStore::open(directory.path()).unwrap();
		let reference = store.put(b"transcript payload").unwrap();

		assert_eq!(store.get(&reference).unwrap(), &b"transcript payload"[..]);
		assert!(store.verify(&reference).unwrap());
	}

	#[test]
	fn identical_content_is_idempotent() {
		let directory = tempdir().unwrap();
		let store = BlobStore::open(directory.path()).unwrap();

		let first = store.put(b"shared payload").unwrap();
		let second = store.put(b"shared payload").unwrap();

		assert_eq!(first, second);
	}

	#[test]
	fn has_changes_after_put() {
		let directory = tempdir().unwrap();
		let store = BlobStore::open(directory.path()).unwrap();
		let expected = BlobRef { hash: *blake3::hash(b"present later").as_bytes(), size: 13 };

		assert!(!store.has(&expected));
		assert_eq!(store.put(b"present later").unwrap(), expected);
		assert!(store.has(&expected));
	}

	#[test]
	fn get_rejects_tampered_size() {
		let directory = tempdir().unwrap();
		let store = BlobStore::open(directory.path()).unwrap();
		let mut reference = store.put(b"length checked").unwrap();
		reference.size += 1;

		assert!(matches!(store.get(&reference), Err(Error::Corrupt { expected: 15, actual: 14 })));
	}

	#[test]
	fn verify_detects_corrupted_file() {
		let directory = tempdir().unwrap();
		let store = BlobStore::open(directory.path()).unwrap();
		let reference = store.put(b"original").unwrap();
		std::fs::write(store.path(&reference), b"tampered").unwrap();

		assert!(!store.verify(&reference).unwrap());
	}

	#[test]
	fn blob_ref_json_hex_round_trip() {
		let reference = BlobRef { hash: [0; 32], size: 7 };
		let json = serde_json::to_string(&reference).unwrap();

		assert_eq!(
			json,
			"{\"h\":\"0000000000000000000000000000000000000000000000000000000000000000\",\"n\":7}"
		);
		assert_eq!(serde_json::from_str::<BlobRef>(&json).unwrap(), reference);
	}
}
