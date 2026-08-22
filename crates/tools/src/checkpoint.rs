//! Durable exploration checkpoint creation and turn-boundary rewind scheduling.

use std::{fmt, future::Future};

use async_stream::stream;
use futures::Stream;
use omp_core::{Str, sf};
use omp_tool::{
	Abort, ArgIssue, ArgIssueKind, CommitError, Constraint, Effects, Ev, IncomingParams, ParamError,
	Part, PromptCaps, Rev, Tool, ToolSpec, ToolTerminal,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Environment bridge to the active Agent Journal and its boundary command
/// queue. Rewind must enqueue, never mutate the journal inline.
pub trait CheckpointControl: Clone + Send + Sync + 'static {
	/// Activates one durable checkpoint and returns an opaque session token.
	fn checkpoint(
		&self,
		goal: Str,
	) -> impl Future<Output = Result<CheckpointAck, CheckpointFault>> + Send;

	/// Validates and schedules a rewind after the active tool batch settles.
	fn schedule_rewind(
		&self,
		token: Str,
		report: Str,
	) -> impl Future<Output = Result<RewindAck, CheckpointFault>> + Send;
}

/// Authoritative checkpoint activation acknowledgement.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CheckpointAck {
	/// Opaque session-owned checkpoint token.
	pub token:      Str,
	/// Checkpoint creation time in epoch milliseconds.
	pub started_at: u64,
}

/// Stable checkpoint-domain failure returned by the active agent.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CheckpointFault {
	/// Machine-readable failure class.
	pub code:    FaultCode,
	/// Stable user-facing guidance.
	pub message: Str,
}

/// Authoritative enqueue acknowledgement.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RewindAck {
	/// Opaque checkpoint token accepted by the active session.
	pub token:   Str,
	/// Agent-issued durable command or receipt identifier.
	pub receipt: Str,
}

/// Checkpoint creation arguments.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointParams {
	/// Goal of the speculative exploration branch.
	#[schemars(with = "String")]
	pub goal: Str,
}

/// Rewind scheduling arguments.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RewindParams {
	/// Opaque token returned by `checkpoint`.
	#[schemars(with = "String")]
	pub token:  Str,
	/// Findings retained after the exploration branch is discarded.
	#[schemars(with = "String")]
	pub report: Str,
}

/// Durable checkpoint token.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CheckpointPayload {
	/// Opaque token accepted only by this session.
	pub token:      Str,
	/// Goal recorded on the durable entry.
	pub goal:       Str,
	/// Checkpoint creation time in epoch milliseconds.
	pub started_at: u64,
}

/// Scheduled rewind receipt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RewindPayload {
	/// Validated opaque checkpoint token.
	pub token:     Str,
	/// Findings retained with the rewind command.
	pub report:    Str,
	/// Agent-issued command receipt identifier.
	pub receipt:   Str,
	/// Stable settlement verdict.
	pub scheduled: bool,
}

/// Checkpoint tools do not stream updates.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Update {}

/// Stable checkpoint failure class.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultCode {
	/// A checkpoint is already active.
	AlreadyActive,
	/// No checkpoint has been created.
	NoActive,
	/// The most recent checkpoint already completed.
	AlreadyCompleted,
	/// The supplied token belongs to another session or checkpoint.
	WrongToken,
	/// The report is empty after trimming.
	EmptyReport,
	/// A rewind is already queued.
	AlreadyScheduled,
	/// The active agent control bridge failed.
	Control,
}

/// Journal bridge or checkpoint validation failure.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Fault {
	code:    FaultCode,
	message: Str,
}
impl fmt::Display for Fault {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.message)
	}
}
impl std::error::Error for Fault {}

/// Creates durable checkpoint entries.
pub struct Checkpoint<C> {
	control: C,
	spec:    ToolSpec,
}
/// Schedules a boundary rewind to a durable checkpoint token.
pub struct Rewind<C> {
	control: C,
	spec:    ToolSpec,
}

/// Creates the paired tools over one active-agent bridge.
pub fn tools<C: CheckpointControl>(control: C) -> (Checkpoint<C>, Rewind<C>) {
	let checkpoint = Checkpoint {
		control: control.clone(),
		spec:    spec(
			"checkpoint",
			"Creates a durable exploration checkpoint with a stated goal and returns its opaque \
			 session token.",
			omp_tool::schema::<CheckpointParams>(),
		),
	};
	let rewind = Rewind {
		control,
		spec: spec(
			"rewind",
			"Schedules rewind to a checkpoint at the next turn boundary, retaining the exploration \
			 findings report.",
			omp_tool::schema::<RewindParams>(),
		),
	};
	(checkpoint, rewind)
}

fn spec(name: &'static str, description: &'static str, schema: bytes::Bytes) -> ToolSpec {
	ToolSpec {
		name: sf!(name),
		rev: Rev { family: Default::default(), n: 1 },
		description: sf!(description),
		schema,
		constraint: Constraint::Schema {
			priority:       255,
			on_unsupported: omp_tool::Fallback::Unspecified,
		},
		effects: Effects::empty(),
		projection_code: omp_tool::native_projection_code(
			env!("CARGO_PKG_NAME"),
			env!("CARGO_PKG_VERSION"),
			include_bytes!("checkpoint.rs"),
		)
		.into(),
	}
}

impl<C: CheckpointControl> Tool for Checkpoint<C> {
	type Fault = Fault;
	type Params = CheckpointParams;
	type Payload = CheckpointPayload;
	type Update = Update;

	fn spec(&self) -> &ToolSpec {
		&self.spec
	}

