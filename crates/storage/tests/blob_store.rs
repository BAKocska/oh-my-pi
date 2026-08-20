//! Integration coverage for durable blob storage.

use std::{
	fs,
	io::{self, Cursor, Write},
	path::Path,
};

use omp_storage::blob::{BlobRef, BlobStore};
use tempfile::tempdir;

fn assert_tmp_empty(root: &Path) {
	assert_eq!(fs::read_dir(root.join("tmp")).unwrap().count(), 0);
}

#[test]
fn staged_write_preserves_multi_chunk_content_exactly() {
	let directory = tempdir().unwrap();
	let store = BlobStore::open(directory.path()).unwrap();
	let chunks: [&[u8]; 5] = [b"first", b"", b"-second-", &[0, 1, 2, 3], b"last"];
	let mut expected = Vec::new();
	let mut stage = store.begin_put().unwrap();

	for chunk in chunks {
		stage.write_all(chunk).unwrap();
		expected.extend_from_slice(chunk);
	}
	let reference = stage.finish().unwrap();

	assert_eq!(reference.hash, *blake3::hash(&expected).as_bytes());
	assert_eq!(reference.size, u64::try_from(expected.len()).unwrap());
	assert_eq!(store.get(&reference).unwrap(), expected.as_slice());
	assert_tmp_empty(directory.path());
}

#[test]
fn reader_and_stage_deduplicate_through_one_authority() {
	let directory = tempdir().unwrap();
	let store = BlobStore::open(directory.path()).unwrap();
	let content = b"one content-addressed payload";
	let from_reader = store.put_reader(Cursor::new(content)).unwrap();
	let mut stage = store.begin_put().unwrap();
	stage.write_all(content).unwrap();
	let from_stage = stage.finish().unwrap();

	assert_eq!(from_stage, from_reader);
	assert_eq!(store.get(&from_stage).unwrap(), &content[..]);
	assert_tmp_empty(directory.path());
}

#[test]
fn dropping_stage_removes_temporary_content() {
	let directory = tempdir().unwrap();
	let store = BlobStore::open(directory.path()).unwrap();
	let mut stage = store.begin_put().unwrap();
	stage.write_all(b"not finalized").unwrap();
	assert_eq!(fs::read_dir(directory.path().join("tmp")).unwrap().count(), 1);

	drop(stage);

	assert_tmp_empty(directory.path());
	let abandoned = BlobRef { hash: *blake3::hash(b"not finalized").as_bytes(), size: 13 };
	assert!(!store.has(&abandoned));
}

#[test]
fn producer_failure_drops_unfinished_stage() {
	fn write_then_fail(mut writer: impl Write) -> io::Result<()> {
		writer.write_all(b"partial serialized value")?;
		Err(io::Error::other("serializer failed"))
	}

	let directory = tempdir().unwrap();
	let store = BlobStore::open(directory.path()).unwrap();
	let mut stage = store.begin_put().unwrap();
	let error = write_then_fail(&mut stage).unwrap_err();
	assert_eq!(error.kind(), io::ErrorKind::Other);
	drop(stage);

	assert_tmp_empty(directory.path());
	let partial = BlobRef { hash: *blake3::hash(b"partial serialized value").as_bytes(), size: 24 };
	assert!(!store.has(&partial));
}

#[test]
fn reader_failure_leaves_no_blob_or_temporary_content() {
	struct FailingReader {
		delivered_prefix: bool,
	}

	impl io::Read for FailingReader {
		fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
			if self.delivered_prefix {
				return Err(io::Error::other("reader failed"));
			}
			self.delivered_prefix = true;
			let prefix = b"partial reader";
			buffer[..prefix.len()].copy_from_slice(prefix);
			Ok(prefix.len())
		}
	}

	let directory = tempdir().unwrap();
	let store = BlobStore::open(directory.path()).unwrap();
	let result = store.put_reader(FailingReader { delivered_prefix: false });

	assert!(
		matches!(result, Err(omp_storage::blob::Error::Io(error)) if error.kind() == io::ErrorKind::Other)
	);
	assert_tmp_empty(directory.path());
	let partial = BlobRef { hash: *blake3::hash(b"partial reader").as_bytes(), size: 14 };
	assert!(!store.has(&partial));
}

#[test]
fn finish_failure_adopts_nothing_and_removes_temporary_content() {
	let directory = tempdir().unwrap();
	let store = BlobStore::open(directory.path()).unwrap();
	let content = b"cannot be adopted";
	let expected = BlobRef {
		hash: *blake3::hash(content).as_bytes(),
		size: u64::try_from(content.len()).unwrap(),
	};
	let mut stage = store.begin_put().unwrap();
	stage.write_all(content).unwrap();
	let destination = store.path(&expected);
	let first_fanout = destination.parent().unwrap().parent().unwrap();
	fs::write(first_fanout, b"blocks destination directory").unwrap();

	assert!(stage.finish().is_err());
	assert!(!store.has(&expected));
	assert_tmp_empty(directory.path());
}

#[test]
fn zero_byte_stage_has_exact_empty_reference() {
	let directory = tempdir().unwrap();
	let store = BlobStore::open(directory.path()).unwrap();
	let reference = store.begin_put().unwrap().finish().unwrap();

	assert_eq!(reference, BlobRef { hash: *blake3::hash(&[]).as_bytes(), size: 0 });
	assert_eq!(store.get(&reference).unwrap(), &[][..]);
	assert_tmp_empty(directory.path());
}

#[test]
fn large_reader_is_streamed_and_verified() {
	const LENGTH: usize = 8 * 1024 * 1024 + 137;
	let directory = tempdir().unwrap();
	let store = BlobStore::open(directory.path()).unwrap();
	let content = (0..LENGTH)
		.map(|index| (index % 251) as u8)
		.collect::<Vec<_>>();
	let reference = store.put_reader(Cursor::new(content.as_slice())).unwrap();

	assert_eq!(reference.hash, *blake3::hash(&content).as_bytes());
	assert_eq!(reference.size, u64::try_from(LENGTH).unwrap());
	assert!(store.verify(&reference).unwrap());
	assert_tmp_empty(directory.path());
}
