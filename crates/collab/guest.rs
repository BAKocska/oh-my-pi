//! Guest-side snapshot reseed and live transcript-v4 append fencing.

use std::{
	collections::BTreeMap,
	path::{Path, PathBuf},
};

use omp_core::{Str, sf};
use omp_proto::collab::v1::{
	AgentSummary, ContextUsage, JournalRecord, ModelMetadata, RegistrySnapshot, SessionStateUpdate,
	SnapshotChunk, UiRequest, VisibilityClass, agent_summary, ui_request,
};
use omp_storage::transcript::{
	SessionId,
	replica::{
		RemoteProvenance, RemoteRecord, Replica, ReplicaError, ReplicaFence, ReplicaVisibility,
	},
};
use thiserror::Error;

/// Commands that remain local while rendering a remote collaboration replica.
///
/// Every command outside this closed set is host-owned and must be refused
/// before command dispatch.
#[must_use]
pub fn guest_command_allowed(command: &str) -> bool {
	let name = command
		.trim()
		.strip_prefix('/')
		.unwrap_or(command.trim())
		.split_ascii_whitespace()
		.next()
		.unwrap_or_default();
	matches!(
		name,
		"dump"
			| "export"
			| "copy"
			| "help"
			| "hotkeys"
			| "theme"
			| "settings"
			| "leave"
			| "collab"
			| "exit"
			| "quit"
	)
}

/// Applies the guest's local pre-send gates.
pub fn admit_guest_input(
	input: &str,
	read_only: bool,
) -> Result<GuestInputDisposition, GuestInputError> {
	if input.trim_start().starts_with('/') {
		if guest_command_allowed(input) {
			Ok(GuestInputDisposition::LocalCommand)
		} else {
			Err(GuestInputError::HostCommand)
		}
	} else if read_only {
		Err(GuestInputError::ReadOnly)
	} else {
		Ok(GuestInputDisposition::RemotePrompt)
	}
}

/// Accepted route for one guest composer submission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestInputDisposition {
	/// Execute through the guest-local command registry.
	LocalCommand,
	/// Send an authenticated prompt request to the host.
	RemotePrompt,
}

/// Guest composer input rejected before local or remote dispatch.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum GuestInputError {
	/// This slash command belongs to the authoritative host session.
	#[error("command is unavailable while joined to a collaboration")]
	HostCommand,
	/// Viewer credentials cannot prompt or mutate the host.
	#[error("this collaboration link is read-only")]
	ReadOnly,
}
/// Host activity edge applied to the guest's local spinner and activity meter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestActivityTransition {
	/// The host became active; start the local activity meter and loader.
	Started,
	/// The host became idle; stop the local activity meter and every transient
	/// loader.
	Stopped,
	/// The host activity state did not change.
	Unchanged,
}

/// Canonical guest footer facts projected from the latest host state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuestFooterFacts {
	/// Number of participants including the host and this guest.
	pub participants:    usize,
	/// Number of prompts queued on the host.
	pub queued_messages: u32,
	/// Whether the host is currently aborting a turn.
	pub aborting:        bool,
}

/// Effects a UI owner applies after one state update.
#[derive(Clone, Debug, PartialEq)]
pub struct GuestStateEffects {
	/// Activity edge for loader/meter reconciliation.
	pub activity:       GuestActivityTransition,
	/// Host-authored terminal title; this never changes the guest cwd.
	pub terminal_title: Str,
	/// Footer facts for the collaboration status segment.
	pub footer:         GuestFooterFacts,
	/// Provider-real context estimate reported by the host.
	pub context:        Option<ContextUsage>,
}

/// Guest-local mirror of host presentation state and visible agents.
///
/// The mirror deliberately contains no relay credentials, host filesystem
/// authority, agent paths, model credentials, or advisor rows.
#[derive(Default)]
pub struct GuestStateMirror {
	state:          Option<SessionStateUpdate>,
	models:         BTreeMap<Str, ModelMetadata>,
	agents:         BTreeMap<Str, AgentSummary>,
	host_streaming: bool,
}

impl GuestStateMirror {
	/// Applies one authoritative host-state snapshot and returns UI effects.
	pub fn apply_state(&mut self, mut state: SessionStateUpdate) -> GuestStateEffects {
		if let Some(context) = state.context_usage.as_mut() {
			context.percent = if context.context_window == 0 {
				0.0
			} else {
				(context.tokens as f64 * 100.0 / context.context_window as f64) as f32
			};
		}
		if let Some(model) = state.model.as_ref() {
			self.models.insert(Str::new(&model.id), model.clone());
		}
		let activity = match (self.host_streaming, state.is_streaming) {
			(false, true) => GuestActivityTransition::Started,
			(true, false) => GuestActivityTransition::Stopped,
			_ => GuestActivityTransition::Unchanged,
		};
		self.host_streaming = state.is_streaming;
		let terminal_title = if state.session_name.trim().is_empty() {
			sf!("OMP collaboration")
		} else {
			Str::new(state.session_name.trim())
		};
		let footer = GuestFooterFacts {
			participants:    state.participants.len().max(1),
			queued_messages: state.queued_message_count,
			aborting:        state.is_aborting,
		};
		let context = state.context_usage.clone();
		self.state = Some(state);
		GuestStateEffects { activity, terminal_title, footer, context }
	}

