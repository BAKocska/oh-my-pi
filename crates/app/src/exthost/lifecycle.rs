//! Extension declaration, verification, and activation lifecycle.

use std::{collections::BTreeSet, future::Future, time::SystemTime};

use omp_agent::HookPhase;
pub use omp_core::{ActivateReason, LifecyclePhase, Principal, RestartReason};
use omp_core::{Provenance, Str};
use thiserror::Error;

use super::{quota::QuotaSpec, services::ServiceManifest};

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
	#[must_use]
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
	#[must_use]
	pub fn new(event: impl Into<Str>, phase: HookPhase) -> Self {
		Self { event: event.into(), phase }
	}
}

/// Canonical tool and hook existence sets for one extension.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DeclarationSet {
	tools: BTreeSet<ToolDeclarationKey>,
	hooks: BTreeSet<HookDeclarationKey>,
}

impl DeclarationSet {
	/// Builds normalized declaration sets from any input order.
	#[must_use]
	pub fn new(
		tools: impl IntoIterator<Item = ToolDeclarationKey>,
		hooks: impl IntoIterator<Item = HookDeclarationKey>,
	) -> Self {
		Self { tools: tools.into_iter().collect(), hooks: hooks.into_iter().collect() }
	}

	/// Iterates over tool identities in canonical order.
	pub fn tools(
		&self,
	) -> impl Iterator<Item = &ToolDeclarationKey> + DoubleEndedIterator + ExactSizeIterator {
		self.tools.iter()
	}

	/// Iterates over hook identities in canonical order.
	pub fn hooks(
		&self,
	) -> impl Iterator<Item = &HookDeclarationKey> + DoubleEndedIterator + ExactSizeIterator {
		self.hooks.iter()
	}
}

/// Exact differences between the manifest and frozen runtime registry.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DeclarationDrift {
	/// Manifest tools absent from the runtime registry.
	pub missing_tools:    Box<[ToolDeclarationKey]>,
	/// Runtime tools absent from the manifest.
	pub unexpected_tools: Box<[ToolDeclarationKey]>,
	/// Manifest hooks absent from the runtime registry.
	pub missing_hooks:    Box<[HookDeclarationKey]>,
	/// Runtime hooks absent from the manifest.
	pub unexpected_hooks: Box<[HookDeclarationKey]>,
}

impl DeclarationDrift {
	fn between(expected: &DeclarationSet, actual: &DeclarationSet) -> Self {
		Self {
			missing_tools:    expected.tools.difference(&actual.tools).cloned().collect(),
			unexpected_tools: actual.tools.difference(&expected.tools).cloned().collect(),
			missing_hooks:    expected.hooks.difference(&actual.hooks).cloned().collect(),
			unexpected_hooks: actual.hooks.difference(&expected.hooks).cloned().collect(),
		}
	}

	/// Returns whether the two declaration sets were equal.
	#[must_use]
	pub fn is_empty(&self) -> bool {
		self.missing_tools.is_empty()
			&& self.unexpected_tools.is_empty()
			&& self.missing_hooks.is_empty()
			&& self.unexpected_hooks.is_empty()
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
	#[must_use]
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
	fn split(self) -> (ActivateReason, Option<RestartReason>) {
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
	#[must_use]
	pub const fn new(principal: Principal) -> Self {
		Self { principal }
	}

	/// Returns the core-owned principal used for extension contexts and durable
	/// stamps.
	#[must_use]
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
	/// An activation handler failed.
	#[error("extension activation failed: {0}")]
	Activation(Str),
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
	/// Per-extension CONTROL quota definitions.
	pub resource_limits:     Box<[QuotaSpec]>,
	/// Every boot class reachable from this manifest's declaration rows.
	pub activation_triggers: BTreeSet<ActivationTrigger>,
}

impl ExtensionManifest {
	/// Builds a mandatory manifest contract from deployment-owned data.
	#[must_use]
	pub fn new(
		provenance: Provenance,
		entry: impl Into<Str>,
		declaration_modules: impl IntoIterator<Item = Str>,
		declarations: DeclarationSet,
		services: ServiceManifest,
		resource_limits: impl IntoIterator<Item = QuotaSpec>,
		activation_triggers: impl IntoIterator<Item = ActivationTrigger>,
	) -> Self {
		Self {
			provenance,
			entry: entry.into(),
			declaration_modules: declaration_modules.into_iter().collect(),
			declarations,
			services,
			resource_limits: resource_limits.into_iter().collect(),
			activation_triggers: activation_triggers.into_iter().collect(),
		}
	}

	/// Builds the explicit first-party `omp_py_eval` manifest.
	///
	/// Callers must still supply core-authenticated provenance and resource
	/// limits; there is no permissive default or runtime-derived expectation.
	#[must_use]
	pub fn py_eval(
		provenance: Provenance,
		resource_limits: impl IntoIterator<Item = QuotaSpec>,
	) -> Self {
		Self::new(
			provenance,
			Str::new_static("omp_py_eval"),
			[],
			DeclarationSet::new([ToolDeclarationKey::new("py_eval", "", 1)], []),
			ServiceManifest::default(),
			resource_limits,
			[ActivationTrigger::FirstReach],
		)
	}

	/// Creates a lifecycle machine fenced to one session epoch.
	#[must_use]
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
			activation_triggers,
			phase: LifecyclePhase::Declared,
			session_started_at,
			session_generation,
			host_generation: 0,
			last_event: None,
		}
	}

	/// Returns the machine's current child lifecycle phase.
	#[must_use]
	pub const fn phase(&self) -> LifecyclePhase {
		self.phase
	}

	/// Iterates over the resolved import order.
	pub fn modules(&self) -> impl Iterator<Item = &str> + DoubleEndedIterator + ExactSizeIterator {
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
