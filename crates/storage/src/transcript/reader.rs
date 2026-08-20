//! Transcript loading and live-chain reconstruction.

use std::{
	fs::{self, File, Metadata},
	io::{self, Read as _, Seek as _, SeekFrom},
	iter::FusedIterator,
	path::{Path, PathBuf},
};

use omp_core::sparse_set::SparseSet;
use serde_json::value::{RawValue, to_raw_value};

use super::{
	codec::{Error, Header, read_header, read_line},
	event::{Event, Kind},
	raweq::raw_eq,
};

/// One physical event line in a loaded transcript.
#[derive(Debug, Clone)]
pub enum Entry {
	/// A decoded event, including verbatim unknown events.
	Ok(Box<Event>),
	/// A malformed line retained at its physical event index.
	Tombstone(Box<RawValue>),
}
/// Equality is byte equality of stored JSON text, preserving verbatim round
/// trips.
impl PartialEq for Entry {
	fn eq(&self, other: &Self) -> bool {
		match (self, other) {
			(Self::Ok(a), Self::Ok(b)) => a == b,
			(Self::Tombstone(a), Self::Tombstone(b)) => raw_eq(a, b),
			_ => false,
		}
	}
}

impl Eq for Entry {}

/// Reusable live-chain membership and ordering over physical event indexes.
///
/// Membership uses one bit per physical line while the retained order preserves
/// the splice ordering required by compact and prompt-rewrite events. Clearing
/// and recomputing the set retains both allocations.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LiveSet {
	bits:  SparseSet<u64>,
	order: Vec<u64>,
}

impl LiveSet {
	/// Creates an empty live set.
	#[must_use]
	pub const fn new() -> Self {
		Self { bits: SparseSet::new(), order: Vec::new() }
	}

	/// Returns the number of live physical event indexes.
	#[must_use]
	pub const fn len(&self) -> usize {
		self.order.len()
	}

	/// Returns whether no physical event index is live.
	#[must_use]
	pub const fn is_empty(&self) -> bool {
		self.order.is_empty()
	}

	/// Returns whether a physical event index belongs to the live chain.
	#[must_use]
	pub fn contains(&self, index: u64) -> bool {
		self.bits.contains(index)
	}

	/// Returns the reusable membership bitmap's capacity in bits.
	#[must_use]
	pub const fn capacity(&self) -> usize {
		self.bits.capacity()
	}

	/// Returns the reusable ordered chain's element capacity.
	#[must_use]
	pub const fn chain_capacity(&self) -> usize {
		self.order.capacity()
	}

	/// Iterates live physical event indexes in reconstructed chain order.
	pub fn iter(
		&self,
	) -> impl DoubleEndedIterator<Item = u64> + ExactSizeIterator + FusedIterator + Clone + '_ {
		self.order.iter().copied()
	}

	fn clear(&mut self) {
		self.bits.clear();
		self.order.clear();
	}

	fn push(&mut self, index: u64) {
		self.bits.insert(index);
		self.order.push(index);
	}

	fn extend(&mut self, indexes: impl IntoIterator<Item = u64>) {
		for index in indexes {
			self.push(index);
		}
	}

	fn rebuild_membership(&mut self) {
		self.bits.clear();
		for &index in &self.order {
			self.bits.insert(index);
		}
	}

	fn rewind(&mut self, target: Option<u64>) -> bool {
		let Some(target) = target else {
			let changed = !self.order.is_empty();
			self.clear();
			return changed;
		};
		if let Some(position) = self.order.iter().position(|candidate| *candidate == target) {
			if position + 1 == self.order.len() {
				return false;
			}
			self.order.truncate(position + 1);
			self.rebuild_membership();
		} else {
			self.clear();
			self.push(target);
		}
		true
	}

	fn compact(&mut self, summary: u64, first_kept: u64) {
		if let Some(position) = self
			.order
			.iter()
			.position(|candidate| *candidate == first_kept)
		{
			self.order.rotate_left(position);
			self.order.truncate(self.order.len() - position);
			self.order.insert(0, summary);
			self.rebuild_membership();
		} else {
			self.clear();
			self.push(summary);
		}
	}
}

/// A loaded transcript with physical event indexes preserved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Log {
	header: Header,
	events: Vec<Entry>,
}

impl Log {
	/// Returns the line-zero identity header.
	#[must_use]
	pub const fn header(&self) -> &Header {
		&self.header
	}

	/// Returns the number of physical event lines, including tombstones.
	#[must_use]
	pub const fn len(&self) -> usize {
		self.events.len()
	}

	/// Returns whether the transcript contains no event lines.
	#[must_use]
	pub const fn is_empty(&self) -> bool {
		self.events.is_empty()
	}

