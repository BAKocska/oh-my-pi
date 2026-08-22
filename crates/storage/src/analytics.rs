//! Consent-gated local derived counters in a content-free database.
//!
//! This index is intentionally a separate SQLite authority from the private
//! prompt FTS database, so no SQL query can join counters to raw prompts.

use std::{path::Path, time::Duration};

use parking_lot::Mutex;
use rusqlite::{Connection, params};

use crate::transcript::SessionId;

/// Content-free analytics dimensions. This type cannot carry prompts,
/// responses, tool arguments, paths, or provider payloads.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AnalyticsCounters {
	/// Settled logical turns.
	pub turns:              u64,
	/// Durable user messages.
	pub user_messages:      u64,
	/// Durable assistant messages.
	pub assistant_messages: u64,
	/// Issued tool calls.
	pub tool_calls:         u64,
	/// Failed tool results.
	pub tool_errors:        u64,
	/// Settled input tokens.
	pub input_tokens:       u64,
	/// Settled output tokens.
	pub output_tokens:      u64,
}

/// Dedicated local analytics authority with no prompt-content schema.
pub struct AnalyticsIndex {
	connection: Mutex<Connection>,
}

impl AnalyticsIndex {
	/// Opens the content-free analytics database.
	pub fn open(path: impl AsRef<Path>) -> rusqlite::Result<Self> {
		let connection = Connection::open(path)?;
		connection.busy_timeout(Duration::from_secs(5))?;
		connection.pragma_update(None, "journal_mode", "WAL")?;
		connection.pragma_update(None, "synchronous", "FULL")?;
		connection.execute_batch(
			"CREATE TABLE IF NOT EXISTS session_counters (
			    session_id TEXT PRIMARY KEY,
			    turns INTEGER NOT NULL,
			    user_messages INTEGER NOT NULL,
			    assistant_messages INTEGER NOT NULL,
			    tool_calls INTEGER NOT NULL,
			    tool_errors INTEGER NOT NULL,
			    input_tokens INTEGER NOT NULL,
			    output_tokens INTEGER NOT NULL,
			    updated_ms INTEGER NOT NULL
			 ) WITHOUT ROWID;",
		)?;
		Ok(Self { connection: Mutex::new(connection) })
	}

	/// Replaces derived counters only after explicit consent. Declined writes
	/// perform no mutation and return `false`.
	pub fn record(
		&self,
		session: &SessionId,
		counters: AnalyticsCounters,
		updated_ms: u64,
		consented: bool,
	) -> rusqlite::Result<bool> {
		if !consented {
			return Ok(false);
		}
		self.connection.lock().execute(
			"INSERT OR REPLACE INTO session_counters(
			 session_id, turns, user_messages, assistant_messages, tool_calls, tool_errors,
			 input_tokens, output_tokens, updated_ms
			 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
			params![
				session.0.as_str(),
				i64_counter(counters.turns)?,
				i64_counter(counters.user_messages)?,
				i64_counter(counters.assistant_messages)?,
				i64_counter(counters.tool_calls)?,
				i64_counter(counters.tool_errors)?,
				i64_counter(counters.input_tokens)?,
				i64_counter(counters.output_tokens)?,
				i64_counter(updated_ms)?,
			],
		)?;
		Ok(true)
	}
}

fn i64_counter(value: u64) -> rusqlite::Result<i64> {
	i64::try_from(value).map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
}