	/// Replaces the visible subagent registry with the host snapshot.
	pub fn apply_registry(&mut self, snapshot: RegistrySnapshot) {
		self.agents.clear();
		for agent in snapshot.agents {
			if agent_summary::Kind::try_from(agent.kind).is_ok() {
				self.agents.insert(Str::new(&agent.id), agent);
			}
		}
	}

	/// Returns the latest host state.
	#[must_use]
	pub const fn state(&self) -> Option<&SessionStateUpdate> {
		self.state.as_ref()
	}

	/// Returns the effective reasoning effort reported by the host.
	#[must_use]
	pub fn reasoning_effort(&self) -> Option<&str> {
		self.state.as_ref()?.thinking_level.as_deref()
	}

	/// Iterates the model catalog learned from host state updates.
	pub fn models(&self) -> impl ExactSizeIterator<Item = &ModelMetadata> + DoubleEndedIterator {
		self.models.values()
	}

	/// Iterates the latest visible agent registry mirror.
	pub fn agents(&self) -> impl ExactSizeIterator<Item = &AgentSummary> + DoubleEndedIterator {
		self.agents.values()
	}
}

/// Guest UI presentation hook implemented by the interactive app boundary.
pub trait GuestUiHooks {
	/// Presents one host-owned select dialog.
	fn present_select(
		&mut self,
		request_id: u32,
		title: &str,
		spec: &omp_proto::collab::v1::SelectSpec,
	);
	/// Presents one host-owned editor dialog.
	fn present_editor(
		&mut self,
		request_id: u32,
		title: &str,
		spec: &omp_proto::collab::v1::EditorSpec,
	);
	/// Dismisses a presented dialog without answering the host.
	fn dismiss(&mut self, request_id: u32);
}

/// Ordered guest dialog owner. Resync and leave dismiss newest-first.
#[derive(Default)]
pub struct GuestUiRequests {
	pending: BTreeMap<u32, UiRequest>,
	order:   Vec<u32>,
}

impl GuestUiRequests {
	/// Presents a valid host request through the matching UI hook.
	pub fn present(
		&mut self,
		request: UiRequest,
		hooks: &mut impl GuestUiHooks,
	) -> Result<(), GuestUiError> {
		let spec = request.spec.as_ref().ok_or(GuestUiError::MissingSpec)?;
		if self.pending.contains_key(&request.request_id) {
			hooks.dismiss(request.request_id);
			self.order.retain(|id| *id != request.request_id);
		}
		match spec {
			ui_request::Spec::Select(spec) => {
				hooks.present_select(request.request_id, &request.title, spec);
			},
			ui_request::Spec::Editor(spec) => {
				hooks.present_editor(request.request_id, &request.title, spec);
			},
		}
		self.order.push(request.request_id);
		self.pending.insert(request.request_id, request);
		Ok(())
	}

	/// Dismisses one request after `ui_request_end`.
	pub fn end(&mut self, request_id: u32, hooks: &mut impl GuestUiHooks) -> bool {
		let existed = self.pending.remove(&request_id).is_some();
		if existed {
			self.order.retain(|id| *id != request_id);
			hooks.dismiss(request_id);
		}
		existed
	}

	/// Dismisses all requests in reverse presentation order on resync or leave.
	pub fn dismiss_all(&mut self, hooks: &mut impl GuestUiHooks) {
		for request_id in self.order.drain(..).rev() {
			hooks.dismiss(request_id);
		}
		self.pending.clear();
	}

	/// Returns whether a request is still presented.
	#[must_use]
	pub fn contains(&self, request_id: u32) -> bool {
		self.pending.contains_key(&request_id)
	}
}

/// Local session destination restored after leaving a replica.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LocalSessionRestore {
	/// Resume the exact prior local transcript.
	Saved(PathBuf),
	/// Return to a fresh unsaved local session.
	Unsaved,
}

/// Exactly-once guest session restoration owner.
#[derive(Default)]
pub struct GuestSessionRestore {
	return_to: Option<LocalSessionRestore>,
	active:    bool,
}

impl GuestSessionRestore {
	/// Captures the local session before switching to the remote replica.
	pub fn begin(&mut self, session_file: Option<&Path>) {
		self.return_to = Some(session_file.map_or(LocalSessionRestore::Unsaved, |path| {
			LocalSessionRestore::Saved(path.to_path_buf())
		}));
		self.active = true;
	}