	/// Returns the entry at a physical event index.
	#[must_use]
	pub fn get(&self, index: u64) -> Option<&Entry> {
		usize::try_from(index)
			.ok()
			.and_then(|index| self.events.get(index))
	}

	/// Recomputes live-chain membership into caller-owned reusable storage.
	///
	/// Ordinary events chain implicitly from the previous event. A rewind
	/// truncates the working chain to its target (or to the root), replacing
	/// the 6.1 million explicit parent pointers that 5,257 rewinds represented
	/// in the measured corpus. Reset begins a new chain boundary. Compact
	/// places its summary before the suffix beginning at `first_kept`, so the
	/// summary stands in for the discarded prefix. Amend and label events
	/// annotate a target but remain on the current chain; they do not navigate
	/// to that target. Tombstones behave as opaque ordinary events so their
	/// indexes remain addressable. No by-id or parent map is built.
	pub fn live_into(&self, out: &mut LiveSet) {
		out.clear();
		self.fold_from(0, out);
	}

	/// Reconstructs the current live chain with one forward fold.
	///
	/// Callers making repeated projections should retain a [`LiveSet`] and use
	/// [`Self::live_into`] instead.
	#[must_use]
	pub fn live(&self) -> Vec<u64> {
		let mut live = LiveSet::new();
		self.live_into(&mut live);
		live.order
	}

	/// Iterates live custom events of one declared kind, oldest physical event
	/// first.
	///
	/// The iterator borrows a previously computed [`LiveSet`], so repeated
	/// projections perform no presence-tracking allocation.
	pub fn custom<'a>(
		&'a self,
		live: &'a LiveSet,
		kind: &'a str,
	) -> impl DoubleEndedIterator<Item = (u64, &'a Event)> + FusedIterator + 'a {
		self
			.events
			.iter()
			.enumerate()
			.filter_map(move |(index, entry)| {
				let index = u64::try_from(index).expect("event indexes fit in u64");
				match entry {
					Entry::Ok(event)
						if live.contains(index)
							&& matches!(
								&event.kind,
								Kind::Custom(custom) if custom.kind() == kind
							) =>
					{
						Some((index, event.as_ref()))
					},
					_ => None,
				}
			})
	}

	fn fold_from(&self, start: usize, out: &mut LiveSet) {
		for index in start..self.events.len() {
			self.fold_entry(index, out);
		}
	}

	fn fold_entry(&self, index: usize, live: &mut LiveSet) -> bool {
		let physical_index = u64::try_from(index).expect("event indexes fit in u64");
		match &self.events[index] {
			Entry::Ok(event) => match event.as_ref() {
				Event { kind: Kind::Item(record), .. } if record.turn_id.is_some() => false,
				Event { kind: Kind::TurnReceipt(receipt), .. } => {
					let complete = receipt.item_events.len() == receipt.outcome.output.len()
						&& receipt.item_events.iter().zip(&receipt.outcome.output).all(
							|(item_index, expected)| {
								matches!(
									self.get(*item_index),
									Some(Entry::Ok(item_event))
										if matches!(
											&item_event.kind,
											Kind::Item(record)
												if record.turn_id.as_ref() == Some(&receipt.turn_id)
													&& &record.item == expected
										)
								)
							},
						);
					if complete && !receipt.item_events.is_empty() {
						live.extend(receipt.item_events.iter().copied());
						true
					} else {
						false
					}
				},
				Event { kind: Kind::Rewind { to }, .. } => live.rewind(*to),
				Event { kind: Kind::Reset, .. } => {
					live.clear();
					live.push(physical_index);
					true
				},
				Event { kind: Kind::Compact { first_kept, .. }, .. } => {
					live.compact(physical_index, *first_kept);
					true
				},
				Event { kind: Kind::PromptRewriteIntent(_) | Kind::PromptRewriteStage(_), .. } => false,
				Event { kind: Kind::PromptRewriteCommit(commit), .. } => {
					let Some(Entry::Ok(intent_event)) = self.get(commit.intent) else {
						return false;
					};
					let Kind::PromptRewriteIntent(intent) = &intent_event.kind else {
						return false;
					};
					if commit.head_events.len() != intent.head.len() {
						return false;
					}
					let complete =
						commit
							.head_events
							.iter()
							.enumerate()
							.all(|(ordinal, stage_index)| {
								matches!(
									self.get(*stage_index),
									Some(Entry::Ok(stage_event))
										if matches!(
											&stage_event.kind,
											Kind::PromptRewriteStage(stage)
												if stage.intent == commit.intent
													&& stage.ordinal == ordinal as u64
													&& stage.item == intent.head[ordinal]
										)
								)
							});
					if !complete {
						return false;
					}
					let replacement = commit
						.head_events
						.iter()
						.chain(&intent.preserved_tail)
						.copied();
					if live.iter().eq(replacement.clone()) {
						return false;
					}
					live.clear();
					live.extend(replacement);
					true
				},
				_ => {
					live.push(physical_index);
					true
				},
			},
			Entry::Tombstone(_) => {
				live.push(physical_index);
				true
			},
		}
	}
}

