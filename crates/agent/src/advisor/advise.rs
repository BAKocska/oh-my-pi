//! Advisor-only `advise` device and escalation-aware delivery queue.

use std::{collections::HashMap, sync::Arc};

use futures::{Stream, stream};
use omp_core::{Str, sf};
use omp_tool::{
	Abort, ArgIssue, ArgIssueKind, CommitError, Constraint, Effects, Ev, IncomingParams, ParamError,
	Part, PromptCaps, Rev, Tool, ToolSpec, ToolTerminal,
};
use parking_lot::Mutex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{AdviceSeverity, normalize_advice};

/// Arguments accepted by the advisor-only `advise@1` device.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdviseParams {
	/// Concrete issue and proposed correction.
	#[schemars(with = "String")]
	pub note:     Str,
	/// Urgency used by the primary-loop router.
	#[serde(default)]
	pub severity: AdviceSeverity,
}

/// Whether a recorded note can be routed immediately.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum AdviceAdmission {
	/// The note is ready for delivery routing.
	Ready,
	/// The note was retained until the current primary update settles.
	Deferred,
	/// The same issue was already recorded at this or a higher severity.
	Suppressed,
}

/// One escalation-qualified advisor note.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueuedAdvice {
	/// Original trimmed note.
	pub note:     Str,
	/// Highest severity observed for the normalized issue.
	pub severity: AdviceSeverity,
}

#[derive(Debug, Default)]
struct QueueState {
	mid_turn:       bool,
	delivered:      HashMap<Str, AdviceSeverity>,
	ready:          Vec<QueuedAdvice>,
	deferred:       Vec<QueuedAdvice>,
	deferred_index: HashMap<Str, usize>,
}

/// Session-scoped queue enforcing escalation-only duplicate delivery.
#[derive(Clone, Debug, Default)]
pub struct AdvisorAdviceQueue {
	state: Arc<Mutex<QueueState>>,
}

impl AdvisorAdviceQueue {
	/// Marks whether the primary is inside a partial model update.
	///
	/// Clearing the boundary promotes deferred notes in their original order.
	pub fn set_mid_turn(&self, mid_turn: bool) {
		let mut state = self.state.lock();
		let was_mid_turn = state.mid_turn;
		state.mid_turn = mid_turn;
		if was_mid_turn && !mid_turn {
			let deferred = std::mem::take(&mut state.deferred);
			state.deferred_index.clear();
			for queued in deferred {
				let key = normalize_advice(queued.note.as_str());
				if state
					.delivered
					.get(&key)
					.is_some_and(|highest| *highest >= queued.severity)
				{
					continue;
				}
				state.delivered.insert(key, queued.severity);
				state.ready.push(queued);
			}
		}
	}

	/// Records a note if it is new or strictly escalates the prior severity.
	pub fn submit(&self, note: &str, severity: AdviceSeverity) -> AdviceAdmission {
		let note = note.trim();
		if note.is_empty() {
			return AdviceAdmission::Suppressed;
		}
		let key = normalize_advice(note);
		let mut state = self.state.lock();
		if state
			.delivered
			.get(&key)
			.is_some_and(|highest| *highest >= severity)
		{
			return AdviceAdmission::Suppressed;
		}
		if severity == AdviceSeverity::Blocker || !state.mid_turn {
			state.delivered.insert(key, severity);
			state.ready.push(QueuedAdvice { note: Str::new(note), severity });
			return AdviceAdmission::Ready;
		}
		if let Some(index) = state.deferred_index.get(&key).copied() {
			let pending = &mut state.deferred[index];
			if severity <= pending.severity {
				return AdviceAdmission::Suppressed;
			}
			pending.severity = severity;
			return AdviceAdmission::Deferred;
		}
		let index = state.deferred.len();
		state.deferred.push(QueuedAdvice { note: Str::new(note), severity });
		state.deferred_index.insert(key, index);
		AdviceAdmission::Deferred
	}

	/// Drains notes ready for primary-loop delivery.
	pub fn drain_ready(&self) -> Vec<QueuedAdvice> {
		std::mem::take(&mut self.state.lock().ready)
	}

