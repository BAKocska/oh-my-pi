//! Phased session task tracking with deterministic state transitions.

use std::{fmt, sync::Arc};

use async_stream::stream;
use futures::Stream;
use omp_core::{Str, sf};
use omp_tool::{
	ArgIssue, ArgIssueKind, Constraint, Effects, Ev, IncomingParams, ParamError, Part, PromptCaps,
	Rev, Tool, ToolSpec, ToolTerminal,
};
use parking_lot::Mutex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Model arguments for `todo@1`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Params {
	/// State transition to perform.
	pub op:     Op,
	/// Complete phased list, required by `init`.
	#[schemars(with = "Option<Vec<Phase>>", default, skip_serializing_if = "Option::is_none")]
	pub list:   Option<Vec<Phase>>,
	/// Phase name for item operations and `append`.
	#[schemars(with = "Option<String>", default, skip_serializing_if = "Option::is_none")]
	pub phase:  Option<Str>,
	/// Item text for single-item operations.
	#[schemars(with = "Option<String>", default, skip_serializing_if = "Option::is_none")]
	pub item:   Option<Str>,
	/// Items appended to `phase`.
	#[schemars(with = "Option<Vec<String>>", default, skip_serializing_if = "Option::is_none")]
	pub items:  Option<Vec<Str>>,
	/// Required explanation when blocking an item.
	#[schemars(with = "Option<String>", default, skip_serializing_if = "Option::is_none")]
	pub reason: Option<Str>,
}

/// Supported todo operations.
#[derive(
	Clone,
	Copy,
	Debug,
	Deserialize,
	Eq,
	JsonSchema,
	PartialEq,
	Serialize,
	strum::Display,
	strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
pub enum Op {
	/// Replaces the complete phased list.
	Init,
	/// Marks one item as in progress.
	Start,
	/// Marks one item completed.
	Done,
	/// Removes one item.
	Rm,
	/// Marks one item abandoned.
	Drop,
	/// Marks one item blocked with a reason.
	Block,
	/// Returns a blocked item to pending.
	Unblock,
	/// Adds pending items to an existing phase.
	Append,
	/// Returns the current state without changing it.
	View,
}

/// One named phase and its ordered items.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct Phase {
	/// Stable phase label.
	#[schemars(with = "String")]
	pub phase: Str,
	/// Items in their user-defined order.
	pub items: Vec<Item>,
}

/// One task item.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct Item {
	/// User-visible task text.
	#[schemars(with = "String")]
	pub text:   Str,
	/// Current lifecycle state.
	#[serde(default)]
	pub status: Status,
	/// Block explanation, only present while blocked.
	#[schemars(with = "Option<String>", default, skip_serializing_if = "Option::is_none")]
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub reason: Option<Str>,
}

/// Durable task state.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Deserialize,
	Eq,
	JsonSchema,
	PartialEq,
	Serialize,
	strum::Display,
	strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
pub enum Status {
	/// Not yet started.
	#[default]
	Pending,
	/// Actively being worked.
	InProgress,
	/// Finished successfully.
	Completed,
	/// Intentionally abandoned.
	Abandoned,
	/// Waiting on an external dependency.
	Blocked,
}

/// Successful todo state after an operation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Payload {
	/// Durable phase tree after the requested operation.
	pub phases:   Vec<Phase>,
	/// Markdown projection of the phase tree.
	pub rendered: Str,
}
/// Todo does not stream progress updates.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Update {}
/// A rejected todo transition.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fault {
	/// An operation's arguments or state transition is invalid.
	Invalid {
		/// Stable validation explanation.
		message: Str,
	},
	/// A named phase or item does not exist.
	Missing {
		/// Stable lookup explanation.
		message: Str,
	},
}
impl fmt::Display for Fault {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Invalid { message } | Self::Missing { message } => f.write_str(message),
		}
	}
}
impl std::error::Error for Fault {}

