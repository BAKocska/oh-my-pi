//! Deterministic construction of the canonical system-prompt head.

use std::{
	collections::HashSet,
	fmt,
	path::{Path, PathBuf},
	sync::Arc,
};

use bytes::Bytes;
use omp_core::{Hash32, Str, sf};
use omp_proto::thread::v1::{self as thread, Item};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Immutable bytes and identity for one workspace context file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextFile {
	/// Workspace-relative or absolute path presented to the model.
	pub path:    PathBuf,
	/// Exact file bytes captured for this snapshot.
	pub content: Bytes,
}

impl ContextFile {
	/// Creates an immutable context-file input.
	#[inline]
	pub fn new(path: impl Into<PathBuf>, content: impl Into<Bytes>) -> Self {
		Self { path: path.into(), content: content.into() }
	}
}

/// Stable source-control identity included in a workspace prompt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VcsIdentity {
	/// Repository root captured for this snapshot.
	pub root: PathBuf,
	/// Stable revision, branch, or ref identity supplied by the host.
	pub head: Str,
}

impl VcsIdentity {
	/// Creates a source-control identity.
	#[inline]
	pub fn new(root: impl Into<PathBuf>, head: impl Into<Str>) -> Self {
		Self { root: root.into(), head: head.into() }
	}
}

/// Immutable input used to render a workspace system prompt.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkspaceInput {
	/// Current workspace directory captured by the host.
	pub cwd:           PathBuf,
	/// Optional source-control identity captured at the same boundary.
	pub vcs:           Option<VcsIdentity>,
	/// Ordered context files with exact, immutable contents.
	pub context_files: Arc<[ContextFile]>,
}

impl WorkspaceInput {
	/// Creates workspace input without source-control identity.
	#[inline]
	pub fn new(cwd: impl Into<PathBuf>, context_files: impl Into<Arc<[ContextFile]>>) -> Self {
		Self { cwd: cwd.into(), vcs: None, context_files: context_files.into() }
	}

	/// Attaches a stable source-control identity.
	#[inline]
	#[must_use]
	pub fn with_vcs(mut self, vcs: VcsIdentity) -> Self {
		self.vcs = Some(vcs);
		self
	}
}

/// Stable BLAKE3 digest of the canonical prompt items.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct PromptHash(Hash32);

impl PromptHash {
	/// Returns the digest bytes.
	#[inline]
	pub const fn as_bytes(&self) -> &[u8; 32] {
		self.0.as_bytes()
	}

	/// Returns the typed digest.
	#[inline]
	pub const fn digest(self) -> Hash32 {
		self.0
	}
}

impl From<[u8; 32]> for PromptHash {
	#[inline]
	fn from(bytes: [u8; 32]) -> Self {
		Self(Hash32::new(bytes))
	}
}

impl From<PromptHash> for [u8; 32] {
	#[inline]
	fn from(hash: PromptHash) -> Self {
		hash.0.into_bytes()
	}
}

impl From<Hash32> for PromptHash {
	#[inline]
	fn from(hash: Hash32) -> Self {
		Self(hash)
	}
}

impl From<PromptHash> for Hash32 {
	#[inline]
	fn from(hash: PromptHash) -> Self {
		hash.0
	}
}
/// BLAKE3 digest of one semantic prompt stability band.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(transparent)]
pub struct BandHash([u8; 32]);

impl BandHash {
	/// Returns the digest bytes.
	#[must_use]
	pub const fn as_bytes(&self) -> &[u8; 32] {
		&self.0
	}
}

/// Semantic stability of a prompt contribution.
///
/// Discriminants are assembly order, not a wire vocabulary.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum SlotClass {
	/// Immutable for the process lifetime.
	Frozen   = 0,
	/// Changes only after an explicit observable configuration event.
	Stable   = 1,
	/// Changes at a compaction or reset epoch boundary.
	Epochal  = 2,
	/// May change on every turn.
	Volatile = 3,
}

