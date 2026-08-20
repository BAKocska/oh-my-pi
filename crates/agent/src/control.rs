//! Serialized extension CONTROL routing into the session journal owner.
//!
//! The mailbox is deliberately receiver-owned rather than spawning a second
//! journal task. The agent loop remains the sole mutable [`Journal`] owner and
//! drains these commands at its established mailbox points; one command is
//! fully handled before another callback may enter.

use std::collections::BTreeMap;

use omp_core::Str;
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
	commands: flume::Sender<ControlCommand>,
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

/// Creates the extension CONTROL mailbox pair.
///
/// The channel is unbounded because every durable request already has a bounded
/// protobuf frame and backpressure happens at the worker request correlation
/// slot. The receiver must stay with the sole [`Journal`] owner.
#[must_use]
pub fn channel() -> (ControlSender, ControlMailbox) {
	let (commands, receiver) = flume::unbounded();
	(ControlSender { commands }, ControlMailbox { commands: receiver })
}

impl ControlSender {
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
}

impl ControlMailbox {
	/// Handles the next command, waiting without holding a lock.
	///
	/// Returns `false` after every sender has closed. Each callback is completed
	/// before this method receives another command.
	pub async fn handle_next(&self, journal: &mut Journal) -> bool {
		let Ok(command) = self.commands.recv_async().await else {
			return false;
		};
		handle_command(journal, command);
		true
	}

	/// Drains at most `limit` commands already waiting at an agent-loop mailbox
	/// point.
	///
	/// Returns the number of completely handled callbacks. A zero limit performs
	/// no work, and bounding the drain preserves fairness with turn traffic.
	pub fn drain_ready(&self, journal: &mut Journal, limit: usize) -> usize {
		let mut handled = 0;
		while handled < limit {
			let Ok(command) = self.commands.try_recv() else {
				break;
			};
			handle_command(journal, command);
			handled += 1;
		}
		handled
	}
}

enum ControlCommand {
	Journal {
		request: JournalRequest,
		reply:   flume::Sender<Result<JournalReply, JournalError>>,
	},
	DeclareEntryKinds {
		extension:    Str,
		declarations: Vec<EntryKindDecl>,
		reply:        flume::Sender<Result<(), JournalError>>,
	},
	Query {
		queries: Vec<JournalQuery>,
		reply:   flume::Sender<Result<Vec<JournalCustomEntry>, JournalError>>,
	},
	SessionStateGet {
		authority: StateAuthority,
		key:       Str,
		reply:     flume::Sender<Result<Option<SessionStateValue>, JournalError>>,
	},
	SessionStateCompareExchange {
		ts:        u64,
		authority: StateAuthority,
		key:       Str,
		expected:  Option<StateRevision>,
		value:     Box<RawValue>,
		request:   DurableRequest,
		reply:     flume::Sender<Result<SessionStateValue, JournalError>>,
	},
	SessionStateWatch {
		authority: StateAuthority,
		key:       Str,
		since:     Option<StateRevision>,
		reply:     flume::Sender<Result<flume::Receiver<SessionStateWatchEvent>, JournalError>>,
	},
	InvocationTransition {
		ts:         u64,
		transition: InvocationTransition,
		reply:      flume::Sender<Result<u64, JournalError>>,
	},
}

fn handle_command(journal: &mut Journal, command: ControlCommand) {
	match command {
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
			let result = journal.latest_session_state(&authority, key.as_str());
			let _ = reply.send(result);
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
			let result = journal.subscribe_session_state(&authority, key, since);
			let _ = reply.send(result);
		},
		ControlCommand::InvocationTransition { ts, transition, reply } => {
			let _ = reply.send(journal.record_invocation_transition(ts, transition));
		},
	}
}
