//! Application execution modes and autonomous goal-loop policy.

/// Durable encoding and restoration of autonomous campaign state.
pub mod persistence;

use std::sync::{
	Arc,
	atomic::{AtomicU64, Ordering},
};

use omp_agent::{
	AgentState, CachedContribution, CampaignEntry, CampaignEntryStatus, CampaignStack, Continuation,
	ContinuationPolicy, ContinuationSource, LoopSignal, PromptError, PromptSlotSource, PromptSource,
	Props, SLOT_TABLE, SlotAssembler, SlotClaim, SlotClass, SlotDecl, SlotId, SlotRegistration,
};
use omp_core::{Str, sf};
/// One visible campaign-slot holder projected by the driver.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VisibleSlotFacts {
	/// Canonical slot name.
	pub slot:        Str,
	/// Campaign declaration currently holding the slot.
	pub holder:      Str,
	/// Durable FIFO tickets waiting behind the holder.
	pub queue_depth: usize,
}
use omp_proto::thread::v1::{Item, Message, Part, Role, item, part};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
	goal::{self, report::GoalBudgetReport},
	plan::{
		ModelSelection, PlanModelTransition, PlanState, PlanWorkflow, TransitionQueue,
		artifacts::canonical_url,
	},
};

/// Process startup surface selected by CLI command or `--mode`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StartupMode {
	/// Interactive terminal chat.
	#[default]
	Interactive,
	/// Non-interactive single response.
	Print,
	/// Headless framed RPC.
	Rpc,
	/// Framed RPC with retained UI envelopes.
	RpcUi,
	/// Agent Client Protocol.
	Acp,
}

/// Mode-neutral protocol defaults. Protocol startup consumes these values
/// without mutating persisted interactive settings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProtocolDefaults {
	/// Automatic terminal titles.
	pub titles:             bool,
	/// PTY-backed shell execution.
	pub pty:                bool,
	/// Interactive splash/chrome.
	pub interactive_chrome: bool,
}

impl StartupMode {
	/// Returns invocation-local defaults for this mode.
	pub const fn defaults(self) -> ProtocolDefaults {
		match self {
			Self::Interactive => ProtocolDefaults {
				titles:             true,
				pty:                true,
				interactive_chrome: true,
			},
			Self::Print => ProtocolDefaults {
				titles:             false,
				pty:                false,
				interactive_chrome: false,
			},
			Self::Rpc | Self::RpcUi | Self::Acp => ProtocolDefaults {
				titles:             false,
				pty:                true,
				interactive_chrome: false,
			},
		}
	}

	/// Rejects `@file` shorthand in RPC UI, where stdin and references belong to
	/// the framed protocol.
	pub fn validate_prompt_words(self, words: &[Str]) -> Result<(), StartupRegimeError> {
		if self == Self::RpcUi && words.iter().any(|word| word.starts_with("@")) {
			return Err(StartupRegimeError::RpcUiReference);
		}
		Ok(())
	}
}

/// Startup-mode usage failure.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum StartupRegimeError {
	/// RPC UI accepts attachments only through typed protocol frames.
	#[error("rpc-ui does not accept @file arguments")]
	RpcUiReference,
}

/// Durable goal lifecycle state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
	/// Objective is eligible for continuation.
	Active,
	/// User-paused without losing accounting.
	Paused,
	/// Hard token budget was reached.
	BudgetLimited,
	/// Objective was achieved.
	Complete,
	/// Objective was abandoned.
	Dropped,
}

/// Goal state projected to commands, prompts, and continuation policy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Goal {
	/// Stable goal identity.
	pub id:                Str,
	/// User-authored objective.
	pub objective:         Str,
	/// Current lifecycle state.
	pub status:            GoalStatus,
	/// Optional hard token budget.
	pub token_budget:      Option<u64>,
	/// Counted tokens, excluding reused cache reads.
	pub tokens_used:       u64,
	/// Accumulated wall-clock seconds.
	pub time_used_seconds: u64,
	started_ms:            u64,
}

