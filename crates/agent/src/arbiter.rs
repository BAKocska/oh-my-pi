//! Named decision-point seams for the closed agent loop.

use std::{array, mem, sync};

use flume::Receiver;
use omp_core::{Point, PointSet, Str};
use serde::{Deserialize, Serialize};

use crate::{
	Journal, JournalError,
	campaign::{
		CampaignEntry, CampaignEntryStatus, CampaignFold, CampaignMachine, CampaignSpec,
		CampaignStack, CampaignStateError, CampaignStepResult, DisengageError, EngageError,
		EngageOptions, EngageReceipt, Reaction, RevivalReport, absorb_lane,
	},
	tool_choice::ToolChoiceQueue,
};

/// Borrowed pending-preview metadata exposed to campaign lanes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingInvokerCx<'a> {
	/// Unique staged-preview identity.
	pub id:          &'a str,
	/// Tool that staged the preview.
	pub source_tool: &'a str,
}

/// Immutable facts available to lanes at one decision point.
#[derive(Clone, Copy, Debug, Default)]
pub struct PointCx<'a> {
	/// Current durable turn identity, when a turn exists.
	pub turn_id:         Option<&'a str>,
	/// Current invocation identity at ADMISSION/BATCH.
	pub invocation_id:   Option<&'a str>,
	/// Current streamed UTF-8 fragment at STREAM.
	pub stream_delta:    Option<&'a str>,
	/// Current epoch milliseconds.
	pub now_ms:          u64,
	/// Whether the preceding operation delivered an observable effect.
	pub delivered:       bool,
	/// Most recently registered staged-preview invoker.
	pub pending_invoker: Option<PendingInvokerCx<'a>>,
}

/// Journal and telemetry representation of one fold.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FoldFact {
	/// Decision point folded.
	pub point:           Point,
	/// Durable turn identity, when the fold belongs to a turn.
	pub turn_id:         Option<Str>,
	/// Participating engagement identities in deterministic order.
	pub lanes:           Vec<Str>,
	/// Winning transition class.
	pub winner:          Str,
	/// Exclusive winning engagement, if any.
	pub winner_lane:     Option<Str>,
	/// Number of accumulated context patches.
	pub patch_count:     u32,
	/// Number of accumulated injections.
	pub injection_count: u32,
	/// Number of unioned denial reasons.
	pub denial_count:    u32,
	/// Number of unresolved holds.
	pub hold_count:      u32,
}

impl FoldFact {
	fn from_campaign(point: Point, cx: &PointCx<'_>, fold: &CampaignFold) -> Self {
		Self {
			point,
			turn_id: cx.turn_id.map(Str::new),
			lanes: fold.lanes.clone(),
			winner: Str::new(<&'static str>::from(fold.winner)),
			winner_lane: fold.winner_lane.clone(),
			patch_count: u32::try_from(fold.patches.len()).unwrap_or(u32::MAX),
			injection_count: u32::try_from(fold.injects.len()).unwrap_or(u32::MAX),
			denial_count: u32::try_from(fold.denials.len()).unwrap_or(u32::MAX),
			hold_count: u32::try_from(fold.holds.len()).unwrap_or(u32::MAX),
		}
	}
}

/// Result of one named arbiter fold.
#[derive(Clone, Debug)]
pub struct Fold {
	/// Campaign arbiter output consumed by the loop call site.
	pub campaign: CampaignFold,
	/// Durable forensic fact emitted for the fold.
	pub fact:     FoldFact,
}

struct RegisteredLane {
	id:         Str,
	precedence: i16,
	lane:       sync::Arc<dyn Lane>,
}

/// One behavior-preserving core lane at fixed decision points.
pub trait Lane: Send + Sync + 'static {
	/// Stable forensic lane identity.
	fn id(&self) -> &str;

	/// Points at which this lane is eligible.
	fn points(&self) -> PointSet;

	/// Higher values fold first.
	fn precedence(&self) -> i16 {
		0
	}

	/// Produces one atomic reaction from point-local facts.
	fn react(&self, point: Point, cx: &PointCx<'_>) -> Reaction;
}

/// Arbiter owner for all point subscriptions and fold observations.
pub struct Arbiter {
	campaigns:          CampaignStack,
	ttsr_campaign:      stream::TtsrCampaign,
	empty_output_retry: settle::EmptyOutputRetry,
	checkpoint_notice:  context::CheckpointNotice,
	retry_chain:        Option<settle::RetryChainCampaign>,
	lanes:              [Vec<RegisteredLane>; 9],
	pending_facts:      Vec<FoldFact>,
	subscribed:         PointSet,
	fact_tx:            flume::Sender<FoldFact>,
	fact_rx:            Receiver<FoldFact>,
}

impl Default for Arbiter {
	fn default() -> Self {
		let (fact_tx, fact_rx) = flume::unbounded();
		Self {
			campaigns: CampaignStack::new(),
			ttsr_campaign: stream::TtsrCampaign::default(),
			empty_output_retry: settle::EmptyOutputRetry::default(),
			checkpoint_notice: context::CheckpointNotice::default(),
			retry_chain: None,
			lanes: array::from_fn(|_| Vec::new()),
			pending_facts: Vec::new(),
			subscribed: PointSet::EMPTY,
			fact_tx,
			fact_rx,
		}
	}
}

impl Arbiter {
	/// Creates an empty arbiter.
	pub fn new() -> Self {
		Self::default()
	}

