//! App-owned automatic-learning capture regime composition.

use std::sync::Arc;

use omp_agent::{
	AgentEvent, AgentPhase, AutolearnSettings, Next, Regime, RegimeContext, RegimeError,
	RegimeLifetime, RegimeSpec, RegimeStateError,
};
use omp_core::{Point, Str};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// Stable declaration identity for the automatic-learning capture regime.
pub const AUTOLEARN_REGIME_ID: &str = "autolearn-capture";

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct AutolearnRegimeState {
	settled_tool_calls: usize,
	aborted:            bool,
	capture_in_flight:  bool,
}

/// App-owned observer feeding settled tool and abort facts into the regime.
#[derive(Clone)]
pub struct AutolearnRegimeHandle {
	state: Arc<Mutex<AutolearnRegimeState>>,
}

impl AutolearnRegimeHandle {
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

/// Session-scoped handler injecting one marked capture turn at the IDLE event.
#[derive(Clone)]
pub struct AutolearnRegime {
	state:         Arc<Mutex<AutolearnRegimeState>>,
	auto_continue: bool,
	threshold:     usize,
}

impl AutolearnRegime {
	/// Builds one declaration, handler, and ordered event observer from live
	/// settings.
	pub fn new(settings: AutolearnSettings) -> (Arc<RegimeSpec>, Self, AutolearnRegimeHandle) {
		let threshold = settings.min_tool_calls.max(1);
		let state = Arc::new(Mutex::new(AutolearnRegimeState::default()));
		let spec = Arc::new(RegimeSpec {
			id: Str::new_static(AUTOLEARN_REGIME_ID),
			events: Point::Idle.set(),
			precedence: 30,
			max_steps: None,
			committed_step_interval_ms: None,
			on_limit: false,
			lifetime: RegimeLifetime::Session,
			family_rev: Str::new_static("dev.omp.app.autolearn-capture@1"),
			when: None,
			owns: Arc::from([]),
			sets: Arc::from([]),
			minimum_duration_ms: None,
		});
		let regime =
			Self { state: Arc::clone(&state), auto_continue: settings.auto_continue, threshold };
		let handle = AutolearnRegimeHandle { state };
		(spec, regime, handle)
	}

	fn take_capture(&self, point: Point) -> Option<omp_proto::thread::v1::Item> {
		if point != Point::Idle {
			return None;
		}
		let mut state = self.state.lock();
		if state.capture_in_flight {
			return None;
		}
		if state.aborted {
			state.settled_tool_calls = 0;
			state.aborted = false;
			return None;
		}
		if !self.auto_continue || state.settled_tool_calls < self.threshold {
			return None;
		}
		state.settled_tool_calls = 0;
		state.capture_in_flight = true;
		Some(omp_agent::capture_interrupt().item)
	}
}

impl Regime for AutolearnRegime {
	fn apply(&mut self, ctx: &mut RegimeContext<'_>, _next: Next<'_>) -> Result<(), RegimeError> {
		if let Some(capture) = self.take_capture(ctx.point()) {
			ctx.append_context(vec![capture]);
			ctx.replace_state(self.state());
		}
		Ok(())
	}

	fn state(&self) -> Str {
		serde_json::to_string(&*self.state.lock()).map_or_else(|_| Str::new_static("{}"), Str::from)
	}

	fn restore(&mut self, payload: &str) -> Result<(), RegimeStateError> {
		let restored = serde_json::from_str(payload).map_err(|_| RegimeStateError::InvalidPayload)?;
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
		let (spec, regime, handle) = AutolearnRegime::new(AutolearnSettings {
			enabled:        true,
			auto_continue:  true,
			min_tool_calls: 2,
		});
		assert_eq!(spec.max_steps, None);
		handle.observe(&tool_finished());
		assert!(regime.take_capture(Point::Idle).is_none());
		handle.observe(&tool_finished());
		let capture = regime.take_capture(Point::Idle).expect("threshold reached");
		assert!(omp_agent::is_capture_item(&capture));
	}
}
