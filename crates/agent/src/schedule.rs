//! Durable schedule declarations, firing deduplication, and recovery policy.

use std::{
	collections::{HashMap, HashSet},
	time::Duration,
};

use omp_core::{Str, sf};
use parking_lot::Mutex;
use thiserror::Error;

/// Maximum individual missed occurrences replayed by backfill recovery.
pub const MAX_BACKFILL: usize = 32;

/// Policy for occurrences missed while the scheduler was unavailable.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display, strum::EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum MissedRunPolicy {
	/// Drop missed occurrences and increment the missed count.
	Skip,
	/// Deliver one catch-up firing for all missed occurrences.
	Coalesce,
	/// Replay occurrences oldest-first, then coalesce any remainder.
	Backfill,
}

/// Artifact resolution policy for standing authorizations.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, strum::Display, strum::EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum UpgradePolicy {
	/// Run the recorded declaration artifact and pause if it disappears.
	#[default]
	Pinned,
	/// Resolve the currently installed artifact at every firing.
	Auto,
}

/// Persistence scope for a schedule.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display, strum::EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum ScheduleScope {
	/// Persist in a single session journal.
	Session,
	/// Persist in the project durable scheduler store.
	Project,
}

/// Trigger shape stored in the durable schedule spec.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Trigger {
	/// Five- or six-field cron expression and IANA timezone.
	Cron {
		/// Five- or six-field cron expression.
		expr:     Str,
		/// IANA timezone used for DST gap/fold resolution.
		timezone: Str,
	},
	/// UTC-monotonic interval unaffected by DST.
	Every {
		/// Interval between occurrences.
		interval: Duration,
		/// Maximum deterministic delay applied to spread simultaneous schedules.
		jitter:   Duration,
		/// Whether occurrences align to wall-clock interval boundaries.
		align:    bool,
	},
	/// One absolute epoch-millisecond occurrence.
	At {
		/// Scheduled epoch-millisecond instant.
		epoch_ms: u64,
	},
	/// Fires after the agent has remained settled for this duration.
	AfterIdle {
		/// Required uninterrupted settled duration.
		idle: Duration,
	},
}

/// Scheduled delivery target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScheduleDelivery {
	/// Inject a canonical prompt into the owner agent.
	Inject {
		/// Canonical prompt text delivered at the requested boundary.
		prompt: Str,
	},
	/// Start a Core-owned background child described by its durable reference.
	Spawn {
		/// Durable subagent specification reference.
		spec_id: Str,
	},
}

/// Hard per-firing and rolling-window budget for unattended work.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ScheduleBudget {
	/// Maximum durable receipt cost for one firing.
	pub max_usd_per_firing_micros: Option<u64>,
	/// Maximum durable receipt cost inside the configured rolling window.
	pub max_usd_per_window_micros: Option<u64>,
	/// Rolling spend window.
	pub window:                    Duration,
	/// Maximum provider requests for one firing.
	pub max_requests_per_firing:   Option<u64>,
}

/// Durable schedule declaration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Schedule {
	/// Stable schedule id.
	pub id:              Str,
	/// Owner-unique declaration name.
	pub name:            Str,
	/// Trigger that supplies future occurrences.
	pub trigger:         Trigger,
	/// Delivery target.
	pub delivery:        ScheduleDelivery,
	/// Session or project persistence scope.
	pub scope:           ScheduleScope,
	/// Authenticated extension owner.
	pub owner:           Str,
	/// Captured principal that pays and resolves credentials.
	pub principal:       Str,
	/// Artifact digest recorded at declaration.
	pub artifact_digest: Str,
	/// Artifact resolution policy.
	pub upgrade:         UpgradePolicy,
	/// Missed-run recovery policy.
	pub missed:          MissedRunPolicy,
	/// Required for project-scoped spawning.
	pub budget:          Option<ScheduleBudget>,
	/// Whether delivery remains armed.
	pub enabled:         bool,
	/// Last delivered scheduled timestamp.
	pub last_ms:         Option<u64>,
	/// Number of completed firings.
	pub fire_count:      u64,
	/// Number of skipped missed occurrences.
	pub miss_count:      u64,
}

/// Durable firing outcome persisted after delivery.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display, strum::EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum FiringOutcome {
	/// A prompt was injected.
	Injected,
	/// A Core-owned child was started or attached.
	Spawned,
	/// The firing was intentionally skipped.
	Skipped,
	/// Delivery, credentials, or artifact resolution failed.
	Failed,
	/// A replay found a completed inject firing.
	Duplicate,
	/// Spending would exceed the schedule budget before dispatch.
	BudgetRefused,
}

