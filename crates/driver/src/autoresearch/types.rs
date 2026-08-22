//! Durable autoresearch domain records.

use std::collections::BTreeMap;

use omp_core::Str;

/// Whether a smaller or larger primary metric is better.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricDirection {
	/// Smaller measurements are improvements.
	#[default]
	Lower,
	/// Larger measurements are improvements.
	Higher,
}

/// Terminal disposition of one experiment run.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentStatus {
	/// Retain and commit the measured change.
	Keep,
	/// Reject the measured change.
	Discard,
	/// The benchmark crashed or timed out.
	Crash,
	/// Validation after the benchmark failed.
	ChecksFailed,
}

/// Numeric metrics keyed by the harness-emitted name.
pub type Metrics = BTreeMap<Str, f64>;
/// Sanitized ASI metadata keyed by the harness-emitted name.
pub type Asi = serde_json::Map<String, serde_json::Value>;

/// Configuration fixed for one experiment session.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct SessionConfig {
	/// Human-readable experiment name.
	pub name:              Str,
	/// User objective, when supplied.
	pub goal:              Option<Str>,
	/// Primary `METRIC` key.
	pub primary_metric:    Str,
	/// Display suffix inferred or selected for the primary metric.
	pub metric_unit:       Str,
	/// Improvement direction.
	pub direction:         MetricDirection,
	/// Dedicated isolation branch, absent only in explicit unisolated mode.
	pub branch:            Option<Str>,
	/// Commit forming the current segment baseline.
	pub baseline_commit:   Option<Str>,
	/// Current multi-segment baseline number.
	pub segment:           u32,
	/// Optional iteration cap for each segment.
	pub max_iterations:    Option<u32>,
	/// Paths expected to change.
	pub scope_paths:       Vec<Str>,
	/// Paths that must not change.
	pub off_limits:        Vec<Str>,
	/// Free-form experiment constraints.
	pub constraints:       Vec<Str>,
	/// Secondary metric names.
	pub secondary_metrics: Vec<Str>,
	/// Persisted experiment playbook.
	pub notes:             Str,
}

/// Facts known when a harness invocation starts.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct RunStart {
	/// Session owning the run.
	pub session_id:      i64,
	/// Session segment at launch.
	pub segment:         u32,
	/// Fixed harness command.
	pub command:         Str,
	/// Millisecond timestamp.
	pub started_at_ms:   i64,
	/// HEAD before edits and benchmark execution.
	pub pre_run_head:    Option<Str>,
	/// Dirty paths present before the run.
	pub pre_dirty_paths: Vec<Str>,
	/// Per-run artifact directory.
	pub artifact_dir:    Str,
}

/// Complete bounded harness outcome.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct RunCompletion {
	/// Run being completed.
	pub run_id:          i64,
	/// Completion timestamp.
	pub completed_at_ms: i64,
	/// Wall duration.
	pub duration_ms:     i64,
	/// Process exit code, absent when no exit was observed.
	pub exit_code:       Option<i32>,
	/// Whether the configured deadline cancelled the process.
	pub timed_out:       bool,
	/// Parsed primary measurement.
	pub parsed_primary:  Option<f64>,
	/// Every valid numeric metric.
	pub parsed_metrics:  Metrics,
	/// Sanitized ASI metadata.
	pub parsed_asi:      Asi,
}

/// Exact tree delta captured before a keep or rollback transaction.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ScopeDelta {
	/// Changed tracked paths.
	pub tracked:    Vec<Str>,
	/// Newly-created untracked paths.
	pub untracked:  Vec<Str>,
	/// Paths outside scope or inside an off-limits prefix.
	pub deviations: Vec<Str>,
}

