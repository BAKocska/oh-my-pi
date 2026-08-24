//! Exercises public regime lifecycle, resource ownership, revival, and journal
//! recovery.

use std::{env, fs, sync::Arc};

use omp_agent::{
	AcquireOutcome, Arbiter, DeclareError, Next, Point, PointCx, PointSet, Regime, RegimeContext,
	RegimeError, RegimeLifetime, RegimeSet, RegimeSpec, RegimeStateError, RegimeStatus,
	RegimeStepResult, RegimeWhen, Resource, ScopedSetting, SettingSlot, StartError, StartOptions,
	StopError,
	arbiter::RegimeFact,
	tool_choice::{PushOptions, ToolChoiceQueue},
};
use omp_core::{Str, sf};
use omp_inference::call::ToolChoice;
use omp_storage::transcript::{Header, SessionId};

struct StatefulRegime {
	state: Str,
}

impl Regime for StatefulRegime {
	fn apply(&mut self, _: &mut RegimeContext<'_>, _: Next<'_>) -> Result<(), RegimeError> {
		Ok(())
	}

	fn state(&self) -> Str {
		self.state.clone()
	}

	fn restore(&mut self, payload: &str) -> Result<(), RegimeStateError> {
		self.state = Str::new(payload);
		Ok(())
	}
}

fn handler(state: &str) -> Box<dyn Regime> {
	Box::new(StatefulRegime { state: Str::new(state) })
}

fn spec(id: &str, precedence: i16, events: PointSet) -> Arc<RegimeSpec> {
	Arc::new(RegimeSpec {
		id: Str::new(id),
		events,
		precedence,
		max_steps: None,
		committed_step_interval_ms: None,
		on_limit: false,
		lifetime: RegimeLifetime::Run,
		family_rev: sf!("test@1"),
		when: None,
		owns: Arc::from([]),
		sets: Arc::from([]),
		minimum_duration_ms: None,
	})
}

fn owning_spec(
	id: &str,
	owns: impl Into<Arc<[Resource]>>,
	minimum_duration_ms: Option<u64>,
) -> Arc<RegimeSpec> {
	let mut declaration = Arc::unwrap_or_clone(spec(id, 0, PointSet::EMPTY));
	declaration.lifetime = RegimeLifetime::Session;
	declaration.owns = owns.into();
	declaration.minimum_duration_ms = minimum_duration_ms;
	Arc::new(declaration)
}

fn journal_path(label: &str) -> std::path::PathBuf {
	env::temp_dir().join(format!(
		"omp-agent-{label}-{}-{}.jsonl",
		std::process::id(),
		omp_core::Ulid::generate()
	))
}

fn journal(path: &std::path::Path, id: &'static str) -> omp_agent::Journal {
	omp_agent::Journal::create(path, &Header {
		v:       4,
		id:      SessionId(Str::new_static(id)),
		created: 1,
		cwd:     env::temp_dir(),
	})
	.expect("create journal")
}

#[test]
fn declarative_when_is_evaluated_from_core_point_facts() {
	let mut regimes = RegimeSet::new();
	let mut declaration = Arc::unwrap_or_clone(spec("automatic", 0, Point::Admission.set()));
	declaration.when = Some(RegimeWhen {
		point:             Point::Admission,
		invocation_id:     Some(sf!("call-1")),
		stream_contains:   None,
		delivered:         Some(true),
		checkpoint_active: Some(false),
	});
	let declaration = Arc::new(declaration);
	let missed = regimes
		.start_when(
			Arc::clone(&declaration),
			handler("state"),
			StartOptions { now_ms: 1, queue: false },
			Point::Admission,
			&PointCx { invocation_id: Some("call-2"), delivered: true, ..PointCx::default() },
		)
		.expect("evaluate predicate");
	assert!(missed.is_none());
	let started = regimes
		.start_when(
			declaration,
			handler("state"),
			StartOptions { now_ms: 2, queue: false },
			Point::Admission,
			&PointCx { invocation_id: Some("call-1"), delivered: true, ..PointCx::default() },
		)
		.expect("start matching regime");
	assert!(started.is_some());
	assert_eq!(regimes.len(), 1);
}

