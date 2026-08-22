//! App-owned advisor model fallback, retry, cooldown, and quota policy.

use std::{
	collections::BTreeMap,
	sync::Arc,
	time::{Duration, Instant},
};

use omp_core::Str;

/// Provider failure class relevant to advisor recovery.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum AdvisorFailureClass {
	/// Transient transport or provider failure.
	Transient,
	/// Provider quota is exhausted and must not be retried automatically.
	Quota,
	/// The model or request shape is permanently unsupported.
	Permanent,
}

/// One explicitly ordered advisor model fallback chain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdvisorFallbackChain {
	selectors: Arc<[Str]>,
}

impl AdvisorFallbackChain {
	/// Builds a non-empty, stable, duplicate-free selector chain.
	pub fn new(selectors: impl IntoIterator<Item = Str>) -> Result<Self, AdvisorResilienceError> {
		let mut retained = Vec::new();
		for selector in selectors {
			let selector = selector.trim();
			if selector.is_empty() {
				return Err(AdvisorResilienceError::EmptySelector);
			}
			if !retained.iter().any(|existing: &Str| *existing == selector) {
				retained.push(Str::new(selector));
			}
		}
		if retained.is_empty() {
			return Err(AdvisorResilienceError::EmptyChain);
		}
		Ok(Self { selectors: retained.into() })
	}

	/// Borrows selectors in exact fallback order.
	pub fn selectors(&self) -> &[Str] {
		&self.selectors
	}
}

/// Retry decision for one advisor update.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdvisorRetryDecision {
	/// Attempt this selector immediately.
	Attempt { selector: Str, attempt: u32 },
	/// Wait until the cooldown expires, then ask again.
	Cooldown { until: Instant },
	/// Quota is hard-latched until an explicit reset or credential refresh.
	QuotaLatched,
	/// Every retry and fallback candidate was exhausted.
	Exhausted,
	/// The current failure is permanent for the configured chain.
	Permanent,
}

/// Invalid resilience configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AdvisorResilienceError {
	/// No fallback selector was supplied.
	#[error("advisor fallback chain must not be empty")]
	EmptyChain,
	/// One selector was empty after trimming.
	#[error("advisor fallback selector must not be empty")]
	EmptySelector,
	/// A retry budget of zero cannot execute an update.
	#[error("advisor retry budget must be positive")]
	ZeroRetryBudget,
}

#[derive(Clone, Debug)]
struct AdvisorBudgetState {
	candidate:      usize,
	attempts:       u32,
	cooldown_until: Option<Instant>,
	quota_latched:  bool,
}

/// Per-advisor retry budget manager owned by production composition.
pub struct AdvisorRetryManager {
	chain:              AdvisorFallbackChain,
	attempts_per_model: u32,
	initial_backoff:    Duration,
	max_backoff:        Duration,
	states:             BTreeMap<Str, AdvisorBudgetState>,
}

impl AdvisorRetryManager {
	/// Creates a manager with bounded exponential cooldowns.
	pub fn new(
		chain: AdvisorFallbackChain,
		attempts_per_model: u32,
		initial_backoff: Duration,
		max_backoff: Duration,
	) -> Result<Self, AdvisorResilienceError> {
		if attempts_per_model == 0 {
			return Err(AdvisorResilienceError::ZeroRetryBudget);
		}
		Ok(Self {
			chain,
			attempts_per_model,
			initial_backoff,
			max_backoff: max_backoff.max(initial_backoff),
			states: BTreeMap::new(),
		})
	}

	/// Selects the next permitted attempt for one stable advisor id.
	pub fn next(&mut self, advisor_id: &str, now: Instant) -> AdvisorRetryDecision {
		let state = self
			.states
			.entry(Str::new(advisor_id))
			.or_insert(AdvisorBudgetState {
				candidate:      0,
				attempts:       0,
				cooldown_until: None,
				quota_latched:  false,
			});
		if state.quota_latched {
			return AdvisorRetryDecision::QuotaLatched;
		}
		if let Some(until) = state.cooldown_until {
			if now < until {
				return AdvisorRetryDecision::Cooldown { until };
			}
			state.cooldown_until = None;
		}
		let Some(selector) = self.chain.selectors().get(state.candidate) else {
			return AdvisorRetryDecision::Exhausted;
		};
		AdvisorRetryDecision::Attempt {
			selector: selector.clone(),
			attempt:  state.attempts.saturating_add(1),
		}
	}

	/// Records a failed attempt and advances retry/fallback policy.
	pub fn record_failure(
		&mut self,
		advisor_id: &str,
		class: AdvisorFailureClass,
		now: Instant,
	) -> AdvisorRetryDecision {
		let state = self
			.states
			.entry(Str::new(advisor_id))
			.or_insert(AdvisorBudgetState {
				candidate:      0,
				attempts:       0,
				cooldown_until: None,
				quota_latched:  false,
			});
		match class {
			AdvisorFailureClass::Quota => {
				state.quota_latched = true;
				AdvisorRetryDecision::QuotaLatched
			},
			AdvisorFailureClass::Permanent => {
				state.candidate = self.chain.selectors().len();
				AdvisorRetryDecision::Permanent
			},
			AdvisorFailureClass::Transient => {
				state.attempts = state.attempts.saturating_add(1);
				if state.attempts >= self.attempts_per_model {
					state.candidate = state.candidate.saturating_add(1);
					state.attempts = 0;
				}
				if state.candidate >= self.chain.selectors().len() {
					return AdvisorRetryDecision::Exhausted;
				}
				let exponent = state.attempts.min(31);
				let factor = 1_u32.checked_shl(exponent).unwrap_or(u32::MAX);
				let backoff = self
					.initial_backoff
					.saturating_mul(factor)
					.min(self.max_backoff);
				let until = now + backoff;
				state.cooldown_until = Some(until);
				AdvisorRetryDecision::Cooldown { until }
			},
		}
	}

	/// Clears retry/cooldown state after one successful update.
	pub fn record_success(&mut self, advisor_id: &str) {
		self.states.remove(advisor_id);
	}

	/// Releases only the quota hard latch after credential refresh or user
	/// reset.
	pub fn reset_quota_latch(&mut self, advisor_id: &str) {
		if let Some(state) = self.states.get_mut(advisor_id) {
			state.quota_latched = false;
			state.cooldown_until = None;
		}
	}
}
