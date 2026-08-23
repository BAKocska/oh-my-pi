//! Extension declaration, verification, and activation lifecycle.

use std::{
	collections::{BTreeMap, BTreeSet},
	future::Future,
	sync::{
		Arc,
		atomic::{AtomicBool, AtomicU64, Ordering},
	},
	time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use flume::Receiver;
use omp_agent::{HookPhase, MailboxSender, OnFailure, device_availability_interrupt};
pub use omp_core::{ActivateReason, LifecyclePhase, Principal, RestartReason, sf};
use omp_core::{InvocationPhase, Provenance, Str};
use omp_ext::config::StaticDeclarations;
use omp_proto::{
	thread::v1::{Item, Message, Part, Role, item, part},
	toolhost::v1::{
		CampaignDeclare, CampaignManifest, CampaignReaction, CampaignScope, CampaignVerdictKind,
		SetAvailability,
	},
	ui::v1::{UiEffect, UiRequest},
};
use omp_tool::{AvailabilityDelta, Registry};
use thiserror::Error;

use super::{
	control::{ControlDispatch, ControlHandle, ControlInvocationAuthority},
	dispatch::{CallbackConcurrency, EventDeadline},
	quota::QuotaSpec,
	services::ServiceManifest,
};

/// Authenticated, generation-fenced worker availability batch.
///
/// The supervisor calls this only after it verifies the owning child
/// generation, so stale host frames never reach shared catalog state.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AvailabilityBatch {
	/// Worker-reported mount transitions in one CONTROL frame.
	pub deltas: Box<[AvailabilityDelta]>,
}

impl AvailabilityBatch {
	/// Decodes one `LifecycleWorkerEnvelope.set_availability` body.
	pub fn from_wire(wire: SetAvailability) -> Self {
		Self {
			deltas: wire
				.deltas
				.into_iter()
				.map(|delta| AvailabilityDelta {
					name:    Str::from(delta.name),
					mounted: delta.available,
					reason:  delta.reason.map(Str::from),
				})
				.collect(),
		}
	}
}
/// One generation-stamped extension observation retained by a headless host.
#[derive(Clone, Debug)]
pub struct HeadlessLifecycleEvent {
	/// Session incarnation which owns the sink.
	pub session_generation: u64,
	/// Authenticated extension-host incarnation.
	pub host_generation:    u64,
	/// Typed lifecycle payload.
	pub kind:               HeadlessLifecycleKind,
}

/// Extension observations supported by every headless protocol host.
#[derive(Clone, Debug)]
pub enum HeadlessLifecycleKind {
	/// One extension generation activated.
	Activated(ActivationEvent),
	/// The command registry changed and hosts must refresh their roster.
	CommandRosterInvalidated,
	/// A retained, non-blocking UI effect.
	UiEffect(Box<UiEffect>),
	/// A correlated UI request requiring a typed answer.
	UiRequest(Box<UiRequest>),
	/// A typed extension lifecycle failure.
	ExtensionError {
		/// Extension whose lifecycle failed.
		extension: Str,
		/// Typed lifecycle failure.
		error:     LifecycleError,
	},
}

/// Lossless receiving half of a [`HeadlessLifecycleSink`].
pub struct HeadlessLifecycleSubscription {
	rx: Receiver<Arc<HeadlessLifecycleEvent>>,
}

impl HeadlessLifecycleSubscription {
	/// Receives the next extension observation.
	pub async fn recv(&self) -> Result<Arc<HeadlessLifecycleEvent>, flume::RecvError> {
		self.rx.recv_async().await
	}

	/// Attempts to receive the next observation without waiting.
	pub fn try_recv(&self) -> Result<Arc<HeadlessLifecycleEvent>, flume::TryRecvError> {
		self.rx.try_recv()
	}
}

/// Lossless generation fence shared by print, RPC, and ACP session owners.
#[derive(Clone)]
pub struct HeadlessLifecycleSink {
	session_generation: u64,
	host_generation:    Arc<AtomicU64>,
	active:             Arc<AtomicBool>,
	tx:                 flume::Sender<Arc<HeadlessLifecycleEvent>>,
}

impl HeadlessLifecycleSink {
	/// Creates a sink for one session incarnation.
	pub fn new(session_generation: u64) -> (Self, HeadlessLifecycleSubscription) {
		let (tx, rx) = flume::unbounded();
		(
			Self {
				session_generation,
				host_generation: Arc::new(AtomicU64::new(0)),
				active: Arc::new(AtomicBool::new(false)),
				tx,
			},
			HeadlessLifecycleSubscription { rx },
		)
	}

	/// Advances the accepted host generation after supervised activation.
	pub fn activate(&self, event: ActivationEvent) -> Result<(), HeadlessSinkError> {
		let generation = event.generation;
		let mut current = self.host_generation.load(Ordering::Acquire);
		loop {
			if generation < current {
				return Err(HeadlessSinkError::StaleGeneration {
					expected: current,
					actual:   generation,
				});
			}
			match self.host_generation.compare_exchange_weak(
				current,
				generation,
				Ordering::AcqRel,
				Ordering::Acquire,
			) {
				Ok(_) => break,
				Err(observed) => current = observed,
			}
		}
		self.active.store(true, Ordering::Release);
		self.publish(generation, HeadlessLifecycleKind::Activated(event))
	}

