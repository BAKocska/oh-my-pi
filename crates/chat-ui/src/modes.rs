//! Retained execution-mode cards and guided goal interview.

use omp_core::{Str, sf};
use omp_tui::{
	Cached, Component, Key, Layer, Mouse, PaintCtx, Prop, Props, Rect, Size, Slot, UiContext,
	components::Boxed, dom,
};

use crate::{PromptEvent, PromptOverlay};

/// Goal lifecycle rendered by the status card.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum GoalCardStatus {
	/// Eligible for autonomous continuation.
	Active,
	/// Deliberately paused.
	Paused,
	/// Hard budget reached.
	BudgetLimited,
	/// Objective achieved.
	Complete,
	/// Objective abandoned.
	Dropped,
}

/// Host-supplied facts for one styled goal card.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalCardFacts {
	/// User-authored objective.
	pub objective:    Str,
	/// Current durable lifecycle state.
	pub status:       GoalCardStatus,
	/// Fresh tokens charged to the goal.
	pub tokens_used:  u64,
	/// Optional hard budget.
	pub token_budget: Option<u64>,
	/// Accumulated wall-clock seconds.
	pub elapsed_secs: u64,
}

/// Styled retained TML goal status card.
pub struct GoalStatusCard {
	inner: Boxed,
}

impl GoalStatusCard {
	/// Builds a card from the latest durable projection.
	#[must_use]
	pub fn new(facts: &GoalCardFacts) -> Self {
		let status = sf!("{}", facts.status);
		let usage = facts.token_budget.map_or_else(
			|| sf!("{} tokens", facts.tokens_used),
			|budget| sf!("{} / {} tokens", facts.tokens_used, budget),
		);
		let elapsed = sf!("{}s elapsed", facts.elapsed_secs);
		let objective = facts.objective.clone();
		let status_color = match facts.status {
			GoalCardStatus::Active => "success",
			GoalCardStatus::Paused => "warning",
			GoalCardStatus::BudgetLimited => "error",
			GoalCardStatus::Complete => "accent",
			GoalCardStatus::Dropped => "muted",
		};
		let inner = Boxed::new()
			.with(Prop::Border, omp_tui::Border::Round)
			.with(Prop::Title, sf!("Goal"))
			.with(Prop::PadX, 1_u16)
			.child(dom! {
				<col>
					<row gap=1>
						<text bold fg={status_color}>{status}</text>
						<text dim>{usage}</text>
						<spacer grow/>
						<text dim>{elapsed}</text>
					</row>
					<md>{objective}</md>
				</col>
			});
		Self { inner }
	}
}

impl Component for GoalStatusCard {
	fn props(&self) -> &Props {
		self.inner.props()
	}

	fn props_mut(&mut self) -> &mut Props {
		self.inner.props_mut()
	}

	fn slot(&self) -> Slot {
		self.inner.slot()
	}

	fn children(&self) -> &[Cached] {
		self.inner.children()
	}

	fn children_mut(&mut self) -> &mut [Cached] {
		self.inner.children_mut()
	}

	fn measure(&mut self, ctx: &UiContext) -> (u16, u16) {
		self.inner.measure(ctx)
	}

	fn height(&mut self, ctx: &UiContext, width: u16) -> u16 {
		self.inner.height(ctx, width)
	}

	fn place(&mut self, ctx: &UiContext, content: Rect) {
		self.inner.place(ctx, content);
	}

	fn paint(&mut self, pc: &mut PaintCtx<'_>, rect: Rect) {
		self.inner.paint(pc, rect);
	}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InterviewStep {
	Objective,
	Budget,
}

/// Completed guided interview values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuidedGoalValues {
	/// Non-empty objective.
	pub objective:    Str,
	/// Optional positive hard budget.
	pub token_budget: Option<u64>,
}

/// Guided interview event.
pub enum GuidedGoalEvent {
	/// Input was consumed and the interview remains active.
	Consumed,
	/// Interview was cancelled without changing goal state.
	Cancel,
	/// Objective and budget passed validation.
	Complete(GuidedGoalValues),
}

/// Two-stage retained guided goal interview.
pub struct GuidedGoalInterview {
	step:      InterviewStep,
	objective: Option<Str>,
	prompt:    PromptOverlay,
	ctx:       UiContext,
}

impl GuidedGoalInterview {
	/// Opens at the objective question.
	#[must_use]
	pub fn open(ctx: &UiContext) -> Self {
		Self {
			step:      InterviewStep::Objective,
			objective: None,
			prompt:    PromptOverlay::open("What outcome should OMP achieve?", false, ctx),
			ctx:       ctx.clone(),
		}
	}

	/// Routes one key through the current interview stage.
	pub fn handle_key(&mut self, key: Key) -> GuidedGoalEvent {
		let event = self.prompt.handle_key(key);
		self.route(event)
	}

	/// Routes pasted text through the current interview stage.
	pub fn handle_paste(&mut self, text: &str) -> GuidedGoalEvent {
		let event = self.prompt.handle_paste(text);
		self.route(event)
	}

	/// Routes pointer input through the active prompt.
	pub fn handle_mouse(
		&mut self,
		col: u16,
		row: u16,
		kind: Mouse,
		viewport: Size,
	) -> GuidedGoalEvent {
		let event = self.prompt.handle_mouse(col, row, kind, viewport);
		self.route(event)
	}

	/// Returns the active centered prompt layer.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		self.prompt.layer(viewport)
	}

	fn route(&mut self, event: PromptEvent) -> GuidedGoalEvent {
		match event {
			PromptEvent::Consumed => GuidedGoalEvent::Consumed,
			PromptEvent::Cancel => GuidedGoalEvent::Cancel,
			PromptEvent::Submit(value) => match self.step {
				InterviewStep::Objective => {
					let objective = value.trim();
					if objective.is_empty() {
						return GuidedGoalEvent::Consumed;
					}
					self.objective = Some(Str::new(objective));
					self.step = InterviewStep::Budget;
					self.prompt =
						PromptOverlay::open("Hard token budget (blank for unbounded)", false, &self.ctx);
					GuidedGoalEvent::Consumed
				},
				InterviewStep::Budget => {
					let value = value.trim();
					let budget = if value.is_empty() {
						None
					} else {
						match value.parse::<u64>() {
							Ok(value) if value > 0 => Some(value),
							_ => return GuidedGoalEvent::Consumed,
						}
					};
					GuidedGoalEvent::Complete(GuidedGoalValues {
						objective:    self.objective.clone().expect("objective stage completed"),
						token_budget: budget,
					})
				},
			},
		}
	}
}
