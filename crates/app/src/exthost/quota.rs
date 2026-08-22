//! CONTROL-side resource accounting and two-level fair dispatch.

use std::{
	collections::{BTreeMap, VecDeque},
	time::Instant,
};

use omp_core::{Duration, Str};
use thiserror::Error;

use crate::envd::worker::HostKey;

/// Canonical CONTROL quota names.
pub mod names {
	/// Fire-and-forget UI effects.
	pub const UI_EFFECTS: &str = "ui.effects";
	/// Incremental UI updates.
	pub const UI_UPDATES: &str = "ui.updates";
	/// Unique telemetry instruments and attribute series.
	pub const TELEMETRY_CARDINALITY: &str = "telemetry.cardinality";
	/// Durable extension-authored journal appends.
	pub const JOURNAL_APPENDS: &str = "journal.appends";
	/// Durable approval-ticket filing requests.
	pub const APPROVAL_REQUESTS: &str = "approval.requests";
	/// Provider discovery and replacement requests.
	pub const PROVIDER_DISCOVERY: &str = "provider.discovery";
}

/// Whether exhausting a quota rejects or silently drops one operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuotaBehavior {
	/// Refuse the operation with [`QuotaExceeded`].
	Hard,
	/// Drop the operation and increment its visible drop counter.
	Soft,
}

/// Definition of one CONTROL-side quota.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuotaSpec {
	/// Stable quota name, such as `journal.appends` or `ui.effects`.
	pub name:                Str,
	/// Maximum usage by one extension in the window.
	pub per_extension_limit: u64,
	/// Maximum aggregate usage by all extensions in one session.
	pub per_session_limit:   u64,
	/// Optional rolling accounting window; absence means session-absolute.
	pub window:              Option<Duration>,
	/// Exhaustion behavior.
	pub behavior:            QuotaBehavior,
}

impl QuotaSpec {
	/// Creates one quota definition.
	pub fn new(
		name: impl Into<Str>,
		per_extension_limit: u64,
		per_session_limit: u64,
		window: Option<Duration>,
		behavior: QuotaBehavior,
	) -> Self {
		Self { name: name.into(), per_extension_limit, per_session_limit, window, behavior }
	}
}

/// Live usage of one quota.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuotaStatus {
	/// Extension-local limit.
	pub limit:  u64,
	/// Extension-local usage in the current window.
	pub used:   u64,
	/// Window length, or `None` for a session-absolute quota.
	pub window: Option<Duration>,
}

/// Snapshot returned by `omp.resources()` and quota failures.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ResourceReceipt {
	/// Live status keyed by quota name.
	pub quotas:  BTreeMap<Str, QuotaStatus>,
	/// Soft-quota drops keyed by quota name.
	pub dropped: BTreeMap<Str, u64>,
}

/// Accounting level which exhausted first.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuotaScope {
	/// One extension exhausted its allocation.
	Extension,
	/// Aggregate extensions exhausted their session allocation.
	Session,
}

/// A hard CONTROL quota refused an operation.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("{quota} quota exceeded at {scope:?} scope")]
pub struct QuotaExceeded {
	/// Exhausted quota name.
	pub quota:   Str,
	/// Scope which exhausted first.
	pub scope:   QuotaScope,
	/// Current extension resource receipt.
	pub receipt: ResourceReceipt,
}

/// Outcome of charging a CONTROL operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChargeOutcome {
	/// The operation is accounted and may proceed.
	Accepted,
	/// A soft quota dropped and counted the operation.
	Dropped,
}

/// Invalid quota configuration or accounting request.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum QuotaError {
	/// Two specifications used the same stable quota name.
	#[error("quota {0} is configured more than once")]
	Duplicate(Str),
	/// Resource limits for one extension were registered more than once.
	#[error("resource limits for {0:?} are already configured")]
	DuplicateExtension(HostKey),
	/// A charge named no configured quota.
	#[error("quota {0} is not configured")]
	Unknown(Str),
	/// A charge named an extension with no admitted resource limits.
	#[error("resource limits for {0:?} are not configured")]
	UnknownExtension(HostKey),
	/// A duration could not be represented by the monotonic clock.
	#[error("quota {0} has an invalid accounting window")]
	InvalidWindow(Str),
	/// A hard quota refused the operation.
	#[error(transparent)]
	Exceeded(#[from] QuotaExceeded),
}

#[derive(Clone, Copy, Debug)]
struct Usage {
	used:    u64,
	dropped: u64,
	started: Instant,
}

impl Usage {
	const fn new(now: Instant) -> Self {
		Self { used: 0, dropped: 0, started: now }
	}