/// The fixed prompt-slot catalog.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum SlotId {
	/// RFC and harness conventions.
	Conventions = 0,
	/// Agent identity.
	Role        = 1,
	/// Runtime capability announcements.
	Runtime     = 2,
	/// Tool and device inventory.
	Tools       = 3,
	/// Tool-use policy.
	Policy      = 4,
	/// Engineering workflow.
	Workflow    = 5,
	/// Installed skills.
	Skills      = 6,
	/// Standing rules.
	Rules       = 7,
	/// General guidance.
	Guidance    = 8,
	/// Workspace identity and files.
	Workspace   = 9,
	/// Compaction-epoch memory.
	Memory      = 10,
	/// Compaction-epoch standing instructions.
	Standing    = 11,
	/// Per-turn recall.
	Recall      = 12,
	/// Per-turn runtime status.
	Status      = 13,
	/// Delivery contract.
	Delivery    = 14,
}

/// A declared prompt contribution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SlotDecl {
	/// Destination slot.
	pub slot:     SlotId,
	/// Declared stability band.
	pub class:    SlotClass,
	/// Stable extension identity used as a deterministic tie-break.
	pub owner:    Str,
	/// Descending order within a slot.
	pub priority: i16,
}

/// Streaming byte sink supplied to a synchronous slot source.
pub trait PromptOut {
	/// Appends UTF-8 text to this contribution.
	fn write_str(&mut self, text: &str);
}

impl PromptOut for String {
	fn write_str(&mut self, text: &str) {
		self.push_str(text);
	}
}

/// Synchronous source of one registered prompt contribution.
pub trait SlotSource: Send + Sync + 'static {
	/// Renders this source from immutable workspace input.
	fn render(&self, workspace: &WorkspaceInput, out: &mut dyn PromptOut)
	-> Result<(), PromptError>;
}

/// Immutable bytes pulled from an extension at activation or invalidation time.
///
/// The host is responsible for double-calling an extension before constructing
/// this value; prompt rendering then never performs socket I/O.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CachedContribution {
	bytes: Str,
}

impl CachedContribution {
	/// Creates a contribution from host-validated immutable bytes.
	#[must_use]
	pub fn new(bytes: impl Into<Str>) -> Self {
		Self { bytes: bytes.into() }
	}
}

impl SlotSource for CachedContribution {
	fn render(
		&self,
		_workspace: &WorkspaceInput,
		out: &mut dyn PromptOut,
	) -> Result<(), PromptError> {
		out.write_str(self.bytes.as_str());
		Ok(())
	}
}
/// Built-in execution-role prompt selected for one immutable turn snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PromptMode {
	/// Read-only planning and review.
	Plan,
	/// Plan validation that switches on the first justified mutation.
	Prewalk,
	/// Capture durable lessons after execution.
	Autolearn,
	/// Continue autonomously toward a durable goal.
	Goal,
	/// Coordinate a broad agent swarm through one orchestration device.
	Vibe,
	/// Refresh compacted memory without performing unrelated work.
	MemoryPipeline,
	/// Advise without taking workspace ownership.
	Advisor,
	/// Gather external evidence before proposing changes.
	Autoresearch,
	/// Audit for exploitable security defects.
	SecurityAudit,
	/// Measure a defined scenario and report reproducible evidence.
	Bench,
	/// Review a concrete change set.
	Review,
	/// Prepare a verified, scoped commit.
	CommitPipeline,
	/// Remove generated residue without changing behavior.
	Cleanse,
	/// Compress context while preserving active constraints.
	Compress,
	/// Coordinate edits with live collaborators.
	LiveCollab,
}