/// Durable intent or outcome record for a schedule occurrence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Firing {
	/// Schedule declaration identity.
	pub schedule_id:     Str,
	/// `(schedule_id, scheduled_at_ms)` idempotency key.
	pub idempotency_key: Str,
	/// Scheduled occurrence timestamp.
	pub at_ms:           u64,
	/// Delay from scheduled to actual delivery.
	pub late_ms:         u64,
	/// Outcome, absent while only intent is journaled.
	pub outcome:         Option<FiringOutcome>,
	/// Artifact actually selected for delivery.
	pub artifact_digest: Str,
	/// Captured owner principal billed for work.
	pub principal:       Str,
	/// Child run identity for spawn delivery.
	pub run_id:          Option<Str>,
	/// Structured diagnostic without secret material.
	pub detail:          Option<Str>,
}

/// Journal boundary required by the scheduler.
///
/// Implementations must atomically persist intent before invoking `deliver`,
/// and persist the returned outcome afterward. The scheduler never owns a
/// runtime or a storage task.
pub trait ScheduleJournal {
	/// Writes a `Kind::Firing` intent with no outcome.
	fn append_firing_intent(&mut self, firing: &Firing) -> Result<(), ScheduleError>;
	/// Writes the final `Kind::Firing` outcome.
	fn append_firing_outcome(&mut self, firing: &Firing) -> Result<(), ScheduleError>;
}

/// Scheduler declaration or execution failure.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ScheduleError {
	/// A project spawn lacks the mandatory hard schedule budget.
	#[error("project-scoped spawn schedules require ScheduleBudget")]
	MissingProjectBudget,
	/// A requested schedule is not declared.
	#[error("invalid schedule trigger: unknown schedule")]
	UnknownSchedule,
	/// An every trigger has a zero interval.
	#[error("invalid schedule trigger: Every interval is zero")]
	ZeroInterval,
	/// A firing would spend outside an installed hard budget.
	#[error("schedule budget refused the firing")]
	BudgetRefused,
	/// The durable journal boundary refused an intent or outcome.
	#[error("schedule journal failed: {0}")]
	Journal(Str),
}

/// In-memory projection of durable schedules and completed firing keys.
///
/// A daemon owns the timer task that calls this table; this type owns no
/// runtime.
pub struct Scheduler {
	schedules: Mutex<HashMap<Str, Schedule>>,
	completed: Mutex<HashSet<Str>>,
}

impl Default for Scheduler {
	fn default() -> Self {
		Self::new()
	}
}

impl Scheduler {
	/// Creates an empty durable schedule projection.
	#[must_use]
	pub fn new() -> Self {
		Self { schedules: Mutex::new(HashMap::new()), completed: Mutex::new(HashSet::new()) }
	}

	/// Validates and upserts an owner-name idempotent schedule declaration.
	pub fn upsert(&self, schedule: Schedule) -> Result<(), ScheduleError> {
		validate(&schedule)?;
		self.schedules.lock().insert(schedule.id.clone(), schedule);
		Ok(())
	}

	/// Records one recovered firing outcome from a durable journal projection.
	pub fn project_firing(&self, firing: &Firing) {
		if firing.outcome.is_some() {
			self.completed.lock().insert(firing.idempotency_key.clone());
		}
	}

	/// Returns a copy of a declared schedule.
	#[must_use]
	pub fn schedule(&self, id: &str) -> Option<Schedule> {
		self.schedules.lock().get(id).cloned()
	}

