//! App-owned automatic-learning capture campaign composition.

use std::sync::Arc;

use omp_agent::{
	AgentEvent, AgentPhase, AutolearnSettings, CampaignMachine, CampaignScope, CampaignSpec,
	CampaignStateError, ExhaustPolicy, Ladder, LadderStep, PointCx, Reaction, Verdict,
};
use omp_core::{Point, Str};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// Stable declaration identity for the automatic-learning capture campaign.
pub const AUTOLEARN_CAMPAIGN_ID: &str = "autolearn-capture";

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct AutolearnCampaignState {
	settled_tool_calls: usize,
	aborted:            bool,
	capture_in_flight:  bool,
}

/// App-owned observer feeding settled tool and abort facts into the campaign.
#[derive(Clone)]
pub struct AutolearnCampaignHandle {
	state: Arc<Mutex<AutolearnCampaignState>>,
}

impl AutolearnCampaignHandle {
	/// Applies one ordered agent-loop observation to capture eligibility.
	pub fn observe(&self, event: &AgentEvent) {
		let mut state = self.state.lock();
		match event {
			AgentEvent::ToolFinished { .. } if !state.capture_in_flight => {
				state.settled_tool_calls = state.settled_tool_calls.saturating_add(1);
			},
			AgentEvent::Failed { .. } => {
				state.settled_tool_calls = 0;
				state.aborted = true;
			},
			AgentEvent::PhaseChanged { to: AgentPhase::Idle, .. } => {
				state.settled_tool_calls = 0;
				state.aborted = false;
				state.capture_in_flight = false;
			},
			_ => {},
		}
	}
}

/// Session-scoped machine injecting one marked capture turn at the IDLE fold.
#[derive(Clone)]
pub struct AutolearnCampaign {
	state:         Arc<Mutex<AutolearnCampaignState>>,
	auto_continue: bool,
	threshold:     usize,
}

impl AutolearnCampaign {
	/// Builds one declaration, machine, and ordered event observer from live
	/// settings.
	pub fn new(settings: AutolearnSettings) -> (Arc<CampaignSpec>, Self, AutolearnCampaignHandle) {
		let threshold = settings.min_tool_calls.max(1);
		let state = Arc::new(Mutex::new(AutolearnCampaignState::default()));
		let capture = omp_agent::capture_interrupt().item;
		let steps = (0..threshold)
			.map(|index| LadderStep {
				label:   Str::from(format!("settled-tool-{}", index.saturating_add(1))),
				verdict: Verdict::Pass,
			})
			.collect::<Vec<_>>();
		let exhaust = if settings.auto_continue {
			ExhaustPolicy::Verdict(Verdict::Inject(vec![capture]))
		} else {
			ExhaustPolicy::Settle
		};
		let spec = Arc::new(CampaignSpec {
			id: Str::new_static(AUTOLEARN_CAMPAIGN_ID),
			points: Point::Idle.set(),
			precedence: 30,
			ladder: Some(Ladder::new(Arc::<[LadderStep]>::from(steps))),
			exhaust,
			scope: CampaignScope::Session,
			family_rev: Str::new_static("dev.omp.app.autolearn-capture@1"),
			when: None,
			members: Arc::from([]),
			claims: Arc::from([]),
			binds: Arc::from([]),
			dwell_ms: None,
		});
		let machine =
			Self { state: Arc::clone(&state), auto_continue: settings.auto_continue, threshold };
		let handle = AutolearnCampaignHandle { state };
		(spec, machine, handle)
	}
}

impl CampaignMachine for AutolearnCampaign {
	fn react(&mut self, point: Point, _: &PointCx<'_>) -> Reaction {
		if point != Point::Idle {
			return Reaction::one(Verdict::Pass);
		}
		let mut state = self.state.lock();
		if state.capture_in_flight {
			return Reaction::one(Verdict::Pass);
		}
		if state.aborted {
			state.settled_tool_calls = 0;
			state.aborted = false;
			return Reaction::one(Verdict::Pass);
		}
		if !self.auto_continue || state.settled_tool_calls < self.threshold {
			return Reaction::one(Verdict::Pass);
		}
		state.settled_tool_calls = 0;
		state.capture_in_flight = true;
		Reaction::one(Verdict::Inject(vec![omp_agent::capture_interrupt().item]))
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

#[cfg(test)]
mod tests {
	use omp_proto::thread::v1::Item;

	use super::*;

	fn tool_finished() -> AgentEvent {
		AgentEvent::ToolFinished {
			call_id: Str::new_static("call"),
			item:    Item::default(),
			usage:   Default::default(),
		}
	}

	#[test]
	fn injects_at_idle_only_after_the_configured_threshold() {
		let (spec, mut campaign, handle) = AutolearnCampaign::new(AutolearnSettings {
			enabled:        true,
			auto_continue:  true,
			min_tool_calls: 2,
		});
		assert_eq!(spec.ladder.as_ref().map(Ladder::len), Some(2));
		handle.observe(&tool_finished());
		assert_eq!(campaign.react(Point::Idle, &PointCx::default()).verdicts, [Verdict::Pass]);
		handle.observe(&tool_finished());
		let reaction = campaign.react(Point::Idle, &PointCx::default());
		assert!(matches!(reaction.verdicts.as_slice(), [Verdict::Inject(items)] if
			items.len() == 1 && omp_agent::is_capture_item(&items[0])));
	}
}