/// In-memory todo executor. Session hosts may snapshot `Payload::phases` into
/// their journal.
pub struct Todo {
	phases: Arc<Mutex<Vec<Phase>>>,
	spec:   ToolSpec,
}
/// Creates the core todo slot tool.
pub fn tool() -> Todo {
	Todo {
		phases: Arc::new(Mutex::new(Vec::new())),
		spec:   ToolSpec {
			name:            sf!("todo"),
			rev:             Rev { family: Str::new(""), n: 1 },
			description:     sf!(
				"Tracks a phased task list. Use `init` once, then `start`, `done`, `drop`, `block`, \
				 `append`, or `view` as work changes.",
			),
			schema:          omp_tool::schema::<Params>(),
			constraint:      Constraint::Schema {
				priority:       100,
				on_unsupported: omp_tool::Fallback::Unspecified,
			},
			effects:         Effects::default(),
			projection_code: omp_tool::native_projection_code(
				env!("CARGO_PKG_NAME"),
				env!("CARGO_PKG_VERSION"),
				include_bytes!("todo.rs"),
			),
		},
	}
}

impl Tool for Todo {
	type Fault = Fault;
	type Params = Params;
	type Payload = Payload;
	type Update = Update;

	fn spec(&self) -> &ToolSpec {
		&self.spec
	}

	fn call<'c>(
		&'c self,
		mut params: IncomingParams<'c>,
	) -> impl Stream<Item = Ev<Update, Payload, Fault>> + Send + 'c {
		stream! {
			let arguments = match params.whole::<Params>().await { Ok(value) => value, Err(error) => { yield param_event(error); return; } };
			if let Err(error) = params.interruptable().committed().await { yield commit_event(error); return; }
			let result = apply(&mut self.phases.lock(), arguments).map(|phases| Payload { rendered: Str::new(render(&phases)), phases });
			yield done(result);
		}
	}

	fn prompt(&self, view: Result<&Payload, &Fault>, _: &PromptCaps) -> Vec<Part> {
		vec![Part::Text {
			text: Str::new(match view {
				Ok(payload) => payload.rendered.to_string(),
				Err(fault) => fault.to_string(),
			}),
		}]
	}
}