/// One provider usage delta folded into goal accounting.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct GoalUsage {
	/// Fresh input tokens.
	pub input_tokens:        u64,
	/// Newly written cache tokens.
	pub cache_write_tokens:  u64,
	/// Reused cache tokens, intentionally excluded from spend.
	pub cached_input_tokens: u64,
	/// Generated output tokens.
	pub output_tokens:       u64,
}

impl GoalUsage {
	/// Returns budget spend while excluding reused cached input.
	pub const fn charged_tokens(self) -> u64 {
		self
			.input_tokens
			.saturating_add(self.cache_write_tokens)
			.saturating_add(self.output_tokens)
	}
}

/// Invalid goal or campaign-projection transition.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum RegimeError {
	/// An operation requires a campaign that is not the visible mode holder.
	#[error("the {required} campaign is not active")]
	CampaignInactive {
		/// Required campaign declaration.
		required: &'static str,
	},
	/// A goal operation was requested without a goal.
	#[error("no goal is configured")]
	NoGoal,
	/// The objective was empty.
	#[error("goal objective must not be empty")]
	EmptyObjective,
	/// A zero token budget was supplied.
	#[error("goal token budget must be positive")]
	InvalidBudget,
	/// The plan artifact was not a canonical session-local URL.
	#[error("plan artifact must be a relative local:// URL")]
	InvalidPlanArtifact,
	/// The requested lifecycle transition is invalid for the current goal.
	#[error("cannot {operation} a goal in {status:?} state")]
	InvalidGoalTransition {
		/// Requested lifecycle operation.
		operation: &'static str,
		/// Current durable state.
		status:    GoalStatus,
	},
	/// A new goal cannot replace a live goal.
	#[error("cannot create a new goal while an unfinished goal exists")]
	GoalExists,
}

/// App-owned metadata paired with the authoritative campaign-slot projection.
#[derive(Debug, Default)]
struct RegimeProjectionState {
	mode_holder:             Option<Str>,
	mode_engagement:         Option<Str>,
	visible_slots:           Arc<[VisibleSlotFacts]>,
	goal:                    Option<Goal>,
	plan:                    PlanState,
	plan_seen:               bool,
	goal_usage_checkpoint:   GoalUsage,
	budget_steering_pending: bool,
	goal_todo_context:       Option<Str>,
}

#[derive(Clone, Debug)]
struct PlanBinding {
	agent:     AgentState,
	selection: Option<ModelSelection>,
	handoff:   Option<ModelSelection>,
}

/// Read projection of the agent-owned [`CampaignStack`] plus app goal/plan
/// metadata.
#[derive(Clone, Debug)]
pub struct CampaignHandle {
	state:            Arc<Mutex<RegimeProjectionState>>,
	policy:           ContinuationPolicy,
	plan_binding:     Arc<Mutex<Option<PlanBinding>>>,
	plan_transitions: Arc<TransitionQueue>,
	revision:         Arc<AtomicU64>,
}
struct ModeAwarePromptSource {
	base:  Arc<dyn PromptSource>,
	modes: CampaignHandle,
}

impl PromptSource for ModeAwarePromptSource {
	fn render(&self, workspace: &Props) -> Result<Vec<Item>, PromptError> {
		let mut items = self.base.render(workspace)?;
		let mut registrations = Vec::new();
		if let Some(slot) = self.modes.mode_holder() {
			registrations.push(PromptSlotSource::new(slot).registration());
		}
		if let Some(goal) = self
			.modes
			.holds_mode("goal")
			.then(|| self.modes.goal())
			.flatten()
			.filter(|goal| goal.status == GoalStatus::Active)
		{
			let todo = self.modes.goal_todo_context();
			registrations.push(SlotRegistration {
				decl:   SlotDecl {
					slot:     SlotId::Status,
					class:    SlotClass::Volatile,
					owner:    sf!("omp.goal"),
					priority: 110,
				},
				source: Arc::new(CachedContribution::new(goal::prompt_context(&goal, todo.as_deref()))),
			});
		}
		if !registrations.is_empty() {
			items.extend(SlotAssembler::new(registrations).render(workspace)?);
		}
		Ok(items)
	}
}

