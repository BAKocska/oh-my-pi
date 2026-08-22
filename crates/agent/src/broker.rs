//! Process-global agent registry and project-scoped IRC routing.

use std::{
	collections::{HashMap, VecDeque},
	fs,
	io::{BufRead as _, Read as _},
	path::{Path, PathBuf},
	sync::{
		Arc, LazyLock, Weak,
		atomic::{AtomicU64, Ordering},
	},
	time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use omp_core::Str;
use omp_proto::thread::v1::{Item, Message as ThreadMessage, Part, Role, item, part};
use parking_lot::Mutex;
use smallvec::SmallVec;
use thiserror::Error;

use crate::{AgentKind, AgentNode, Interrupt, InterruptClass, InterruptSource, MailboxSender};

const MAILBOX_CAPACITY: usize = 100;
const ACTIVITY_MAX_CHARS: usize = 80;
const DISCOVERY_DIAGNOSTIC_CAPACITY: usize = 128;
const DELIVERY_DEDUP_CAPACITY: usize = 1_024;
const PREFIX_MAX_LINES: usize = 64;
const PREFIX_MAX_BYTES: usize = 256 * 1_024;
const TASK_SUMMARY_MAX_CHARS: usize = 160;
const QUERY_MAX_BYTES: usize = 4 * 1_024 * 1_024;
const QUERY_MAX_CHARS: usize = 4_096;
const QUERY_MAX_DEPTH: usize = 64;
const QUERY_MAX_DURATION: Duration = Duration::from_millis(100);

/// Delivery boundary requested by a peer message.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display, strum::EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum DeliveryMode {
	/// Deliver at a tool-completion boundary without cancelling work.
	Aside,
	/// Deliver as an immediate steer interrupt.
	Steer,
	/// Deliver only before the next turn.
	NextTurn,
}

/// Fire-and-forget delivery outcome for one recipient.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display, strum::EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum Receipt {
	/// Injected into a running recipient at its requested boundary.
	Injected,
	/// Injected into an idle live recipient, which may begin a turn.
	Woken,
	/// Accepted by the recipient's cold-revival transport.
	Revived,
	/// No live or revivable target accepted the message.
	Failed,
}

/// Process-global lifecycle state retained after a live loop is detached.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display, strum::EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum RegistryStatus {
	/// A turn is active.
	Running = 0,
	/// A live in-memory session is waiting for work.
	Idle    = 1,
	/// Live resources were evicted; the durable journal can restart the loop.
	Parked  = 2,
}

/// Classification for a bounded transcript-prefix discovery diagnostic.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display, strum::EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum DiscoveryDiagnosticKind {
	/// The v4 header or a prefix event was malformed.
	Corrupt,
	/// The bounded prefix ended before durable child initialization appeared.
	Incomplete,
}

/// A retained diagnostic for an on-disk journal that could not be registered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryDiagnostic {
	/// Journal path that was inspected.
	pub path: PathBuf,
	/// Stable machine-readable classification.
	pub kind: DiscoveryDiagnosticKind,
}

/// Generation-fenced request to detach one idle live runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParkLease {
	/// Parked durable registry projection.
	pub record:   AgentRecord,
	/// Revision that must still match before live resources are detached.
	pub revision: u64,
}

/// Historical, non-secret metrics retained after an agent parks.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AgentHistory {
	/// Provider requests completed by this agent.
	pub requests:      u64,
	/// Metered input tokens.
	pub input_tokens:  u64,
	/// Metered output and reasoning tokens.
	pub output_tokens: u64,
	/// Durable receipt cost in micro-USD.
	pub usd_micros:    u64,
	/// Total active duration in milliseconds.
	pub duration_ms:   u64,
	/// Markdown or structured output artifact.
	pub output_path:   Option<PathBuf>,
	/// Preserved patch artifact.
	pub patch_path:    Option<PathBuf>,
	/// Preserved branch name.
	pub branch:        Option<Str>,
	/// Historical terminal outcome; cancellation and failure never destroy
	/// identity.
	pub terminal:      Option<crate::SubagentTerminalStatus>,
}

/// Clone-cheap process-global roster projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentRecord {
	/// Stable process identity.
	pub id:               Str,
	/// Human-readable routing name.
	pub name:             Str,
	/// Main, subagent, or advisor classification.
	pub kind:             AgentKind,
	/// Parent agent identity.
	pub parent:           Option<Str>,
	/// Owning durable session identity.
	pub session:          Str,
	/// Recursion depth.
	pub depth:            u16,
	/// Current process-global lifecycle state.
	pub status:           RegistryStatus,
	/// Sanitized one-line activity gist.
	pub activity:         Str,
	/// Last lifecycle or activity change in epoch milliseconds.
	pub last_activity_ms: u64,
	/// Read-only transcript backing history and cold revival.
	pub transcript:       Option<PathBuf>,
	/// Agent definition name used to create the session.
	pub definition:       Option<Str>,
	/// Requested model role or selector retained from child initialization.
	pub model:            Option<Str>,
	/// Actual serving model most recently observed in the bounded prefix.
	pub serving_model:    Option<Str>,
	/// Normalized task summary for historical rosters.
	pub task:             Option<Str>,
	/// Historical execution and merge facts.
	pub history:          AgentHistory,
}

/// Credential-free registry row safe for collaboration presentation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CollabAgentRecord {
	/// Stable process identity.
	pub id:               Str,
	/// Sanitized display name.
	pub name:             Str,
	/// Main or task-subagent classification; advisors are never representable.
	pub kind:             CollabAgentKind,
	/// Visible parent agent identity.
	pub parent:           Option<Str>,
	/// Current lifecycle state.
	pub status:           RegistryStatus,
	/// Whether a bounded transcript fetch may be requested.
	pub has_transcript:   bool,
	/// Last activity change in epoch milliseconds.
	pub last_activity_ms: u64,
}

/// Agent kinds permitted in collaboration registry snapshots.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum CollabAgentKind {
	/// Main session agent.
	Main,
	/// User-visible task subagent.
	Sub,
}

/// Generation-fenced collaboration registry snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CollabRegistrySnapshot {
	/// Monotonic process-global registry generation.
	pub generation: u64,
	/// Deterministically ordered public registry rows.
	pub agents:     Arc<[CollabAgentRecord]>,
}

