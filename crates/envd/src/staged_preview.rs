//! Session-scoped campaign enforcing resolution of staged tool proposals.

use std::{
	sync::Arc,
	time::{SystemTime, UNIX_EPOCH},
};

use omp_agent::{
	CampaignMachine, CampaignScope, CampaignSpec, CampaignStateError, ExhaustPolicy, Ladder,
	LadderStep, Reaction, Verdict,
};
use omp_core::{Point, Str, sf};
use omp_proto::thread::v1::{self as thread, Item};
use omp_tools::staging::{
	PREVIEW_PENDING_NOTICE, PendingPreview, PreviewDecision, PreviewObserver, PreviewObserverError,
	PreviewRejection, parse_resolution_invoke,
};
const MAX_FORCE_ATTEMPTS: u8 = 3;
const EXHAUST_DETAIL: &str =
	"staged preview resolution exhausted; the proposal was rejected without being applied";
/// Builds the late-bound observer installed at the environment staging seam.
pub(super) fn observer(sender: omp_agent::ControlSender) -> PreviewObserver {
	Arc::new(move |pending| {
		let sender = sender.clone();
		Box::pin(async move {
			let receipt = sender
				.engage_campaign(
					spec(),
					Box::new(StagedPreviewCampaign::new(pending.clone()).with_sender(sender.clone())),
					omp_agent::EngageOptions { now_ms: now_ms(), queue: false },
				)
				.await
				.map_err(|_| PreviewObserverError::Rejected)?;
			let pending_id = pending.id.clone();
			let resolution = pending.invoker.clone();
			let cleanup_sender = sender.clone();
			let cleanup_id = pending.id.clone();
			let engagement = receipt.engagement.clone();
			let invoker: omp_agent::tool_choice::Invoker = Arc::new(move |input| {
				let resolution = resolution.clone();
				let cleanup_sender = cleanup_sender.clone();
				let cleanup_id = cleanup_id.clone();
				let engagement = engagement.clone();
				Box::pin(async move {
					let result =
						parse_resolution_invoke(&input).and_then(|decision| resolution(decision));
					match result {
						Ok(outcome) => {
							tokio::spawn(async move {
								let _ = cleanup_sender.remove_pending_invoker(cleanup_id).await;
								let _ = cleanup_sender.disengage_campaign(engagement).await;
							});
							serde_json::to_value(outcome).unwrap_or_else(
								|_| serde_json::json!({ "error": "staged proposal result encoding failed" }),
							)
						},
						Err(error) => serde_json::json!({ "error": error.to_string() }),
					}
				})
			});
			if sender
				.register_pending_invoker(pending_id, pending.source_tool, invoker)
				.await
				.is_err()
			{
				let _ = sender.disengage_campaign(receipt.engagement).await;
				return Err(PreviewObserverError::Unavailable);
			}
			Ok(())
		})
	})
}

fn now_ms() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map_or(0, |duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
}
/// Builds the bounded staged-preview campaign declaration.
pub(super) fn spec() -> Arc<CampaignSpec> {
	Arc::new(CampaignSpec {
		id:         Str::new_static("staged-preview"),
		points:     Point::Context.set().with(Point::ToolChoice),
		precedence: 0,
		ladder:     Some(Ladder::new(Arc::from([
			LadderStep { label: Str::new_static("pending-preview-notice"), verdict: Verdict::Pass },
			LadderStep {
				label:   Str::new_static("force-resolution-invoke-1"),
				verdict: Verdict::Pass,
			},
			LadderStep {
				label:   Str::new_static("force-resolution-invoke-2"),
				verdict: Verdict::Pass,
			},
			LadderStep {
				label:   Str::new_static("force-resolution-invoke-3"),
				verdict: Verdict::Pass,
			},
		]))),
		exhaust:    ExhaustPolicy::Fault { detail: Str::new_static(EXHAUST_DETAIL) },
		scope:      CampaignScope::Session,
		family_rev: Str::new_static("dev.omp.tools.staged-preview@1"),
		when:       None,
		members:    Arc::from([]),
		claims:     Arc::from([]),
		binds:      Arc::from([]),
		dwell_ms:   None,
	})
}

/// One proposal-specific staged-preview escalation machine.
pub(super) struct StagedPreviewCampaign {
	pending: PendingPreview,
	rung:    u8,
	cleanup: Option<omp_agent::ControlSender>,
}