impl CampaignHandle {
	/// Creates an empty read projection. [`Self::sync_campaigns`] supplies
	/// authority.
	pub fn new() -> Self {
		Self {
			state:            Arc::new(Mutex::new(RegimeProjectionState::default())),
			policy:           ContinuationPolicy::default(),
			plan_binding:     Arc::new(Mutex::new(None)),
			plan_transitions: Arc::new(TransitionQueue::default()),
			revision:         Arc::new(AtomicU64::new(0)),
		}
	}

	/// Refreshes user-facing slot facts from the authoritative agent campaign
	/// stack.
	pub fn sync_campaigns(&self, campaigns: &CampaignStack) {
		let claims = [
			SlotClaim::Worktree,
			SlotClaim::Director,
			SlotClaim::EditorSurface,
			SlotClaim::BatchExecution,
			SlotClaim::Mode,
		];
		let visible_slots = claims
			.iter()
			.filter(|claim| {
				campaigns
					.slots()
					.declaration(claim)
					.is_some_and(|declaration| declaration.visible)
			})
			.filter_map(|claim| {
				let engagement = campaigns.slots().owner(claim)?;
				let holder = campaigns.spec_id(engagement).unwrap_or(engagement);
				Some(VisibleSlotFacts {
					slot:        Str::new(claim.name()),
					holder:      Str::new(holder),
					queue_depth: campaigns.slots().queue_depth(claim),
				})
			})
			.collect::<Vec<_>>();
		let mode_engagement = campaigns.slots().owner(&SlotClaim::Mode);
		let mode_holder = mode_engagement.and_then(|id| campaigns.spec_id(id));
		self.apply_projection(
			visible_slots.into(),
			mode_holder.map(Str::new),
			mode_engagement.map(Str::new),
		);
	}

	/// Refreshes slot facts from campaign entries returned by an actor command.
	pub fn sync_entries(&self, entries: &[CampaignEntry]) {
		let campaigns = entries
			.iter()
			.filter_map(|entry| {
				omp_agent::core_regime(entry.spec_id.as_str()).map(|(spec, _)| (entry, spec))
			})
			.collect::<Vec<_>>();
		let visible_slots = SLOT_TABLE
			.iter()
			.filter(|declaration| declaration.visible)
			.filter_map(|declaration| {
				let holder = campaigns.iter().find(|(entry, spec)| {
					entry.status == CampaignEntryStatus::Engaged
						&& spec
							.claims
							.iter()
							.any(|claim| claim.name() == declaration.name)
				})?;
				let queue_depth = campaigns
					.iter()
					.filter(|(entry, spec)| {
						entry.status == CampaignEntryStatus::Queued
							&& spec
								.claims
								.iter()
								.any(|claim| claim.name() == declaration.name)
					})
					.count();
				Some(VisibleSlotFacts {
					slot: Str::new_static(declaration.name),
					holder: holder.0.spec_id.clone(),
					queue_depth,
				})
			})
			.collect::<Vec<_>>();
		let mode = campaigns.iter().find(|(entry, spec)| {
			entry.status == CampaignEntryStatus::Engaged
				&& spec.claims.iter().any(|claim| claim == &SlotClaim::Mode)
		});
		self.apply_projection(
			visible_slots.into(),
			mode.map(|(entry, _)| entry.spec_id.clone()),
			mode.map(|(entry, _)| entry.engagement.clone()),
		);
	}

	fn apply_projection(
		&self,
		visible_slots: Arc<[VisibleSlotFacts]>,
		mode_holder: Option<Str>,
		mode_engagement: Option<Str>,
	) {
		let mut state = self.state.lock();
		let previous_holder = state.mode_holder.clone();
		state.visible_slots = visible_slots;
		state.mode_holder = mode_holder;
		state.mode_engagement = mode_engagement;
		let plan_entered =
			previous_holder.as_deref() != Some("plan") && state.mode_holder.as_deref() == Some("plan");
		let plan_exited =
			previous_holder.as_deref() == Some("plan") && state.mode_holder.as_deref() != Some("plan");
		drop(state);
		if plan_entered {
			self.activate_plan(false);
		} else if plan_exited {
			self.deactivate_plan();
		}
		self.revision.fetch_add(1, Ordering::Release);
	}

