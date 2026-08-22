//! Guest-side snapshot reseed and live transcript-v4 append fencing.

use std::path::{Path, PathBuf};

use omp_proto::collab::v1::{JournalRecord, SnapshotChunk, VisibilityClass};
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
}