#[test]
fn queued_tool_choices_retain_exclusive_in_flight_ownership() {
	let mut queue = ToolChoiceQueue::new();
	queue.push_once(ToolChoice::Named("alpha".into()), PushOptions::default());
	queue.push_once(ToolChoice::Named("beta".into()), PushOptions::default());
	assert_eq!(queue.len(), 2);
	assert!(matches!(queue.claim_next(), Some(ToolChoice::Named(_))));
	assert!(queue.claim_next().is_none(), "one choice remains exclusively in flight");
	queue.resolve();
	assert!(matches!(queue.claim_next(), Some(ToolChoice::Named(_))));
}

#[test]
fn committed_step_bounds_saturate_and_respect_the_interval() {
	let mut declaration = Arc::unwrap_or_clone(spec("bounded", 0, Point::TurnEnd.set()));
	declaration.max_steps = Some(2);
	declaration.committed_step_interval_ms = Some(10);
	declaration.on_limit = true;
	let mut regimes = RegimeSet::new();
	let activation = regimes
		.start(Arc::new(declaration), handler("state"), StartOptions { now_ms: 1, queue: false })
		.expect("start bounded regime")
		.activation;
	assert_eq!(regimes.advance(activation.as_str(), 10), RegimeStepResult::Advanced {
		committed_steps: 1,
	});
	assert_eq!(
		regimes.advance(activation.as_str(), 19),
		RegimeStepResult::Advanced { committed_steps: 1 },
		"an uncommitted interval must not consume the bound"
	);
	assert_eq!(regimes.advance(activation.as_str(), 20), RegimeStepResult::Limited {
		committed_steps: 2,
	});
	assert_eq!(regimes.advance(activation.as_str(), 30), RegimeStepResult::Limited {
		committed_steps: 2,
	});
	assert_eq!(regimes.records()[0].committed_steps, 2);
}

#[test]
fn resource_ownership_is_fifo_and_scoped_settings_follow_the_owner() {
	let mut declaration = Arc::unwrap_or_clone(owning_spec("mode", [Resource::Mode], None));
	declaration.sets =
		Arc::from([ScopedSetting { slot: SettingSlot::PromptSlot, value: sf!("mode") }]);
	let declaration = Arc::new(declaration);
	let mut regimes = RegimeSet::new();
	let holder = regimes
		.start(Arc::clone(&declaration), handler("holder"), StartOptions {
			now_ms: 41,
			queue:  false,
		})
		.expect("start holder")
		.activation;
	let denied = regimes
		.start(Arc::clone(&declaration), handler("denied"), StartOptions {
			now_ms: 42,
			queue:  false,
		})
		.expect_err("non-queueing acquisition must be denied");
	assert_eq!(denied, StartError::Acquire {
		resource: Resource::Mode,
		outcome:  AcquireOutcome::Denied { holder: holder.clone(), since: 41 },
	});
	let queued = regimes
		.start(declaration, handler("queued"), StartOptions { now_ms: 43, queue: true })
		.expect("queue waiter");
	assert_eq!(queued.resource, Some(Resource::Mode));
	assert_eq!(queued.outcome, AcquireOutcome::Queued { holder: holder.clone(), since: 41 });
	assert!(regimes.is_queued(queued.activation.as_str()));
	let records = regimes.records();
	assert_eq!(records.len(), 2);
	assert!(
		records
			.iter()
			.any(|record| { record.activation == holder && record.status == RegimeStatus::Active })
	);
	assert!(records.iter().any(|record| {
		record.activation == queued.activation && record.status == RegimeStatus::Queued
	}));
	assert_eq!(regimes.resources().owner(&Resource::Mode), Some(holder.as_str()));
	assert_eq!(regimes.resources().current(&SettingSlot::PromptSlot), Some("mode"));
	assert!(regimes.stop(holder.as_str(), 44).expect("stop holder"));
	assert!(!regimes.is_queued(queued.activation.as_str()));
	assert_eq!(regimes.resources().owner(&Resource::Mode), Some(queued.activation.as_str()));
	assert_eq!(regimes.resources().queue_depth(&Resource::Mode), 0);
	assert_eq!(regimes.resources().current(&SettingSlot::PromptSlot), Some("mode"));
}