	/// Returns the durable campaign owner registered at this arbiter.
	pub const fn campaigns(&self) -> &CampaignStack {
		&self.campaigns
	}

	/// Returns mutable access to the durable campaign owner.
	pub const fn campaigns_mut(&mut self) -> &mut CampaignStack {
		&mut self.campaigns
	}

	pub(crate) const fn ttsr_campaign_mut(&mut self) -> &mut stream::TtsrCampaign {
		&mut self.ttsr_campaign
	}

	pub(crate) const fn checkpoint_notice_mut(&mut self) -> &mut context::CheckpointNotice {
		&mut self.checkpoint_notice
	}

	pub(crate) fn restore_empty_output_retry(&mut self, spent: u8) {
		self.empty_output_retry = settle::EmptyOutputRetry::recovered(spent);
	}

	pub(crate) const fn reset_empty_output_retry(&mut self) {
		self.empty_output_retry.reset();
	}

	pub(crate) const fn empty_output_retry_spent(&self) -> u8 {
		self.empty_output_retry.spent()
	}

	pub(crate) fn react_empty_output_retry(&mut self, cx: &PointCx<'_>) -> Reaction {
		self.empty_output_retry.react(Point::Settle, cx)
	}

	pub(crate) fn set_retry_chain(&mut self, routes: Vec<Str>) {
		if let Some(chain) = self.retry_chain.as_mut()
			&& chain.routes() == routes.as_slice()
		{
			chain.retry_now();
		} else {
			self.retry_chain = Some(settle::RetryChainCampaign::new(routes));
		}
	}

	/// Engages and journals one lane as a single lifecycle operation.
	pub fn engage(
		&mut self,
		spec: sync::Arc<CampaignSpec>,
		machine: Box<dyn CampaignMachine>,
		journal: &mut Journal,
		options: EngageOptions,
	) -> Result<EngageReceipt, ArbiterError> {
		let receipt = self.campaigns.engage(spec, machine, options)?;
		let entry = self
			.campaigns
			.entries()
			.into_iter()
			.find(|entry| entry.engagement == receipt.engagement)
			.expect("new engagement has one durable record");
		if let Err(error) = journal.append_campaign_entry(options.now_ms, &entry) {
			self.campaigns.cut(receipt.engagement.as_str());
			return Err(ArbiterError::Journal(error));
		}
		Ok(receipt)
	}

	/// Disengages one lane after dwell and journals its terminal lifecycle.
	pub fn disengage(
		&mut self,
		engagement: &str,
		now_ms: u64,
		journal: &mut Journal,
	) -> Result<bool, ArbiterError> {
		let Some(mut terminal) = self
			.campaigns
			.entries()
			.into_iter()
			.find(|entry| entry.engagement == engagement)
		else {
			return Ok(false);
		};
		if !self.campaigns.check_disengage(engagement, now_ms)? {
			return Ok(false);
		}
		terminal.status = CampaignEntryStatus::Disengaged;
		journal.append_campaign_entry(now_ms, &terminal)?;
		let removed = self.campaigns.cut(engagement);
		if removed {
			self.checkpoint(journal, now_ms)?;
		}
		Ok(removed)
	}

	/// Updates one machine state and journals the resulting campaign entry.
	pub fn update_state(
		&mut self,
		engagement: &str,
		payload: &[u8],
		now_ms: u64,
		journal: &mut Journal,
	) -> Result<CampaignEntry, ArbiterError> {
		let entry = self.campaigns.update_state(engagement, payload)?;
		journal.append_campaign_entry(now_ms, &entry)?;
		Ok(entry)
	}

	/// Explicitly steps one campaign and journals its rung or exhaustion.
	pub fn step(
		&mut self,
		engagement: &str,
		now_ms: u64,
		journal: &mut Journal,
	) -> Result<CampaignStepResult, JournalError> {
		let mut terminal = self
			.campaigns
			.entries()
			.into_iter()
			.find(|entry| entry.engagement == engagement);
		let result = self.campaigns.step_result(engagement, now_ms);
		if matches!(result, CampaignStepResult::Terminal { .. })
			&& let Some(entry) = terminal.as_mut()
		{
			entry.status = CampaignEntryStatus::Exhausted;
			journal.append_campaign_entry(now_ms, entry)?;
		}
		self.checkpoint(journal, now_ms)?;
		Ok(result)
	}