impl PromptMode {
	const fn prompt(self) -> &'static str {
		match self {
			Self::Plan => {
				"Plan mode is active. Inspect freely, but do not mutate the workspace or spawn \
				 isolated agents. Produce an executable plan grounded in repository evidence.\n"
			},
			Self::Prewalk => {
				"Prewalk is active. Validate the proposed path first; mutate only after recording a \
				 concrete reason to execute. If no work is required, settle as a no-op.\n"
			},
			Self::Autolearn => {
				"Autolearn is active. Capture only durable, generalizable lessons supported by \
				 completed work; do not turn transient failures into standing rules.\n"
			},
			Self::Goal => {
				"Goal mode is active. Continue toward the stated goal within its live budget, pausing \
				 only on a real external prerequisite or explicit user control.\n"
			},
			Self::Vibe => {
				"Vibe mode is active. Coordinate the swarm through the single agent device, keep \
				 ownership explicit, and integrate only observable completed results.\n"
			},
			Self::MemoryPipeline => {
				"Memory-pipeline mode is active. Reconcile durable memory with current evidence, \
				 preserve provenance, and avoid unrelated workspace changes.\n"
			},
			Self::Advisor => {
				"Advisor mode is active. Return evidence-backed advice and risks without claiming \
				 workspace mutations you do not own.\n"
			},
			Self::Autoresearch => {
				"Autoresearch mode is active. Prefer primary sources, distinguish observation from \
				 inference, and retain source links for every material claim.\n"
			},
			Self::SecurityAudit => {
				"Security-audit mode is active. Report exploitable, evidence-backed findings with \
				 affected boundaries and realistic impact; omit speculative noise.\n"
			},
			Self::Bench => {
				"Bench mode is active. Hold the scenario and measurement method constant, run the \
				 defined workload, and report reproducible observations.\n"
			},
			Self::Review => {
				"Review mode is active. Prioritize correctness, security, and regressions in the \
				 supplied change set; cite exact evidence and omit style-only churn.\n"
			},
			Self::CommitPipeline => {
				"Commit-pipeline mode is active. Keep the change scoped, verify its observable \
				 contract, and exclude unrelated user work from the commit.\n"
			},
			Self::Cleanse => {
				"Cleanse mode is active. Remove generated residue and dead scaffolding while \
				 preserving observable behavior and user-authored work.\n"
			},
			Self::Compress => {
				"Compress mode is active. Preserve active requirements, decisions, provenance, and \
				 unresolved blockers while deleting redundant context.\n"
			},
			Self::LiveCollab => {
				"Live-collab mode is active. Announce ownership before overlap, consume peer results \
				 at clear boundaries, and never overwrite concurrent user work.\n"
			},
		}
	}
}

/// Immutable built-in [`SlotSource`] for one execution mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModePromptSource {
	mode: PromptMode,
}

impl ModePromptSource {
	/// Creates the prompt source for an active mode.
	#[must_use]
	pub const fn new(mode: PromptMode) -> Self {
		Self { mode }
	}

	/// Wraps this source in the canonical volatile status slot.
	#[must_use]
	pub fn registration(self) -> SlotRegistration {
		SlotRegistration {
			decl:   SlotDecl {
				slot:     SlotId::Status,
				class:    SlotClass::Volatile,
				owner:    sf!("omp.mode"),
				priority: 100,
			},
			source: Arc::new(self),
		}
	}
}

impl SlotSource for ModePromptSource {
	fn render(
		&self,
		_workspace: &WorkspaceInput,
		out: &mut dyn PromptOut,
	) -> Result<(), PromptError> {
		out.write_str(self.mode.prompt());
		Ok(())
	}
}

/// Availability snapshot for built-in internal-resource prompt entries.
///
/// These are immutable per assembler instance so conditional rendering remains
/// deterministic across the assembler's double-render check.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ConditionalPromptEntries {
	/// `memory://root` is mounted for durable session memory.
	pub memory_root:    bool,
	/// `security://scans` is mounted for retained audit findings.
	pub security_scans: bool,
	/// `vault://` is mounted with Obsidian operators.
	pub obsidian_vault: bool,
}

impl ConditionalPromptEntries {
	/// Wraps the available entries in the stable runtime slot.
	#[must_use]
	pub fn registration(self) -> SlotRegistration {
		SlotRegistration {
			decl:   SlotDecl {
				slot:     SlotId::Runtime,
				class:    SlotClass::Stable,
				owner:    sf!("omp.conditional-entries"),
				priority: 0,
			},
			source: Arc::new(self),
		}
	}
}

