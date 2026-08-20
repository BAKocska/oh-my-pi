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
	transcript::InvocationTransition,
};
use serde_json::value::RawValue;
use thiserror::Error;

use crate::{
	journal::{
		Journal, JournalCustomEntry, JournalError, JournalQuery, JournalReply, JournalRequest,
		SessionStateValue, SessionStateWatchEvent,
	},
	journal_kinds::EntryKindDecl,
};

/// A cloneable sender for authenticated extension CONTROL operations.
#[derive(Clone)]
pub struct ControlSender {
	commands:     flume::Sender<ControlCommand>,
	next_receipt: Arc<AtomicU64>,
}

/// The receive half retained by the sole mutable journal owner.
pub struct ControlMailbox {
	commands: flume::Receiver<ControlCommand>,
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
}

/// Authoritative acknowledgement that a rewind entered the boundary queue.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewindAck {
	/// Requested live journal head.
	pub target:  u64,
	/// Agent-issued command identifier.
	pub receipt: Str,
}

/// A rewind command surfaced to the agent loop for boundary execution.
pub struct ScheduledRewind {
	/// Durable journal event index to rewind to.
	pub target: u64,
	/// Caller-declared rewind scope label.
	pub scope:  Str,
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
#[must_use]
pub fn channel() -> (ControlSender, ControlMailbox) {
	let (commands, receiver) = flume::unbounded();
	(ControlSender { commands, next_receipt: Arc::new(AtomicU64::new(1)) }, ControlMailbox {
		commands: receiver,
	})
}

impl ControlSender {
	/// Appends a Core-authored labeled checkpoint and returns its durable token.
	///
	/// # Errors
	pub async fn checkpoint(&self, label: Str) -> Result<u64, ControlError> {
		let sequence = self.next_receipt.fetch_add(1, Ordering::Relaxed);
		let request_id = sf!("checkpoint-{sequence}");
		let (reply, response) = flume::bounded(1);
		self
			.commands
			.send(ControlCommand::Checkpoint { label, request_id, reply })
			.map_err(|_| ControlError::Closed)?;
		response
			.recv_async()
			.await
			.map_err(|_| ControlError::Closed)?
			.map_err(ControlError::from)
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
	pub async fn schedule_rewind(&self, target: u64, scope: Str) -> Result<RewindAck, ControlError> {
		let sequence = self.next_receipt.fetch_add(1, Ordering::Relaxed);
		let receipt = sf!("rewind-{sequence}");
		let (ack, response) = flume::bounded(1);
		self
			.commands
			.send(ControlCommand::Rewind { target, scope, receipt: receipt.clone(), ack })
			.map_err(|_| ControlError::Closed)?;
		response
			.recv_async()
			.await
			.map_err(|_| ControlError::Closed)
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
		handle_command(journal, command)
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
			if let ControlMailboxEvent::Rewind(rewind) = handle_command(journal, command) {
				surfaced.push_back(rewind);
			}
			handled += 1;
		}
		handled
	}
}

pub(crate) enum ControlCommand {
	Checkpoint {
		label:      Str,
		request_id: Str,
		reply:      flume::Sender<JournalReplyResult<u64>>,
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
		target:  u64,
		scope:   Str,
		receipt: Str,
		ack:     flume::Sender<RewindAck>,
	},
}

fn handle_command(journal: &mut Journal, command: ControlCommand) -> ControlMailboxEvent {
	match command {
		ControlCommand::Checkpoint { label, request_id, reply } => {
			let _ = reply.send(journal.checkpoint(crate::r#loop::now_ms(), label, request_id));
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
		ControlCommand::Rewind { target, scope, receipt, ack } => {
			let _ = ack.send(RewindAck { target, receipt });
			return ControlMailboxEvent::Rewind(ScheduledRewind { target, scope });
		},
	}
	ControlMailboxEvent::JournalHandled
}
