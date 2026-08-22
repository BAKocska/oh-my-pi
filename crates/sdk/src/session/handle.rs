//! Durable embedded-session handle and cold-revival actor.

use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc, time::Instant};

use omp_agent::{
	AbortHandle, Agent, AgentError, AgentEvent, AgentRunSummary, CampaignEntry, CampaignMachine,
	CampaignSpec, EngageOptions, EngageReceipt, EventSubscription, TurnClient, TurnId,
};
use omp_core::Str;
use omp_proto::thread::v1::Item;
use omp_telemetry::firehose::{Envelope, Event as TelemetryEvent, Firehose, SessionDispatch};
use parking_lot::Mutex;
use thiserror::Error;

use super::SessionDiagnostics;
use crate::CallbackSet;

/// Stable durable identity retained when a live loop is disposed or parked.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionIdentity {
	/// Stable journal/session identifier.
	pub id:                Str,
	/// Append-only v4 journal backing cold revival.
	pub journal_path:      PathBuf,
	/// Optional compare-and-swap revision required before revival.
	pub expected_revision: Option<u64>,
}

impl SessionIdentity {
	/// Creates a durable identity over one authoritative journal.
	pub fn new(id: impl Into<Str>, journal_path: impl Into<PathBuf>) -> Self {
		Self {
			id:                id.into(),
			journal_path:      journal_path.into(),
			expected_revision: None,
		}
	}
}

/// Non-secret request passed to an application-owned cold-revival factory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRevivalRequest {
	/// Stable durable session identity.
	pub identity: SessionIdentity,
}

/// Typed failure returned by a cold-revival factory.
#[derive(Debug, Error)]
pub enum SessionRevivalError {
	/// The expected journal revision no longer matches the durable authority.
	#[error("session journal revision changed before revival")]
	RevisionConflict,
	/// The journal does not exist or does not belong to the requested session.
	#[error("session journal identity is unavailable for revival")]
	Unavailable,
	/// Application production composition failed.
	#[error("session production composition failed")]
	Composition {
		/// Typed application error retained as the source.
		#[source]
		source: Box<dyn std::error::Error + Send + Sync>,
	},
}

impl SessionRevivalError {
	/// Wraps a typed application composition error.
	pub fn composition(source: impl std::error::Error + Send + Sync + 'static) -> Self {
		Self::Composition { source: Box::new(source) }
	}
}

/// Cold, application-owned runtime construction future.
pub type SessionRevivalFuture =
	Pin<Box<dyn Future<Output = Result<SessionRuntime, SessionRevivalError>> + Send + 'static>>;

/// Factory that reconstructs an equivalent loop from the append-only journal.
pub type SessionRevivalFactory =
	Arc<dyn Fn(SessionRevivalRequest) -> SessionRevivalFuture + Send + Sync + 'static>;

/// Observable lifecycle of the in-memory loop behind a durable identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum SessionLifecycle {
	/// The in-memory loop is ready for a submission.
	Ready,
	/// A caller submission is active.
	Running,
	/// Live resources were released; a later submit performs cold revival.
	Disposed,
	/// The application factory is reconstructing the loop from its journal.
	Reviving,
	/// The handle actor has terminated and accepts no further work.
	Closed,
}

/// Lifecycle receiver that does not expose the mutable watch sender.
#[derive(Clone)]
pub struct SessionLifecycleSubscription {
	rx: tokio::sync::watch::Receiver<SessionLifecycle>,
}

impl SessionLifecycleSubscription {
	/// Returns the latest lifecycle state.
	pub fn current(&self) -> SessionLifecycle {
		*self.rx.borrow()
	}

	/// Waits for and returns the next lifecycle state.
	pub async fn changed(
		&mut self,
	) -> Result<SessionLifecycle, tokio::sync::watch::error::RecvError> {
		self.rx.changed().await?;
		Ok(*self.rx.borrow())
	}
}

/// Session-handle operation failure.
#[derive(Debug, Error)]
pub enum SessionHandleError {
	/// The live agent loop rejected or failed the submission.
	#[error(transparent)]
	Agent(#[from] AgentError),
	/// Cold revival failed before a loop could accept the submission.
	#[error(transparent)]
	Revival(#[from] SessionRevivalError),
	/// The handle actor was closed.
	#[error("session handle is closed")]
	Closed,
	/// Launch requires an active Tokio runtime.
	#[error("session handle launch requires an active Tokio runtime")]
	NoRuntime,
	/// No revival factory was installed for a disposed runtime.
	#[error("disposed session has no cold-revival factory")]
	NotRevivable,
}

type SubmitFuture<'a> =
	Pin<Box<dyn Future<Output = Result<AgentRunSummary, AgentError>> + Send + 'a>>;
type EngageFuture<'a> = Pin<
	Box<dyn Future<Output = Result<(EngageReceipt, Vec<CampaignEntry>), AgentError>> + Send + 'a>,
>;
type DisengageFuture<'a> =
	Pin<Box<dyn Future<Output = Result<(bool, Vec<CampaignEntry>), AgentError>> + Send + 'a>>;

trait RuntimeDriver: Send {
	fn submit<'a>(&'a mut self, items: Vec<Item>, turn_id: TurnId) -> SubmitFuture<'a>;
	fn engage<'a>(
		&'a mut self,
		spec: Arc<CampaignSpec>,
		machine: Box<dyn CampaignMachine>,
		options: EngageOptions,
	) -> EngageFuture<'a>;
	fn disengage<'a>(&'a mut self, engagement: Str, now_ms: u64) -> DisengageFuture<'a>;
}