/// Registry compare-and-swap or persistence failure.
#[derive(Debug, Error)]
pub enum RegistryError {
	/// The stable id was not registered.
	#[error("agent {0} is not registered")]
	NotFound(Str),
	/// The expected record revision did not match.
	#[error("agent {id} registry revision changed (expected {expected}, actual {actual})")]
	Revision {
		/// Stable agent id.
		id:       Str,
		/// Revision supplied by the caller.
		expected: u64,
		/// Current revision.
		actual:   u64,
	},
	/// A recovered display alias remains reserved by another stable identity.
	#[error("agent display name remains reserved: {0}")]
	NameReserved(Str),
	/// The requested agent or history artifact does not exist.
	#[error("agent resource was not found: {0}")]
	ResourceNotFound(Str),
	/// Agent output was not valid JSON.
	#[error("agent output is not valid JSON")]
	InvalidJson(#[source] serde_json::Error),
	/// The jq program could not be loaded or compiled.
	#[error("agent output query is invalid")]
	InvalidQuery,
	/// The jq program exceeded its query, input, output, depth, or time bound.
	#[error("agent output query exceeded a safety limit")]
	QueryLimit,
	/// The jq program emitted no value.
	#[error("agent output query emitted no value")]
	QueryEmpty,
	/// The jq program emitted more than one value.
	#[error("agent output query emitted more than one value")]
	QueryMultiple,
	/// A transcript or artifact could not be read.
	#[error("agent resource I/O failed: {0}")]
	Io(#[from] std::io::Error),
}

struct RegistryEntry {
	record:       AgentRecord,
	revision:     u64,
	live_history: Option<Arc<[u8]>>,
}

struct RegistryInner {
	records:     Mutex<HashMap<Str, RegistryEntry>>,
	diagnostics: Mutex<VecDeque<DiscoveryDiagnostic>>,
	generation:  tokio::sync::watch::Sender<u64>,
}

/// Process-global CAS registry for live, parked, and disk-recovered agents.
#[derive(Clone)]
pub struct AgentRegistry {
	inner: Arc<RegistryInner>,
}

impl Default for AgentRegistry {
	fn default() -> Self {
		Self::new()
	}
}

impl AgentRegistry {
	/// Returns the one process-global registry.
	#[must_use]
	pub fn global() -> &'static Self {
		static GLOBAL: LazyLock<AgentRegistry> = LazyLock::new(AgentRegistry::new);
		&GLOBAL
	}

	/// Creates an independent registry, primarily for an isolated daemon or
	/// test.
	#[must_use]
	pub fn new() -> Self {
		let (generation, _) = tokio::sync::watch::channel(0_u64);
		Self {
			inner: Arc::new(RegistryInner {
				records: Mutex::new(HashMap::new()),
				diagnostics: Mutex::new(VecDeque::with_capacity(DISCOVERY_DIAGNOSTIC_CAPACITY)),
				generation,
			}),
		}
	}

	/// Subscribes to every registration, lifecycle, activity, and history
	/// change.
	#[must_use]
	pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
		self.inner.generation.subscribe()
	}

	/// Returns the current process-global generation.
	#[must_use]
	pub fn generation(&self) -> u64 {
		*self.inner.generation.borrow()
	}

	/// Registers `record` iff `expected` matches the current revision. `None`
	/// requires the id to be absent.
	pub fn compare_and_register(
		&self,
		mut record: AgentRecord,
		expected: Option<u64>,
	) -> Result<u64, RegistryError> {
		let mut records = self.inner.records.lock();
		let existing_key = records
			.keys()
			.find(|id| id.as_str().eq_ignore_ascii_case(record.id.as_str()))
			.cloned();
		let previous = existing_key.as_ref().and_then(|id| records.get(id));
		match previous {
			Some(entry) if expected != Some(entry.revision) => {
				return Err(RegistryError::Revision {
					id:       entry.record.id.clone(),
					expected: expected.unwrap_or(0),
					actual:   entry.revision,
				});
			},
			None if expected.is_some() => return Err(RegistryError::NotFound(record.id)),
			_ => {},
		}
		if records.iter().any(|(id, entry)| {
			existing_key.as_ref() != Some(id)
				&& entry
					.record
					.name
					.as_str()
					.eq_ignore_ascii_case(record.name.as_str())
		}) {
			return Err(RegistryError::NameReserved(record.name));
		}
		let key = existing_key.unwrap_or_else(|| record.id.clone());
		record.id = key.clone();
		record.activity = sanitize_activity(record.activity.as_str());
		let previous = records.get(&key);
		let revision = previous.map_or(1, |entry| entry.revision.saturating_add(1));
		let live_history = previous.and_then(|entry| entry.live_history.clone());
		records.insert(key, RegistryEntry { record, revision, live_history });
		drop(records);
		self.bump_generation();
		Ok(revision)
	}

	/// Registers a live tree node while preserving its durable identity.
	pub fn register_node(
		&self,
		node: &AgentNode,
		status: RegistryStatus,
		transcript: Option<PathBuf>,
	) -> Result<u64, RegistryError> {
		let previous = self.revision(node.id.as_str());
		self.compare_and_register(
			AgentRecord {
				id: node.id.clone(),
				name: node.name.clone(),
				kind: node.kind,
				parent: node.parent.clone(),
				session: node.session.clone(),
				depth: node.depth,
				status,
				activity: node.activity(),
				last_activity_ms: now_ms(),
				transcript,
				definition: None,
				model: None,
				serving_model: None,
				task: None,
				history: AgentHistory::default(),
			},
			previous,
		)
	}

	/// Returns one record and its CAS revision.
	#[must_use]
	pub fn record(&self, id: &str) -> Option<(AgentRecord, u64)> {
		let records = self.inner.records.lock();
		let (_, entry) = find_record(&records, id)?;
		Some((entry.record.clone(), entry.revision))
	}

	/// Lists the roster deterministically, optionally retaining advisors.
	#[must_use]
	pub fn roster(&self, include_advisors: bool) -> Vec<AgentRecord> {
		let mut records = self
			.inner
			.records
			.lock()
			.values()
			.filter(|entry| include_advisors || entry.record.kind != AgentKind::Advisor)
			.map(|entry| entry.record.clone())
			.collect::<Vec<_>>();
		records.sort_by(|left, right| {
			left
				.last_activity_ms
				.cmp(&right.last_activity_ms)
				.then_with(|| left.id.cmp(&right.id))
		});
		records
	}

	/// Returns a generation-fenced, credential-free collaboration roster.
	///
	/// Advisor identities, transcript paths, workspace/session ids, models,
	/// activity text, and historical artifacts remain host-local.
	#[must_use]
	pub fn collab_snapshot(&self) -> CollabRegistrySnapshot {
		let generation = self.generation();
		let agents = self
			.roster(false)
			.into_iter()
			.filter_map(|record| {
				let kind = match record.kind {
					AgentKind::Main => CollabAgentKind::Main,
					AgentKind::Subagent => CollabAgentKind::Sub,
					AgentKind::Advisor => return None,
				};
				Some(CollabAgentRecord {
					id: record.id,
					name: record.name,
					kind,
					parent: record.parent,
					status: record.status,
					has_transcript: record.transcript.is_some(),
					last_activity_ms: record.last_activity_ms,
				})
			})
			.collect::<Vec<_>>()
			.into();
		CollabRegistrySnapshot { generation, agents }
	}

	/// CAS-updates one lifecycle state.
	pub fn set_status(
		&self,
		id: &str,
		expected: Option<u64>,
		status: RegistryStatus,
	) -> Result<u64, RegistryError> {
		self.update(id, expected, |record| {
			record.status = status;
			Ok(())
		})
	}

	/// Replaces the sanitized activity gist and refreshes idle TTL accounting.
	pub fn set_activity(&self, id: &str, activity: &str) -> Result<u64, RegistryError> {
		let activity = sanitize_activity(activity);
		self.update(id, None, |record| {
			record.activity = activity;
			Ok(())
		})
	}

	/// Replaces durable transcript, model, task, and historical result facts.
	pub fn set_history(
		&self,
		id: &str,
		transcript: Option<PathBuf>,
		model: Option<Str>,
		task: Option<Str>,
		history: AgentHistory,
	) -> Result<u64, RegistryError> {
		self.update(id, None, |record| {
			record.transcript = transcript;
			record.model = model;
			record.task = task;
			record.history = history;
			Ok(())
		})
	}

	/// Retains a terminal generation outcome without changing durable identity.
	pub fn set_terminal(
		&self,
		id: &str,
		terminal: crate::SubagentTerminalStatus,
	) -> Result<u64, RegistryError> {
		self.update(id, None, |record| {
			record.history.terminal = Some(terminal.bounded());
			Ok(())
		})
	}

