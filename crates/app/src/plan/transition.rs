//! Settled-boundary plan model and thinking transitions.

use omp_agent::AgentState;
use omp_core::Str;
use omp_proto::inference::v1::Reasoning;
use parking_lot::Mutex;

/// One complete model/thinking selection.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelSelection {
	/// Catalog model selector.
	pub model:    Str,
	/// Explicit reasoning configuration, or no provider override.
	pub thinking: Option<Reasoning>,
}

impl ModelSelection {
	/// Captures the effective coding selection from an agent snapshot.
	#[must_use]
	pub fn capture(state: &AgentState) -> Self {
		let snapshot = state.snapshot();
		Self {
			model:    Str::new(snapshot.turn.params.model.as_str()),
			thinking: snapshot.turn.params.thinking.clone(),
		}
	}

	fn apply(&self, state: &AgentState) {
		state.update(|snapshot| {
			snapshot.turn.params.model = self.model.as_str().to_owned();
			snapshot.turn.params.thinking.clone_from(&self.thinking);
		});
	}
}

/// Observable plan selection transition result.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum PlanModelTransition {
	/// Effective model and thinking were already correct.
	Unchanged,
	/// Selection was applied immediately at a settled boundary.
	Applied,
	/// Selection is queued until streaming settles.
	Deferred,
}

#[derive(Debug, Default)]
struct QueueState {
	streaming: bool,
	coding:    Option<ModelSelection>,
	pending:   Option<ModelSelection>,
}

/// Serializes plan entry/exit selection changes around streaming boundaries.
#[derive(Debug, Default)]
pub struct TransitionQueue {
	state: Mutex<QueueState>,
}

impl TransitionQueue {
	/// Marks the session as streaming. Future transitions are queued.
	pub fn begin_streaming(&self) {
		self.state.lock().streaming = true;
	}

	/// Applies the newest queued selection after the active stream settles.
	pub fn settle(&self, agent: &AgentState) -> PlanModelTransition {
		let pending = {
			let mut state = self.state.lock();
			state.streaming = false;
			state.pending.take()
		};
		pending
			.map_or(PlanModelTransition::Unchanged, |selection| apply_if_changed(agent, &selection))
	}

	/// Enters the configured planning selection and remembers the effective
	/// coding selection exactly once for later restoration.
	pub fn enter(
		&self,
		agent: &AgentState,
		planning: Option<ModelSelection>,
	) -> PlanModelTransition {
		let current = ModelSelection::capture(agent);
		let mut target = planning.unwrap_or_else(|| current.clone());
		if target.model == current.model && target.thinking.is_none() {
			target.thinking.clone_from(&current.thinking);
		}
		let mut state = self.state.lock();
		state.coding.get_or_insert(current);
		queue_or_apply(&mut state, agent, target)
	}

	/// Restores the model and thinking selection captured on plan entry.
	pub fn exit(&self, agent: &AgentState) -> PlanModelTransition {
		let mut state = self.state.lock();
		let Some(coding) = state.coding.take() else {
			state.pending = None;
			return PlanModelTransition::Unchanged;
		};
		queue_or_apply(&mut state, agent, coding)
	}
}

fn queue_or_apply(
	state: &mut QueueState,
	agent: &AgentState,
	target: ModelSelection,
) -> PlanModelTransition {
	if state.streaming {
		state.pending = Some(target);
		PlanModelTransition::Deferred
	} else {
		state.pending = None;
		apply_if_changed(agent, &target)
	}
}

fn apply_if_changed(agent: &AgentState, target: &ModelSelection) -> PlanModelTransition {
	if ModelSelection::capture(agent) == *target {
		PlanModelTransition::Unchanged
	} else {
		target.apply(agent);
		PlanModelTransition::Applied
	}
}
