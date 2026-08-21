//! Per-invocation admission gate between finalized arguments and authorization.

use std::{io::Cursor, path::Path, time::Duration};

use bytes::{Bytes, BytesMut};
use omp_core::Str;
use omp_proto::{
	env::v1::{Admission, AdmitInvocation},
	policy::v1::{BashIr, EffectEnvelope, PolicyDenied},
};
use omp_shell_engine::{
	analysis,
	parser::{Parser, ParserOptions},
};
use omp_tool::Effects;
use serde_json::Value;
use thiserror::Error;

/// A finalized admission result, with policy transformation applied before the
/// executor can observe arguments.
pub enum AdmissionDecision {
	/// The effective canonical arguments and regenerated shell facts.
	Allowed {
		/// RFC 8785-compatible serde canonical argument bytes for the executor.
		raw:  Bytes,
		/// Shell facts regenerated from the effective command, when applicable.
		bash: Option<BashIr>,
	},
	/// A refusal carrying the one wire vocabulary for policy denial.
	Denied(PolicyDenied),
}

/// A protocol or transformation failure while admitting an invocation.
#[derive(Debug, Error)]
pub enum AdmissionError {
	/// Arguments ended in a value that cannot be transformed as an object.
	#[error("finalized invocation arguments must be a JSON object")]
	ArgumentsNotObject,
	/// A policy transform was not a valid JSON merge patch.
	#[error("admission argument patch is not valid JSON")]
	InvalidPatch,
	/// The admission response belonged to another invocation.
	#[error("admission response invocation id did not match the pending invocation")]
	WrongInvocation,
	/// No admission query has reached the finalized-arguments transition.
	#[error("admission response arrived before finalized arguments")]
	NotPending,
	/// The query's single response has already been accepted.
	#[error("admission response was already supplied")]
	AlreadyAnswered,
}

/// Env-owned one-shot admission state for one invocation.
pub struct AdmissionGate {
	invocation_id: Str,
	tool_name:     Str,
	deadline:      tokio::time::Instant,
	fragments:     BytesMut,
	requested:     Option<Value>,
	query_emitted: bool,
	answer_tx:     Option<flume::Sender<Admission>>,
	answer_rx:     flume::Receiver<Admission>,
}

impl AdmissionGate {
	/// Starts an OPEN invocation gate whose deadline is enforced by env.
	pub(crate) fn new(invocation_id: Str, tool_name: Str, deadline: Duration) -> Self {
		let (answer_tx, answer_rx) = flume::bounded(1);
		Self {
			invocation_id,
			tool_name,
			deadline: tokio::time::Instant::now() + deadline,
			fragments: BytesMut::new(),
			requested: None,
			query_emitted: false,
			answer_tx: Some(answer_tx),
			answer_rx,
		}
	}

	/// Appends one raw argument fragment and emits a query once it is one JSON
	/// document. Further fragments remain the caller's protocol violation.
	pub(crate) fn push_fragment(
		&mut self,
		fragment: &str,
		cwd: &Path,
		root: &Path,
	) -> Option<AdmitInvocation> {
		if self.query_emitted {
			return None;
		}
		self.fragments.extend_from_slice(fragment.as_bytes());
		let value = serde_json::from_slice::<Value>(&self.fragments).ok()?;
		if !value.is_object() {
			return None;
		}
		Some(self.finish_query(value, cwd, root))
	}

	/// Finalizes a call that supplied its complete arguments only with
	/// `ArgsCommitted`, replacing any incomplete speculative fragments.
	pub(crate) fn finalize(
		&mut self,
		raw: &[u8],
		cwd: &Path,
		root: &Path,
	) -> Result<Option<AdmitInvocation>, AdmissionError> {
		if self.query_emitted {
			return Ok(None);
		}
		let value =
			serde_json::from_slice::<Value>(raw).map_err(|_| AdmissionError::ArgumentsNotObject)?;
		if !value.is_object() {
			return Err(AdmissionError::ArgumentsNotObject);
		}
		self.fragments.clear();
		self.fragments.extend_from_slice(raw);
		Ok(Some(self.finish_query(value, cwd, root)))
	}