/// Requested disposition and its crash-recovery inputs.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct DispositionIntent {
	/// Run being settled.
	pub run_id:        i64,
	/// Requested terminal status.
	pub status:        ExperimentStatus,
	/// Human-readable result description.
	pub description:   Str,
	/// Reported primary measurement.
	pub metric:        f64,
	/// Reported secondary measurements.
	pub metrics:       Metrics,
	/// Sanitized ASI metadata.
	pub asi:           Asi,
	/// Exact tree delta used for commit or rollback.
	pub delta:         ScopeDelta,
	/// Required explanation when retaining a deviation.
	pub justification: Option<Str>,
	/// Commit to restore from on rollback.
	pub rollback_head: Option<Str>,
	/// Timestamp at which settlement started.
	pub started_at_ms: i64,
}

/// Successful completion of one disposition transaction.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct DispositionSettled {
	/// Run being settled.
	pub run_id:        i64,
	/// Resulting commit for a kept run.
	pub commit:        Option<Str>,
	/// MAD confidence after this run.
	pub confidence:    Option<f64>,
	/// Settlement timestamp.
	pub settled_at_ms: i64,
}

/// Append-only autoresearch journal vocabulary.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum JournalFact {
	/// Create a new durable experiment session.
	SessionOpened {
		/// Stable SQLite projection id allocated by the journal owner.
		id:     i64,
		/// Complete session configuration.
		config: SessionConfig,
		/// Creation timestamp.
		at_ms:  i64,
	},
	/// Replace session configuration or begin a new segment.
	SessionUpdated {
		/// Session being updated.
		id:     i64,
		/// Complete replacement configuration.
		config: SessionConfig,
		/// Update timestamp.
		at_ms:  i64,
	},
	/// Close the active session.
	SessionClosed {
		/// Session being closed.
		id:    i64,
		/// Close timestamp.
		at_ms: i64,
	},
	/// Persist playbook notes.
	NotesUpdated {
		/// Session being updated.
		id:    i64,
		/// Complete replacement notes.
		notes: Str,
		/// Update timestamp.
		at_ms: i64,
	},
	/// Start one harness invocation.
	RunStarted {
		/// Stable run id allocated by the journal owner.
		id:    i64,
		/// Launch facts.
		start: RunStart,
	},
	/// Record the process outcome and parsed output.
	RunCompleted(RunCompletion),
	/// Mark an incomplete run abandoned during rehydration.
	RunAbandoned {
		/// Run being abandoned.
		run_id: i64,
		/// Abandonment timestamp.
		at_ms:  i64,
	},
	/// Begin an idempotent Git disposition transaction.
	DispositionStarted(DispositionIntent),
	/// Publish successful Git settlement.
	DispositionSettled(DispositionSettled),
	/// Mark a prior run suspect and exclude it from baseline math.
	RunFlagged {
		/// Run being flagged.
		run_id: i64,
		/// User/model-provided reason.
		reason: Str,
		/// Flag timestamp.
		at_ms:  i64,
	},
	/// Register one artifact owned by a run.
	ArtifactRecorded {
		/// Run owning the artifact.
		run_id: i64,
		/// Artifact kind, such as `benchmark_log`.
		kind:   Str,
		/// Shared artifact/blob authority URI.
		uri:    Str,
		/// Exact byte length.
		bytes:  u64,
		/// Registration timestamp.
		at_ms:  i64,
	},
}

/// Reconstructed session-local runtime state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RuntimeState {
	/// Durable campaign engagement restored by the Agent journal.
	pub engagement:   Option<Str>,
	/// Current user goal.
	pub goal:         Option<Str>,
	/// Active session projection.
	pub session:      Option<SessionConfig>,
	/// Latest pending run, if any.
	pub pending_run:  Option<i64>,
	/// Whether a hidden resume should be queued after settlement.
	pub resume_armed: bool,
	/// Current dashboard presentation.
	pub dashboard:    DashboardMode,
}

/// Autoresearch dashboard presentation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DashboardMode {
	/// Compact sticky summary.
	#[default]
	Collapsed,
	/// Expanded inline run table.
	Expanded,
	/// Fullscreen navigable run dashboard.
	Fullscreen,
}
