//! Named decision-point seams for the closed agent loop.

use std::{mem, sync};

use flume::Receiver;
use omp_core::{Point, PointSet, Str};
use serde::{Deserialize, Serialize};

use crate::{
	Journal, JournalError,
	regime::{
		Regime, RegimeDraft, RegimeRecord, RegimeResolution, RegimeSet, RegimeSpec, RegimeStateError,
		RegimeStatus, RegimeStepResult, RevivalReport, StartError, StartOptions, StartReceipt,
		StopError, absorb_draft, evaluate_regime,
	},
	tool_choice::ToolChoiceQueue,
};

/// Immutable facts available to lanes at one decision point.
#[derive(Clone, Copy, Debug, Default)]
pub struct PointCx<'a> {
	/// Current durable turn identity, when a turn exists.
	pub turn_id:           Option<&'a str>,
	/// Current invocation identity at ADMISSION/BATCH.
	pub invocation_id:     Option<&'a str>,
	/// Current streamed UTF-8 fragment at STREAM.
	pub stream_delta:      Option<&'a str>,
	/// Current epoch milliseconds.
	pub now_ms:            u64,
	/// Whether the preceding operation delivered an observable effect.
	pub delivered:         bool,
	/// Whether an exploration checkpoint is currently active.
	pub checkpoint_active: bool,
}

/// Durable forensic representation of one resolved regime event.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RegimeFact {
	/// Fixed event resolved.
	pub point:                  Point,
	/// Durable turn identity, when the event belongs to a turn.
	pub turn_id:                Option<Str>,
	/// Participating activation identities in deterministic order.
	pub participants:           Vec<Str>,
	/// Resolved control class.
	pub control:                Str,
	/// Activation supplying the exclusive control, if any.
	pub controlling_activation: Option<Str>,
	/// Number of committed context rewrites.
	pub rewrite_count:          u32,
	/// Number of committed context appends.
	pub append_count:           u32,
	/// Number of combined rejection reasons.
	pub rejection_count:        u32,
	/// Number of unresolved waits.
	pub wait_count:             u32,
}

