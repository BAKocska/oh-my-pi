//! Durable, bounded campaigns folded at the agent loop's fixed decision points.

use std::{
	collections::{BTreeMap, VecDeque},
	sync::Arc,
};

use omp_core::{Point, PointSet, Str, Ulid};
use omp_llm_inference::call::ToolChoice;
use omp_proto::thread::v1::Item;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::{
	ContextPatch,
	tool_choice::{
		DirectiveCallbacks, DirectivePriority, PushOptions, RejectOutcome, ToolChoiceQueue,
	},
};

/// Stable identity of an campaign declaration.
pub type CampaignSpecId = Str;
/// Stable identity of one engaged campaign instance.
pub type EngagementId = Str;

/// Lifetime over which an engagement remains eligible.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[repr(u8)]
#[serde(rename_all = "snake_case")]
pub enum CampaignScope {
	/// The current model/tool turn only.
	Turn,
	/// The current caller submission.
	Run,
	/// The durable session, including process revival.
	Session,
}

/// Named exclusive resource claimed by an engagement.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotClaim {
	/// The next forced tool choice.
	ToolChoice,
	/// Exclusive workspace mutation regime.
	Worktree,
	/// Exclusive loop director.
	Director,
	/// Exclusive editor surface.
	EditorSurface,
	/// Exclusive background batch execution.
	BatchExecution,
	/// User-visible regime exclusivity.
	Mode,
	/// A declaration supplied by a future core slot table.
	Named(Str),
}

impl SlotClaim {
	/// Returns the canonical declaration name.
	pub fn name(&self) -> &str {
		match self {
			Self::ToolChoice => "tool_choice",
			Self::Worktree => "worktree",
			Self::Director => "director",
			Self::EditorSurface => "editor-surface",
			Self::BatchExecution => "batch-execution",
			Self::Mode => "mode",
			Self::Named(name) => name.as_str(),
		}
	}
}

/// One canonical exclusive-slot declaration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SlotDecl {
	/// Stable name accepted by campaign declarations.
	pub name:      &'static str,
	/// Whether the holder belongs in user-facing slot projections.
	pub visible:   bool,
	/// Whether a conflicting engagement may file a FIFO ticket.
	pub queueable: bool,
}

/// Core slot vocabulary registered by every [`SlotRegistry`].
pub const SLOT_TABLE: [SlotDecl; 6] = [
	SlotDecl { name: "tool_choice", visible: false, queueable: true },
	SlotDecl { name: "worktree", visible: true, queueable: true },
	SlotDecl { name: "director", visible: true, queueable: true },
	SlotDecl { name: "editor-surface", visible: true, queueable: true },
	SlotDecl { name: "batch-execution", visible: true, queueable: true },
	SlotDecl { name: "mode", visible: true, queueable: true },
];

/// Named stackable binding slot.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BindSlot {
	/// Advertised tool set.
	Toolset,
	/// Model routing selection.
	ModelRoute,
	/// Prompt contribution slot.
	PromptSlot,
	/// Interrupt delivery policy.
	DeliveryPolicy,
	/// A core-registered additional binding slot.
	Named(Str),
}

/// One engagement-scoped LIFO binding.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ScopedBinding {
	/// Addressed stack.
	pub slot:  BindSlot,
	/// Opaque value interpreted by the slot owner.
	pub value: Str,
}

/// Required-deadline park request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HoldTicket {
	/// Stable ticket identity.
	pub id:          Str,
	/// Absolute epoch-millisecond deadline.
	pub deadline_ms: u64,
	/// User-visible reason.
	pub reason:      Str,
}

/// Closed vocabulary emitted by campaign machines.
#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
	/// Identity reaction.
	Pass,
	/// Veto settlement and begin another turn.
	Continue,
	/// Veto the current attempt.
	Deny {
		/// User-visible unionable reason.
		reason: Str,
	},
	/// Park until a required-deadline ticket resolves.
	Hold(HoldTicket),
	/// Serialize a named forced tool through [`ToolChoiceQueue`].
	Force {
		/// Tool name to require.
		tool: Str,
	},
	/// Abort the active stream or batch.
	Cut {
		/// User-visible cut reason.
		reason: Str,
	},
	/// End with a structured fault.
	Fault {
		/// Terminal structured diagnostic.
		detail: Str,
	},
	/// Terminate a subagent after bounded policy exhaustion.
	Kill {
		/// Process-style exit code.
		exit:   i32,
		/// Stable termination reason.
		reason: Str,
	},
	/// The campaign reached its success terminal.
	Done,
	/// Rewrite the provider wire context.
	Patch(ContextPatch),
	/// Inject canonical context items.
	Inject(Vec<Item>),
	/// Push an engagement-scoped binding.
	Bind(ScopedBinding),
}

/// One observable rung in a finite escalation ladder.
#[derive(Clone, Debug, PartialEq)]
pub struct LadderStep {
	/// Stable forensic label for the rung.
	pub label:   Str,
	/// Reaction produced while this rung is current.
	pub verdict: Verdict,
}

/// A finite sequence of reactions. An empty ladder is rejected by engagement.
#[derive(Clone, Debug, PartialEq)]
pub struct Ladder {
	steps:        Arc<[LadderStep]>,
	min_interval: Option<u64>,
}

impl Ladder {
	/// Constructs a finite ladder.
	pub fn new(steps: impl Into<Arc<[LadderStep]>>) -> Self {
		Self { steps: steps.into(), min_interval: None }
	}

	/// Sets the minimum epoch-millisecond interval between delivered rungs.
	pub const fn with_min_interval(mut self, min_interval: u64) -> Self {
		self.min_interval = Some(min_interval);
		self
	}

	/// Returns the minimum interval between delivered rungs.
	pub const fn min_interval(&self) -> Option<u64> {
		self.min_interval
	}

	/// Returns the number of bounded rungs.
	pub fn len(&self) -> usize {
		self.steps.len()
	}

	/// Returns whether the ladder contains no rung.
	pub fn is_empty(&self) -> bool {
		self.steps.is_empty()
	}

	/// Returns a rung by cursor position.
	pub fn step(&self, cursor: usize) -> Option<&LadderStep> {
		self.steps.get(cursor)
	}
}

/// Action taken when a ladder has no remaining rung.
#[derive(Clone, Debug, PartialEq)]
pub enum ExhaustPolicy {
	/// Remove the lane and permit normal settlement.
	Settle,
	/// Remove the lane and produce a structured fault.
	Fault {
		/// Terminal structured diagnostic.
		detail: Str,
	},
	/// Emit a specific terminal reaction.
	Verdict(Verdict),
}

/// Data-only trigger evaluated by Core before auto-engaging a campaign.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CampaignWhen {
	/// Point whose event may auto-engage this declaration.
	pub point:           Point,
	/// Optional exact invocation identity.
	pub invocation_id:   Option<Str>,
	/// Optional streamed fragment substring.
	pub stream_contains: Option<Str>,
	/// Optional delivered-effect predicate.
	pub delivered:       Option<bool>,
}

impl CampaignWhen {
	/// Evaluates only immutable Core facts, without extension IPC.
	pub fn matches(&self, point: Point, cx: &crate::arbiter::PointCx<'_>) -> bool {
		self.point == point
			&& self
				.invocation_id
				.as_ref()
				.is_none_or(|expected| cx.invocation_id == Some(expected.as_str()))
			&& self.stream_contains.as_ref().is_none_or(|needle| {
				cx.stream_delta
					.is_some_and(|delta| delta.contains(needle.as_str()))
			}) && self
			.delivered
			.is_none_or(|delivered| cx.delivered == delivered)
	}
}