	/// Returns the monotonic projection revision used by retained UI refresh.
	pub fn revision(&self) -> u64 {
		self.revision.load(Ordering::Acquire)
	}

	/// Returns the visible slot projection without rebuilding it per frame.
	pub fn visible_slots(&self) -> Arc<[VisibleSlotFacts]> {
		Arc::clone(&self.state.lock().visible_slots)
	}

	/// Returns the visible mode-holder declaration.
	pub fn mode_holder(&self) -> Option<Str> {
		self.state.lock().mode_holder.clone()
	}

	/// Returns the current mode-holder engagement identity.
	pub fn mode_engagement(&self) -> Option<Str> {
		self.state.lock().mode_engagement.clone()
	}

	/// Returns whether `holder` owns the canonical mode slot.
	pub fn holds_mode(&self, holder: &str) -> bool {
		self
			.state
			.lock()
			.mode_holder
			.as_ref()
			.is_some_and(|active| active == holder)
	}

	/// Binds the active agent selection authority. Plan entry and exit then
	/// apply model/thinking changes without provider mutation during streaming.
	pub fn bind_plan_selection(&self, agent: AgentState, selection: Option<ModelSelection>) {
		*self.plan_binding.lock() = Some(PlanBinding { agent, selection, handoff: None });
	}

	/// Arms a one-shot selection applied when the plan campaign exits,
	/// replacing restoration of the pre-plan selection (`--plan-yolo-into`).
	///
	/// Requires a prior [`Self::bind_plan_selection`]; the handoff is consumed
	/// by the first plan exit and later plan cycles restore normally.
	pub fn bind_plan_handoff(&self, selection: ModelSelection) {
		if let Some(binding) = self.plan_binding.lock().as_mut() {
			binding.handoff = Some(selection);
		}
	}

	/// Marks the current inference stream active for deferred plan transitions.
	pub fn begin_streaming(&self) {
		self.plan_transitions.begin_streaming();
	}

	/// Applies the newest queued plan transition at settlement.
	pub fn settle_plan_transition(&self) -> PlanModelTransition {
		self
			.plan_binding
			.lock()
			.as_ref()
			.map_or(PlanModelTransition::Unchanged, |binding| {
				self.plan_transitions.settle(&binding.agent)
			})
	}

	/// Applies app plan metadata after the plan campaign acquires the mode slot.
	pub fn activate_plan(&self, _plan_yolo: bool) {
		{
			let mut state = self.state.lock();
			let previous = state.plan_seen.then(|| state.plan.clone());
			state.plan = PlanState::entered(previous.as_ref());
			state.plan_seen = true;
		}
		if let Some(binding) = self.plan_binding.lock().as_ref() {
			self
				.plan_transitions
				.enter(&binding.agent, binding.selection.clone());
		}
	}

	/// Returns the durable plan projection.
	pub fn plan(&self) -> Option<PlanState> {
		let state = self.state.lock();
		state.plan_seen.then(|| state.plan.clone())
	}

	/// Selects the approved-plan workflow.
	pub fn set_plan_workflow(&self, workflow: PlanWorkflow) -> Result<PlanState, RegimeError> {
		if !self.holds_mode("plan") {
			return Err(RegimeError::CampaignInactive { required: "plan" });
		}
		let plan = {
			let mut state = self.state.lock();
			if !state.plan.enabled {
				return Err(RegimeError::CampaignInactive { required: "plan" });
			}
			state.plan.workflow = workflow;
			state.plan.clone()
		};
		Ok(plan)
	}