impl SlotSource for ConditionalPromptEntries {
	fn render(
		&self,
		_workspace: &WorkspaceInput,
		out: &mut dyn PromptOut,
	) -> Result<(), PromptError> {
		if self.memory_root {
			out.write_str(
				"`memory://root` is available for durable memory; read before replacing and preserve \
				 provenance.\n",
			);
		}
		if self.security_scans {
			out.write_str(
				"`security://scans` is available for retained audit findings; treat entries as \
				 evidence, not automatic truth.\n",
			);
		}
		if self.obsidian_vault {
			out.write_str(
				"`vault://` Obsidian operators are available; preserve links and frontmatter when \
				 editing vault notes.\n",
			);
		}
		Ok(())
	}
}

/// One declaration paired with its immutable or host-cached source.
#[derive(Clone)]
pub struct SlotRegistration {
	/// Registration metadata.
	pub decl:   SlotDecl,
	/// Source that provides this declaration's bytes.
	pub source: Arc<dyn SlotSource>,
}

/// Journal-facing record for a dropped nondeterministic contribution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VolatilePrompt {
	/// Slot whose source differed on its two renders.
	pub slot:   SlotId,
	/// Extension identity of the rejected source.
	pub owner:  Str,
	/// Digest of the first bytes.
	pub first:  BandHash,
	/// Digest of the second bytes.
	pub second: BandHash,
}

/// Receives durable `omp.VolatilePrompt` records.
pub trait VolatilePromptJournal: Send + Sync + 'static {
	/// Appends one dropped-contribution record.
	fn volatile_prompt(&self, record: VolatilePrompt);
}

/// Composes registered slots into a deterministic canonical prompt source.
pub struct SlotAssembler {
	registrations: Vec<SlotRegistration>,
	dropped:       Mutex<HashSet<Str>>,
	journal:       Option<Arc<dyn VolatilePromptJournal>>,
}

impl SlotAssembler {
	/// Creates an assembler, sorting registrations by class, declared slot,
	/// priority, and owner.
	#[must_use]
	pub fn new(mut registrations: Vec<SlotRegistration>) -> Self {
		registrations.sort_by(|left, right| {
			left
				.decl
				.class
				.cmp(&right.decl.class)
				.then(left.decl.slot.cmp(&right.decl.slot))
				.then(right.decl.priority.cmp(&left.decl.priority))
				.then(left.decl.owner.cmp(&right.decl.owner))
		});
		Self { registrations, dropped: Mutex::new(HashSet::new()), journal: None }
	}

	/// Attaches the durable journal sink used for rejected volatile sources.
	#[must_use]
	pub fn with_journal(mut self, journal: Arc<dyn VolatilePromptJournal>) -> Self {
		self.journal = Some(journal);
		self
	}

	/// Renders and returns hashes for all four semantic bands.
	pub fn render_banded(
		&self,
		workspace: &WorkspaceInput,
	) -> Result<(RenderedPrompt, [BandHash; 4]), PromptError> {
		let (items, bands) = self
			.banded_render(workspace)?
			.expect("slot assembler is banded");
		let mut hasher = Hash32::hasher();
		for band in bands {
			hasher.update(band.as_bytes());
		}
		Ok((RenderedPrompt { items: items.into(), hash: PromptHash(hasher.finalize()) }, bands))
	}

	fn assemble(&self, workspace: &WorkspaceInput) -> Result<AssembledSlots, PromptError> {
		let mut band_bytes = [String::new(), String::new(), String::new(), String::new()];
		for registration in &self.registrations {
			if self.dropped.lock().contains(&registration.decl.owner) {
				continue;
			}
			let mut first = String::new();
			registration.source.render(workspace, &mut first)?;
			let mut second = String::new();
			registration.source.render(workspace, &mut second)?;
			if first != second {
				let record = VolatilePrompt {
					slot:   registration.decl.slot,
					owner:  registration.decl.owner.clone(),
					first:  hash_band(first.as_bytes()),
					second: hash_band(second.as_bytes()),
				};
				self.dropped.lock().insert(registration.decl.owner.clone());
				if let Some(journal) = &self.journal {
					journal.volatile_prompt(record);
				}
				continue;
			}
			band_bytes[registration.decl.class as usize].push_str(&first);
		}
		let bands = band_bytes
			.each_ref()
			.map(|bytes| hash_band(bytes.as_bytes()));
		let items = band_bytes
			.into_iter()
			.filter(|bytes| !bytes.is_empty())
			.map(system_text)
			.collect();
		Ok(AssembledSlots { items, bands })
	}
}

