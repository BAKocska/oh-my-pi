//! Serialized extension CONTROL routing into the session journal owner.
//!
//! The mailbox is deliberately receiver-owned rather than spawning a second
//! journal task. The agent loop remains the sole mutable [`Journal`] owner and
//! drains these commands at its established mailbox points; one command is
//! fully handled before another callback may enter.

use std::{
	collections::{BTreeMap, VecDeque},
	sync::{
		Arc,
		atomic::{AtomicU64, Ordering},
	},
};

use omp_core::{Str, sf};
use omp_storage::{
	state::{DurableRequest, StateAuthority, StateRevision},
	transcript::{InvocationTransition, ModelChange, TitleSource},
};
use parking_lot::Mutex;
use serde_json::value::RawValue;
use thiserror::Error;

use crate::{
	journal::{
		Journal, JournalCustomEntry, JournalError, JournalQuery, JournalReply, JournalRequest,
		SessionStateValue, SessionStateWatchEvent,
	},
	journal_kinds::EntryKindDecl,
	r#loop::{ActiveCheckpoint, CheckpointState},
};

/// A cloneable sender for authenticated extension CONTROL operations.
#[derive(Clone)]
pub struct ControlSender {
	commands:         flume::Sender<ControlCommand>,
	next_receipt:     Arc<AtomicU64>,
	checkpoint_state: Arc<Mutex<CheckpointState>>,
}

/// The receive half retained by the sole mutable journal owner.
pub struct ControlMailbox {
	commands:         flume::Receiver<ControlCommand>,
	checkpoint_state: Arc<Mutex<CheckpointState>>,
}

/// Failure to deliver or execute a journal-owner CONTROL operation.
#[derive(Debug, Error)]
pub enum ControlError {
	/// The sole journal owner has stopped receiving commands.
	#[error("agent CONTROL journal owner is unavailable")]
	Closed,
	/// The journal rejected the authenticated operation.
	#[error(transparent)]
	Journal(#[from] JournalError),
	/// A second checkpoint was requested before the active one settled.
	#[error("checkpoint already active")]
	CheckpointAlreadyActive,
	/// Rewind was requested before any checkpoint was created.
	#[error("no active checkpoint")]
	NoActiveCheckpoint,
	/// Rewind was repeated after the active checkpoint completed.
	#[error("checkpoint already completed; continue from the retained rewind report")]
	CheckpointAlreadyCompleted,
	/// The opaque token was not issued by this active session.
	#[error("checkpoint token does not belong to the active session")]
	WrongCheckpointToken,
	/// A rewind report contained no findings.
	#[error("rewind report must not be empty")]
	EmptyRewindReport,
	/// A rewind for the active checkpoint is already queued.
	#[error("rewind already scheduled for the active checkpoint")]
	RewindAlreadyScheduled,
}

/// Authoritative acknowledgement that a checkpoint became active.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckpointAck {
	/// Opaque session-owned checkpoint token.
	pub token:      Str,
	/// Checkpoint creation time in epoch milliseconds.
	pub started_at: u64,
}

/// Authoritative acknowledgement that a rewind entered the boundary queue.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewindAck {
	/// Opaque token accepted for the queued rewind.
	pub token:   Str,
	/// Agent-issued command identifier.
	pub receipt: Str,
}

/// A rewind command surfaced to the agent loop for boundary execution.
pub struct ScheduledRewind {
	/// Opaque session-owned checkpoint token.
	pub token:      Str,
	/// Durable journal event index resolved from the active checkpoint.
	pub target:     u64,
	/// Findings retained after discarded exploration.
	pub report:     Str,
	/// Exploration goal retained for recovery guidance.
	pub goal:       Str,
	/// Checkpoint creation time in epoch milliseconds.
	pub started_at: u64,
}

/// Result of receiving one typed CONTROL command.
///
/// Journal-owner harnesses outside the agent loop drive
/// [`ControlMailbox::handle_next`] and match on this; loop-scoped rewinds must
/// be executed (or refused) by whoever owns full agent state.
pub enum ControlMailboxEvent {
	/// Every sender has closed.
	Closed,
	/// A journal-scoped command completed on the journal owner.
	JournalHandled,
	/// A loop-scoped rewind is ready for boundary execution.
	Rewind(ScheduledRewind),
}