	fn call<'c>(
		&'c self,
		mut incoming: IncomingParams<'c>,
	) -> impl Stream<Item = Ev<Update, CheckpointPayload, Fault>> + Send + 'c {
		stream! {
			let params = match incoming.whole::<CheckpointParams>().await { Ok(value) => value, Err(error) => { yield param_event(error); return; } };
			if params.goal.trim().is_empty() {
				yield done_checkpoint(Err(fault(FaultCode::Control, "goal must not be empty")));
				return;
			}
			if let Err(error) = incoming.interruptable().committed().await { yield commit_checkpoint(error); return; }
			let goal = params.goal;
			let result = self.control.checkpoint(goal.clone()).await
				.map(|ack| CheckpointPayload { token: ack.token, goal, started_at: ack.started_at })
				.map_err(|fault| Fault { code: fault.code, message: fault.message });
			yield done_checkpoint(result);
		}
	}

	fn prompt(&self, view: Result<&CheckpointPayload, &Fault>, _: &PromptCaps) -> Vec<Part> {
		vec![Part::Text {
			text: match view {
				Ok(payload) => {
					sf!("Checkpoint {} created for: {}", payload.token, payload.goal)
				},
				Err(fault) => fault.message.clone(),
			},
		}]
	}
}

impl<C: CheckpointControl> Tool for Rewind<C> {
	type Fault = Fault;
	type Params = RewindParams;
	type Payload = RewindPayload;
	type Update = Update;

	fn spec(&self) -> &ToolSpec {
		&self.spec
	}

	fn call<'c>(
		&'c self,
		mut incoming: IncomingParams<'c>,
	) -> impl Stream<Item = Ev<Update, RewindPayload, Fault>> + Send + 'c {
		stream! {
			let params = match incoming.whole::<RewindParams>().await { Ok(value) => value, Err(error) => { yield param_event(error); return; } };
			if params.report.trim().is_empty() {
				yield done_rewind(Err(fault(FaultCode::EmptyReport, "report must not be empty")));
				return;
			}
			if let Err(error) = incoming.interruptable().committed().await { yield commit_rewind(error); return; }
			let report = params.report;
			let result = self.control.schedule_rewind(params.token, report.clone()).await
				.map(|ack| RewindPayload { token: ack.token, report, receipt: ack.receipt, scheduled: true })
				.map_err(|fault| Fault { code: fault.code, message: fault.message });
			yield done_rewind(result);
		}
	}

	fn prompt(&self, view: Result<&RewindPayload, &Fault>, _: &PromptCaps) -> Vec<Part> {
		vec![Part::Text {
			text: match view {
				Ok(payload) => sf!(
					"Rewind to checkpoint {} scheduled at turn boundary (receipt {}).",
					payload.token,
					payload.receipt
				),
				Err(fault) => fault.message.clone(),
			},
		}]
	}
}

const fn fault(code: FaultCode, message: &'static str) -> Fault {
	Fault { code, message: sf!(message) }
}
const fn done_checkpoint(
	result: Result<CheckpointPayload, Fault>,
) -> Ev<Update, CheckpointPayload, Fault> {
	Ev::Done(ToolTerminal::Done { result, useless: false })
}
const fn done_rewind(result: Result<RewindPayload, Fault>) -> Ev<Update, RewindPayload, Fault> {
	Ev::Done(ToolTerminal::Done { result, useless: false })
}
fn param_event<P>(error: ParamError) -> Ev<Update, P, Fault> {
	match error {
		ParamError::Args(issue) => Ev::Args(*issue),
		ParamError::Interrupted(interrupt) => {
			Ev::Aborted(Abort::Interrupted { reason: interrupt.reason })
		},
		ParamError::Protocol(message) => Ev::Args(protocol_issue(message)),
	}
}
fn commit_checkpoint(error: CommitError) -> Ev<Update, CheckpointPayload, Fault> {
	commit_event(error)
}
fn commit_rewind(error: CommitError) -> Ev<Update, RewindPayload, Fault> {
	commit_event(error)
}
fn commit_event<P>(error: CommitError) -> Ev<Update, P, Fault> {
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
		expected: sf!("one committed JSON argument object"),
		kind:     ArgIssueKind::Protocol,
		example:  None,
		found:    Some(message),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[derive(Clone)]
	struct Control;
	impl CheckpointControl for Control {
		fn checkpoint(
			&self,
			_: Str,
		) -> impl Future<Output = Result<CheckpointAck, CheckpointFault>> + Send {
			std::future::ready(Ok(CheckpointAck { token: sf!("opaque"), started_at: 42 }))
		}

		fn schedule_rewind(
			&self,
			token: Str,
			_: Str,
		) -> impl Future<Output = Result<RewindAck, CheckpointFault>> + Send {
			std::future::ready(Ok(RewindAck { token, receipt: sf!("rewind-1") }))
		}
	}

	#[test]
	fn pair_has_distinct_canonical_slots() {
		let (checkpoint, rewind) = tools(Control);
		assert_eq!(checkpoint.spec().name, "checkpoint");
		assert_eq!(rewind.spec().name, "rewind");
	}

	#[test]
	fn argument_contracts_are_closed() {
		assert!(
			serde_json::from_value::<CheckpointParams>(
				serde_json::json!({"goal":"inspect","extra":true})
			)
			.is_err()
		);
		assert!(
			serde_json::from_value::<RewindParams>(
				serde_json::json!({"token":"opaque","report":"finding"})
			)
			.is_ok()
		);
	}
}