impl PromptSource for SlotAssembler {
	fn render(&self, workspace: &WorkspaceInput) -> Result<Vec<Item>, PromptError> {
		Ok(self.assemble(workspace)?.items)
	}

	fn banded_render(
		&self,
		workspace: &WorkspaceInput,
	) -> Result<Option<(Vec<Item>, [BandHash; 4])>, PromptError> {
		let first = self.assemble(workspace)?;
		let second = self.assemble(workspace)?;
		if first.items != second.items {
			return Err(PromptError::Volatile);
		}
		Ok(Some((first.items, first.bands)))
	}
}

struct AssembledSlots {
	items: Vec<Item>,
	bands: [BandHash; 4],
}

fn hash_band(bytes: &[u8]) -> BandHash {
	BandHash(Hash32::sum(bytes).into_bytes())
}

/// A checked canonical prompt head and its content hash.
#[derive(Clone, Debug, PartialEq)]
pub struct RenderedPrompt {
	/// Ordered canonical system items.
	pub items: Arc<[Item]>,
	/// BLAKE3 digest of the canonical serialized items.
	pub hash:  PromptHash,
}

/// Synchronous source of canonical system-prompt items.
///
/// Implementations receive only immutable workspace input. Callers must use
/// [`render_prompt`] so the source is rendered twice and checked for volatile
/// output before its items enter a thread.
pub trait PromptSource: Send + Sync + 'static {
	/// Renders one candidate prompt head from immutable input.
	fn render(&self, workspace: &WorkspaceInput) -> Result<Vec<Item>, PromptError>;

	/// Optionally renders a head whose stability bands have semantic hashes.
	fn banded_render(
		&self,
		_workspace: &WorkspaceInput,
	) -> Result<Option<(Vec<Item>, [BandHash; 4])>, PromptError> {
		Ok(None)
	}
}

/// Deterministic plain-text renderer for workspace identity and context files.
#[derive(Clone, Copy, Debug, Default)]
pub struct WorkspacePromptSource;

impl PromptSource for WorkspacePromptSource {
	fn render(&self, workspace: &WorkspaceInput) -> Result<Vec<Item>, PromptError> {
		let cwd = prompt_path(&workspace.cwd)?;
		let mut identity = String::with_capacity(cwd.len() + 96);
		identity.push_str("Workspace\nDirectory: ");
		identity.push_str(cwd);
		if let Some(vcs) = &workspace.vcs {
			identity.push_str("\nRepository: ");
			identity.push_str(prompt_path(&vcs.root)?);
			identity.push_str("\nRevision: ");
			identity.push_str(vcs.head.as_str());
		}

		let mut items = Vec::with_capacity(1 + workspace.context_files.len());
		items.push(system_text(identity));
		for file in workspace.context_files.iter() {
			let path = prompt_path(&file.path)?;
			let content = std::str::from_utf8(&file.content)
				.map_err(|source| PromptError::ContextEncoding { path: file.path.clone(), source })?;
			let mut text = String::with_capacity(path.len() + content.len() + 32);
			text.push_str("Context file: ");
			text.push_str(path);
			text.push('\n');
			text.push_str(content);
			items.push(system_text(text));
		}
		Ok(items)
	}
}

