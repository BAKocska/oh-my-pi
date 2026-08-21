//! Application execution modes and autonomous goal-loop policy.

use std::sync::Arc;

use omp_agent::{
	Continuation, ContinuationPolicy, ContinuationSource, ExecutionMode, ExecutionModeHandle,
	LoopSignal, ModePromptSource, PromptError, PromptMode, PromptSource, SlotAssembler,
	WorkspaceInput,
};
use omp_core::{Str, sf};
use omp_proto::thread::v1::{Item, Message, Part, Role, item, part};
use parking_lot::Mutex;
use thiserror::Error;

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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
#[derive(Clone, Debug, Eq, PartialEq)]
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
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
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
	#[must_use]
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
}

#[derive(Debug, Default)]
struct ModeState {
	mode: ActiveMode,
	goal: Option<Goal>,
}

/// Shared application mode authority paired with the agent invocation handle.
#[derive(Clone, Debug)]
pub struct ExecutionModes {
	state:  Arc<Mutex<ModeState>>,
	handle: ExecutionModeHandle,
	policy: ContinuationPolicy,
}
struct ModeAwarePromptSource {
	base:  Arc<dyn PromptSource>,
	modes: ExecutionModes,
}

impl PromptSource for ModeAwarePromptSource {
	fn render(&self, workspace: &WorkspaceInput) -> Result<Vec<Item>, PromptError> {
		let mut items = self.base.render(workspace)?;
		if let Some(mode) = self.modes.prompt_mode() {
			let source = SlotAssembler::new(vec![ModePromptSource::new(mode).registration()]);
			items.extend(source.render(workspace)?);
		}
		Ok(items)
	}
}

impl ExecutionModes {
	/// Creates a mode authority whose invocation metadata is enforced env-side.
	#[must_use]
	pub fn new(handle: ExecutionModeHandle) -> Self {
		Self {
			state: Arc::new(Mutex::new(ModeState::default())),
			handle,
			policy: ContinuationPolicy::default(),
		}
	}

	/// Returns the active mode, reconciling one-way prewalk/yolo transitions.
	#[must_use]
	pub fn active(&self) -> ActiveMode {
		let effective = match self.handle.get() {
			ExecutionMode::Standard => ActiveMode::Standard,
			ExecutionMode::Plan | ExecutionMode::PlanYolo => ActiveMode::Plan,
			ExecutionMode::Goal => ActiveMode::Goal,
			ExecutionMode::Vibe => ActiveMode::Vibe,
			ExecutionMode::Prewalk => ActiveMode::Prewalk,
		};
		let mut state = self.state.lock();
		if matches!(state.mode, ActiveMode::Plan | ActiveMode::Prewalk)
			&& effective == ActiveMode::Standard
		{
			state.mode = ActiveMode::Standard;
		}
		state.mode
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
		)
	}

	/// Maps the active runtime mode onto the built-in prompt vocabulary.
	#[must_use]
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
	#[must_use]
	pub fn prompt_source(&self, base: Arc<dyn PromptSource>) -> Arc<dyn PromptSource> {
		Arc::new(ModeAwarePromptSource { base, modes: self.clone() })
	}

	/// Exits plan mode without disturbing goal state.
	pub fn exit_plan(&self) {
		let mut state = self.state.lock();
		if state.mode == ActiveMode::Plan {
			state.mode = ActiveMode::Standard;
			self.handle.set(ExecutionMode::Standard);
		}
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
		let goal = Goal {
			id: Str::from(ulid::Ulid::generate().to_string()),
			objective,
			status: GoalStatus::Active,
			token_budget,
			tokens_used: 0,
			time_used_seconds: 0,
			started_ms: now_ms,
		};
		state.mode = ActiveMode::Goal;
		state.goal = Some(goal.clone());
		self.handle.set(ExecutionMode::Goal);
		Ok(goal)
	}

	/// Returns the latest goal projection.
	#[must_use]
	pub fn goal(&self) -> Option<Goal> {
		self.state.lock().goal.clone()
	}

	/// Pauses active goal continuation and accounting time.
	pub fn pause_goal(&self, now_ms: u64) -> Result<Goal, ModeError> {
		self.update_goal(now_ms, |goal| goal.status = GoalStatus::Paused)
	}

	/// Resumes a paused goal.
	pub fn resume_goal(&self, now_ms: u64) -> Result<Goal, ModeError> {
		let goal = self.update_goal(now_ms, |goal| goal.status = GoalStatus::Active)?;
		self.state.lock().mode = ActiveMode::Goal;
		self.handle.set(ExecutionMode::Goal);
		Ok(goal)
	}

	/// Marks the goal complete and leaves goal mode.
	pub fn complete_goal(&self, now_ms: u64) -> Result<Goal, ModeError> {
		self.finish_goal(now_ms, GoalStatus::Complete)
	}

	/// Drops the goal and leaves goal mode.
	pub fn drop_goal(&self, now_ms: u64) -> Result<Goal, ModeError> {
		self.finish_goal(now_ms, GoalStatus::Dropped)
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
			self.handle.set(ExecutionMode::Standard);
		}
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
			self.handle.set(ExecutionMode::Standard);
		}
		Ok(goal)
	}

	/// Produces the settled-boundary goal decision using Core loop evidence.
	#[must_use]
	pub fn goal_continuation(&self, signal: &LoopSignal, now_ms: u64) -> Continuation {
		let state = self.state.lock();
		let Some(goal) = state.goal.as_ref() else {
			return Continuation::Settle;
		};
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
	#[must_use]
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
		self.handle.set(ExecutionMode::Standard);
		Ok(goal)
	}
}

impl ContinuationSource for ExecutionModes {
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
