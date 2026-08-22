//! Application execution modes and autonomous goal-loop policy.

pub mod persistence;

use std::sync::Arc;

use omp_agent::{
	AgentState, CachedContribution, Continuation, ContinuationPolicy, ContinuationSource,
	ExecutionMode, ExecutionModeHandle, LoopSignal, ModePromptSource, PromptError, PromptMode,
	PromptSource, SlotAssembler, SlotClass, SlotDecl, SlotId, SlotRegistration, WorkspaceInput,
};
use omp_core::{Str, sf};
use omp_proto::thread::v1::{Item, Message, Part, Role, item, part};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use self::persistence::{ModePersistence, ModePersistenceError};
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
	pub fn validate_prompt_words(self, words: &[Str]) -> Result<(), StartupModeError> {
		if self == Self::RpcUi && words.iter().any(|word| word.starts_with("@")) {
			return Err(StartupModeError::RpcUiReference);
		}
		Ok(())
	}
}

/// Startup-mode usage failure.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum StartupModeError {
	/// RPC UI accepts attachments only through typed protocol frames.
	#[error("rpc-ui does not accept @file arguments")]
	RpcUiReference,
}

/// Mutually exclusive user-facing execution mode.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ActiveMode {
	/// Normal interactive execution.
	#[default]
	Standard,
	/// Read-only planning.
	Plan,
	/// Cheap reason-first prewalk.
	Prewalk,
	/// Goal auto-continuation.
	Goal,
	/// Director/worker orchestration.
	Vibe,
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

/// Invalid or mutually exclusive mode transition.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ModeError {
	/// Another mode must be exited first.
	#[error("cannot enter {requested:?} mode while {active:?} mode is active")]
	Conflict {
		/// Requested mode.
		requested: ActiveMode,
		/// Existing mode.
		active:    ActiveMode,
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

/// Serializable autonomous-mode projection stored in the session journal.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModeProjection {
	/// Durable plan-mode state.
	pub plan: Option<PlanState>,
	/// Durable goal projection.
	pub goal: Option<Goal>,
}
#[derive(Debug, Default)]
struct ModeState {
	mode:                    ActiveMode,
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
}

/// Shared application mode authority paired with the agent invocation handle.
#[derive(Clone, Debug)]
pub struct ExecutionModes {
	state:            Arc<Mutex<ModeState>>,
	handle:           ExecutionModeHandle,
	policy:           ContinuationPolicy,
	plan_binding:     Arc<Mutex<Option<PlanBinding>>>,
	plan_transitions: Arc<TransitionQueue>,
	persistence:      Arc<Mutex<Option<ModePersistence>>>,
}
struct ModeAwarePromptSource {
	base:  Arc<dyn PromptSource>,
	modes: ExecutionModes,
}