	/// Publishes a command-roster invalidation for the active host generation.
	pub fn invalidate_commands(&self, generation: u64) -> Result<(), HeadlessSinkError> {
		self.publish(generation, HeadlessLifecycleKind::CommandRosterInvalidated)
	}

	/// Publishes a retained UI effect for the active host generation.
	pub fn ui_effect(&self, generation: u64, effect: UiEffect) -> Result<(), HeadlessSinkError> {
		self.publish(generation, HeadlessLifecycleKind::UiEffect(Box::new(effect)))
	}

	/// Publishes a correlated UI request for the active host generation.
	pub fn ui_request(&self, generation: u64, request: UiRequest) -> Result<(), HeadlessSinkError> {
		self.publish(generation, HeadlessLifecycleKind::UiRequest(Box::new(request)))
	}

	/// Publishes a typed extension error for the active host generation.
	pub fn extension_error(
		&self,
		generation: u64,
		extension: impl Into<Str>,
		error: LifecycleError,
	) -> Result<(), HeadlessSinkError> {
		self.publish(generation, HeadlessLifecycleKind::ExtensionError {
			extension: extension.into(),
			error,
		})
	}

	fn publish(
		&self,
		generation: u64,
		kind: HeadlessLifecycleKind,
	) -> Result<(), HeadlessSinkError> {
		if !self.active.load(Ordering::Acquire) {
			return Err(HeadlessSinkError::Inactive);
		}
		let expected = self.host_generation.load(Ordering::Acquire);
		if generation != expected {
			return Err(HeadlessSinkError::StaleGeneration { expected, actual: generation });
		}
		self
			.tx
			.send(Arc::new(HeadlessLifecycleEvent {
				session_generation: self.session_generation,
				host_generation: generation,
				kind,
			}))
			.map_err(|_| HeadlessSinkError::Disconnected)
	}
}

/// Rejection from a generation-stamped headless lifecycle sink.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum HeadlessSinkError {
	/// A worker attempted to publish before supervised activation.
	#[error("headless extension host is not active")]
	Inactive,
	/// An old worker attempted to publish.
	#[error("stale headless host generation: expected {expected}, got {actual}")]
	StaleGeneration {
		/// Active host generation.
		expected: u64,
		/// Published generation.
		actual:   u64,
	},
	/// The owning headless session has already disposed its subscription.
	#[error("headless lifecycle sink is disconnected")]
	Disconnected,
}

/// App-side destination for a verified `SetAvailability` CONTROL frame.
pub trait AvailabilitySink: Send + Sync {
	/// Applies one complete worker availability batch.
	fn set_availability(&self, batch: AvailabilityBatch);
}

/// Catalog and mailbox implementation of [`AvailabilitySink`].
///
/// The registry accepts unmounts immediately and conservatively ignores
/// mounts. The one turn-boundary system item still reports all worker facts,
/// allowing normal next-turn composition to surface availability changes.
pub struct RegistryAvailabilitySink {
	registry: Arc<Registry>,
	mailbox:  MailboxSender,
}

impl RegistryAvailabilitySink {
	/// Binds a shared catalog and the agent's turn-boundary mailbox producer.
	pub const fn new(registry: Arc<Registry>, mailbox: MailboxSender) -> Self {
		Self { registry, mailbox }
	}
}

impl AvailabilitySink for RegistryAvailabilitySink {
	fn set_availability(&self, batch: AvailabilityBatch) {
		self.registry.apply_availability(&batch.deltas);
		let mut text = String::from("Extension device availability changed:");
		for delta in &batch.deltas {
			text.push(' ');
			text.push_str(delta.name.as_str());
			text.push_str(if delta.mounted {
				" is available"
			} else {
				" is unavailable"
			});
			if let Some(reason) = &delta.reason {
				text.push_str(" (");
				text.push_str(reason.as_str());
				text.push(')');
			}
			text.push('.');
		}
		let item = Item {
			seq:           0,
			created_at_ms: 0,
			kind:          Some(item::Kind::Message(Message {
				role:  Role::System as i32,
				parts: vec![Part { kind: Some(part::Kind::Text(text)) }],
			})),
			props:         None,
		};
		let _ = self
			.mailbox
			.try_enqueue(device_availability_interrupt(item));
	}
}
/// A tool identity in the authoritative manifest declaration set.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ToolDeclarationKey {
	/// Public tool name.
	pub name:   Str,
	/// Compatibility family.
	pub family: Str,
	/// Monotonic revision within the family.
	pub rev:    u16,
}

impl ToolDeclarationKey {
	/// Creates a tool declaration identity.
	pub fn new(name: impl Into<Str>, family: impl Into<Str>, rev: u16) -> Self {
		Self { name: name.into(), family: family.into(), rev }
	}
}

/// A hook identity in the authoritative manifest declaration set.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct HookDeclarationKey {
	/// Stable event name.
	pub event: Str,
	/// Phase in which the handler runs.
	pub phase: HookPhase,
}

impl HookDeclarationKey {
	/// Creates a hook declaration identity.
	pub fn new(event: impl Into<Str>, phase: HookPhase) -> Self {
		Self { event: event.into(), phase }
	}
}

