//! Cross-process reconstruction of durable session intent.

use std::path::{Path, PathBuf};

use omp_core::Str;
use omp_proto::thread::v1::Item;
use omp_scribe::{Value, map};
use omp_storage::transcript::{self, Kind, ModelChange};
use thiserror::Error;

use crate::{AgentSnapshot, Journal, JournalError};

/// Cold-revival failure.
#[derive(Debug, Error)]
pub enum RevivalError {
	/// The journal could not be opened or projected.
	#[error(transparent)]
	Journal(#[from] JournalError),
	/// The transcript could not be read while deriving restart intent.
	#[error(transparent)]
	Transcript(#[from] transcript::Error),
}

/// Durable facts required to restart an equivalent loop around a journal.
pub struct RevivedSession {
	/// Sole mutable owner reopened on the existing journal.
	pub journal:        Journal,
	/// Reconstructed loop snapshot, including workspace and tool manifest.
	pub snapshot:       AgentSnapshot,
	/// Canonical live context after reset, rewind, and compaction projection.
	pub live_items:     Vec<Item>,
	/// Most recent journaled temporary model selection.
	pub model_override: Option<ModelChange>,
	/// Whether inference must discard provider-native session affinity before
	/// the next request.
	pub provider_reset: bool,
	/// Original immutable workspace root recorded by the journal header.
	pub original_root:  PathBuf,
}

/// Cold-loads the journal and applies its durable projections on the supplied
/// current policy/grants/tool registry snapshot.
///
/// The supplied snapshot owns current executable capabilities and policy. The
/// journal restores only names that still exist in that registry, preventing a
/// stale manifest from granting a tool that the restarted process did not
/// mount.
pub fn revive(path: &Path, snapshot: AgentSnapshot) -> Result<RevivedSession, RevivalError> {
	let journal = Journal::open(path)?;
	revive_existing(path, journal, snapshot)
}

/// Reconstructs durable intent while retaining an already-open sole journal
/// owner.
pub fn revive_existing(
	path: &Path,
	journal: Journal,
	mut snapshot: AgentSnapshot,
) -> Result<RevivedSession, RevivalError> {
	let log = transcript::load(path)?;
	let mut model_override = None;
	let mut provider_reset = false;
	for index in log.live() {
		let Some(transcript::Entry::Ok(event)) = log.get(index) else {
			continue;
		};
		match &event.kind {
			Kind::Infer { model: transcript::Patch::Set(change), .. } => {
				model_override = Some(change.clone());
			},
			Kind::Infer { model: transcript::Patch::Clear, .. } => model_override = None,
			Kind::ProviderReset => provider_reset = true,
			Kind::TurnReceipt(_) => provider_reset = false,
			_ => {},
		}
	}
	let roots = journal.workspace_roots(&log.header().cwd)?;
	let primary_uri = roots.primary().to_string_lossy().into_owned();
	snapshot
		.props
		.set(crate::prompt_keys::CWD, primary_uri.clone());
	let primary = map! { "canonical_uri" => primary_uri };
	let additional = roots
		.secondary()
		.iter()
		.map(|root| map! { "canonical_uri" => root.as_os_str().to_string_lossy().into_owned() })
		.collect::<Vec<_>>();
	let all = std::iter::once(primary.clone())
		.chain(additional.iter().cloned())
		.collect::<Vec<Value>>();
	snapshot.props.set(
		crate::prompt_keys::ROOTS,
		map! { "revision" => 0_i64, "primary" => primary, "roots" => all },
	);
	snapshot
		.props
		.set(crate::prompt_keys::ADDITIONAL_ROOTS, additional);
	if let Some(start) = journal.latest_turn_start() {
		let mounted = &snapshot.registry;
		snapshot.enabled_tools = start
			.enabled_tools
			.iter()
			.filter(|name| mounted.live_identity(name.as_str()).is_some())
			.cloned()
			.collect::<Vec<Str>>()
			.into();
	}
	let live_items = journal.items_at(&journal.live_item_events()?)?;
	Ok(RevivedSession {
		journal,
		snapshot,
		live_items,
		model_override,
		provider_reset,
		original_root: log.header().cwd.clone(),
	})
}