/// Immutable declaration shared by every engagement of one campaign.
#[derive(Clone, Debug)]
pub struct CampaignSpec {
	/// Stable declaration identity.
	pub id:         CampaignSpecId,
	/// Subscribed decision points.
	pub points:     PointSet,
	/// Higher values are folded first within one origin.
	pub precedence: i16,
	/// Finite escalation policy. `None` is valid for scope-bounded standing
	/// lanes.
	pub ladder:     Option<Ladder>,
	/// Terminal action after bounded exhaustion.
	pub exhaust:    ExhaustPolicy,
	/// Engagement lifetime.
	pub scope:      CampaignScope,
	/// State schema identity (`family@rev`).
	pub family_rev: Str,
	/// Optional data-only Core-side auto-engagement predicate.
	pub when:       Option<CampaignWhen>,
	/// Child specs whose lifecycle is tied to this lane.
	pub members:    Arc<[CampaignSpecId]>,
	/// Exclusive claims acquired atomically at engagement.
	pub claims:     Arc<[SlotClaim]>,
	/// Engagement-scoped bindings pushed after every claim is granted.
	pub binds:      Arc<[ScopedBinding]>,
	/// Minimum epoch-millisecond residence before a non-cut exit.
	pub dwell_ms:   Option<u64>,
}

/// Returns the core plan-regime declaration.
pub fn plan_regime_spec() -> CampaignSpec {
	regime_spec("plan", [SlotClaim::Mode, SlotClaim::Worktree])
}

/// Returns the core vibe-regime declaration.
pub fn vibe_regime_spec() -> CampaignSpec {
	regime_spec("vibe", [SlotClaim::Mode, SlotClaim::Director])
}

/// Returns the core goal-regime declaration.
pub fn goal_regime_spec() -> CampaignSpec {
	let mut spec = regime_spec("goal", [SlotClaim::Mode]);
	spec.points = Point::Context.set();
	spec.family_rev = Str::new_static("dev.omp.core.goal@1");
	spec.exhaust = ExhaustPolicy::Fault { detail: Str::new_static("goal token budget exhausted") };
	spec
}

/// Returns the core autoresearch-regime declaration.
pub fn autoresearch_regime_spec() -> CampaignSpec {
	regime_spec("autoresearch", [SlotClaim::Mode, SlotClaim::Worktree])
}

fn regime_spec<const N: usize>(id: &'static str, claims: [SlotClaim; N]) -> CampaignSpec {
	CampaignSpec {
		id:         Str::new_static(id),
		points:     PointSet::EMPTY,
		precedence: 0,
		ladder:     None,
		exhaust:    ExhaustPolicy::Settle,
		scope:      CampaignScope::Session,
		family_rev: Str::new_static("dev.omp.core.regime@1"),
		when:       None,
		members:    Arc::from([]),
		claims:     Arc::from(claims),
		binds:      Arc::from([ScopedBinding {
			slot:  BindSlot::PromptSlot,
			value: Str::new_static(id),
		}]),
		dwell_ms:   None,
	}
}

/// Stateful core or extension adapter evaluated at subscribed points.
pub trait CampaignMachine: Send + Sync + 'static {
	/// Produces one atomic reaction without mutating any sibling lane.
	fn react(&mut self, point: Point, cx: &crate::arbiter::PointCx<'_>) -> Reaction;

	/// Returns the durable state payload for journaling.
	fn state(&self) -> Str;

	/// Restores a state payload with the spec's `family@rev` already validated.
	fn restore(&mut self, payload: &str) -> Result<(), CampaignStateError>;

	/// Applies a live state update. Revival continues to call [`Self::restore`].
	fn update(&mut self, payload: &[u8]) -> Result<(), CampaignStateError> {
		let payload = std::str::from_utf8(payload).map_err(|_| CampaignStateError::InvalidPayload)?;
		self.restore(payload)
	}
}

/// Stateless machine backing the built-in session regimes.
#[derive(Default)]
pub struct RegimeMachine;

impl CampaignMachine for RegimeMachine {
	fn react(&mut self, _: Point, _: &crate::arbiter::PointCx<'_>) -> Reaction {
		Reaction::one(Verdict::Pass)
	}

	fn state(&self) -> Str {
		Str::new_static("{}")
	}

	fn restore(&mut self, payload: &str) -> Result<(), CampaignStateError> {
		if payload == "{}" {
			Ok(())
		} else {
			Err(CampaignStateError::InvalidPayload)
		}
	}
}

/// Durable state owned by the built-in goal regime.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct GoalCampaignState {
	/// Durable objective supplied by the user.
	pub objective:          Str,
	/// Optional hard token budget.
	pub budget_tokens:      Option<u64>,
	/// Tokens charged to the objective so far.
	pub spent_tokens:       u64,
	/// Bitset for delivered 50%, 75%, and 90% transition steers.
	pub thresholds_crossed: u8,
}

/// Stateful goal regime that emits each budget-transition steer once.
#[derive(Default)]
pub struct GoalCampaign {
	state:   GoalCampaignState,
	pending: u8,
}

impl GoalCampaign {
	fn crossed(state: &GoalCampaignState) -> u8 {
		let Some(budget) = state.budget_tokens.filter(|budget| *budget != 0) else {
			return 0;
		};
		let ratio = state.spent_tokens.saturating_mul(100) / budget;
		u8::from(ratio >= 50) | (u8::from(ratio >= 75) << 1) | (u8::from(ratio >= 90) << 2)
	}
}

impl CampaignMachine for GoalCampaign {
	fn react(&mut self, point: Point, _: &crate::arbiter::PointCx<'_>) -> Reaction {
		if point != Point::Context {
			return Reaction::one(Verdict::Pass);
		}
		if self
			.state
			.budget_tokens
			.is_some_and(|budget| self.state.spent_tokens >= budget)
		{
			return Reaction::one(Verdict::Fault {
				detail: Str::new_static("goal token budget exhausted"),
			});
		}
		if self.pending == 0 {
			return Reaction::one(Verdict::Pass);
		}
		let bit = self.pending.trailing_zeros() as u8;
		self.pending &= !(1 << bit);
		let percent = [50, 75, 90][usize::from(bit)];
		Reaction::one(Verdict::Inject(vec![campaign_message(format!(
			"Goal budget reached {percent}% ({} tokens spent). Reassess progress against the \
			 objective and preserve budget for the highest-value remaining work.",
			self.state.spent_tokens,
		))]))
	}

	fn state(&self) -> Str {
		Str::from(
			serde_json::to_string(&self.state)
				.expect("goal campaign state has infallible JSON serialization"),
		)
	}

	fn restore(&mut self, payload: &str) -> Result<(), CampaignStateError> {
		self.state = serde_json::from_str(payload).map_err(|_| CampaignStateError::InvalidPayload)?;
		self.pending = 0;
		Ok(())
	}

	fn update(&mut self, payload: &[u8]) -> Result<(), CampaignStateError> {
		let mut next: GoalCampaignState =
			serde_json::from_slice(payload).map_err(|_| CampaignStateError::InvalidPayload)?;
		let crossed = Self::crossed(&next);
		self.pending |= crossed & !self.state.thresholds_crossed;
		next.thresholds_crossed |= crossed | self.state.thresholds_crossed;
		self.state = next;
		Ok(())
	}
}

/// Resolves one built-in regime declaration and its machine.
pub fn core_regime(id: &str) -> Option<(Arc<CampaignSpec>, Box<dyn CampaignMachine>)> {
	let (spec, machine): (CampaignSpec, Box<dyn CampaignMachine>) = match id {
		"plan" => (plan_regime_spec(), Box::new(RegimeMachine)),
		"vibe" => (vibe_regime_spec(), Box::new(RegimeMachine)),
		"goal" => (goal_regime_spec(), Box::new(GoalCampaign::default())),
		"autoresearch" => (autoresearch_regime_spec(), Box::new(RegimeMachine)),
		_ => return None,
	};
	Some((Arc::new(spec), machine))
}

