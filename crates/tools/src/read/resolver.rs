//! Dense URL-scheme dispatch and constructor-owned resolver primitives.

use std::{collections::HashMap, future::Future, ops::Range, str::FromStr as _, sync::Arc};

use omp_core::{
	CowBytes, Str, sparse_index::TrySparseIndex, sparse_map::SparseMap, sparse_set::SparseSet,
};
use omp_tool::ArtifactLifetime;
use parking_lot::RwLock;
use smallvec::SmallVec;
use strum::{EnumString, FromRepr, IntoStaticStr, VariantArray};

use super::{
	Fault,
	selector::{LineRange, ParsedSelector, SelectorError},
};

/// Canonical generated-data input shared with the frozen Python URL parser.
pub const URL_VOCABULARY_JSON: &str = include_str!("../../url-vocab.json");

/// A built-in URL scheme.
///
/// The discriminants are deliberately dense because [`ResolverTable`] uses
/// them as [`SparseMap`] keys on every read.
#[derive(
	IntoStaticStr,
	VariantArray,
	Clone,
	Copy,
	Debug,
	EnumString,
	Eq,
	FromRepr,
	Hash,
	PartialEq,
	serde::Deserialize,
	serde::Serialize,
)]
#[repr(u8)]
#[serde(rename_all = "lowercase")]
#[strum(ascii_case_insensitive, serialize_all = "lowercase")]
pub enum Scheme {
	/// A workspace or environment file path.
	File,
	/// HTTP or HTTPS.
	#[strum(to_string = "http", serialize = "https")]
	Http,
	/// A session artifact or durable content digest.
	Artifact,
	/// A read-only transcript.
	History,
	/// Settled subagent output.
	Agent,
	/// Session scratch storage.
	Local,
	/// Project memory.
	Memory,
	/// An MCP-owned resource URI.
	Mcp,
	/// Installed skill content.
	Skill,
	/// Installed rule content.
	Rule,
	/// Bundled harness documentation.
	Omp,
	/// A cached GitHub issue.
	Issue,
	/// A cached GitHub pull request.
	Pr,
	/// A remote SSH resource.
	Ssh,
	/// Security scan state.
	Security,
	/// A granted vault resource.
	Vault,
	/// Detached-job output.
	Job,
	/// A session-registered merge conflict region.
	Conflict,
	/// A syntactically valid scheme outside the built-in vocabulary.
	Unknown,
}

impl Scheme {
	/// Every dense built-in variant in discriminant order.
	pub const ALL: &'static [Self] = <Self as VariantArray>::VARIANTS;

	/// Parses a caller spelling, mapping syntactically valid unrecognized names
	/// to [`Scheme::Unknown`].
	#[must_use]
	pub fn parse(value: &str) -> Self {
		Self::from_str(value).unwrap_or(Self::Unknown)
	}

	/// Whether this scheme's resource grammar permits a trailing read selector.
	#[must_use]
	pub const fn accepts_selectors(self) -> bool {
		!matches!(self, Self::Mcp | Self::Unknown)
	}
}

/// An invalid dense scheme index.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("invalid scheme index {0}")]
pub struct SchemeIndexError(usize);

impl TrySparseIndex for Scheme {
	type Error = SchemeIndexError;

	fn index(&self) -> usize {
		usize::from(*self as u8)
	}

	fn try_from_index(index: usize) -> Result<Self, Self::Error> {
		let repr = u8::try_from(index).map_err(|_| SchemeIndexError(index))?;
		Self::from_repr(repr).ok_or(SchemeIndexError(index))
	}
}

/// A compact index into constructor-owned resolver state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolverId(usize);

impl ResolverId {
	/// Returns the resolver's constructor-order index.
	#[must_use]
	pub const fn index(self) -> usize {
		self.0
	}
}

/// Resolves one URL scheme to readable bytes.
///
/// Implement this trait on a concrete resolver or on an enum containing every
/// resolver kind used by a host. That keeps the future unboxed and the state
/// constructor-owned without a per-call trait-object allocation.
pub trait Resolve: Send + Sync + 'static {
	/// Reads the addressed resource, applying `selector` when supported.
	fn read<'a>(
		&'a self,
		resource: &'a str,
		selector: &'a ParsedSelector,
	) -> impl Future<Output = Result<CowBytes<'static>, Fault>> + Send + 'a;
}

/// Canonical metadata for one resolver registration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchemeEntry {
	/// Dense scheme identity.
	pub scheme:      Scheme,
	/// Generated Python enum member spelling.
	pub member:      Str,
	/// Whether reads route to the registered resolver under current policy.
	pub readable:    bool,
	/// Whether the current policy permits minting this scheme.
	pub mintable:    bool,
	/// Whether the scheme resource grammar accepts trailing read selectors.
	pub selectors:   bool,
	/// Human-readable live capability description.
	pub description: Str,
}