/// Runtime capability declarations whose use must fail closed when absent.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum EscapeCapability {
	/// Focus-owned bounded raw terminal-input subscription.
	RawTerminalInput,
	/// Trusted direct-filesystem escape with durable grant provenance.
	DirectFilesystem,
}

/// Canonical tool, hook, action, and sanctioned-escape existence sets for one
/// extension.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DeclarationSet {
	tools:   BTreeSet<ToolDeclarationKey>,
	hooks:   BTreeSet<HookDeclarationKey>,
	actions: BTreeSet<Str>,
	escapes: BTreeSet<EscapeCapability>,
}

impl DeclarationSet {
	/// Builds normalized declaration sets from any input order.
	pub fn new(
		tools: impl IntoIterator<Item = ToolDeclarationKey>,
		hooks: impl IntoIterator<Item = HookDeclarationKey>,
	) -> Self {
		Self {
			tools:   tools.into_iter().collect(),
			hooks:   hooks.into_iter().collect(),
			actions: BTreeSet::new(),
			escapes: BTreeSet::new(),
		}
	}

	/// Adds the exact static action and sanctioned-escape declarations admitted
	/// from the manifest before Python starts.
	pub fn with_runtime(
		mut self,
		actions: impl IntoIterator<Item = Str>,
		escapes: impl IntoIterator<Item = EscapeCapability>,
	) -> Self {
		self.actions = actions.into_iter().collect();
		self.escapes = escapes.into_iter().collect();
		self
	}

	/// Iterates over tool identities in canonical order.
	pub fn tools(&self) -> impl DoubleEndedIterator<Item = &ToolDeclarationKey> + ExactSizeIterator {
		self.tools.iter()
	}

	/// Iterates over hook identities in canonical order.
	pub fn hooks(&self) -> impl DoubleEndedIterator<Item = &HookDeclarationKey> + ExactSizeIterator {
		self.hooks.iter()
	}

	/// Iterates exact action names in canonical order.
	pub fn actions(&self) -> impl DoubleEndedIterator<Item = &Str> + ExactSizeIterator {
		self.actions.iter()
	}

	/// Returns whether a sanctioned escape was statically admitted.
	pub fn permits(&self, capability: EscapeCapability) -> bool {
		self.escapes.contains(&capability)
	}
}

/// Exact differences between the manifest and frozen runtime registry.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DeclarationDrift {
	/// Manifest tools absent from the runtime registry.
	pub missing_tools:      Box<[ToolDeclarationKey]>,
	/// Runtime tools absent from the manifest.
	pub unexpected_tools:   Box<[ToolDeclarationKey]>,
	/// Manifest hooks absent from the runtime registry.
	pub missing_hooks:      Box<[HookDeclarationKey]>,
	/// Runtime hooks absent from the manifest.
	pub unexpected_hooks:   Box<[HookDeclarationKey]>,
	/// Manifest actions absent from the runtime registry.
	pub missing_actions:    Box<[Str]>,
	/// Runtime actions absent from the manifest.
	pub unexpected_actions: Box<[Str]>,
	/// Manifest sanctioned escapes absent from the runtime registry.
	pub missing_escapes:    Box<[EscapeCapability]>,
	/// Runtime sanctioned escapes absent from the manifest.
	pub unexpected_escapes: Box<[EscapeCapability]>,
}

impl DeclarationDrift {
	fn between(expected: &DeclarationSet, actual: &DeclarationSet) -> Self {
		Self {
			missing_tools:      expected.tools.difference(&actual.tools).cloned().collect(),
			unexpected_tools:   actual.tools.difference(&expected.tools).cloned().collect(),
			missing_hooks:      expected.hooks.difference(&actual.hooks).cloned().collect(),
			unexpected_hooks:   actual.hooks.difference(&expected.hooks).cloned().collect(),
			missing_actions:    expected
				.actions
				.difference(&actual.actions)
				.cloned()
				.collect(),
			unexpected_actions: actual
				.actions
				.difference(&expected.actions)
				.cloned()
				.collect(),
			missing_escapes:    expected
				.escapes
				.difference(&actual.escapes)
				.copied()
				.collect(),
			unexpected_escapes: actual
				.escapes
				.difference(&expected.escapes)
				.copied()
				.collect(),
		}
	}

	/// Returns whether the two declaration sets were equal.
	pub fn is_empty(&self) -> bool {
		self.missing_tools.is_empty()
			&& self.unexpected_tools.is_empty()
			&& self.missing_hooks.is_empty()
			&& self.unexpected_hooks.is_empty()
			&& self.missing_actions.is_empty()
			&& self.unexpected_actions.is_empty()
			&& self.missing_escapes.is_empty()
			&& self.unexpected_escapes.is_empty()
	}
}

/// The four manifest activation classes.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ActivationTrigger {
	/// Static metadata served without starting Python.
	Static,
	/// Start the child when the declared surface is first used.
	FirstReach,
	/// Start the child before the first model prompt.
	BeforeFirstPrompt,
	/// Start the child before the UI first paints or accepts input.
	BeforeUiInput,
}

impl ActivationTrigger {
	/// Returns whether this trigger requires an extension-host child.
	pub const fn requires_host(self) -> bool {
		!matches!(self, Self::Static)
	}
}