struct AgentRuntime<C: TurnClient + Send + 'static> {
	agent: Agent<C>,
}

impl<C: TurnClient + Send + 'static> RuntimeDriver for AgentRuntime<C> {
	fn submit<'a>(&'a mut self, items: Vec<Item>, turn_id: TurnId) -> SubmitFuture<'a> {
		Box::pin(self.agent.submit(items, turn_id))
	}

	fn engage<'a>(
		&'a mut self,
		spec: Arc<CampaignSpec>,
		machine: Box<dyn CampaignMachine>,
		options: EngageOptions,
	) -> EngageFuture<'a> {
		Box::pin(async move {
			let receipt = self.agent.engage_campaign(spec, machine, options)?;
			let entries = self.agent.arbiter().campaigns().entries();
			Ok((receipt, entries))
		})
	}

	fn disengage<'a>(&'a mut self, engagement: Str, now_ms: u64) -> DisengageFuture<'a> {
		Box::pin(async move {
			let removed = self.agent.disengage_campaign(engagement.as_str(), now_ms)?;
			let entries = self.agent.arbiter().campaigns().entries();
			Ok((removed, entries))
		})
	}
}

/// Erased live-loop bundle consumed once by [`SessionHandle`].
///
/// Embedders can construct this from the native agent loop, but cannot recover
/// mutable loop, process, or journal internals after handing it to the handle.
pub struct SessionRuntime {
	driver:  Box<dyn RuntimeDriver>,
	events:  EventSubscription,
	abort:   AbortHandle,
	dispose: Vec<Box<dyn FnOnce() + Send + 'static>>,
}

impl SessionRuntime {
	/// Takes ownership of one fully composed native agent loop.
	pub fn from_agent<C>(agent: Agent<C>) -> Self
	where
		C: TurnClient + Send + 'static,
	{
		let events = agent.events().subscribe_lossless();
		let abort = agent.abort_handle();
		Self { driver: Box::new(AgentRuntime { agent }), events, abort, dispose: Vec::new() }
	}

	/// Registers one synchronous authority-release action run when this runtime
	/// is disposed, replaced during revival, or dropped after actor shutdown.
	pub fn on_dispose(mut self, callback: impl FnOnce() + Send + 'static) -> Self {
		self.dispose.push(Box::new(callback));
		self
	}
}

impl Drop for SessionRuntime {
	fn drop(&mut self) {
		for callback in self.dispose.drain(..).rev() {
			callback();
		}
	}
}

enum Command {
	Submit {
		items:   Vec<Item>,
		turn_id: TurnId,
		reply:   flume::Sender<Result<AgentRunSummary, SessionHandleError>>,
	},
	EngageCampaign {
		spec:    Arc<CampaignSpec>,
		machine: Box<dyn CampaignMachine>,
		options: EngageOptions,
		reply:   flume::Sender<Result<(EngageReceipt, Vec<CampaignEntry>), SessionHandleError>>,
	},
	DisengageCampaign {
		engagement: Str,
		now_ms:     u64,
		reply:      flume::Sender<Result<(bool, Vec<CampaignEntry>), SessionHandleError>>,
	},
	Dispose {
		reply: flume::Sender<()>,
	},
}

struct HandleInner {
	identity:    SessionIdentity,
	diagnostics: SessionDiagnostics,
	callbacks:   CallbackSet,
	commands:    flume::Sender<Command>,
	abort:       Mutex<Option<AbortHandle>>,
	lifecycle:   tokio::sync::watch::Sender<SessionLifecycle>,
	firehose:    Option<Arc<Firehose>>,
}

/// Clone-cheap handle for submitting to, interrupting, disposing, and reviving
/// one durable agent journal.
#[derive(Clone)]
pub struct SessionHandle {
	inner: Arc<HandleInner>,
}

impl Drop for SessionHandle {
	fn drop(&mut self) {
		if Arc::strong_count(&self.inner) == 1 {
			self.interrupt();
		}
	}
}