/// Prompt rendering or canonicalization failure.
#[derive(Debug, Error)]
pub enum PromptError {
	/// The source emitted different items for identical immutable input.
	#[error("prompt source emitted volatile output for identical workspace input")]
	Volatile,
	/// A prompt item was not a canonical, unstamped system message.
	#[error("prompt item {index} is not a canonical unstamped system message")]
	InvalidItem {
		/// Zero-based index of the invalid item.
		index: usize,
	},
	/// A workspace path could not be represented exactly as UTF-8.
	#[error("workspace path is not valid UTF-8: {0:?}")]
	PathEncoding(PathBuf),
	/// A context file was not valid UTF-8.
	#[error("context file is not valid UTF-8: {path:?}")]
	ContextEncoding {
		/// Path of the invalid context file.
		path:   PathBuf,
		/// UTF-8 decoding failure.
		#[source]
		source: std::str::Utf8Error,
	},
	/// Canonical item serialization failed.
	#[error("failed to serialize canonical prompt items")]
	Serialize(#[from] serde_json::Error),
	/// A custom prompt source rejected its input.
	#[error("prompt source failed: {0}")]
	Source(Str),
}

/// Renders, validates, volatility-checks, and hashes one prompt head.
///
/// Plain sources are invoked twice against identical immutable input. Banded
/// sources perform the same check at their contribution boundary before their
/// four semantic hashes are folded into the wire-compatible prompt hash.
pub fn render_prompt(
	source: &dyn PromptSource,
	workspace: &WorkspaceInput,
) -> Result<RenderedPrompt, PromptError> {
	if let Some((items, bands)) = source.banded_render(workspace)? {
		validate_items(&items)?;
		let mut hasher = Hash32::hasher();
		for band in bands {
			hasher.update(band.as_bytes());
		}
		return Ok(RenderedPrompt { items: items.into(), hash: PromptHash(hasher.finalize()) });
	}
	let first = source.render(workspace)?;
	validate_items(&first)?;
	let second = source.render(workspace)?;
	validate_items(&second)?;
	if first != second {
		return Err(PromptError::Volatile);
	}
	drop(second);

	let mut hasher = Hash32::hasher();
	serde_json::to_writer(&mut hasher, &first)?;
	let hash = PromptHash(hasher.finalize());
	Ok(RenderedPrompt { items: first.into(), hash })
}

fn validate_items(items: &[Item]) -> Result<(), PromptError> {
	for (index, item) in items.iter().enumerate() {
		let canonical = item.seq == 0
			&& item.created_at_ms == 0
			&& item.props.is_none()
			&& matches!(
				item.kind.as_ref(),
				Some(thread::item::Kind::Message(message))
					if message.role == thread::Role::System as i32
			);
		if !canonical {
			return Err(PromptError::InvalidItem { index });
		}
	}
	Ok(())
}

fn prompt_path(path: &Path) -> Result<&str, PromptError> {
	path
		.to_str()
		.ok_or_else(|| PromptError::PathEncoding(path.to_path_buf()))
}

fn system_text(text: String) -> Item {
	Item {
		seq:           0,
		created_at_ms: 0,
		kind:          Some(thread::item::Kind::Message(thread::Message {
			role:  thread::Role::System as i32,
			parts: vec![thread::Part { kind: Some(thread::part::Kind::Text(text)) }],
		})),
		props:         None,
	}
}

impl fmt::Display for PromptHash {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		self.0.fmt(formatter)
	}
}

#[cfg(test)]
mod tests {
	use std::sync::atomic::{AtomicBool, Ordering};

	use super::*;

	#[test]
	fn workspace_prompt_is_canonical_and_stable() {
		let workspace = WorkspaceInput::new(
			"/workspace",
			Arc::from([ContextFile::new("AGENTS.md", Bytes::from_static(b"rules"))]),
		)
		.with_vcs(VcsIdentity::new("/workspace", "abc123"));
		let first = render_prompt(&WorkspacePromptSource, &workspace).expect("first render");
		let second = render_prompt(&WorkspacePromptSource, &workspace).expect("second render");

		assert_eq!(first, second);
		assert_eq!(first.items.len(), 2);
		assert!(first.items.iter().all(|item| matches!(
			item.kind.as_ref(),
			Some(thread::item::Kind::Message(message))
				if message.role == thread::Role::System as i32
		)));
		let changed = WorkspaceInput::new(
			"/workspace",
			Arc::from([ContextFile::new("AGENTS.md", Bytes::from_static(b"changed"))]),
		)
		.with_vcs(VcsIdentity::new("/workspace", "abc123"));
		let changed = render_prompt(&WorkspacePromptSource, &changed).expect("changed render");
		assert_ne!(first.hash, changed.hash);
	}