/// Failure to restore a declared state family.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CampaignStateError {
	/// The payload could not be decoded by its machine.
	#[error("campaign state payload is invalid")]
	InvalidPayload,
	/// No active or queued engagement matched a requested update.
	#[error("campaign engagement is not active")]
	MissingEngagement,
}

/// Atomic output of one machine tick.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reaction {
	/// Verdicts committed together. At most one transition should be present.
	pub verdicts: Vec<Verdict>,
}

impl Reaction {
	/// Constructs a one-verdict reaction.
	pub fn one(verdict: Verdict) -> Self {
		Self { verdicts: vec![verdict] }
	}

	/// Appends a payload or transition to the atomic reaction.
	pub fn push(&mut self, verdict: Verdict) {
		self.verdicts.push(verdict);
	}
}

/// Terminal transition selected by the arbiter.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum WinnerKind {
	/// No transition was requested.
	Pass,
	/// At least one lane requested another turn.
	Continue,
	/// A forced choice became the queue head.
	Force,
	/// At least one attempt was denied.
	Deny,
	/// At least one required-deadline hold is unresolved.
	Hold,
	/// The stream or batch was cut.
	Cut,
	/// A terminal fault won.
	Fault,
}

/// Owned result of one deterministic N-lane fold.
#[derive(Clone, Debug)]
pub struct CampaignFold {
	/// Winning transition class.
	pub winner:      WinnerKind,
	/// Engagement that supplied the exclusive winner, if any.
	pub winner_lane: Option<EngagementId>,
	/// Ordered context rewrites.
	pub patches:     Vec<ContextPatch>,
	/// Accumulated canonical injections.
	pub injects:     Vec<Item>,
	/// Union of denial reasons.
	pub denials:     Vec<Str>,
	/// Unresolved hold tickets.
	pub holds:       Vec<HoldTicket>,
	/// Stack bindings emitted by this fold.
	pub binds:       Vec<ScopedBinding>,
	/// Every lane that participated, in deterministic order.
	pub lanes:       Vec<EngagementId>,
	/// Lanes removed after reaching a terminal.
	pub terminated:  Vec<EngagementId>,
}

impl Default for CampaignFold {
	fn default() -> Self {
		Self {
			winner:      WinnerKind::Pass,
			winner_lane: None,
			patches:     Vec::new(),
			injects:     Vec::new(),
			denials:     Vec::new(),
			holds:       Vec::new(),
			binds:       Vec::new(),
			lanes:       Vec::new(),
			terminated:  Vec::new(),
		}
	}
}

#[derive(Clone, Debug)]
struct SlotOwner {
	slot:       SlotClaim,
	engagement: EngagementId,
	since:      u64,
}

#[derive(Clone, Debug)]
struct QueuedClaim {
	engagement: EngagementId,
	since:      u64,
}

/// Result of one exclusive slot claim.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClaimOutcome {
	/// The requester owns the slot now.
	Granted,
	/// The requester was filed behind the current owner.
	Queued {
		/// Current slot owner.
		holder: EngagementId,
		/// Epoch millisecond at which the holder engaged.
		since:  u64,
	},
	/// The current owner rejected a non-queueing request.
	Denied {
		/// Current slot owner.
		holder: EngagementId,
		/// Epoch millisecond at which the holder engaged.
		since:  u64,
	},
}

/// A campaign declaration references an unknown canonical slot.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum DeclareError {
	/// The claim name is absent from [`SLOT_TABLE`].
	#[error("campaign declaration names an unknown slot")]
	UnknownSlot {
		/// Unknown claim.
		slot: SlotClaim,
	},
}

/// Registry for named exclusive claims and stack-addressed bindings.
pub struct SlotRegistry {
	declarations: BTreeMap<Str, SlotDecl>,
	owners:       BTreeMap<Str, SlotOwner>,
	queues:       BTreeMap<Str, VecDeque<QueuedClaim>>,
	bindings:     BTreeMap<BindSlot, Vec<(EngagementId, Str)>>,
}

impl Default for SlotRegistry {
	fn default() -> Self {
		let declarations = SLOT_TABLE
			.into_iter()
			.map(|declaration| (Str::new_static(declaration.name), declaration))
			.collect();
		Self {
			declarations,
			owners: BTreeMap::new(),
			queues: BTreeMap::new(),
			bindings: BTreeMap::new(),
		}
	}
}

impl SlotRegistry {
	/// Returns the canonical declaration for one claim.
	pub fn declaration(&self, slot: &SlotClaim) -> Option<&SlotDecl> {
		self.declarations.get(slot.name())
	}

	/// Validates every claim named by a campaign declaration.
	pub fn declare(&self, spec: &CampaignSpec) -> Result<(), DeclareError> {
		for slot in spec.claims.iter() {
			if self.declaration(slot).is_none() {
				return Err(DeclareError::UnknownSlot { slot: slot.clone() });
			}
		}
		Ok(())
	}

	/// Attempts to acquire one exclusive slot.
	pub fn claim(
		&mut self,
		slot: SlotClaim,
		engagement: EngagementId,
		since: u64,
		queue: bool,
	) -> Result<ClaimOutcome, DeclareError> {
		let declaration = self
			.declaration(&slot)
			.copied()
			.ok_or_else(|| DeclareError::UnknownSlot { slot: slot.clone() })?;
		if let Some(owner) = self.owners.get(slot.name()) {
			if owner.engagement == engagement {
				return Ok(ClaimOutcome::Granted);
			}
			let outcome = if queue && declaration.queueable {
				let waiting = self.queues.entry(Str::new(slot.name())).or_default();
				if !waiting
					.iter()
					.any(|candidate| candidate.engagement == engagement)
				{
					waiting.push_back(QueuedClaim { engagement, since });
				}
				ClaimOutcome::Queued { holder: owner.engagement.clone(), since: owner.since }
			} else {
				ClaimOutcome::Denied { holder: owner.engagement.clone(), since: owner.since }
			};
			return Ok(outcome);
		}
		self
			.owners
			.insert(Str::new(slot.name()), SlotOwner { slot, engagement, since });
		Ok(ClaimOutcome::Granted)
	}

	/// Releases every claim and binding owned by an engagement.
	pub fn release(&mut self, engagement: &str) -> Vec<(SlotClaim, EngagementId)> {
		let released: Vec<_> = self
			.owners
			.iter()
			.filter(|(_, owner)| owner.engagement == engagement)
			.map(|(name, owner)| (name.clone(), owner.slot.clone()))
			.collect();
		let mut granted = Vec::new();
		for (name, slot) in released {
			self.owners.remove(name.as_str());
			if let Some(next) = self
				.queues
				.get_mut(name.as_str())
				.and_then(VecDeque::pop_front)
			{
				self.owners.insert(name, SlotOwner {
					slot:       slot.clone(),
					engagement: next.engagement.clone(),
					since:      next.since,
				});
				granted.push((slot, next.engagement));
			}
		}
		for waiting in self.queues.values_mut() {
			waiting.retain(|candidate| candidate.engagement != engagement);
		}
		for stack in self.bindings.values_mut() {
			stack.retain(|(owner, _)| owner != engagement);
		}
		granted
	}

	/// Pushes one value on its addressed LIFO stack.
	pub fn bind(&mut self, engagement: EngagementId, binding: ScopedBinding) {
		self
			.bindings
			.entry(binding.slot)
			.or_default()
			.push((engagement, binding.value));
	}