/// Why one activation sequence is running.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActivationCause {
	/// First activation for a declared surface.
	FirstReach,
	/// Re-activation after the supervisor replaced a child.
	Restart(RestartReason),
}

impl ActivationCause {
	const fn split(self) -> (ActivateReason, Option<RestartReason>) {
		match self {
			Self::FirstReach => (ActivateReason::FirstReach, None),
			Self::Restart(reason) => (reason.activate_reason(), Some(reason)),
		}
	}
}

/// One-daemon principal authority used by the v1 OS-user model.
#[derive(Clone, Debug)]
pub struct PrincipalAuthority {
	principal: Principal,
}

impl PrincipalAuthority {
	/// Pins a daemon to its authenticated operating-system principal.
	pub const fn new(principal: Principal) -> Self {
		Self { principal }
	}

	/// Returns the core-owned principal used for extension contexts and durable
	/// stamps.
	pub const fn principal(&self) -> &Principal {
		&self.principal
	}

	/// Refuses attaching a client authenticated as a different OS user.
	pub fn admit(&self, candidate: &Principal) -> Result<(), PrincipalMismatch> {
		if candidate.id() == self.principal.id() {
			Ok(())
		} else {
			Err(PrincipalMismatch {
				expected: Str::from(self.principal.id()),
				actual:   Str::from(candidate.id()),
			})
		}
	}
}

/// A client tried to attach to a daemon owned by another OS principal.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("daemon principal is {expected}, not {actual}")]
pub struct PrincipalMismatch {
	/// Principal pinned when the daemon started.
	pub expected: Str,
	/// Principal presented by the attaching client.
	pub actual:   Str,
}

/// Host and session generations carried by an activation request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GenerationFence {
	/// Child restart counter.
	pub host:    u64,
	/// Session epoch into which the child was spawned.
	pub session: u64,
}

/// Payload dispatched to the extension's `extension_activate` handlers.
#[derive(Clone, Debug)]
pub struct ActivationEvent {
	/// Coarse activation class exposed to handlers.
	pub reason:             ActivateReason,
	/// Fine restart cause, absent on first reach.
	pub restart_reason:     Option<RestartReason>,
	/// Original session start time, including for late activation.
	pub session_started_at: SystemTime,
	/// Host generation fenced by core.
	pub generation:         u64,
	/// Manifest trigger which caused this child to be needed.
	pub trigger:            ActivationTrigger,
}

/// Result of requesting activation for a generation.
#[derive(Clone, Debug)]
pub enum ActivationDisposition {
	/// The surface is static and intentionally started no child.
	Inert,
	/// A fresh generation completed activation.
	Activated(ActivationEvent),
	/// This generation had already activated and was not dispatched twice.
	AlreadyActive(ActivationEvent),
}

/// Runtime boundary used by the lifecycle machine after declaration.
///
/// Implementations are CONTROL-host adapters for the post-`RegisterTools`
/// handshake. Neither method may route through the journal or agent messaging.
pub trait LifecycleHost {
	/// Sends `FreezeDeclarations` and waits for the child to seal its registry.
	fn freeze(&mut self) -> impl Future<Output = Result<(), Str>> + Send;
	/// Dispatches `extension_activate` over CONTROL.
	fn activate(
		&mut self,
		event: &ActivationEvent,
		principal: &Principal,
	) -> impl Future<Output = Result<(), Str>> + Send;
}
/// Live CONTROL implementation of the post-declaration lifecycle boundary.
pub struct ControlLifecycleHost {
	control:         ControlHandle,
	extension:       Str,
	session:         Str,
	host_generation: u64,
	next_invocation: AtomicU64,
}

impl ControlLifecycleHost {
	/// Binds lifecycle dispatch to one authenticated child incarnation.
	pub fn new(control: ControlHandle, extension: Str, session: Str, host_generation: u64) -> Self {
		Self { control, extension, session, host_generation, next_invocation: AtomicU64::new(1) }
	}

	fn authority(
		&self,
		name: &'static str,
		phase: InvocationPhase,
		lifecycle: LifecyclePhase,
	) -> ControlInvocationAuthority {
		let id = self.next_invocation.fetch_add(1, Ordering::Relaxed);
		ControlInvocationAuthority {
			invocation: sf!("lifecycle:{}:{}:{}", self.extension, self.host_generation, id),
			phase,
			session: self.session.clone(),
			turn: None,
			event: Some(sf!("{name}")),
			call: None,
			device: None,
			effects: Box::new([]),
			place_kind: sf!("host"),
			lifecycle,
			roots: Box::new([]),
			remote: false,
			has_ui: false,
			headless: true,
			settings: serde_json::Map::new(),
			secret_settings: Box::new([]),
			data: None,
			direct_filesystem: None,
		}
	}
}

impl LifecycleHost for ControlLifecycleHost {
	fn freeze(&mut self) -> impl Future<Output = Result<(), Str>> + Send {
		let dispatch = ControlDispatch {
			operation: sf!("omp.lifecycle.freeze"),
			arguments: serde_json::Map::new(),
			authority: self.authority("freeze", InvocationPhase::Open, LifecyclePhase::Frozen),
			policy:    CallbackConcurrency::Serialized,
			deadline:  EventDeadline { at: Instant::now() + Duration::from_secs(10) },
		};
		async move {
			self
				.control
				.dispatch(dispatch)
				.await
				.map(|_| ())
				.map_err(|error| Str::from(error.to_string()))
		}
	}