impl PromptSource for ModeAwarePromptSource {
	fn render(&self, workspace: &WorkspaceInput) -> Result<Vec<Item>, PromptError> {
		let mut items = self.base.render(workspace)?;
		let mut registrations = Vec::new();
		if let Some(mode) = self.modes.prompt_mode() {
			registrations.push(ModePromptSource::new(mode).registration());
		}
		if let Some(goal) = (self.modes.active() == ActiveMode::Goal)
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

impl ExecutionModes {
	/// Creates a mode authority whose invocation metadata is enforced env-side.
	pub fn new(handle: ExecutionModeHandle) -> Self {
		Self {
			state: Arc::new(Mutex::new(ModeState::default())),
			handle,
			policy: ContinuationPolicy::default(),
			plan_binding: Arc::new(Mutex::new(None)),
			plan_transitions: Arc::new(TransitionQueue::default()),
			persistence: Arc::new(Mutex::new(None)),
		}
	}

	/// Restores the latest journal-folded autonomous state.
	pub fn from_projection(handle: ExecutionModeHandle, projection: ModeProjection) -> Self {
		let modes = Self::new(handle);
		{
			let mut state = modes.state.lock();
			if let Some(mut plan) = projection.plan {
				if plan.enabled {
					plan.reentry = true;
				}
				state.plan_seen = true;
				state.plan = plan;
			}
			state.goal = projection.goal.map(|mut goal| {
				goal.started_ms = epoch_millis();
				goal
			});
			state.mode = if state.plan.enabled {
				ActiveMode::Plan
			} else if state
				.goal
				.as_ref()
				.is_some_and(|goal| goal.status == GoalStatus::Active)
			{
				ActiveMode::Goal
			} else {
				ActiveMode::Standard
			};
			modes.handle.set(match state.mode {
				ActiveMode::Plan => ExecutionMode::Plan,
				ActiveMode::Goal => ExecutionMode::Goal,
				_ => ExecutionMode::Standard,
			});
		}
		modes
	}

	/// Returns the projection to append after every lifecycle mutation.
	pub fn projection(&self) -> ModeProjection {
		let state = self.state.lock();
		ModeProjection { plan: state.plan_seen.then(|| state.plan.clone()), goal: state.goal.clone() }
	}

	/// Attaches the sole journal-backed persistence actor.
	pub fn attach_persistence(&self, persistence: ModePersistence) {
		*self.persistence.lock() = Some(persistence);
		let projection = self.projection();
		if projection.plan.is_some() || projection.goal.is_some() {
			let _ = self.persist_projection();
		}
	}

	/// Queues the latest projection from a synchronous UI transition.
	pub fn persist_projection(&self) -> Result<(), ModePersistenceError> {
		let persistence = self.persistence.lock().clone();
		if let Some(persistence) = persistence {
			persistence.store(self.projection())?;
		}
		Ok(())
	}

	/// Waits until the latest projection is durably acknowledged.
	pub async fn flush_projection(&self) -> Result<(), ModePersistenceError> {
		let persistence = self.persistence.lock().clone();
		if let Some(persistence) = persistence {
			persistence.flush(self.projection()).await?;
		}
		Ok(())
	}

	/// Binds the active agent selection authority. Plan entry and exit then
	/// apply model/thinking changes without provider mutation during streaming.
	pub fn bind_plan_selection(&self, agent: AgentState, selection: Option<ModelSelection>) {
		*self.plan_binding.lock() = Some(PlanBinding { agent, selection });
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

	/// Returns the active mode, reconciling one-way prewalk/yolo transitions.
	pub fn active(&self) -> ActiveMode {
		let effective = match self.handle.get() {
			ExecutionMode::Standard => ActiveMode::Standard,
			ExecutionMode::Plan | ExecutionMode::PlanYolo => ActiveMode::Plan,
			ExecutionMode::Goal => ActiveMode::Goal,
			ExecutionMode::Vibe => ActiveMode::Vibe,
			ExecutionMode::Prewalk => ActiveMode::Prewalk,
		};
		let mut state = self.state.lock();
		let exited_plan = state.mode == ActiveMode::Plan && effective == ActiveMode::Standard;
		if matches!(state.mode, ActiveMode::Plan | ActiveMode::Prewalk)
			&& effective == ActiveMode::Standard
		{
			state.mode = ActiveMode::Standard;
			if exited_plan {
				state.plan = state.plan.exited();
			}
		}
		let mode = state.mode;
		drop(state);
		if exited_plan {
			if let Some(binding) = self.plan_binding.lock().as_ref() {
				self.plan_transitions.exit(&binding.agent);
			}
			let _ = self.persist_projection();
		}
		mode
	}

	/// Enters plan mode; plan-yolo allows one env-authorized first mutation.
	pub fn enter_plan(&self, plan_yolo: bool) -> Result<(), ModeError> {
		self.enter(
			ActiveMode::Plan,
			if plan_yolo {
				ExecutionMode::PlanYolo
			} else {
				ExecutionMode::Plan
			},
		)?;
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
		let _ = self.persist_projection();
		Ok(())
	}

	/// Returns the durable plan projection.
	pub fn plan(&self) -> Option<PlanState> {
		let state = self.state.lock();
		state.plan_seen.then(|| state.plan.clone())
	}

	/// Selects the approved-plan workflow.
	pub fn set_plan_workflow(&self, workflow: PlanWorkflow) -> Result<PlanState, ModeError> {
		let plan = {
			let mut state = self.state.lock();
			if state.mode != ActiveMode::Plan || !state.plan.enabled {
				return Err(ModeError::Conflict { requested: ActiveMode::Plan, active: state.mode });
			}
			state.plan.workflow = workflow;
			state.plan.clone()
		};
		let _ = self.persist_projection();
		Ok(plan)
	}

	/// Replaces the canonical active plan artifact reference.
	pub fn set_plan_artifact(&self, artifact: impl Into<Str>) -> Result<PlanState, ModeError> {
		let artifact = artifact.into();
		let artifact =
			canonical_url(artifact.as_str()).map_err(|_| ModeError::InvalidPlanArtifact)?;
		let plan = {
			let mut state = self.state.lock();
			if state.mode != ActiveMode::Plan || !state.plan.enabled {
				return Err(ModeError::Conflict { requested: ActiveMode::Plan, active: state.mode });
			}
			state.plan.artifact = artifact;
			state.plan.clone()
		};
		let _ = self.persist_projection();
		Ok(plan)
	}

	/// Maps the active runtime mode onto the built-in prompt vocabulary.
	pub fn prompt_mode(&self) -> Option<PromptMode> {
		match self.active() {
			ActiveMode::Standard => None,
			ActiveMode::Plan => Some(PromptMode::Plan),
			ActiveMode::Prewalk => Some(PromptMode::Prewalk),
			ActiveMode::Goal => Some(PromptMode::Goal),
			ActiveMode::Vibe => Some(PromptMode::Vibe),
		}
	}

	/// Wraps an existing prompt source with the active mode `SlotSource`.
	pub fn prompt_source(&self, base: Arc<dyn PromptSource>) -> Arc<dyn PromptSource> {
		Arc::new(ModeAwarePromptSource { base, modes: self.clone() })
	}

	/// Exits plan mode without disturbing goal state.
	pub fn exit_plan(&self) {
		let exited = {
			let mut state = self.state.lock();
			if state.mode != ActiveMode::Plan {
				return;
			}
			state.mode = ActiveMode::Standard;
			state.plan = state.plan.exited();
			self.handle.set(ExecutionMode::Standard);
			true
		};
		if exited && let Some(binding) = self.plan_binding.lock().as_ref() {
			self.plan_transitions.exit(&binding.agent);
		}
		let _ = self.persist_projection();
	}

	/// Arms prewalk until the first mutating effect supplies a reason to
	/// execute.
	pub fn arm_prewalk(&self) -> Result<(), ModeError> {
		self.enter(ActiveMode::Prewalk, ExecutionMode::Prewalk)
	}

	/// Disarms prewalk before it reaches a mutating effect.
	pub fn disarm_prewalk(&self) {
		let mut state = self.state.lock();
		if state.mode == ActiveMode::Prewalk {
			state.mode = ActiveMode::Standard;
			self.handle.set(ExecutionMode::Standard);
		}
	}

	/// Creates or replaces the active goal.
	pub fn set_goal(
		&self,
		objective: impl Into<Str>,
		token_budget: Option<u64>,
		now_ms: u64,
	) -> Result<Goal, ModeError> {
		let objective = objective.into();
		if objective.as_str().trim().is_empty() {
			return Err(ModeError::EmptyObjective);
		}
		if token_budget == Some(0) {
			return Err(ModeError::InvalidBudget);
		}
		let mut state = self.state.lock();
		if !matches!(state.mode, ActiveMode::Standard | ActiveMode::Goal) {
			return Err(ModeError::Conflict { requested: ActiveMode::Goal, active: state.mode });
		}
		if state
			.goal
			.as_ref()
			.is_some_and(|goal| !matches!(goal.status, GoalStatus::Complete | GoalStatus::Dropped))
		{
			return Err(ModeError::GoalExists);
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
		state.mode = ActiveMode::Goal;
		state.goal = Some(goal.clone());
		state.goal_usage_checkpoint = GoalUsage::default();
		state.budget_steering_pending = false;
		self.handle.set(ExecutionMode::Goal);
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
	pub fn pause_goal(&self, now_ms: u64) -> Result<Goal, ModeError> {
		let status = self.goal().ok_or(ModeError::NoGoal)?.status;
		if !matches!(status, GoalStatus::Active | GoalStatus::BudgetLimited) {
			return Err(ModeError::InvalidGoalTransition { operation: "pause", status });
		}
		let goal = self.update_goal(now_ms, |goal| goal.status = GoalStatus::Paused)?;
		let mut state = self.state.lock();
		state.mode = ActiveMode::Standard;
		state.budget_steering_pending = false;
		self.handle.set(ExecutionMode::Standard);
		Ok(goal)
	}

	/// Resumes a paused, dropped, or budget-limited goal.
	pub fn resume_goal(&self, now_ms: u64) -> Result<Goal, ModeError> {
		let status = self.goal().ok_or(ModeError::NoGoal)?.status;
		if !matches!(status, GoalStatus::Paused | GoalStatus::Dropped | GoalStatus::BudgetLimited) {
			return Err(ModeError::InvalidGoalTransition { operation: "resume", status });
		}
		let goal = self.update_goal(now_ms, |goal| goal.status = GoalStatus::Active)?;
		let mut state = self.state.lock();
		state.mode = ActiveMode::Goal;
		state.budget_steering_pending = false;
		state.goal_usage_checkpoint = GoalUsage::default();
		self.handle.set(ExecutionMode::Goal);
		Ok(goal)
	}

	/// Marks the goal complete and leaves goal mode.
	pub fn complete_goal(&self, now_ms: u64) -> Result<Goal, ModeError> {
		let status = self.goal().ok_or(ModeError::NoGoal)?.status;
		if matches!(status, GoalStatus::Complete | GoalStatus::Dropped) {
			return Err(ModeError::InvalidGoalTransition { operation: "complete", status });
		}
		self.finish_goal(now_ms, GoalStatus::Complete)
	}

	/// Drops the goal and leaves goal mode.
	pub fn drop_goal(&self, now_ms: u64) -> Result<Goal, ModeError> {
		let status = self.goal().ok_or(ModeError::NoGoal)?.status;
		if status == GoalStatus::Dropped {
			return Err(ModeError::InvalidGoalTransition { operation: "drop", status });
		}
		self.finish_goal(now_ms, GoalStatus::Dropped)
	}

	/// Returns the exact model-visible completion accounting report.
	pub fn goal_completion_report(&self) -> Result<Str, ModeError> {
		let goal = self.goal().ok_or(ModeError::NoGoal)?;
		if goal.status != GoalStatus::Complete {
			return Err(ModeError::InvalidGoalTransition {
				operation: "report completion for",
				status:    goal.status,
			});
		}
		Ok(GoalBudgetReport::from_goal(&goal).model_prompt())
	}

	/// Replaces the hard token budget.
	pub fn set_goal_budget(&self, budget: u64) -> Result<Goal, ModeError> {
		if budget == 0 {
			return Err(ModeError::InvalidBudget);
		}
		let mut state = self.state.lock();
		let (goal, limited) = {
			let goal = state.goal.as_mut().ok_or(ModeError::NoGoal)?;
			goal.token_budget = Some(budget);
			let limited = goal.tokens_used >= budget;
			if limited {
				goal.status = GoalStatus::BudgetLimited;
			}
			(goal.clone(), limited)
		};
		if limited {
			state.mode = ActiveMode::Standard;
			state.budget_steering_pending = true;
			self.handle.set(ExecutionMode::Standard);
		} else if goal.status == GoalStatus::BudgetLimited {
			let goal = state
				.goal
				.as_mut()
				.expect("goal exists while updating its budget");
			goal.status = GoalStatus::Active;
			state.mode = ActiveMode::Goal;
			state.budget_steering_pending = false;
			self.handle.set(ExecutionMode::Goal);
		}
		let goal = state
			.goal
			.clone()
			.expect("goal exists while updating its budget");
		Ok(goal)
	}

	/// Charges one usage delta and applies the hard budget transition.
	pub fn record_goal_usage(&self, usage: GoalUsage, now_ms: u64) -> Result<Goal, ModeError> {
		let mut state = self.state.lock();
		let (goal, limited) = {
			let goal = state.goal.as_mut().ok_or(ModeError::NoGoal)?;
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
			state.mode = ActiveMode::Standard;
			state.budget_steering_pending = true;
			self.handle.set(ExecutionMode::Standard);
		}
		drop(state);
		let _ = self.persist_projection();
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
	) -> Result<Goal, ModeError> {
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
	) -> Result<Option<Goal>, ModeError> {
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
		if state.mode != ActiveMode::Goal || goal.status != GoalStatus::Active || signal.stalled {
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

	/// Enters vibe mode, refusing plan/goal/prewalk overlap.
	pub fn enter_vibe(&self) -> Result<(), ModeError> {
		self.enter(ActiveMode::Vibe, ExecutionMode::Vibe)
	}

	/// Leaves vibe mode.
	pub fn exit_vibe(&self) {
		let mut state = self.state.lock();
		if state.mode == ActiveMode::Vibe {
			state.mode = ActiveMode::Standard;
			self.handle.set(ExecutionMode::Standard);
		}
	}

	fn enter(&self, requested: ActiveMode, execution: ExecutionMode) -> Result<(), ModeError> {
		let mut state = self.state.lock();
		if state.mode != ActiveMode::Standard {
			return Err(ModeError::Conflict { requested, active: state.mode });
		}
		state.mode = requested;
		self.handle.set(execution);
		Ok(())
	}

	fn update_goal(&self, now_ms: u64, update: impl FnOnce(&mut Goal)) -> Result<Goal, ModeError> {
		let mut state = self.state.lock();
		let goal = state.goal.as_mut().ok_or(ModeError::NoGoal)?;
		if goal.status == GoalStatus::Active {
			goal.time_used_seconds = goal
				.time_used_seconds
				.saturating_add(now_ms.saturating_sub(goal.started_ms) / 1_000);
		}
		goal.started_ms = now_ms;
		update(goal);
		Ok(goal.clone())
	}

	fn finish_goal(&self, now_ms: u64, status: GoalStatus) -> Result<Goal, ModeError> {
		let goal = self.update_goal(now_ms, |goal| goal.status = status)?;
		let mut state = self.state.lock();
		state.mode = ActiveMode::Standard;
		state.budget_steering_pending = false;
		self.handle.set(ExecutionMode::Standard);
		Ok(goal)
	}
}

impl ContinuationSource for ExecutionModes {
	fn decide(&self, signal: &LoopSignal, now_ms: u64) -> (Continuation, ContinuationPolicy) {
		(self.goal_continuation(signal, now_ms), self.continuation_policy())
	}
}

fn epoch_millis() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.unwrap_or_default()
		.as_millis()
		.try_into()
		.unwrap_or(u64::MAX)
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
	fn plan_goal_and_vibe_are_mutually_exclusive() {
		let modes = ExecutionModes::new(ExecutionModeHandle::default());
		modes.enter_plan(false).expect("enter plan");
		assert!(matches!(
			modes.set_goal("ship", None, 0),
			Err(ModeError::Conflict { active: ActiveMode::Plan, .. })
		));
		assert!(matches!(modes.enter_vibe(), Err(ModeError::Conflict { .. })));
		modes.exit_plan();
		modes.enter_vibe().expect("enter vibe");
		assert!(matches!(modes.enter_plan(false), Err(ModeError::Conflict { .. })));
	}

	#[test]
	fn goal_accounting_excludes_cached_input_and_hard_stops() {
		let modes = ExecutionModes::new(ExecutionModeHandle::default());
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
		assert_eq!(modes.active(), ActiveMode::Standard);
	}

	#[test]
	fn stalled_loop_signal_prevents_goal_continuation() {
		let modes = ExecutionModes::new(ExecutionModeHandle::default());
		modes.set_goal("ship <safely>", None, 0).expect("set goal");
		assert!(matches!(
			modes.goal_continuation(&LoopSignal::default(), 1),
			Continuation::Continue { .. }
		));
		let signal = LoopSignal { stalled: true, ..LoopSignal::default() };
		assert_eq!(modes.goal_continuation(&signal, 2), Continuation::Settle);
	}
}