impl SchemeEntry {
	/// Constructs metadata, deriving canonical member and selector vocabulary
	/// from the dense scheme.
	#[must_use]
	pub fn new(scheme: Scheme, readable: bool, mintable: bool, description: impl Into<Str>) -> Self {
		Self {
			scheme,
			member: Str::from(format!("{scheme:?}").to_ascii_uppercase()),
			readable,
			mintable,
			selectors: scheme.accepts_selectors(),
			description: description.into(),
		}
	}
}

/// Device-hash-keyed resolver metadata shared with extension hosts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchemeSnapshot {
	/// Registry device-side digest that invalidates this snapshot.
	pub device_hash: [u8; 32],
	/// Constructor-order scheme metadata.
	pub entries:     Box<[SchemeEntry]>,
}

/// Error constructing a resolver table.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ResolverTableError {
	/// Two resolver values claimed one scheme.
	#[error("duplicate resolver for {0:?}")]
	Duplicate(Scheme),
	/// Unknown schemes cannot be registered.
	#[error("the unknown scheme cannot have a resolver")]
	Unknown,
}

/// Builder for one immutable constructor-owned resolver table.
#[derive(Debug)]
pub struct ResolverTableBuilder<R> {
	claimed:   SparseSet<Scheme>,
	entries:   Vec<SchemeEntry>,
	resolvers: Vec<R>,
}

impl<R> Default for ResolverTableBuilder<R> {
	fn default() -> Self {
		Self {
			claimed:   SparseSet::with_capacity(Scheme::ALL.len()),
			entries:   Vec::new(),
			resolvers: Vec::new(),
		}
	}
}

impl<R> ResolverTableBuilder<R> {
	/// Registers one resolver and its live policy metadata.
	pub fn register(&mut self, entry: SchemeEntry, resolver: R) -> Result<(), ResolverTableError> {
		if entry.scheme == Scheme::Unknown {
			return Err(ResolverTableError::Unknown);
		}
		if !self.claimed.insert(entry.scheme) {
			return Err(ResolverTableError::Duplicate(entry.scheme));
		}
		self.entries.push(entry);
		self.resolvers.push(resolver);
		Ok(())
	}

	/// Freezes registrations into an O(1) dispatch table.
	#[must_use]
	pub fn build(self) -> ResolverTable<R> {
		let mut routes = SparseMap::with_capacity(Scheme::ALL.len());
		for (index, entry) in self.entries.iter().enumerate() {
			if entry.readable {
				routes.insert(entry.scheme, ResolverId(index));
			}
		}
		ResolverTable {
			routes,
			entries: self.entries.into_boxed_slice(),
			resolvers: self.resolvers.into_boxed_slice(),
		}
	}
}

/// O(1) scheme dispatch into concrete, constructor-owned resolver state.
#[derive(Debug)]
pub struct ResolverTable<R> {
	routes:    SparseMap<Scheme, ResolverId>,
	entries:   Box<[SchemeEntry]>,
	resolvers: Box<[R]>,
}

impl<R> Default for ResolverTable<R> {
	fn default() -> Self {
		ResolverTableBuilder::default().build()
	}
}

impl<R> ResolverTable<R> {
	/// Starts an empty metadata-bearing resolver builder.
	#[must_use]
	pub fn builder() -> ResolverTableBuilder<R> {
		ResolverTableBuilder::default()
	}

	/// Returns the dense route map used by dispatch.
	#[must_use]
	pub const fn routes(&self) -> &SparseMap<Scheme, ResolverId> {
		&self.routes
	}

	/// Returns every registered scheme's live metadata.
	#[must_use]
	pub const fn entries(&self) -> &[SchemeEntry] {
		&self.entries
	}

	/// Captures metadata under the registry device-side digest.
	#[must_use]
	pub fn snapshot(&self, device_hash: [u8; 32]) -> SchemeSnapshot {
		SchemeSnapshot { device_hash, entries: self.entries.clone() }
	}

	/// Returns the resolver selected for `scheme`.
	#[must_use]
	pub fn get(&self, scheme: Scheme) -> Option<&R> {
		let id = *self.routes.get(scheme)?;
		self.resolvers.get(id.index())
	}
}

impl<R: Resolve> ResolverTable<R> {
	/// Dispatches one read, returning `None` when this deployment has no reader
	/// for the scheme.
	pub async fn read(
		&self,
		scheme: Scheme,
		resource: &str,
		selector: &ParsedSelector,
	) -> Option<Result<CowBytes<'static>, Fault>> {
		Some(self.get(scheme)?.read(resource, selector).await)
	}
}