/// Applies a transition to a phased list.
pub fn apply(phases: &mut Vec<Phase>, params: Params) -> Result<Vec<Phase>, Fault> {
	match params.op {
		Op::Init => {
			*phases = params
				.list
				.ok_or_else(|| invalid("`list` is required for init"))?
		},
		Op::View => {},
		Op::Append => {
			let phase = required(params.phase, "phase")?;
			let items = params
				.items
				.ok_or_else(|| invalid("`items` is required for append"))?;
			let target = phases
				.iter_mut()
				.find(|entry| entry.phase == phase)
				.ok_or_else(|| missing("phase", &phase))?;
			target.items.extend(items.into_iter().map(|text| Item {
				text,
				status: Status::Pending,
				reason: None,
			}));
		},
		op => {
			let phase = required(params.phase, "phase")?;
			let item = required(params.item, "item")?;
			let target = phases
				.iter_mut()
				.find(|entry| entry.phase == phase)
				.ok_or_else(|| missing("phase", &phase))?;
			if op == Op::Rm {
				let index = target
					.items
					.iter()
					.position(|entry| entry.text == item)
					.ok_or_else(|| missing("item", &item))?;
				target.items.remove(index);
				return Ok(phases.clone());
			}
			let entry = target
				.items
				.iter_mut()
				.find(|entry| entry.text == item)
				.ok_or_else(|| missing("item", &item))?;
			match op {
				Op::Start => {
					entry.status = Status::InProgress;
					entry.reason = None;
				},
				Op::Done => {
					entry.status = Status::Completed;
					entry.reason = None;
				},
				Op::Drop => {
					entry.status = Status::Abandoned;
					entry.reason = None;
				},
				Op::Block => {
					entry.status = Status::Blocked;
					entry.reason = Some(
						params
							.reason
							.ok_or_else(|| invalid("`reason` is required for block"))?,
					);
				},
				Op::Unblock => {
					if entry.status != Status::Blocked {
						return Err(invalid("only blocked items can be unblocked"));
					}
					entry.status = Status::Pending;
					entry.reason = None;
				},
				Op::Init | Op::Rm | Op::Append | Op::View => unreachable!(),
			}
		},
	}
	Ok(phases.clone())
}
fn required(value: Option<Str>, name: &str) -> Result<Str, Fault> {
	value.ok_or_else(|| invalid(&format!("`{name}` is required")))
}
fn invalid(message: &str) -> Fault {
	Fault::Invalid { message: Str::new(message) }
}
fn missing(kind: &str, value: &str) -> Fault {
	Fault::Missing { message: sf!("{kind} not found: {value}") }
}
/// Formats the durable state as a Markdown checklist.
pub fn render(phases: &[Phase]) -> String {
	phases
		.iter()
		.enumerate()
		.map(|(index, phase)| {
			let items = phase
				.items
				.iter()
				.map(|item| {
					let mark = match item.status {
						Status::Completed => "x",
						_ => " ",
					};
					let suffix = match item.status {
						Status::Pending | Status::Completed => String::new(),
						status => format!(
							" ({status}{})",
							item
								.reason
								.as_ref()
								.map(|reason| format!(": {reason}"))
								.unwrap_or_default()
						),
					};
					format!("- [{mark}] {}{suffix}", item.text)
				})
				.collect::<Vec<_>>()
				.join("\n");
			format!("{}. {}\n{}", index + 1, phase.phase, items)
		})
		.collect::<Vec<_>>()
		.join("\n\n")
}
fn done(result: Result<Payload, Fault>) -> Ev<Update, Payload, Fault> {
	Ev::Done(ToolTerminal::Done { result, useless: false })
}
fn param_event(error: ParamError) -> Ev<Update, Payload, Fault> {
	match error {
		ParamError::Args(issue) => Ev::Args(*issue),
		ParamError::Interrupted(interrupt) => {
			Ev::Aborted(omp_tool::Abort::Interrupted { reason: interrupt.reason })
		},
		ParamError::Protocol(message) => Ev::Args(protocol_issue(message)),
	}
}
fn commit_event(error: omp_tool::CommitError) -> Ev<Update, Payload, Fault> {
	match error {
		omp_tool::CommitError::Aborted => Ev::Aborted(omp_tool::Abort::InputDropped),
		omp_tool::CommitError::Interrupted(interrupt) => {
			Ev::Aborted(omp_tool::Abort::Interrupted { reason: interrupt.reason })
		},
		omp_tool::CommitError::Protocol(message) => Ev::Args(protocol_issue(message)),
	}
}
fn protocol_issue(message: Str) -> ArgIssue {
	ArgIssue {
		path:     Vec::new(),
		expected: sf!("one committed JSON argument object"),
		kind:     ArgIssueKind::Protocol,
		example:  Some(sf!(r#"{{"op":"view"}}"#)),
		found:    Some(message),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	fn init() -> Vec<Phase> {
		vec![Phase {
			phase: sf!("Build"),
			items: vec![Item { text: sf!("port"), status: Status::Pending, reason: None }],
		}]
	}
	#[test]
	fn transitions_and_append_preserve_phase_order() {
		let mut phases = Vec::new();
		apply(&mut phases, Params {
			op:     Op::Init,
			list:   Some(init()),
			phase:  None,
			item:   None,
			items:  None,
			reason: None,
		})
		.unwrap();
		apply(&mut phases, Params {
			op:     Op::Start,
			list:   None,
			phase:  Some(sf!("Build")),
			item:   Some(sf!("port")),
			items:  None,
			reason: None,
		})
		.unwrap();
		apply(&mut phases, Params {
			op:     Op::Append,
			list:   None,
			phase:  Some(sf!("Build")),
			item:   None,
			items:  Some(vec![sf!("test")]),
			reason: None,
		})
		.unwrap();
		assert_eq!(phases[0].items[0].status, Status::InProgress);
		assert_eq!(phases[0].items[1].text, "test");
	}
	#[test]
	fn block_requires_reason_and_unblock_returns_pending() {
		let mut phases = init();
		assert!(
			apply(&mut phases, Params {
				op:     Op::Block,
				list:   None,
				phase:  Some(sf!("Build")),
				item:   Some(sf!("port")),
				items:  None,
				reason: None,
			})
			.is_err()
		);
		apply(&mut phases, Params {
			op:     Op::Block,
			list:   None,
			phase:  Some(sf!("Build")),
			item:   Some(sf!("port")),
			items:  None,
			reason: Some(sf!("blocked")),
		})
		.unwrap();
		apply(&mut phases, Params {
			op:     Op::Unblock,
			list:   None,
			phase:  Some(sf!("Build")),
			item:   Some(sf!("port")),
			items:  None,
			reason: None,
		})
		.unwrap();
		assert_eq!(phases[0].items[0].status, Status::Pending);
	}
}