type JournalReplyResult<T> = Result<T, JournalError>;

/// Creates the extension CONTROL mailbox pair.
///
/// The channel is unbounded because every durable request already has a bounded
/// protobuf frame and backpressure happens at the worker request correlation
/// slot. The receiver must stay with the sole [`Journal`] owner.
pub fn channel() -> (ControlSender, ControlMailbox) {
	let (commands, receiver) = flume::unbounded();
	let checkpoint_state = Arc::new(Mutex::new(CheckpointState::default()));
	(
		ControlSender {
			commands,
			next_receipt: Arc::new(AtomicU64::new(1)),
			checkpoint_state: Arc::clone(&checkpoint_state),
		},
		ControlMailbox { commands: receiver, checkpoint_state },
	)
}

impl ControlSender {
	pub(crate) fn checkpoint_state(&self) -> Arc<Mutex<CheckpointState>> {
		Arc::clone(&self.checkpoint_state)
	}

	/// Appends an in-place reset boundary through the sole journal owner.
	///
	/// # Errors
	/// Returns a closed-owner or typed journal transition failure.
	pub async fn reset(&self, ts: u64) -> Result<u64, ControlError> {
		let (reply, response) = flume::bounded(1);
		self
			.commands
			.send(ControlCommand::Reset { ts, reply })
			.map_err(|_| ControlError::Closed)?;
		response
			.recv_async()
			.await
			.map_err(|_| ControlError::Closed)?
			.map_err(ControlError::from)
	}

	/// Appends a provider-reset hint through the sole journal owner.
	///
	/// # Errors
	/// Returns a closed-owner or typed journal transition failure.
	pub async fn provider_reset(&self, ts: u64) -> Result<u64, ControlError> {
		let (reply, response) = flume::bounded(1);
		self
			.commands
			.send(ControlCommand::ProviderReset { ts, reply })
			.map_err(|_| ControlError::Closed)?;
		response
			.recv_async()
			.await
			.map_err(|_| ControlError::Closed)?
			.map_err(ControlError::from)
	}

	/// Appends a user-assigned durable session title through the journal owner.
	///
	/// # Errors
	/// Returns a closed-owner or typed journal transition failure.
	pub async fn set_title(&self, ts: u64, title: Str) -> Result<u64, ControlError> {
		let (reply, response) = flume::bounded(1);
		self
			.commands
			.send(ControlCommand::SetTitle { ts, title, reply })
			.map_err(|_| ControlError::Closed)?;
		response
			.recv_async()
			.await
			.map_err(|_| ControlError::Closed)?
			.map_err(ControlError::from)
	}

	/// Appends a session-only effective model override through the journal
	/// owner.
	///
	/// # Errors
	/// Returns a closed-owner or typed journal transition failure.
	pub async fn model_override(&self, ts: u64, model: ModelChange) -> Result<u64, ControlError> {
		let (reply, response) = flume::bounded(1);
		self
			.commands
			.send(ControlCommand::ModelOverride { ts, model, reply })
			.map_err(|_| ControlError::Closed)?;
		response
			.recv_async()
			.await
			.map_err(|_| ControlError::Closed)?
			.map_err(ControlError::from)
	}

	/// Appends a Core-authored exploration checkpoint and returns its opaque
	/// session token.
	///
	/// # Errors
	pub async fn checkpoint(&self, goal: Str) -> Result<CheckpointAck, ControlError> {
		let (reply, response) = flume::bounded(1);
		self
			.commands
			.send(ControlCommand::Checkpoint { goal, reply })
			.map_err(|_| ControlError::Closed)?;
		response
			.recv_async()
			.await
			.map_err(|_| ControlError::Closed)?
	}

	/// Requests one authenticated journal operation and awaits its assigned
	/// indexes.
	///
	/// # Errors
	/// Returns [`ControlError::Closed`] if the journal owner stopped, or the
	/// journal's typed failure after it handled the request.
	pub async fn journal(&self, request: JournalRequest) -> Result<JournalReply, ControlError> {
		let (reply, response) = flume::bounded(1);
		self
			.commands
			.send(ControlCommand::Journal { request, reply })
			.map_err(|_| ControlError::Closed)?;
		response
			.recv_async()
			.await
			.map_err(|_| ControlError::Closed)?
			.map_err(ControlError::from)
	}