	fn refresh(&mut self, window: Option<std::time::Duration>, now: Instant) {
		if window.is_some_and(|window| now.saturating_duration_since(self.started) >= window) {
			self.used = 0;
			self.started = now;
		}
	}
}

#[derive(Default)]
struct ExtensionUsage {
	quotas: BTreeMap<Str, Usage>,
}

#[derive(Default)]
struct SessionUsage {
	quotas:     BTreeMap<Str, Usage>,
	extensions: BTreeMap<HostKey, ExtensionUsage>,
}

/// Daemon-owned CONTROL quota ledger.
///
/// Accounting is nested by session and extension. A charge must fit both
/// limits, preventing one extension from starving its peers and one session
/// from bypassing its aggregate allocation.
#[derive(Default)]
pub struct ControlQuotaLedger {
	specs:          BTreeMap<HostKey, BTreeMap<Str, QuotaSpec>>,
	session_limits: BTreeMap<Str, u64>,
	sessions:       BTreeMap<Str, SessionUsage>,
}

impl ControlQuotaLedger {
	/// Creates an empty ledger. Every admitted manifest must register its own
	/// limits before the extension can send CONTROL work.
	pub fn new() -> Self {
		Self::default()
	}

	/// Registers the mandatory resource-limit table from one admitted manifest.
	pub fn register_limits(
		&mut self,
		extension: HostKey,
		specs: impl IntoIterator<Item = QuotaSpec>,
	) -> Result<(), QuotaError> {
		if self.specs.contains_key(&extension) {
			return Err(QuotaError::DuplicateExtension(extension));
		}
		let mut indexed = BTreeMap::new();
		for spec in specs {
			let name = spec.name.clone();
			if indexed.insert(name.clone(), spec).is_some() {
				return Err(QuotaError::Duplicate(name));
			}
		}
		for spec in indexed.values() {
			self
				.session_limits
				.entry(spec.name.clone())
				.and_modify(|limit| *limit = (*limit).min(spec.per_session_limit))
				.or_insert(spec.per_session_limit);
		}
		self.specs.insert(extension, indexed);
		Ok(())
	}

	/// Charges one operation against its extension and session allocations.
	///
	/// Hard exhaustion returns [`QuotaError::Exceeded`]. Soft exhaustion does
	/// not mutate `used`; it increments the drop count and returns a receipt.
	pub fn charge(
		&mut self,
		session: impl Into<Str>,
		extension: &HostKey,
		quota: &str,
		amount: u64,
		now: Instant,
	) -> Result<ChargeOutcome, QuotaError> {
		let session = session.into();
		let spec = self
			.specs
			.get(extension)
			.ok_or_else(|| QuotaError::UnknownExtension(extension.clone()))?
			.get(quota)
			.cloned()
			.ok_or_else(|| QuotaError::Unknown(Str::from(quota)))?;
		let session_limit = self.session_limits[spec.name.as_str()];
		let window = spec
			.window
			.map(Duration::to_std)
			.transpose()
			.map_err(|_| QuotaError::InvalidWindow(spec.name.clone()))?;

		let session_usage = self.sessions.entry(session.clone()).or_default();
		let aggregate = session_usage
			.quotas
			.entry(spec.name.clone())
			.or_insert_with(|| Usage::new(now));
		aggregate.refresh(window, now);
		let extension_usage = session_usage
			.extensions
			.entry(extension.clone())
			.or_default()
			.quotas
			.entry(spec.name.clone())
			.or_insert_with(|| Usage::new(now));
		extension_usage.refresh(window, now);

		let extension_exhausted =
			extension_usage.used.saturating_add(amount) > spec.per_extension_limit;
		let session_exhausted = aggregate.used.saturating_add(amount) > session_limit;
		if extension_exhausted || session_exhausted {
			let scope = if extension_exhausted {
				QuotaScope::Extension
			} else {
				QuotaScope::Session
			};
			if spec.behavior == QuotaBehavior::Soft {
				extension_usage.dropped = extension_usage.dropped.saturating_add(1);
				return Ok(ChargeOutcome::Dropped);
			}
			let receipt = self.resources(&session, extension, now)?;
			return Err(QuotaExceeded { quota: spec.name, scope, receipt }.into());
		}

		extension_usage.used = extension_usage.used.saturating_add(amount);
		aggregate.used = aggregate.used.saturating_add(amount);
		Ok(ChargeOutcome::Accepted)
	}