	fn activate(
		&mut self,
		event: &ActivationEvent,
		_principal: &Principal,
	) -> impl Future<Output = Result<(), Str>> + Send {
		let reason: &str = event.reason.into();
		let trigger = match event.trigger {
			ActivationTrigger::Static => "static",
			ActivationTrigger::FirstReach => "first_reach",
			ActivationTrigger::BeforeFirstPrompt => "before_first_prompt",
			ActivationTrigger::BeforeUiInput => "before_ui_input",
		};
		let started_at_ms = event
			.session_started_at
			.duration_since(UNIX_EPOCH)
			.unwrap_or_default()
			.as_millis()
			.try_into()
			.unwrap_or(u64::MAX);
		let mut arguments = serde_json::Map::new();
		arguments.insert(
			String::from("payload"),
			serde_json::json!({
				"extension": self.extension.as_str(),
				"reason": reason,
				"session_started_at": started_at_ms,
				"generation": event.generation,
				"trigger": trigger,
			}),
		);
		let dispatch = ControlDispatch {
			operation: sf!("omp.lifecycle.activate"),
			arguments,
			authority: self.authority(
				"extension_activate",
				InvocationPhase::EffectsAuthorized,
				LifecyclePhase::Active,
			),
			policy: CallbackConcurrency::Serialized,
			deadline: EventDeadline { at: Instant::now() + Duration::from_secs(10) },
		};
		async move {
			self
				.control
				.dispatch(dispatch)
				.await
				.map(|_| ())
				.map_err(|error| Str::from(error.to_string()))
		}
	}
}

/// Failure of a declaration or activation sequence.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum LifecycleError {
	/// A frame belonged to an old host or session generation.
	#[error(
		"stale generation: expected session {expected_session} and host >= {current_host}, got \
		 session {actual_session} host {actual_host}"
	)]
	StaleGeneration {
		/// Session generation owned by the machine.
		expected_session: u64,
		/// Last accepted host generation.
		current_host:     u64,
		/// Session generation on the request.
		actual_session:   u64,
		/// Host generation on the request.
		actual_host:      u64,
	},
	/// Dispatch attempted through a boot class absent from the manifest.
	#[error("activation trigger {0:?} is not declared by the extension manifest")]
	UndeclaredTrigger(ActivationTrigger),
	/// Importing one manifest module failed.
	#[error("declaration import {module} failed: {message}")]
	Import {
		/// Module whose body failed.
		module:  Str,
		/// Host-provided failure description.
		message: Str,
	},
	/// The child could not seal its declaration registry.
	#[error("declaration freeze failed: {0}")]
	Freeze(Str),
	/// The frozen runtime registry differed from the manifest.
	#[error("frozen declarations differ from the manifest")]
	Drift(DeclarationDrift),
	/// A campaign declaration violated the mode-slot contract.
	#[error(transparent)]
	CampaignManifest(#[from] CampaignManifestError),
	/// An activation handler failed.
	#[error("extension activation failed: {0}")]
	Activation(Str),
}

/// Structured rejection for a campaign declaration that can silently act as a
/// mode.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum CampaignManifestError {
	/// A Session campaign binds a mode-affecting surface without owning or
	/// composing the mode slot.
	#[error(
		"session campaign {campaign} binds {binding} without claiming mode or declaring composition"
	)]
	ModeClaimRequired {
		/// Campaign specification identifier.
		campaign: Str,
		/// Mode-affecting binding that triggered the rejection.
		binding:  Str,
	},
}

/// Declarations retained by the extension host from DECLARE through FREEZE.
#[derive(Clone, Debug, Default)]
pub struct CampaignDeclarationTable {
	manifests: Box<[CampaignManifest]>,
}

impl CampaignDeclarationTable {
	/// Validates and retains one worker declaration table.
	pub fn declare(declaration: CampaignDeclare) -> Result<Self, CampaignManifestError> {
		validate_campaign_manifests(&declaration.manifests)?;
		Ok(Self { manifests: declaration.manifests.into_boxed_slice() })
	}

	/// Revalidates the sealed table at FREEZE before any campaign may activate.
	pub fn freeze(&self) -> Result<(), CampaignManifestError> {
		validate_campaign_manifests(&self.manifests)
	}

	/// Returns the exact declaration order supplied by the worker.
	pub fn manifests(&self) -> &[CampaignManifest] {
		&self.manifests
	}
}

/// Rejects Session campaigns that stealth-bind the model or toolset.
pub fn validate_campaign_manifests(
	manifests: &[CampaignManifest],
) -> Result<(), CampaignManifestError> {
	for manifest in manifests {
		if CampaignScope::try_from(manifest.scope) != Ok(CampaignScope::Session)
			|| manifest.composes
			|| manifest.claims.iter().any(|claim| claim == "mode")
		{
			continue;
		}
		if let Some(binding) = manifest.binds.iter().find(|binding| {
			binding.eq_ignore_ascii_case("toolset") || binding.eq_ignore_ascii_case("model")
		}) {
			return Err(CampaignManifestError::ModeClaimRequired {
				campaign: Str::from(manifest.id.as_str()),
				binding:  Str::from(binding.as_str()),
			});
		}
	}
	Ok(())
}

