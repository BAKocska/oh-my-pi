//! App-owned advisor model fallback, retry, cooldown, and quota policy.

use std::{
	collections::{BTreeMap, VecDeque},
	sync::Arc,
	time::{Duration, Instant},
};

use omp_agent::{
	CampaignMachine, CampaignScope, CampaignSpec, CampaignStateError, CampaignStepResult,
	ControlError, ControlSender, EngageOptions, ExhaustPolicy, Ladder, LadderStep, PointCx,
	Reaction, Verdict, broker_now_ms,
	advisor::{AdviceDelivery, AdviceSeverity, DeliveryContext, RoutedAdvice},
};
use omp_core::{Point, PointSet, Str};
use omp_proto::thread::v1::{self as thread, Item};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

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
/// Typed terminal reason retained after an advisor campaign is muted.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdvisorMuteReason {
	/// Repeated unsafe advisor turns exhausted the quarantine ladder.
	QuarantineExhausted {
		/// Classification attached to the final quarantined turn.
		reason: Str,
	},
}

/// Result of offering one guarded note to the advisor campaign.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdvisorCampaignSubmission {
	/// The note was accepted for delivery at the named loop boundary.
	Accepted(AdviceDelivery),
	/// The advisor was already muted and cannot enqueue more notes.
	Muted(AdvisorMuteReason),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum AdvisorDeliveryLane {
	ContextOrIdle,
	TurnEnd,
	Idle,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PendingAdvice {
	advisor_id: Str,
	note:       Str,
	severity:   AdviceSeverity,
}

impl From<RoutedAdvice> for PendingAdvice {
	fn from(advice: RoutedAdvice) -> Self {
		Self { advisor_id: advice.advisor_id, note: advice.note, severity: advice.severity }
	}
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct AdvisorCampaignState {
	context_or_idle:  VecDeque<PendingAdvice>,
	turn_end:         VecDeque<PendingAdvice>,
	idle:             VecDeque<PendingAdvice>,
	last_delivery_ms: Option<u64>,
	quarantines:      u32,
	muted:            Option<AdvisorMuteReason>,
}

/// App-owned handle feeding one active advisor's delivery campaign.
#[derive(Clone)]
pub struct AdvisorCampaignHandle {
	state: Arc<Mutex<AdvisorCampaignState>>,
}

impl AdvisorCampaignHandle {
	/// Maps and queues one delivery decision for a later arbiter fold.
	pub fn submit(
		&self,
		advice: RoutedAdvice,
		context: DeliveryContext,
	) -> AdvisorCampaignSubmission {
		let mut state = self.state.lock();
		if let Some(reason) = state.muted.clone() {
			return AdvisorCampaignSubmission::Muted(reason);
		}
		let delivery = advisor_delivery(advice.severity, context);
		let pending = PendingAdvice::from(advice);
		match delivery {
			AdviceDelivery::Aside => state.context_or_idle.push_back(pending),
			AdviceDelivery::Steer => state.turn_end.push_back(pending),
			AdviceDelivery::Preserve => state.idle.push_back(pending),
		}
		AdvisorCampaignSubmission::Accepted(delivery)
	}

	/// Advances the campaign's durable quarantine ladder and returns a typed
	/// mute reason when its finite bound exhausts.
	pub async fn record_quarantine(
		&self,
		control: &ControlSender,
		engagement: &str,
		reason: impl Into<Str>,
	) -> Result<Option<AdvisorMuteReason>, ControlError> {
		if let Some(reason) = self.state.lock().muted.clone() {
			return Ok(Some(reason));
		}
		let reason = reason.into();
		let stepped = control
			.step_campaign(Str::new(engagement), reason.clone())
			.await?;
		let mut state = self.state.lock();
		match stepped {
			CampaignStepResult::Missing => Ok(state.muted.clone()),
			CampaignStepResult::Advanced { .. } => {
				state.quarantines = state.quarantines.saturating_add(1);
				Ok(None)
			},
			CampaignStepResult::Terminal { .. } => {
				let muted =
					AdvisorMuteReason::QuarantineExhausted { reason };
				state.muted = Some(muted.clone());
				state.context_or_idle.clear();
				state.turn_end.clear();
				state.idle.clear();
				Ok(Some(muted))
			},
		}
	}

	/// Returns the terminal mute reason, when this advisor exhausted policy.
	pub fn muted_reason(&self) -> Option<AdvisorMuteReason> {
		self.state.lock().muted.clone()
	}
}

/// One campaign machine converting advisor notes into fixed-point Inject
/// verdicts.
pub struct AdvisorDeliveryCampaign {
	state:       Arc<Mutex<AdvisorCampaignState>>,
	immunity_ms: u64,
}

/// Lifecycle owner for one active advisor's engaged delivery campaign.
pub struct ActiveAdvisorCampaign {
	control:    ControlSender,
	engagement: Str,
	handle:     AdvisorCampaignHandle,
}

impl ActiveAdvisorCampaign {
	/// Engages exactly one delivery campaign when an advisor child starts.
	pub async fn engage(
		control: ControlSender,
		advisor_id: &str,
		immunity: Duration,
		quarantine_bound: u32,
	) -> Result<Self, ControlError> {
		let (spec, machine, handle) =
			AdvisorDeliveryCampaign::new(advisor_id, immunity, quarantine_bound);
		let receipt = control
			.engage_campaign(
				spec,
				Box::new(machine),
				EngageOptions { now_ms: broker_now_ms(), queue: false },
			)
			.await?;
		Ok(Self { control, engagement: receipt.engagement, handle })
	}

	/// Returns the feeding handle for guarded advisor notes.
	pub const fn handle(&self) -> &AdvisorCampaignHandle {
		&self.handle
	}

	/// Advances this advisor's durable quarantine ladder.
	pub async fn record_quarantine(
		&self,
		reason: impl Into<Str>,
	) -> Result<Option<AdvisorMuteReason>, ControlError> {
		self
			.handle
			.record_quarantine(&self.control, self.engagement.as_str(), reason)
			.await
	}

	/// Disengages the campaign when the advisor child stops.
	pub async fn disengage(&self) -> Result<bool, ControlError> {
		self
			.control
			.disengage_campaign(self.engagement.clone())
			.await
	}

	/// Returns the durable engagement identity.
	pub fn engagement(&self) -> &str {
		self.engagement.as_str()
	}
}

impl AdvisorDeliveryCampaign {
	/// Builds the session-scoped declaration, machine, and feeding handle for
	/// one active advisor.
	pub fn new(
		advisor_id: &str,
		immunity: Duration,
		quarantine_bound: u32,
	) -> (Arc<CampaignSpec>, Self, AdvisorCampaignHandle) {
		let immunity_ms = u64::try_from(immunity.as_millis()).unwrap_or(u64::MAX);
		let quarantine_bound = quarantine_bound.max(2);
		let state = Arc::new(Mutex::new(AdvisorCampaignState::default()));
		let steps = (0..quarantine_bound.saturating_sub(1))
			.map(|index| LadderStep {
				label:   Str::from(format!("quarantine-{}", index.saturating_add(1))),
				verdict: Verdict::Pass,
			})
			.collect::<Vec<_>>();
		let spec = Arc::new(CampaignSpec {
			id:         Str::from(format!("advisor-delivery/{advisor_id}")),
			points:     PointSet::EMPTY
				.with(Point::Context)
				.with(Point::TurnEnd)
				.with(Point::Idle),
			precedence: 40,
			ladder:     Some(
				Ladder::new(Arc::<[LadderStep]>::from(steps)).with_min_interval(immunity_ms),
			),
			exhaust:    ExhaustPolicy::Fault {
				detail: Str::new_static("advisor muted after quarantine exhaustion"),
			},
			scope:      CampaignScope::Session,
			family_rev: Str::new_static("dev.omp.app.advisor-delivery@1"),
			when:       None,
			members:    Arc::from([]),
			claims:     Arc::from([]),
			binds:      Arc::from([]),
			dwell_ms:   Some(immunity_ms),
		});
		let machine = Self { state: Arc::clone(&state), immunity_ms };
		let handle = AdvisorCampaignHandle { state };
		(spec, machine, handle)
	}
}

impl CampaignMachine for AdvisorDeliveryCampaign {
	fn react(&mut self, point: Point, cx: &PointCx<'_>) -> Reaction {
		let mut state = self.state.lock();
		if state.muted.is_some()
			|| state
				.last_delivery_ms
				.is_some_and(|last| cx.now_ms < last.saturating_add(self.immunity_ms))
		{
			return Reaction::one(Verdict::Pass);
		}
		let mut pending = Vec::new();
		match point {
			Point::Context => pending.extend(state.context_or_idle.drain(..)),
			Point::TurnEnd => pending.extend(state.turn_end.drain(..)),
			Point::Idle => {
				pending.extend(state.context_or_idle.drain(..));
				pending.extend(state.idle.drain(..));
			},
			_ => return Reaction::one(Verdict::Pass),
		}
		let items = pending.into_iter().map(advisor_item).collect::<Vec<_>>();
		if items.is_empty() {
			return Reaction::one(Verdict::Pass);
		}
		state.last_delivery_ms = Some(cx.now_ms);
		Reaction::one(Verdict::Inject(items))
	}

	fn state(&self) -> Str {
		serde_json::to_string(&*self.state.lock()).map_or_else(|_| Str::new_static("{}"), Str::from)
	}

	fn restore(&mut self, payload: &str) -> Result<(), CampaignStateError> {
		let restored =
			serde_json::from_str(payload).map_err(|_| CampaignStateError::InvalidPayload)?;
		*self.state.lock() = restored;
		Ok(())
	}
}

fn advisor_delivery(severity: AdviceSeverity, context: DeliveryContext) -> AdviceDelivery {
	if severity == AdviceSeverity::Nit {
		return AdviceDelivery::Aside;
	}
	if context.externally_interrupted || context.plan_mode {
		return AdviceDelivery::Preserve;
	}
	if context.update_in_progress && severity != AdviceSeverity::Blocker {
		return AdviceDelivery::Aside;
	}
	if context.streaming {
		return AdviceDelivery::Steer;
	}
	if context.deferred_client_turns {
		return AdviceDelivery::Preserve;
	}
	if context.terminal_answer && !context.queued_work && severity == AdviceSeverity::Concern {
		return AdviceDelivery::Preserve;
	}
	AdviceDelivery::Steer
}

fn advisor_item(advice: PendingAdvice) -> Item {
	let severity: &'static str = advice.severity.into();
	let mut text =
		String::with_capacity(advice.advisor_id.len() + advice.note.len() + severity.len() + 16);
	text.push_str("[Advisor ");
	text.push_str(advice.advisor_id.as_str());
	text.push_str(" (");
	text.push_str(severity);
	text.push_str(")]\n");
	text.push_str(advice.note.as_str());
	Item {
		kind: Some(thread::item::Kind::Message(thread::Message {
			role:  thread::Role::System as i32,
			parts: vec![thread::Part { kind: Some(thread::part::Kind::Text(text)) }],
		})),
		..Item::default()
	}
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
#[cfg(test)]
mod tests {
	use super::*;

	fn advice(note: &'static str) -> RoutedAdvice {
		RoutedAdvice {
			advisor_id: Str::new_static("watchdog"),
			note:       Str::new_static(note),
			severity:   AdviceSeverity::Nit,
		}
	}

	#[test]
	fn immunity_window_blocks_delivery_until_elapsed() {
		let (spec, mut campaign, handle) =
			AdvisorDeliveryCampaign::new("watchdog", Duration::from_millis(25), 2);
		assert_eq!(spec.dwell_ms, Some(25));
		assert_eq!(spec.ladder.as_ref().and_then(Ladder::min_interval), Some(25));
		assert_eq!(
			handle.submit(advice("first"), DeliveryContext::default()),
			AdvisorCampaignSubmission::Accepted(AdviceDelivery::Aside)
		);
		assert!(matches!(
			campaign.react(Point::Context, &PointCx { now_ms: 100, ..PointCx::default() }).verdicts.as_slice(),
			[Verdict::Inject(items)] if items.len() == 1
		));
		assert_eq!(
			handle.submit(advice("second"), DeliveryContext::default()),
			AdvisorCampaignSubmission::Accepted(AdviceDelivery::Aside)
		);
		assert_eq!(
			campaign
				.react(Point::Idle, &PointCx { now_ms: 124, ..PointCx::default() })
				.verdicts,
			[Verdict::Pass]
		);
		assert!(matches!(
			campaign.react(Point::Idle, &PointCx { now_ms: 125, ..PointCx::default() }).verdicts.as_slice(),
			[Verdict::Inject(items)] if items.len() == 1
		));
	}
}
