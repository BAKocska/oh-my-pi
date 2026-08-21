//! Per-session telemetry side index and durable `AutoQA` issue store.
//!
//! Telemetry payloads remain in `telemetry.bin`; SQLite stores only indexed
//! columns and byte offsets. This deliberately branches from transcript entry
//! indexes rather than copying settled outcomes into a second journal.

use std::{
	fs::{File, OpenOptions},
	io::{self, Write as _},
	path::{Path, PathBuf},
	sync::{
		Arc,
		atomic::{AtomicBool, AtomicU64, Ordering},
	},
};

use omp_core::Str;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};

/// A byte offset into a session's append-only `telemetry.bin` side file.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct TelemetryWatermark(pub u64);

/// The indexed scalar facts of one telemetry record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexedEvent {
	/// Session containing the event.
	pub session_id:     Str,
	/// Offset of the framed payload in that session's `telemetry.bin`.
	pub offset:         TelemetryWatermark,
	/// Firehose kind string.
	pub kind:           Str,
	/// Event timestamp in Unix milliseconds.
	pub occurred_at_ms: u64,
	/// True when this row was reconstructed from transcript replay.
	pub backfilled:     bool,
}

/// Result of an indexed telemetry query.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TelemetryQueryResult {
	/// Rows selected by the core-side query.
	pub rows:       Vec<IndexedEvent>,
	/// Whether the caller's install-time access floor removed older rows.
	pub floored:    bool,
	/// Whether transcript replay supplied any returned rows.
	pub backfilled: bool,
}

/// Durable `AutoQA` issue metadata kept beside telemetry index rows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredIssue {
	/// Stable issue identifier.
	pub id:            Str,
	/// Session that filed the issue.
	pub session_id:    Str,
	/// Device that produced the reported result.
	pub device:        Str,
	/// Committed device revision, if the target has one.
	pub rev:           Option<Str>,
	/// User consent disposition.
	pub consent:       Str,
	/// Creation timestamp in Unix milliseconds.
	pub created_at_ms: u64,
}

/// A cancellation guard for a core-side query.
#[derive(Clone, Debug, Default)]
pub struct QueryGuard(Arc<AtomicBool>);

impl QueryGuard {
	/// Creates an uncancelled query guard.
	#[must_use]
	pub fn new() -> Self {
		Self::default()
	}

	/// Returns whether query work should stop.
	#[must_use]
	pub fn cancelled(&self) -> bool {
		self.0.load(Ordering::Relaxed)
	}
}

impl Drop for QueryGuard {
	fn drop(&mut self) {
		self.0.store(true, Ordering::Relaxed);
	}
}
/// Operators accepted by the restricted telemetry `where` language.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WhereOp {
	/// Exact equality.
	Eq,
	/// Exact inequality.
	Ne,
	/// Greater-than comparison.
	Gt,
	/// Greater-than-or-equal comparison.
	Ge,
	/// Less-than comparison.
	Lt,
	/// Less-than-or-equal comparison.
	Le,
}

impl WhereOp {
	const fn sql(self) -> &'static str {
		match self {
			Self::Eq => "=",
			Self::Ne => "!=",
			Self::Gt => ">",
			Self::Ge => ">=",
			Self::Lt => "<",
			Self::Le => "<=",
		}
	}
}

/// A scalar predicate that compiles to SQLite and has a matching Rust
/// evaluator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Where {
	/// Indexed field name: `kind`, `occurred_at_ms`, or `offset`.
	pub field: Str,
	/// Comparison operator.
	pub op:    WhereOp,
	/// String representation of the scalar value.
	pub value: Str,
}

