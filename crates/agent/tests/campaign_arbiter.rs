use std::sync::Arc;

use bytes::Bytes;
use omp_agent::{
	Arbiter, CampaignMachine, CampaignScope, CampaignSpec, CampaignStack, CampaignWhen,
	ContextPatch, EngageOptions, ExhaustPolicy, HoldTicket, Ladder, LadderStep, Point, PointCx,
	PointSet, Reaction, SlotClaim, Verdict, WinnerKind,
};
use omp_core::{Str, sf};
use omp_storage::transcript::{Header, SessionId};
use proptest::prelude::*;

struct StaticMachine {
	verdict: Verdict,
	state:   Str,
}

impl CampaignMachine for StaticMachine {
	fn react(&mut self, _: Point, _: &PointCx<'_>) -> Reaction {
		Reaction::one(self.verdict.clone())
	}

	fn state(&self) -> Str {
		self.state.clone()
	}

	fn restore(&mut self, payload: &str) -> Result<(), omp_agent::CampaignStateError> {
		self.state = Str::new(payload);
		Ok(())
	}
}

fn spec(id: &str, precedence: i16, points: PointSet, ladder: Option<Ladder>) -> Arc<CampaignSpec> {
	Arc::new(CampaignSpec {
		id: Str::new(id),
		points,
		precedence,
		ladder,
		exhaust: ExhaustPolicy::Settle,
		scope: CampaignScope::Run,
		family_rev: sf!("test@1"),
		when: None,
		members: Arc::from([]),
		claims: Arc::from([]),
		binds: Arc::from([]),
		dwell_ms: None,
	})
}

fn machine(verdict: Verdict) -> Box<dyn CampaignMachine> {
	Box::new(StaticMachine { verdict, state: sf!("state") })
}

fn claim_spec(
	id: &str,
	claims: impl Into<Arc<[SlotClaim]>>,
	dwell_ms: Option<u64>,
) -> Arc<CampaignSpec> {
	let mut declaration = Arc::unwrap_or_clone(spec(id, 0, PointSet::EMPTY, None));
	declaration.scope = CampaignScope::Session;
	declaration.claims = claims.into();
	declaration.dwell_ms = dwell_ms;
	Arc::new(declaration)
}
use omp_agent::tool_choice::ToolChoiceQueue;

#[test]
fn declarative_when_is_evaluated_from_core_point_facts() {
	let mut stack = CampaignStack::new();
	let mut declaration = Arc::unwrap_or_clone(spec("automatic", 0, Point::Admission.set(), None));
	declaration.when = Some(CampaignWhen {
		point:           Point::Admission,
		invocation_id:   Some(sf!("call-1")),
		stream_contains: None,
		delivered:       Some(true),
	});
	let declaration = Arc::new(declaration);
	let missed = stack
		.engage_when(
			Arc::clone(&declaration),
			machine(Verdict::Pass),
			EngageOptions { now_ms: 1, queue: false },
			Point::Admission,
			&PointCx { invocation_id: Some("call-2"), delivered: true, ..PointCx::default() },
		)
		.unwrap();
	assert!(missed.is_none());
	let engaged = stack
		.engage_when(
			declaration,
			machine(Verdict::Pass),
			EngageOptions { now_ms: 2, queue: false },
			Point::Admission,
			&PointCx { invocation_id: Some("call-1"), delivered: true, ..PointCx::default() },
		)
		.unwrap();
	assert!(engaged.is_some());
}

