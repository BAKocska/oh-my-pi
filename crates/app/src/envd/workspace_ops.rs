//! Content-addressed workspace generations and copy-on-write worktrees.

#[cfg(target_os = "macos")]
use std::ffi::CString;
use std::{
	collections::{BTreeMap, HashMap},
	fs::{self, File},
	io::{self, Cursor, Read, Write},
	ops::ControlFlow,
	path::{Component, Path, PathBuf},
	sync::{
		Arc,
		atomic::{AtomicU64, Ordering},
	},
};
#[cfg(target_os = "linux")]
use std::{fs::OpenOptions, os::fd::AsRawFd as _};

use bytes::Bytes;
use omp_core::{Str, encoding::hex};
use omp_proto::{document::v1 as document_pb, env::v1 as pb};
use omp_walker::{FileType, WalkOrder};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use super::{
	super::{
		blobs::{BlobError, BlobHost, BlobId},
		docs::{DocumentError, DocumentHost, DocumentLease, WorkspaceLease, lease_target},
		tool_document::read_whole,
	},
	WorkspaceError, WorkspaceHost,
};

const MANIFEST_MAGIC: &[u8; 7] = b"OMPWS2\0";
const DIFF_MAGIC: &[u8; 8] = b"OMPWSD1\0";
const IO_BUFFER_BYTES: usize = 64 * 1024;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;