	/// Reads the current top value without allocating.
	pub fn binding(&self, slot: &BindSlot) -> Option<&str> {
		self
			.bindings
			.get(slot)
			.and_then(|stack| stack.last())
			.map(|(_, value)| value.as_str())
	}

	/// Pops the current top binding.
	pub fn pop_binding(&mut self, slot: &BindSlot) -> Option<(EngagementId, Str)> {
		self.bindings.get_mut(slot).and_then(Vec::pop)
	}

	/// Returns the current exclusive owner.
	pub fn owner(&self, slot: &SlotClaim) -> Option<&str> {
		self
			.owners
			.get(slot.name())
			.map(|owner| owner.engagement.as_str())
	}

	/// Returns holder identity and engagement time for a claimed slot.
	pub fn holder(&self, slot: &SlotClaim) -> Option<(&str, u64)> {
		self
			.owners
			.get(slot.name())
			.map(|owner| (owner.engagement.as_str(), owner.since))
	}

	/// Returns the durable FIFO depth for one slot.
	pub fn queue_depth(&self, slot: &SlotClaim) -> usize {
		self.queues.get(slot.name()).map_or(0, VecDeque::len)
	}
}

#[derive(Clone, Copy, Debug)]
enum ForceFeedback {
	Resolved,
	Rejected,
}

#[derive(Clone, Debug)]
struct ForceEvent {
	engagement: EngagementId,
	outcome:    ForceFeedback,
}

struct Engagement {
	spec:             Arc<CampaignSpec>,
	id:               EngagementId,
	engaged_at:       Ulid,
	engaged_since_ms: u64,
	cursor:           usize,
	last_step_at:     Option<u64>,
	machine:          Box<dyn CampaignMachine>,
	parent:           Option<EngagementId>,
	last:             Option<WinnerKind>,
	queued:           bool,
}

/// Options controlling claim arbitration for one engagement.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EngageOptions {
	/// Epoch millisecond recorded as the claim-holder start.
	pub now_ms: u64,
	/// File a durable FIFO ticket when a queueable slot is occupied.
	pub queue:  bool,
}

/// Result of an accepted active or queued engagement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EngageReceipt {
	/// Stable engagement or queue-ticket identity.
	pub engagement: EngagementId,
	/// Conflicting slot for a queued ticket; absent for an immediate grant.
	pub slot:       Option<SlotClaim>,
	/// Aggregate claim result.
	pub outcome:    ClaimOutcome,
}

/// Result of explicitly stepping one campaign ladder.
#[derive(Clone, Debug, PartialEq)]
pub enum CampaignStepResult {
	/// No engagement matched the identity.
	Missing,
	/// The ladder remains active at this rung.
	Advanced {
		/// Current rung, absent for a scope-bounded machine.
		step: Option<LadderStep>,
	},
	/// The bound tripped and emitted its exhaust verdict.
	Terminal {
		/// Declared exhaustion verdict.
		verdict: Verdict,
	},
}

/// Durable owner of active and queued campaign engagements.
pub struct CampaignStack {
	engagements: BTreeMap<EngagementId, Engagement>,
	slots:       SlotRegistry,
	force_tx:    flume::Sender<ForceEvent>,
	force_rx:    flume::Receiver<ForceEvent>,
}

impl Default for CampaignStack {
	fn default() -> Self {
		let (force_tx, force_rx) = flume::unbounded();
		Self { engagements: BTreeMap::new(), slots: SlotRegistry::default(), force_tx, force_rx }
	}
}

/// Failure to engage a lane.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum EngageError {
	/// The declaration is invalid.
	#[error(transparent)]
	Declare(#[from] DeclareError),
	/// A finite policy declared no rung.
	#[error("campaign ladder is empty")]
	EmptyLadder,
	/// Another engagement already uses the same identity.
	#[error("campaign engagement identity is already active")]
	Duplicate,
	/// A named exclusive slot rejected this engagement.
	#[error("campaign slot claim was denied")]
	Claim {
		/// Conflicting slot.
		slot:    SlotClaim,
		/// Structured arbitration result, normally [`ClaimOutcome::Denied`].
		outcome: ClaimOutcome,
	},
}

/// Failure to leave a campaign engagement.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum DisengageError {
	/// A non-cut exit arrived before the declared dwell elapsed.
	#[error("campaign dwell has not elapsed")]
	Dwelling {
		/// Engagement refusing the exit.
		engagement: EngagementId,
		/// Earliest permitted non-cut exit, as epoch milliseconds.
		until_ms:   u64,
	},
}

impl CampaignStack {
	/// Creates an empty durable campaign owner.
	pub fn new() -> Self {
		Self::default()
	}

	/// Validates a declaration against the canonical slot table.
	pub fn declare(&self, spec: &CampaignSpec) -> Result<(), DeclareError> {
		self.slots.declare(spec)
	}

	/// Engages one machine after atomically acquiring all declared claims.
	pub fn engage(
		&mut self,
		spec: Arc<CampaignSpec>,
		machine: Box<dyn CampaignMachine>,
		options: EngageOptions,
	) -> Result<EngageReceipt, EngageError> {
		self.engage_member(spec, machine, None, options)
	}

	/// Engages a declared machine only when its Core-side predicate matches.
	pub fn engage_when(
		&mut self,
		spec: Arc<CampaignSpec>,
		machine: Box<dyn CampaignMachine>,
		options: EngageOptions,
		point: Point,
		cx: &crate::arbiter::PointCx<'_>,
	) -> Result<Option<EngageReceipt>, EngageError> {
		if !spec
			.when
			.as_ref()
			.is_some_and(|when| when.matches(point, cx))
		{
			return Ok(None);
		}
		self.engage(spec, machine, options).map(Some)
	}

	/// Engages a scope-tied member of an existing lane.
	pub fn engage_member(
		&mut self,
		spec: Arc<CampaignSpec>,
		machine: Box<dyn CampaignMachine>,
		parent: Option<EngagementId>,
		options: EngageOptions,
	) -> Result<EngageReceipt, EngageError> {
		self.declare(&spec)?;
		if spec.ladder.as_ref().is_some_and(Ladder::is_empty) {
			return Err(EngageError::EmptyLadder);
		}
		let engaged_at = Ulid::generate();
		let id = Str::from(engaged_at.to_string());
		if self.engagements.contains_key(id.as_str()) {
			return Err(EngageError::Duplicate);
		}
		let mut conflict = None;
		for claim in spec.claims.iter() {
			match self
				.slots
				.claim(claim.clone(), id.clone(), options.now_ms, false)?
			{
				ClaimOutcome::Granted => {},
				outcome @ (ClaimOutcome::Denied { .. } | ClaimOutcome::Queued { .. }) => {
					conflict = Some((claim.clone(), outcome));
					break;
				},
			}
		}
		if let Some((slot, denied)) = conflict {
			self.slots.release(id.as_str());
			if !options.queue {
				return Err(EngageError::Claim { slot, outcome: denied });
			}
			let outcome = self
				.slots
				.claim(slot.clone(), id.clone(), options.now_ms, true)?;
			if matches!(outcome, ClaimOutcome::Denied { .. }) {
				return Err(EngageError::Claim { slot, outcome });
			}
			self.engagements.insert(id.clone(), Engagement {
				spec,
				id: id.clone(),
				engaged_at,
				engaged_since_ms: options.now_ms,
				cursor: 0,
				last_step_at: None,
				machine,
				parent,
				last: None,
				queued: true,
			});
			return Ok(EngageReceipt { engagement: id, slot: Some(slot), outcome });
		}
		for binding in spec.binds.iter().cloned() {
			self.slots.bind(id.clone(), binding);
		}
		self.engagements.insert(id.clone(), Engagement {
			spec,
			id: id.clone(),
			engaged_at,
			engaged_since_ms: options.now_ms,
			cursor: 0,
			last_step_at: None,
			machine,
			parent,
			last: None,
			queued: false,
		});
		Ok(EngageReceipt { engagement: id, slot: None, outcome: ClaimOutcome::Granted })
	}