	/// Replaces the canonical active plan artifact reference.
	pub fn set_plan_artifact(&self, artifact: impl Into<Str>) -> Result<PlanState, RegimeError> {
		let artifact = artifact.into();
		let artifact =
			canonical_url(artifact.as_str()).map_err(|_| RegimeError::InvalidPlanArtifact)?;
		if !self.holds_mode("plan") {
			return Err(RegimeError::CampaignInactive { required: "plan" });
		}
		let plan = {
			let mut state = self.state.lock();
			if !state.plan.enabled {
				return Err(RegimeError::CampaignInactive { required: "plan" });
			}
			state.plan.artifact = artifact;
			state.plan.clone()
		};
		Ok(plan)
	}

	/// Wraps an existing prompt source with the active mode `SlotSource`.
	pub fn prompt_source(&self, base: Arc<dyn PromptSource>) -> Arc<dyn PromptSource> {
		Arc::new(ModeAwarePromptSource { base, modes: self.clone() })
	}

	/// Applies app plan metadata after the plan campaign releases the mode slot.
	pub fn deactivate_plan(&self) {
		let mut state = self.state.lock();
		state.plan = state.plan.exited();
		drop(state);
		if let Some(binding) = self.plan_binding.lock().as_mut() {
			match binding.handoff.take() {
				Some(target) => {
					self.plan_transitions.exit_into(&binding.agent, target);
				},
				None => {
					self.plan_transitions.exit(&binding.agent);
				},
			}
		}
	}

	/// Creates or replaces the active goal.
	pub fn set_goal(
		&self,
		objective: impl Into<Str>,
		token_budget: Option<u64>,
		now_ms: u64,
	) -> Result<Goal, RegimeError> {
		let objective = objective.into();
		if objective.as_str().trim().is_empty() {
			return Err(RegimeError::EmptyObjective);
		}
		if token_budget == Some(0) {
			return Err(RegimeError::InvalidBudget);
		}
		let mut state = self.state.lock();
		if state
			.goal
			.as_ref()
			.is_some_and(|goal| !matches!(goal.status, GoalStatus::Complete | GoalStatus::Dropped))
		{
			return Err(RegimeError::GoalExists);
		}
		let goal = Goal {
			id: Str::from(omp_core::Ulid::generate().to_string()),
			objective,
			status: GoalStatus::Active,
			token_budget,
			tokens_used: 0,
			time_used_seconds: 0,
			started_ms: now_ms,
		};
		state.goal = Some(goal.clone());
		state.goal_usage_checkpoint = GoalUsage::default();
		state.budget_steering_pending = false;
		Ok(goal)
	}

	/// Returns the latest goal projection.
	pub fn goal(&self) -> Option<Goal> {
		self.state.lock().goal.clone()
	}

	/// Replaces the live todo context injected through the goal status slot.
	pub fn set_goal_todo_context(&self, todo: Option<Str>) {
		self.state.lock().goal_todo_context = todo;
	}

	/// Returns the current goal todo context.
	pub fn goal_todo_context(&self) -> Option<Str> {
		self.state.lock().goal_todo_context.clone()
	}

	/// Pauses active or budget-limited goal continuation and accounting time.
	pub fn pause_goal(&self, now_ms: u64) -> Result<Goal, RegimeError> {
		let status = self.goal().ok_or(RegimeError::NoGoal)?.status;
		if !matches!(status, GoalStatus::Active | GoalStatus::BudgetLimited) {
			return Err(RegimeError::InvalidGoalTransition { operation: "pause", status });
		}
		let goal = self.update_goal(now_ms, |goal| goal.status = GoalStatus::Paused)?;
		self.state.lock().budget_steering_pending = false;
		Ok(goal)
	}

	/// Resumes a paused, dropped, or budget-limited goal.
	pub fn resume_goal(&self, now_ms: u64) -> Result<Goal, RegimeError> {
		let status = self.goal().ok_or(RegimeError::NoGoal)?.status;
		if !matches!(status, GoalStatus::Paused | GoalStatus::Dropped | GoalStatus::BudgetLimited) {
			return Err(RegimeError::InvalidGoalTransition { operation: "resume", status });
		}
		let goal = self.update_goal(now_ms, |goal| goal.status = GoalStatus::Active)?;
		let mut state = self.state.lock();
		state.budget_steering_pending = false;
		state.goal_usage_checkpoint = GoalUsage::default();
		Ok(goal)
	}

