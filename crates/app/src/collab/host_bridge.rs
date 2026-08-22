//! Journal-owner bridge for bounded collaboration snapshot and live
//! replication.

use bytes::Bytes;
use omp_agent::{
	Journal, JournalError, ReplicationEvent, ReplicationRecord, ReplicationSubscription,
	ReplicationTerminal, ReplicationVisibility,
};
use omp_collab::{
	codec::REPEATED_MAX_COUNT,
	replication::{MAX_REPLICATED_PAYLOAD_BYTES, ReplicationError, shrink_for_replication},
};
use omp_proto::collab::v1::{JournalRecord, SnapshotChunk, VisibilityClass};
use prost::Message as _;
use thiserror::Error;

/// Soft protobuf size target for each initial snapshot frame.
pub const SNAPSHOT_CHUNK_SOFT_BYTES: usize = 512 * 1024;

/// Catch-up snapshot plus ordered live records from the authoritative journal.
pub struct HostJournalBridge {
	subscription: ReplicationSubscription,
}

impl HostJournalBridge {
	/// Captures a race-free catch-up and registers bounded live delivery.
	pub fn subscribe(journal: &mut Journal) -> Result<Self, HostBridgeError> {
		Ok(Self { subscription: journal.subscribe_replication()? })
	}

	/// Builds ordered soft-bounded snapshot chunks, always ending with `final`.
	pub fn snapshot_chunks(&mut self) -> Result<Vec<SnapshotChunk>, HostBridgeError> {
		let host_revision_watermark = self.subscription.host_revision();
		let mut chunks = Vec::new();
		let mut entries = Vec::new();
		let mut encoded_bytes = 0_usize;
		while let Some(record) = self.subscription.next_catch_up() {
			let record = wire_record(record)?;
			let record_bytes = record.encoded_len().saturating_add(10);
			if !entries.is_empty()
				&& (entries.len() >= REPEATED_MAX_COUNT
					|| encoded_bytes.saturating_add(record_bytes) > SNAPSHOT_CHUNK_SOFT_BYTES)
			{
				chunks.push(SnapshotChunk {
					entries: std::mem::take(&mut entries),
					r#final: false,
					host_revision_watermark,
				});
				encoded_bytes = 0;
			}
			encoded_bytes = encoded_bytes.saturating_add(record_bytes);
			entries.push(record);
		}
		chunks.push(SnapshotChunk { entries, r#final: true, host_revision_watermark });
		Ok(chunks)
	}

	/// Receives the next live record or explicit bounded-lag terminal.
	pub async fn recv(&self) -> Result<HostReplicationEvent, HostBridgeError> {
		match self.subscription.recv().await? {
			ReplicationEvent::Record(record) => Ok(HostReplicationEvent::Record(wire_record(record)?)),
			ReplicationEvent::Terminal(terminal) => Ok(HostReplicationEvent::Terminal(terminal)),
		}
	}
}

/// Live host bridge delivery.
#[derive(Clone, Debug)]
pub enum HostReplicationEvent {
	/// One ordered committed transcript-v4 record.
	Record(JournalRecord),
	/// Bounded lag or journal-owner closure.
	Terminal(ReplicationTerminal),
}

/// Host journal bridge failure.
#[derive(Debug, Error)]
pub enum HostBridgeError {
	/// Journal subscription failed.
	#[error(transparent)]
	Journal(#[from] JournalError),
	/// The live journal owner closed its channel.
	#[error("journal replication subscription closed without a terminal event")]
	Closed(#[from] flume::RecvError),
	/// A public transcript record was not valid JSON.
	#[error("public transcript-v4 record is not valid JSON")]
	Json(#[source] serde_json::Error),
	/// Deterministic per-entry shrinking could not fit the hard frame ceiling.
	#[error(transparent)]
	Shrink(#[from] ReplicationError),
}

fn wire_record(record: ReplicationRecord) -> Result<JournalRecord, HostBridgeError> {
	let visibility_class = match record.visibility {
		ReplicationVisibility::PublicTranscript => VisibilityClass::PublicTranscript,
		ReplicationVisibility::HostLocalOmitted => VisibilityClass::HostLocalOmitted,
	} as i32;
	let transcript_v4_json = if record.json.len() <= MAX_REPLICATED_PAYLOAD_BYTES {
		record.json
	} else {
		let value = serde_json::from_slice(&record.json).map_err(HostBridgeError::Json)?;
		Bytes::from(shrink_for_replication(&value)?.encode()?)
	};
	Ok(JournalRecord { revision: record.revision, transcript_v4_json, visibility_class })
}