	/// Checks whether an engagement may leave without mutating it.
	pub fn check_disengage(&self, engagement: &str, now_ms: u64) -> Result<bool, DisengageError> {
		let Some(lane) = self.engagements.get(engagement) else {
			return Ok(false);
		};
		if !lane.queued
			&& let Some(dwell_ms) = lane.spec.dwell_ms
		{
			let until_ms = lane.engaged_since_ms.saturating_add(dwell_ms);
			if now_ms < until_ms {
				return Err(DisengageError::Dwelling { engagement: lane.id.clone(), until_ms });
			}
		}
		Ok(true)
	}

	/// Removes an engagement and its entire member subtree after dwell.
	pub fn disengage(&mut self, engagement: &str, now_ms: u64) -> Result<bool, DisengageError> {
		if !self.check_disengage(engagement, now_ms)? {
			return Ok(false);
		}
		Ok(self.remove_subtree(engagement))
	}

	/// Removes an engagement immediately after a winning cut.
	pub fn cut(&mut self, engagement: &str) -> bool {
		self.remove_subtree(engagement)
	}

	/// Marks a lane satisfied and removes its subtree after dwell.
	pub fn satisfy(&mut self, engagement: &str, now_ms: u64) -> Result<bool, DisengageError> {
		self.disengage(engagement, now_ms)
	}

	/// Advances one finite ladder explicitly, applying exhaustion at the bound.
	pub fn step(&mut self, engagement: &str, now_ms: u64) -> Option<Verdict> {
		let lane = self.engagements.get_mut(engagement)?;
		let ladder = lane.spec.ladder.as_ref()?;
		if ladder
			.min_interval()
			.zip(lane.last_step_at)
			.is_some_and(|(interval, last)| now_ms < last.saturating_add(interval))
		{
			return None;
		}
		lane.last_step_at = Some(now_ms);
		if lane.cursor.saturating_add(1) < ladder.len() {
			lane.cursor = lane.cursor.saturating_add(1);
			return ladder.step(lane.cursor).map(|step| step.verdict.clone());
		}
		if lane.cursor < ladder.len() {
			lane.cursor = ladder.len();
			return None;
		}
		let exhaust = lane.spec.exhaust.clone();
		self.cut(engagement);
		Some(exhaust_verdict(exhaust))
	}

	/// Explicitly steps a ladder and reports its durable rung or terminal.
	pub fn step_result(&mut self, engagement: &str, now_ms: u64) -> CampaignStepResult {
		if !self.engagements.contains_key(engagement) {
			return CampaignStepResult::Missing;
		}
		let verdict = self.step(engagement, now_ms);
		let Some(lane) = self.engagements.get(engagement) else {
			return CampaignStepResult::Terminal { verdict: verdict.unwrap_or(Verdict::Pass) };
		};
		CampaignStepResult::Advanced {
			step: lane
				.spec
				.ladder
				.as_ref()
				.and_then(|ladder| ladder.step(lane.cursor))
				.cloned(),
		}
	}

	fn remove_subtree(&mut self, engagement: &str) -> bool {
		let mut subtree = vec![Str::new(engagement)];
		let mut cursor = 0;
		while cursor < subtree.len() {
			let parent = subtree[cursor].clone();
			for child in self.engagements.values() {
				if child
					.parent
					.as_ref()
					.is_some_and(|candidate| candidate == &parent)
					&& !subtree.contains(&child.id)
				{
					subtree.push(child.id.clone());
				}
			}
			cursor += 1;
		}
		let mut removed = false;
		let mut grants = Vec::new();
		for id in subtree.into_iter().rev() {
			removed |= self.engagements.remove(id.as_str()).is_some();
			grants.extend(self.slots.release(id.as_str()));
		}
		for (_, granted) in grants {
			self.activate_grant(granted);
		}
		removed
	}

	fn activate_grant(&mut self, engagement: EngagementId) {
		let Some(lane) = self.engagements.get(&engagement) else {
			self.slots.release(engagement.as_str());
			return;
		};
		if !lane.queued {
			return;
		}
		let claims = Arc::clone(&lane.spec.claims);
		let since = lane.engaged_since_ms;
		for claim in claims.iter() {
			match self
				.slots
				.claim(claim.clone(), engagement.clone(), since, false)
			{
				Ok(ClaimOutcome::Granted) => {},
				Ok(denied @ (ClaimOutcome::Denied { .. } | ClaimOutcome::Queued { .. })) => {
					self.slots.release(engagement.as_str());
					let _ = self
						.slots
						.claim(claim.clone(), engagement.clone(), since, true);
					let _ = denied;
					return;
				},
				Err(_) => {
					self.slots.release(engagement.as_str());
					return;
				},
			}
		}
		if let Some(lane) = self.engagements.get_mut(&engagement) {
			for binding in lane.spec.binds.iter().cloned() {
				self.slots.bind(engagement.clone(), binding);
			}
			lane.queued = false;
		}
	}

	/// Folds every active lane subscribed to `point` in deterministic order.
	pub fn fold(
		&mut self,
		point: Point,
		cx: &crate::arbiter::PointCx<'_>,
		tool_choices: Option<&mut ToolChoiceQueue>,
	) -> CampaignFold {
		self.apply_force_feedback();
		let mut order: Vec<_> = self
			.engagements
			.values()
			.filter(|lane| !lane.queued && lane.spec.points.contains(point))
			.map(|lane| (lane.spec.precedence, lane.engaged_at, lane.id.clone()))
			.collect();
		order.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

		let mut fold = CampaignFold::default();
		let mut forces = Vec::new();
		let mut bounded_ticks = Vec::new();
		for (_, _, id) in order {
			let Some(lane) = self.engagements.get_mut(id.as_str()) else {
				continue;
			};
			fold.lanes.push(id.clone());
			if lane
				.spec
				.ladder
				.as_ref()
				.and_then(Ladder::min_interval)
				.zip(lane.last_step_at)
				.is_some_and(|(interval, last)| cx.now_ms < last.saturating_add(interval))
			{
				continue;
			}
			let reaction = lane.machine.react(point, cx);
			let mut pauses_bound = false;
			for verdict in reaction.verdicts {
				match verdict {
					Verdict::Pass => {},
					Verdict::Patch(patch) => fold.patches.push(patch),
					Verdict::Inject(mut items) => fold.injects.append(&mut items),
					Verdict::Bind(binding) => {
						self.slots.bind(id.clone(), binding.clone());
						fold.binds.push(binding);
					},
					Verdict::Continue => select_winner(&mut fold, WinnerKind::Continue, &id),
					Verdict::Force { tool } => forces.push((id.clone(), tool)),
					Verdict::Deny { reason } => {
						fold.denials.push(reason);
						select_winner(&mut fold, WinnerKind::Deny, &id);
					},
					Verdict::Hold(ticket) => {
						pauses_bound = true;
						fold.holds.push(ticket);
						select_winner(&mut fold, WinnerKind::Hold, &id);
					},
					Verdict::Cut { .. } => select_winner(&mut fold, WinnerKind::Cut, &id),
					Verdict::Fault { .. } | Verdict::Kill { .. } => {
						select_winner(&mut fold, WinnerKind::Fault, &id);
					},
					Verdict::Done => fold.terminated.push(id.clone()),
				}
			}
			if cx.delivered
				&& lane.spec.ladder.is_some()
				&& !pauses_bound
				&& !forces.iter().any(|(owner, _)| owner == &id)
			{
				bounded_ticks.push(id);
			}
		}

		if let Some((id, _)) = forces.first() {
			if !matches!(
				fold.winner,
				WinnerKind::Cut | WinnerKind::Hold | WinnerKind::Deny | WinnerKind::Fault
			) {
				fold.winner = WinnerKind::Force;
				fold.winner_lane = Some(id.clone());
			}
			if let Some(queue) = tool_choices {
				for (position, (engagement, tool)) in forces.into_iter().enumerate() {
					self.queue_force(queue, engagement, tool, position == 0);
				}
			}
		}

		for lane in &fold.lanes {
			if let Some(active) = self.engagements.get_mut(lane.as_str()) {
				active.last = Some(fold.winner);
			}
		}
		for lane in bounded_ticks {
			if let Some(verdict) = self.step(lane.as_str(), cx.now_ms) {
				match verdict {
					Verdict::Fault { .. } | Verdict::Kill { .. } => {
						select_winner(&mut fold, WinnerKind::Fault, &lane);
					},
					Verdict::Deny { reason } => {
						fold.denials.push(reason);
						select_winner(&mut fold, WinnerKind::Deny, &lane);
					},
					Verdict::Cut { .. } => select_winner(&mut fold, WinnerKind::Cut, &lane),
					Verdict::Continue => select_winner(&mut fold, WinnerKind::Continue, &lane),
					_ => {},
				}
			}
		}
		let mut terminated = Vec::new();
		for candidate in std::mem::take(&mut fold.terminated) {
			if matches!(self.disengage(candidate.as_str(), cx.now_ms), Ok(true)) {
				terminated.push(candidate);
			}
		}
		fold.terminated = terminated;
		fold
	}