	/// Marks the goal complete and leaves goal mode.
	pub fn complete_goal(&self, now_ms: u64) -> Result<Goal, RegimeError> {
		let status = self.goal().ok_or(RegimeError::NoGoal)?.status;
		if matches!(status, GoalStatus::Complete | GoalStatus::Dropped) {
			return Err(RegimeError::InvalidGoalTransition { operation: "complete", status });
		}
		self.finish_goal(now_ms, GoalStatus::Complete)
	}

	/// Drops the goal and leaves goal mode.
	pub fn drop_goal(&self, now_ms: u64) -> Result<Goal, RegimeError> {
		let status = self.goal().ok_or(RegimeError::NoGoal)?.status;
		if status == GoalStatus::Dropped {
			return Err(RegimeError::InvalidGoalTransition { operation: "drop", status });
		}
		self.finish_goal(now_ms, GoalStatus::Dropped)
	}

	/// Returns the exact model-visible completion accounting report.
	pub fn goal_completion_report(&self) -> Result<Str, RegimeError> {
		let goal = self.goal().ok_or(RegimeError::NoGoal)?;
		if goal.status != GoalStatus::Complete {
			return Err(RegimeError::InvalidGoalTransition {
				operation: "report completion for",
				status:    goal.status,
			});
		}
		Ok(GoalBudgetReport::from_goal(&goal).model_prompt())
	}

	/// Replaces the hard token budget.
	pub fn set_goal_budget(&self, budget: u64) -> Result<Goal, RegimeError> {
		if budget == 0 {
			return Err(RegimeError::InvalidBudget);
		}
		let mut state = self.state.lock();
		let (goal, limited) = {
			let goal = state.goal.as_mut().ok_or(RegimeError::NoGoal)?;
			goal.token_budget = Some(budget);
			let limited = goal.tokens_used >= budget;
			if limited {
				goal.status = GoalStatus::BudgetLimited;
			}
			(goal.clone(), limited)
		};
		if limited {
			state.budget_steering_pending = true;
		} else if goal.status == GoalStatus::BudgetLimited {
			let goal = state
				.goal
				.as_mut()
				.expect("goal exists while updating its budget");
			goal.status = GoalStatus::Active;
			state.budget_steering_pending = false;
		}
		let goal = state
			.goal
			.clone()
			.expect("goal exists while updating its budget");
		Ok(goal)
	}

	/// Charges one usage delta and applies the hard budget transition.
	pub fn record_goal_usage(&self, usage: GoalUsage, now_ms: u64) -> Result<Goal, RegimeError> {
		let mut state = self.state.lock();
		let (goal, limited) = {
			let goal = state.goal.as_mut().ok_or(RegimeError::NoGoal)?;
			if goal.status == GoalStatus::Active {
				goal.tokens_used = goal.tokens_used.saturating_add(usage.charged_tokens());
				goal.time_used_seconds = goal
					.time_used_seconds
					.saturating_add(now_ms.saturating_sub(goal.started_ms) / 1_000);
				goal.started_ms = now_ms;
				if goal
					.token_budget
					.is_some_and(|budget| goal.tokens_used >= budget)
				{
					goal.status = GoalStatus::BudgetLimited;
				}
			}
			(goal.clone(), goal.status == GoalStatus::BudgetLimited)
		};
		if limited {
			state.budget_steering_pending = true;
		}
		Ok(goal)
	}

	/// Checkpoints cumulative provider usage at a non-goal tool boundary.
	///
	/// The stored checkpoint prevents double charging when the same cumulative
	/// receipt is observed at both a tool boundary and turn settlement.
	pub fn checkpoint_goal_usage(
		&self,
		cumulative: GoalUsage,
		now_ms: u64,
	) -> Result<Goal, RegimeError> {
		let delta = {
			let mut state = self.state.lock();
			let previous = state.goal_usage_checkpoint;
			state.goal_usage_checkpoint = cumulative;
			GoalUsage {
				input_tokens:        cumulative
					.input_tokens
					.saturating_sub(previous.input_tokens),
				cache_write_tokens:  cumulative
					.cache_write_tokens
					.saturating_sub(previous.cache_write_tokens),
				cached_input_tokens: cumulative
					.cached_input_tokens
					.saturating_sub(previous.cached_input_tokens),
				output_tokens:       cumulative
					.output_tokens
					.saturating_sub(previous.output_tokens),
			}
		};
		self.record_goal_usage(delta, now_ms)
	}