#[test]
fn child_stop_releases_the_complete_resource_and_setting_subtree() {
	let mut regimes = RegimeSet::new();
	let parent = regimes
		.start(owning_spec("parent", [Resource::Mode], None), handler("parent"), StartOptions {
			now_ms: 1,
			queue:  false,
		})
		.expect("start parent")
		.activation;
	let mut child_spec = Arc::unwrap_or_clone(owning_spec("child", [Resource::Worktree], None));
	child_spec.sets =
		Arc::from([ScopedSetting { slot: SettingSlot::ModelRoute, value: sf!("child-route") }]);
	let child = regimes
		.start_child(Arc::new(child_spec), handler("child"), Some(parent.clone()), StartOptions {
			now_ms: 2,
			queue:  false,
		})
		.expect("start child")
		.activation;
	assert_eq!(regimes.resources().owner(&Resource::Worktree), Some(child.as_str()));
	assert!(regimes.stop(parent.as_str(), 3).expect("stop subtree"));
	assert!(regimes.is_empty());
	assert_eq!(regimes.resources().owner(&Resource::Mode), None);
	assert_eq!(regimes.resources().owner(&Resource::Worktree), None);
	assert_eq!(regimes.resources().current(&SettingSlot::ModelRoute), None);
}

#[test]
fn unknown_resources_are_rejected_at_declaration() {
	assert_eq!(omp_agent::RESOURCE_TABLE.map(|resource| resource.name), [
		"tool_choice",
		"worktree",
		"director",
		"editor-surface",
		"batch-execution",
		"mode",
	]);
	let declaration = owning_spec("unknown", [Resource::Named(sf!("missing"))], None);
	assert_eq!(
		RegimeSet::new().declare(&declaration),
		Err(DeclareError::UnknownResource { resource: Resource::Named(sf!("missing")) })
	);
}

#[test]
fn built_in_session_regimes_own_the_mode_resource() {
	let owns = |spec: RegimeSpec| spec.owns.iter().cloned().collect::<Vec<_>>();
	assert_eq!(owns(omp_agent::plan_regime_spec()), vec![Resource::Mode, Resource::Worktree]);
	assert_eq!(owns(omp_agent::vibe_regime_spec()), vec![Resource::Mode, Resource::Director]);
	assert_eq!(owns(omp_agent::goal_regime_spec()), vec![Resource::Mode]);
	assert_eq!(owns(omp_agent::autoresearch_regime_spec()), vec![
		Resource::Mode,
		Resource::Worktree
	]);
}

#[test]
fn minimum_duration_refuses_ordinary_stop_but_cancel_is_immediate() {
	let mut regimes = RegimeSet::new();
	let activation = regimes
		.start(
			owning_spec("minimum-stay", [Resource::Mode], Some(100)),
			handler("state"),
			StartOptions { now_ms: 10, queue: false },
		)
		.expect("start minimum-duration regime")
		.activation;
	assert_eq!(
		regimes.stop(activation.as_str(), 109),
		Err(StopError::MinimumDuration { activation: activation.clone(), until_ms: 110 })
	);
	assert!(regimes.cancel(activation.as_str()));
	assert!(regimes.is_empty());
}

#[test]
fn revival_preserves_activation_state_and_committed_step_count() {
	let declaration = spec("revive", 0, Point::TurnEnd.set());
	let mut before = RegimeSet::new();
	let activation = before
		.start(Arc::clone(&declaration), handler("initial"), StartOptions {
			now_ms: 1,
			queue:  false,
		})
		.expect("start regime")
		.activation;
	assert_eq!(before.advance(activation.as_str(), 2), RegimeStepResult::Advanced {
		committed_steps: 1,
	});
	let records = before.records();
	let mut after = RegimeSet::new();
	let report = after.revive(records, |_| Some((Arc::clone(&declaration), handler("fresh"))));
	assert_eq!(report.resumed, vec![activation.clone()]);
	assert!(report.failed.is_empty());
	let resumed = after.records();
	assert_eq!(resumed[0].activation, activation);
	assert_eq!(resumed[0].state, "initial");
	assert_eq!(resumed[0].committed_steps, 1);
	assert_eq!(resumed[0].status, RegimeStatus::Active);
}

