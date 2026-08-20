//! Project-scoped agent roster routing over canonical mailbox items.

use std::{
	collections::HashMap,
	time::{SystemTime, UNIX_EPOCH},
};

use omp_core::Str;
use omp_proto::thread::v1::{Item, Message as ThreadMessage, Part, Role, item, part};
use parking_lot::Mutex;
use smallvec::SmallVec;
use thiserror::Error;

use crate::{AgentNode, Interrupt, InterruptClass, InterruptSource, MailboxSender};

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

/// Per-recipient result of message routing.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display, strum::EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum Receipt {
	/// Handed to a running recipient mailbox.
	Delivered,
	/// Handed to an idle recipient that may begin a turn.
	Woken,
	/// Recipient was cold-revived by a daemon transport.
	Revived,
	/// The transport retained the message for a later handoff.
	Buffered,
	/// No live messageable target matched.
	Failed,
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

/// Broker transport or routing failure.
#[derive(Debug, Error)]
pub enum BrokerError {
	/// A live transport became unavailable while sending.
	#[error("broker transport is unavailable")]
	Transport,
}

struct RegisteredNode {
	name:       Str,
	session:    Str,
	mailbox:    MailboxSender,
	message_tx: flume::Sender<PeerMessage>,
	idle:       bool,
}

/// Core-owned project routing table.
///
/// The table has no durable message payload store: a successful send places one
/// canonical [`Item`] into the recipient loop's mailbox, where the loop
/// journals it like every other input. The observer receiver exists solely for
/// `wait_for` reply correlation and never substitutes for the thread item.
pub struct Broker {
	project:    Str,
	nodes:      Mutex<HashMap<Str, RegisteredNode>>,
	generation: tokio::sync::watch::Sender<u64>,
}

impl Broker {
	/// Creates an empty broker for one canonical project display name.
	#[must_use]
	pub fn new(project: Str) -> Self {
		let (generation, _) = tokio::sync::watch::channel(0_u64);
		Self { project, nodes: Mutex::new(HashMap::new()), generation }
	}

	/// Registers a messageable roster node and returns its reply observer.
	pub fn register(&self, node: &AgentNode, mailbox: MailboxSender) -> BrokerInbox {
		let (tx, rx) = flume::bounded(100);
		self.nodes.lock().insert(node.id.clone(), RegisteredNode {
			name: node.name.clone(),
			session: node.session.clone(),
			mailbox,
			message_tx: tx,
			idle: true,
		});
		self.bump_generation();
		BrokerInbox { receiver: rx, roster: self.generation.subscribe() }
	}

	/// Removes a terminal or tombstoned node from routing.
	pub fn unregister(&self, id: &str) {
		if self.nodes.lock().remove(id).is_some() {
			self.bump_generation();
		}
	}

	/// Sets whether a routed node is currently idle.
	pub fn set_idle(&self, id: &str, idle: bool) {
		if let Some(node) = self.nodes.lock().get_mut(id) {
			node.idle = idle;
		}
	}

	/// Routes one message to every address match, returning compact receipts.
	///
	/// A closed recipient mailbox is a confirmed transport failure. An unmatched
	/// address is normal and returns a single [`Receipt::Failed`].
	pub fn send(&self, message: PeerMessage) -> Result<SmallVec<Receipt, 4>, BrokerError> {
		let mut receipts = SmallVec::new();
		let mut nodes = self.nodes.lock();
		for (id, node) in nodes
			.iter_mut()
			.filter(|(id, node)| self.matches(&message.to, id, node))
		{
			let _ = id;
			let source = InterruptSource::Peer { from: message.from.clone() };
			let interrupt =
				Interrupt { class: class(message.mode), item: peer_item(&message), source };
			node
				.mailbox
				.try_enqueue(interrupt)
				.map_err(|_| BrokerError::Transport)?;
			if node.message_tx.try_send(message.clone()).is_err() {
				// The canonical mailbox delivery already succeeded; reply
				// observation is lossy.
			}
			receipts.push(if node.idle {
				Receipt::Woken
			} else {
				Receipt::Delivered
			});
		}
		if receipts.is_empty() {
			receipts.push(Receipt::Failed);
		}
		Ok(receipts)
	}

	/// Lists currently messageable node identities for the project or a session.
	#[must_use]
	pub fn peers(&self, session: Option<&str>) -> SmallVec<Str, 4> {
		self
			.nodes
			.lock()
			.iter()
			.filter_map(|(id, node)| {
				(session.is_none() || session == Some(node.session.as_str())).then(|| id.clone())
			})
			.collect()
	}

	fn matches(&self, address: &str, id: &str, node: &RegisteredNode) -> bool {
		address == id
			|| address == node.name.as_str()
			|| (address == "all")
			|| (address == "project:all" && !self.project.is_empty())
			|| address
				.strip_prefix("session:")
				.is_some_and(|session| session == node.session.as_str())
	}

	fn bump_generation(&self) {
		self
			.generation
			.send_modify(|generation| *generation = generation.wrapping_add(1));
	}
}

/// Receiver used for reply correlation and liveness-aware waits.
pub struct BrokerInbox {
	receiver: flume::Receiver<PeerMessage>,
	roster:   tokio::sync::watch::Receiver<u64>,
}

impl BrokerInbox {
	/// Waits for a matching canonical delivery or returns early when liveness
	/// changes.
	///
	/// The roster `watch` is selected directly; no polling timer can deadlock a
	/// wait after the awaited peer exits.
	pub async fn wait_for(
		&mut self,
		sender: Option<&str>,
		reply_to: Option<&str>,
	) -> Option<PeerMessage> {
		loop {
			tokio::select! {
				message = self.receiver.recv_async() => {
					let Ok(message) = message else { return None };
					if sender.is_none_or(|sender| sender == message.from.as_str())
						&& reply_to.is_none_or(|reply| message.reply_to.as_deref() == Some(reply)) {
						return Some(message);
					}
				},
				changed = self.roster.changed() => {
					if changed.is_err() { return None; }
					return None;
				},
			}
		}
	}
}

fn class(mode: DeliveryMode) -> InterruptClass {
	match mode {
		DeliveryMode::Aside => InterruptClass::Immediate,
		DeliveryMode::Steer => InterruptClass::Immediate,
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
