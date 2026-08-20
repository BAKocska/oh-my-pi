//! Process-global agent registry and project-scoped IRC routing.

use std::{
	collections::{HashMap, VecDeque},
	fs,
	io::BufRead as _,
	path::{Path, PathBuf},
	sync::{Arc, LazyLock, Weak},
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use omp_core::Str;
use omp_proto::thread::v1::{Item, Message as ThreadMessage, Part, Role, item, part};
use parking_lot::Mutex;
use smallvec::SmallVec;
use thiserror::Error;

use crate::{AgentKind, AgentNode, Interrupt, InterruptClass, InterruptSource, MailboxSender};

const MAILBOX_CAPACITY: usize = 100;
const ACTIVITY_MAX_CHARS: usize = 80;

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
	/// The session is disposed but its transcript can revive it.
	Parked  = 2,
	/// A tombstone permanently prevents revival.
	Aborted = 3,
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
	/// Effective model selector, including an explicit thinking suffix.
	pub model:            Option<Str>,
	/// Normalized task summary for historical rosters.
	pub task:             Option<Str>,
	/// Historical execution and merge facts.
	pub history:          AgentHistory,
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
	/// A tombstoned id cannot be registered or revived.
	#[error("agent {0} is aborted and cannot be revived")]
	Tombstoned(Str),
	/// The requested agent or history artifact does not exist.
	#[error("agent resource was not found: {0}")]
	ResourceNotFound(Str),
	/// A transcript or artifact could not be read.
	#[error("agent resource I/O failed: {0}")]
	Io(#[from] std::io::Error),
}

struct RegistryEntry {
	record:   AgentRecord,
	revision: u64,
}

struct RegistryInner {
	records:    Mutex<HashMap<Str, RegistryEntry>>,
	generation: tokio::sync::watch::Sender<u64>,
}

/// Process-global CAS registry for live, parked, and aborted agents.
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
		Self { inner: Arc::new(RegistryInner { records: Mutex::new(HashMap::new()), generation }) }
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
		match records.get(&record.id) {
			Some(entry) if entry.record.status == RegistryStatus::Aborted => {
				return Err(RegistryError::Tombstoned(record.id));
			},
			Some(entry) if expected != Some(entry.revision) => {
				return Err(RegistryError::Revision {
					id:       record.id,
					expected: expected.unwrap_or(0),
					actual:   entry.revision,
				});
			},
			None if expected.is_some() => return Err(RegistryError::NotFound(record.id)),
			_ => {},
		}
		record.activity = sanitize_activity(record.activity.as_str());
		record.last_activity_ms = now_ms();
		let revision = records
			.get(&record.id)
			.map_or(1, |entry| entry.revision.saturating_add(1));
		records.insert(record.id.clone(), RegistryEntry { record, revision });
		drop(records);
		self.bump_generation();
		Ok(revision)
	}

	/// Registers a live tree node, replacing only a non-aborted prior
	/// generation.
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

	/// CAS-updates one lifecycle state.
	pub fn set_status(
		&self,
		id: &str,
		expected: Option<u64>,
		status: RegistryStatus,
	) -> Result<u64, RegistryError> {
		self.update(id, expected, |record| {
			if record.status == RegistryStatus::Aborted && status != RegistryStatus::Aborted {
				return Err(RegistryError::Tombstoned(record.id.clone()));
			}
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

	/// Parks idle records whose TTL elapsed and returns records whose owners
	/// should dispose their live sessions.
	pub fn park_expired(&self, now: u64, ttl: Duration) -> Vec<AgentRecord> {
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
				parked.push(entry.record.clone());
			}
		}
		drop(records);
		if !parked.is_empty() {
			self.bump_generation();
		}
		parked
	}

	/// Writes a transcript-adjacent tombstone and permanently aborts an agent.
	pub fn abort(&self, id: &str) -> Result<u64, RegistryError> {
		let transcript = self.record(id).and_then(|(record, _)| record.transcript);
		if let Some(path) = transcript {
			fs::write(tombstone_path(&path), id.as_bytes())?;
		}
		self.set_status(id, None, RegistryStatus::Aborted)
	}

	/// Imports valid transcript headers as parked records. Corrupt files,
	/// header-only files, and explicit tombstones are skipped.
	pub fn discover_transcripts(&self, directory: &Path) -> Result<usize, RegistryError> {
		let mut imported = 0;
		for entry in fs::read_dir(directory)? {
			let path = entry?.path();
			if path.extension().and_then(std::ffi::OsStr::to_str) != Some("jsonl")
				|| tombstone_path(&path).exists()
			{
				continue;
			}
			let Some(record) = cold_record(&path)? else {
				continue;
			};
			let expected = self.revision(record.id.as_str());
			if self.compare_and_register(record, expected).is_ok() {
				imported += 1;
			}
		}
		Ok(imported)
	}

	/// Resolves `agent://<id>` or `agent://<id>/<child>` to immutable artifact
	/// bytes. Child names become dot-separated artifact stems.
	pub fn resolve_agent(&self, resource: &str) -> Result<Vec<u8>, RegistryError> {
		let resource = resource.trim_start_matches('/');
		let (id, child) = resource.split_once('/').unwrap_or((resource, ""));
		let (record, _) = self
			.record(id)
			.ok_or_else(|| RegistryError::ResourceNotFound(Str::new(id)))?;
		let path = if child.is_empty() {
			record.history.output_path
		} else {
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
			Some(parent.join(format!("{}.{}.md", record.id, child)))
		}
		.ok_or_else(|| RegistryError::ResourceNotFound(Str::new(resource)))?;
		Ok(fs::read(path)?)
	}

	/// Resolves `history://` to a roster index and `history://<id>` to immutable
	/// transcript bytes.
	pub fn resolve_history(&self, resource: &str) -> Result<Vec<u8>, RegistryError> {
		let id = resource.trim_matches('/');
		if id.is_empty() {
			return Ok(self.history_index().into_bytes());
		}
		let (record, _) = self
			.record(id)
			.ok_or_else(|| RegistryError::ResourceNotFound(Str::new(id)))?;
		let path = record
			.transcript
			.ok_or_else(|| RegistryError::ResourceNotFound(Str::new(id)))?;
		Ok(fs::read(path)?)
	}

	/// Renders the live/parked/disk transcript index used by `history://`.
	#[must_use]
	pub fn history_index(&self) -> String {
		let mut output =
			String::from("| id | name | kind | status | parent | model | last active |\n");
		output.push_str("|---|---|---|---|---|---|---:|\n");
		let now = now_ms();
		for record in self.roster(false) {
			let age = now.saturating_sub(record.last_activity_ms) / 1_000;
			output.push_str(&format!(
				"| {} | {} | {} | {} | {} | {} | {}s |\n",
				record.id,
				record.name,
				record.kind,
				record.status,
				record.parent.as_deref().unwrap_or("-"),
				record.model.as_deref().unwrap_or("-"),
				age,
			));
		}
		output
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
	pub id:         Str,
	/// Stable sender agent identity.
	pub from:       Str,
	/// Address supplied by the sender.
	pub to:         Str,
	/// Plain-prose coordination text.
	pub text:       Str,
	/// Delivery boundary.
	pub mode:       DeliveryMode,
	/// Optional prior message being answered.
	pub reply_to:   Option<Str>,
	/// Sender wall-clock timestamp.
	pub sent_ms:    u64,
	/// Sender session identity.
	pub session_id: Str,
}

/// A message accepted by a cold-revival owner.
#[derive(Clone, Debug)]
pub struct RevivalRequest {
	/// Parked recipient identity.
	pub recipient: Str,
	/// First message to inject after reconstruction.
	pub message:   PeerMessage,
}

/// Broker routing failure independent of per-recipient receipts.
#[derive(Debug, Error)]
pub enum BrokerError {
	/// Empty addresses are never broadcast implicitly.
	#[error("broker address is empty")]
	EmptyAddress,
}

struct InboxState {
	queue:  Mutex<VecDeque<PeerMessage>>,
	notify: tokio::sync::Notify,
}

impl InboxState {
	fn new() -> Self {
		Self {
			queue:  Mutex::new(VecDeque::with_capacity(MAILBOX_CAPACITY)),
			notify: Default::default(),
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

struct RegisteredNode {
	name:    Str,
	session: Str,
	mailbox: Option<MailboxSender>,
	inbox:   Arc<InboxState>,
	revival: Option<flume::Sender<RevivalRequest>>,
	idle:    bool,
}

struct BrokerInner {
	project:    Str,
	nodes:      Mutex<HashMap<Str, RegisteredNode>>,
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
		Self {
			inner: Arc::new(BrokerInner {
				project,
				nodes: Mutex::new(HashMap::new()),
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
				name:    node.name.clone(),
				session: node.session.clone(),
				mailbox: Some(mailbox),
				inbox:   Arc::clone(&inbox),
				revival: None,
				idle:    true,
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
			name:    record.name,
			session: record.session,
			mailbox: None,
			inbox:   Arc::new(InboxState::new()),
			revival: Some(revival),
			idle:    true,
		});
		self.bump_generation();
		Ok(())
	}

	/// Attaches a reconstructed live mailbox without replacing historical data.
	pub fn attach_live(
		&self,
		id: &str,
		mailbox: MailboxSender,
	) -> Result<BrokerInbox, RegistryError> {
		let mut nodes = self.inner.nodes.lock();
		let (_, node) =
			find_node_mut(&mut nodes, id).ok_or_else(|| RegistryError::NotFound(Str::new(id)))?;
		node.mailbox = Some(mailbox);
		node.idle = true;
		let state = Arc::clone(&node.inbox);
		let owner = nodes
			.keys()
			.find(|key| key.as_str().eq_ignore_ascii_case(id))
			.cloned()
			.ok_or_else(|| RegistryError::NotFound(Str::new(id)))?;
		drop(nodes);
		self
			.inner
			.registry
			.set_status(id, None, RegistryStatus::Idle)?;
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
	pub fn park(&self, id: &str) -> Result<(), RegistryError> {
		let mut nodes = self.inner.nodes.lock();
		let (_, node) =
			find_node_mut(&mut nodes, id).ok_or_else(|| RegistryError::NotFound(Str::new(id)))?;
		node.mailbox = None;
		node.idle = true;
		drop(nodes);
		self
			.inner
			.registry
			.set_status(id, None, RegistryStatus::Parked)?;
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
	/// turns. Each match produces exactly one receipt.
	pub fn send(&self, message: PeerMessage) -> Result<SmallVec<Receipt, 4>, BrokerError> {
		if message.to.is_empty() {
			return Err(BrokerError::EmptyAddress);
		}
		let mut receipts = SmallVec::new();
		let mut lifecycle = SmallVec::<(Str, RegistryStatus), 4>::new();
		let mut nodes = self.inner.nodes.lock();
		for (id, node) in nodes
			.iter_mut()
			.filter(|(id, node)| matches_address(&self.inner.project, &message.to, id, node))
		{
			if self
				.inner
				.registry
				.record(id)
				.is_some_and(|(record, _)| record.status == RegistryStatus::Aborted)
			{
				receipts.push(Receipt::Failed);
				continue;
			}
			if let Some(mailbox) = node.mailbox.as_ref() {
				let interrupt = Interrupt {
					class:  class(message.mode),
					item:   peer_item(&message),
					source: InterruptSource::Peer { from: message.from.clone() },
				};
				if mailbox.try_enqueue(interrupt).is_ok() {
					node.inbox.push(message.clone());
					receipts.push(if node.idle {
						Receipt::Woken
					} else {
						Receipt::Injected
					});
					lifecycle.push((
						id.clone(),
						if node.idle {
							RegistryStatus::Idle
						} else {
							RegistryStatus::Running
						},
					));
					continue;
				}
				node.mailbox = None;
			}
			if node.revival.as_ref().is_some_and(|revival| {
				revival
					.try_send(RevivalRequest { recipient: id.clone(), message: message.clone() })
					.is_ok()
			}) {
				node.inbox.push(message.clone());
				receipts.push(Receipt::Revived);
				lifecycle.push((id.clone(), RegistryStatus::Running));
			} else {
				// A failed handoff remains available to inbox/wait, bounded by
				// the same FIFO cap. It is never injected a second time.
				node.inbox.push(message.clone());
				receipts.push(Receipt::Failed);
			}
		}
		drop(nodes);
		for (id, status) in lifecycle {
			let _ = self.inner.registry.set_status(id.as_str(), None, status);
		}
		if receipts.is_empty() {
			receipts.push(Receipt::Failed);
		}
		Ok(receipts)
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
			.filter_map(|(id, node)| {
				(session.is_none() || session == Some(node.session.as_str())).then(|| id.clone())
			})
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
	records
		.iter()
		.find(|(candidate, _)| candidate.as_str().eq_ignore_ascii_case(id))
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
		Some(sender) => find_node(&nodes, sender).is_some_and(|(id, _)| {
			broker
				.registry
				.record(id)
				.is_some_and(|(record, _)| record.status != RegistryStatus::Aborted)
		}),
		None => nodes.iter().any(|(id, _)| {
			id.as_str() != owner
				&& broker
					.registry
					.record(id)
					.is_some_and(|(record, _)| record.status != RegistryStatus::Aborted)
		}),
	}
}

fn class(mode: DeliveryMode) -> InterruptClass {
	match mode {
		DeliveryMode::Aside | DeliveryMode::Steer => InterruptClass::Immediate,
		DeliveryMode::NextTurn => InterruptClass::TurnBoundary,
	}
}

/// Encodes a peer message as the canonical thread item journaled by the loop.
#[must_use]
pub fn peer_item(message: &PeerMessage) -> Item {
	Item {
		seq:           0,
		created_at_ms: message.sent_ms,
		kind:          Some(item::Kind::Message(ThreadMessage {
			role:  Role::User as i32,
			parts: vec![Part { kind: Some(part::Kind::Text(message.text.to_string())) }],
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

fn tombstone_path(transcript: &Path) -> PathBuf {
	let mut value = transcript.as_os_str().to_os_string();
	value.push(".tombstone");
	PathBuf::from(value)
}

fn cold_record(path: &Path) -> Result<Option<AgentRecord>, RegistryError> {
	let file = fs::File::open(path)?;
	let mut lines = std::io::BufReader::new(file).lines();
	let Some(header) = lines.next().transpose()? else {
		return Ok(None);
	};
	let Ok(header) = omp_storage::transcript::read_header(header.as_bytes()) else {
		return Ok(None);
	};
	// A header without at least one durable event is not a resumable agent.
	if lines.next().transpose()?.is_none() {
		return Ok(None);
	}
	let id = header.id.0.clone();
	let name = path
		.file_stem()
		.and_then(std::ffi::OsStr::to_str)
		.map_or_else(|| id.clone(), Str::new);
	Ok(Some(AgentRecord {
		id,
		name,
		kind: AgentKind::Subagent,
		parent: None,
		session: header.id.0,
		depth: 1,
		status: RegistryStatus::Parked,
		activity: Default::default(),
		last_activity_ms: header.created,
		transcript: Some(path.to_path_buf()),
		definition: None,
		model: None,
		task: None,
		history: AgentHistory::default(),
	}))
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
			id:         sf!("message-{index}"),
			from:       from.into(),
			to:         to.into(),
			text:       sf!("body-{index}"),
			mode:       DeliveryMode::Aside,
			reply_to:   None,
			sent_ms:    now_ms(),
			session_id: "session".into(),
		}
	}

	#[test]
	fn registry_cas_ttl_and_tombstones_are_monotonic() {
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
		assert_eq!(parked[0].status, RegistryStatus::Parked);
		registry.abort("worker").expect("abort");
		assert!(matches!(
			registry.register_node(&node, RegistryStatus::Idle, None),
			Err(RegistryError::Tombstoned(_))
		));
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
		registry.abort("peer").expect("abort");
		assert_eq!(
			inbox
				.wait_for_timeout(Some("peer"), None, Some(Duration::from_secs(1)))
				.await,
			Err(WaitError::PeerDead)
		);
	}
}