	/// Returns the live extension-local receipt for `omp.resources()`.
	pub fn resources(
		&mut self,
		session: &str,
		extension: &HostKey,
		now: Instant,
	) -> Result<ResourceReceipt, QuotaError> {
		let mut receipt = ResourceReceipt::default();
		let specs = self
			.specs
			.get(extension)
			.ok_or_else(|| QuotaError::UnknownExtension(extension.clone()))?;
		let Some(session_usage) = self.sessions.get_mut(session) else {
			for spec in specs.values() {
				receipt.quotas.insert(spec.name.clone(), QuotaStatus {
					limit:  spec.per_extension_limit,
					used:   0,
					window: spec.window,
				});
			}
			return Ok(receipt);
		};
		let extension_usage = session_usage
			.extensions
			.entry(extension.clone())
			.or_default();
		for spec in specs.values() {
			let window = spec
				.window
				.map(Duration::to_std)
				.transpose()
				.map_err(|_| QuotaError::InvalidWindow(spec.name.clone()))?;
			let usage = extension_usage
				.quotas
				.entry(spec.name.clone())
				.or_insert_with(|| Usage::new(now));
			usage.refresh(window, now);
			receipt.quotas.insert(spec.name.clone(), QuotaStatus {
				limit:  spec.per_extension_limit,
				used:   usage.used,
				window: spec.window,
			});
			if usage.dropped != 0 {
				receipt.dropped.insert(spec.name.clone(), usage.dropped);
			}
		}
		Ok(receipt)
	}
}

struct ExtensionQueue<T> {
	values: VecDeque<T>,
}

impl<T> Default for ExtensionQueue<T> {
	fn default() -> Self {
		Self { values: VecDeque::new() }
	}
}

struct SessionQueue<T> {
	active_extensions: VecDeque<HostKey>,
	extensions:        BTreeMap<HostKey, ExtensionQueue<T>>,
}

impl<T> Default for SessionQueue<T> {
	fn default() -> Self {
		Self { active_extensions: VecDeque::new(), extensions: BTreeMap::new() }
	}
}

/// A two-level round-robin queue for CONTROL work.
///
/// Sessions rotate at the outer level and active extensions rotate within
/// their session. Empty keys are removed immediately, so idle extensions and
/// sessions consume no dispatch turns.
pub struct FairControlQueue<T> {
	active_sessions: VecDeque<Str>,
	sessions:        BTreeMap<Str, SessionQueue<T>>,
	len:             usize,
}

impl<T> Default for FairControlQueue<T> {
	fn default() -> Self {
		Self {
			active_sessions: VecDeque::new(),
			sessions:        BTreeMap::new(),
			len:             0,
		}
	}
}

impl<T> FairControlQueue<T> {
	/// Creates an empty fair queue.
	pub fn new() -> Self {
		Self::default()
	}

	/// Returns the number of queued operations.
	pub const fn len(&self) -> usize {
		self.len
	}

	/// Returns whether no CONTROL operation is queued.
	pub const fn is_empty(&self) -> bool {
		self.len == 0
	}

	/// Adds work while activating each session and extension at most once.
	pub fn push(&mut self, session: impl Into<Str>, extension: HostKey, value: T) {
		let session = session.into();
		let is_new_session = !self.sessions.contains_key(&session);
		let queue = self.sessions.entry(session.clone()).or_default();
		let extension_queue = queue.extensions.entry(extension.clone()).or_default();
		if extension_queue.values.is_empty() {
			queue.active_extensions.push_back(extension);
		}
		extension_queue.values.push_back(value);
		if is_new_session {
			self.active_sessions.push_back(session);
		}
		self.len += 1;
	}

	/// Removes the next operation using session-then-extension round robin.
	pub fn pop(&mut self) -> Option<T> {
		let session = self.active_sessions.pop_front()?;
		let mut remove_session = false;
		let value = {
			let queue = self
				.sessions
				.get_mut(&session)
				.expect("active session has a queue");
			let extension = queue
				.active_extensions
				.pop_front()
				.expect("active session has an extension");
			let extension_queue = queue
				.extensions
				.get_mut(&extension)
				.expect("active extension has a queue");
			let value = extension_queue
				.values
				.pop_front()
				.expect("active extension has work");
			if extension_queue.values.is_empty() {
				queue.extensions.remove(&extension);
			} else {
				queue.active_extensions.push_back(extension);
			}
			if queue.active_extensions.is_empty() {
				remove_session = true;
			} else {
				self.active_sessions.push_back(session.clone());
			}
			value
		};
		if remove_session {
			self.sessions.remove(&session);
		}
		self.len -= 1;
		Some(value)
	}
}