	/// Fires one occurrence with intent-before-delivery and
	/// outcome-after-delivery.
	pub fn fire<J, D>(
		&self,
		journal: &mut J,
		schedule_id: &str,
		at_ms: u64,
		now_ms: u64,
		mut deliver: D,
	) -> Result<Firing, ScheduleError>
	where
		J: ScheduleJournal,
		D: FnMut(
			&Schedule,
			&Firing,
		) -> Result<(FiringOutcome, Option<Str>, Option<Str>), ScheduleError>,
	{
		let schedule = self
			.schedule(schedule_id)
			.ok_or_else(|| ScheduleError::UnknownSchedule)?;
		let key = firing_key(schedule.id.as_str(), at_ms);
		let mut firing = Firing {
			schedule_id: schedule.id.clone(),
			idempotency_key: key.clone(),
			at_ms,
			late_ms: now_ms.saturating_sub(at_ms),
			outcome: None,
			artifact_digest: schedule.artifact_digest.clone(),
			principal: schedule.principal.clone(),
			run_id: None,
			detail: None,
		};
		journal.append_firing_intent(&firing)?;
		if self.completed.lock().contains(key.as_str()) {
			firing.outcome = Some(match &schedule.delivery {
				ScheduleDelivery::Inject { .. } => FiringOutcome::Duplicate,
				ScheduleDelivery::Spawn { .. } => FiringOutcome::Spawned,
			});
			journal.append_firing_outcome(&firing)?;
			return Ok(firing);
		}
		let (outcome, run_id, detail) = deliver(&schedule, &firing)?;
		firing.outcome = Some(outcome);
		firing.run_id = run_id;
		firing.detail = detail;
		journal.append_firing_outcome(&firing)?;
		self.completed.lock().insert(key);
		if let Some(schedule) = self.schedules.lock().get_mut(schedule_id) {
			schedule.last_ms = Some(at_ms);
			schedule.fire_count = schedule.fire_count.saturating_add(1);
		}
		Ok(firing)
	}

	/// Applies a missed-run policy to already-calculated missed timestamps.
	#[must_use]
	pub fn recover_missed(&self, schedule_id: &str, missed: &[u64]) -> Vec<u64> {
		let mut schedules = self.schedules.lock();
		let Some(schedule) = schedules.get_mut(schedule_id) else {
			return Vec::new();
		};
		match schedule.missed {
			MissedRunPolicy::Skip => {
				schedule.miss_count = schedule.miss_count.saturating_add(missed.len() as u64);
				Vec::new()
			},
			MissedRunPolicy::Coalesce => missed.last().copied().into_iter().collect(),
			MissedRunPolicy::Backfill if missed.len() <= MAX_BACKFILL => missed.to_vec(),
			MissedRunPolicy::Backfill => {
				let mut recovered = missed[..MAX_BACKFILL].to_vec();
				if let Some(last) = missed.last() {
					recovered.push(*last);
				}
				recovered
			},
		}
	}
}

fn validate(schedule: &Schedule) -> Result<(), ScheduleError> {
	if matches!(schedule.scope, ScheduleScope::Project)
		&& matches!(&schedule.delivery, ScheduleDelivery::Spawn { .. })
		&& schedule.budget.is_none()
	{
		return Err(ScheduleError::MissingProjectBudget);
	}
	if let Trigger::Every { interval, .. } = &schedule.trigger
		&& interval.is_zero()
	{
		return Err(ScheduleError::ZeroInterval);
	}
	Ok(())
}

/// Constructs the stable idempotency key for an occurrence.
#[must_use]
pub fn firing_key(schedule_id: &str, at_ms: u64) -> Str {
	sf!("{schedule_id}:{at_ms}")
}

#[cfg(test)]
mod tests {
	use super::*;

	struct Journal(Vec<Firing>);
	impl ScheduleJournal for Journal {
		fn append_firing_intent(&mut self, firing: &Firing) -> Result<(), ScheduleError> {
			self.0.push(firing.clone());
			Ok(())
		}

		fn append_firing_outcome(&mut self, firing: &Firing) -> Result<(), ScheduleError> {
			self.0.push(firing.clone());
			Ok(())
		}
	}
	fn schedule() -> Schedule {
		Schedule {
			id:              sf!("s"),
			name:            sf!("s"),
			trigger:         Trigger::At { epoch_ms: 1 },
			delivery:        ScheduleDelivery::Inject { prompt: sf!("go") },
			scope:           ScheduleScope::Session,
			owner:           sf!("ext"),
			principal:       sf!("p"),
			artifact_digest: sf!("d"),
			upgrade:         UpgradePolicy::Pinned,
			missed:          MissedRunPolicy::Coalesce,
			budget:          None,
			enabled:         true,
			last_ms:         None,
			fire_count:      0,
			miss_count:      0,
		}
	}
	#[test]
	fn duplicate_inject_is_journaled_after_intent() {
		let scheduler = Scheduler::new();
		scheduler.upsert(schedule()).unwrap();
		let mut journal = Journal(Vec::new());
		scheduler
			.fire(&mut journal, "s", 1, 1, |_, _| Ok((FiringOutcome::Injected, None, None)))
			.unwrap();
		let replay = scheduler
			.fire(&mut journal, "s", 1, 2, |_, _| panic!("duplicate must not deliver"))
			.unwrap();
		assert_eq!(replay.outcome, Some(FiringOutcome::Duplicate));
		assert_eq!(journal.0.len(), 4);
	}
}