proptest! {
	#[test]
	fn commutative_lanes_are_order_independent(keys in any::<[u8; 3]>()) {
		let mut left = CampaignStack::new();
		let mut right = CampaignStack::new();
		let reactions = [
			Verdict::Inject(vec![omp_agent::Item::default()]),
			Verdict::Deny { reason: sf!("guard") },
			Verdict::Continue,
		];
		for index in 0..3 {
			left
				.engage(
					spec(&format!("left-{index}"), 0, Point::Settle.set(), None),
					machine(reactions[index].clone()),
					EngageOptions { now_ms: 1, queue: false },
				)
				.unwrap();
		}
		let mut order = [0_usize, 1, 2];
		order.sort_by_key(|index| keys[*index]);
		for index in order {
			right
				.engage(
					spec(&format!("right-{index}-{}", right.len()), 0, Point::Settle.set(), None),
					machine(reactions[index].clone()),
					EngageOptions { now_ms: 1, queue: false },
				)
				.unwrap();
		}
		let cx = PointCx::default();
		let a = left.fold(Point::Settle, &cx, None);
		let b = right.fold(Point::Settle, &cx, None);
		prop_assert_eq!(a.winner, b.winner);
		prop_assert_eq!(a.injects.len(), b.injects.len());
		prop_assert_eq!(a.denials.len(), b.denials.len());
	}
}

#[test]
fn two_force_claims_are_serialized_by_the_existing_queue() {
	let mut book = CampaignStack::new();
	book
		.engage(
			spec("force-a", 10, Point::ToolChoice.set(), None),
			machine(Verdict::Force { tool: sf!("alpha") }),
			EngageOptions { now_ms: 1, queue: false },
		)
		.unwrap();
	book
		.engage(
			spec("force-b", 0, Point::ToolChoice.set(), None),
			machine(Verdict::Force { tool: sf!("beta") }),
			EngageOptions { now_ms: 1, queue: false },
		)
		.unwrap();
	let mut queue = ToolChoiceQueue::new();
	let fold = book.fold(Point::ToolChoice, &PointCx::default(), Some(&mut queue));
	assert_eq!(fold.winner, WinnerKind::Force);
	assert_eq!(queue.len(), 2);
	assert!(matches!(queue.claim_next(), Some(omp_inference::call::ToolChoice::Named(_))));
	assert!(queue.claim_next().is_none(), "one claim remains exclusively in flight");
	queue.resolve();
	assert!(matches!(queue.claim_next(), Some(omp_inference::call::ToolChoice::Named(_))));
}

#[test]
fn every_finite_ladder_terminates_at_its_static_bound() {
	for bound in 1..=16 {
		let ladder = Ladder::new(
			(0..bound)
				.map(|rung| LadderStep { label: sf!("rung-{rung}"), verdict: Verdict::Pass })
				.collect::<Vec<_>>(),
		);
		let mut book = CampaignStack::new();
		book
			.engage(
				spec("bounded", 0, Point::TurnEnd.set(), Some(ladder)),
				machine(Verdict::Pass),
				EngageOptions { now_ms: 1, queue: false },
			)
			.unwrap();
		for now_ms in 1..=bound + 1 {
			book.fold(
				Point::TurnEnd,
				&PointCx {
					now_ms: u64::try_from(now_ms).unwrap_or(u64::MAX),
					delivered: true,
					..PointCx::default()
				},
				None,
			);
		}
		assert!(book.is_empty(), "bound {bound} did not terminate");
	}
}