	/// Clears turn and dedupe state when advisor context is re-primed.
	pub fn reset(&self) {
		*self.state.lock() = QueueState::default();
	}
}

/// Successful advise result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AdvisePayload {
	/// Admission decision; suppressed calls intentionally remain successful.
	pub admission: Str,
}

/// The advise device does not stream intermediate updates.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AdviseUpdate {}

/// Typed advise-device refusal.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, thiserror::Error)]
pub enum AdviseFault {
	/// The note was empty after trimming.
	#[error("advice note must not be empty")]
	EmptyNote,
}

/// Advisor-only advise tool bound to one session queue.
pub struct AdviseTool {
	queue: AdvisorAdviceQueue,
	spec:  ToolSpec,
}

/// Creates `advise@1` over a session-scoped queue.
pub fn tool(queue: AdvisorAdviceQueue) -> AdviseTool {
	AdviseTool {
		queue,
		spec: ToolSpec {
			name:            sf!("advise"),
			rev:             Rev { family: Str::default(), n: 1 },
			description:     sf!(
				"Records one concrete review note for the primary agent. Use nit for optional \
				 cleanup, concern for material risk, and blocker only for broken work."
			),
			schema:          omp_tool::schema::<AdviseParams>(),
			constraint:      Constraint::Schema {
				priority:       100,
				on_unsupported: omp_tool::Fallback::Unspecified,
			},
			effects:         Effects::empty(),
			projection_code: omp_tool::native_projection_code(
				env!("CARGO_PKG_NAME"),
				env!("CARGO_PKG_VERSION"),
				include_bytes!("advise.rs"),
			)
			.into(),
		},
	}
}

impl Tool for AdviseTool {
	type Fault = AdviseFault;
	type Params = AdviseParams;
	type Payload = AdvisePayload;
	type Update = AdviseUpdate;

	fn spec(&self) -> &ToolSpec {
		&self.spec
	}

