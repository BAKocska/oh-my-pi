//! Transcript v4's append-only event journal.
//!
//! Line zero is the identity header and every later physical line is an event,
//! so an event index is always its line number minus one. Malformed lines
//! remain as tombstones rather than being dropped; otherwise every later
//! reference would shift. Corrections and navigation are later events, never
//! edits to old bytes. Replay capsules follow the complementary storage rule
//! that every byte exists in exactly one place: neutral data in blocks,
//! provider-only residue in capsules, and large payloads
//! in the content-addressed blob store.

pub mod block;
pub mod capsule;
pub mod codec;
#[cfg(test)]
mod codec_tests;
pub mod event;
pub mod import;
pub mod msg;
pub mod patch;
mod raweq;
pub mod reader;
pub mod replay;
pub mod replica;
pub mod types;
pub mod writer;

pub use block::{Block, BlockKind, Replay};
pub use codec::{Error, Header, read_header, read_line, write_header, write_line};
pub use event::{
	ApprovalDecided, ApprovalReason, ApprovalTicketFiled, ChildLifecycleEntry, ChildSessionInit,
	ChildWorkspaceIdentity, Custom, EntryUndecodable, Event, HookOutcome, ItemRecord, JobRegistered,
	JobSettled, Kind, PolicyDecision, PromptRewriteCommit, PromptRewriteIntent, PromptRewriteStage,
	SnapcompactArchive, SupersededCompaction, ToolBatchAuthorized, TurnAbort, TurnInputItem,
	TurnInputRecord, TurnOptionsRecord, TurnReceipt, TurnStart,
};
pub use import::{
	ForeignFormat, ImportDiagnostic, ImportedEntry, ImportedTranscript, parse_foreign_jsonl,
};
pub use msg::{
	Content, MAX_PERSISTED_CHARS, Msg, PERSISTENCE_TRUNCATION_NOTICE, UserBlock,
	truncate_persisted_text,
};
pub use patch::Patch;
pub use reader::{
	DiagnosticKind, Entry, LiveSet, Log, ReadCounters, ReadDiagnostic, Reader, RefreshReport,
	RefreshState, VisitReport, load, visit_batched,
};
pub use types::*;
pub use writer::Writer;