	/// Pauses an active goal after a user interrupt while preserving its spend.
	pub fn interrupt_goal(
		&self,
		now_ms: u64,
		user_interrupt: bool,
	) -> Result<Option<Goal>, RegimeError> {
		if !user_interrupt || self.goal().is_none() {
			return Ok(self.goal());
		}
		if self
			.goal()
			.is_some_and(|goal| goal.status == GoalStatus::Active)
		{
			let goal = self.pause_goal(now_ms)?;
			return Ok(Some(goal));
		}
		Ok(self.goal())
	}

	/// Produces the settled-boundary goal decision using Core loop evidence.
	pub fn goal_continuation(&self, signal: &LoopSignal, now_ms: u64) -> Continuation {
		let goal_holds_mode = self.holds_mode("goal");
		let mut state = self.state.lock();
		let Some(goal) = state.goal.clone() else {
			return Continuation::Settle;
		};
		if state.budget_steering_pending {
			state.budget_steering_pending = false;
			return Continuation::Continue {
				owner:          sf!("goal"),
				item:           system_item(
					format!(
						"<system-injection reason=\"goal-budget-limit\">\nThe hard goal budget has been \
						 reached. Stop autonomous work and report the best achieved result \
						 now.\n{}\n</system-injection>",
						goal::prompt_context(&goal, None),
					),
					now_ms,
				),
				label:          Some(goal.id),
				collapse_prior: false,
			};
		}
		if !goal_holds_mode || goal.status != GoalStatus::Active || signal.stalled {
			return Continuation::Settle;
		}
		Continuation::Continue {
			owner:          sf!("goal"),
			item:           system_item(
				format!(
					"<system-injection>\nContinue working autonomously toward this \
					 objective:\n<objective>{}</objective>\n</system-injection>",
					escape_xml(goal.objective.as_str())
				),
				now_ms,
			),
			label:          Some(goal.id.clone()),
			collapse_prior: true,
		}
	}

	/// Returns the owner policy applied to goal continuations.
	pub const fn continuation_policy(&self) -> ContinuationPolicy {
		self.policy
	}

	fn update_goal(&self, now_ms: u64, update: impl FnOnce(&mut Goal)) -> Result<Goal, RegimeError> {
		let mut state = self.state.lock();
		let goal = state.goal.as_mut().ok_or(RegimeError::NoGoal)?;
		if goal.status == GoalStatus::Active {
			goal.time_used_seconds = goal
				.time_used_seconds
				.saturating_add(now_ms.saturating_sub(goal.started_ms) / 1_000);
		}
		goal.started_ms = now_ms;
		update(goal);
		Ok(goal.clone())
	}

	fn finish_goal(&self, now_ms: u64, status: GoalStatus) -> Result<Goal, RegimeError> {
		let goal = self.update_goal(now_ms, |goal| goal.status = status)?;
		self.state.lock().budget_steering_pending = false;
		Ok(goal)
	}
}

impl ContinuationSource for CampaignHandle {
	fn decide(&self, signal: &LoopSignal, now_ms: u64) -> (Continuation, ContinuationPolicy) {
		(self.goal_continuation(signal, now_ms), self.continuation_policy())
	}
}

fn system_item(text: String, now_ms: u64) -> Item {
	Item {
		seq:           0,
		created_at_ms: now_ms,
		kind:          Some(item::Kind::Message(Message {
			role:  i32::from(Role::System),
			parts: vec![Part { kind: Some(part::Kind::Text(text)) }],
		})),
		props:         None,
	}
}