	/// Restores after intentional leave or a terminal disconnect.
	///
	/// A reconnecting relay does not call this method; callers invoke it only
	/// after reconnect is exhausted.
	pub fn take(&mut self) -> Option<LocalSessionRestore> {
		if !self.active {
			return None;
		}
		self.active = false;
		self.return_to.take()
	}
}

/// Guest dialog protocol failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum GuestUiError {
	/// A request had neither a select nor editor specification.
	#[error("collaboration UI request has no presentation specification")]
	MissingSpec,
}

/// Maximum physical records retained in one in-flight snapshot accumulator.
pub const SNAPSHOT_RECORD_MAX: usize = 1_000_000;

struct PendingSnapshot {
	fence:            ReplicaFence,
	expected_records: usize,
	records:          Vec<RemoteRecord>,
}

/// Guest-owned collaboration replica that survives reconnect reseeds in place.
pub struct GuestReplica {
	replica: Replica,
	active:  ReplicaFence,
	pending: Option<PendingSnapshot>,
	ready:   bool,
}

impl GuestReplica {
	/// Creates an empty replica using only guest-local cwd and secret-free host
	/// provenance.
	pub fn create(
		path: &Path,
		id: SessionId,
		created: u64,
		local_cwd: PathBuf,
		remote: RemoteProvenance,
	) -> Result<Self, GuestReplicaError> {
		let mut replica = Replica::create(path, id, created, local_cwd, remote)?;
		let active = replica.begin_reseed();
		Ok(Self { replica, active, pending: None, ready: false })
	}

	/// Reopens a replica without adopting host cwd or credentials.
	pub fn open(path: &Path, expected_room_id: &str) -> Result<Self, GuestReplicaError> {
		let mut replica = Replica::open(path, expected_room_id)?;
		let active = replica.begin_reseed();
		Ok(Self { replica, active, pending: None, ready: false })
	}

	/// Returns the durable replica.
	#[must_use]
	pub const fn replica(&self) -> &Replica {
		&self.replica
	}

	/// Starts a reconnect or initial snapshot, fencing all older live frames.
	pub fn begin_snapshot(&mut self, expected_records: usize) -> Result<(), GuestReplicaError> {
		if expected_records > SNAPSHOT_RECORD_MAX {
			return Err(GuestReplicaError::SnapshotTooLarge {
				actual:  expected_records,
				maximum: SNAPSHOT_RECORD_MAX,
			});
		}
		let fence = self.replica.begin_reseed();
		self.ready = false;
		self.active = fence;
		self.pending = Some(PendingSnapshot {
			fence,
			expected_records,
			records: Vec::with_capacity(expected_records.min(4096)),
		});
		Ok(())
	}

	/// Applies one ordered snapshot chunk and atomically publishes on `final`.
	///
	/// Returns `true` only after the final chunk is durably reseeded.
	pub fn push_snapshot_chunk(&mut self, chunk: SnapshotChunk) -> Result<bool, GuestReplicaError> {
		let pending = self
			.pending
			.as_mut()
			.ok_or(GuestReplicaError::OrphanSnapshotChunk)?;
		if pending.records.len().saturating_add(chunk.entries.len()) > pending.expected_records
			|| pending.records.len().saturating_add(chunk.entries.len()) > SNAPSHOT_RECORD_MAX
		{
			return Err(GuestReplicaError::SnapshotEntryOverflow {
				expected: pending.expected_records,
			});
		}
		for record in chunk.entries {
			pending.records.push(convert_record(record)?);
		}
		if !chunk.r#final {
			return Ok(false);
		}
		if pending.records.len() != pending.expected_records {
			return Err(GuestReplicaError::SnapshotCountMismatch {
				expected: pending.expected_records,
				actual:   pending.records.len(),
			});
		}
		let pending = self.pending.take().expect("pending snapshot checked above");
		self
			.replica
			.commit_reseed(pending.fence, chunk.host_revision_watermark, &pending.records)?;
		self.ready = true;
		Ok(true)
	}

	/// Appends one live record only for the post-snapshot active generation.
	pub fn append_live(&mut self, record: JournalRecord) -> Result<u64, GuestReplicaError> {
		if self.pending.is_some() {
			return Err(GuestReplicaError::SnapshotInProgress);
		}
		if !self.ready {
			return Err(GuestReplicaError::SnapshotRequired);
		}
		let record = convert_record(record)?;
		Ok(self.replica.append_live(self.active, &record)?)
	}
}