	/// Returns the active lane count.
	pub fn len(&self) -> usize {
		self.engagements.len()
	}

	/// Returns whether no campaign is active.
	pub fn is_empty(&self) -> bool {
		self.engagements.is_empty()
	}

	/// Returns the current slot registry.
	pub const fn slots(&self) -> &SlotRegistry {
		&self.slots
	}

	/// Returns mutable access to slot bindings for loop-owned one-shot pops.
	pub(crate) const fn slots_mut(&mut self) -> &mut SlotRegistry {
		&mut self.slots
	}

	/// Resolves an engagement identity to its stable declaration identity.
	pub fn spec_id(&self, engagement: &str) -> Option<&str> {
		self
			.engagements
			.get(engagement)
			.map(|lane| lane.spec.id.as_str())
	}

	/// Returns whether an accepted engagement is waiting in a slot queue.
	pub fn is_queued(&self, engagement: &str) -> bool {
		self
			.engagements
			.get(engagement)
			.is_some_and(|lane| lane.queued)
	}

	/// Applies a live machine-state update and returns its durable record.
	pub fn update_state(
		&mut self,
		engagement: &str,
		payload: &[u8],
	) -> Result<CampaignEntry, CampaignStateError> {
		let lane = self
			.engagements
			.get_mut(engagement)
			.ok_or(CampaignStateError::MissingEngagement)?;
		lane.machine.update(payload)?;
		self
			.entries()
			.into_iter()
			.find(|entry| entry.engagement == engagement)
			.ok_or(CampaignStateError::MissingEngagement)
	}

	/// Produces durable records for every active engagement and queue ticket.
	pub fn entries(&self) -> Vec<CampaignEntry> {
		self
			.engagements
			.values()
			.map(|lane| CampaignEntry {
				spec_id:          lane.spec.id.clone(),
				family_rev:       lane.spec.family_rev.clone(),
				state:            lane.machine.state(),
				ladder_position:  u32::try_from(lane.cursor).unwrap_or(u32::MAX),
				engagement:       lane.id.clone(),
				engaged_at:       lane.engaged_at.to_string().into(),
				engaged_since_ms: lane.engaged_since_ms,
				parent:           lane.parent.clone(),
				status:           if lane.queued {
					CampaignEntryStatus::Queued
				} else {
					CampaignEntryStatus::Engaged
				},
			})
			.collect()
	}

	/// Rebuilds active engagements from their latest durable entries.
	///
	/// Missing specs, invalid state, and malformed identities degrade to
	/// exhausted records instead of retaining an unserviceable lane.
	pub fn revive<F>(
		&mut self,
		entries: impl IntoIterator<Item = CampaignEntry>,
		mut resolve: F,
	) -> RevivalReport
	where
		F: FnMut(&str) -> Option<(Arc<CampaignSpec>, Box<dyn CampaignMachine>)>,
	{
		let mut report = RevivalReport::default();
		let mut entries = entries.into_iter().collect::<Vec<_>>();
		entries.sort_by(|left, right| left.engaged_at.cmp(&right.engaged_at));
		for mut entry in entries {
			if !matches!(entry.status, CampaignEntryStatus::Engaged | CampaignEntryStatus::Queued) {
				continue;
			}
			let Some((spec, mut machine)) = resolve(entry.spec_id.as_str()) else {
				entry.status = CampaignEntryStatus::Exhausted;
				report.exhausted.push(entry);
				continue;
			};
			let Ok(engaged_at) = Ulid::from_string(entry.engaged_at.as_str()) else {
				entry.status = CampaignEntryStatus::Exhausted;
				report.exhausted.push(entry);
				continue;
			};
			if spec.family_rev != entry.family_rev
				|| self.declare(&spec).is_err()
				|| machine.restore(entry.state.as_str()).is_err()
			{
				entry.status = CampaignEntryStatus::Exhausted;
				report.exhausted.push(entry);
				continue;
			}
			let wants_queue = entry.status == CampaignEntryStatus::Queued;
			let mut queued = false;
			let mut claimed = true;
			for claim in spec.claims.iter() {
				let result = self.slots.claim(
					claim.clone(),
					entry.engagement.clone(),
					entry.engaged_since_ms,
					wants_queue,
				);
				if wants_queue {
					if !matches!(result, Ok(ClaimOutcome::Granted | ClaimOutcome::Queued { .. })) {
						claimed = false;
						break;
					}
					if matches!(result, Ok(ClaimOutcome::Queued { .. })) {
						queued = true;
						break;
					}
				} else if !matches!(result, Ok(ClaimOutcome::Granted)) {
					claimed = false;
					break;
				}
			}
			if !claimed {
				self.slots.release(entry.engagement.as_str());
				entry.status = CampaignEntryStatus::Exhausted;
				report.exhausted.push(entry);
				continue;
			}
			let cursor = usize::try_from(entry.ladder_position).unwrap_or(usize::MAX);
			self
				.engagements
				.insert(entry.engagement.clone(), Engagement {
					spec,
					id: entry.engagement.clone(),
					engaged_at,
					engaged_since_ms: entry.engaged_since_ms,
					cursor,
					last_step_at: None,
					machine,
					parent: entry.parent.clone(),
					last: None,
					queued,
				});
			if !queued && let Some(lane) = self.engagements.get(entry.engagement.as_str()) {
				for binding in lane.spec.binds.iter().cloned() {
					self.slots.bind(entry.engagement.clone(), binding);
				}
			}
			report.resumed.push(entry.engagement);
		}
		report
	}