/// Outcome of incrementally refreshing a transcript reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshReport {
	/// What changed since the previous refresh.
	pub state:         RefreshState,
	/// Physical index that the next complete event line will receive.
	pub next_index:    u64,
	/// Byte offset where a writer may append after repairing any torn tail.
	pub append_offset: u64,
	/// Number of incomplete bytes after `append_offset`.
	pub tail_bytes:    u64,
}

/// Classification of bytes observed by an incremental refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshState {
	/// The file contains no bytes beyond the prior complete-line watermark.
	Unchanged,
	/// One or more complete bytes were consumed without an incomplete tail.
	Advanced {
		/// Number of newly parsed physical event lines.
		records: u64,
	},
	/// Bytes remain after the last complete line and must be repaired before an
	/// append.
	TornTail {
		/// Number of complete event lines parsed before the incomplete tail.
		records: u64,
	},
}

/// Incremental reader for one append-only transcript.
///
/// The reader retains decoded events, live-chain storage, and the byte offset
/// immediately after the last complete line. Refreshes parse only bytes at or
/// beyond that watermark.
pub struct Reader {
	path:              PathBuf,
	file:              File,
	identity:          FileIdentity,
	watermark:         u64,
	header_terminated: bool,
	tail_bytes:        u64,
	log:               Log,
	live:              LiveSet,
}

impl Reader {
	/// Opens a transcript and parses its complete physical lines.
	pub fn open(path: &Path) -> Result<Self, Error> {
		let mut file = File::open(path)?;
		let identity = file_identity(&file.metadata()?);
		let mut bytes = Vec::new();
		file.read_to_end(&mut bytes)?;
		if bytes.is_empty() {
			return Err(Error::MissingHeader);
		}

		let (header, event_start, header_terminated) =
			if let Some(header_end) = bytes.iter().position(|byte| *byte == b'\n') {
				(read_header(&bytes[..header_end])?, header_end + 1, true)
			} else {
				(read_header(&bytes)?, bytes.len(), false)
			};
		let event_bytes = &bytes[event_start..];
		let complete_len = event_bytes
			.iter()
			.rposition(|byte| *byte == b'\n')
			.map_or(0, |end| end + 1);
		let mut events = Vec::new();
		push_complete_entries(&mut events, &event_bytes[..complete_len]);
		let watermark = u64::try_from(event_start + complete_len).expect("file offsets fit in u64");
		let tail_bytes =
			u64::try_from(event_bytes.len() - complete_len).expect("file offsets fit in u64");
		let log = Log { header, events };
		let mut live = LiveSet::new();
		log.live_into(&mut live);
		Ok(Self {
			path: path.to_owned(),
			file,
			identity,
			watermark,
			header_terminated,
			tail_bytes,
			log,
			live,
		})
	}

	/// Parses complete lines appended since the previous refresh.
	///
	/// Replacement or truncation below the complete-line watermark returns an
	/// error without changing the decoded log or live set.
	pub fn refresh(&mut self) -> Result<RefreshReport, Error> {
		let path_metadata = fs::metadata(&self.path)?;
		if file_identity(&path_metadata) != self.identity {
			return Err(changed_file("transcript path was replaced"));
		}
		let file_len = self.file.metadata()?.len();
		if file_len < self.watermark {
			return Err(changed_file("transcript was truncated below the read watermark"));
		}

		self.file.seek(SeekFrom::Start(self.watermark))?;
		let mut appended = Vec::new();
		self.file.read_to_end(&mut appended)?;
		let path_metadata = fs::metadata(&self.path)?;
		if file_identity(&path_metadata) != self.identity {
			return Err(changed_file("transcript path was replaced during refresh"));
		}
		if self.file.metadata()?.len() < self.watermark {
			return Err(changed_file("transcript was truncated during refresh"));
		}
		if appended.is_empty() {
			self.tail_bytes = 0;
			return Ok(self.report(RefreshState::Unchanged));
		}

		let mut start = 0;
		let mut header_terminated = self.header_terminated;
		if !header_terminated {
			let Some(header_newline) = appended.iter().position(|byte| *byte == b'\n') else {
				self.tail_bytes = u64::try_from(appended.len()).expect("file offsets fit in u64");
				return Ok(self.report(RefreshState::TornTail { records: 0 }));
			};
			if header_newline != 0 {
				return Err(changed_file("bytes were inserted after an unterminated header"));
			}
			start = 1;
			header_terminated = true;
		}

		let complete_len = appended[start..]
			.iter()
			.rposition(|byte| *byte == b'\n')
			.map_or(0, |end| end + 1);
		let consumed = start + complete_len;
		let mut entries = Vec::new();
		push_complete_entries(&mut entries, &appended[start..consumed]);
		let records = u64::try_from(entries.len()).expect("event counts fit in u64");
		let first_new = self.log.events.len();
		self.log.events.extend(entries);
		self.log.fold_from(first_new, &mut self.live);
		self.watermark = self
			.watermark
			.checked_add(u64::try_from(consumed).expect("file offsets fit in u64"))
			.expect("file offsets fit in u64");
		self.header_terminated = header_terminated;
		self.tail_bytes = u64::try_from(appended.len() - consumed).expect("file offsets fit in u64");
		let state = if self.tail_bytes != 0 {
			RefreshState::TornTail { records }
		} else {
			RefreshState::Advanced { records }
		};
		Ok(self.report(state))
	}