fn escape_xml(value: &str) -> String {
	value
		.replace('&', "&amp;")
		.replace('<', "&lt;")
		.replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn mode_slot_denials_preserve_holder_and_since_at_the_app_projection_seam() {
		let mut stack = CampaignStack::new();
		let (plan, plan_machine) = omp_agent::core_regime("plan").expect("plan regime");
		let granted = stack
			.engage(plan, plan_machine, omp_agent::EngageOptions { now_ms: 41, queue: false })
			.expect("plan grant");
		for contender in ["vibe", "goal"] {
			let (spec, machine) = omp_agent::core_regime(contender).expect("contender regime");
			let error = stack
				.engage(spec, machine, omp_agent::EngageOptions { now_ms: 42, queue: false })
				.expect_err("mode claim must deny");
			assert_eq!(error, omp_agent::EngageError::Claim {
				slot:    SlotClaim::Mode,
				outcome: omp_agent::ClaimOutcome::Denied {
					holder: granted.engagement.clone(),
					since:  41,
				},
			});
		}
		let projection = CampaignHandle::new();
		projection.sync_campaigns(&stack);
		assert_eq!(projection.mode_holder().as_deref(), Some("plan"));
		assert_eq!(projection.mode_engagement(), Some(granted.engagement));
	}

	#[test]
	fn queued_mode_ticket_projects_depth_and_auto_grants_on_release() {
		let mut stack = CampaignStack::new();
		let (plan, plan_machine) = omp_agent::core_regime("plan").expect("plan regime");
		let granted = stack
			.engage(plan, plan_machine, omp_agent::EngageOptions { now_ms: 41, queue: false })
			.expect("plan grant");
		let (vibe, vibe_machine) = omp_agent::core_regime("vibe").expect("vibe regime");
		let ticket = stack
			.engage(vibe, vibe_machine, omp_agent::EngageOptions { now_ms: 42, queue: true })
			.expect("vibe queue ticket");
		assert!(matches!(ticket.outcome, omp_agent::ClaimOutcome::Queued { .. }));
		let projection = CampaignHandle::new();
		projection.sync_campaigns(&stack);
		let mode = projection
			.visible_slots()
			.iter()
			.find(|slot| slot.slot == "mode")
			.cloned()
			.expect("mode projection");
		assert_eq!(mode.holder, "plan");
		assert_eq!(mode.queue_depth, 1);

		stack
			.disengage(granted.engagement.as_str(), 43)
			.expect("plan exit");
		projection.sync_campaigns(&stack);
		assert_eq!(projection.mode_holder().as_deref(), Some("vibe"));
		assert_eq!(projection.mode_engagement(), Some(ticket.engagement));
	}

	#[test]
	fn goal_accounting_excludes_cached_input_and_hard_stops() {
		let modes = CampaignHandle::new();
		modes.set_goal("ship", Some(10), 1_000).expect("set goal");
		let goal = modes
			.record_goal_usage(
				GoalUsage {
					input_tokens:        3,
					cache_write_tokens:  2,
					cached_input_tokens: 100,
					output_tokens:       5,
				},
				2_000,
			)
			.expect("record usage");
		assert_eq!(goal.tokens_used, 10);
		assert_eq!(goal.status, GoalStatus::BudgetLimited);
	}

	#[test]
	fn stalled_loop_signal_prevents_goal_continuation() {
		let mut stack = CampaignStack::new();
		let (spec, machine) = omp_agent::core_regime("goal").expect("goal regime");
		stack
			.engage(spec, machine, omp_agent::EngageOptions { now_ms: 0, queue: false })
			.expect("goal grant");
		let modes = CampaignHandle::new();
		modes.sync_campaigns(&stack);
		modes.set_goal("ship <safely>", None, 0).expect("set goal");
		assert!(matches!(
			modes.goal_continuation(&LoopSignal::default(), 1),
			Continuation::Continue { .. }
		));
		let signal = LoopSignal { stalled: true, ..LoopSignal::default() };
		assert_eq!(modes.goal_continuation(&signal, 2), Continuation::Settle);
	}
}