#[test]
fn six_campaign_collision_has_deterministic_winner_and_serialized_forces() {
	let mut arbiter = Arbiter::new();
	let point = Point::Settle.set().union(Point::ToolChoice.set());
	let lanes = [
		("force-a", 20, Verdict::Force { tool: sf!("alpha") }),
		("force-b", 10, Verdict::Force { tool: sf!("beta") }),
		(
			"hold",
			30,
			Verdict::Hold(HoldTicket {
				id:          sf!("approval"),
				deadline_ms: 1_000_000,
				reason:      sf!("review"),
			}),
		),
		("continue-a", 5, Verdict::Continue),
		("continue-b", 4, Verdict::Continue),
		("patch", 1, Verdict::Patch(ContextPatch(Bytes::from_static(b"patch")))),
	];
	let mut ids = Vec::new();
	for (id, precedence, verdict) in lanes {
		ids.push(
			arbiter
				.campaigns_mut()
				.engage(spec(id, precedence, point, None), machine(verdict), EngageOptions {
					now_ms: 1,
					queue:  false,
				})
				.unwrap(),
		);
	}
	let mut queue = ToolChoiceQueue::new();
	let fold = arbiter.fold(
		Point::Settle,
		&PointCx { turn_id: Some("turn-1"), ..PointCx::default() },
		Some(&mut queue),
	);
	assert_eq!(fold.campaign.winner, WinnerKind::Hold);
	assert_eq!(fold.campaign.lanes.len(), 6);
	assert_eq!(fold.campaign.patches.len(), 1);
	assert_eq!(queue.len(), 2, "losing force claims remain durable queue work");
	assert_eq!(fold.fact.point, Point::Settle);
	assert_eq!(fold.fact.turn_id.as_deref(), Some("turn-1"));
	assert_eq!(fold.fact.winner.as_str(), "hold");
	for receipt in ids {
		arbiter
			.campaigns_mut()
			.disengage(receipt.engagement.as_str(), 2)
			.unwrap();
	}
	let earned = arbiter.fold(Point::Settle, &PointCx::default(), None);
	assert_eq!(earned.campaign.winner, WinnerKind::Pass);
}

#[test]
fn revival_preserves_engagement_and_ladder_cursor() {
	let ladder = Ladder::new(vec![
		LadderStep { label: sf!("one"), verdict: Verdict::Pass },
		LadderStep { label: sf!("two"), verdict: Verdict::Pass },
		LadderStep { label: sf!("three"), verdict: Verdict::Pass },
	]);
	let declaration = spec("revive", 0, Point::TurnEnd.set(), Some(ladder));
	let mut before = CampaignStack::new();
	before
		.engage(Arc::clone(&declaration), machine(Verdict::Pass), EngageOptions {
			now_ms: 1,
			queue:  false,
		})
		.unwrap();
	before.fold(Point::TurnEnd, &PointCx { now_ms: 2, delivered: true, ..PointCx::default() }, None);
	let entries = before.entries();
	assert_eq!(entries[0].ladder_position, 1);
	let engagement = entries[0].engagement.clone();

	let mut after = CampaignStack::new();
	let report = after.revive(entries, |_| Some((Arc::clone(&declaration), machine(Verdict::Pass))));
	assert_eq!(report.resumed, vec![engagement.clone()]);
	assert!(report.exhausted.is_empty());
	let resumed = after.entries();
	assert_eq!(resumed[0].engagement, engagement);
	assert_eq!(resumed[0].ladder_position, 1);
}
#[test]
fn schema_bump_degrades_revived_state_to_exhausted() {
	let declaration = spec("schema-bump", 0, Point::Settle.set(), None);
	let mut before = CampaignStack::new();
	before
		.engage(Arc::clone(&declaration), machine(Verdict::Pass), EngageOptions {
			now_ms: 1,
			queue:  false,
		})
		.unwrap();
	let entries = before.entries();
	let mut bumped = (*declaration).clone();
	bumped.family_rev = sf!("test@2");

	let mut after = CampaignStack::new();
	let report = after.revive(entries, |_| Some((Arc::new(bumped.clone()), machine(Verdict::Pass))));
	assert!(report.resumed.is_empty());
	assert_eq!(report.exhausted.len(), 1);
	assert_eq!(report.exhausted[0].status, omp_agent::CampaignEntryStatus::Exhausted);
	assert!(after.is_empty());
}