impl StagedPreviewCampaign {
	/// Creates the machine for one exact pending-invoker identity.
	pub(super) const fn new(pending: PendingPreview) -> Self {
		Self { pending, rung: 0, cleanup: None }
	}

	fn reminder(&self) -> Item {
		let text = sf!(
			"{PREVIEW_PENDING_NOTICE} The pending proposal was staged by {}.",
			self.pending.source_tool.as_str()
		);
		Item {
			kind: Some(thread::item::Kind::Message(thread::Message {
				role:  thread::Role::User as i32,
				parts: vec![thread::Part { kind: Some(thread::part::Kind::Text(text.to_string())) }],
			})),
			..Item::default()
		}
	}

	fn with_sender(mut self, sender: omp_agent::ControlSender) -> Self {
		self.cleanup = Some(sender);
		self
	}

	fn exhaust(&mut self) -> Reaction {
		let decision = PreviewDecision::Reject(PreviewRejection::CampaignExhausted);
		let _ = (self.pending.invoker)(decision);
		if let Some(sender) = self.cleanup.clone() {
			let id = self.pending.id.clone();
			tokio::spawn(async move {
				let _ = sender.remove_pending_invoker(id).await;
			});
		}
		self.rung = MAX_FORCE_ATTEMPTS.saturating_add(2);
		Reaction {
			verdicts: vec![Verdict::Fault { detail: Str::new_static(EXHAUST_DETAIL) }, Verdict::Done],
		}
	}
}

impl CampaignMachine for StagedPreviewCampaign {
	fn react(&mut self, point: Point, cx: &omp_agent::arbiter::PointCx<'_>) -> Reaction {
		if cx
			.pending_invoker
			.is_none_or(|head| head.id != self.pending.id.as_str())
		{
			return Reaction::one(Verdict::Done);
		}
		if self.rung == 0 {
			if point != Point::Context {
				return Reaction::one(Verdict::Pass);
			}
			self.rung = 1;
			return Reaction::one(Verdict::Inject(vec![self.reminder()]));
		}
		if point != Point::ToolChoice {
			return Reaction::one(Verdict::Pass);
		}
		if self.rung <= MAX_FORCE_ATTEMPTS {
			self.rung = self.rung.saturating_add(1);
			return Reaction::one(Verdict::Force { tool: Str::new_static("dyn") });
		}
		self.exhaust()
	}

	fn state(&self) -> Str {
		Str::from(self.rung.to_string())
	}