	#[test]
	fn volatile_source_is_rejected() {
		struct VolatileSource(AtomicBool);

		impl PromptSource for VolatileSource {
			fn render(&self, _workspace: &WorkspaceInput) -> Result<Vec<Item>, PromptError> {
				let prior = self.0.fetch_xor(true, Ordering::Relaxed);
				Ok(vec![system_text(prior.to_string())])
			}
		}

		let source = VolatileSource(AtomicBool::new(false));
		assert!(matches!(
			render_prompt(&source, &WorkspaceInput::default()),
			Err(PromptError::Volatile)
		));
	}
	#[derive(Clone)]
	struct TextSource(&'static str);

	impl SlotSource for TextSource {
		fn render(
			&self,
			_workspace: &WorkspaceInput,
			out: &mut dyn PromptOut,
		) -> Result<(), PromptError> {
			out.write_str(self.0);
			Ok(())
		}
	}

	#[test]
	fn band_hash_stability_isolated_to_volatile_band() {
		let stable = SlotRegistration {
			decl:   SlotDecl {
				slot:     SlotId::Policy,
				class:    SlotClass::Stable,
				owner:    "policy".into(),
				priority: 0,
			},
			source: Arc::new(TextSource("policy")),
		};
		let volatile_a = SlotRegistration {
			decl:   SlotDecl {
				slot:     SlotId::Status,
				class:    SlotClass::Volatile,
				owner:    "status".into(),
				priority: 0,
			},
			source: Arc::new(TextSource("one")),
		};
		let volatile_b =
			SlotRegistration { source: Arc::new(TextSource("two")), ..volatile_a.clone() };
		let (_, first) = SlotAssembler::new(vec![stable.clone(), volatile_a])
			.render_banded(&WorkspaceInput::default())
			.unwrap();
		let (_, second) = SlotAssembler::new(vec![stable, volatile_b])
			.render_banded(&WorkspaceInput::default())
			.unwrap();
		assert_eq!(first[..3], second[..3]);
		assert_ne!(first[3], second[3]);
	}

	#[test]
	fn volatile_slot_is_dropped_and_journaled() {
		struct VolatileSlot(AtomicBool);
		impl SlotSource for VolatileSlot {
			fn render(
				&self,
				_workspace: &WorkspaceInput,
				out: &mut dyn PromptOut,
			) -> Result<(), PromptError> {
				out.write_str(if self.0.fetch_xor(true, Ordering::Relaxed) {
					"a"
				} else {
					"b"
				});
				Ok(())
			}
		}
		#[derive(Default)]
		struct Journal(parking_lot::Mutex<Vec<VolatilePrompt>>);
		impl VolatilePromptJournal for Journal {
			fn volatile_prompt(&self, record: VolatilePrompt) {
				self.0.lock().push(record);
			}
		}
		let journal = Arc::new(Journal::default());
		let source = SlotRegistration {
			decl:   SlotDecl {
				slot:     SlotId::Recall,
				class:    SlotClass::Volatile,
				owner:    "recall".into(),
				priority: 0,
			},
			source: Arc::new(VolatileSlot(AtomicBool::new(false))),
		};
		let (rendered, _) = SlotAssembler::new(vec![source])
			.with_journal(journal.clone())
			.render_banded(&WorkspaceInput::default())
			.unwrap();
		assert!(rendered.items.is_empty());
		assert_eq!(journal.0.lock().len(), 1);
	}

	#[test]
	fn built_in_mode_and_conditional_sources_render_only_selected_entries() {
		let mut mode = String::new();
		ModePromptSource::new(PromptMode::Prewalk)
			.render(&WorkspaceInput::default(), &mut mode)
			.unwrap();
		assert!(mode.contains("reason to execute"));
		assert!(mode.contains("no-op"));

		let mut conditional = String::new();
		ConditionalPromptEntries {
			memory_root:    true,
			security_scans: false,
			obsidian_vault: true,
		}
		.render(&WorkspaceInput::default(), &mut conditional)
		.unwrap();
		assert!(conditional.contains("memory://root"));
		assert!(!conditional.contains("security://scans"));
		assert!(conditional.contains("vault://"));
	}
}