	/// Atomically declares one authenticated extension's complete entry-kind
	/// set.
	///
	/// # Errors
	/// Returns a closed-owner or typed journal declaration failure.
	pub async fn declare_entry_kinds(
		&self,
		extension: Str,
		declarations: Vec<EntryKindDecl>,
	) -> Result<(), ControlError> {
		let (reply, response) = flume::bounded(1);
		self
			.commands
			.send(ControlCommand::DeclareEntryKinds { extension, declarations, reply })
			.map_err(|_| ControlError::Closed)?;
		response
			.recv_async()
			.await
			.map_err(|_| ControlError::Closed)?
			.map_err(ControlError::from)
	}

	/// Runs authenticated, namespace-scoped custom-entry queries on the sole
	/// journal owner and returns rows in ascending physical-index order.
	///
	/// # Errors
	/// Returns a closed-owner or typed journal query failure.
	pub async fn query(
		&self,
		queries: Vec<JournalQuery>,
	) -> Result<Vec<JournalCustomEntry>, ControlError> {
		let (reply, response) = flume::bounded(1);
		self
			.commands
			.send(ControlCommand::Query { queries, reply })
			.map_err(|_| ControlError::Closed)?;
		response
			.recv_async()
			.await
			.map_err(|_| ControlError::Closed)?
			.map_err(ControlError::from)
	}

	/// Reads the latest live SESSION-scoped value from the canonical journal.
	///
	/// # Errors
	/// Returns a closed-owner or typed journal authority/query failure.
	pub async fn session_state_get(
		&self,
		authority: StateAuthority,
		key: Str,
	) -> Result<Option<SessionStateValue>, ControlError> {
		let (reply, response) = flume::bounded(1);
		self
			.commands
			.send(ControlCommand::SessionStateGet { authority, key, reply })
			.map_err(|_| ControlError::Closed)?;
		response
			.recv_async()
			.await
			.map_err(|_| ControlError::Closed)?
			.map_err(ControlError::from)
	}

	/// Atomically compares and replaces one SESSION-scoped value.
	///
	/// # Errors
	/// Returns a closed-owner, stale revision, authority, or journal failure.
	pub async fn session_state_compare_exchange(
		&self,
		ts: u64,
		authority: StateAuthority,
		key: Str,
		expected: Option<StateRevision>,
		value: Box<RawValue>,
		request: DurableRequest,
	) -> Result<SessionStateValue, ControlError> {
		let (reply, response) = flume::bounded(1);
		self
			.commands
			.send(ControlCommand::SessionStateCompareExchange {
				ts,
				authority,
				key,
				expected,
				value,
				request,
				reply,
			})
			.map_err(|_| ControlError::Closed)?;
		response
			.recv_async()
			.await
			.map_err(|_| ControlError::Closed)?
			.map_err(ControlError::from)
	}

	/// Subscribes to ordered SESSION state changes without pinning a journal
	/// callback.
	///
	/// The bounded receiver includes catch-up values newer than `since` followed
	/// by durable live updates. Dropping it cancels the subscription; terminal
	/// events distinguish lag from journal shutdown.
	///
	/// # Errors
	/// Returns a closed-owner or typed journal authority/subscription failure.
	pub async fn session_state_watch(
		&self,
		authority: StateAuthority,
		key: Str,
		since: Option<StateRevision>,
	) -> Result<flume::Receiver<SessionStateWatchEvent>, ControlError> {
		let (reply, response) = flume::bounded(1);
		self
			.commands
			.send(ControlCommand::SessionStateWatch { authority, key, since, reply })
			.map_err(|_| ControlError::Closed)?;
		response
			.recv_async()
			.await
			.map_err(|_| ControlError::Closed)?
			.map_err(ControlError::from)
	}

	/// Persists one invocation-machine transition on the same owner as extension
	/// entries.
	///
	/// # Errors
	/// Returns a closed-owner or typed journal transition failure.
	pub async fn invocation_transition(
		&self,
		ts: u64,
		transition: InvocationTransition,
	) -> Result<u64, ControlError> {
		let (reply, response) = flume::bounded(1);
		self
			.commands
			.send(ControlCommand::InvocationTransition { ts, transition, reply })
			.map_err(|_| ControlError::Closed)?;
		response
			.recv_async()
			.await
			.map_err(|_| ControlError::Closed)?
			.map_err(ControlError::from)
	}