	fn restore(&mut self, payload: &str) -> Result<(), CampaignStateError> {
		let rung = payload
			.parse()
			.map_err(|_| CampaignStateError::InvalidPayload)?;
		if rung > MAX_FORCE_ATTEMPTS.saturating_add(2) {
			return Err(CampaignStateError::InvalidPayload);
		}
		self.rung = rung;
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;

	use omp_agent::{
		CampaignMachine as _, CampaignStack, EngageOptions, Verdict, tool_choice::ToolChoiceQueue,
	};
	use omp_tools::staging::{PreviewError, PreviewObserver, PreviewRegistry, StagedAction};
	use parking_lot::Mutex;
	use serde_json::{Value, json};

	use super::*;

	struct RecordingAction(Arc<Mutex<Vec<PreviewDecision>>>);

	impl StagedAction for RecordingAction {
		fn finalize(&mut self, decision: &PreviewDecision) -> Result<Value, PreviewError> {
			self.0.lock().push(decision.clone());
			Ok(json!({ "rejected": true }))
		}
	}

	#[tokio::test]
	async fn exhaust_rejects_pending_proposal_with_typed_reason() {
		let registry = PreviewRegistry::new();
		let captured = Arc::new(Mutex::new(None));
		let captured_for_observer = Arc::clone(&captured);
		let observer: PreviewObserver = Arc::new(move |pending| {
			*captured_for_observer.lock() = Some(pending);
			Box::pin(async { Ok(()) })
		});
		registry.bind_observer(observer);
		let decisions = Arc::new(Mutex::new(Vec::new()));
		let pending = registry
			.stage(
				Str::new_static("ast_edit"),
				Str::new_static("one file would change"),
				RecordingAction(Arc::clone(&decisions)),
			)
			.await
			.expect("proposal staged");
		let mut machine = StagedPreviewCampaign::new(pending.clone());
		let cx = omp_agent::arbiter::PointCx {
			pending_invoker: Some(omp_agent::arbiter::PendingInvokerCx {
				id:          pending.id.as_str(),
				source_tool: pending.source_tool.as_str(),
			}),
			..omp_agent::arbiter::PointCx::default()
		};
		let notice = machine.react(Point::Context, &cx);
		assert!(matches!(notice.verdicts.as_slice(), [Verdict::Inject(_)]));
		for _ in 0..MAX_FORCE_ATTEMPTS {
			let forced = machine.react(Point::ToolChoice, &cx);
			assert!(matches!(
				forced.verdicts.as_slice(),
				[Verdict::Force { tool }] if tool == "dyn"
			));
		}
		let exhausted = machine.react(Point::ToolChoice, &cx);
		assert!(matches!(exhausted.verdicts.as_slice(), [Verdict::Fault { .. }, Verdict::Done]));
		assert_eq!(decisions.lock().as_slice(), &[PreviewDecision::Reject(
			PreviewRejection::CampaignExhausted
		)]);
		assert!(!registry.is_pending(pending.id.as_str()));
		assert_eq!(spec().ladder.as_ref().map(Ladder::len), Some(4));
	}
	#[tokio::test]
	async fn stage_forces_dyn_resolution_and_resolution_disengages() {
		let registry = PreviewRegistry::new();
		let captured = Arc::new(Mutex::new(None));
		let captured_for_observer = Arc::clone(&captured);
		registry.bind_observer(Arc::new(move |pending| {
			*captured_for_observer.lock() = Some(pending);
			Box::pin(async { Ok(()) })
		}));
		let decisions = Arc::new(Mutex::new(Vec::new()));
		let pending = registry
			.stage(
				Str::new_static("ast_edit"),
				Str::new_static("one file would change"),
				RecordingAction(Arc::clone(&decisions)),
			)
			.await
			.expect("proposal staged");
		let resolver = pending.invoker.clone();
		let invoker: omp_agent::tool_choice::Invoker = Arc::new(move |input| {
			let resolver = resolver.clone();
			Box::pin(async move {
				let decision = parse_resolution_invoke(&input).expect("resolution invoke");
				serde_json::to_value(resolver(decision).expect("proposal resolved"))
					.expect("outcome encoded")
			})
		});
		let mut choices = ToolChoiceQueue::new();
		choices.register_pending_invoker(pending.id.clone(), pending.source_tool.clone(), invoker);
		let mut campaigns = CampaignStack::new();
		let receipt = campaigns
			.engage(spec(), Box::new(StagedPreviewCampaign::new(pending.clone())), EngageOptions {
				now_ms: 1,
				queue:  false,
			})
			.expect("campaign engaged");
		let cx = omp_agent::arbiter::PointCx {
			now_ms: 1,
			pending_invoker: Some(omp_agent::arbiter::PendingInvokerCx {
				id:          pending.id.as_str(),
				source_tool: pending.source_tool.as_str(),
			}),
			..omp_agent::arbiter::PointCx::default()
		};
		let notice = campaigns.fold(Point::Context, &cx, Some(&mut choices));
		assert_eq!(notice.injects.len(), 1);
		let forced = campaigns.fold(Point::ToolChoice, &cx, Some(&mut choices));
		assert!(matches!(forced.winner, omp_agent::WinnerKind::Force));
		assert!(matches!(
			choices.claim_next(),
			Some(omp_inference::call::ToolChoice::Named(tool)) if tool == "dyn"
		));
		let outcome = choices
			.invoke_in_flight(json!({
				"do_": "invoke/resolve",
				"reason": "Apply the reviewed rewrite."
			}))
			.expect("forced pending invoker")
			.await;
		assert_eq!(outcome["decision"]["resolve"]["reason"], "Apply the reviewed rewrite.");
		choices.remove_pending_invoker(pending.id.as_str());
		choices.resolve();
		let done = campaigns.fold(
			Point::Context,
			&omp_agent::arbiter::PointCx { now_ms: 2, ..Default::default() },
			Some(&mut choices),
		);
		assert_eq!(done.terminated.as_slice(), &[receipt.engagement.clone()]);
		assert!(campaigns.spec_id(receipt.engagement.as_str()).is_none());
		assert_eq!(decisions.lock().as_slice(), &[PreviewDecision::Resolve {
			reason: Str::new_static("Apply the reviewed rewrite."),
		}]);
	}
}