/// A resolver marker used when a host installs no internal URL readers.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoResolver;

impl Resolve for NoResolver {
	fn read<'a>(
		&'a self,
		_resource: &'a str,
		_selector: &'a ParsedSelector,
	) -> impl Future<Output = Result<CowBytes<'static>, Fault>> + Send + 'a {
		async { unreachable!("NoResolver is never installed in a ResolverTable") }
	}
}

/// Exact byte length reported by the authoritative blob store.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlobStat {
	/// Exact stored byte length.
	pub byte_len: u64,
}

/// Immutable artifact metadata resolved by ordinal or digest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactRecord {
	/// Content digest in the environment blob namespace.
	pub digest:   Str,
	/// Retention tier controlling whether digest-form addressing is legal.
	pub lifetime: ArtifactLifetime,
}

/// Resolves artifact names to immutable artifact records.
pub trait ArtifactCatalog: Send + Sync + 'static {
	/// Resolves a short ordinal in the current session.
	fn by_ordinal(
		&self,
		ordinal: u64,
	) -> impl Future<Output = Result<Option<ArtifactRecord>, Fault>> + Send + '_;
	/// Resolves a content digest visible across sessions.
	fn by_digest<'a>(
		&'a self,
		digest: &'a str,
	) -> impl Future<Output = Result<Option<ArtifactRecord>, Fault>> + Send + 'a;
}

/// Authoritative artifact-byte storage.
pub trait BlobAuthority: Send + Sync + 'static {
	/// Stats stored bytes. This value, never a peer's claimed size, determines
	/// the legal read range.
	fn stat<'a>(
		&'a self,
		digest: &'a str,
	) -> impl Future<Output = Result<BlobStat, Fault>> + Send + 'a;
	/// Reads one exact byte range from an immutable blob.
	fn read_range<'a>(
		&'a self,
		digest: &'a str,
		range: Range<u64>,
	) -> impl Future<Output = Result<CowBytes<'static>, Fault>> + Send + 'a;
}

#[derive(Debug)]
struct LineOffsets {
	starts: Box<[usize]>,
	len:    usize,
}

impl LineOffsets {
	fn scan(bytes: &[u8]) -> Self {
		let mut starts = Vec::with_capacity(bytecount::count(bytes, b'\n').saturating_add(1));
		starts.push(0);
		for (index, byte) in bytes.iter().copied().enumerate() {
			if byte == b'\n' {
				starts.push(index + 1);
			}
		}
		Self { starts: starts.into_boxed_slice(), len: bytes.len() }
	}

	fn byte_range(&self, range: LineRange) -> Result<Range<usize>, SelectorError> {
		let start_line = usize::try_from(range.start_line).unwrap_or(usize::MAX);
		if start_line == 0 || start_line > self.starts.len() {
			return Err(SelectorError::from_message(format!(
				"Line {} is out of bounds; resource has {} lines.",
				range.start_line,
				self.starts.len()
			)));
		}
		let end_line = range
			.end_line
			.map_or(self.starts.len(), |end| usize::try_from(end).unwrap_or(usize::MAX))
			.min(self.starts.len());
		let start = self.starts[start_line - 1];
		let end = self.starts.get(end_line).copied().unwrap_or(self.len);
		Ok(start..end)
	}
}

/// Cached line-to-byte offsets for immutable resolver resources.
///
/// Cache entries retain offsets only. Returned slices share the resolver's
/// [`CowBytes`] backing allocation.
#[derive(Debug, Default)]
pub struct LineOffsetCache(RwLock<HashMap<Str, Arc<LineOffsets>>>);

impl LineOffsetCache {
	/// Returns cached offsets for `key`, if the resource has been scanned.
	fn get(&self, key: &str) -> Option<Arc<LineOffsets>> {
		self.0.read().get(key).cloned()
	}

	/// Scans and caches an immutable resource once.
	fn index(&self, key: &str, bytes: &[u8]) -> Arc<LineOffsets> {
		if let Some(offsets) = self.get(key) {
			return offsets;
		}
		let offsets = Arc::new(LineOffsets::scan(bytes));
		self
			.0
			.write()
			.entry(Str::from(key))
			.or_insert_with(|| offsets.clone())
			.clone()
	}

	/// Applies one line range without copying its backing blob.
	pub fn slice<'a>(
		&self,
		key: &str,
		bytes: &CowBytes<'a>,
		range: LineRange,
	) -> Result<CowBytes<'a>, SelectorError> {
		let offsets = self.index(key, bytes);
		Ok(bytes.slice(offsets.byte_range(range)?))
	}
}