	/// Schedules a full agent rewind for the next turn boundary.
	///
	/// The acknowledgement means the sole owner accepted the command into its
	/// boundary queue; execution deliberately happens later, after any active
	/// tool batch settles.
	///
	/// # Errors
	/// Returns [`ControlError::Closed`] if the agent loop stopped receiving.
	pub async fn schedule_rewind(&self, token: Str, report: Str) -> Result<RewindAck, ControlError> {
		let sequence = self.next_receipt.fetch_add(1, Ordering::Relaxed);
		let receipt = sf!("rewind-{sequence}");
		let (ack, response) = flume::bounded(1);
		self
			.commands
			.send(ControlCommand::Rewind { token, report, receipt: receipt.clone(), ack })
			.map_err(|_| ControlError::Closed)?;
		response
			.recv_async()
			.await
			.map_err(|_| ControlError::Closed)?
	}
}

impl ControlMailbox {
	/// Handles the next typed command, waiting without holding a lock.
	///
	/// Journal commands complete immediately. Loop-scoped commands are surfaced
	/// to the caller, which must execute them at its documented boundary.
	pub async fn handle_next(&self, journal: &mut Journal) -> ControlMailboxEvent {
		let Ok(command) = self.commands.recv_async().await else {
			return ControlMailboxEvent::Closed;
		};
		handle_command(journal, command, &self.checkpoint_state)
	}

	/// Drains at most `limit` commands already waiting at an agent-loop mailbox
	/// point.
	///
	/// Journal commands retain their existing latency. Loop-scoped commands are
	/// appended to `surfaced` in receive order for later boundary execution.
	pub(crate) fn drain_ready(
		&self,
		journal: &mut Journal,
		limit: usize,
		surfaced: &mut VecDeque<ScheduledRewind>,
	) -> usize {
		let mut handled = 0;
		while handled < limit {
			let Ok(command) = self.commands.try_recv() else {
				break;
			};
			if let ControlMailboxEvent::Rewind(rewind) =
				handle_command(journal, command, &self.checkpoint_state)
			{
				surfaced.push_back(rewind);
			}
			handled += 1;
		}
		handled
	}
}

pub(crate) enum ControlCommand {
	Reset {
		ts:    u64,
		reply: flume::Sender<JournalReplyResult<u64>>,
	},
	ProviderReset {
		ts:    u64,
		reply: flume::Sender<JournalReplyResult<u64>>,
	},
	ModelOverride {
		ts:    u64,
		model: ModelChange,
		reply: flume::Sender<JournalReplyResult<u64>>,
	},
	SetTitle {
		ts:    u64,
		title: Str,
		reply: flume::Sender<JournalReplyResult<u64>>,
	},
	Checkpoint {
		goal:  Str,
		reply: flume::Sender<Result<CheckpointAck, ControlError>>,
	},
	Journal {
		request: JournalRequest,
		reply:   flume::Sender<JournalReplyResult<JournalReply>>,
	},
	DeclareEntryKinds {
		extension:    Str,
		declarations: Vec<EntryKindDecl>,
		reply:        flume::Sender<JournalReplyResult<()>>,
	},
	Query {
		queries: Vec<JournalQuery>,
		reply:   flume::Sender<JournalReplyResult<Vec<JournalCustomEntry>>>,
	},
	SessionStateGet {
		authority: StateAuthority,
		key:       Str,
		reply:     flume::Sender<JournalReplyResult<Option<SessionStateValue>>>,
	},
	SessionStateCompareExchange {
		ts:        u64,
		authority: StateAuthority,
		key:       Str,
		expected:  Option<StateRevision>,
		value:     Box<RawValue>,
		request:   DurableRequest,
		reply:     flume::Sender<JournalReplyResult<SessionStateValue>>,
	},
	SessionStateWatch {
		authority: StateAuthority,
		key:       Str,
		since:     Option<StateRevision>,
		reply:     flume::Sender<JournalReplyResult<flume::Receiver<SessionStateWatchEvent>>>,
	},
	InvocationTransition {
		ts:         u64,
		transition: InvocationTransition,
		reply:      flume::Sender<JournalReplyResult<u64>>,
	},
	Rewind {
		token:   Str,
		report:  Str,
		receipt: Str,
		ack:     flume::Sender<Result<RewindAck, ControlError>>,
	},
}