	/// Cuts one campaign without dwell and journals its terminal lifecycle.
	pub fn cut(
		&mut self,
		engagement: &str,
		now_ms: u64,
		journal: &mut Journal,
	) -> Result<bool, JournalError> {
		let Some(mut terminal) = self
			.campaigns
			.entries()
			.into_iter()
			.find(|entry| entry.engagement == engagement)
		else {
			return Ok(false);
		};
		terminal.status = CampaignEntryStatus::Disengaged;
		journal.append_campaign_entry(now_ms, &terminal)?;
		let removed = self.campaigns.cut(engagement);
		self.checkpoint(journal, now_ms)?;
		Ok(removed)
	}

	/// Journals the current cursor and state of every active lane.
	pub fn checkpoint(&self, journal: &mut Journal, now_ms: u64) -> Result<(), JournalError> {
		for entry in self.campaigns.entries() {
			journal.append_campaign_entry(now_ms, &entry)?;
		}
		Ok(())
	}

	/// Revives journaled lanes and journals unloadable-state exhaustion.
	pub fn recover<F>(
		&mut self,
		journal: &mut Journal,
		mut resolve: F,
		now_ms: u64,
	) -> Result<RevivalReport, JournalError>
	where
		F: FnMut(&str) -> Option<(sync::Arc<CampaignSpec>, Box<dyn CampaignMachine>)>,
	{
		let entries = journal.recover_campaign_entries()?;
		let report = self.campaigns.revive(entries, |id| resolve(id));
		for entry in &report.exhausted {
			journal.append_campaign_entry(now_ms, entry)?;
		}
		Ok(report)
	}

	/// Registers point bits. Unregistered points still emit a degenerate fold
	/// fact.
	pub fn register_points(&mut self, points: PointSet) {
		self.subscribed = self.subscribed.union(points);
	}

	/// Registers one lane into every subscribed per-point list.
	pub fn register(&mut self, lane: sync::Arc<dyn Lane>) {
		let points = lane.points();
		let id = Str::new(lane.id());
		let precedence = lane.precedence();
		for point in Point::ALL {
			if !points.contains(point) {
				continue;
			}
			let list = &mut self.lanes[usize::from(point.ordinal())];
			list.push(RegisteredLane { id: id.clone(), precedence, lane: sync::Arc::clone(&lane) });
			list.sort_by(|left, right| {
				right
					.precedence
					.cmp(&left.precedence)
					.then_with(|| left.id.cmp(&right.id))
			});
		}
		self.register_points(points);
	}

	/// Returns the union of registered point subscriptions.
	pub const fn subscriptions(&self) -> PointSet {
		self.subscribed
	}

	/// Purely folds one point and emits the same fact to the telemetry stream.
	pub fn fold(
		&mut self,
		point: Point,
		cx: &PointCx<'_>,
		tool_choices: Option<&mut ToolChoiceQueue>,
	) -> Fold {
		let campaign = self.campaigns.fold(point, cx, tool_choices);
		let mut campaign = campaign;
		for lane in &self.lanes[usize::from(point.ordinal())] {
			absorb_lane(&mut campaign, lane.id.clone(), lane.lane.react(point, cx));
		}
		if point == Point::Stream && self.ttsr_campaign.has_pending() {
			absorb_lane(&mut campaign, Str::new_static("ttsr"), self.ttsr_campaign.react(point, cx));
		}
		if point == Point::Context && self.checkpoint_notice.is_active() {
			absorb_lane(
				&mut campaign,
				Str::new_static("checkpoint"),
				self.checkpoint_notice.react(point, cx),
			);
		}
		if matches!(point, Point::PreModel | Point::Stream)
			&& self
				.retry_chain
				.as_ref()
				.is_some_and(|chain| chain.is_active())
			&& let Some(chain) = self.retry_chain.as_mut()
		{
			absorb_lane(&mut campaign, Str::new_static("retry-chain"), chain.react(point, cx));
		}
		let fact = FoldFact::from_campaign(point, cx, &campaign);
		let _ = self.fact_tx.send(fact.clone());
		Fold { campaign, fact }
	}

