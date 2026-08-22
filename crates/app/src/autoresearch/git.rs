//! Git-isolated autoresearch transactions and crash recovery.

use std::{collections::BTreeMap, error::Error as StdError, future::Future};

use omp_core::{Str, sf};
use tokio_util::sync::CancellationToken;

use super::{
	helpers::{branch_candidate, normalize_path, scope_deviations},
	types::{DispositionIntent, ExperimentStatus, ScopeDelta, SessionConfig},
};
use crate::envd::vcs::git::mutation::{
	GitMutation, IsolationCommit, MutationError, MutationOutcome,
};

/// Read-only Git facts required around a named mutation transaction.
pub trait IsolationQueries {
	/// Query failure.
	type Error: StdError + Send + Sync + 'static;

	/// Current branch, absent for detached HEAD.
	fn current_branch(
		&self,
		cancel: &CancellationToken,
	) -> impl Future<Output = Result<Option<Str>, Self::Error>> + Send;
	/// Current commit id.
	fn head(
		&self,
		cancel: &CancellationToken,
	) -> impl Future<Output = Result<Option<Str>, Self::Error>> + Send;
	/// Whether a local branch already exists.
	fn branch_exists(
		&self,
		branch: &str,
		cancel: &CancellationToken,
	) -> impl Future<Output = Result<bool, Self::Error>> + Send;
	/// NUL-framed porcelain-v1 status for the repository.
	fn status(
		&self,
		cancel: &CancellationToken,
	) -> impl Future<Output = Result<Vec<u8>, Self::Error>> + Send;
	/// HEAD after a possibly completed keep transaction.
	fn head_after_recovery(
		&self,
		cancel: &CancellationToken,
	) -> impl Future<Output = Result<Option<Str>, Self::Error>> + Send {
		self.head(cancel)
	}
}

/// Autoresearch Git transaction failure.
#[derive(Debug, thiserror::Error)]
pub enum GitError<E: StdError + 'static> {
	/// A read-only repository query failed.
	#[error("autoresearch Git query failed")]
	Query(#[source] E),
	/// The named mutation authority rejected or failed a transaction.
	#[error("autoresearch Git isolation mutation failed")]
	Mutation(#[from] MutationError),
	/// Git is required unless the caller explicitly selected unisolated mode.
	#[error(
		"autoresearch requires a Git checkout; pass explicit unisolated mode to continue without \
		 isolation"
	)]
	GitRequired,
	/// Branch creation or a fixed commit was rejected by Git.
	#[error("autoresearch Git isolation transaction was rejected")]
	Rejected,
	/// A kept run with scope deviations requires an explicit justification.
	#[error("keeping a scope-deviating run requires justification")]
	MissingJustification,
	/// Crash recovery could not prove that an ambiguous keep commit completed.
	#[error("autoresearch keep transaction is ambiguous and still has uncommitted paths")]
	AmbiguousKeep,
}

/// Result of creating or reusing one experiment branch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IsolationState {
	/// Dedicated branch, absent only in explicit unisolated mode.
	pub branch:          Option<Str>,
	/// Baseline commit after preserving initial dirty paths.
	pub baseline_commit: Option<Str>,
	/// Whether this call created the branch.
	pub created:         bool,
	/// Paths committed as the dirty baseline.
	pub preserved_paths: Vec<Str>,
}

/// Ensures `autoresearch/<goal>-YYYYMMDD[-N]` isolation.
///
/// Dirty user work is carried onto the new branch and committed through the
/// closed autoresearch mutation vocabulary before any experiment begins.
pub async fn ensure_isolation<Q: IsolationQueries>(
	queries: Option<&Q>,
	mutation: Option<&GitMutation>,
	goal: Option<&str>,
	date: &str,
	explicit_unisolated: bool,
	cancel: &CancellationToken,
) -> Result<IsolationState, GitError<Q::Error>> {
	let (Some(queries), Some(mutation)) = (queries, mutation) else {
		return if explicit_unisolated {
			Ok(IsolationState {
				branch:          None,
				baseline_commit: None,
				created:         false,
				preserved_paths: Vec::new(),
			})
		} else {
			Err(GitError::GitRequired)
		};
	};
	if let Some(branch) = queries
		.current_branch(cancel)
		.await
		.map_err(GitError::Query)?
		&& branch.starts_with("autoresearch/")
	{
		return Ok(IsolationState {
			branch:          Some(branch),
			baseline_commit: queries.head(cancel).await.map_err(GitError::Query)?,
			created:         false,
			preserved_paths: Vec::new(),
		});
	}

	let mut suffix = None;
	let branch = loop {
		let candidate = branch_candidate(goal, date, suffix);
		if !queries
			.branch_exists(candidate.as_str(), cancel)
			.await
			.map_err(GitError::Query)?
		{
			break candidate;
		}
		suffix = Some(suffix.unwrap_or(0) + 1);
	};
	if !mutation
		.create_isolation_branch(branch.as_str(), cancel)
		.await?
		.is_applied()
	{
		return Err(GitError::Rejected);
	}
	let status = queries.status(cancel).await.map_err(GitError::Query)?;
	let entries = parse_status(&status);
	let preserved_paths = entries
		.iter()
		.map(|entry| entry.path.clone())
		.collect::<Vec<_>>();
	if !preserved_paths.is_empty() {
		let paths = preserved_paths.iter().map(Str::as_str).collect::<Vec<_>>();
		if !mutation
			.commit_isolation(IsolationCommit::AutoresearchBaseline, &paths, cancel)
			.await?
			.is_applied()
		{
			return Err(GitError::Rejected);
		}
	}
	Ok(IsolationState {
		branch: Some(branch),
		baseline_commit: queries.head(cancel).await.map_err(GitError::Query)?,
		created: true,
		preserved_paths,
	})
}