	/// Returns the decoded transcript prefix.
	#[must_use]
	pub const fn log(&self) -> &Log {
		&self.log
	}

	/// Returns the live-chain projection for the decoded prefix.
	#[must_use]
	pub const fn live(&self) -> &LiveSet {
		&self.live
	}

	/// Returns the physical index assigned to the next complete event.
	#[must_use]
	pub fn next_index(&self) -> u64 {
		u64::try_from(self.log.len()).expect("event indexes fit in u64")
	}

	/// Returns the complete-line byte watermark.
	#[must_use]
	pub const fn append_offset(&self) -> u64 {
		self.watermark
	}

	/// Returns whether bytes remain after the complete-line watermark.
	#[must_use]
	pub const fn has_torn_tail(&self) -> bool {
		self.tail_bytes != 0
	}

	/// Returns the incomplete byte count after the complete-line watermark.
	#[must_use]
	pub const fn tail_bytes(&self) -> u64 {
		self.tail_bytes
	}

	fn report(&self, state: RefreshState) -> RefreshReport {
		RefreshReport {
			state,
			next_index: self.next_index(),
			append_offset: self.append_offset(),
			tail_bytes: self.tail_bytes(),
		}
	}
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
	device: u64,
	inode:  u64,
}

#[cfg(not(unix))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
	created: Option<std::time::SystemTime>,
}

#[cfg(unix)]
fn file_identity(metadata: &Metadata) -> FileIdentity {
	use std::os::unix::fs::MetadataExt as _;

	FileIdentity { device: metadata.dev(), inode: metadata.ino() }
}

#[cfg(not(unix))]
fn file_identity(metadata: &Metadata) -> FileIdentity {
	FileIdentity { created: metadata.created().ok() }
}

fn changed_file(message: &'static str) -> Error {
	Error::Io(io::Error::new(io::ErrorKind::InvalidData, message))
}

/// Loads a transcript while preserving every physical event index.
pub fn load(path: &Path) -> Result<Log, Error> {
	let bytes = fs::read(path)?;
	if bytes.is_empty() {
		return Err(Error::MissingHeader);
	}
	let (header_line, event_bytes) = match bytes.iter().position(|byte| *byte == b'\n') {
		Some(end) => (&bytes[..end], &bytes[end + 1..]),
		None => (&bytes[..], &[][..]),
	};
	let header = read_header(header_line)?;
	let mut events = Vec::new();
	let mut start = 0;
	for end in event_bytes
		.iter()
		.enumerate()
		.filter_map(|(index, byte)| (*byte == b'\n').then_some(index))
	{
		push_entry(&mut events, &event_bytes[start..end]);
		start = end + 1;
	}
	if start < event_bytes.len() {
		push_entry(&mut events, &event_bytes[start..]);
	}
	Ok(Log { header, events })
}

fn push_complete_entries(events: &mut Vec<Entry>, bytes: &[u8]) {
	let mut start = 0;
	for end in bytes
		.iter()
		.enumerate()
		.filter_map(|(index, byte)| (*byte == b'\n').then_some(index))
	{
		push_entry(events, &bytes[start..end]);
		start = end + 1;
	}
	debug_assert_eq!(start, bytes.len());
}

fn push_entry(events: &mut Vec<Entry>, line: &[u8]) {
	if let Ok(event) = read_line(line) {
		events.push(Entry::Ok(Box::new(event)));
	} else {
		let source = String::from_utf8_lossy(line);
		let raw = to_raw_value(source.as_ref()).expect("a JSON string is always serializable");
		events.push(Entry::Tombstone(raw));
	}
}