#[test]
fn state_schema_mismatch_fails_revival_without_an_active_alias() {
	let declaration = spec("schema-bump", 0, Point::Settle.set());
	let mut before = RegimeSet::new();
	before
		.start(Arc::clone(&declaration), handler("state"), StartOptions { now_ms: 1, queue: false })
		.expect("start regime");
	let records = before.records();
	let mut bumped = (*declaration).clone();
	bumped.family_rev = sf!("test@2");
	let mut after = RegimeSet::new();
	let report = after.revive(records, |_| Some((Arc::new(bumped.clone()), handler("state"))));
	assert!(report.resumed.is_empty());
	assert_eq!(report.failed.len(), 1);
	assert_eq!(report.failed[0].status, RegimeStatus::Failed);
	assert!(after.is_empty());
}

#[test]
fn journal_recovery_preserves_activation_records_and_resource_ownership() {
	let path = journal_path("regime-revive");
	let mut journal = journal(&path, "regime-revive");
	let declaration = owning_spec("durable", [Resource::Mode], None);
	let mut first = Arbiter::new();
	let activation = first
		.start(Arc::clone(&declaration), handler("durable-state"), &mut journal, StartOptions {
			now_ms: 2,
			queue:  false,
		})
		.expect("start durable regime")
		.activation;
	assert_eq!(
		first
			.advance(activation.as_str(), 3, &mut journal)
			.expect("record committed step"),
		RegimeStepResult::Advanced { committed_steps: 1 }
	);
	drop(first);

	let mut revived = Arbiter::new();
	let report = revived
		.recover(
			&mut journal,
			|id| (id == "durable").then(|| (Arc::clone(&declaration), handler("fresh"))),
			4,
		)
		.expect("revive regime set");
	assert_eq!(report.resumed, vec![activation.clone()]);
	let records = revived.regimes().records();
	assert_eq!(records[0].activation, activation);
	assert_eq!(records[0].state, "durable-state");
	assert_eq!(records[0].committed_steps, 1);
	assert_eq!(
		revived.regimes().resources().holder(&Resource::Mode),
		Some((records[0].activation.as_str(), 2))
	);
	drop(journal);
	fs::remove_file(path).expect("remove journal");
}

#[test]
fn stopped_activation_is_the_latest_durable_record() {
	let path = journal_path("regime-stop");
	let mut journal = journal(&path, "regime-stop");
	let declaration = spec("durable-stop", 0, PointSet::EMPTY);
	let mut arbiter = Arbiter::new();
	let activation = arbiter
		.start(declaration, handler("state"), &mut journal, StartOptions { now_ms: 1, queue: false })
		.expect("start regime")
		.activation;
	assert!(
		arbiter
			.stop(activation.as_str(), 2, &mut journal)
			.expect("stop regime")
	);
	assert!(arbiter.regimes().is_empty());
	let records = journal
		.recover_regime_records()
		.expect("recover lifecycle records");
	assert_eq!(records.len(), 1);
	assert_eq!(records[0].activation, activation);
	assert_eq!(records[0].status, RegimeStatus::Stopped);
	drop(journal);
	fs::remove_file(path).expect("remove journal");
}

#[test]
fn regime_facts_do_not_change_model_visible_journal_bytes() {
	let path = journal_path("regime-fact-golden");
	let mut journal = journal(&path, "regime-fact-golden");
	journal
		.append_optimistic(2, omp_agent::Item::default(), None)
		.expect("append scripted input");
	let before_events = journal.live_item_events().expect("project before fact");
	let before = serde_json::to_vec(&journal.items_at(&before_events).expect("read before items"))
		.expect("serialize before");
	journal
		.append_regime_fact(3, &RegimeFact {
			point:                  Point::Settle,
			turn_id:                Some(sf!("turn-1")),
			participants:           vec![sf!("activation-1")],
			control:                sf!("retry"),
			controlling_activation: Some(sf!("activation-1")),
			rewrite_count:          1,
			append_count:           2,
			rejection_count:        0,
			wait_count:             0,
		})
		.expect("append regime fact");
	let after_events = journal.live_item_events().expect("project after fact");
	let after = serde_json::to_vec(&journal.items_at(&after_events).expect("read after items"))
		.expect("serialize after");
	assert_eq!(after, before, "regime facts must not perturb model-visible journal bytes");
	drop(journal);
	fs::remove_file(path).expect("remove journal");
}