/// One NUL-safe porcelain-v1 path entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusPath {
	/// Repository-relative path.
	pub path:      Str,
	/// Whether the path is untracked.
	pub untracked: bool,
}

/// Parses line or NUL framed porcelain-v1 status, retaining both rename paths.
pub fn parse_status(status: &[u8]) -> Vec<StatusPath> {
	if status.contains(&0) {
		parse_status_nul(status)
	} else {
		parse_status_lines(status)
	}
}

fn parse_status_nul(status: &[u8]) -> Vec<StatusPath> {
	let mut entries = BTreeMap::<Str, bool>::new();
	let mut cursor = 0;
	while cursor + 3 <= status.len() {
		let code = &status[cursor..cursor + 2];
		cursor += 3;
		let Some(end) = status[cursor..].iter().position(|byte| *byte == 0) else {
			break;
		};
		add_path(&mut entries, &status[cursor..cursor + end], code == b"??");
		cursor += end + 1;
		if matches!(code.first(), Some(b'R' | b'C')) || matches!(code.get(1), Some(b'R' | b'C')) {
			let Some(end) = status[cursor..].iter().position(|byte| *byte == 0) else {
				break;
			};
			add_path(&mut entries, &status[cursor..cursor + end], false);
			cursor += end + 1;
		}
	}
	entries
		.into_iter()
		.map(|(path, untracked)| StatusPath { path, untracked })
		.collect()
}

fn parse_status_lines(status: &[u8]) -> Vec<StatusPath> {
	let mut entries = BTreeMap::<Str, bool>::new();
	for line in status.split(|byte| *byte == b'\n') {
		if line.len() < 4 {
			continue;
		}
		let untracked = &line[..2] == b"??";
		for path in line[3..].split(|byte| *byte == b'>') {
			let path = path.strip_suffix(b" -").unwrap_or(path);
			add_path(&mut entries, path, untracked);
		}
	}
	entries
		.into_iter()
		.map(|(path, untracked)| StatusPath { path, untracked })
		.collect()
}

fn add_path(entries: &mut BTreeMap<Str, bool>, raw: &[u8], untracked: bool) {
	let Ok(path) = std::str::from_utf8(raw) else {
		return;
	};
	let path = path.trim().trim_matches('"');
	if path.is_empty() {
		return;
	}
	entries
		.entry(normalize_path(path))
		.and_modify(|value| *value &= untracked)
		.or_insert(untracked);
}

/// Computes exact paths introduced or changed since the run started.
pub fn run_delta(before: &[Str], current_status: &[u8], session: &SessionConfig) -> ScopeDelta {
	let before = before
		.iter()
		.map(Str::as_str)
		.collect::<std::collections::BTreeSet<_>>();
	let changed = parse_status(current_status)
		.into_iter()
		.filter(|entry| !before.contains(entry.path.as_str()))
		.collect::<Vec<_>>();
	let deviations = scope_deviations(
		changed.iter().map(|entry| entry.path.clone()),
		&session.scope_paths,
		&session.off_limits,
	);
	let mut delta = ScopeDelta { deviations, ..ScopeDelta::default() };
	for entry in changed {
		if entry.untracked {
			delta.untracked.push(entry.path);
		} else {
			delta.tracked.push(entry.path);
		}
	}
	delta
}