	/// Parks idle records whose TTL elapsed and returns records whose owners
	/// should dispose their live sessions.
	pub fn park_expired(&self, now: u64, ttl: Duration) -> Vec<ParkLease> {
		if ttl.is_zero() {
			return Vec::new();
		}
		let ttl = u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX);
		let mut records = self.inner.records.lock();
		let mut parked = Vec::new();
		for entry in records.values_mut() {
			if entry.record.status == RegistryStatus::Idle
				&& now.saturating_sub(entry.record.last_activity_ms) >= ttl
			{
				entry.record.status = RegistryStatus::Parked;
				entry.record.last_activity_ms = now;
				entry.revision = entry.revision.saturating_add(1);
				parked.push(ParkLease { record: entry.record.clone(), revision: entry.revision });
			}
		}
		drop(records);
		if !parked.is_empty() {
			self.bump_generation();
		}
		parked
	}

	/// Imports bounded valid transcript prefixes as parked records.
	///
	/// Malformed and incomplete journals remain untouched and are exposed
	/// through [`Self::discovery_diagnostics`].
	pub fn discover_transcripts(&self, directory: &Path) -> Result<usize, RegistryError> {
		let mut imported = 0;
		for entry in fs::read_dir(directory)? {
			let path = entry?.path();
			if path.extension().and_then(std::ffi::OsStr::to_str) != Some("jsonl") {
				continue;
			}
			match cold_record(&path)? {
				ColdScan::Record(record) => {
					let expected = self.revision(record.id.as_str());
					if self.compare_and_register(record, expected).is_ok() {
						imported += 1;
					}
				},
				ColdScan::Skipped(kind) => self.record_discovery_diagnostic(path, kind),
			}
		}
		Ok(imported)
	}

	/// Returns retained bounded-prefix diagnostics, oldest first.
	#[must_use]
	pub fn discovery_diagnostics(&self) -> Vec<DiscoveryDiagnostic> {
		self.inner.diagnostics.lock().iter().cloned().collect()
	}

	/// Resolves `agent://<id>` or `agent://<id>/<child>` to immutable artifact
	/// bytes. Child names become dot-separated artifact stems.
	pub fn resolve_agent(&self, resource: &str) -> Result<Vec<u8>, RegistryError> {
		Ok(fs::read(self.agent_path(resource)?)?)
	}

	/// Resolves an output and applies one bounded jq-compatible expression.
	pub fn resolve_agent_query(
		&self,
		resource: &str,
		query: &str,
	) -> Result<Vec<u8>, RegistryError> {
		let path = self.agent_path(resource)?;
		if fs::metadata(&path)?.len() > QUERY_MAX_BYTES as u64 {
			return Err(RegistryError::QueryLimit);
		}
		let bytes = fs::read(path)?;
		bounded_json_query(&bytes, query)
	}

	fn agent_path(&self, resource: &str) -> Result<PathBuf, RegistryError> {
		let resource = resource.trim_start_matches('/');
		let (id, child) = resource.split_once('/').unwrap_or((resource, ""));
		let (record, _) = self
			.record(id)
			.ok_or_else(|| RegistryError::ResourceNotFound(Str::new(id)))?;
		if child.is_empty() {
			return record
				.history
				.output_path
				.ok_or_else(|| RegistryError::ResourceNotFound(Str::new(resource)));
		}
		let child = child.replace('/', ".");
		if !valid_artifact_component(&child) {
			return Err(RegistryError::ResourceNotFound(Str::new(resource)));
		}
		let parent = record
			.history
			.output_path
			.as_ref()
			.and_then(|path| path.parent())
			.ok_or_else(|| RegistryError::ResourceNotFound(Str::new(resource)))?;
		Ok(parent.join(format!("{}.{}.md", record.id, child)))
	}

	/// Replaces the live in-memory transcript projection for one session.
	///
	/// Returns whether a matching live registry entry was present.
	#[must_use]
	pub fn set_live_history(&self, session: &str, history: Vec<u8>) -> bool {
		let mut records = self.inner.records.lock();
		let Some(entry) = records
			.values_mut()
			.find(|entry| entry.record.session == session || entry.record.id == session)
		else {
			return false;
		};
		entry.live_history = Some(Arc::from(history));
		true
	}

	/// Resolves `history://` to a roster index and `history://<id>` to immutable
	/// transcript bytes.
	pub fn resolve_history(&self, resource: &str) -> Result<Vec<u8>, RegistryError> {
		let id = resource.trim_matches('/');
		if id.is_empty() {
			return Ok(self.history_index().into_bytes());
		}
		let (live_history, path) = {
			let records = self.inner.records.lock();
			let (_, entry) = find_record(&records, id)
				.ok_or_else(|| RegistryError::ResourceNotFound(Str::new(id)))?;
			(entry.live_history.clone(), entry.record.transcript.clone())
		};
		if let Some(history) = live_history {
			return Ok(history.to_vec());
		}
		let path = path.ok_or_else(|| RegistryError::ResourceNotFound(Str::new(id)))?;
		Ok(fs::read(path)?)
	}

	/// Renders the live/parked/disk transcript index used by `history://`.
	#[must_use]
	pub fn history_index(&self) -> String {
		let mut output = String::from(
			"| id | name | kind | status | parent/depth | definition | model → serving | task | last \
			 active |\n",
		);
		output.push_str("|---|---|---|---|---|---|---|---|---:|\n");
		let now = now_ms();
		for record in self.roster(false) {
			let age = now.saturating_sub(record.last_activity_ms) / 1_000;
			output.push_str(&format!(
				"| {} | {} | {} | {} | {}/{} | {} | {} → {} | {} | {}s |\n",
				record.id,
				record.name,
				record.kind,
				record.status,
				record.parent.as_deref().unwrap_or("-"),
				record.depth,
				record.definition.as_deref().unwrap_or("-"),
				record.model.as_deref().unwrap_or("-"),
				record.serving_model.as_deref().unwrap_or("-"),
				record.task.as_deref().unwrap_or("-"),
				age,
			));
		}
		output
	}

	fn record_discovery_diagnostic(&self, path: PathBuf, kind: DiscoveryDiagnosticKind) {
		let mut diagnostics = self.inner.diagnostics.lock();
		if diagnostics.len() == DISCOVERY_DIAGNOSTIC_CAPACITY {
			diagnostics.pop_front();
		}
		diagnostics.push_back(DiscoveryDiagnostic { path, kind });
		drop(diagnostics);
		self.bump_generation();
	}

	fn revision(&self, id: &str) -> Option<u64> {
		self.record(id).map(|(_, revision)| revision)
	}

	fn update(
		&self,
		id: &str,
		expected: Option<u64>,
		change: impl FnOnce(&mut AgentRecord) -> Result<(), RegistryError>,
	) -> Result<u64, RegistryError> {
		let mut records = self.inner.records.lock();
		let key = records
			.keys()
			.find(|candidate| candidate.as_str().eq_ignore_ascii_case(id))
			.cloned()
			.ok_or_else(|| RegistryError::NotFound(Str::new(id)))?;
		let entry = records.get_mut(&key).expect("key selected from same map");
		if let Some(expected) = expected
			&& expected != entry.revision
		{
			return Err(RegistryError::Revision { id: key, expected, actual: entry.revision });
		}
		change(&mut entry.record)?;
		entry.record.last_activity_ms = now_ms();
		entry.revision = entry.revision.saturating_add(1);
		let revision = entry.revision;
		drop(records);
		self.bump_generation();
		Ok(revision)
	}

	fn bump_generation(&self) {
		self
			.inner
			.generation
			.send_modify(|generation| *generation = generation.wrapping_add(1));
	}
}

/// Metadata projected alongside a canonical peer thread item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerMessage {
	/// Stable message identity.
	pub id:            Str,
	/// Stable sender agent identity.
	pub from:          Str,
	/// Address supplied by the sender.
	pub to:            Str,
	/// Plain-prose coordination text.
	pub text:          Str,
	/// Delivery boundary.
	pub mode:          DeliveryMode,
	/// Optional prior message being answered.
	pub reply_to:      Option<Str>,
	/// Sender wall-clock timestamp.
	pub sent_ms:       u64,
	/// Sender session identity.
	pub session_id:    Str,
	/// Whether the sender is synchronously awaiting a reply.
	pub expects_reply: bool,
}

