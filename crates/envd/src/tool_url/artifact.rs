//! Session artifact resolver backed by the authoritative catalog and blob
//! store.

use std::{
	fmt::{self, Display},
	fs, io,
	ops::Range,
	sync::Arc,
};

use omp_core::{CowBytes, Str};
use omp_storage::{
	blob::{BlobRef, BlobStore},
	gc,
	gc::{ArtifactCatalog as StorageArtifactCatalog, ArtifactRecord as StorageArtifactRecord},
	transcript::SessionId,
};
use omp_tools::read::{
	Fault,
	resolver::{
		ArtifactCatalog, ArtifactRecord, ArtifactResolver, BlobAuthority, BlobStat, Resolve,
		ResourceCompletion, ResourceEntry, ResourceList, fuzzy_score,
	},
	selector::ParsedSelector,
};
use parking_lot::Mutex;
use url::Url;

const MAX_INLINE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone)]
struct CatalogAuthority {
	catalog: Arc<Mutex<StorageArtifactCatalog>>,
	session: SessionId,
}

impl CatalogAuthority {
	fn storage_record(&self, resource: &str) -> Result<StorageArtifactRecord, Fault> {
		if resource.len() == 64 && resource.bytes().all(|byte| byte.is_ascii_hexdigit()) {
			let reference = BlobRef::parse_hex(resource, 0).map_err(storage_fault)?;
			self
				.catalog
				.lock()
				.stat_digest(reference.hash.into_bytes())
				.map_err(storage_fault)
		} else {
			let ordinal = resource.parse::<u64>().map_err(|_| Fault::Invalid {
				message: Str::new(format!(
					"Invalid artifact address '{resource}'; use a session ordinal or 64-hex durable \
					 digest"
				)),
			})?;
			self
				.catalog
				.lock()
				.stat_ordinal(&self.session, ordinal)
				.map_err(storage_fault)
		}
	}

	fn records(&self, limit: u32) -> Result<Vec<StorageArtifactRecord>, Fault> {
		self
			.catalog
			.lock()
			.list(Some(&self.session), None, limit)
			.map(|page| page.records)
			.map_err(storage_fault)
	}
}

impl ArtifactCatalog for CatalogAuthority {
	async fn by_ordinal(&self, ordinal: u64) -> Result<Option<ArtifactRecord>, Fault> {
		match self.catalog.lock().stat_ordinal(&self.session, ordinal) {
			Ok(record) => Ok(Some(project_record(record))),
			Err(gc::Error::ArtifactNotFound) => Ok(None),
			Err(error) => Err(storage_fault(error)),
		}
	}

	async fn by_digest<'a>(&'a self, digest: &'a str) -> Result<Option<ArtifactRecord>, Fault> {
		let reference = BlobRef::parse_hex(digest, 0).map_err(storage_fault)?;
		match self.catalog.lock().stat_digest(reference.hash.into_bytes()) {
			Ok(record) => Ok(Some(project_record(record))),
			Err(gc::Error::ArtifactNotFound) => Ok(None),
			Err(error) => Err(storage_fault(error)),
		}
	}
}

#[derive(Clone, Debug)]
struct BlobStoreAuthority {
	store: BlobStore,
}

impl BlobStoreAuthority {
	fn reference(&self, digest: &str) -> Result<BlobRef, Fault> {
		let probe = BlobRef::parse_hex(digest, 0).map_err(storage_fault)?;
		let path = self.store.path(&probe);
		let size = fs::metadata(path).map_err(io_fault)?.len();
		BlobRef::parse_hex(digest, size).map_err(storage_fault)
	}
}

impl BlobAuthority for BlobStoreAuthority {
	async fn stat<'a>(&'a self, digest: &'a str) -> Result<BlobStat, Fault> {
		Ok(BlobStat { byte_len: self.reference(digest)?.size })
	}

	async fn read_range<'a>(
		&'a self,
		digest: &'a str,
		range: Range<u64>,
	) -> Result<CowBytes<'static>, Fault> {
		let reference = self.reference(digest)?;
		let bytes = self.store.get(&reference).map_err(storage_fault)?;
		let start = usize::try_from(range.start).map_err(|_| Fault::Invalid {
			message: Str::new_static("Artifact range exceeds host address limits."),
		})?;
		let end = usize::try_from(range.end).map_err(|_| Fault::Invalid {
			message: Str::new_static("Artifact range exceeds host address limits."),
		})?;
		if start > end || end > bytes.len() {
			return Err(Fault::Invalid {
				message: Str::new_static("Artifact range exceeds stored content."),
			});
		}
		Ok(CowBytes::from(bytes.slice(start..end)))
	}
}