/// Authoritative admitted manifest data required to start one extension.
///
/// This value is built from static deployment metadata before Python starts.
/// Runtime registration is never used to infer any expected declaration.
#[derive(Clone, Debug)]
pub struct ExtensionManifest {
	/// Core-authenticated artifact and installation provenance.
	pub provenance:          Provenance,
	/// Canonical entry module imported first.
	pub entry:               Str,
	/// Declaration modules in manifest order after `entry`.
	pub declaration_modules: Box<[Str]>,
	/// Authoritative tool and hook existence sets.
	pub declarations:        DeclarationSet,
	/// Provider declarations and consumer service grants.
	pub services:            ServiceManifest,
	/// Uniform sealed CONTROL declaration snapshot from the deployment manifest.
	static_declarations:     Arc<StaticDeclarations>,
	/// Per-extension CONTROL quota definitions.
	pub resource_limits:     Box<[QuotaSpec]>,
	/// Every boot class reachable from this manifest's declaration rows.
	pub activation_triggers: BTreeSet<ActivationTrigger>,
}

impl ExtensionManifest {
	/// Builds a mandatory manifest contract from deployment-owned data.
	pub fn new(
		provenance: Provenance,
		entry: impl Into<Str>,
		declaration_modules: impl IntoIterator<Item = Str>,
		declarations: DeclarationSet,
		services: ServiceManifest,
		resource_limits: impl IntoIterator<Item = QuotaSpec>,
		activation_triggers: impl IntoIterator<Item = ActivationTrigger>,
	) -> Self {
		Self::new_with_static(
			provenance,
			entry,
			declaration_modules,
			declarations,
			services,
			StaticDeclarations::default(),
			resource_limits,
			activation_triggers,
		)
	}

	/// Builds a mandatory manifest contract including every sealed public
	/// declaration table parsed from authenticated deployment data.
	pub fn new_with_static(
		provenance: Provenance,
		entry: impl Into<Str>,
		declaration_modules: impl IntoIterator<Item = Str>,
		declarations: DeclarationSet,
		services: ServiceManifest,
		static_declarations: StaticDeclarations,
		resource_limits: impl IntoIterator<Item = QuotaSpec>,
		activation_triggers: impl IntoIterator<Item = ActivationTrigger>,
	) -> Self {
		let entry = entry.into();
		let mut ordered_modules = Vec::new();
		for row in &static_declarations.ordered {
			if !row.module.is_empty() && row.module != entry && !ordered_modules.contains(&row.module)
			{
				ordered_modules.push(row.module.clone());
			}
		}
		for module in declaration_modules {
			if module != entry && !ordered_modules.contains(&module) {
				ordered_modules.push(module);
			}
		}
		let mut activation_triggers = activation_triggers.into_iter().collect::<BTreeSet<_>>();
		for row in &static_declarations.ordered {
			let trigger = match row.trigger.as_str() {
				"static" => Some(ActivationTrigger::Static),
				"lazy" | "first_reach" => Some(ActivationTrigger::FirstReach),
				"eager-prompt" | "before_first_prompt" => Some(ActivationTrigger::BeforeFirstPrompt),
				"eager-ui" | "before_ui_input" => Some(ActivationTrigger::BeforeUiInput),
				"" => Some(match row.kind.as_str() {
					"completion" => ActivationTrigger::BeforeUiInput,
					"prompt_slot" => ActivationTrigger::BeforeFirstPrompt,
					"credential" | "secret" | "placement" => ActivationTrigger::Static,
					_ => ActivationTrigger::FirstReach,
				}),
				_ => Some(ActivationTrigger::FirstReach),
			};
			activation_triggers.extend(trigger);
		}
		Self {
			provenance,
			entry,
			declaration_modules: ordered_modules.into_boxed_slice(),
			declarations,
			services,
			static_declarations: Arc::new(static_declarations),
			resource_limits: resource_limits.into_iter().collect(),
			activation_triggers,
		}
	}

	/// Returns the immutable declaration snapshot admitted before child import.
	pub fn static_declarations(&self) -> &StaticDeclarations {
		&self.static_declarations
	}

	/// Builds the explicit first-party `omp_py_eval` manifest.
	///
	/// Callers must still supply core-authenticated provenance and resource
	/// limits; there is no permissive default or runtime-derived expectation.
	pub fn py_eval(
		provenance: Provenance,
		resource_limits: impl IntoIterator<Item = QuotaSpec>,
	) -> Self {
		Self::new(
			provenance,
			sf!("omp_py_eval"),
			[],
			DeclarationSet::new([ToolDeclarationKey::new("py_eval", "", 1)], []),
			ServiceManifest::default(),
			resource_limits,
			[ActivationTrigger::FirstReach],
		)
	}

	/// Creates a lifecycle machine fenced to one session epoch.
	pub fn lifecycle(
		&self,
		session_started_at: SystemTime,
		session_generation: u64,
	) -> LifecycleMachine {
		LifecycleMachine::new(
			self.entry.clone(),
			self.declaration_modules.iter().cloned(),
			self.declarations.clone(),
			self.activation_triggers.clone(),
			session_started_at,
			session_generation,
		)
	}
}