fn handle_command(
	journal: &mut Journal,
	command: ControlCommand,
	checkpoint_state: &Mutex<CheckpointState>,
) -> ControlMailboxEvent {
	match command {
		ControlCommand::Reset { ts, reply } => {
			let _ = reply.send(journal.reset(ts));
		},
		ControlCommand::ProviderReset { ts, reply } => {
			let _ = reply.send(journal.provider_reset(ts));
		},
		ControlCommand::ModelOverride { ts, model, reply } => {
			let _ = reply.send(journal.model_override(ts, model));
		},
		ControlCommand::SetTitle { ts, title, reply } => {
			let _ = reply.send(journal.append_title(ts, title, TitleSource::User));
		},
		ControlCommand::Checkpoint { goal, reply } => {
			let mut state = checkpoint_state.lock();
			if state.active.is_some() {
				let _ = reply.send(Err(ControlError::CheckpointAlreadyActive));
			} else {
				let token = Str::from(omp_core::Ulid::generate().to_string());
				let started_at = crate::r#loop::now_ms();
				match journal.checkpoint(started_at, token.as_str(), goal.as_str(), started_at) {
					Ok(event) => {
						state.active = Some(ActiveCheckpoint {
							opaque_token: token.clone(),
							event,
							goal,
							started_at,
						});
						state.rewind_scheduled = false;
						let _ = reply.send(Ok(CheckpointAck { token, started_at }));
					},
					Err(error) => {
						let _ = reply.send(Err(ControlError::Journal(error)));
					},
				}
			}
		},
		ControlCommand::Journal { request, reply } => {
			let _ = reply.send(journal.handle_request(request));
		},
		ControlCommand::DeclareEntryKinds { extension, declarations, reply } => {
			let _ = reply.send(journal.declare_entry_kinds(extension.as_str(), declarations));
		},
		ControlCommand::Query { queries, reply } => {
			let mut rows = BTreeMap::new();
			let result = queries.into_iter().try_for_each(|query| {
				for row in journal.query_custom(&query)? {
					rows.insert(row.index, row);
				}
				Ok::<_, JournalError>(())
			});
			let _ = reply.send(result.map(|()| rows.into_values().collect()));
		},
		ControlCommand::SessionStateGet { authority, key, reply } => {
			let _ = reply.send(journal.latest_session_state(&authority, key.as_str()));
		},
		ControlCommand::SessionStateCompareExchange {
			ts,
			authority,
			key,
			expected,
			value,
			request,
			reply,
		} => {
			let result =
				journal.compare_exchange_session_state(ts, &authority, key, expected, value, &request);
			let _ = reply.send(result);
		},
		ControlCommand::SessionStateWatch { authority, key, since, reply } => {
			let _ = reply.send(journal.subscribe_session_state(&authority, key, since));
		},
		ControlCommand::InvocationTransition { ts, transition, reply } => {
			let _ = reply.send(journal.record_invocation_transition(ts, transition));
		},
		ControlCommand::Rewind { token, report, receipt, ack } => {
			let mut state = checkpoint_state.lock();
			let Some(active) = state.active.clone() else {
				let error = if state.last_completed.is_some() {
					ControlError::CheckpointAlreadyCompleted
				} else {
					ControlError::NoActiveCheckpoint
				};
				let _ = ack.send(Err(error));
				return ControlMailboxEvent::JournalHandled;
			};
			if token != active.opaque_token {
				let _ = ack.send(Err(ControlError::WrongCheckpointToken));
				return ControlMailboxEvent::JournalHandled;
			}
			let report = Str::new(report.trim());
			if report.is_empty() {
				let _ = ack.send(Err(ControlError::EmptyRewindReport));
				return ControlMailboxEvent::JournalHandled;
			}
			if state.rewind_scheduled {
				let _ = ack.send(Err(ControlError::RewindAlreadyScheduled));
				return ControlMailboxEvent::JournalHandled;
			}
			state.rewind_scheduled = true;
			let _ = ack.send(Ok(RewindAck { token: token.clone(), receipt }));
			return ControlMailboxEvent::Rewind(ScheduledRewind {
				token,
				target: active.event,
				report,
				goal: active.goal,
				started_at: active.started_at,
			});
		},
	}
	ControlMailboxEvent::JournalHandled
}