	fn call<'c>(
		&'c self,
		mut incoming: IncomingParams<'c>,
	) -> impl Stream<Item = Ev<AdviseUpdate, AdvisePayload, AdviseFault>> + Send + 'c {
		stream::once(async move {
			let params = match incoming.whole::<AdviseParams>().await {
				Ok(value) => value,
				Err(error) => return param_event(error),
			};
			if params.note.trim().is_empty() {
				return Ev::Done(ToolTerminal::Done {
					result:  Err(AdviseFault::EmptyNote),
					useless: true,
				});
			}
			if let Err(error) = incoming.interruptable().committed().await {
				return commit_event(error);
			}
			let admission = self.queue.submit(params.note.as_str(), params.severity);
			Ev::Done(ToolTerminal::Done {
				result:  Ok(AdvisePayload { admission: sf!(<&'static str>::from(admission)) }),
				useless: admission == AdviceAdmission::Suppressed,
			})
		})
	}

	fn prompt(&self, view: Result<&AdvisePayload, &AdviseFault>, _: &PromptCaps) -> Vec<Part> {
		let text = match view {
			Ok(payload) if payload.admission == "deferred" => sf!(
				"Deferred — primary is mid-turn; this note will be delivered automatically when \
				 the turn completes. Do not re-raise the same point."
			),
			Ok(payload) if payload.admission == "suppressed" => sf!("Duplicate advice ignored."),
			Ok(_) => sf!("Recorded."),
			Err(AdviseFault::EmptyNote) => sf!("Advice note must not be empty."),
		};
		vec![Part::Text { text }]
	}
}

fn param_event(error: ParamError) -> Ev<AdviseUpdate, AdvisePayload, AdviseFault> {
	match error {
		ParamError::Args(issue) => Ev::Args(*issue),
		ParamError::Interrupted(interrupt) => {
			Ev::Aborted(Abort::Interrupted { reason: interrupt.reason })
		},
		ParamError::Protocol(message) => Ev::Args(protocol_issue(message)),
	}
}

fn commit_event(error: CommitError) -> Ev<AdviseUpdate, AdvisePayload, AdviseFault> {
	match error {
		CommitError::Aborted => Ev::Aborted(Abort::InputDropped),
		CommitError::Interrupted(interrupt) => {
			Ev::Aborted(Abort::Interrupted { reason: interrupt.reason })
		},
		CommitError::Protocol(message) => Ev::Args(protocol_issue(message)),
	}
}

fn protocol_issue(message: Str) -> ArgIssue {
	ArgIssue {
		path:     Vec::new(),
		expected: sf!("one committed advise argument object"),
		kind:     ArgIssueKind::Protocol,
		example:  Some(sf!(r#"{{"note":"Concrete issue","severity":"concern"}}"#)),
		found:    Some(message),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use omp_tool::{Dialect, ModelClass};

	#[test]
	fn deferred_notes_dedupe_escalate_in_place_and_flush_oldest_first() {
		let queue = AdvisorAdviceQueue::default();
		queue.set_mid_turn(true);
		assert_eq!(
			queue.submit("First issue.", AdviceSeverity::Nit),
			AdviceAdmission::Deferred
		);
		assert_eq!(
			queue.submit("Second issue.", AdviceSeverity::Concern),
			AdviceAdmission::Deferred
		);
		assert_eq!(
			queue.submit("First   issue.", AdviceSeverity::Concern),
			AdviceAdmission::Deferred
		);
		assert_eq!(
			queue.submit("First issue.", AdviceSeverity::Nit),
			AdviceAdmission::Suppressed
		);
		assert!(queue.drain_ready().is_empty());

		queue.set_mid_turn(false);
		assert_eq!(
			queue.drain_ready(),
			[
				QueuedAdvice {
					note:     Str::new_static("First issue."),
					severity: AdviceSeverity::Concern,
				},
				QueuedAdvice {
					note:     Str::new_static("Second issue."),
					severity: AdviceSeverity::Concern,
				},
			]
		);
	}

	#[test]
	fn blocker_bypasses_mid_turn_and_suppresses_lower_deferred_copy() {
		let queue = AdvisorAdviceQueue::default();
		queue.set_mid_turn(true);
		assert_eq!(
			queue.submit("Unsafe operation.", AdviceSeverity::Concern),
			AdviceAdmission::Deferred
		);
		assert_eq!(
			queue.submit("Unsafe operation.", AdviceSeverity::Blocker),
			AdviceAdmission::Ready
		);
		assert_eq!(
			queue.drain_ready(),
			[QueuedAdvice {
				note:     Str::new_static("Unsafe operation."),
				severity: AdviceSeverity::Blocker,
			}]
		);
		queue.set_mid_turn(false);
		assert!(queue.drain_ready().is_empty());
	}

	#[test]
	fn delivered_note_accepts_only_a_later_escalation() {
		let queue = AdvisorAdviceQueue::default();
		assert_eq!(
			queue.submit("Regression risk.", AdviceSeverity::Nit),
			AdviceAdmission::Ready
		);
		let _ = queue.drain_ready();
		assert_eq!(
			queue.submit("Regression risk.", AdviceSeverity::Nit),
			AdviceAdmission::Suppressed
		);
		assert_eq!(
			queue.submit("Regression risk.", AdviceSeverity::Concern),
			AdviceAdmission::Ready
		);
	}

	#[test]
	fn deferred_tool_prompt_states_that_delivery_is_automatic() {
		let queue = AdvisorAdviceQueue::default();
		let tool = tool(queue);
		let payload = AdvisePayload { admission: sf!("deferred") };
		let parts = tool.prompt(
			Ok(&payload),
			&PromptCaps {
				maximum_parts:      1,
				maximum_text_bytes: 512,
				media:              false,
				dialect:            Dialect::default(),
				model_class:        ModelClass::default(),
			},
		);
		assert!(matches!(
			parts.as_slice(),
			[Part::Text { text }] if text.contains("Deferred")
				&& text.contains("delivered automatically")
		));
	}
}