impl RegimeFact {
	fn from_resolution(point: Point, cx: &PointCx<'_>, resolution: &RegimeResolution) -> Self {
		Self {
			point,
			turn_id: cx.turn_id.map(Str::new),
			participants: resolution.participants.clone(),
			control: Str::new(<&'static str>::from(resolution.control)),
			controlling_activation: resolution.controlling_activation.clone(),
			rewrite_count: u32::try_from(resolution.patches.len()).unwrap_or(u32::MAX),
			append_count: u32::try_from(resolution.injects.len()).unwrap_or(u32::MAX),
			rejection_count: u32::try_from(resolution.denials.len()).unwrap_or(u32::MAX),
			wait_count: u32::try_from(resolution.waits.len()).unwrap_or(u32::MAX),
		}
	}

	/// Whether the fact records any regime activity worth persisting.
	///
	/// Uncontested resolutions (no participants, no control, zero committed
	/// effects) resolve on every stream delta; journaling them writes two
	/// no-op lines per chunk. Only material facts are durable.
	pub fn is_material(&self) -> bool {
		!self.participants.is_empty()
			|| self.control != "none"
			|| self.controlling_activation.is_some()
			|| self.rewrite_count > 0
			|| self.append_count > 0
			|| self.rejection_count > 0
			|| self.wait_count > 0
	}
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedEvent {
	pub(crate) regime: RegimeResolution,
	pub(crate) fact:   RegimeFact,
}

/// Arbiter owner for all point subscriptions and resolution facts.
pub struct Arbiter {
	regimes:            RegimeSet,
	ttsr_regime:        stream::TtsrRegime,
	empty_output_retry: settle::EmptyOutputRetry,
	checkpoint_notice:  context::CheckpointNotice,
	retry_chain:        Option<settle::RetryChainRegime>,
	pending_facts:      Vec<RegimeFact>,
	pending_records:    Vec<RegimeRecord>,
	subscribed:         PointSet,
	fact_tx:            flume::Sender<RegimeFact>,
	fact_rx:            Receiver<RegimeFact>,
}

impl Default for Arbiter {
	fn default() -> Self {
		let (fact_tx, fact_rx) = flume::unbounded();
		Self {
			regimes: RegimeSet::new(),
			ttsr_regime: stream::TtsrRegime::default(),
			empty_output_retry: settle::EmptyOutputRetry::default(),
			checkpoint_notice: context::CheckpointNotice::default(),
			retry_chain: None,
			pending_facts: Vec::new(),
			pending_records: Vec::new(),
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

	/// Returns the durable regime owner.
	pub const fn regimes(&self) -> &RegimeSet {
		&self.regimes
	}

	/// Returns mutable access to the durable regime owner.
	pub const fn regimes_mut(&mut self) -> &mut RegimeSet {
		&mut self.regimes
	}

	pub(crate) const fn ttsr_regime_mut(&mut self) -> &mut stream::TtsrRegime {
		&mut self.ttsr_regime
	}

	pub(crate) const fn checkpoint_notice_mut(&mut self) -> &mut context::CheckpointNotice {
		&mut self.checkpoint_notice
	}

	pub(crate) fn restore_empty_output_retry(&mut self, committed_steps: u8) {
		self.empty_output_retry = settle::EmptyOutputRetry::recovered(committed_steps);
	}

	pub(crate) const fn reset_empty_output_retry(&mut self) {
		self.empty_output_retry.reset();
	}

	pub(crate) const fn empty_output_retry_spent(&self) -> u8 {
		self.empty_output_retry.spent()
	}

	pub(crate) fn apply_empty_output_retry(&mut self, cx: &PointCx<'_>) -> RegimeDraft {
		evaluate_regime(
			Point::Settle,
			cx,
			"empty-output-retry",
			u32::from(self.empty_output_retry.spent()),
			|ctx, next| self.empty_output_retry.apply(ctx, next),
		)
		.expect("core empty-output regime is infallible")
	}

	pub(crate) fn set_retry_chain(&mut self, routes: Vec<Str>) {
		if let Some(chain) = self.retry_chain.as_mut()
			&& chain.routes() == routes.as_slice()
		{
			chain.retry_now();
		} else {
			self.retry_chain = Some(settle::RetryChainRegime::new(routes));
		}
	}

	/// Starts and journals one activation atomically.
	pub fn start(
		&mut self,
		spec: sync::Arc<RegimeSpec>,
		handler: Box<dyn Regime>,
		journal: &mut Journal,
		options: StartOptions,
	) -> Result<StartReceipt, ArbiterError> {
		let receipt = self.regimes.start(spec, handler, options)?;
		let record = self
			.regimes
			.records()
			.into_iter()
			.find(|record| record.activation == receipt.activation)
			.expect("new activation has one durable record");
		if let Err(error) = journal.append_regime_record(options.now_ms, &record) {
			self.regimes.cancel(receipt.activation.as_str());
			return Err(ArbiterError::Journal(error));
		}
		Ok(receipt)
	}

	/// Stops one activation after minimum duration and journals the transition.
	///
	/// While a durable turn is pending, the in-memory activation is released
	/// immediately (so the next turn starts outside the regime) and the
	/// terminal record is buffered for [`Arbiter::flush`] at settlement; the
	/// journal rejects extension writes mid-turn.
	pub fn stop(
		&mut self,
		activation: &str,
		now_ms: u64,
		journal: &mut Journal,
	) -> Result<bool, ArbiterError> {
		let Some(mut terminal) = self
			.regimes
			.records()
			.into_iter()
			.find(|record| record.activation == activation)
		else {
			return Ok(false);
		};
		if !self.regimes.check_stop(activation, now_ms)? {
			return Ok(false);
		}
		terminal.status = RegimeStatus::Stopped;
		if journal.pending_turn().is_some() {
			self.pending_records.push(terminal);
			return Ok(self.regimes.cancel(activation));
		}
		journal.append_regime_record(now_ms, &terminal)?;
		let removed = self.regimes.cancel(activation);
		if removed {
			self.checkpoint(journal, now_ms)?;
		}
		Ok(removed)
	}

	/// Cancels one activation immediately and journals the transition.
	///
	/// Mid-turn cancellations release the activation now and buffer the
	/// terminal record for [`Arbiter::flush`], mirroring [`Arbiter::stop`].
	pub fn cancel(
		&mut self,
		activation: &str,
		now_ms: u64,
		journal: &mut Journal,
	) -> Result<bool, JournalError> {
		let Some(mut terminal) = self
			.regimes
			.records()
			.into_iter()
			.find(|record| record.activation == activation)
		else {
			return Ok(false);
		};
		terminal.status = RegimeStatus::Stopped;
		if journal.pending_turn().is_some() {
			self.pending_records.push(terminal);
			return Ok(self.regimes.cancel(activation));
		}
		journal.append_regime_record(now_ms, &terminal)?;
		let removed = self.regimes.cancel(activation);
		self.checkpoint(journal, now_ms)?;
		Ok(removed)
	}

	/// Updates one handler state and journals the resulting record.
	pub fn update_state(
		&mut self,
		activation: &str,
		payload: &[u8],
		now_ms: u64,
		journal: &mut Journal,
	) -> Result<RegimeRecord, ArbiterError> {
		let record = self.regimes.update_state(activation, payload)?;
		journal.append_regime_record(now_ms, &record)?;
		Ok(record)
	}

	/// Advances one activation's committed-step accounting and journals it.
	pub fn advance(
		&mut self,
		activation: &str,
		now_ms: u64,
		journal: &mut Journal,
	) -> Result<RegimeStepResult, JournalError> {
		let result = self.regimes.advance(activation, now_ms);
		if !matches!(result, RegimeStepResult::Missing) {
			if let Some(record) = self
				.regimes
				.records()
				.into_iter()
				.find(|record| record.activation == activation)
			{
				journal.append_regime_record(now_ms, &record)?;
			}
		}
		self.checkpoint(journal, now_ms)?;
		Ok(result)
	}

	/// Journals current state for every active or queued activation.
	pub fn checkpoint(&self, journal: &mut Journal, now_ms: u64) -> Result<(), JournalError> {
		for record in self.regimes.records() {
			journal.append_regime_record(now_ms, &record)?;
		}
		Ok(())
	}

	/// Revives durable activations and journals typed revival failures.
	pub fn recover<F>(
		&mut self,
		journal: &mut Journal,
		mut resolve: F,
		now_ms: u64,
	) -> Result<RevivalReport, JournalError>
	where
		F: FnMut(&str) -> Option<(sync::Arc<RegimeSpec>, Box<dyn Regime>)>,
	{
		let records = journal.recover_regime_records()?;
		let report = self.regimes.revive(records, |id| resolve(id));
		for record in &report.failed {
			journal.append_regime_record(now_ms, record)?;
		}
		Ok(report)
	}

	/// Registers fixed event bits for fast loop placement checks.
	pub fn register_points(&mut self, points: PointSet) {
		self.subscribed = self.subscribed.union(points);
	}

	/// Returns the union of registered event subscriptions.
	pub const fn subscriptions(&self) -> PointSet {
		self.subscribed
	}

	/// Resolves one fixed event and emits its durable forensic fact.
	pub(crate) fn resolve(
		&mut self,
		point: Point,
		cx: &PointCx<'_>,
		tool_choices: Option<&mut ToolChoiceQueue>,
	) -> ResolvedEvent {
		let mut regime = self.regimes.resolve(point, cx, tool_choices);
		if point == Point::Stream && self.ttsr_regime.has_pending() {
			if let Ok(draft) =
				evaluate_regime(point, cx, "ttsr", 0, |ctx, next| self.ttsr_regime.apply(ctx, next))
			{
				absorb_draft(&mut regime, Str::new_static("ttsr"), draft);
			}
		}
		if point == Point::Context && self.checkpoint_notice.is_active() {
			if let Ok(draft) = evaluate_regime(point, cx, "checkpoint", 0, |ctx, next| {
				self.checkpoint_notice.apply(ctx, next)
			}) {
				absorb_draft(&mut regime, Str::new_static("checkpoint"), draft);
			}
		}
		if matches!(point, Point::PreModel | Point::Stream)
			&& self
				.retry_chain
				.as_ref()
				.is_some_and(|chain| chain.is_active())
			&& let Some(chain) = self.retry_chain.as_mut()
		{
			if let Ok(draft) =
				evaluate_regime(point, cx, "retry-chain", 0, |ctx, next| chain.apply(ctx, next))
			{
				absorb_draft(&mut regime, Str::new_static("retry-chain"), draft);
			}
		}
		let fact = RegimeFact::from_resolution(point, cx, &regime);
		let _ = self.fact_tx.send(fact.clone());
		ResolvedEvent { regime, fact }
	}

	/// Resolves and atomically appends the forensic fact to the journal.
	///
	/// Immaterial facts (see [`RegimeFact::is_material`]) are resolved and
	/// reported on the telemetry channel but never journaled.
	pub(crate) fn resolve_and_record(
		&mut self,
		point: Point,
		cx: &PointCx<'_>,
		tool_choices: Option<&mut ToolChoiceQueue>,
		journal: &mut Journal,
	) -> Result<ResolvedEvent, JournalError> {
		let resolved = self.resolve(point, cx, tool_choices);
		if !resolved.fact.is_material() {
			return Ok(resolved);
		}
		if journal.pending_turn().is_some() {
			self.pending_facts.push(resolved.fact.clone());
		} else {
			self.flush(journal, cx.now_ms)?;
			journal.append_regime_fact(cx.now_ms, &resolved.fact)?;
			self.checkpoint(journal, cx.now_ms)?;
		}
		Ok(resolved)
	}

	/// Flushes facts buffered while a durable turn was pending.
	///
	/// Also lands terminal records for regimes stopped or cancelled mid-turn.
	pub fn flush(&mut self, journal: &mut Journal, now_ms: u64) -> Result<(), JournalError> {
		for fact in mem::take(&mut self.pending_facts) {
			journal.append_regime_fact(now_ms, &fact)?;
		}
		for record in mem::take(&mut self.pending_records) {
			journal.append_regime_record(now_ms, &record)?;
		}
		self.checkpoint(journal, now_ms)
	}

	/// Drains one telemetry fact without blocking.
	pub fn try_fact(&self) -> Option<RegimeFact> {
		self.fact_rx.try_recv().ok()
	}
}
/// Failure while starting, stopping, updating, or journaling a regime.
#[derive(Debug, thiserror::Error)]
pub enum ArbiterError {
	/// The regime declaration or resource set rejected activation.
	#[error(transparent)]
	Start(#[from] StartError),
	/// The activation's minimum-duration policy rejected exit.
	#[error(transparent)]
	Stop(#[from] StopError),
	/// The handler rejected a live state update.
	#[error(transparent)]
	State(#[from] RegimeStateError),
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

	use crate::{
		Journal, JournalError,
		journal_kinds::{CHECKPOINT_KIND, REWIND_REPORT_KIND},
		r#loop::now_ms,
		regime::{Next, Regime, RegimeContext, RegimeError, RegimeStateError},
	};

	/// Session-scoped checkpoint notice regime.
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

	impl Regime for CheckpointNotice {
		fn apply(&mut self, ctx: &mut RegimeContext<'_>, next: Next<'_>) -> Result<(), RegimeError> {
			if ctx.point() == Point::Context && self.active {
				ctx.append_context(vec![crate::prompt::checkpoint_active_reminder()]);
			} else if !self.active {
				next.complete();
			}
			Ok(())
		}

		fn state(&self) -> Str {
			Str::new_static(if self.active { "active" } else { "inactive" })
		}

		fn restore(&mut self, payload: &str) -> Result<(), RegimeStateError> {
			self.active = match payload {
				"active" => true,
				"inactive" => false,
				_ => return Err(RegimeStateError::InvalidPayload),
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
		continuation::{Continuation, ContinuationPolicy, ContinuationSource, LoopSignal},
		r#loop::now_ms,
		regime::{
			Next, Regime, RegimeContext, RegimeError, RegimeStateError, ScopedSetting, SettingSlot,
		},
		turn::empty_stop,
	};

	/// Behavior-preserving empty-output retry counter and finite cap.
	#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
	pub(crate) struct EmptyOutputRetry {
		spent: u8,
	}
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
	impl Regime for EmptyOutputRetry {
		fn apply(&mut self, ctx: &mut RegimeContext<'_>, next: Next<'_>) -> Result<(), RegimeError> {
			if ctx.point() != Point::Settle {
				return Ok(());
			}
			let Some(item) = self.step() else {
				next.fail("Assistant returned no final output after retry cap; try switching models");
				return Ok(());
			};
			ctx.rewrite_context(crate::ContextPatch(Bytes::from_static(b"drop:turn-tail")));
			ctx.append_context(vec![item]);
			next.retry();
			Ok(())
		}

		fn state(&self) -> Str {
			Str::from(self.spent.to_string())
		}

		fn restore(&mut self, payload: &str) -> Result<(), RegimeStateError> {
			self.spent = payload
				.parse()
				.map_err(|_| RegimeStateError::InvalidPayload)?;
			Ok(())
		}
	}

	/// Provider failover regime that scopes each route in chain order.
	pub(crate) struct RetryChainRegime {
		routes:         Vec<Str>,
		cursor:         usize,
		cooldown_ms:    u64,
		cooldown_until: Option<u64>,
	}

	impl RetryChainRegime {
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

	impl Regime for RetryChainRegime {
		fn apply(&mut self, ctx: &mut RegimeContext<'_>, _: Next<'_>) -> Result<(), RegimeError> {
			if !matches!(ctx.point(), Point::PreModel | Point::Stream) {
				return Ok(());
			}
			if self
				.cooldown_until
				.is_some_and(|until| ctx.facts().now_ms < until)
			{
				return Ok(());
			}
			self.cooldown_until = None;
			let Some(route) = self.routes.get(self.cursor).cloned() else {
				return Ok(());
			};
			self.cursor = self.cursor.saturating_add(1);
			self.cooldown_until = Some(ctx.facts().now_ms.saturating_add(self.cooldown_ms));
			ctx.rewrite_context(crate::ContextPatch(bytes::Bytes::from_static(b"provider-failover")));
			ctx.set_scoped(ScopedSetting { slot: SettingSlot::ModelRoute, value: route });
			Ok(())
		}

		fn state(&self) -> Str {
			Str::from(format!("{}:{}", self.cursor, self.cooldown_until.unwrap_or(0)))
		}

		fn restore(&mut self, payload: &str) -> Result<(), RegimeStateError> {
			let (cursor, until) = payload
				.split_once(':')
				.ok_or(RegimeStateError::InvalidPayload)?;
			self.cursor = cursor
				.parse()
				.map_err(|_| RegimeStateError::InvalidPayload)?;
			self.cooldown_until = Some(
				until
					.parse()
					.map_err(|_| RegimeStateError::InvalidPayload)?,
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
	pub(crate) struct TtsrRegimeCancel {
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
	pub(crate) struct TtsrRegime {
		registry:    Option<TtsrRegistry>,
		deferred:    Vec<DeferredTtsr>,
		pending_cut: Option<TtsrRegimeCancel>,
	}
	use std::mem;

	use omp_inference::recovery::repetition::StreamRecoveryKind;

	use crate::regime::{Next, Regime, RegimeContext, RegimeError, RegimeStateError};

	impl TtsrRegime {
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
		) -> Option<TtsrRegimeCancel> {
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
				let cut = TtsrRegimeCancel { matches, source: state.source };
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
	impl Regime for TtsrRegime {
		fn apply(&mut self, ctx: &mut RegimeContext<'_>, next: Next<'_>) -> Result<(), RegimeError> {
			if ctx.point() != Point::Stream {
				return Ok(());
			}
			let Some(cancel) = self.pending_cut.take() else {
				return Ok(());
			};
			let text = ttsr_reminder_text(&cancel.matches);
			ctx.append_context(vec![ttsr_reminder_item(text)]);
			next.cancel("stream rule interrupted generation");
			Ok(())
		}

		fn state(&self) -> Str {
			Str::new_static("{}")
		}

		fn restore(&mut self, payload: &str) -> Result<(), RegimeStateError> {
			if payload == "{}" {
				Ok(())
			} else {
				Err(RegimeStateError::InvalidPayload)
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
#[cfg(test)]
mod tests {
	use std::{env, fs};

	use omp_core::sf;
	use omp_storage::transcript::{Header, SessionId};

	use super::*;
	use crate::regime::{Next, RegimeContext, RegimeError, RegimeLifetime};

	struct Noop;

	impl Regime for Noop {
		fn apply(&mut self, _: &mut RegimeContext<'_>, _: Next<'_>) -> Result<(), RegimeError> {
			Ok(())
		}

		fn state(&self) -> Str {
			Str::new_static("{}")
		}

		fn restore(&mut self, _: &str) -> Result<(), RegimeStateError> {
			Ok(())
		}
	}

	fn fact_lines(path: &std::path::Path) -> usize {
		fs::read_to_string(path).map_or(0, |text| {
			text
				.lines()
				.filter(|line| line.contains("dev.omp.core.regime-fact"))
				.count()
		})
	}

	#[test]
	fn immaterial_resolutions_are_not_journaled() {
		let path = env::temp_dir().join(format!(
			"omp-agent-arbiter-facts-{}-{}.jsonl",
			std::process::id(),
			omp_core::Ulid::generate()
		));
		let mut journal = Journal::create(&path, &Header {
			v:       4,
			id:      SessionId(sf!("arbiter-facts")),
			created: 1,
			cwd:     env::temp_dir(),
		})
		.expect("create journal");
		let mut arbiter = Arbiter::new();
		let cx = PointCx { turn_id: Some("turn-1"), now_ms: 1, ..PointCx::default() };
		for _ in 0..3 {
			arbiter
				.resolve_and_record(Point::Stream, &cx, None, &mut journal)
				.expect("resolve immaterial stream point");
		}
		assert_eq!(fact_lines(&path), 0, "no-op resolutions must not journal facts");

		let spec = sync::Arc::new(RegimeSpec {
			id: sf!("noop"),
			events: PointSet::from(Point::Stream),
			precedence: 0,
			max_steps: None,
			committed_step_interval_ms: None,
			on_limit: false,
			lifetime: RegimeLifetime::Run,
			family_rev: sf!("test@1"),
			when: None,
			owns: sync::Arc::from([]),
			sets: sync::Arc::from([]),
			minimum_duration_ms: None,
		});
		arbiter
			.start(spec, Box::new(Noop), &mut journal, StartOptions { now_ms: 2, queue: false })
			.expect("start regime");
		let resolved = arbiter
			.resolve_and_record(Point::Stream, &cx, None, &mut journal)
			.expect("resolve material stream point");
		assert!(resolved.fact.is_material(), "participating regime makes the fact material");
		assert_eq!(fact_lines(&path), 1, "material resolutions stay durable");
		drop(journal);
		fs::remove_file(path).expect("remove journal");
	}
}