	/// Folds and atomically appends the forensic fact to the session journal.
	pub fn fold_and_record(
		&mut self,
		point: Point,
		cx: &PointCx<'_>,
		tool_choices: Option<&mut ToolChoiceQueue>,
		journal: &mut Journal,
	) -> Result<Fold, JournalError> {
		let fold = self.fold(point, cx, tool_choices);
		if journal.pending_turn().is_some() {
			self.pending_facts.push(fold.fact.clone());
		} else {
			self.flush(journal, cx.now_ms)?;
			journal.append_arbiter_fold(cx.now_ms, &fold.fact)?;
			self.checkpoint(journal, cx.now_ms)?;
		}
		Ok(fold)
	}

	/// Flushes facts buffered while a durable turn was pending.
	pub fn flush(&mut self, journal: &mut Journal, now_ms: u64) -> Result<(), JournalError> {
		for fact in mem::take(&mut self.pending_facts) {
			journal.append_arbiter_fold(now_ms, &fact)?;
		}
		self.checkpoint(journal, now_ms)
	}

	/// Drains one telemetry fold fact without blocking.
	pub fn try_fact(&self) -> Option<FoldFact> {
		self.fact_rx.try_recv().ok()
	}
}
/// Failure while engaging a journaled arbiter lane.
#[derive(Debug, thiserror::Error)]
pub enum ArbiterError {
	/// The campaign declaration or claim set rejected engagement.
	#[error(transparent)]
	Engage(#[from] EngageError),
	/// The engagement's dwell policy rejected exit.
	#[error(transparent)]
	Disengage(#[from] DisengageError),
	/// The machine rejected a live state update.
	#[error(transparent)]
	State(#[from] CampaignStateError),
	/// The durable lifecycle append failed.
	#[error(transparent)]
	Journal(#[from] JournalError),
}

/// CONTEXT-point core lanes.
pub(crate) mod context {
	use omp_core::{Point, Str};
	use omp_proto::thread::v1::{self as thread, Item, item};
	use omp_storage::transcript::{Entry, Kind};
	use serde::Deserialize;

	use super::PointCx;
	use crate::{
		Journal, JournalError,
		campaign::{CampaignMachine, CampaignStateError, Reaction, Verdict},
		journal_kinds::{CHECKPOINT_KIND, REWIND_REPORT_KIND},
		r#loop::now_ms,
	};

	/// Session-scoped checkpoint notice campaign.
	#[derive(Default)]
	pub(crate) struct CheckpointNotice {
		active: bool,
	}

	impl CheckpointNotice {
		pub(crate) const fn set_active(&mut self, active: bool) {
			self.active = active;
		}

		pub(crate) const fn is_active(&self) -> bool {
			self.active
		}
	}

	impl CampaignMachine for CheckpointNotice {
		fn react(&mut self, point: Point, _: &PointCx<'_>) -> Reaction {
			if point == Point::Context && self.active {
				Reaction::one(Verdict::Inject(vec![crate::prompt::checkpoint_active_reminder()]))
			} else if self.active {
				Reaction::one(Verdict::Pass)
			} else {
				Reaction::one(Verdict::Done)
			}
		}

		fn state(&self) -> Str {
			Str::new_static(if self.active { "active" } else { "inactive" })
		}

		fn restore(&mut self, payload: &str) -> Result<(), CampaignStateError> {
			self.active = match payload {
				"active" => true,
				"inactive" => false,
				_ => return Err(CampaignStateError::InvalidPayload),
			};
			Ok(())
		}
	}

	/// Active durable checkpoint notice.
	#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
	pub(crate) struct ActiveCheckpoint {
		pub(crate) opaque_token: Str,
		pub(crate) event:        u64,
		pub(crate) goal:         Str,
		pub(crate) started_at:   u64,
	}

	/// Most recently completed checkpoint.
	#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
	pub(crate) struct CompletedCheckpoint {
		pub(crate) opaque_token: Str,
		pub(crate) goal:         Str,
		pub(crate) report:       Str,
		pub(crate) started_at:   u64,
		pub(crate) rewound_at:   u64,
	}

	/// Projection recovered from checkpoint journal facts.
	#[derive(Debug, Default)]
	pub(crate) struct CheckpointState {
		pub(crate) active:           Option<ActiveCheckpoint>,
		pub(crate) last_completed:   Option<CompletedCheckpoint>,
		pub(crate) rewind_scheduled: bool,
	}

	pub(crate) fn recover_checkpoint_state(
		journal: &Journal,
	) -> Result<CheckpointState, JournalError> {
		#[derive(Deserialize)]
		struct CheckpointRecord {
			token:      Str,
			goal:       Str,
			started_at: u64,
		}
		#[derive(Deserialize)]
		struct RewindRecord {
			token:      Str,
			goal:       Str,
			report:     Str,
			started_at: u64,
			rewound_at: u64,
		}
		let log = journal.load()?;
		let mut state = CheckpointState::default();
		for index in log.as_ref().iter() {
			let Some(Entry::Ok(event)) = log.get(index) else {
				continue;
			};
			let Kind::Custom(custom) = &event.kind else {
				continue;
			};
			match custom.kind() {
				CHECKPOINT_KIND => {
					let Some(data) = custom.data() else { continue };
					let Ok(record) = serde_json::from_str::<CheckpointRecord>(data.get()) else {
						continue;
					};
					state.active = Some(ActiveCheckpoint {
						opaque_token: record.token,
						event:        index,
						goal:         record.goal,
						started_at:   record.started_at,
					});
					state.rewind_scheduled = false;
				},
				REWIND_REPORT_KIND => {
					let Some(data) = custom.data() else { continue };
					let Ok(record) = serde_json::from_str::<RewindRecord>(data.get()) else {
						continue;
					};
					if state
						.active
						.as_ref()
						.is_some_and(|active| active.opaque_token == record.token)
					{
						state.active = None;
					}
					state.last_completed = Some(CompletedCheckpoint {
						opaque_token: record.token,
						goal:         record.goal,
						report:       record.report,
						started_at:   record.started_at,
						rewound_at:   record.rewound_at,
					});
					state.rewind_scheduled = false;
				},
				_ => {},
			}
		}
		Ok(state)
	}

	pub(crate) fn compaction_instruction(text: Str) -> Item {
		message(thread::Role::User, text.to_string())
	}

	pub(crate) fn rewind_background_warning(count: usize) -> Item {
		message(
			thread::Role::System,
			format!(
				"<system-injection>\nRewind left {count} background job(s) running; their settlements \
				 may still arrive. Cancel them explicitly if they are no longer \
				 wanted.\n</system-injection>"
			),
		)
	}

	fn message(role: thread::Role, text: String) -> Item {
		Item {
			created_at_ms: now_ms(),
			kind: Some(item::Kind::Message(thread::Message {
				role:  role as i32,
				parts: vec![thread::Part { kind: Some(thread::part::Kind::Text(text)) }],
			})),
			..Default::default()
		}
	}
}

/// SETTLE-point core lanes.
pub(crate) mod settle {
	use bytes::Bytes;
	use omp_core::{Point, Str};
	use omp_proto::{
		inference::v1 as pb,
		thread::v1::{self as thread, Item, item},
	};

	use crate::{
		campaign::{BindSlot, CampaignMachine, CampaignStateError, Reaction, ScopedBinding, Verdict},
		continuation::{Continuation, ContinuationPolicy, ContinuationSource, LoopSignal},
		r#loop::now_ms,
		turn::empty_stop,
	};

	/// Behavior-preserving empty-output retry counter and finite cap.
	#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
	pub(crate) struct EmptyOutputRetry {
		spent: u8,
	}
	use super::PointCx;
	use crate::prompt_assets::render_empty_stop_retry;

	impl EmptyOutputRetry {
		pub(crate) const CAP: u8 = 3;

		pub(crate) const fn recovered(spent: u8) -> Self {
			Self { spent }
		}

		pub(crate) const fn spent(self) -> u8 {
			self.spent
		}

		pub(crate) const fn can_retry(self) -> bool {
			self.spent < Self::CAP
		}

		pub(crate) fn step(&mut self) -> Option<Item> {
			if !self.can_retry() {
				return None;
			}
			self.spent = self.spent.saturating_add(1);
			Some(Self::item(self.spent))
		}

		pub(crate) const fn reset(&mut self) {
			self.spent = 0;
		}
	}
	impl CampaignMachine for EmptyOutputRetry {
		fn react(&mut self, point: Point, _: &PointCx<'_>) -> Reaction {
			if point != Point::Settle {
				return Reaction::one(Verdict::Pass);
			}
			let Some(item) = self.step() else {
				return Reaction::one(Verdict::Fault {
					detail: Str::new_static(
						"Assistant returned no final output after retry cap; try switching models",
					),
				});
			};
			Reaction {
				verdicts: vec![
					Verdict::Patch(crate::ContextPatch(Bytes::from_static(b"drop:turn-tail"))),
					Verdict::Inject(vec![item]),
					Verdict::Continue,
				],
			}
		}

		fn state(&self) -> Str {
			Str::from(self.spent.to_string())
		}

		fn restore(&mut self, payload: &str) -> Result<(), CampaignStateError> {
			self.spent = payload
				.parse()
				.map_err(|_| CampaignStateError::InvalidPayload)?;
			Ok(())
		}
	}

	/// Provider failover campaign that binds each route in chain order.
	pub(crate) struct RetryChainCampaign {
		routes:         Vec<Str>,
		cursor:         usize,
		cooldown_ms:    u64,
		cooldown_until: Option<u64>,
	}

	impl RetryChainCampaign {
		pub(crate) fn new(routes: Vec<Str>) -> Self {
			Self { routes, cursor: 0, cooldown_ms: 1_000, cooldown_until: None }
		}

		pub(crate) fn routes(&self) -> &[Str] {
			&self.routes
		}

		pub(crate) const fn retry_now(&mut self) {
			self.cooldown_until = None;
		}

		pub(crate) fn is_active(&self) -> bool {
			self.cursor < self.routes.len()
		}
	}

	impl CampaignMachine for RetryChainCampaign {
		fn react(&mut self, point: Point, cx: &PointCx<'_>) -> Reaction {
			if !matches!(point, Point::PreModel | Point::Stream) {
				return Reaction::one(Verdict::Pass);
			}
			if self.cooldown_until.is_some_and(|until| cx.now_ms < until) {
				return Reaction::one(Verdict::Pass);
			}
			self.cooldown_until = None;
			let Some(route) = self.routes.get(self.cursor).cloned() else {
				return Reaction::one(Verdict::Done);
			};
			self.cursor = self.cursor.saturating_add(1);
			self.cooldown_until = Some(cx.now_ms.saturating_add(self.cooldown_ms));
			Reaction {
				verdicts: vec![
					Verdict::Patch(crate::ContextPatch(bytes::Bytes::from_static(b"provider-failover"))),
					Verdict::Bind(ScopedBinding { slot: BindSlot::ModelRoute, value: route }),
				],
			}
		}

		fn state(&self) -> Str {
			Str::from(format!("{}:{}", self.cursor, self.cooldown_until.unwrap_or(0)))
		}

		fn restore(&mut self, payload: &str) -> Result<(), CampaignStateError> {
			let (cursor, until) = payload
				.split_once(':')
				.ok_or(CampaignStateError::InvalidPayload)?;
			self.cursor = cursor
				.parse()
				.map_err(|_| CampaignStateError::InvalidPayload)?;
			self.cooldown_until = Some(
				until
					.parse()
					.map_err(|_| CampaignStateError::InvalidPayload)?,
			);
			Ok(())
		}
	}

	impl EmptyOutputRetry {
		pub(crate) fn item(attempt: u8) -> Item {
			let mut text = String::new();
			render_empty_stop_retry(&mut text, usize::from(attempt), usize::from(Self::CAP));
			message(text)
		}

		pub(crate) fn cap_detail(error: &pb::TurnError) -> String {
			const DETAIL: &str =
				"Assistant returned no final output after retry cap; try switching models";
			let diagnostic = error
				.diagnostics
				.iter()
				.rev()
				.find(|diagnostic| diagnostic.code.starts_with("empty_stop."));
			match diagnostic.map(|diagnostic| (diagnostic.code.as_str(), diagnostic.detail.as_str())) {
				Some((empty_stop::BILLED_OUTPUT, billed)) => {
					let tokens: u64 = billed.parse().unwrap_or(0);
					let plural = if tokens == 1 { "" } else { "s" };
					format!(
						"Assistant returned an empty stop after retry cap, but the provider billed \
						 {tokens} output token{plural} for it; content was generated and then dropped \
						 before delivery, which usually points to a provider-side content filter or a \
						 lossy API translation rather than a context problem"
					)
				},
				Some((empty_stop::EMPTY, _)) => "Assistant returned an empty stop after retry cap; \
				                                 try switching models or removing large attachments \
				                                 from recent context"
					.to_owned(),
				_ => DETAIL.to_owned(),
			}
		}
	}

	fn message(text: String) -> Item {
		Item {
			seq:           0,
			created_at_ms: now_ms(),
			kind:          Some(item::Kind::Message(thread::Message {
				role:  thread::Role::User as i32,
				parts: vec![thread::Part { kind: Some(thread::part::Kind::Text(text)) }],
			})),
			props:         None,
		}
	}

	/// Core-source lane evaluated before the AgentSettled hook lane.
	pub(crate) fn source_candidate(
		source: Option<&dyn ContinuationSource>,
		signal: &LoopSignal,
		now_ms: u64,
	) -> (Continuation, ContinuationPolicy) {
		source.map_or((Continuation::Settle, ContinuationPolicy::default()), |source| {
			source.decide(signal, now_ms)
		})
	}
}

/// STREAM-point core lane retaining all TTSR splice state.
pub(crate) mod stream {
	use std::fmt::Write as _;

	use omp_core::{Point, Str, sf};
	use omp_proto::thread::v1::{self as thread, Item, item};

	use crate::{TtsrMatch, TtsrMatchContext, TtsrRegistry, TtsrSource, r#loop::now_ms};

	#[derive(Clone)]
	pub(crate) struct TtsrCampaignCut {
		pub(crate) matches: Vec<TtsrMatch>,
		pub(crate) source:  TtsrSource,
	}

	struct DeferredTtsr {
		matched: TtsrMatch,
		source:  TtsrSource,
	}

	pub(crate) struct TtsrStreamPart {
		source:     TtsrSource,
		tool_name:  Option<Str>,
		stream_key: Str,
		arguments:  String,
	}

	impl TtsrStreamPart {
		pub(crate) fn new(index: u32, source: TtsrSource, tool_name: Option<&str>) -> Self {
			Self {
				source,
				tool_name: tool_name.map(Str::new),
				stream_key: sf!("part:{}:{}", index, source),
				arguments: String::new(),
			}
		}
	}

	#[derive(Default)]
	pub(crate) struct TtsrCampaign {
		registry:    Option<TtsrRegistry>,
		deferred:    Vec<DeferredTtsr>,
		pending_cut: Option<TtsrCampaignCut>,
	}
	use std::mem;

	use omp_inference::recovery::repetition::StreamRecoveryKind;

	use super::PointCx;
	use crate::campaign::{CampaignMachine, CampaignStateError, Reaction, Verdict};

	impl TtsrCampaign {
		pub(crate) fn install(&mut self, registry: TtsrRegistry) {
			self.registry = Some(registry);
			self.deferred.clear();
			self.pending_cut = None;
		}

		pub(crate) fn reset_streams(&mut self) {
			if let Some(registry) = self.registry.as_mut() {
				registry.reset_streams();
			}
		}

		pub(crate) fn advance_message(&mut self) {
			if let Some(registry) = self.registry.as_mut() {
				registry.advance_message();
			}
		}

		pub(crate) fn mark_injected<'a>(&mut self, names: impl IntoIterator<Item = &'a str>) {
			if let Some(registry) = self.registry.as_mut() {
				registry.mark_injected(names);
			}
		}

		pub(crate) fn check_delta(
			&mut self,
			state: &mut TtsrStreamPart,
			fragment: &str,
		) -> Option<TtsrCampaignCut> {
			let registry = self.registry.as_mut()?;
			let mut paths = Vec::new();
			let mut snapshot = None;
			if state.source == TtsrSource::Tool {
				state.arguments.push_str(fragment);
				let parsed = omp_slopjson::parse_streaming(state.arguments.as_str());
				collect_ttsr_paths(&parsed, &mut paths);
				snapshot = Some(tool_matcher_snapshot(&parsed, state.arguments.as_str()));
			}
			let path_refs = paths.iter().map(Str::as_str).collect::<Vec<_>>();
			let context = TtsrMatchContext {
				source:     state.source,
				tool_name:  state.tool_name.as_ref().map(Str::as_str),
				file_paths: path_refs.as_slice(),
				stream_key: Some(state.stream_key.as_str()),
			};
			let mut matches = if let Some(snapshot) = snapshot.as_deref() {
				registry.check_snapshot(snapshot, context).into_vec()
			} else {
				registry.check_delta(fragment, context).into_vec()
			};
			if let Some(snapshot) = snapshot.as_deref()
				&& registry.has_ast_rules()
				&& let Ok(ast_matches) = registry.check_ast_snapshot(snapshot, context)
			{
				for matched in ast_matches {
					if !matches.iter().any(|present| present.name == matched.name) {
						matches.push(matched);
					}
				}
			}
			if matches.is_empty() {
				return None;
			}
			if matches
				.iter()
				.any(|matched| matched.interrupt_mode.interrupts(state.source))
			{
				let cut = TtsrCampaignCut { matches, source: state.source };
				self.pending_cut = Some(cut.clone());
				return Some(cut);
			}
			for matched in matches {
				if !self
					.deferred
					.iter()
					.any(|present| present.matched.name == matched.name)
				{
					self
						.deferred
						.push(DeferredTtsr { matched, source: state.source });
				}
			}
			None
		}

		pub(crate) fn take_deferred(&mut self) -> Option<(TtsrSource, Vec<TtsrMatch>, String, Item)> {
			if self.deferred.is_empty() {
				return None;
			}
			let deferred = mem::take(&mut self.deferred);
			let source = deferred[0].source;
			let matches = deferred
				.into_iter()
				.map(|entry| entry.matched)
				.collect::<Vec<_>>();
			let text = ttsr_reminder_text(&matches);
			let item = ttsr_reminder_item(text.clone());
			Some((source, matches, text, item))
		}

		pub(crate) fn has_pending(&self) -> bool {
			self.pending_cut.is_some() || !self.deferred.is_empty()
		}
	}
	impl CampaignMachine for TtsrCampaign {
		fn react(&mut self, point: Point, _: &PointCx<'_>) -> Reaction {
			if point != Point::Stream {
				return Reaction::default();
			}
			let Some(cut) = self.pending_cut.take() else {
				return Reaction::one(Verdict::Pass);
			};
			let text = ttsr_reminder_text(&cut.matches);
			Reaction {
				verdicts: vec![
					Verdict::Cut { reason: Str::new_static("stream rule interrupted generation") },
					Verdict::Inject(vec![ttsr_reminder_item(text)]),
				],
			}
		}

		fn state(&self) -> Str {
			Str::new_static("{}")
		}

		fn restore(&mut self, payload: &str) -> Result<(), CampaignStateError> {
			if payload == "{}" {
				Ok(())
			} else {
				Err(CampaignStateError::InvalidPayload)
			}
		}
	}

	fn collect_ttsr_paths(value: &omp_slopjson::Value, paths: &mut Vec<Str>) {
		match value {
			omp_slopjson::Value::Object(object) => {
				for (key, value) in object.iter() {
					let normalized = key.to_ascii_lowercase();
					let path_field = normalized == "path"
						|| normalized == "file"
						|| normalized.ends_with("_path")
						|| normalized.ends_with("path");
					if path_field
						&& let Some(path) = value.as_str()
						&& !path.is_empty()
						&& !paths.iter().any(|present| present == path)
					{
						paths.push(Str::new(path));
					}
					if normalized == "paths" || normalized == "files" {
						for path in value.as_array().unwrap_or_default() {
							if let Some(path) = path.as_str()
								&& !path.is_empty()
								&& !paths.iter().any(|present| present == path)
							{
								paths.push(Str::new(path));
							}
						}
					}
					collect_ttsr_paths(value, paths);
				}
			},
			omp_slopjson::Value::Array(values) => {
				for value in values {
					collect_ttsr_paths(value, paths);
				}
			},
			_ => {},
		}
	}

	fn tool_matcher_snapshot(value: &omp_slopjson::Value, fallback: &str) -> String {
		let mut snapshot = String::new();
		collect_ttsr_source(value, None, &mut snapshot);
		if snapshot.is_empty() {
			snapshot.push_str(fallback);
		}
		snapshot
	}

	fn collect_ttsr_source(value: &omp_slopjson::Value, field: Option<&str>, output: &mut String) {
		match value {
			omp_slopjson::Value::String(text)
				if field.is_some_and(|field| {
					matches!(
						field,
						"content"
							| "text" | "new"
							| "new_text" | "newtext"
							| "replacement"
							| "patch" | "code"
					)
				}) =>
			{
				if !output.is_empty() {
					output.push('\n');
				}
				output.push_str(text);
			},
			omp_slopjson::Value::Object(object) => {
				for (key, value) in object.iter() {
					let normalized = key.to_ascii_lowercase();
					collect_ttsr_source(value, Some(normalized.as_str()), output);
				}
			},
			omp_slopjson::Value::Array(values) => {
				for value in values {
					collect_ttsr_source(value, field, output);
				}
			},
			_ => {},
		}
	}

	pub(crate) fn ttsr_reminder_text(matches: &[TtsrMatch]) -> String {
		let mut text = String::from(
			"<system-injection>\nThe previous generation was interrupted by the following stream \
			 rules. Correct the output before continuing.\n",
		);
		for matched in matches {
			let _ =
				writeln!(text, "\nRule `{}`:\n{}", matched.name.as_str(), matched.content.as_str());
		}
		text.push_str("</system-injection>");
		text
	}

	pub(crate) fn ttsr_reminder_item(text: String) -> Item {
		Item {
			seq:           0,
			created_at_ms: now_ms(),
			kind:          Some(item::Kind::Message(thread::Message {
				role:  thread::Role::User as i32,
				parts: vec![thread::Part { kind: Some(thread::part::Kind::Text(text)) }],
			})),
			props:         None,
		}
	}

	pub(crate) fn stream_recovery_item(kind: StreamRecoveryKind) -> Item {
		let reason = match kind {
			StreamRecoveryKind::Http2Reset => {
				"the provider reset the response stream before output committed"
			},
			StreamRecoveryKind::FirstEventStall => {
				"the provider produced no first response event before the watchdog expired"
			},
			StreamRecoveryKind::PostToolIdleStall => {
				"the provider stalled after tool results before producing another event"
			},
		};
		Item {
			seq:           0,
			created_at_ms: now_ms(),
			kind:          Some(item::Kind::Message(thread::Message {
				role:  thread::Role::User as i32,
				parts: vec![thread::Part {
					kind: Some(thread::part::Kind::Text(format!(
						"<system-injection>\nThe prior model attempt was retried because {reason}. \
						 Continue from the retained context without repeating completed \
						 work.\n</system-injection>"
					))),
				}],
			})),
			props:         None,
		}
	}
}