/// Executes a previously journaled disposition intent.
pub async fn settle<Q: IsolationQueries>(
	queries: &Q,
	mutation: &GitMutation,
	intent: &DispositionIntent,
	cancel: &CancellationToken,
) -> Result<Option<Str>, GitError<Q::Error>> {
	if intent.status == ExperimentStatus::Keep {
		if !intent.delta.deviations.is_empty() && intent.justification.is_none() {
			return Err(GitError::MissingJustification);
		}
		let mut paths = intent
			.delta
			.tracked
			.iter()
			.map(Str::as_str)
			.collect::<Vec<_>>();
		paths.extend(intent.delta.untracked.iter().map(Str::as_str));
		if paths.is_empty() {
			return queries.head(cancel).await.map_err(GitError::Query);
		}
		let metrics = fixed_metrics_json(intent);
		let outcome = mutation
			.commit_isolation(
				IsolationCommit::AutoresearchRun {
					description:  intent.description.as_str(),
					metrics_json: metrics.as_str(),
				},
				&paths,
				cancel,
			)
			.await?;
		if !outcome.is_applied() {
			return Err(GitError::Rejected);
		}
		return queries.head(cancel).await.map_err(GitError::Query);
	}
	let target = intent.rollback_head.as_deref().unwrap_or("HEAD");
	let tracked = intent
		.delta
		.tracked
		.iter()
		.map(Str::as_str)
		.collect::<Vec<_>>();
	let untracked = intent
		.delta
		.untracked
		.iter()
		.map(Str::as_str)
		.collect::<Vec<_>>();
	if !mutation
		.rollback_isolation(target, &tracked, &untracked, cancel)
		.await?
		.is_applied()
	{
		return Err(GitError::Rejected);
	}
	Ok(None)
}

/// Replays an interrupted transaction without touching paths outside its
/// journaled delta.
pub async fn recover<Q: IsolationQueries>(
	queries: &Q,
	mutation: &GitMutation,
	intent: &DispositionIntent,
	cancel: &CancellationToken,
) -> Result<Option<Str>, GitError<Q::Error>> {
	if intent.status != ExperimentStatus::Keep {
		return settle(queries, mutation, intent, cancel).await;
	}
	let status = queries.status(cancel).await.map_err(GitError::Query)?;
	let dirty = parse_status(&status);
	let intended_dirty = dirty.iter().any(|entry| {
		intent.delta.tracked.contains(&entry.path) || intent.delta.untracked.contains(&entry.path)
	});
	if intended_dirty {
		return settle(queries, mutation, intent, cancel).await;
	}
	queries
		.head_after_recovery(cancel)
		.await
		.map_err(GitError::Query)
}

fn fixed_metrics_json(intent: &DispositionIntent) -> Str {
	let mut values = serde_json::Map::new();
	values.insert(
		"status".to_owned(),
		serde_json::Value::String(match intent.status {
			ExperimentStatus::Keep => "keep".to_owned(),
			ExperimentStatus::Discard => "discard".to_owned(),
			ExperimentStatus::Crash => "crash".to_owned(),
			ExperimentStatus::ChecksFailed => "checks_failed".to_owned(),
		}),
	);
	values.insert("metric".to_owned(), serde_json::json!(intent.metric));
	for (name, value) in &intent.metrics {
		values.insert(name.to_string(), serde_json::json!(value));
	}
	serde_json::to_string(&values)
		.map(Str::from)
		.unwrap_or_else(|_| sf!(""))
}

/// Returns whether a mutation completed successfully.
pub const fn applied(outcome: &MutationOutcome) -> bool {
	outcome.is_applied()
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::autoresearch::types::MetricDirection;

	#[test]
	fn status_delta_never_includes_preexisting_user_paths() {
		let session = SessionConfig {
			name:              "x".into(),
			goal:              None,
			primary_metric:    "m".into(),
			metric_unit:       Str::default(),
			direction:         MetricDirection::Lower,
			branch:            None,
			baseline_commit:   None,
			segment:           0,
			max_iterations:    None,
			scope_paths:       vec!["src".into()],
			off_limits:        vec!["src/secret".into()],
			constraints:       Vec::new(),
			secondary_metrics: Vec::new(),
			notes:             Str::default(),
		};
		let delta =
			run_delta(&["README".into()], b" M README\0 M src/lib.rs\0?? src/secret/key\0", &session);
		assert_eq!(delta.tracked, [Str::from("src/lib.rs")]);
		assert_eq!(delta.untracked, [Str::from("src/secret/key")]);
		assert_eq!(delta.deviations, [Str::from("src/secret/key")]);
	}
}