/// Production artifact resolver with cap guidance, path mode and completion.
pub(crate) struct ArtifactUrlResolver {
	inner:   ArtifactResolver<CatalogAuthority, BlobStoreAuthority>,
	catalog: CatalogAuthority,
	blobs:   BlobStoreAuthority,
}

impl ArtifactUrlResolver {
	pub(super) fn open(store: BlobStore, session: &str) -> Result<Self, gc::Error> {
		let catalog = CatalogAuthority {
			catalog: Arc::new(Mutex::new(StorageArtifactCatalog::open(&store)?)),
			session: SessionId(Str::new(session)),
		};
		let blobs = BlobStoreAuthority { store };
		Ok(Self { inner: ArtifactResolver::new(catalog.clone(), blobs.clone()), catalog, blobs })
	}
}

impl Resolve for ArtifactUrlResolver {
	async fn read<'a>(
		&'a self,
		resource: &'a str,
		selector: &'a ParsedSelector,
	) -> Result<CowBytes<'static>, Fault> {
		let record = self.catalog.storage_record(resource)?;
		if record.reference.size > MAX_INLINE_BYTES
			&& matches!(selector, ParsedSelector::None | ParsedSelector::Raw)
		{
			return Err(Fault::Invalid {
				message: Str::new(format!(
					"Artifact {resource} is {} bytes; full internal resolution is blocked. Use line \
					 selectors such as artifact://{resource}:1-3000 or path-only mode.",
					record.reference.size
				)),
			});
		}
		self.inner.read(resource, selector).await
	}

	async fn list(
		&self,
		resource: &str,
		max_entries: usize,
		_max_bytes: usize,
	) -> Result<ResourceList, Fault> {
		if !resource.trim_matches('/').is_empty() {
			return Err(Fault::Invalid {
				message: Str::new_static("Artifact listing is supported only at artifact:// root."),
			});
		}
		let fetch = u32::try_from(max_entries.saturating_add(1)).unwrap_or(u32::MAX);
		let records = self.catalog.records(fetch)?;
		let truncated = records.len() > max_entries;
		let entries = records
			.into_iter()
			.take(max_entries)
			.map(|record| ResourceEntry {
				uri:       Str::new(format!("artifact://{}", record.ordinal)),
				name:      Str::new(record.ordinal.to_string()),
				directory: false,
				size:      record.reference.size,
			})
			.collect();
		Ok(ResourceList { entries, truncated })
	}

	async fn path(&self, resource: &str) -> Result<Option<Str>, Fault> {
		let record = self.catalog.storage_record(resource)?;
		let path = self.blobs.store.path(&record.reference);
		let url = Url::from_file_path(path).map_err(|()| Fault::Invalid {
			message: Str::new_static("Artifact path cannot be represented as a file URI."),
		})?;
		Ok(Some(Str::new(url.as_str())))
	}

	async fn complete(
		&self,
		query: &str,
		max_results: usize,
	) -> Result<Vec<ResourceCompletion>, Fault> {
		let fetch = u32::try_from(max_results.saturating_add(1)).unwrap_or(u32::MAX);
		let mut matches = self
			.catalog
			.records(fetch)?
			.into_iter()
			.filter_map(|record| {
				let ordinal = record.ordinal.to_string();
				let score = fuzzy_score(query, &ordinal)?;
				Some(ResourceCompletion {
					value: Str::new(format!("artifact://{ordinal}")),
					description: Str::new(format!("{} bytes", record.reference.size)),
					score,
				})
			})
			.collect::<Vec<_>>();
		matches.truncate(max_results);
		Ok(matches)
	}
}

impl fmt::Debug for ArtifactUrlResolver {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str("ArtifactUrlResolver(..)")
	}
}

fn project_record(record: StorageArtifactRecord) -> ArtifactRecord {
	ArtifactRecord {
		digest:   Str::new(record.reference.hash.to_string()),
		lifetime: record.lifetime,
	}
}

fn storage_fault(error: impl Display) -> Fault {
	Fault::Source { message: Str::new(format!("Artifact storage failed: {error}")) }
}

fn io_fault(source: io::Error) -> Fault {
	Fault::Source { message: Str::new(format!("Artifact storage I/O failed: {source}")) }
}