#[test]
fn journal_kill_and_revive_preserves_ladder_rung() {
	let path = std::env::temp_dir().join(format!(
		"omp-agent-campaign-revive-{}-{}.jsonl",
		std::process::id(),
		omp_core::Ulid::generate()
	));
	let mut journal = omp_agent::Journal::create(&path, &Header {
		v:       4,
		id:      SessionId(sf!("campaign-revive")),
		created: 1,
		cwd:     std::env::temp_dir(),
	})
	.expect("create journal");
	let mut declaration = Arc::unwrap_or_clone(spec(
		"durable",
		0,
		Point::TurnEnd.set(),
		Some(Ladder::new(vec![
			LadderStep { label: sf!("one"), verdict: Verdict::Pass },
			LadderStep { label: sf!("two"), verdict: Verdict::Pass },
			LadderStep { label: sf!("three"), verdict: Verdict::Pass },
		])),
	));
	declaration.claims = Arc::from([SlotClaim::Mode]);
	let declaration = Arc::new(declaration);
	let mut first = Arbiter::new();
	let engagement = first
		.engage(Arc::clone(&declaration), machine(Verdict::Pass), &mut journal, EngageOptions {
			now_ms: 2,
			queue:  false,
		})
		.expect("engage durable campaign")
		.engagement;
	first
		.fold_and_record(
			Point::TurnEnd,
			&PointCx { turn_id: Some("turn-1"), now_ms: 3, delivered: true, ..PointCx::default() },
			None,
			&mut journal,
		)
		.expect("checkpoint first rung");
	drop(first);

	let mut revived = Arbiter::new();
	let report = revived
		.recover(
			&mut journal,
			|id| (id == "durable").then(|| (Arc::clone(&declaration), machine(Verdict::Pass))),
			4,
		)
		.expect("revive campaign stack");
	assert_eq!(report.resumed, vec![engagement.clone()]);
	let entries = revived.campaigns().entries();
	assert_eq!(entries[0].engagement, engagement);
	assert_eq!(entries[0].ladder_position, 1);
	assert_eq!(revived.campaigns().slots().holder(&SlotClaim::Mode), Some((engagement.as_str(), 2)),);
	drop(journal);
	std::fs::remove_file(path).expect("remove journal");
}

#[test]
fn slot_claims_and_binding_stacks_release_with_subtrees() {
	let mut slots = omp_agent::SlotRegistry::default();
	assert_eq!(
		slots.claim(SlotClaim::Worktree, sf!("parent"), 7, false),
		Ok(omp_agent::ClaimOutcome::Granted)
	);
	assert!(matches!(
		slots.claim(SlotClaim::Worktree, sf!("child"), 8, true),
		Ok(omp_agent::ClaimOutcome::Queued { holder, since })
			if holder == "parent" && since == 7
	));
	let granted = slots.release("parent");
	assert_eq!(granted, vec![(SlotClaim::Worktree, sf!("child"))]);
	assert_eq!(slots.owner(&SlotClaim::Worktree), Some("child"));
}

#[test]
fn canonical_slot_table_rejects_unknown_declarations() {
	assert_eq!(omp_agent::SLOT_TABLE.map(|slot| slot.name), [
		"tool_choice",
		"worktree",
		"director",
		"editor-surface",
		"batch-execution",
		"mode",
	],);
	let declaration = claim_spec("unknown", [SlotClaim::Named(sf!("missing"))], None);
	assert_eq!(
		CampaignStack::new().declare(&declaration),
		Err(omp_agent::DeclareError::UnknownSlot { slot: SlotClaim::Named(sf!("missing")) }),
	);
}

#[test]
fn built_in_regimes_claim_the_mode_slot() {
	let claims = |spec: CampaignSpec| spec.claims.iter().cloned().collect::<Vec<_>>();
	assert_eq!(claims(omp_agent::plan_regime_spec()), vec![SlotClaim::Mode, SlotClaim::Worktree],);
	assert_eq!(claims(omp_agent::vibe_regime_spec()), vec![SlotClaim::Mode, SlotClaim::Director],);
	assert_eq!(claims(omp_agent::goal_regime_spec()), vec![SlotClaim::Mode]);
	assert_eq!(claims(omp_agent::autoresearch_regime_spec()), vec![
		SlotClaim::Mode,
		SlotClaim::Worktree
	],);
}