/// Artifact resolver backed by a catalog and authoritative blob store.
#[derive(Debug)]
pub struct ArtifactResolver<C, B> {
	catalog: C,
	blobs:   B,
	lines:   LineOffsetCache,
}

impl<C, B> ArtifactResolver<C, B> {
	/// Constructs an artifact resolver with an empty line-offset cache.
	#[must_use]
	pub fn new(catalog: C, blobs: B) -> Self {
		Self { catalog, blobs, lines: LineOffsetCache::default() }
	}
}

impl<C: ArtifactCatalog, B: BlobAuthority> ArtifactResolver<C, B> {
	async fn record(&self, resource: &str) -> Result<ArtifactRecord, Fault> {
		let record = if resource.len() == 64 && resource.bytes().all(|byte| byte.is_ascii_hexdigit())
		{
			let record = self.catalog.by_digest(resource).await?;
			record.filter(|entry| entry.lifetime == ArtifactLifetime::Durable)
		} else {
			let ordinal = resource.parse::<u64>().map_err(|_| Fault::Invalid {
				message: Str::from(format!(
					"Invalid artifact address '{resource}'; use a session ordinal or 64-hex durable \
					 digest"
				)),
			})?;
			self.catalog.by_ordinal(ordinal).await?
		};
		record.ok_or_else(|| Fault::source(format!("Artifact '{resource}' not found")))
	}

	async fn all_bytes(
		&self,
		record: &ArtifactRecord,
		size: u64,
	) -> Result<CowBytes<'static>, Fault> {
		self.blobs.read_range(&record.digest, 0..size).await
	}

	async fn selected_bytes(
		&self,
		record: &ArtifactRecord,
		size: u64,
		ranges: &[LineRange],
	) -> Result<CowBytes<'static>, Fault> {
		let Some(offsets) = self.lines.get(&record.digest) else {
			let bytes = self.all_bytes(record, size).await?;
			std::str::from_utf8(&bytes).map_err(|_| Fault::Invalid {
				message: Str::new_static("Artifact selectors require UTF-8 text"),
			})?;
			let offsets = self.lines.index(&record.digest, &bytes);
			if ranges.len() == 1 {
				let range = offsets.byte_range(ranges[0]).map_err(selector_fault)?;
				return Ok(bytes.slice(range));
			}
			let mut joined = Vec::new();
			for range in ranges {
				let range = offsets.byte_range(*range).map_err(selector_fault)?;
				joined.extend_from_slice(&bytes.slice(range));
			}
			return Ok(CowBytes::from(joined));
		};

		if ranges.len() == 1 {
			let range = offsets.byte_range(ranges[0]).map_err(selector_fault)?;
			return self
				.blobs
				.read_range(&record.digest, usize_range_to_u64(range)?)
				.await;
		}

		let mut pieces: SmallVec<CowBytes<'static>, 2> = SmallVec::new();
		let mut total = 0usize;
		for range in ranges {
			let bytes = self
				.blobs
				.read_range(
					&record.digest,
					usize_range_to_u64(offsets.byte_range(*range).map_err(selector_fault)?)?,
				)
				.await?;
			total = total.saturating_add(bytes.len());
			pieces.push(bytes);
		}
		let mut joined = Vec::with_capacity(total);
		for piece in pieces {
			joined.extend_from_slice(&piece);
		}
		Ok(CowBytes::from(joined))
	}
}

impl<C: ArtifactCatalog, B: BlobAuthority> Resolve for ArtifactResolver<C, B> {
	async fn read<'a>(
		&'a self,
		resource: &'a str,
		selector: &'a ParsedSelector,
	) -> Result<CowBytes<'static>, Fault> {
		let record = self.record(resource).await?;
		let size = self.blobs.stat(&record.digest).await?.byte_len;
		match selector {
			ParsedSelector::Lines { ranges, .. } => self.selected_bytes(&record, size, ranges).await,
			ParsedSelector::None | ParsedSelector::Raw | ParsedSelector::Conflicts => {
				self.all_bytes(&record, size).await
			},
		}
	}
}

fn selector_fault(error: SelectorError) -> Fault {
	Fault::Invalid { message: Str::from(error.to_string()) }
}

fn usize_range_to_u64(range: Range<usize>) -> Result<Range<u64>, Fault> {
	let start = u64::try_from(range.start).map_err(|_| Fault::Invalid {
		message: Str::new_static("Artifact line offset exceeds the blob protocol range"),
	})?;
	let end = u64::try_from(range.end).map_err(|_| Fault::Invalid {
		message: Str::new_static("Artifact line offset exceeds the blob protocol range"),
	})?;
	Ok(start..end)
}