/// One message leg's stable delivery state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveryReceipt {
	/// Resolved recipient identity or unresolved requested address.
	pub to:          Str,
	/// How the message reached the recipient.
	pub outcome:     Receipt,
	/// Read-only journal pointer supplied when a known recipient cannot run.
	pub history_uri: Option<Str>,
}

/// Routed event published once for a non-deduplicated delivery leg.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoutedEvent {
	/// Message with its stable id and exact visible body.
	pub message:       PeerMessage,
	/// Result for the resolved delivery leg.
	pub delivery:      DeliveryReceipt,
	/// Whether the main UI should show this body as a display-only observation.
	pub relay_to_main: bool,
}

/// A message accepted by a cold-revival owner.
#[derive(Clone, Debug)]
pub struct RevivalRequest {
	/// Parked recipient identity.
	pub recipient:         Str,
	/// Registry revision that selected this parked generation.
	pub registry_revision: u64,
	/// First message to inject after reconstruction.
	pub message:           PeerMessage,
}

/// Broker routing failure independent of per-recipient receipts.
#[derive(Debug, Error)]
pub enum BrokerError {
	/// Empty addresses are never broadcast implicitly.
	#[error("broker address is empty")]
	EmptyAddress,
}

struct WaitFilter {
	generation: u64,
	sender:     Option<Str>,
	reply_to:   Option<Str>,
}

struct InboxState {
	queue:       Mutex<VecDeque<PeerMessage>>,
	waiter:      Mutex<Option<WaitFilter>>,
	next_waiter: AtomicU64,
	notify:      tokio::sync::Notify,
}

impl InboxState {
	fn new() -> Self {
		Self {
			queue:       Mutex::new(VecDeque::with_capacity(MAILBOX_CAPACITY)),
			waiter:      Mutex::new(None),
			next_waiter: AtomicU64::new(1),
			notify:      Default::default(),
		}
	}

	fn push(&self, message: PeerMessage) {
		let mut queue = self.queue.lock();
		if queue.len() == MAILBOX_CAPACITY {
			queue.pop_front();
		}
		queue.push_back(message);
		drop(queue);
		self.notify.notify_waiters();
	}

	fn register_waiter(
		self: &Arc<Self>,
		sender: Option<&str>,
		reply_to: Option<&str>,
	) -> WaitRegistration {
		let generation = self.next_waiter.fetch_add(1, Ordering::Relaxed);
		*self.waiter.lock() = Some(WaitFilter {
			generation,
			sender: sender.map(Str::new),
			reply_to: reply_to.map(Str::new),
		});
		WaitRegistration { state: Arc::clone(self), generation }
	}

	fn deliver_waiter(&self, message: &PeerMessage) -> bool {
		let matches = self.waiter.lock().as_ref().is_some_and(|waiter| {
			waiter
				.sender
				.as_deref()
				.is_none_or(|sender| sender.eq_ignore_ascii_case(message.from.as_str()))
				&& waiter
					.reply_to
					.as_deref()
					.is_none_or(|reply| message.reply_to.as_deref() == Some(reply))
		});
		if matches {
			self.push(message.clone());
		}
		matches
	}

	fn matching(&self, sender: Option<&str>, reply_to: Option<&str>) -> Option<PeerMessage> {
		let mut queue = self.queue.lock();
		let index = queue.iter().position(|message| {
			sender.is_none_or(|sender| sender.eq_ignore_ascii_case(message.from.as_str()))
				&& reply_to.is_none_or(|reply| message.reply_to.as_deref() == Some(reply))
		})?;
		queue.remove(index)
	}

	fn read(&self, peek: bool) -> Vec<PeerMessage> {
		let mut queue = self.queue.lock();
		if peek {
			queue.iter().cloned().collect()
		} else {
			queue.drain(..).collect()
		}
	}
}

struct WaitRegistration {
	state:      Arc<InboxState>,
	generation: u64,
}

impl Drop for WaitRegistration {
	fn drop(&mut self) {
		let mut waiter = self.state.waiter.lock();
		if waiter
			.as_ref()
			.is_some_and(|waiter| waiter.generation == self.generation)
		{
			*waiter = None;
		}
	}
}

struct RegisteredNode {
	name:            Str,
	session:         Str,
	mailbox:         Option<MailboxSender>,
	inbox:           Arc<InboxState>,
	revival:         Option<flume::Sender<RevivalRequest>>,
	revival_pending: bool,
	idle:            bool,
}

struct DeliveryCache {
	entries: HashMap<(Str, Str), DeliveryReceipt>,
	order:   VecDeque<(Str, Str)>,
}

impl DeliveryCache {
	fn new() -> Self {
		Self {
			entries: HashMap::with_capacity(DELIVERY_DEDUP_CAPACITY),
			order:   VecDeque::with_capacity(DELIVERY_DEDUP_CAPACITY),
		}
	}

	fn get(&self, message: &str, recipient: &str) -> Option<DeliveryReceipt> {
		self
			.entries
			.iter()
			.find(|((cached_message, cached_recipient), _)| {
				cached_message == message && cached_recipient.eq_ignore_ascii_case(recipient)
			})
			.map(|(_, delivery)| delivery.clone())
	}

	fn insert(&mut self, message: &str, delivery: DeliveryReceipt) {
		let key = (Str::new(message), delivery.to.clone());
		if self.entries.contains_key(&key) {
			return;
		}
		if self.order.len() == DELIVERY_DEDUP_CAPACITY
			&& let Some(expired) = self.order.pop_front()
		{
			self.entries.remove(&expired);
		}
		self.order.push_back(key.clone());
		self.entries.insert(key, delivery);
	}
}

struct BrokerInner {
	project:    Str,
	nodes:      Mutex<HashMap<Str, RegisteredNode>>,
	deliveries: Mutex<DeliveryCache>,
	events:     tokio::sync::broadcast::Sender<RoutedEvent>,
	generation: tokio::sync::watch::Sender<u64>,
	registry:   AgentRegistry,
}

/// Core-owned project routing table backed by the process-global registry.
#[derive(Clone)]
pub struct Broker {
	inner: Arc<BrokerInner>,
}

impl Broker {
	/// Creates a broker using the process-global lifecycle registry.
	#[must_use]
	pub fn new(project: Str) -> Self {
		Self::with_registry(project, AgentRegistry::global().clone())
	}

	/// Creates a broker with an explicit registry.
	#[must_use]
	pub fn with_registry(project: Str, registry: AgentRegistry) -> Self {
		let (generation, _) = tokio::sync::watch::channel(0_u64);
		let (events, _) = tokio::sync::broadcast::channel(MAILBOX_CAPACITY);
		Self {
			inner: Arc::new(BrokerInner {
				project,
				nodes: Mutex::new(HashMap::new()),
				deliveries: Mutex::new(DeliveryCache::new()),
				events,
				generation,
				registry,
			}),
		}
	}

	/// Returns the registry shared with URL resolvers and roster projections.
	#[must_use]
	pub fn registry(&self) -> &AgentRegistry {
		&self.inner.registry
	}

	/// Subscribes to message-id-bearing delivery events.
	#[must_use]
	pub fn subscribe_routes(&self) -> tokio::sync::broadcast::Receiver<RoutedEvent> {
		self.inner.events.subscribe()
	}