	fn finish_query(&mut self, value: Value, cwd: &Path, root: &Path) -> AdmitInvocation {
		let bash = bash_ir(&self.tool_name, &value, cwd, root);
		self.requested = Some(value);
		self.query_emitted = true;
		AdmitInvocation {
			invocation_id: self.invocation_id.to_string(),
			bash,
			deadline_ms: self
				.deadline
				.saturating_duration_since(tokio::time::Instant::now())
				.as_millis()
				.try_into()
				.unwrap_or(u64::MAX),
			props: Default::default(),
		}
	}

	/// Accepts Core's one answer without allowing it to block the dispatcher.
	pub(crate) fn answer(&mut self, admission: Admission) -> Result<(), AdmissionError> {
		if !self.query_emitted {
			return Err(AdmissionError::NotPending);
		}
		if admission.invocation_id != self.invocation_id {
			return Err(AdmissionError::WrongInvocation);
		}
		let Some(answer_tx) = self.answer_tx.take() else {
			return Err(AdmissionError::AlreadyAnswered);
		};
		answer_tx
			.send(admission)
			.map_err(|_| AdmissionError::AlreadyAnswered)
	}

	/// Reports whether Core's one admission answer has arrived.
	pub(crate) const fn is_answered(&self) -> bool {
		self.query_emitted && self.answer_tx.is_none()
	}

	/// Returns the deadline for a query that is waiting on Core.
	pub(crate) fn pending_deadline(&self) -> Option<tokio::time::Instant> {
		(self.query_emitted && self.answer_tx.is_some()).then_some(self.deadline)
	}

	/// Converts an unanswered query whose deadline has elapsed into env's
	/// synthetic denial. The connection loop owns this transition.
	pub(crate) fn expire(&mut self, now: tokio::time::Instant) -> Option<PolicyDenied> {
		(self.query_emitted && self.answer_tx.is_some() && now >= self.deadline).then(|| {
			self.answer_tx.take();
			timeout_denial(&self.invocation_id)
		})
	}

	/// Waits for Core's answer through the env-owned deadline, synthesizing the
	/// structured fail-closed denial when it expires or the relay closes.
	pub(crate) async fn decide(&self, cwd: &Path, root: &Path) -> AdmissionDecision {
		let answer = tokio::time::timeout_at(self.deadline, self.answer_rx.recv_async()).await;
		let Ok(Ok(admission)) = answer else {
			return AdmissionDecision::Denied(timeout_denial(&self.invocation_id));
		};
		if !admission.allow {
			return AdmissionDecision::Denied(
				admission
					.denied
					.unwrap_or_else(|| timeout_denial(&self.invocation_id)),
			);
		}
		let Some(requested) = self.requested.as_ref() else {
			return AdmissionDecision::Denied(timeout_denial(&self.invocation_id));
		};
		let Ok((raw, bash)) =
			apply_admission_patch(requested, &admission.args_patch, &self.tool_name, cwd, root)
		else {
			return AdmissionDecision::Denied(invalid_patch_denial(&self.invocation_id));
		};
		AdmissionDecision::Allowed { raw, bash }
	}
}

/// Refuses an envelope that would widen the resolved tool declaration.
pub fn effects_narrow_or_refuse(
	requested: Option<&EffectEnvelope>,
	maximum: &Effects,
) -> Option<Effects> {
	let requested = requested.map(Effects::try_from).transpose().ok()?;
	match requested {
		Some(requested) => maximum.narrow(requested),
		None => Some(Effects::empty()),
	}
}

fn apply_admission_patch(
	requested: &Value,
	patch: &[u8],
	tool_name: &str,
	cwd: &Path,
	root: &Path,
) -> Result<(Bytes, Option<BashIr>), AdmissionError> {
	let mut effective = requested.clone();
	if !patch.is_empty() {
		let patch = serde_json::from_slice(patch).map_err(|_| AdmissionError::InvalidPatch)?;
		merge_patch(&mut effective, patch);
	}
	if !effective.is_object() {
		return Err(AdmissionError::ArgumentsNotObject);
	}
	let raw = serde_json::to_vec(&effective).map_err(|_| AdmissionError::InvalidPatch)?;
	let bash = bash_ir(tool_name, &effective, cwd, root);
	Ok((Bytes::from(raw), bash))
}