impl SessionHandle {
	pub(crate) fn launch(
		identity: SessionIdentity,
		diagnostics: SessionDiagnostics,
		callbacks: CallbackSet,
		runtime: Option<SessionRuntime>,
		revival: Option<SessionRevivalFactory>,
		constructed_at: Instant,
		firehose: Option<Arc<Firehose>>,
	) -> Result<Self, SessionHandleError> {
		let initial = if runtime.is_some() {
			SessionLifecycle::Ready
		} else {
			SessionLifecycle::Disposed
		};
		let (commands, rx) = flume::unbounded();
		let (lifecycle, _) = tokio::sync::watch::channel(initial);
		let abort = runtime.as_ref().map(|runtime| runtime.abort.clone());
		let inner = Arc::new(HandleInner {
			identity,
			diagnostics,
			callbacks,
			commands,
			abort: Mutex::new(abort),
			lifecycle,
			firehose,
		});
		let actor_inner = Arc::downgrade(&inner);
		let runtime_handle =
			tokio::runtime::Handle::try_current().map_err(|_| SessionHandleError::NoRuntime)?;
		runtime_handle.spawn(run_handle_actor(actor_inner, rx, runtime, revival, constructed_at));
		Ok(Self { inner })
	}

	/// Returns the stable journal identity.
	pub fn identity(&self) -> &SessionIdentity {
		&self.inner.identity
	}

	/// Returns typed construction, fallback, LSP, and launch diagnostics.
	pub fn diagnostics(&self) -> &SessionDiagnostics {
		&self.inner.diagnostics
	}

	/// Publishes a host-owned typed event through the handle fan-out.
	pub fn publish(&self, event: AgentEvent) {
		for callback in &self.inner.callbacks.events {
			callback(&event);
		}
		self
			.inner
			.callbacks
			.events_bus()
			.publish_shared(Arc::new(event));
	}

	/// Adds a bounded lossy typed-event subscription suitable for host UI.
	pub fn subscribe(&self, capacity: usize) -> omp_agent::LossyEventSubscription {
		self.inner.callbacks.events_bus().subscribe_ui(capacity)
	}

	/// Adds an ordered lossless typed-event subscription suitable for an SDK
	/// host.
	pub fn subscribe_lossless(&self) -> EventSubscription {
		self.inner.callbacks.events_bus().subscribe_lossless()
	}

	/// Subscribes to in-memory lifecycle transitions.
	pub fn lifecycle(&self) -> SessionLifecycleSubscription {
		SessionLifecycleSubscription { rx: self.inner.lifecycle.subscribe() }
	}

	/// Submits canonical caller-authored items. A disposed handle transparently
	/// reloads its journal through the guarded revival factory first.
	pub async fn submit(
		&self,
		items: impl IntoIterator<Item = Item>,
		turn_id: TurnId,
	) -> Result<AgentRunSummary, SessionHandleError> {
		let (reply, rx) = flume::bounded(1);
		self
			.inner
			.commands
			.send_async(Command::Submit { items: items.into_iter().collect(), turn_id, reply })
			.await
			.map_err(|_| SessionHandleError::Closed)?;
		rx.recv_async()
			.await
			.map_err(|_| SessionHandleError::Closed)?
	}

	/// Engages and journals a campaign on the actor-owned agent loop.
	pub async fn engage_campaign(
		&self,
		spec: Arc<CampaignSpec>,
		machine: Box<dyn CampaignMachine>,
		options: EngageOptions,
	) -> Result<(EngageReceipt, Vec<CampaignEntry>), SessionHandleError> {
		let (reply, response) = flume::bounded(1);
		self
			.inner
			.commands
			.send_async(Command::EngageCampaign { spec, machine, options, reply })
			.await
			.map_err(|_| SessionHandleError::Closed)?;
		response
			.recv_async()
			.await
			.map_err(|_| SessionHandleError::Closed)?
	}

	/// Disengages a campaign and returns the resulting complete campaign
	/// projection.
	pub async fn disengage_campaign(
		&self,
		engagement: Str,
		now_ms: u64,
	) -> Result<(bool, Vec<CampaignEntry>), SessionHandleError> {
		let (reply, response) = flume::bounded(1);
		self
			.inner
			.commands
			.send_async(Command::DisengageCampaign { engagement, now_ms, reply })
			.await
			.map_err(|_| SessionHandleError::Closed)?;
		response
			.recv_async()
			.await
			.map_err(|_| SessionHandleError::Closed)?
	}

	/// Interrupts the active submission without waiting for the actor mailbox.
	pub fn interrupt(&self) {
		if let Some(abort) = self.inner.abort.lock().as_ref() {
			abort.abort();
		}
	}