/// Deterministic lifecycle state for one admitted extension.
pub struct LifecycleMachine {
	modules:             Box<[Str]>,
	expected:            DeclarationSet,
	campaigns:           CampaignDeclarationTable,
	campaign_faults:     BTreeMap<Str, u8>,
	activation_triggers: BTreeSet<ActivationTrigger>,
	phase:               LifecyclePhase,
	session_started_at:  SystemTime,
	session_generation:  u64,
	host_generation:     u64,
	last_event:          Option<ActivationEvent>,
}

impl LifecycleMachine {
	/// Builds a machine and resolves the canonical import order: entry first,
	/// followed by distinct declaration modules in manifest order.
	fn new(
		entry: impl Into<Str>,
		declaration_modules: impl IntoIterator<Item = Str>,
		expected: DeclarationSet,
		activation_triggers: BTreeSet<ActivationTrigger>,
		session_started_at: SystemTime,
		session_generation: u64,
	) -> Self {
		let entry = entry.into();
		let mut seen = BTreeSet::new();
		let mut modules = Vec::new();
		seen.insert(entry.clone());
		modules.push(entry);
		for module in declaration_modules {
			if seen.insert(module.clone()) {
				modules.push(module);
			}
		}
		Self {
			modules: modules.into_boxed_slice(),
			expected,
			campaigns: CampaignDeclarationTable::default(),
			campaign_faults: BTreeMap::new(),
			activation_triggers,
			phase: LifecyclePhase::Declared,
			session_started_at,
			session_generation,
			host_generation: 0,
			last_event: None,
		}
	}

	/// Returns the machine's current child lifecycle phase.
	pub const fn phase(&self) -> LifecyclePhase {
		self.phase
	}

	/// Accepts and validates the worker campaign table before FREEZE.
	pub fn declare_campaigns(&mut self, declaration: CampaignDeclare) -> Result<(), LifecycleError> {
		match CampaignDeclarationTable::declare(declaration) {
			Ok(campaigns) => {
				self.campaigns = campaigns;
				Ok(())
			},
			Err(error) => {
				self.phase = LifecyclePhase::Degraded;
				Err(LifecycleError::CampaignManifest(error))
			},
		}
	}

	/// Iterates over the resolved import order.
	/// Resolves one failed extension callback through its declared hook policy.
	///
	/// The second fault in one engagement force-exhausts the lane and degrades
	/// this extension generation.
	pub fn campaign_failure(
		&mut self,
		engagement: &str,
		campaign_rev: u32,
		on_failure: OnFailure,
	) -> CampaignReaction {
		let faults = self
			.campaign_faults
			.entry(Str::new(engagement))
			.and_modify(|faults| *faults = faults.saturating_add(1))
			.or_insert(1);
		let verdict = if *faults >= 2 {
			self.phase = LifecyclePhase::Degraded;
			CampaignVerdictKind::Exhausted
		} else if on_failure == OnFailure::Deny {
			CampaignVerdictKind::Deny
		} else {
			CampaignVerdictKind::Pass
		};
		CampaignReaction {
			engagement_id: engagement.to_owned(),
			campaign_rev,
			verdict: verdict.into(),
			verdict_payload: Default::default(),
			step: None,
			new_state: Default::default(),
			props: None,
		}
	}

	/// Clears transient fault accounting after one valid reaction.
	pub fn campaign_reacted(&mut self, engagement: &str) {
		self.campaign_faults.remove(engagement);
	}

	/// Iterates over the resolved import order.
	pub fn modules(&self) -> impl DoubleEndedIterator<Item = &str> + ExactSizeIterator {
		self.modules.iter().map(Str::as_str)
	}

	/// Records a failed sequential manifest import and degrades this generation.
	pub fn import_failed(
		&mut self,
		module: impl Into<Str>,
		message: impl Into<Str>,
	) -> LifecycleError {
		self.phase = LifecyclePhase::Degraded;
		LifecycleError::Import { module: module.into(), message: message.into() }
	}