fn merge_patch(target: &mut Value, patch: Value) {
	let Value::Object(patch) = patch else {
		*target = patch;
		return;
	};
	if !target.is_object() {
		*target = Value::Object(Default::default());
	}
	let Value::Object(target) = target else {
		return;
	};
	for (key, value) in patch {
		if value.is_null() {
			target.remove(&key);
		} else {
			merge_patch(target.entry(key).or_insert(Value::Null), value);
		}
	}
}

fn bash_ir(tool_name: &str, args: &Value, cwd: &Path, root: &Path) -> Option<BashIr> {
	if tool_name != "shell" {
		return None;
	}
	let command = args.get("command")?.as_str()?;
	let mut parser = Parser::new(Cursor::new(command), &ParserOptions::default());
	Some(match parser.parse_program() {
		Ok(program) => BashIr::from(&analysis::analyze(
			&program,
			cwd.to_string_lossy().as_ref(),
			root.to_string_lossy().as_ref(),
		)),
		Err(error) => BashIr {
			source: command.to_owned(),
			parse_ok: false,
			parse_error: Some(error.to_string()),
			..BashIr::default()
		},
	})
}

fn timeout_denial(invocation_id: &str) -> PolicyDenied {
	PolicyDenied {
		reason:      "admission deadline elapsed".into(),
		code:        "admission_timeout".into(),
		decision_id: invocation_id.to_owned(),
		rules:       Vec::new(),
		props:       Default::default(),
	}
}

fn invalid_patch_denial(invocation_id: &str) -> PolicyDenied {
	PolicyDenied {
		reason:      "admission transformation was invalid".into(),
		code:        "admission_invalid_patch".into(),
		decision_id: invocation_id.to_owned(),
		rules:       Vec::new(),
		props:       Default::default(),
	}
}

#[cfg(test)]
mod tests {
	use std::{path::Path, time::Duration};

	use bytes::Bytes;
	use omp_core::sf;
	use omp_proto::policy::v1::{EffectEnvelope, ExecEffects};
	use omp_tool::{Effects, ExecEffects as ToolExecEffects};

	use super::{AdmissionDecision, AdmissionGate, apply_admission_patch, effects_narrow_or_refuse};

	#[tokio::test]
	async fn deadline_synthesizes_a_structured_denial() {
		let gate = AdmissionGate::new(sf!("call"), sf!("shell"), Duration::ZERO);
		let AdmissionDecision::Denied(denied) =
			gate.decide(Path::new("/work"), Path::new("/work")).await
		else {
			panic!("elapsed admission deadline must deny");
		};
		assert_eq!(denied.code, "admission_timeout");
	}

	#[test]
	fn patch_changes_effective_args_and_regenerates_shell_facts() {
		let requested = serde_json::json!({"command": "echo requested"});
		let (raw, bash) = apply_admission_patch(
			&requested,
			br#"{"command":"echo effective"}"#,
			"shell",
			Path::new("/work"),
			Path::new("/work"),
		)
		.expect("valid merge patch");
		assert_eq!(raw, Bytes::from_static(br#"{"command":"echo effective"}"#));
		assert_eq!(bash.expect("shell facts").source, "echo effective");
	}

	#[test]
	fn widened_effect_envelope_is_refused() {
		let maximum = Effects {
			exec: Some(ToolExecEffects { commands: [sf!("git")].into(), network: false }),
			..Effects::empty()
		};
		let requested = EffectEnvelope {
			exec: Some(ExecEffects {
				commands: vec!["git".into(), "curl".into()],
				network:  false,
				props:    None,
			}),
			..EffectEnvelope::default()
		};
		assert!(effects_narrow_or_refuse(Some(&requested), &maximum).is_none());
	}
}