#[test]
fn mode_claim_denials_are_structured_and_queue_grants_on_exit() {
	let declaration = claim_spec("mode", [SlotClaim::Mode], None);
	let mut stack = CampaignStack::new();
	let holder = stack
		.engage(Arc::clone(&declaration), machine(Verdict::Pass), EngageOptions {
			now_ms: 41,
			queue:  false,
		})
		.unwrap()
		.engagement;
	for since in [42, 43] {
		let error = stack
			.engage(Arc::clone(&declaration), machine(Verdict::Pass), EngageOptions {
				now_ms: since,
				queue:  false,
			})
			.unwrap_err();
		assert_eq!(error, omp_agent::EngageError::Claim {
			slot:    SlotClaim::Mode,
			outcome: omp_agent::ClaimOutcome::Denied { holder: holder.clone(), since: 41 },
		},);
	}
	let queued = stack
		.engage(declaration, machine(Verdict::Pass), EngageOptions { now_ms: 44, queue: true })
		.unwrap();
	assert_eq!(queued.outcome, omp_agent::ClaimOutcome::Queued {
		holder: holder.clone(),
		since:  41,
	},);
	assert_eq!(stack.slots().queue_depth(&SlotClaim::Mode), 1);
	stack.disengage(holder.as_str(), 45).unwrap();
	assert_eq!(stack.slots().owner(&SlotClaim::Mode), Some(queued.engagement.as_str()));
	assert_eq!(stack.slots().queue_depth(&SlotClaim::Mode), 0);
}

#[test]
fn dwell_refuses_non_cut_exit() {
	let declaration = claim_spec("dwelling", [SlotClaim::Mode], Some(100));
	let mut stack = CampaignStack::new();
	let engagement = stack
		.engage(declaration, machine(Verdict::Pass), EngageOptions { now_ms: 10, queue: false })
		.unwrap()
		.engagement;
	assert_eq!(
		stack.disengage(engagement.as_str(), 109),
		Err(omp_agent::DisengageError::Dwelling { engagement: engagement.clone(), until_ms: 110 }),
	);
	assert!(stack.cut(engagement.as_str()));
}
#[test]
fn arbiter_fact_keeps_the_model_journal_byte_identical() {
	let path = std::env::temp_dir().join(format!(
		"omp-agent-arbiter-golden-{}-{}.jsonl",
		std::process::id(),
		omp_core::Ulid::generate()
	));
	let mut journal = omp_agent::Journal::create(&path, &Header {
		v:       4,
		id:      SessionId(sf!("arbiter-golden")),
		created: 1,
		cwd:     std::env::temp_dir(),
	})
	.expect("create journal");
	journal
		.append_optimistic(2, omp_agent::Item::default(), None)
		.expect("append scripted input");
	let before_events = journal.live_item_events().expect("project before fold");
	let before = serde_json::to_vec(&journal.items_at(&before_events).expect("read before items"))
		.expect("serialize before");

	let mut arbiter = Arbiter::new();
	let mut facts = Vec::new();
	for (turn_index, turn_id) in ["script-turn-1", "script-turn-2"].into_iter().enumerate() {
		for point in Point::ALL {
			let fold = arbiter
				.fold_and_record(
					point,
					&PointCx {
						turn_id: Some(turn_id),
						now_ms: 3 + u64::try_from(turn_index).unwrap_or(u64::MAX),
						..PointCx::default()
					},
					None,
					&mut journal,
				)
				.expect("record scripted fold");
			assert_eq!(fold.fact.lanes, Vec::<Str>::new());
			facts.push(arbiter.try_fact().expect("fold emits telemetry fact"));
		}
	}
	assert_eq!(facts.len(), 18);
	assert!(facts.iter().all(|fact| fact.winner == "pass"));
	assert_eq!(
		facts.iter().map(|fact| fact.point).collect::<Vec<_>>(),
		Point::ALL.into_iter().chain(Point::ALL).collect::<Vec<_>>(),
	);

	let after_events = journal.live_item_events().expect("project after fold");
	let after = serde_json::to_vec(&journal.items_at(&after_events).expect("read after items"))
		.expect("serialize after");
	assert_eq!(after, before, "arbiter facts must not perturb model-visible journal bytes");
	drop(journal);
	std::fs::remove_file(path).expect("remove journal");
}