/// Guest replica protocol or storage failure.
#[derive(Debug, Error)]
pub enum GuestReplicaError {
	/// Durable replica operation failed.
	#[error(transparent)]
	Replica(#[from] ReplicaError),
	/// Welcome advertised an unreasonable physical record count.
	#[error("collaboration snapshot has {actual} records; maximum is {maximum}")]
	SnapshotTooLarge {
		/// Advertised count.
		actual:  usize,
		/// Hard accumulator ceiling.
		maximum: usize,
	},
	/// A chunk arrived before a welcome began a snapshot.
	#[error("collaboration snapshot chunk arrived without an active snapshot")]
	OrphanSnapshotChunk,
	/// Chunks exceeded the welcome's exact entry count.
	#[error("collaboration snapshot exceeded its expected {expected} records")]
	SnapshotEntryOverflow {
		/// Welcome-advertised count.
		expected: usize,
	},
	/// A final chunk arrived before all advertised physical records.
	#[error("collaboration snapshot expected {expected} records, received {actual}")]
	SnapshotCountMismatch {
		/// Welcome-advertised count.
		expected: usize,
		/// Received count.
		actual:   usize,
	},
	/// Live traffic arrived before the active snapshot was durably published.
	#[error("collaboration live record arrived while a snapshot is in progress")]
	SnapshotInProgress,
	/// Live traffic arrived before any complete snapshot established the fence.
	#[error("collaboration live record arrived before a complete snapshot")]
	SnapshotRequired,
	/// A journal record carried an unknown visibility value.
	#[error("collaboration journal record has an unknown visibility class")]
	UnknownVisibility,
}

fn convert_record(record: JournalRecord) -> Result<RemoteRecord, GuestReplicaError> {
	let visibility = match VisibilityClass::try_from(record.visibility_class) {
		Ok(VisibilityClass::PublicTranscript) => ReplicaVisibility::PublicTranscript,
		Ok(VisibilityClass::PublicPresentation) => ReplicaVisibility::PublicPresentation,
		Ok(VisibilityClass::HostLocalOmitted) => ReplicaVisibility::HostLocalOmitted,
		Ok(VisibilityClass::Unspecified) | Err(_) => {
			return Err(GuestReplicaError::UnknownVisibility);
		},
	};
	Ok(RemoteRecord { revision: record.revision, visibility, json: record.transcript_v4_json })
}
#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn guest_command_filter_is_closed() {
		assert!(guest_command_allowed("/export transcript.html"));
		assert!(guest_command_allowed(" /leave "));
		assert!(!guest_command_allowed("/model anthropic/example"));
		assert!(!guest_command_allowed("/agents"));
	}

	#[test]
	fn read_only_gate_precedes_remote_prompt_send() {
		assert_eq!(admit_guest_input("hello", true), Err(GuestInputError::ReadOnly));
		assert_eq!(admit_guest_input("/help", true), Ok(GuestInputDisposition::LocalCommand),);
		assert_eq!(admit_guest_input("hello", false), Ok(GuestInputDisposition::RemotePrompt),);
	}
	#[test]
	fn state_mirror_reconciles_activity_catalog_registry_and_context() {
		let mut mirror = GuestStateMirror::default();
		let effects = mirror.apply_state(SessionStateUpdate {
			is_streaming: true,
			queued_message_count: 2,
			session_name: "remote".to_owned(),
			model: Some(ModelMetadata {
				id:             "model-1".to_owned(),
				name:           "Model".to_owned(),
				provider:       "provider".to_owned(),
				context_window: 100,
			}),
			thinking_level: Some("high".to_owned()),
			context_usage: Some(ContextUsage {
				tokens:         25,
				context_window: 100,
				percent:        0.0,
			}),
			participants: vec![Default::default(), Default::default()],
			..SessionStateUpdate::default()
		});
		assert_eq!(effects.activity, GuestActivityTransition::Started);
		assert_eq!(effects.footer.participants, 2);
		assert_eq!(effects.context.expect("context").percent, 25.0);
		assert_eq!(mirror.reasoning_effort(), Some("high"));
		assert_eq!(mirror.models().len(), 1);

		mirror.apply_registry(RegistrySnapshot {
			agents: vec![AgentSummary { id: "agent-1".to_owned(), ..AgentSummary::default() }],
		});
		assert_eq!(mirror.agents().len(), 1);
		let effects =
			mirror.apply_state(SessionStateUpdate { is_streaming: false, ..Default::default() });
		assert_eq!(effects.activity, GuestActivityTransition::Stopped);
	}

	#[test]
	fn local_session_restore_is_exactly_once() {
		let mut restore = GuestSessionRestore::default();
		restore.begin(Some(Path::new("/tmp/session.jsonl")));
		assert_eq!(
			restore.take(),
			Some(LocalSessionRestore::Saved(PathBuf::from("/tmp/session.jsonl")))
		);
		assert_eq!(restore.take(), None);
	}
}