	/// Registers a messageable live node and returns its bounded inbox.
	pub fn register(
		&self,
		node: &AgentNode,
		mailbox: MailboxSender,
	) -> Result<BrokerInbox, RegistryError> {
		self
			.inner
			.registry
			.register_node(node, RegistryStatus::Idle, None)?;
		let inbox = Arc::new(InboxState::new());
		self
			.inner
			.nodes
			.lock()
			.insert(node.id.clone(), RegisteredNode {
				name:            node.name.clone(),
				session:         node.session.clone(),
				mailbox:         Some(mailbox),
				inbox:           Arc::clone(&inbox),
				revival:         None,
				revival_pending: false,
				idle:            true,
			});
		self.bump_generation();
		Ok(BrokerInbox {
			owner:     node.id.clone(),
			state:     inbox,
			broker:    Arc::downgrade(&self.inner),
			roster:    self.inner.generation.subscribe(),
			lifecycle: self.inner.registry.subscribe(),
		})
	}

	/// Registers a parked record with a nonblocking cold-revival transport.
	pub fn register_parked(
		&self,
		mut record: AgentRecord,
		revival: flume::Sender<RevivalRequest>,
	) -> Result<(), RegistryError> {
		record.status = RegistryStatus::Parked;
		let expected = self.inner.registry.revision(record.id.as_str());
		self
			.inner
			.registry
			.compare_and_register(record.clone(), expected)?;
		self.inner.nodes.lock().insert(record.id, RegisteredNode {
			name:            record.name,
			session:         record.session,
			mailbox:         None,
			inbox:           Arc::new(InboxState::new()),
			revival:         Some(revival),
			revival_pending: false,
			idle:            true,
		});
		self.bump_generation();
		Ok(())
	}

	/// Attaches a reconstructed live mailbox without replacing historical data.
	pub fn attach_live(
		&self,
		id: &str,
		expected_revision: u64,
		mailbox: MailboxSender,
	) -> Result<BrokerInbox, RegistryError> {
		let mut nodes = self.inner.nodes.lock();
		let (_, node) =
			find_node_mut(&mut nodes, id).ok_or_else(|| RegistryError::NotFound(Str::new(id)))?;
		self
			.inner
			.registry
			.set_status(id, Some(expected_revision), RegistryStatus::Idle)?;
		node.mailbox = Some(mailbox);
		node.revival_pending = false;
		node.idle = true;
		let state = Arc::clone(&node.inbox);
		let owner = nodes
			.keys()
			.find(|key| key.as_str().eq_ignore_ascii_case(id))
			.cloned()
			.ok_or_else(|| RegistryError::NotFound(Str::new(id)))?;
		drop(nodes);
		self.bump_generation();
		Ok(BrokerInbox {
			owner,
			state,
			broker: Arc::downgrade(&self.inner),
			roster: self.inner.generation.subscribe(),
			lifecycle: self.inner.registry.subscribe(),
		})
	}

	/// Removes a terminal node from routing while retaining registry history.
	pub fn unregister(&self, id: &str) {
		let removed = {
			let mut nodes = self.inner.nodes.lock();
			let key = nodes
				.keys()
				.find(|key| key.as_str().eq_ignore_ascii_case(id))
				.cloned();
			key.is_some_and(|key| nodes.remove(&key).is_some())
		};
		if removed {
			self.bump_generation();
		}
	}

	/// Marks a live session parked and detaches its mailbox.
	pub fn park(&self, id: &str, expected_revision: u64) -> Result<(), RegistryError> {
		let mut nodes = self.inner.nodes.lock();
		let (_, node) =
			find_node_mut(&mut nodes, id).ok_or_else(|| RegistryError::NotFound(Str::new(id)))?;
		self
			.inner
			.registry
			.set_status(id, Some(expected_revision), RegistryStatus::Parked)?;
		node.mailbox = None;
		node.revival_pending = false;
		node.idle = true;
		drop(nodes);
		self.bump_generation();
		Ok(())
	}

	/// Sets whether a routed node is currently idle.
	pub fn set_idle(&self, id: &str, idle: bool) -> Result<(), RegistryError> {
		let mut nodes = self.inner.nodes.lock();
		let (_, node) =
			find_node_mut(&mut nodes, id).ok_or_else(|| RegistryError::NotFound(Str::new(id)))?;
		node.idle = idle;
		drop(nodes);
		self.inner.registry.set_status(
			id,
			None,
			if idle {
				RegistryStatus::Idle
			} else {
				RegistryStatus::Running
			},
		)?;
		Ok(())
	}

	/// Routes one message to every address match without waiting for recipient
	/// turns. Each match publishes exactly one message-id-bearing event.
	pub fn route(&self, message: PeerMessage) -> Result<SmallVec<DeliveryReceipt, 4>, BrokerError> {
		if message.to.is_empty() {
			return Err(BrokerError::EmptyAddress);
		}
		let mut deliveries = SmallVec::new();
		let mut lifecycle = SmallVec::<(Str, RegistryStatus), 4>::new();
		let mut events = SmallVec::<RoutedEvent, 4>::new();
		let mut nodes = self.inner.nodes.lock();
		let sender_is_main = self
			.inner
			.registry
			.record(message.from.as_str())
			.is_some_and(|(record, _)| record.kind == AgentKind::Main);
		let broadcast_has_main = is_broadcast(message.to.as_str())
			&& nodes.iter().any(|(id, node)| {
				matches_address(&self.inner.project, &message.to, id, node)
					&& self
						.inner
						.registry
						.record(id)
						.is_some_and(|(record, _)| record.kind == AgentKind::Main)
			});
		for (id, node) in nodes
			.iter_mut()
			.filter(|(id, node)| matches_address(&self.inner.project, &message.to, id, node))
		{
			if let Some(cached) = self.inner.deliveries.lock().get(message.id.as_str(), id) {
				deliveries.push(cached);
				continue;
			}
			let outcome = if node.inbox.deliver_waiter(&message) {
				Receipt::Injected
			} else if let Some(mailbox) = node.mailbox.as_ref() {
				let interrupt = Interrupt {
					class:  class(message.mode),
					item:   peer_item(&message),
					source: InterruptSource::Peer { from: message.from.clone() },
				};
				if mailbox.try_enqueue(interrupt).is_ok() {
					node.inbox.push(message.clone());
					if node.idle {
						Receipt::Woken
					} else {
						Receipt::Injected
					}
				} else {
					node.mailbox = None;
					node.inbox.push(message.clone());
					Receipt::Failed
				}
			} else if node.revival_pending {
				node.inbox.push(message.clone());
				Receipt::Revived
			} else if node.revival.as_ref().is_some_and(|revival| {
				self
					.inner
					.registry
					.record(id)
					.is_some_and(|(_, registry_revision)| {
						revival
							.try_send(RevivalRequest {
								recipient: id.clone(),
								registry_revision,
								message: message.clone(),
							})
							.is_ok()
					})
			}) {
				node.revival_pending = true;
				Receipt::Revived
			} else {
				node.inbox.push(message.clone());
				Receipt::Failed
			};
			if outcome != Receipt::Failed && outcome != Receipt::Revived {
				lifecycle.push((
					id.clone(),
					if outcome == Receipt::Woken || !node.idle {
						RegistryStatus::Running
					} else {
						RegistryStatus::Idle
					},
				));
			}
			let history_uri = (outcome == Receipt::Failed)
				.then(|| history_uri(&self.inner.registry, id))
				.flatten();
			let delivery = DeliveryReceipt { to: id.clone(), outcome, history_uri };
			self
				.inner
				.deliveries
				.lock()
				.insert(message.id.as_str(), delivery.clone());
			let recipient_is_main = self
				.inner
				.registry
				.record(id)
				.is_some_and(|(record, _)| record.kind == AgentKind::Main);
			events.push(RoutedEvent {
				message:       message.clone(),
				delivery:      delivery.clone(),
				relay_to_main: outcome != Receipt::Failed
					&& !sender_is_main
					&& !recipient_is_main
					&& !broadcast_has_main,
			});
			deliveries.push(delivery);
		}
		drop(nodes);
		for (id, status) in lifecycle {
			let _ = self.inner.registry.set_status(id.as_str(), None, status);
		}
		if deliveries.is_empty() {
			let history_uri = history_uri(&self.inner.registry, message.to.as_str());
			let delivery =
				DeliveryReceipt { to: message.to.clone(), outcome: Receipt::Failed, history_uri };
			let cached = self
				.inner
				.deliveries
				.lock()
				.get(message.id.as_str(), delivery.to.as_str());
			if let Some(cached) = cached {
				deliveries.push(cached);
			} else {
				self
					.inner
					.deliveries
					.lock()
					.insert(message.id.as_str(), delivery.clone());
				events.push(RoutedEvent {
					message:       message.clone(),
					delivery:      delivery.clone(),
					relay_to_main: false,
				});
				deliveries.push(delivery);
			}
		}
		for event in events {
			let _ = self.inner.events.send(event);
		}
		Ok(deliveries)
	}