	fn queue_force(
		&self,
		queue: &mut ToolChoiceQueue,
		engagement: EngagementId,
		tool: Str,
		head: bool,
	) {
		let pending_invoker = (tool == "dyn").then(|| queue.pending_invoker()).flatten();
		let resolved_tx = self.force_tx.clone();
		let rejected_tx = self.force_tx.clone();
		let resolved_id = engagement.clone();
		let rejected_id = engagement.clone();
		queue.push_once(ToolChoice::Named(tool), PushOptions {
			priority:  if head {
				DirectivePriority::Head
			} else {
				DirectivePriority::Tail
			},
			label:     Some(engagement),
			callbacks: DirectiveCallbacks {
				on_resolved: Some(Arc::new(move |_| {
					let _ = resolved_tx.send(ForceEvent {
						engagement: resolved_id.clone(),
						outcome:    ForceFeedback::Resolved,
					});
				})),
				on_rejected: Some(Arc::new(move |_| {
					let _ = rejected_tx.send(ForceEvent {
						engagement: rejected_id.clone(),
						outcome:    ForceFeedback::Rejected,
					});
					RejectOutcome::Drop
				})),
				on_invoked:  pending_invoker,
			},
		});
	}

	fn apply_force_feedback(&mut self) {
		while let Ok(event) = self.force_rx.try_recv() {
			if matches!(event.outcome, ForceFeedback::Resolved) {
				let _ = self.step(event.engagement.as_str(), crate::r#loop::now_ms());
			}
		}
	}
}

fn winner_rank(kind: WinnerKind) -> u8 {
	match kind {
		WinnerKind::Pass => 0,
		WinnerKind::Continue => 1,
		WinnerKind::Force => 2,
		WinnerKind::Deny => 3,
		WinnerKind::Hold => 4,
		WinnerKind::Cut => 5,
		WinnerKind::Fault => 6,
	}
}

fn select_winner(fold: &mut CampaignFold, candidate: WinnerKind, lane: &EngagementId) {
	if winner_rank(candidate) > winner_rank(fold.winner) {
		fold.winner = candidate;
		fold.winner_lane = Some(lane.clone());
	}
}
pub(crate) fn absorb_lane(fold: &mut CampaignFold, lane: EngagementId, reaction: Reaction) {
	fold.lanes.push(lane.clone());
	for verdict in reaction.verdicts {
		match verdict {
			Verdict::Pass => {},
			Verdict::Patch(patch) => fold.patches.push(patch),
			Verdict::Inject(mut items) => fold.injects.append(&mut items),
			Verdict::Bind(binding) => fold.binds.push(binding),
			Verdict::Continue => select_winner(fold, WinnerKind::Continue, &lane),
			Verdict::Force { .. } => select_winner(fold, WinnerKind::Force, &lane),
			Verdict::Deny { reason } => {
				fold.denials.push(reason);
				select_winner(fold, WinnerKind::Deny, &lane);
			},
			Verdict::Hold(ticket) => {
				fold.holds.push(ticket);
				select_winner(fold, WinnerKind::Hold, &lane);
			},
			Verdict::Cut { .. } => select_winner(fold, WinnerKind::Cut, &lane),
			Verdict::Fault { .. } | Verdict::Kill { .. } => {
				select_winner(fold, WinnerKind::Fault, &lane);
			},
			Verdict::Done => fold.terminated.push(lane.clone()),
		}
	}
}

/// Bounded subagent structured-yield escalation.
#[derive(Default)]
pub struct SubagentYieldCampaign {
	rung: u8,
}

impl CampaignMachine for SubagentYieldCampaign {
	fn react(&mut self, point: Point, _: &crate::arbiter::PointCx<'_>) -> Reaction {
		match (self.rung, point) {
			(0, Point::Settle) => {
				self.rung = 1;
				Reaction {
					verdicts: vec![
						Verdict::Inject(vec![campaign_message(
							"Return the required structured yield payload now.",
						)]),
						Verdict::Continue,
					],
				}
			},
			(1, Point::Stream) => {
				self.rung = 2;
				Reaction::one(Verdict::Cut { reason: Str::new_static("yield budget exceeded") })
			},
			(2, Point::ToolChoice) => {
				self.rung = 3;
				Reaction::one(Verdict::Force { tool: Str::new_static("yield") })
			},
			(3, _) => {
				self.rung = 4;
				Reaction::one(Verdict::Kill {
					exit:   1,
					reason: Str::new_static("structured yield campaign exhausted"),
				})
			},
			_ => Reaction::one(Verdict::Done),
		}
	}

	fn state(&self) -> Str {
		Str::from(self.rung.to_string())
	}

	fn restore(&mut self, payload: &str) -> Result<(), CampaignStateError> {
		self.rung = payload
			.parse()
			.map_err(|_| CampaignStateError::InvalidPayload)?;
		Ok(())
	}
}

/// SETTLE barrier that vetoes stop while agent-loop jobs remain pending.
pub struct QuiescenceBarrier {
	jobs:    Arc<crate::JobBoard>,
	pending: Vec<Item>,
}

impl QuiescenceBarrier {
	/// Creates a barrier over the authoritative job board.
	pub fn new(jobs: Arc<crate::JobBoard>) -> Self {
		Self { jobs, pending: Vec::new() }
	}

	/// Queues one settled async-result injection for the next veto.
	pub fn push_async_result(&mut self, item: Item) {
		self.pending.push(item);
	}
}

impl CampaignMachine for QuiescenceBarrier {
	fn react(&mut self, point: Point, _: &crate::arbiter::PointCx<'_>) -> Reaction {
		if point != Point::Settle {
			return Reaction::one(Verdict::Pass);
		}
		if self.jobs.is_empty() {
			return Reaction::one(Verdict::Done);
		}
		Reaction {
			verdicts: vec![
				Verdict::Deny { reason: Str::new_static("agent-loop jobs pending") },
				Verdict::Inject(std::mem::take(&mut self.pending)),
			],
		}
	}

	fn state(&self) -> Str {
		Str::new_static("{}")
	}

	fn restore(&mut self, payload: &str) -> Result<(), CampaignStateError> {
		if payload == "{}" {
			Ok(())
		} else {
			Err(CampaignStateError::InvalidPayload)
		}
	}
}

/// Legacy `session_stop` compatibility campaign with the global bound of eight.
#[derive(Default)]
pub struct LegacySessionStopCampaign {
	spent: u8,
}

impl CampaignMachine for LegacySessionStopCampaign {
	fn react(&mut self, point: Point, _: &crate::arbiter::PointCx<'_>) -> Reaction {
		if point != Point::Settle {
			return Reaction::one(Verdict::Pass);
		}
		if self.spent >= 8 {
			return Reaction::one(Verdict::Done);
		}
		self.spent = self.spent.saturating_add(1);
		Reaction::one(Verdict::Continue)
	}

	fn state(&self) -> Str {
		Str::from(self.spent.to_string())
	}

	fn restore(&mut self, payload: &str) -> Result<(), CampaignStateError> {
		self.spent = payload
			.parse()
			.map_err(|_| CampaignStateError::InvalidPayload)?;
		Ok(())
	}
}

fn campaign_message(text: impl Into<String>) -> Item {
	use omp_proto::thread::v1::{self as thread};
	Item {
		created_at_ms: crate::r#loop::now_ms(),
		kind: Some(thread::item::Kind::Message(thread::Message {
			role:  thread::Role::User as i32,
			parts: vec![thread::Part { kind: Some(thread::part::Kind::Text(text.into())) }],
		})),
		..Item::default()
	}
}

fn exhaust_verdict(policy: ExhaustPolicy) -> Verdict {
	match policy {
		ExhaustPolicy::Settle => Verdict::Pass,
		ExhaustPolicy::Fault { detail } => Verdict::Fault { detail },
		ExhaustPolicy::Verdict(verdict) => verdict,
	}
}

/// Durable first-class engagement record stored by the journal.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CampaignEntry {
	/// Stable declaration identity.
	pub spec_id:          CampaignSpecId,
	/// State schema identity (`family@rev`).
	pub family_rev:       Str,
	/// Opaque typed-state payload.
	pub state:            Str,
	/// Current finite ladder cursor.
	pub ladder_position:  u32,
	/// Stable engagement ULID string.
	pub engagement:       EngagementId,
	/// Ordering ULID retained separately for forensic readability.
	pub engaged_at:       Str,
	/// Epoch millisecond restored into slot-holder diagnostics.
	pub engaged_since_ms: u64,
	/// Parent engagement for scope-tied subtree revival.
	pub parent:           Option<EngagementId>,
	/// Lifecycle transition represented by this record.
	pub status:           CampaignEntryStatus,
}
/// Outcome of rebuilding an [`CampaignStack`] from journal state.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RevivalReport {
	/// Engagement identities restored at their exact cursors.
	pub resumed:   Vec<EngagementId>,
	/// Unloadable entries degraded to `Exhausted(Settle)`.
	pub exhausted: Vec<CampaignEntry>,
}

