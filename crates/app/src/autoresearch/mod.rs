//! Journal-backed autonomous experiment loop.
//!
//! Autoresearch is a named feature consumer of Git mutation authority, never a
//! general commit surface. Every lifecycle mutation is appended to the v4
//! journal before its SQLite query projection changes.

pub mod command;
pub mod dashboard;
pub mod engine;
pub mod git;
pub mod helpers;
pub mod storage;
pub mod types;

pub use command::{Command, ParseError, completions, parse};
pub use dashboard::{Dashboard, RunRow};
pub use engine::{
	AutoresearchHost, ClearTree, DEFAULT_TIMEOUT, Engine, EngineError, HARNESS, HarnessOutput,
	InitExperiment, LogExperiment,
};
use omp_core::{Str, sf};
use omp_storage::transcript::Custom;
use serde_json::value::RawValue;
pub use storage::{JournalAppender, RecordError, Storage, StorageError, StoragePaths};
pub use types::*;

/// v4 custom-event kind carrying authoritative autoresearch facts.
pub const JOURNAL_KIND: &str = "autoresearch";
/// Current autoresearch fact schema revision.
pub const JOURNAL_REVISION: &str = "1";
/// Tool names active only on a branch-matching autoresearch session.
pub const TOOL_NAMES: [&str; 4] =
	["init_experiment", "run_experiment", "log_experiment", "update_notes"];

/// Encodes one fact for a core-authenticated v4 custom event.
pub fn fact_data(fact: &JournalFact) -> Result<Box<RawValue>, serde_json::Error> {
	serde_json::value::to_raw_value(fact)
}

/// Decodes one autoresearch custom event for replay or projection rebuild.
pub fn decode_custom(custom: &Custom) -> Result<Option<JournalFact>, serde_json::Error> {
	if custom.kind() != JOURNAL_KIND || custom.rev() != Some(JOURNAL_REVISION) {
		return Ok(None);
	}
	custom
		.data()
		.map(|data| serde_json::from_str(data.get()))
		.transpose()
}

/// Returns the strict JSON Schema for one autoresearch tool.
pub fn tool_schema(name: &str) -> Option<serde_json::Value> {
	match name {
		"init_experiment" => Some(serde_json::json!({
			"type": "object",
			"additionalProperties": false,
			"required": ["name", "primary_metric"],
			"properties": {
				"name": {"type": "string", "minLength": 1},
				"goal": {"type": "string"},
				"primary_metric": {"type": "string", "minLength": 1},
				"metric_unit": {"type": "string"},
				"direction": {"enum": ["lower", "higher"]},
				"secondary_metrics": {"type": "array", "items": {"type": "string"}},
				"scope_paths": {"type": "array", "items": {"type": "string"}},
				"off_limits": {"type": "array", "items": {"type": "string"}},
				"constraints": {"type": "array", "items": {"type": "string"}},
				"max_iterations": {"type": "integer", "minimum": 1},
				"new_segment": {"type": "boolean"},
				"unisolated": {"type": "boolean"}
			}
		})),
		"run_experiment" => Some(serde_json::json!({
			"type": "object",
			"additionalProperties": false,
			"properties": {"timeout_seconds": {"type": "number", "minimum": 0}}
		})),
		"log_experiment" => Some(serde_json::json!({
			"type": "object",
			"additionalProperties": false,
			"required": ["metric", "status", "description"],
			"properties": {
				"metric": {"type": "number"},
				"status": {"enum": ["keep", "discard", "crash", "checks_failed"]},
				"description": {"type": "string", "minLength": 1},
				"metrics": {"type": "object", "additionalProperties": {"type": "number"}},
				"asi": {"type": "object"},
				"justification": {"type": "string"},
				"flag_runs": {"type": "array", "items": {"type": "object", "additionalProperties": false, "required": ["run_id", "reason"], "properties": {"run_id": {"type": "integer"}, "reason": {"type": "string", "minLength": 1}}}}
			}
		})),
		"update_notes" => Some(serde_json::json!({
			"type": "object",
			"additionalProperties": false,
			"required": ["notes"],
			"properties": {"notes": {"type": "string"}}
		})),
		_ => None,
	}
}

/// Phase-one setup prompt installed when mode starts before initialization.
pub fn setup_prompt(goal: Option<&str>) -> Str {
	Str::from(format!(
		"Autoresearch setup phase. Objective: {}. Inspect existing measurements and build \
		 executable {HARNESS}. It must run the real workload, exit nonzero on failure, and print at \
		 least one finite `METRIC name=value` line. Run it yourself before init_experiment. Do not \
		 fabricate metrics or optimize the harness instead of the target.",
		goal.unwrap_or("discover a measurable improvement"),
	))
}

/// Iteration prompt containing every durable control input.
pub fn experiment_prompt(
	session: &SessionConfig,
	baseline: Option<f64>,
	best: Option<f64>,
	deviations: &[Str],
) -> Str {
	let direction = match session.direction {
		MetricDirection::Lower => "lower",
		MetricDirection::Higher => "higher",
	};
	let scope = if session.scope_paths.is_empty() {
		sf!("the repository")
	} else {
		Str::from(
			session
				.scope_paths
				.iter()
				.map(Str::as_str)
				.collect::<Vec<_>>()
				.join(", "),
		)
	};
	Str::from(format!(
		"Autoresearch iteration. Goal: {}. Session: {}. Segment: {}. Primary metric: {}{} \
		 ({direction} is better). Baseline: {}. Best: {}. Scope: {}. Off limits: {}. Constraints: \
		 {}. Prior deviations: {}. Playbook notes:\n{}\nMake one attributable change, run \
		 {HARNESS}, then log keep/discard/crash/checks_failed. Never keep a scope deviation without \
		 justification.",
		session
			.goal
			.as_deref()
			.unwrap_or("improve the measured system"),
		session.name,
		session.segment,
		session.primary_metric,
		session.metric_unit,
		baseline.map_or_else(|| "pending".to_owned(), |value| value.to_string()),
		best.map_or_else(|| "pending".to_owned(), |value| value.to_string()),
		scope,
		join_or_none(&session.off_limits),
		join_or_none(&session.constraints),
		join_or_none(deviations),
		session.notes,
	))
}

fn join_or_none(values: &[Str]) -> String {
	if values.is_empty() {
		"none".to_owned()
	} else {
		values
			.iter()
			.map(Str::as_str)
			.collect::<Vec<_>>()
			.join(", ")
	}
}