	/// Routes a message and returns its compact outcome vocabulary.
	pub fn send(&self, message: PeerMessage) -> Result<SmallVec<Receipt, 4>, BrokerError> {
		Ok(self
			.route(message)?
			.into_iter()
			.map(|delivery| delivery.outcome)
			.collect())
	}

	/// Drains or peeks at one agent's bounded FIFO inbox.
	pub fn inbox(&self, id: &str, peek: bool) -> Result<Vec<PeerMessage>, RegistryError> {
		let nodes = self.inner.nodes.lock();
		let (_, node) = find_node(&nodes, id).ok_or_else(|| RegistryError::NotFound(Str::new(id)))?;
		Ok(node.inbox.read(peek))
	}

	/// Returns one agent's unread bounded-inbox count.
	pub fn unread_count(&self, id: &str) -> Result<usize, RegistryError> {
		let nodes = self.inner.nodes.lock();
		let (_, node) = find_node(&nodes, id).ok_or_else(|| RegistryError::NotFound(Str::new(id)))?;
		Ok(node.inbox.queue.lock().len())
	}

	/// Lists currently messageable node identities for the project or a session.
	#[must_use]
	pub fn peers(&self, session: Option<&str>) -> SmallVec<Str, 4> {
		self
			.inner
			.nodes
			.lock()
			.iter()
			.filter(|&(_id, node)| session.is_none() || session == Some(node.session.as_str()))
			.map(|(id, _node)| id.clone())
			.collect()
	}

	fn bump_generation(&self) {
		self
			.inner
			.generation
			.send_modify(|generation| *generation = generation.wrapping_add(1));
	}
}

/// Why a blocking wait ended without a matching message.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum WaitError {
	/// The requested deadline elapsed.
	#[error("IRC wait timed out")]
	Timeout,
	/// The awaited peer died or no other live peers remain.
	#[error("IRC wait aborted because the peer is no longer live")]
	PeerDead,
	/// The owning broker was dropped.
	#[error("IRC broker is no longer available")]
	BrokerGone,
}

/// Receiver used for bounded inbox access and liveness-aware waits.
pub struct BrokerInbox {
	owner:     Str,
	state:     Arc<InboxState>,
	broker:    Weak<BrokerInner>,
	roster:    tokio::sync::watch::Receiver<u64>,
	lifecycle: tokio::sync::watch::Receiver<u64>,
}

impl BrokerInbox {
	/// Waits indefinitely for a matching delivery or liveness abort.
	pub async fn wait_for(
		&mut self,
		sender: Option<&str>,
		reply_to: Option<&str>,
	) -> Option<PeerMessage> {
		self
			.wait_for_timeout(sender, reply_to, None)
			.await
			.ok()
			.flatten()
	}

	/// Waits for a matching delivery with an optional deadline. Unmatched
	/// messages remain FIFO-ordered for later inbox reads or waits.
	pub async fn wait_for_timeout(
		&mut self,
		sender: Option<&str>,
		reply_to: Option<&str>,
		timeout: Option<Duration>,
	) -> Result<Option<PeerMessage>, WaitError> {
		if let Some(message) = self.state.matching(sender, reply_to) {
			return Ok(Some(message));
		}
		let _registration = self.state.register_waiter(sender, reply_to);
		let deadline = timeout.map(|duration| tokio::time::Instant::now() + duration);
		loop {
			let notified = self.state.notify.notified();
			if let Some(message) = self.state.matching(sender, reply_to) {
				return Ok(Some(message));
			}
			let broker = self.broker.upgrade().ok_or(WaitError::BrokerGone)?;
			if !peer_is_live(&broker, self.owner.as_str(), sender) {
				return Err(WaitError::PeerDead);
			}
			if let Some(deadline) = deadline {
				tokio::select! {
					() = notified => {},
					changed = self.roster.changed() => {
						if changed.is_err() {
							return Err(WaitError::BrokerGone);
						}
					},
					changed = self.lifecycle.changed() => {
						if changed.is_err() {
							return Err(WaitError::BrokerGone);
						}
					},
					() = tokio::time::sleep_until(deadline) => return Err(WaitError::Timeout),
				}
			} else {
				tokio::select! {
					() = notified => {},
					changed = self.roster.changed() => {
						if changed.is_err() {
							return Err(WaitError::BrokerGone);
						}
					},
					changed = self.lifecycle.changed() => {
						if changed.is_err() {
							return Err(WaitError::BrokerGone);
						}
					},
				}
			}
		}
	}

	/// Drains or peeks at every unread message without double delivery.
	pub fn inbox(&self, peek: bool) -> Vec<PeerMessage> {
		self.state.read(peek)
	}

	/// Returns the unread FIFO count.
	#[must_use]
	pub fn unread_count(&self) -> usize {
		self.state.queue.lock().len()
	}
}

fn find_record<'a>(
	records: &'a HashMap<Str, RegistryEntry>,
	id: &str,
) -> Option<(&'a Str, &'a RegistryEntry)> {
	records.iter().find(|(candidate, entry)| {
		candidate.as_str().eq_ignore_ascii_case(id)
			|| entry.record.name.as_str().eq_ignore_ascii_case(id)
	})
}

fn find_node<'a>(
	nodes: &'a HashMap<Str, RegisteredNode>,
	id: &str,
) -> Option<(&'a Str, &'a RegisteredNode)> {
	nodes
		.iter()
		.find(|(candidate, _)| candidate.as_str().eq_ignore_ascii_case(id))
}

fn find_node_mut<'a>(
	nodes: &'a mut HashMap<Str, RegisteredNode>,
	id: &str,
) -> Option<(&'a Str, &'a mut RegisteredNode)> {
	nodes
		.iter_mut()
		.find(|(candidate, _)| candidate.as_str().eq_ignore_ascii_case(id))
}

fn is_broadcast(address: &str) -> bool {
	address == "all" || address == "project:all" || address.starts_with("session:")
}

fn matches_address(project: &str, address: &str, id: &str, node: &RegisteredNode) -> bool {
	address.eq_ignore_ascii_case(id)
		|| address.eq_ignore_ascii_case(node.name.as_str())
		|| address == "all"
		|| (address == "project:all" && !project.is_empty())
		|| address
			.strip_prefix("session:")
			.is_some_and(|session| session == node.session.as_str())
}

fn peer_is_live(broker: &BrokerInner, owner: &str, sender: Option<&str>) -> bool {
	let nodes = broker.nodes.lock();
	match sender {
		Some(sender) => find_node(&nodes, sender).is_some(),
		None => nodes.keys().any(|id| id.as_str() != owner),
	}
}

fn history_uri(registry: &AgentRegistry, id: &str) -> Option<Str> {
	registry
		.record(id)
		.filter(|(record, _)| record.transcript.is_some())
		.map(|(record, _)| omp_core::sf!("history://{}", record.id))
}