	/// Releases live loop resources while retaining durable identity. A later
	/// submission remains valid when a cold-revival factory is installed.
	pub async fn dispose(&self) -> Result<(), SessionHandleError> {
		self.interrupt();
		let (reply, rx) = flume::bounded(1);
		self
			.inner
			.commands
			.send_async(Command::Dispose { reply })
			.await
			.map_err(|_| SessionHandleError::Closed)?;
		rx.recv_async()
			.await
			.map_err(|_| SessionHandleError::Closed)
	}
}

async fn run_handle_actor(
	inner: std::sync::Weak<HandleInner>,
	commands: flume::Receiver<Command>,
	mut runtime: Option<SessionRuntime>,
	revival: Option<SessionRevivalFactory>,
	constructed_at: Instant,
) {
	while let Ok(command) = commands.recv_async().await {
		let Some(shared) = inner.upgrade() else {
			break;
		};
		match command {
			Command::Dispose { reply } => {
				shared.abort.lock().take();
				runtime = None;
				shared.lifecycle.send_replace(SessionLifecycle::Disposed);
				let _ = reply.send(());
				continue;
			},
			Command::EngageCampaign { spec, machine, options, reply } => {
				let Some(live) = runtime.as_mut() else {
					let _ = reply.send(Err(SessionHandleError::NotRevivable));
					continue;
				};
				let result = live
					.driver
					.engage(spec, machine, options)
					.await
					.map_err(SessionHandleError::from);
				let _ = reply.send(result);
			},
			Command::DisengageCampaign { engagement, now_ms, reply } => {
				let Some(live) = runtime.as_mut() else {
					let _ = reply.send(Err(SessionHandleError::NotRevivable));
					continue;
				};
				let result = live
					.driver
					.disengage(engagement, now_ms)
					.await
					.map_err(SessionHandleError::from);
				let _ = reply.send(result);
			},
			Command::Submit { items, turn_id, reply } => {
				if runtime.is_none() {
					shared.lifecycle.send_replace(SessionLifecycle::Reviving);
					let revived = if let Some(factory) = &revival {
						factory(SessionRevivalRequest { identity: shared.identity.clone() }).await
					} else {
						Err(SessionRevivalError::Unavailable)
					};
					match revived {
						Ok(next) => {
							*shared.abort.lock() = Some(next.abort.clone());
							runtime = Some(next);
							shared.lifecycle.send_replace(SessionLifecycle::Ready);
						},
						Err(SessionRevivalError::Unavailable) if revival.is_none() => {
							shared.lifecycle.send_replace(SessionLifecycle::Disposed);
							let _ = reply.send(Err(SessionHandleError::NotRevivable));
							continue;
						},
						Err(error) => {
							shared.lifecycle.send_replace(SessionLifecycle::Disposed);
							let _ = reply.send(Err(error.into()));
							continue;
						},
					}
				}
				shared.lifecycle.send_replace(SessionLifecycle::Running);
				let Some(live) = runtime.as_mut() else {
					let _ = reply.send(Err(SessionHandleError::NotRevivable));
					continue;
				};
				let submit = live.driver.submit(items, turn_id);
				tokio::pin!(submit);
				let result = loop {
					tokio::select! {
						result = &mut submit => break result.map_err(SessionHandleError::from),
						event = live.events.recv() => {
							let Ok(event) = event else { continue; };
							publish_event(&shared, event, constructed_at);
						},
					}
				};
				while let Ok(event) = live.events.try_recv() {
					publish_event(&shared, event, constructed_at);
				}
				shared.lifecycle.send_replace(SessionLifecycle::Ready);
				let _ = reply.send(result);
			},
		}
	}
	if let Some(shared) = inner.upgrade() {
		shared.abort.lock().take();
		shared.lifecycle.send_replace(SessionLifecycle::Closed);
	}
}

fn publish_event(inner: &HandleInner, event: Arc<AgentEvent>, constructed_at: Instant) {
	let first_provider_event =
		matches!(event.as_ref(), AgentEvent::PhaseChanged { to: omp_agent::AgentPhase::Turning, .. })
			&& inner.diagnostics.launch().first_dispatch_ms.is_none();
	if first_provider_event {
		let elapsed = constructed_at.elapsed();
		let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
		inner.diagnostics.record_first_dispatch(elapsed_ms);
		if let Some(firehose) = &inner.firehose {
			firehose.publish(TelemetryEvent::SessionDispatch(SessionDispatch {
				envelope:   Envelope {
					session_id: inner.identity.id.clone(),
					agent_id: inner.identity.id.clone(),
					occurred_at_ms: now_ms(),
					..Envelope::default()
				},
				latency_ms: elapsed_ms,
			}));
		}
		if let Some(callback) = &inner.callbacks.first_dispatch {
			callback(elapsed);
		}
	}
	for callback in &inner.callbacks.events {
		callback(&event);
	}
	inner.callbacks.events_bus().publish_shared(event);
}

fn now_ms() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map_or(0, |duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
}