/// Lifecycle represented by one [`CampaignEntry`].
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[derive(strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum CampaignEntryStatus {
	/// Lane is active at this cursor.
	Engaged,
	/// Engagement is a durable FIFO claim ticket.
	Queued,
	/// Lane reached success and was removed.
	Satisfied,
	/// Lane exhausted and was removed.
	Exhausted,
	/// Lane was explicitly removed.
	Disengaged,
}

/// Shared set of required-deadline holds used by PRE_MODEL and ADMISSION.
#[derive(Clone, Default)]
pub struct HoldSet {
	inner: Arc<HoldSetInner>,
}

#[derive(Default)]
struct HoldSetInner {
	tickets: Mutex<BTreeMap<Str, HoldTicket>>,
	notify:  tokio::sync::Notify,
}

impl HoldSet {
	/// Inserts or replaces one ticket. A zero deadline is rejected.
	pub fn insert(&self, ticket: HoldTicket) -> Result<(), HoldError> {
		if ticket.deadline_ms == 0 {
			return Err(HoldError::MissingDeadline);
		}
		self.inner.tickets.lock().insert(ticket.id.clone(), ticket);
		self.inner.notify.notify_waiters();
		Ok(())
	}

	/// Resolves one ticket idempotently.
	pub fn resolve(&self, id: &str) -> bool {
		let removed = self.inner.tickets.lock().remove(id).is_some();
		if removed {
			self.inner.notify.notify_waiters();
		}
		removed
	}

	/// Parks until every ticket resolves, a deadline elapses, or abort changes.
	pub async fn wait_empty(
		&self,
		mut abort: tokio::sync::watch::Receiver<u64>,
	) -> Result<(), HoldError> {
		loop {
			let deadline = {
				let tickets = self.inner.tickets.lock();
				if tickets.is_empty() {
					return Ok(());
				}
				tickets
					.values()
					.map(|ticket| ticket.deadline_ms)
					.min()
					.expect("nonempty")
			};
			let now = crate::r#loop::now_ms();
			if now >= deadline {
				return Err(HoldError::Deadline);
			}
			let sleep = tokio::time::sleep(std::time::Duration::from_millis(deadline - now));
			tokio::pin!(sleep);
			tokio::select! {
				() = self.inner.notify.notified() => {},
				_ = &mut sleep => return Err(HoldError::Deadline),
				changed = abort.changed() => {
					if changed.is_ok() { return Err(HoldError::Aborted); }
				},
			}
		}
	}
}

/// Failure while parking on campaign holds.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum HoldError {
	/// A hold omitted its mandatory deadline.
	#[error("campaign hold requires a deadline")]
	MissingDeadline,
	/// A hold reached its deadline.
	#[error("campaign hold deadline elapsed")]
	Deadline,
	/// The submission abort watch changed.
	#[error("campaign hold was aborted")]
	Aborted,
}

#[cfg(test)]
mod builtin_tests {
	use omp_env::EnvClient;
	use omp_tool::{ArtifactLifetime, ExpectedArtifact, JobOwner, JobRef};

	use super::*;
	use crate::mailbox::Mailbox;

	#[test]
	fn subagent_yield_exhausts_to_kill_after_force() {
		let mut campaign = SubagentYieldCampaign::default();
		let cx = crate::PointCx::default();
		assert!(matches!(campaign.react(Point::Settle, &cx).verdicts.as_slice(), [
			Verdict::Inject(_),
			Verdict::Continue
		]));
		assert!(matches!(campaign.react(Point::Stream, &cx).verdicts.as_slice(), [
			Verdict::Cut { .. }
		]));
		assert!(matches!(
			campaign.react(Point::ToolChoice, &cx).verdicts.as_slice(),
			[Verdict::Force { tool }] if tool == "yield"
		));
		assert!(matches!(campaign.react(Point::Settle, &cx).verdicts.as_slice(), [Verdict::Kill {
			exit: 1,
			..
		}]));
	}

	#[test]
	fn quiescence_exhausts_when_the_last_job_settles() {
		let mailbox = Mailbox::new();
		let (env, _transport) = EnvClient::in_process(0);
		let board = Arc::new(crate::JobBoard::new(env, mailbox.sender()));
		let job = JobRef {
			id:       Str::new_static("job-1"),
			owner:    JobOwner::AgentLoop { agent_id: Str::new_static("agent") },
			metadata: Arc::default(),
			artifact: ExpectedArtifact {
				description: Str::new_static("test"),
				media_type:  None,
				lifetime:    ArtifactLifetime::Session,
			},
		};
		assert!(board.register(job));
		let mut campaign = QuiescenceBarrier::new(Arc::clone(&board));
		assert!(matches!(
			campaign
				.react(Point::Settle, &crate::PointCx::default())
				.verdicts
				.first(),
			Some(Verdict::Deny { .. })
		));
		board.settle("job-1", Item::default()).unwrap();
		assert!(matches!(
			campaign
				.react(Point::Settle, &crate::PointCx::default())
				.verdicts
				.as_slice(),
			[Verdict::Done]
		));
	}

	#[test]
	fn goal_state_emits_threshold_once_and_faults_at_budget() {
		let mut campaign = GoalCampaign::default();
		let at_half = serde_json::to_vec(&GoalCampaignState {
			objective:          Str::new_static("ship"),
			budget_tokens:      Some(100),
			spent_tokens:       50,
			thresholds_crossed: 0,
		})
		.unwrap();
		campaign.update(&at_half).unwrap();
		assert!(matches!(
			campaign
				.react(Point::Context, &crate::PointCx::default())
				.verdicts
				.as_slice(),
			[Verdict::Inject(_)]
		));
		assert!(matches!(
			campaign
				.react(Point::Context, &crate::PointCx::default())
				.verdicts
				.as_slice(),
			[Verdict::Pass]
		));
		let exhausted = serde_json::to_vec(&GoalCampaignState {
			objective:          Str::new_static("ship"),
			budget_tokens:      Some(100),
			spent_tokens:       100,
			thresholds_crossed: 1,
		})
		.unwrap();
		campaign.update(&exhausted).unwrap();
		assert!(matches!(
			campaign
				.react(Point::Context, &crate::PointCx::default())
				.verdicts
				.as_slice(),
			[Verdict::Fault { .. }]
		));
	}

	#[test]
	fn legacy_session_stop_exhausts_after_eight_continues() {
		let mut campaign = LegacySessionStopCampaign::default();
		for _ in 0..8 {
			assert!(matches!(
				campaign
					.react(Point::Settle, &crate::PointCx::default())
					.verdicts
					.as_slice(),
				[Verdict::Continue]
			));
		}
		assert!(matches!(
			campaign
				.react(Point::Settle, &crate::PointCx::default())
				.verdicts
				.as_slice(),
			[Verdict::Done]
		));
	}
}