/// Snapshot, restore, or isolated-worktree failure.
#[derive(Debug, Error)]
pub enum WorkspaceOperationError {
	/// Workspace traversal failed.
	#[error(transparent)]
	Workspace(#[from] WorkspaceError),
	/// Blob storage failed.
	#[error(transparent)]
	Blob(#[from] BlobError),
	/// Document authority failed.
	#[error(transparent)]
	Document(#[from] DocumentError),
	/// Filesystem access failed.
	#[error("workspace filesystem operation failed: {0}")]
	Io(#[from] io::Error),
	/// A caller-supplied relative path escaped its workspace root.
	#[error("workspace path escapes its isolated root")]
	OutsideRoot,
	/// A snapshot identifier or manifest was malformed.
	#[error("invalid workspace generation: {0}")]
	InvalidGeneration(Str),
	/// The requested worktree does not exist.
	#[error("worktree {0:?} was not found")]
	WorktreeNotFound(Str),
	/// A worktree name is empty or contains path separators.
	#[error("invalid worktree name")]
	InvalidWorktreeName,
	/// A worktree registry record was malformed.
	#[error("invalid worktree registry record: {0}")]
	InvalidWorktreeRecord(Str),
}

/// Result of merging an isolated worktree without invoking a VCS subprocess.
#[derive(Clone, Debug)]
pub struct WorktreeMerge {
	/// Current worktree identity and generation.
	pub worktree:  pb::WorktreeInfo,
	/// Content-addressed manifest-diff artifact produced by `patch` strategy.
	pub artifact:  Option<BlobId>,
	/// Internal branch metadata produced by `branch` strategy.
	pub branch:    Option<Str>,
	/// Structured conflicts that prevented the requested disposition.
	pub conflicts: Vec<pb::WorkspaceConflict>,
}

/// Environment-owned content-addressed workspace and worktree service.
#[derive(Clone)]
pub struct WorkspaceOperations {
	inner: Arc<OperationsInner>,
}

struct OperationsInner {
	workspace:       WorkspaceHost,
	documents:       DocumentHost,
	blobs:           BlobHost,
	worktree_root:   PathBuf,
	cache:           Mutex<HashMap<PathBuf, CachedFile>>,
	worktrees:       Mutex<HashMap<Str, WorktreeRecord>>,
	next_generation: AtomicU64,
}

#[derive(Clone)]
struct CachedFile {
	fingerprint: FileFingerprint,
	blob:        BlobId,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct FileFingerprint {
	len:         u64,
	modified_ns: u128,
	mode:        u32,
	identity:    u64,
	change_ns:   i128,
}

#[derive(Clone)]
struct WorktreeRecord {
	id:         Str,
	root:       PathBuf,
	base:       Str,
	generation: u64,
	branch:     Option<Str>,
	owner_pid:  u32,
}

#[derive(Deserialize, Serialize)]
struct DurableWorktreeRecord {
	version:     u8,
	id:          String,
	root:        PathBuf,
	base:        String,
	generation:  u64,
	branch:      Option<String>,
	owner_pid:   u32,
	class:       String,
	source_root: PathBuf,
}

struct WorktreeBuild {
	root:  PathBuf,
	armed: bool,
}

impl Drop for WorktreeBuild {
	fn drop(&mut self) {
		if self.armed {
			let _ = fs::remove_dir_all(&self.root);
		}
	}
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ManifestEntry {
	path: Str,
	mode: u32,
	hash: [u8; 32],
}

struct Manifest {
	prefixes: Vec<Str>,
	entries:  BTreeMap<Str, ManifestEntry>,
}

impl WorkspaceOperations {
	/// Opens persistent workspace operations beneath an environment-private
	/// state root.
	pub fn open(
		workspace: WorkspaceHost,
		documents: DocumentHost,
		blobs: BlobHost,
		state_root: impl AsRef<Path>,
	) -> Result<Self, WorkspaceOperationError> {
		fs::create_dir_all(state_root.as_ref())?;
		fs::create_dir_all(state_root.as_ref().join(".branches"))?;
		fs::create_dir_all(state_root.as_ref().join(".records"))?;
		let worktree_root = fs::canonicalize(state_root)?;
		let (worktrees, next_generation) = load_worktree_records(&worktree_root)?;
		Ok(Self {
			inner: Arc::new(OperationsInner {
				workspace,
				documents,
				blobs,
				worktree_root,
				cache: Mutex::new(HashMap::new()),
				worktrees: Mutex::new(worktrees),
				next_generation: AtomicU64::new(next_generation),
			}),
		})
	}

	/// Captures a bounded, streaming manifest and returns its hash as generation
	/// identity.
	pub fn snapshot(
		&self,
		request: &pb::SnapshotWorkspace,
		cancel: &CancellationToken,
	) -> Result<pb::WorkspaceSnapshot, WorkspaceOperationError> {
		self.snapshot_at(self.inner.workspace.root(), &request.paths, cancel)
	}

	/// Restores one generation through document leases, always capturing an undo
	/// generation first.
	pub async fn restore(
		&self,
		request: &pb::RestoreWorkspace,
		cancel: &CancellationToken,
	) -> Result<pb::WorkspaceRestored, WorkspaceOperationError> {
		let undo = self.snapshot_at(self.inner.workspace.root(), &[], cancel)?;
		let current = self.load_manifest(&undo.snapshot_id)?;
		let target = self.load_manifest(&request.snapshot_id)?;
		let mut restored = pb::WorkspaceRestored {
			snapshot_id:      request.snapshot_id.clone(),
			undo_snapshot_id: undo.snapshot_id,
			conflicts:        Vec::new(),
			partial:          false,
			props:            Default::default(),
		};
		let plans = self.plan_restore(&target, &current, cancel).await?;
		let (workspace_lease, lease_conflicts) = self
			.acquire_restore_lease(&plans, request.dry_run, cancel)
			.await?;
		restored.conflicts.extend(lease_conflicts);
		if request.dry_run || !restored.conflicts.is_empty() {
			return Ok(restored);
		}
		if plans.is_empty() {
			return Ok(restored);
		}
		let Some(workspace_lease) = workspace_lease else {
			return Err(WorkspaceOperationError::InvalidGeneration(Str::new_static(
				"document authority omitted an uncontested workspace lease",
			)));
		};

		let mut committed = 0_usize;
		for plan in plans {
			if cancel.is_cancelled() {
				return Err(WorkspaceError::Cancelled.into());
			}
			let path = plan.path().clone();
			match self.apply_restore_plan(plan, cancel).await {
				Ok(()) => committed += 1,
				Err(failure) => {
					restored.partial = committed != 0 || failure.effects;
					restored
						.conflicts
						.push(workspace_conflict(path, failure.reason, None));
					break;
				},
			}
		}
		drop(workspace_lease);
		Ok(restored)
	}

	/// Creates an isolated copy-on-write root from the current workspace
	/// generation.
	pub fn create_worktree(
		&self,
		request: &pb::CreateWorktree,
		cancel: &CancellationToken,
	) -> Result<pb::WorktreeInfo, WorkspaceOperationError> {
		validate_worktree_name(&request.name)?;
		let snapshot = self.snapshot_at(self.inner.workspace.root(), &request.paths, cancel)?;
		if request
			.base
			.as_deref()
			.is_some_and(|base| base != snapshot.snapshot_id)
		{
			return Err(WorkspaceOperationError::InvalidGeneration(Str::new_static(
				"copy-on-write creation requires the live workspace generation",
			)));
		}
		let id = Str::from(format!("{}-{}", request.name, Ulid::generate()));
		let root = self.inner.worktree_root.join(id.as_str());
		if root.parent() != Some(self.inner.worktree_root.as_path()) {
			return Err(WorkspaceOperationError::OutsideRoot);
		}
		fs::create_dir(&root)?;
		let mut build = WorktreeBuild { root: root.clone(), armed: true };
		let manifest = self.load_manifest(&snapshot.snapshot_id)?;
		for entry in manifest.entries.values() {
			if cancel.is_cancelled() {
				return Err(WorkspaceError::Cancelled.into());
			}
			let source = checked_join(self.inner.workspace.root(), entry.path.as_str())?;
			let source = fs::canonicalize(source)?;
			if !source.starts_with(self.inner.workspace.root()) {
				return Err(WorkspaceOperationError::OutsideRoot);
			}
			let destination = checked_join(&root, entry.path.as_str())?;
			if let Some(parent) = destination.parent() {
				fs::create_dir_all(parent)?;
			}
			clone_file_cow(&source, &destination)?;
			set_mode(&destination, entry.mode)?;
		}
		let generation = self.inner.next_generation.fetch_add(1, Ordering::Relaxed);
		let record = WorktreeRecord {
			id: id.clone(),
			root,
			base: Str::from(snapshot.snapshot_id),
			generation,
			branch: None,
			owner_pid: if request.owner_pid == 0 {
				std::process::id()
			} else {
				request.owner_pid
			},
		};
		self.write_worktree_record(&record)?;
		build.armed = false;
		self.inner.worktrees.lock().insert(id, record.clone());
		worktree_info(&record)
	}

	/// Destroys an isolated root, refusing dirty content unless `force` is set.
	pub fn destroy_worktree(
		&self,
		request: &pb::DestroyWorktree,
		cancel: &CancellationToken,
	) -> Result<pb::WorktreeInfo, WorkspaceOperationError> {
		let key = Str::from(request.id.as_str());
		let record = self
			.inner
			.worktrees
			.lock()
			.get(&key)
			.cloned()
			.ok_or_else(|| WorkspaceOperationError::WorktreeNotFound(key.clone()))?;
		self.ensure_registered_root(&record.root)?;
		if !request.force {
			let current = self.snapshot_at(&record.root, &[], cancel)?;
			let current = self.load_manifest(&current.snapshot_id)?;
			let base = self.load_manifest(record.base.as_str())?;
			if current.entries != base.entries {
				return Err(WorkspaceOperationError::InvalidGeneration(Str::new_static(
					"worktree has unmerged changes",
				)));
			}
		}
		fs::remove_dir_all(&record.root)?;
		remove_if_exists(&self.record_path(record.id.as_str()))?;
		remove_if_exists(
			&self
				.inner
				.worktree_root
				.join(".branches")
				.join(record.id.as_str()),
		)?;
		self.inner.worktrees.lock().remove(&key);
		worktree_info(&record)
	}

	/// Emits a manifest diff artifact or records internal branch metadata for a
	/// worktree.
	pub fn merge_worktree(
		&self,
		request: &pb::MergeWorktree,
		cancel: &CancellationToken,
	) -> Result<WorktreeMerge, WorkspaceOperationError> {
		let key = Str::from(request.id.as_str());
		let mut record = self
			.inner
			.worktrees
			.lock()
			.get(&key)
			.cloned()
			.ok_or_else(|| WorkspaceOperationError::WorktreeNotFound(key.clone()))?;
		self.ensure_registered_root(&record.root)?;
		let current_snapshot = self.snapshot_at(&record.root, &[], cancel)?;
		let base = self.load_manifest(record.base.as_str())?;
		let current = self.load_manifest(&current_snapshot.snapshot_id)?;
		let mode = pb::MergeMode::try_from(request.mode).unwrap_or(pb::MergeMode::Unspecified);
		let (artifact, branch) = match mode {
			pb::MergeMode::Patch => (Some(self.write_manifest_diff(&base, &current, cancel)?), None),
			pb::MergeMode::Branch => {
				let branch = Str::from(format!("omp/agent/{}", record.id));
				if !request.dry_run {
					record.branch = Some(branch.clone());
					fs::write(
						self
							.inner
							.worktree_root
							.join(".branches")
							.join(record.id.as_str()),
						current_snapshot.snapshot_id.as_bytes(),
					)?;
					self.write_worktree_record(&record)?;
					self.inner.worktrees.lock().insert(key, record.clone());
				}
				(None, Some(branch))
			},
			pb::MergeMode::None | pb::MergeMode::Unspecified => (None, record.branch.clone()),
		};
		Ok(WorktreeMerge {
			worktree: worktree_info(&record)?,
			artifact,
			branch,
			conflicts: Vec::new(),
		})
	}

	fn snapshot_at(
		&self,
		root: &Path,
		paths: &[String],
		cancel: &CancellationToken,
	) -> Result<pb::WorkspaceSnapshot, WorkspaceOperationError> {
		let root = fs::canonicalize(root)?;
		if root != self.inner.workspace.root() {
			self.ensure_registered_root(&root)?;
		}
		let prefixes = normalize_prefixes(paths)?;
		let host = WorkspaceHost::open(&root)?;
		let request = host
			.request()
			.hidden(true)
			.gitignore(true)
			.skip_git(true)
			.order(WalkOrder::Path);
		let mut manifest = self.inner.blobs.begin_spill()?;
		manifest.write_all(MANIFEST_MAGIC)?;
		write_u32(&mut manifest, prefixes.len())?;
		let mut manifest_bytes = MANIFEST_MAGIC.len() as u64 + 4;
		for prefix in &prefixes {
			manifest_bytes = manifest_bytes.saturating_add(4 + prefix.len() as u64);
			check_manifest_bound(manifest_bytes)?;
			write_bytes(&mut manifest, prefix.as_bytes())?;
		}
		let mut files = 0_u64;
		let mut bytes = 0_u64;
		let mut failure = None;
		host.walk_stream(&request, cancel, |entry| {
			if entry.file_type != FileType::File || !selected(entry.relative_path, &prefixes) {
				return ControlFlow::Continue(());
			}
			let result = (|| {
				let (blob, mode) = self.hash_file(entry.absolute_path.as_ref(), cancel)?;
				let encoded = 4_u64 + entry.relative_path.len() as u64 + 4 + 32;
				manifest_bytes = manifest_bytes.saturating_add(encoded);
				check_manifest_bound(manifest_bytes)?;
				write_bytes(&mut manifest, entry.relative_path.as_bytes())?;
				manifest.write_all(&mode.to_be_bytes())?;
				manifest.write_all(&blob.hash)?;
				files += 1;
				bytes = bytes.saturating_add(blob.size);
				Ok::<(), WorkspaceOperationError>(())
			})();
			if let Err(error) = result {
				failure = Some(error);
				ControlFlow::Break(())
			} else {
				ControlFlow::Continue(())
			}
		})?;
		if let Some(error) = failure {
			return Err(error);
		}
		if cancel.is_cancelled() {
			return Err(WorkspaceError::Cancelled.into());
		}
		let reference = manifest.finish().map_err(BlobError::from)?;
		let snapshot_id = hex::encode(&reference.hash).into_string();
		Ok(pb::WorkspaceSnapshot {
			snapshot_id,
			manifest_hash: Bytes::copy_from_slice(&reference.hash),
			files,
			bytes,
			props: Default::default(),
		})
	}

	fn hash_file(
		&self,
		path: &Path,
		cancel: &CancellationToken,
	) -> Result<(BlobId, u32), WorkspaceOperationError> {
		let mut source = open_snapshot_file(path)?;
		let metadata = source.metadata()?;
		if !metadata.is_file() {
			return Err(WorkspaceOperationError::OutsideRoot);
		}
		let fingerprint = file_fingerprint(&metadata);
		if let Some(cached) = self.inner.cache.lock().get(path)
			&& cached.fingerprint == fingerprint
		{
			return Ok((cached.blob, fingerprint.mode));
		}
		let mut stage = self.inner.blobs.begin_spill()?;
		let mut buffer = Box::new([0_u8; IO_BUFFER_BYTES]);
		loop {
			if cancel.is_cancelled() {
				return Err(WorkspaceError::Cancelled.into());
			}
			let read = source.read(&mut buffer[..])?;
			if read == 0 {
				break;
			}
			stage.write_all(&buffer[..read])?;
		}
		let blob = BlobId::from(stage.finish().map_err(BlobError::from)?);
		self
			.inner
			.cache
			.lock()
			.insert(path.to_path_buf(), CachedFile { fingerprint, blob });
		Ok((blob, fingerprint.mode))
	}

	fn load_manifest(&self, snapshot_id: &str) -> Result<Manifest, WorkspaceOperationError> {
		let hash = hex::decode(snapshot_id).into_array::<32>().map_err(|_| {
			WorkspaceOperationError::InvalidGeneration(Str::new_static("invalid manifest hash"))
		})?;
		let stat = self.inner.blobs.stat(&hash)?;
		if !stat.present || stat.size > MAX_MANIFEST_BYTES {
			return Err(WorkspaceOperationError::InvalidGeneration(Str::new_static(
				"manifest is missing or exceeds the size bound",
			)));
		}
		let bytes = self.inner.blobs.get(BlobId { hash, size: stat.size })?;
		parse_manifest(&bytes)
	}

	async fn plan_restore(
		&self,
		target: &Manifest,
		current: &Manifest,
		cancel: &CancellationToken,
	) -> Result<Vec<RestorePlan>, WorkspaceOperationError> {
		let mut plans = Vec::new();
		for entry in target.entries.values() {
			if cancel.is_cancelled() {
				return Err(WorkspaceError::Cancelled.into());
			}
			let path = checked_join(self.inner.workspace.root(), entry.path.as_str())?;
			let uri = file_uri(&path)?;
			let lease = self.inner.documents.open(uri, None, cancel).await?;
			let presence = document_pb::DocumentPresence::try_from(lease.head().presence)
				.unwrap_or(document_pb::DocumentPresence::Unspecified);
			if presence == document_pb::DocumentPresence::Present {
				let content = read_whole(&self.inner.documents, &lease).await?;
				let actual = *blake3::hash(&content).as_bytes();
				let mode = fs::metadata(&path).map_or(0, |metadata| file_mode(&metadata));
				if actual == entry.hash && mode == entry.mode {
					continue;
				}
				plans.push(RestorePlan::Replace { entry: entry.clone(), lease });
			} else {
				plans.push(RestorePlan::Create { entry: entry.clone(), lease });
			}
		}
		for entry in current.entries.values() {
			if !selected(entry.path.as_str(), &target.prefixes)
				|| target.entries.contains_key(&entry.path)
			{
				continue;
			}
			let path = checked_join(self.inner.workspace.root(), entry.path.as_str())?;
			let lease = self
				.inner
				.documents
				.open(file_uri(&path)?, None, cancel)
				.await?;
			plans.push(RestorePlan::Delete { path: entry.path.clone(), lease });
		}
		Ok(plans)
	}

	async fn acquire_restore_lease(
		&self,
		plans: &[RestorePlan],
		dry_run: bool,
		cancel: &CancellationToken,
	) -> Result<(Option<WorkspaceLease>, Vec<pb::WorkspaceConflict>), WorkspaceOperationError> {
		if plans.is_empty() {
			return Ok((None, Vec::new()));
		}
		let mut paths = BTreeMap::new();
		for plan in plans {
			let path = plan.path().clone();
			let absolute = checked_join(self.inner.workspace.root(), path.as_str())?;
			paths.insert(file_uri(&absolute)?.to_string(), path);
		}
		let (lease, response) = self
			.inner
			.documents
			.acquire_workspace_lease(
				document_pb::AcquireWorkspaceLeaseRequest {
					uris: paths.keys().cloned().collect(),
					transaction_id: Bytes::copy_from_slice(&Ulid::generate().to_bytes()),
					dry_run,
				},
				cancel,
			)
			.await?;
		let conflicts = response
			.conflicts
			.into_iter()
			.map(|conflict| {
				let path = paths
					.get(&conflict.uri)
					.cloned()
					.unwrap_or_else(|| Str::from(conflict.uri));
				workspace_conflict(
					path,
					pb::ConflictReason::OpenLease,
					Some(Str::from(hex::encode(&conflict.active_lease_id).into_string())),
				)
			})
			.collect();
		Ok((lease, conflicts))
	}

	async fn apply_restore_plan(
		&self,
		plan: RestorePlan,
		cancel: &CancellationToken,
	) -> Result<(), ApplyFailure> {
		let before_commit = |reason| ApplyFailure { reason, effects: false };
		let (path, lease, operation, mode) = match plan {
			RestorePlan::Replace { entry, lease } => {
				let bytes = self
					.read_entry_blob(&entry)
					.map_err(|error| before_commit(map_operation_error(&error)))?;
				let revision = lease
					.head()
					.revision
					.clone()
					.ok_or_else(|| before_commit(pb::ConflictReason::ModifiedAfterSnapshot))?;
				let mutation = document_pb::TextMutation {
					base_revision: Some(revision),
					change:        Some(document_pb::text_mutation::Change::ProposedContent(bytes)),
					stale_policy:  document_pb::StalePolicy::Fail as i32,
					format_policy: document_pb::FormatPolicy::Disabled as i32,
				};
				(
					entry.path,
					lease,
					document_pb::document_mutation::Operation::Text(mutation),
					Some(entry.mode),
				)
			},
			RestorePlan::Create { entry, lease } => {
				let bytes = self
					.read_entry_blob(&entry)
					.map_err(|error| before_commit(map_operation_error(&error)))?;
				let mutation = document_pb::CreateMutation {
					content:           bytes,
					existing_document: document_pb::ExistingDocumentPolicy::FailIfExists as i32,
					format_policy:     document_pb::FormatPolicy::Disabled as i32,
				};
				(
					entry.path,
					lease,
					document_pb::document_mutation::Operation::Create(mutation),
					Some(entry.mode),
				)
			},
			RestorePlan::Delete { path, lease } => {
				let revision = lease
					.head()
					.revision
					.clone()
					.ok_or_else(|| before_commit(pb::ConflictReason::ModifiedAfterSnapshot))?;
				(
					path,
					lease,
					document_pb::document_mutation::Operation::Delete(document_pb::DeleteMutation {
						base_revision: Some(revision),
					}),
					None,
				)
			},
		};
		let response = self
			.inner
			.documents
			.commit_transaction(
				Bytes::copy_from_slice(&Ulid::generate().to_bytes()),
				vec![document_pb::DocumentMutation {
					document:  Some(lease_target(&lease)),
					operation: Some(operation),
				}],
				cancel,
			)
			.await
			.map_err(|_| ApplyFailure {
				reason:  pb::ConflictReason::GenerationChanged,
				effects: true,
			})?;
		match response.outcome {
			Some(document_pb::commit_transaction_response::Outcome::Committed(_)) => {
				if let Some(mode) = mode {
					let absolute =
						checked_join(self.inner.workspace.root(), path.as_str()).map_err(|_| {
							ApplyFailure { reason: pb::ConflictReason::OutsideRoot, effects: true }
						})?;
					set_mode(&absolute, mode).map_err(|_| ApplyFailure {
						reason:  pb::ConflictReason::Permission,
						effects: true,
					})?;
				}
				Ok(())
			},
			Some(document_pb::commit_transaction_response::Outcome::Rejected(rejected)) => {
				Err(before_commit(map_reject_reason(rejected.reason)))
			},
			Some(document_pb::commit_transaction_response::Outcome::PartiallyCommitted(partial)) => {
				Err(ApplyFailure { reason: map_reject_reason(partial.reason), effects: true })
			},
			None => Err(before_commit(pb::ConflictReason::GenerationChanged)),
		}
	}

	fn read_entry_blob(&self, entry: &ManifestEntry) -> Result<Bytes, WorkspaceOperationError> {
		let stat = self.inner.blobs.stat(&entry.hash)?;
		if !stat.present {
			return Err(WorkspaceOperationError::InvalidGeneration(Str::new_static(
				"file blob is missing",
			)));
		}
		Ok(self
			.inner
			.blobs
			.get(BlobId { hash: entry.hash, size: stat.size })?)
	}

	fn write_manifest_diff(
		&self,
		base: &Manifest,
		current: &Manifest,
		cancel: &CancellationToken,
	) -> Result<BlobId, WorkspaceOperationError> {
		let mut stage = self.inner.blobs.begin_spill()?;
		stage.write_all(DIFF_MAGIC)?;
		for (path, entry) in &current.entries {
			if cancel.is_cancelled() {
				return Err(WorkspaceError::Cancelled.into());
			}
			if base.entries.get(path) == Some(entry) {
				continue;
			}
			stage.write_all(b"M")?;
			write_bytes(&mut stage, path.as_bytes())?;
			stage.write_all(&entry.mode.to_be_bytes())?;
			stage.write_all(&entry.hash)?;
		}
		for path in base.entries.keys() {
			if current.entries.contains_key(path) {
				continue;
			}
			stage.write_all(b"D")?;
			write_bytes(&mut stage, path.as_bytes())?;
		}
		Ok(BlobId::from(stage.finish().map_err(BlobError::from)?))
	}

	fn record_path(&self, id: &str) -> PathBuf {
		self
			.inner
			.worktree_root
			.join(".records")
			.join(format!("{id}.json"))
	}

	fn write_worktree_record(&self, record: &WorktreeRecord) -> Result<(), WorkspaceOperationError> {
		let durable = DurableWorktreeRecord {
			version:     1,
			id:          record.id.to_string(),
			root:        record.root.clone(),
			base:        record.base.to_string(),
			generation:  record.generation,
			branch:      record.branch.as_ref().map(ToString::to_string),
			owner_pid:   record.owner_pid,
			class:       "task-isolation".to_owned(),
			source_root: self.inner.workspace.root().to_path_buf(),
		};
		let bytes = serde_json::to_vec(&durable).map_err(|error| {
			WorkspaceOperationError::InvalidWorktreeRecord(Str::from(error.to_string()))
		})?;
		let path = self.record_path(record.id.as_str());
		let temporary = path.with_extension(format!("json.{}.tmp", Ulid::generate()));
		fs::write(&temporary, bytes)?;
		fs::rename(temporary, path)?;
		Ok(())
	}

	fn ensure_registered_root(&self, root: &Path) -> Result<(), WorkspaceOperationError> {
		let root = fs::canonicalize(root)?;
		if !root.starts_with(&self.inner.worktree_root)
			|| root.parent() != Some(self.inner.worktree_root.as_path())
			|| !self
				.inner
				.worktrees
				.lock()
				.values()
				.any(|record| record.root == root)
		{
			return Err(WorkspaceOperationError::OutsideRoot);
		}
		Ok(())
	}
}

fn load_worktree_records(
	worktree_root: &Path,
) -> Result<(HashMap<Str, WorktreeRecord>, u64), WorkspaceOperationError> {
	let mut records = HashMap::new();
	let mut next_generation = 1_u64;
	for entry in fs::read_dir(worktree_root.join(".records"))? {
		let entry = entry?;
		if !entry.file_type()?.is_file()
			|| entry.path().extension().and_then(|value| value.to_str()) != Some("json")
		{
			continue;
		}
		let durable: DurableWorktreeRecord = match fs::read(entry.path())
			.ok()
			.and_then(|bytes| serde_json::from_slice(&bytes).ok())
		{
			Some(record) => record,
			None => {
				tracing::warn!(path = %entry.path().display(), "ignoring malformed worktree record");
				continue;
			},
		};
		if durable.version != 1
			|| durable.id.is_empty()
			|| durable.root.parent() != Some(worktree_root)
			|| !durable.root.exists()
		{
			continue;
		}
		next_generation = next_generation.max(durable.generation.saturating_add(1));
		let id = Str::from(durable.id);
		records.insert(id.clone(), WorktreeRecord {
			id,
			root: durable.root,
			base: Str::from(durable.base),
			generation: durable.generation,
			branch: durable.branch.map(Str::from),
			owner_pid: durable.owner_pid,
		});
	}
	Ok((records, next_generation))
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
	match fs::remove_file(path) {
		Ok(()) => Ok(()),
		Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
		Err(error) => Err(error),
	}
}

enum RestorePlan {
	Replace { entry: ManifestEntry, lease: DocumentLease },
	Create { entry: ManifestEntry, lease: DocumentLease },
	Delete { path: Str, lease: DocumentLease },
}

struct ApplyFailure {
	reason:  pb::ConflictReason,
	effects: bool,
}

impl RestorePlan {
	const fn path(&self) -> &Str {
		match self {
			Self::Replace { entry, .. } | Self::Create { entry, .. } => &entry.path,
			Self::Delete { path, .. } => path,
		}
	}
}

fn normalize_prefixes(paths: &[String]) -> Result<Vec<Str>, WorkspaceOperationError> {
	let mut prefixes = Vec::with_capacity(paths.len());
	for path in paths {
		let normalized = normalize_relative(path)?;
		if !prefixes.contains(&normalized) {
			prefixes.push(normalized);
		}
	}
	prefixes.sort_unstable();
	Ok(prefixes)
}

fn normalize_relative(path: &str) -> Result<Str, WorkspaceOperationError> {
	let path = Path::new(path);
	if path.is_absolute() || path.as_os_str().is_empty() {
		return Err(WorkspaceOperationError::OutsideRoot);
	}
	let mut normalized = String::new();
	for component in path.components() {
		match component {
			Component::Normal(component) => {
				let component = component
					.to_str()
					.ok_or(WorkspaceOperationError::OutsideRoot)?;
				if !normalized.is_empty() {
					normalized.push('/');
				}
				normalized.push_str(component);
			},
			Component::CurDir => {},
			_ => return Err(WorkspaceOperationError::OutsideRoot),
		}
	}
	if normalized.is_empty() {
		return Err(WorkspaceOperationError::OutsideRoot);
	}
	Ok(Str::from(normalized))
}

fn selected(path: &str, prefixes: &[Str]) -> bool {
	prefixes.is_empty()
		|| prefixes.iter().any(|prefix| {
			path == prefix.as_str()
				|| path
					.strip_prefix(prefix.as_str())
					.is_some_and(|suffix| suffix.starts_with('/'))
		})
}

fn checked_join(root: &Path, relative: &str) -> Result<PathBuf, WorkspaceOperationError> {
	let normalized = normalize_relative(relative)?;
	let joined = root.join(normalized.as_str());
	if joined.starts_with(root) {
		Ok(joined)
	} else {
		Err(WorkspaceOperationError::OutsideRoot)
	}
}

fn parse_manifest(bytes: &[u8]) -> Result<Manifest, WorkspaceOperationError> {
	let mut input = Cursor::new(bytes);
	let mut magic = [0_u8; MANIFEST_MAGIC.len()];
	input.read_exact(&mut magic).map_err(invalid_manifest)?;
	if &magic != MANIFEST_MAGIC {
		return Err(WorkspaceOperationError::InvalidGeneration(Str::new_static(
			"manifest magic mismatch",
		)));
	}
	let prefix_count = read_u32(&mut input)?;
	if u64::from(prefix_count) > (bytes.len() as u64).saturating_sub(input.position()) / 4 {
		return Err(WorkspaceOperationError::InvalidGeneration(Str::new_static(
			"manifest prefix count exceeds remaining bytes",
		)));
	}
	let mut prefixes = Vec::with_capacity(prefix_count as usize);
	for _ in 0..prefix_count {
		let prefix = Str::from(read_string(&mut input)?);
		if normalize_relative(prefix.as_str())? != prefix {
			return Err(WorkspaceOperationError::InvalidGeneration(Str::new_static(
				"manifest prefix is not canonical",
			)));
		}
		prefixes.push(prefix);
	}
	if !prefixes.windows(2).all(|pair| pair[0] < pair[1]) {
		return Err(WorkspaceOperationError::InvalidGeneration(Str::new_static(
			"manifest prefixes are not strictly sorted",
		)));
	}
	let mut entries = BTreeMap::new();
	while input.position() < bytes.len() as u64 {
		let path = Str::from(read_string(&mut input)?);
		if normalize_relative(path.as_str())? != path {
			return Err(WorkspaceOperationError::InvalidGeneration(Str::new_static(
				"manifest path is not canonical",
			)));
		}
		let mut mode = [0_u8; 4];
		input.read_exact(&mut mode).map_err(invalid_manifest)?;
		let mode = u32::from_be_bytes(mode);
		if mode & !0o7777 != 0 {
			return Err(WorkspaceOperationError::InvalidGeneration(Str::new_static(
				"manifest mode is invalid",
			)));
		}
		let mut hash = [0_u8; 32];
		input.read_exact(&mut hash).map_err(invalid_manifest)?;
		let entry = ManifestEntry { path: path.clone(), mode, hash };
		if entries.insert(path, entry).is_some() {
			return Err(WorkspaceOperationError::InvalidGeneration(Str::new_static(
				"duplicate manifest path",
			)));
		}
	}
	Ok(Manifest { prefixes, entries })
}

fn write_bytes(writer: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
	let length =
		u32::try_from(bytes.len()).map_err(|_| io::Error::other("manifest path exceeds u32"))?;
	writer.write_all(&length.to_be_bytes())?;
	writer.write_all(bytes)
}

fn write_u32(writer: &mut impl Write, value: usize) -> io::Result<()> {
	let value = u32::try_from(value).map_err(|_| io::Error::other("manifest count exceeds u32"))?;
	writer.write_all(&value.to_be_bytes())
}

fn read_u32(reader: &mut impl Read) -> Result<u32, WorkspaceOperationError> {
	let mut bytes = [0_u8; 4];
	reader.read_exact(&mut bytes).map_err(invalid_manifest)?;
	Ok(u32::from_be_bytes(bytes))
}

fn read_string(reader: &mut impl Read) -> Result<String, WorkspaceOperationError> {
	let length = read_u32(reader)? as usize;
	if length > MAX_MANIFEST_BYTES as usize {
		return Err(WorkspaceOperationError::InvalidGeneration(Str::new_static(
			"manifest path exceeds bound",
		)));
	}
	let mut bytes = vec![0_u8; length];
	reader.read_exact(&mut bytes).map_err(invalid_manifest)?;
	String::from_utf8(bytes).map_err(|_| {
		WorkspaceOperationError::InvalidGeneration(Str::new_static("manifest path is not UTF-8"))
	})
}

fn invalid_manifest(error: io::Error) -> WorkspaceOperationError {
	WorkspaceOperationError::InvalidGeneration(Str::from(error.to_string()))
}

const fn check_manifest_bound(bytes: u64) -> Result<(), WorkspaceOperationError> {
	if bytes > MAX_MANIFEST_BYTES {
		Err(WorkspaceOperationError::InvalidGeneration(Str::new_static(
			"manifest exceeds size bound",
		)))
	} else {
		Ok(())
	}
}

fn validate_worktree_name(name: &str) -> Result<(), WorkspaceOperationError> {
	if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
		Err(WorkspaceOperationError::InvalidWorktreeName)
	} else {
		Ok(())
	}
}

fn worktree_info(record: &WorktreeRecord) -> Result<pb::WorktreeInfo, WorkspaceOperationError> {
	Ok(pb::WorktreeInfo {
		id:         record.id.to_string(),
		root_uri:   file_uri(&record.root)?.to_string(),
		base:       record.base.to_string(),
		generation: record.generation,
		props:      Default::default(),
	})
}

fn file_uri(path: &Path) -> Result<Str, WorkspaceOperationError> {
	url::Url::from_file_path(path)
		.map(|url| Str::from(url.to_string()))
		.map_err(|()| WorkspaceOperationError::OutsideRoot)
}

fn workspace_conflict(
	path: Str,
	reason: pb::ConflictReason,
	detail: Option<Str>,
) -> pb::WorkspaceConflict {
	pb::WorkspaceConflict {
		path:   path.to_string(),
		reason: reason as i32,
		detail: detail.map(|detail| detail.to_string()),
	}
}

fn map_reject_reason(reason: i32) -> pb::ConflictReason {
	match document_pb::TransactionRejectReason::try_from(reason) {
		Ok(document_pb::TransactionRejectReason::PreconditionFailed) => pb::ConflictReason::OpenLease,
		Ok(
			document_pb::TransactionRejectReason::StaleBase
			| document_pb::TransactionRejectReason::OverlappingChange
			| document_pb::TransactionRejectReason::ExternalModification
			| document_pb::TransactionRejectReason::RevisionExpired,
		) => pb::ConflictReason::ModifiedAfterSnapshot,
		Ok(document_pb::TransactionRejectReason::PersistFailed) => pb::ConflictReason::Permission,
		_ => pb::ConflictReason::PathChanged,
	}
}

fn map_operation_error(error: &WorkspaceOperationError) -> pb::ConflictReason {
	match error {
		WorkspaceOperationError::OutsideRoot => pb::ConflictReason::OutsideRoot,
		WorkspaceOperationError::InvalidGeneration(_) => pb::ConflictReason::PathMissing,
		WorkspaceOperationError::Io(error) if error.kind() == io::ErrorKind::PermissionDenied => {
			pb::ConflictReason::Permission
		},
		_ => pb::ConflictReason::PathChanged,
	}
}

fn file_fingerprint(metadata: &fs::Metadata) -> FileFingerprint {
	let modified_ns = metadata
		.modified()
		.ok()
		.and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
		.map_or(0, |value| value.as_nanos());
	FileFingerprint {
		len: metadata.len(),
		modified_ns,
		mode: file_mode(metadata),
		identity: file_identity(metadata),
		change_ns: file_change_ns(metadata),
	}
}

#[cfg(unix)]
fn open_snapshot_file(path: &Path) -> io::Result<File> {
	use std::os::unix::fs::OpenOptionsExt as _;
	File::options()
		.read(true)
		.custom_flags(libc::O_NOFOLLOW)
		.open(path)
}

#[cfg(not(unix))]
fn open_snapshot_file(path: &Path) -> io::Result<File> {
	File::open(path)
}

#[cfg(unix)]
fn file_mode(metadata: &fs::Metadata) -> u32 {
	use std::os::unix::fs::MetadataExt as _;
	metadata.mode() & 0o7777
}

#[cfg(not(unix))]
fn file_mode(metadata: &fs::Metadata) -> u32 {
	if metadata.permissions().readonly() {
		0o444
	} else {
		0o666
	}
}

#[cfg(unix)]
fn file_identity(metadata: &fs::Metadata) -> u64 {
	use std::os::unix::fs::MetadataExt as _;
	metadata.ino()
}

#[cfg(not(unix))]
fn file_identity(_metadata: &fs::Metadata) -> u64 {
	0
}

#[cfg(unix)]
fn file_change_ns(metadata: &fs::Metadata) -> i128 {
	use std::os::unix::fs::MetadataExt as _;
	i128::from(metadata.ctime()) * 1_000_000_000 + i128::from(metadata.ctime_nsec())
}

#[cfg(not(unix))]
fn file_change_ns(_metadata: &fs::Metadata) -> i128 {
	0
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
	use std::os::unix::fs::PermissionsExt as _;
	fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
	let mut permissions = fs::metadata(path)?.permissions();
	permissions.set_readonly(mode & 0o222 == 0);
	fs::set_permissions(path, permissions)
}

fn clone_file_cow(source: &Path, destination: &Path) -> Result<(), WorkspaceOperationError> {
	match try_clone_file_cow(source, destination) {
		Ok(()) => Ok(()),
		Err(error) if cow_is_unsupported(&error) => {
			hardlink_copy_fallback(source, destination).map_err(Into::into)
		},
		Err(error) => Err(error.into()),
	}
}

#[cfg(target_os = "macos")]
fn try_clone_file_cow(source: &Path, destination: &Path) -> io::Result<()> {
	use std::os::unix::ffi::OsStrExt as _;
	let source = CString::new(source.as_os_str().as_bytes())
		.map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains NUL"))?;
	let destination = CString::new(destination.as_os_str().as_bytes())
		.map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "destination path contains NUL"))?;
	// SAFETY: both C strings are live, NUL-terminated filesystem paths and flags=0.
	let result = unsafe { libc::clonefile(source.as_ptr(), destination.as_ptr(), 0) };
	if result == 0 {
		Ok(())
	} else {
		Err(io::Error::last_os_error())
	}
}

#[cfg(target_os = "linux")]
fn try_clone_file_cow(source: &Path, destination: &Path) -> io::Result<()> {
	let source = File::open(source)?;
	let output = OpenOptions::new()
		.write(true)
		.create_new(true)
		.open(destination)?;
	const FICLONE: libc::c_ulong = 0x4004_9409;
	// SAFETY: FICLONE reads both valid file descriptors and does not retain them.
	let result = unsafe { libc::ioctl(output.as_raw_fd(), FICLONE, source.as_raw_fd()) };
	if result == 0 {
		Ok(())
	} else {
		let error = io::Error::last_os_error();
		drop(output);
		let _ = fs::remove_file(destination);
		Err(error)
	}
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn try_clone_file_cow(_source: &Path, _destination: &Path) -> io::Result<()> {
	Err(io::Error::from(io::ErrorKind::Unsupported))
}

#[cfg(target_os = "macos")]
fn cow_is_unsupported(error: &io::Error) -> bool {
	matches!(error.raw_os_error(), Some(libc::ENOTSUP | libc::EXDEV))
}

#[cfg(target_os = "linux")]
fn cow_is_unsupported(error: &io::Error) -> bool {
	matches!(
		error.raw_os_error(),
		Some(libc::EOPNOTSUPP | libc::EXDEV | libc::ENOTTY | libc::EINVAL)
	)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn cow_is_unsupported(_error: &io::Error) -> bool {
	true
}

fn hardlink_copy_fallback(source: &Path, destination: &Path) -> io::Result<()> {
	// Probe the cheap same-device path, then immediately break the link before
	// exposing the worktree. A mutable workspace must never share writable
	// inodes with its isolated child.
	if fs::hard_link(source, destination).is_ok() {
		let temporary = destination.with_extension(format!("omp-copy-{}", Ulid::generate()));
		let copied = fs::copy(source, &temporary);
		if let Err(error) = copied {
			let _ = fs::remove_file(destination);
			let _ = fs::remove_file(temporary);
			return Err(error);
		}
		#[cfg(windows)]
		fs::remove_file(destination)?;
		fs::rename(temporary, destination)
	} else {
		fs::copy(source, destination).map(|_| ())
	}
}