const fn class(mode: DeliveryMode) -> InterruptClass {
	match mode {
		DeliveryMode::Aside | DeliveryMode::Steer => InterruptClass::Immediate,
		DeliveryMode::NextTurn => InterruptClass::TurnBoundary,
	}
}

/// Encodes a peer message as the canonical thread item journaled by the loop.
#[must_use]
pub fn peer_item(message: &PeerMessage) -> Item {
	let mut text = String::new();
	crate::prompt_assets::render_parent_irc(&mut text, message.from.as_str(), message.text.as_str());
	Item {
		seq:           0,
		created_at_ms: message.sent_ms,
		kind:          Some(item::Kind::Message(ThreadMessage {
			role:  Role::User as i32,
			parts: vec![Part { kind: Some(part::Kind::Text(text)) }],
		})),
		props:         None,
	}
}

/// Returns the current epoch milliseconds for caller-created messages.
#[must_use]
pub fn now_ms() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or_default()
		.as_millis() as u64
}

fn sanitize_activity(activity: &str) -> Str {
	let mut sanitized = String::with_capacity(activity.len().min(ACTIVITY_MAX_CHARS));
	for character in activity.chars() {
		if sanitized.chars().count() == ACTIVITY_MAX_CHARS {
			break;
		}
		if character == '\n' || character == '\r' || character.is_control() {
			if !sanitized.ends_with(' ') {
				sanitized.push(' ');
			}
		} else {
			sanitized.push(character);
		}
	}
	Str::new(sanitized.trim())
}

enum ColdScan {
	Record(AgentRecord),
	Skipped(DiscoveryDiagnosticKind),
}

fn cold_record(path: &Path) -> Result<ColdScan, RegistryError> {
	use omp_storage::transcript::{Kind, Msg, Patch, UserBlock};

	let file = fs::File::open(path)?;
	let mut reader =
		std::io::BufReader::new(file).take(u64::try_from(PREFIX_MAX_BYTES).unwrap_or(u64::MAX));
	let mut line = String::new();
	if reader.read_line(&mut line)? == 0 {
		return Ok(ColdScan::Skipped(DiscoveryDiagnosticKind::Incomplete));
	}
	let Ok(header) = omp_storage::transcript::read_header(line.as_bytes()) else {
		return Ok(ColdScan::Skipped(if reader.limit() == 0 {
			DiscoveryDiagnosticKind::Incomplete
		} else {
			DiscoveryDiagnosticKind::Corrupt
		}));
	};

	let mut definition = None;
	let mut parent = None;
	let mut display_name = None;
	let mut depth = 1;
	let mut model = None;
	let mut serving_model = None;
	let mut task = None;
	let mut history = AgentHistory::default();
	let mut last_activity_ms = header.created;
	let mut saw_revival = false;

	for _ in 0..PREFIX_MAX_LINES {
		line.clear();
		if reader.read_line(&mut line)? == 0 {
			break;
		}
		let Ok(event) = omp_storage::transcript::read_line(line.as_bytes()) else {
			return Ok(ColdScan::Skipped(if reader.limit() == 0 {
				DiscoveryDiagnosticKind::Incomplete
			} else {
				DiscoveryDiagnosticKind::Corrupt
			}));
		};
		last_activity_ms = last_activity_ms.max(event.ts);
		match event.kind {
			Kind::Init { agent, revival: Some(revival), .. } => {
				saw_revival = true;
				parent = agent;
				if !revival.parent_id.is_empty() {
					parent = Some(revival.parent_id);
				}
				if !revival.display_name.is_empty() {
					display_name = Some(revival.display_name);
				}
				depth = revival.depth;
				definition = Some(revival.definition);
				model = Some(revival.model_role);
				serving_model = revival.serving_model.as_ref().map(model_label);
			},
			Kind::Msg(Msg::User { content, synthetic: false, .. }) if task.is_none() => {
				task = content.into_iter().find_map(|block| match block {
					UserBlock::Text { text } if !text.trim().is_empty() => {
						Some(normalize_task_summary(text.as_str()))
					},
					UserBlock::Text { .. } | UserBlock::Image { .. } => None,
				});
			},
			Kind::Item(record) if task.is_none() => {
				task = task_summary_from_item(&record.item);
			},
			Kind::TurnInput(input) if task.is_none() => {
				task = task_summary_from_item(&input.item);
			},
			Kind::Msg(Msg::Assistant { model: served, usage, timing, .. }) => {
				history.requests = history.requests.saturating_add(1);
				history.input_tokens = history
					.input_tokens
					.saturating_add(usage.input)
					.saturating_add(usage.cache_read);
				history.output_tokens = history.output_tokens.saturating_add(usage.output);
				history.duration_ms = history.duration_ms.saturating_add(timing.duration_ms);
				serving_model = Some(model_label(&served));
			},
			Kind::Infer { model: Patch::Set(change), .. } => {
				model = Some(model_label(&change.model));
			},
			Kind::ChildLifecycle(lifecycle) if lifecycle.child_id == header.id.0 => {
				if let Some(kind) = lifecycle
					.terminal_status
					.as_deref()
					.and_then(|status| status.parse().ok())
				{
					history.terminal = Some(crate::SubagentTerminalStatus {
						kind,
						summary: lifecycle.terminal_status.unwrap_or_default(),
						disposition: crate::SubagentDisposition::default(),
					});
				}
			},
			Kind::EntryUndecodable(_) => {
				return Ok(ColdScan::Skipped(DiscoveryDiagnosticKind::Corrupt));
			},
			_ => {},
		}
	}
	if !saw_revival {
		return Ok(ColdScan::Skipped(DiscoveryDiagnosticKind::Incomplete));
	}

	let id = header.id.0.clone();
	let name = path
		.file_stem()
		.and_then(std::ffi::OsStr::to_str)
		.map_or_else(|| id.clone(), Str::new);
	let name = display_name.unwrap_or(name);
	Ok(ColdScan::Record(AgentRecord {
		id,
		name,
		kind: AgentKind::Subagent,
		parent,
		session: header.id.0,
		depth,
		status: RegistryStatus::Parked,
		activity: Default::default(),
		last_activity_ms,
		transcript: Some(path.to_path_buf()),
		definition,
		model,
		serving_model,
		task,
		history,
	}))
}

fn task_summary_from_item(item: &Item) -> Option<Str> {
	let item::Kind::Message(message) = item.kind.as_ref()? else {
		return None;
	};
	if message.role != Role::User as i32 {
		return None;
	}
	message
		.parts
		.iter()
		.find_map(|part| match part.kind.as_ref()? {
			part::Kind::Text(text) if !text.trim().is_empty() => Some(normalize_task_summary(text)),
			_ => None,
		})
}

fn model_label(model: &omp_storage::transcript::ModelRef) -> Str {
	omp_core::sf!("{}/{}", model.provider.0, model.model.0)
}

fn normalize_task_summary(task: &str) -> Str {
	let mut summary = String::with_capacity(task.len().min(TASK_SUMMARY_MAX_CHARS));
	for character in task.chars().take(TASK_SUMMARY_MAX_CHARS) {
		if character == '\n' || character == '\r' || character.is_control() {
			if !summary.ends_with(' ') {
				summary.push(' ');
			}
		} else {
			summary.push(character);
		}
	}
	Str::new(summary.trim())
}