/// Error produced when a telemetry query is not safely indexable.
#[derive(Debug, thiserror::Error)]
pub enum QueryError {
	/// The requested field has no index column or Rust evaluator.
	#[error("unknown telemetry query field {0}")]
	UnknownField(Str),
	/// A numeric predicate had an invalid value.
	#[error("invalid numeric telemetry query value {0}")]
	InvalidNumber(Str),
	/// A side-file operation failed.
	#[error("telemetry side-file error: {0}")]
	Io(#[from] io::Error),
	/// SQLite reported an error.
	#[error("telemetry index error: {0}")]
	Sql(#[from] rusqlite::Error),
	/// The query was cancelled by dropping its guard.
	#[error("telemetry query cancelled")]
	Cancelled,
}

impl Where {
	/// Produces the SQL expression for this predicate after whitelisting its
	/// column.
	///
	/// # Errors
	/// Returns [`QueryError`] when the field is not indexed or a numeric literal
	/// cannot be represented exactly.
	pub fn to_sql(&self) -> Result<String, QueryError> {
		match self.field.as_str() {
			"kind" => Ok(format!("kind {} ?", self.op.sql())),
			"occurred_at_ms" | "offset" => {
				self
					.value
					.as_str()
					.parse::<u64>()
					.map_err(|_| QueryError::InvalidNumber(self.value.clone()))?;
				Ok(format!("{} {} ?", self.field, self.op.sql()))
			},
			_ => Err(QueryError::UnknownField(self.field.clone())),
		}
	}

	/// Evaluates this exact indexed predicate without SQLite for transcript
	/// backfill.
	///
	/// # Errors
	/// Returns the same field/value errors as [`Self::to_sql`].
	pub fn matches(&self, event: &IndexedEvent) -> Result<bool, QueryError> {
		match self.field.as_str() {
			"kind" => Ok(compare_str(event.kind.as_str(), self.value.as_str(), self.op)),
			"occurred_at_ms" => compare_u64(event.occurred_at_ms, &self.value, self.op),
			"offset" => compare_u64(event.offset.0, &self.value, self.op),
			_ => Err(QueryError::UnknownField(self.field.clone())),
		}
	}
}

fn compare_str(left: &str, right: &str, op: WhereOp) -> bool {
	match op {
		WhereOp::Eq => left == right,
		WhereOp::Ne => left != right,
		WhereOp::Gt => left > right,
		WhereOp::Ge => left >= right,
		WhereOp::Lt => left < right,
		WhereOp::Le => left <= right,
	}
}

fn compare_u64(left: u64, right: &Str, op: WhereOp) -> Result<bool, QueryError> {
	let right = right
		.as_str()
		.parse::<u64>()
		.map_err(|_| QueryError::InvalidNumber(right.clone()))?;
	Ok(match op {
		WhereOp::Eq => left == right,
		WhereOp::Ne => left != right,
		WhereOp::Gt => left > right,
		WhereOp::Ge => left >= right,
		WhereOp::Lt => left < right,
		WhereOp::Le => left <= right,
	})
}

/// Incremental side-file indexer backed by the project telemetry database.
pub struct TelemetryIndex {
	database:    Mutex<Connection>,
	side_file:   Mutex<File>,
	side_path:   PathBuf,
	next_offset: AtomicU64,
}

impl TelemetryIndex {
	/// Opens the session's side file and the project telemetry index database.
	///
	/// # Errors
	/// Returns file-system or SQLite errors when the durable index cannot be
	/// opened or initialized.
	pub fn open(session_dir: &Path, database_path: &Path) -> Result<Self, QueryError> {
		std::fs::create_dir_all(session_dir)?;
		let side_path = session_dir.join("telemetry.bin");
		let side_file = OpenOptions::new()
			.create(true)
			.append(true)
			.read(true)
			.open(&side_path)?;
		let next_offset = side_file.metadata()?.len();
		let database = Connection::open(database_path)?;
		database.execute_batch(
			"CREATE TABLE IF NOT EXISTS telemetry_events (
				session_id TEXT NOT NULL,
				offset INTEGER NOT NULL,
				kind TEXT NOT NULL,
				occurred_at_ms INTEGER NOT NULL,
				backfilled INTEGER NOT NULL DEFAULT 0,
				PRIMARY KEY(session_id, offset)
			);
			CREATE INDEX IF NOT EXISTS telemetry_events_query
				ON telemetry_events(session_id, kind, occurred_at_ms, offset);
			CREATE TABLE IF NOT EXISTS telemetry_issues (
				id TEXT PRIMARY KEY,
				session_id TEXT NOT NULL,
				device TEXT NOT NULL,
				rev TEXT,
				consent TEXT NOT NULL,
				created_at_ms INTEGER NOT NULL
			);",
		)?;
		Ok(Self {
			database: Mutex::new(database),
			side_file: Mutex::new(side_file),
			side_path,
			next_offset: AtomicU64::new(next_offset),
		})
	}

	/// Returns the current byte-offset watermark of `telemetry.bin`.
	#[must_use]
	pub fn watermark(&self) -> TelemetryWatermark {
		TelemetryWatermark(self.next_offset.load(Ordering::Acquire))
	}

	/// Returns the session side-file path.
	#[must_use]
	pub fn side_path(&self) -> &Path {
		&self.side_path
	}

	/// Appends one encoded event and incrementally indexes its scalar facts.
	///
	/// # Errors
	/// Returns an I/O or SQLite error when either durable write fails.
	pub fn append(
		&self,
		session_id: &str,
		kind: &str,
		occurred_at_ms: u64,
		encoded: &[u8],
	) -> Result<TelemetryWatermark, QueryError> {
		let length = u32::try_from(encoded.len()).map_err(|_| {
			QueryError::Io(io::Error::new(
				io::ErrorKind::InvalidInput,
				"telemetry event exceeds u32 frame",
			))
		})?;
		let offset;
		{
			let mut file = self.side_file.lock();
			offset = self.next_offset.load(Ordering::Acquire);
			file.write_all(&length.to_le_bytes())?;
			file.write_all(encoded)?;
			file.flush()?;
			self
				.next_offset
				.store(offset.saturating_add(u64::from(length) + 4), Ordering::Release);
		}
		self.database.lock().execute(
			"INSERT INTO telemetry_events(session_id, offset, kind, occurred_at_ms, backfilled)
			 VALUES(?1, ?2, ?3, ?4, 0)",
			params![session_id, offset, kind, occurred_at_ms],
		)?;
		Ok(TelemetryWatermark(offset))
	}

	/// Inserts a transcript-replayed telemetry row without duplicating its
	/// payload.
	///
	/// # Errors
	/// Returns a SQLite error when the index row cannot be written.
	pub fn backfill(&self, event: &IndexedEvent) -> Result<(), QueryError> {
		self.database.lock().execute(
			"INSERT OR IGNORE INTO telemetry_events(session_id, offset, kind, occurred_at_ms, \
			 backfilled)
			 VALUES(?1, ?2, ?3, ?4, 1)",
			params![
				event.session_id.as_str(),
				event.offset.0,
				event.kind.as_str(),
				event.occurred_at_ms
			],
		)?;
		Ok(())
	}

	/// Executes a core-side index query; only returned rows should cross
	/// CONTROL.
	///
	/// # Errors
	/// Returns invalid-predicate, SQLite, or cancellation errors.
	pub fn query(
		&self,
		session_id: &str,
		predicate: Option<&Where>,
		install_floor: Option<TelemetryWatermark>,
		guard: &QueryGuard,
	) -> Result<TelemetryQueryResult, QueryError> {
		if guard.cancelled() {
			return Err(QueryError::Cancelled);
		}
		let (sql, value) = match predicate {
			Some(predicate) => {
				(format!(" AND {}", predicate.to_sql()?), Some(predicate.value.as_str()))
			},
			None => (String::new(), None),
		};
		let floor = install_floor.map_or(0, |floor| floor.0);
		let statement = format!(
			"SELECT session_id, offset, kind, occurred_at_ms, backfilled FROM telemetry_events
			 WHERE session_id = ?1 AND offset >= ?2{sql} ORDER BY offset ASC"
		);
		let database = self.database.lock();
		let mut query = database.prepare(&statement)?;
		let mut rows = if let Some(value) = value {
			query.query(params![session_id, floor, value])?
		} else {
			query.query(params![session_id, floor])?
		};
		let mut result = TelemetryQueryResult::default();
		while let Some(row) = rows.next()? {
			if guard.cancelled() {
				return Err(QueryError::Cancelled);
			}
			let backfilled = row.get::<_, i64>(4)? != 0;
			result.backfilled |= backfilled;
			result.rows.push(IndexedEvent {
				session_id: Str::from(row.get::<_, String>(0)?),
				offset: TelemetryWatermark(row.get(1)?),
				kind: Str::from(row.get::<_, String>(2)?),
				occurred_at_ms: row.get(3)?,
				backfilled,
			});
		}
		result.floored = install_floor.is_some();
		Ok(result)
	}

	/// Stores an `AutoQA` issue in the audit-tier issue table.
	///
	/// # Errors
	/// Returns a SQLite error when the durable issue row cannot be written.
	pub fn store_issue(&self, issue: &StoredIssue) -> Result<(), QueryError> {
		self.database.lock().execute(
			"INSERT OR REPLACE INTO telemetry_issues(id, session_id, device, rev, consent, \
			 created_at_ms)
			 VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
			params![
				issue.id.as_str(),
				issue.session_id.as_str(),
				issue.device.as_str(),
				issue.rev.as_ref().map(|rev| rev.as_str()),
				issue.consent.as_str(),
				issue.created_at_ms,
			],
		)?;
		Ok(())
	}

	/// Reads a durable `AutoQA` issue by identifier.
	///
	/// # Errors
	/// Returns a SQLite error when the issue table cannot be queried.
	pub fn issue(&self, id: &str) -> Result<Option<StoredIssue>, QueryError> {
		self
			.database
			.lock()
			.query_row(
				"SELECT id, session_id, device, rev, consent, created_at_ms FROM telemetry_issues \
				 WHERE id = ?1",
				params![id],
				|row| {
					Ok(StoredIssue {
						id:            Str::from(row.get::<_, String>(0)?),
						session_id:    Str::from(row.get::<_, String>(1)?),
						device:        Str::from(row.get::<_, String>(2)?),
						rev:           row.get::<_, Option<String>>(3)?.map(Str::from),
						consent:       Str::from(row.get::<_, String>(4)?),
						created_at_ms: row.get(5)?,
					})
				},
			)
			.optional()
			.map_err(QueryError::from)
	}
}

#[cfg(test)]
mod tests {
	use omp_core::sf;
	use tempfile::tempdir;

	use super::*;

	#[test]
	fn sql_and_backfill_predicates_agree() {
		let predicate = Where { field: sf!("kind"), op: WhereOp::Eq, value: sf!("tool_call") };
		assert_eq!(predicate.to_sql().unwrap(), "kind = ?");
		assert!(
			predicate
				.matches(&IndexedEvent {
					session_id:     sf!("s"),
					offset:         TelemetryWatermark(0),
					kind:           sf!("tool_call"),
					occurred_at_ms: 1,
					backfilled:     true,
				})
				.unwrap()
		);
	}

	#[test]
	fn append_tracks_byte_offset_watermark() {
		let temporary = tempdir().unwrap();
		let index =
			TelemetryIndex::open(temporary.path(), &temporary.path().join("telemetry.sqlite"))
				.unwrap();
		assert_eq!(index.append("s", "turn_start", 1, b"one").unwrap(), TelemetryWatermark(0));
		assert_eq!(index.watermark(), TelemetryWatermark(7));
	}
}