	/// Validates a completed `RegisterTools` declaration set, then runs
	/// FREEZE → ACTIVATE while recording the verified lifecycle transition.
	///
	/// Python imports have already run sequentially in [`Self::modules`] order
	/// before this method is entered. Repeating an already-active generation is
	/// idempotent. Older host or session generations are rejected before any
	/// host callback is entered.
	pub async fn activate_declared<H: LifecycleHost>(
		&mut self,
		host: &mut H,
		declared: &DeclarationSet,
		fence: GenerationFence,
		trigger: ActivationTrigger,
		cause: ActivationCause,
		principal: &Principal,
	) -> Result<ActivationDisposition, LifecycleError> {
		if !self.activation_triggers.contains(&trigger) {
			return Err(LifecycleError::UndeclaredTrigger(trigger));
		}
		if !trigger.requires_host() {
			return Ok(ActivationDisposition::Inert);
		}
		if fence.session != self.session_generation || fence.host < self.host_generation {
			return Err(LifecycleError::StaleGeneration {
				expected_session: self.session_generation,
				current_host:     self.host_generation,
				actual_session:   fence.session,
				actual_host:      fence.host,
			});
		}
		if fence.host == self.host_generation
			&& self.phase == LifecyclePhase::Active
			&& let Some(event) = self.last_event.clone()
		{
			return Ok(ActivationDisposition::AlreadyActive(event));
		}

		self.host_generation = fence.host;
		self.phase = LifecyclePhase::Declared;
		let drift = DeclarationDrift::between(&self.expected, declared);
		if !drift.is_empty() {
			self.phase = LifecyclePhase::Degraded;
			return Err(LifecycleError::Drift(drift));
		}
		if let Err(error) = self.campaigns.freeze() {
			self.phase = LifecyclePhase::Degraded;
			return Err(LifecycleError::CampaignManifest(error));
		}
		if let Err(message) = host.freeze().await {
			self.phase = LifecyclePhase::Degraded;
			return Err(LifecycleError::Freeze(message));
		}
		self.phase = LifecyclePhase::Frozen;
		self.phase = LifecyclePhase::Verified;

		let (reason, restart_reason) = cause.split();
		let event = ActivationEvent {
			reason,
			restart_reason,
			session_started_at: self.session_started_at,
			generation: fence.host,
			trigger,
		};
		if let Err(message) = host.activate(&event, principal).await {
			self.phase = LifecyclePhase::Degraded;
			return Err(LifecycleError::Activation(message));
		}
		self.phase = LifecyclePhase::Active;
		self.last_event = Some(event.clone());
		Ok(ActivationDisposition::Activated(event))
	}
}
#[cfg(test)]
mod tests {
	use super::*;

	fn core_campaign(
		id: &str,
		scope: CampaignScope,
		claims: &[&str],
		binds: &[&str],
		composes: bool,
	) -> CampaignManifest {
		CampaignManifest {
			id: id.to_owned(),
			scope: scope.into(),
			claims: claims.iter().map(|value| (*value).to_owned()).collect(),
			binds: binds.iter().map(|value| (*value).to_owned()).collect(),
			composes,
			..Default::default()
		}
	}

	#[test]
	fn core_campaign_specs_cannot_declare_stealth_modes() {
		let stealth =
			core_campaign("stealth", CampaignScope::Session, &["worktree"], &["Toolset"], false);
		assert_eq!(
			validate_campaign_manifests(&[stealth]),
			Err(CampaignManifestError::ModeClaimRequired {
				campaign: Str::from("stealth"),
				binding:  Str::from("Toolset"),
			})
		);
		let mut lifecycle = LifecycleMachine::new(
			"core",
			[],
			DeclarationSet::default(),
			BTreeSet::new(),
			SystemTime::UNIX_EPOCH,
			1,
		);
		let error = lifecycle
			.declare_campaigns(CampaignDeclare {
				manifests: vec![core_campaign(
					"stealth",
					CampaignScope::Session,
					&[],
					&["Model"],
					false,
				)],
				..Default::default()
			})
			.expect_err("stealth mode must fail at declare");
		assert!(matches!(
			error,
			LifecycleError::CampaignManifest(CampaignManifestError::ModeClaimRequired { .. })
		));
		assert_eq!(lifecycle.phase(), LifecyclePhase::Degraded);
	}

	#[test]
	fn campaign_faults_apply_hook_policy_and_degrade_on_second_fault() {
		let mut lifecycle = LifecycleMachine::new(
			"core",
			[],
			DeclarationSet::default(),
			BTreeSet::new(),
			SystemTime::UNIX_EPOCH,
			1,
		);
		let deferred = lifecycle.campaign_failure("eng-1", 1, OnFailure::Defer);
		assert_eq!(deferred.verdict, CampaignVerdictKind::Pass as i32);
		assert_eq!(lifecycle.phase(), LifecyclePhase::Declared);
		let exhausted = lifecycle.campaign_failure("eng-1", 1, OnFailure::Deny);
		assert_eq!(exhausted.verdict, CampaignVerdictKind::Exhausted as i32);
		assert_eq!(lifecycle.phase(), LifecyclePhase::Degraded);

		lifecycle.campaign_reacted("eng-1");
		let denied = lifecycle.campaign_failure("eng-1", 1, OnFailure::Deny);
		assert_eq!(denied.verdict, CampaignVerdictKind::Deny as i32);
	}

	#[test]
	fn core_campaign_specs_may_own_compose_or_avoid_the_mode_slot() {
		let manifests = [
			core_campaign("plan", CampaignScope::Session, &["mode", "worktree"], &["Toolset"], false),
			core_campaign("code-mode", CampaignScope::Session, &[], &["Model"], true),
			core_campaign("turn-route", CampaignScope::Run, &[], &["Model"], false),
			core_campaign("prompt-notice", CampaignScope::Session, &[], &["PromptSlot"], false),
		];
		assert_eq!(validate_campaign_manifests(&manifests), Ok(()));

		let table = CampaignDeclarationTable::declare(CampaignDeclare {
			manifests: manifests.into(),
			..Default::default()
		})
		.expect("valid core campaign declarations");
		assert_eq!(table.manifests().len(), 4);
		assert_eq!(table.freeze(), Ok(()));
	}
}