fn bounded_json_query(bytes: &[u8], query: &str) -> Result<Vec<u8>, RegistryError> {
	use hifijson::token::Lex as _;
	use jaq_core::{
		Ctx, RcIter,
		compile::Compiler,
		load::{Arena, File, Loader},
	};
	use jaq_json::Val;

	if bytes.len() > QUERY_MAX_BYTES
		|| query.chars().count() > QUERY_MAX_CHARS
		|| !query_is_safe(query)
	{
		return Err(RegistryError::QueryLimit);
	}
	serde_json::from_slice::<serde_json::Value>(bytes).map_err(RegistryError::InvalidJson)?;
	let arena = Arena::default();
	let loader = Loader::new(jaq_std::defs().chain(jaq_json::defs()));
	let modules = loader
		.load(&arena, File { path: (), code: query })
		.map_err(|_| RegistryError::InvalidQuery)?;
	let filter = Compiler::default()
		.with_funs(jaq_std::funs().chain(jaq_json::funs()))
		.compile(modules)
		.map_err(|_| RegistryError::InvalidQuery)?;

	let mut lexer = hifijson::SliceLexer::new(bytes);
	let token = lexer
		.ws_token()
		.expect("serde-validated JSON has one token");
	let input = Val::parse(token, &mut lexer).map_err(|_| RegistryError::QueryLimit)?;
	let empty = Box::new(core::iter::empty()) as Box<dyn Iterator<Item = Result<Val, String>>>;
	let inputs = RcIter::new(empty);
	let ctx = Ctx::new(Vec::new(), &inputs);
	let started = Instant::now();
	let mut values = filter.run((ctx, input));
	let first = values
		.next()
		.ok_or(RegistryError::QueryEmpty)?
		.map_err(|_| RegistryError::InvalidQuery)?;
	if started.elapsed() > QUERY_MAX_DURATION {
		return Err(RegistryError::QueryLimit);
	}
	if values.next().is_some() {
		return Err(RegistryError::QueryMultiple);
	}
	let output = first.to_string().into_bytes();
	if output.len() > QUERY_MAX_BYTES || started.elapsed() > QUERY_MAX_DURATION {
		return Err(RegistryError::QueryLimit);
	}
	let value =
		serde_json::from_slice::<serde_json::Value>(&output).map_err(RegistryError::InvalidJson)?;
	if json_depth(&value, 0) > QUERY_MAX_DEPTH {
		return Err(RegistryError::QueryLimit);
	}
	Ok(output)
}

fn query_is_safe(query: &str) -> bool {
	const DENIED: &[&str] = &[
		"debug",
		"env",
		"foreach",
		"gsub",
		"halt",
		"halt_error",
		"input",
		"inputs",
		"match",
		"range",
		"recurse",
		"reduce",
		"repeat",
		"scan",
		"stderr",
		"sub",
		"test",
		"until",
		"while",
	];
	let mut quoted = false;
	let mut escaped = false;
	let mut field = false;
	let mut previous = None;
	let mut token = String::new();
	for character in query.chars().chain(core::iter::once(' ')) {
		if quoted {
			if escaped {
				escaped = false;
			} else if character == '\\' {
				escaped = true;
			} else if character == '"' {
				quoted = false;
			}
			continue;
		}
		if character == '"' {
			quoted = true;
			token.clear();
		} else if character.is_ascii_alphanumeric() || character == '_' {
			if token.is_empty() {
				field = previous == Some('.');
			}
			token.push(character);
		} else {
			if !field && DENIED.contains(&token.as_str()) {
				return false;
			}
			token.clear();
			if !character.is_whitespace() {
				previous = Some(character);
			}
		}
	}
	!quoted
}

fn json_depth(value: &serde_json::Value, depth: usize) -> usize {
	match value {
		serde_json::Value::Array(values) => values
			.iter()
			.map(|value| json_depth(value, depth.saturating_add(1)))
			.max()
			.unwrap_or(depth),
		serde_json::Value::Object(values) => values
			.values()
			.map(|value| json_depth(value, depth.saturating_add(1)))
			.max()
			.unwrap_or(depth),
		_ => depth,
	}
}

fn valid_artifact_component(value: &str) -> bool {
	!value.is_empty()
		&& value
			.bytes()
			.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
	use omp_core::sf;

	use super::*;
	use crate::{AgentTree, Budget, Mailbox};

	fn node(tree: &AgentTree, id: &str, name: &str) -> Arc<AgentNode> {
		tree
			.register(
				id.into(),
				name.into(),
				AgentKind::Subagent,
				None,
				"session".into(),
				Budget::default(),
			)
			.expect("node")
	}

	fn message(from: &str, to: &str, index: usize) -> PeerMessage {
		PeerMessage {
			id:            sf!("message-{index}"),
			from:          from.into(),
			to:            to.into(),
			text:          sf!("body-{index}"),
			mode:          DeliveryMode::Aside,
			reply_to:      None,
			sent_ms:       now_ms(),
			session_id:    "session".into(),
			expects_reply: false,
		}
	}

	#[test]
	fn registry_cas_ttl_and_parking_preserve_identity() {
		let registry = AgentRegistry::new();
		let tree = AgentTree::standard(2);
		let node = node(&tree, "worker", "Worker");
		let revision = registry
			.register_node(&node, RegistryStatus::Idle, None)
			.expect("register");
		assert!(matches!(
			registry.set_status("worker", Some(revision + 1), RegistryStatus::Running),
			Err(RegistryError::Revision { .. })
		));
		let parked = registry.park_expired(now_ms() + 10_000, Duration::from_secs(1));
		assert_eq!(parked.len(), 1);
		assert_eq!(parked[0].record.status, RegistryStatus::Parked);
		registry
			.register_node(&node, RegistryStatus::Idle, None)
			.expect("revive parked identity");
		assert_eq!(
			registry
				.record("Worker")
				.expect("alias remains reserved")
				.0
				.id,
			node.id
		);
	}

	#[test]
	fn mailbox_is_fifo_capped_and_receipts_are_fire_and_forget() {
		let registry = AgentRegistry::new();
		let broker = Broker::with_registry("project".into(), registry);
		let tree = AgentTree::standard(2);
		let worker = node(&tree, "worker", "Worker");
		let mailbox = Mailbox::new();
		let inbox = broker
			.register(&worker, mailbox.sender())
			.expect("register");
		for index in 0..105 {
			assert_eq!(
				broker
					.send(message("Main", "worker", index))
					.expect("send")
					.as_slice(),
				[Receipt::Woken]
			);
		}
		assert_eq!(inbox.unread_count(), MAILBOX_CAPACITY);
		let messages = inbox.inbox(false);
		assert_eq!(messages.first().expect("first retained").id.as_str(), "message-5");
		assert_eq!(messages.last().expect("last retained").id.as_str(), "message-104");
		assert_eq!(inbox.unread_count(), 0);
	}

	#[tokio::test]
	async fn wait_preserves_unmatched_messages_and_aborts_on_peer_death() {
		let registry = AgentRegistry::new();
		let broker = Broker::with_registry("project".into(), registry.clone());
		let tree = AgentTree::standard(2);
		let owner = node(&tree, "owner", "Owner");
		let peer = node(&tree, "peer", "Peer");
		let owner_mailbox = Mailbox::new();
		let peer_mailbox = Mailbox::new();
		let mut inbox = broker
			.register(&owner, owner_mailbox.sender())
			.expect("owner");
		broker.register(&peer, peer_mailbox.sender()).expect("peer");
		broker
			.send(message("other", "owner", 0))
			.expect("unmatched");
		broker.send(message("peer", "owner", 1)).expect("matched");
		let matched = inbox
			.wait_for_timeout(Some("peer"), None, Some(Duration::from_secs(1)))
			.await
			.expect("wait")
			.expect("message");
		assert_eq!(matched.id.as_str(), "message-1");
		assert_eq!(inbox.inbox(true)[0].id.as_str(), "message-0");
		broker.unregister("peer");
		assert_eq!(
			inbox
				.wait_for_timeout(Some("peer"), None, Some(Duration::from_secs(1)))
				.await,
			Err(WaitError::PeerDead)
		);
	}
}
