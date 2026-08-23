//! Shared staged-preview lifecycle for proposal-producing tools.

use std::{
	collections::BTreeMap,
	future::Future,
	io,
	path::PathBuf,
	pin::Pin,
	sync::{
		Arc,
		atomic::{AtomicU64, Ordering},
	},
};

use omp_core::{Str, sf};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Stable notice appended when a tool leaves a proposal uncommitted.
pub const PREVIEW_PENDING_NOTICE: &str = "A staged proposal is pending. Finalize it with dyn \
                                          using do_ `invoke/resolve` or `invoke/reject` and a \
                                          one-sentence `reason` before using another tool.";

/// Exact dynamic-device operation applying the pending proposal (`dyn
/// {"do_":"invoke/resolve",...}`).
pub const RESOLVE_OPERATION: &str = "invoke/resolve";
/// Exact dynamic-device operation discarding the pending proposal (`dyn
/// {"do_":"invoke/reject",...}`).
pub const REJECT_OPERATION: &str = "invoke/reject";
/// Why a staged proposal was rejected.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewRejection {
	/// The model explicitly rejected the proposal.
	Requested {
		/// One-sentence rejection reason.
		reason: Str,
	},
	/// The staged-preview campaign reached its finite escalation bound.
	CampaignExhausted,
}

/// A final decision for one staged proposal.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewDecision {
	/// Apply the proposal.
	Resolve {
		/// One-sentence application reason.
		reason: Str,
	},
	/// Discard the proposal.
	Reject(PreviewRejection),
}

/// Successful terminal result from a staged action.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PreviewOutcome {
	/// Unique proposal identity.
	pub id:       Str,
	/// Applied or rejected decision.
	pub decision: PreviewDecision,
	/// Action-specific result payload.
	pub payload:  Value,
}

/// Failure to resolve a staged proposal.
#[derive(Debug, thiserror::Error)]
pub enum PreviewError {
	/// No live proposal has this identity.
	#[error("staged proposal is no longer pending")]
	Unknown,
	/// The resolution invocation omitted its required reason.
	#[error("staged proposal resolution requires a one-sentence reason")]
	MissingReason,
	/// The invocation targeted a device that is not `resolve` or `reject`.
	#[error("device path is not a staged-proposal resolution device")]
	NotResolution,
	/// The staged action refused or could not finalize.
	#[error("staged proposal finalization failed")]
	Action(#[from] PreviewActionError),
}
/// Typed failure produced while finalizing a staged action.
#[derive(Debug, thiserror::Error)]
pub enum PreviewActionError {
	/// A staged document no longer matches its preflight revision.
	#[error("a staged document revision changed before resolution")]
	RevisionChanged {
		/// Document whose revision changed.
		path: PathBuf,
	},
	/// A filesystem operation failed.
	#[error("staged proposal filesystem operation failed")]
	Io {
		/// Resource being read, snapshotted, or replaced.
		path:   PathBuf,
		/// Typed I/O source.
		#[source]
		source: io::Error,
	},
	/// The terminal action payload could not be encoded.
	#[error("staged proposal result could not be encoded")]
	Encode(#[from] serde_json::Error),
}

/// Failure to announce a newly staged proposal to the active agent owner.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PreviewObserverError {
	/// No active agent owns the preview queue.
	#[error("staged proposal cannot be announced because the agent owner is unavailable")]
	Unavailable,
	/// The active agent rejected campaign engagement.
	#[error("staged proposal campaign engagement was rejected")]
	Rejected,
}

/// Action retained until an explicit resolution arrives.
pub trait StagedAction: Send + 'static {
	/// Applies or rejects this action. An error retains it for a corrected
	/// retry.
	fn finalize(&mut self, decision: &PreviewDecision) -> Result<Value, PreviewError>;
}

/// Synchronous resolver installed in the agent's pending-invoker head.
pub type PreviewInvoker =
	Arc<dyn Fn(PreviewDecision) -> Result<PreviewOutcome, PreviewError> + Send + Sync + 'static>;

/// Metadata and resolver for one newly staged proposal.
#[derive(Clone)]
pub struct PendingPreview {
	/// Unique proposal identity.
	pub id:          Str,
	/// Tool that produced the proposal.
	pub source_tool: Str,
	/// Bounded model-facing proposal summary.
	pub summary:     Str,
	/// Single-settlement resolver.
	pub invoker:     PreviewInvoker,
}

/// Future returned by the late-bound agent observer.
pub type PreviewObserverFuture =
	Pin<Box<dyn Future<Output = Result<(), PreviewObserverError>> + Send + 'static>>;
/// Callback that registers the pending invoker and engages its campaign.
pub type PreviewObserver =
	Arc<dyn Fn(PendingPreview) -> PreviewObserverFuture + Send + Sync + 'static>;

struct Entry {
	action: Box<dyn StagedAction>,
}

struct Inner {
	entries:  Mutex<BTreeMap<Str, Entry>>,
	observer: Mutex<Option<PreviewObserver>>,
	next_id:  AtomicU64,
}

/// Shared proposal registry used by every staging-capable tool in one
/// environment.
#[derive(Clone)]
pub struct PreviewRegistry(Arc<Inner>);

impl Default for PreviewRegistry {
	fn default() -> Self {
		Self(Arc::new(Inner {
			entries:  Mutex::new(BTreeMap::new()),
			observer: Mutex::new(None),
			next_id:  AtomicU64::new(1),
		}))
	}
}

impl PreviewRegistry {
	/// Creates an empty registry with no active agent observer.
	pub fn new() -> Self {
		Self::default()
	}

	/// Replaces the observer used for subsequently staged proposals.
	pub fn bind_observer(&self, observer: PreviewObserver) {
		*self.0.observer.lock() = Some(observer);
	}

	/// Removes the active observer without discarding already staged proposals.
	pub fn unbind_observer(&self) {
		self.0.observer.lock().take();
	}

	/// Stages an action, then announces it to the active agent owner.
	///
	/// An observer failure rolls the action back so no unresolvable proposal is
	/// left behind.
	pub async fn stage(
		&self,
		source_tool: Str,
		summary: Str,
		action: impl StagedAction,
	) -> Result<PendingPreview, PreviewObserverError> {
		let sequence = self.0.next_id.fetch_add(1, Ordering::Relaxed);
		let id = sf!("pending-action:{}:{sequence}", source_tool.as_str());
		self
			.0
			.entries
			.lock()
			.insert(id.clone(), Entry { action: Box::new(action) });
		let registry = self.clone();
		let invoke_id = id.clone();
		let invoker: PreviewInvoker =
			Arc::new(move |decision| registry.finalize(invoke_id.as_str(), decision));
		let pending = PendingPreview { id: id.clone(), source_tool, summary, invoker };
		let observer = self.0.observer.lock().clone();
		let Some(observer) = observer else {
			self.0.entries.lock().remove(id.as_str());
			return Err(PreviewObserverError::Unavailable);
		};
		if let Err(error) = observer(pending.clone()).await {
			self.0.entries.lock().remove(id.as_str());
			return Err(error);
		}
		Ok(pending)
	}

	/// Returns whether an exact proposal remains unresolved.
	pub fn is_pending(&self, id: &str) -> bool {
		self.0.entries.lock().contains_key(id)
	}

	/// Finalizes one exact proposal, removing it only after successful
	/// settlement.
	pub fn finalize(
		&self,
		id: &str,
		decision: PreviewDecision,
	) -> Result<PreviewOutcome, PreviewError> {
		let mut entries = self.0.entries.lock();
		let entry = entries.get_mut(id).ok_or(PreviewError::Unknown)?;
		let payload = entry.action.finalize(&decision)?;
		entries.remove(id);
		Ok(PreviewOutcome { id: Str::new(id), decision, payload })
	}
}

/// Parses the exact `dyn` resolution invocation accepted by a pending preview.
///
/// # Errors
///
/// [`PreviewError::NotResolution`] unless `do_` is exactly `invoke/resolve` or
/// `invoke/reject`; [`PreviewError::MissingReason`] when `reason` is absent,
/// non-string, or blank.
pub fn parse_resolution_invoke(input: &Value) -> Result<PreviewDecision, PreviewError> {
	let object = input.as_object().ok_or(PreviewError::NotResolution)?;
	let operation = object
		.get("do_")
		.and_then(Value::as_str)
		.unwrap_or_default();
	let reason = object
		.get("reason")
		.and_then(Value::as_str)
		.map(str::trim)
		.filter(|reason| !reason.is_empty())
		.map(Str::new)
		.ok_or(PreviewError::MissingReason)?;
	match operation {
		RESOLVE_OPERATION => Ok(PreviewDecision::Resolve { reason }),
		REJECT_OPERATION => Ok(PreviewDecision::Reject(PreviewRejection::Requested { reason })),
		_ => Err(PreviewError::NotResolution),
	}
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;

	use parking_lot::Mutex;
	use serde_json::json;

	use super::*;

	struct RecordingAction(Arc<Mutex<Vec<PreviewDecision>>>);

	impl StagedAction for RecordingAction {
		fn finalize(&mut self, decision: &PreviewDecision) -> Result<Value, PreviewError> {
			self.0.lock().push(decision.clone());
			Ok(json!({ "settled": true }))
		}
	}

	#[tokio::test]
	async fn stage_requires_observer_and_finalizes_once() {
		let registry = PreviewRegistry::new();
		let seen = Arc::new(Mutex::new(Vec::new()));
		let captured = Arc::new(Mutex::new(None));
		let captured_for_observer = Arc::clone(&captured);
		registry.bind_observer(Arc::new(move |pending| {
			*captured_for_observer.lock() = Some(pending);
			Box::pin(async { Ok(()) })
		}));
		let pending = registry
			.stage(sf!("ast_edit"), sf!("two files changed"), RecordingAction(Arc::clone(&seen)))
			.await
			.expect("proposal staged");
		assert!(registry.is_pending(pending.id.as_str()));
		let decision = parse_resolution_invoke(&json!({
			"do_": "invoke/resolve",
			"reason": "Apply the reviewed rewrite."
		}))
		.expect("valid resolution");
		let outcome = (captured.lock().take().expect("observer called").invoker)(decision.clone())
			.expect("proposal resolved");
		assert_eq!(outcome.decision, decision);
		assert_eq!(seen.lock().as_slice(), &[decision]);
		assert!(!registry.is_pending(pending.id.as_str()));
	}

	#[tokio::test]
	async fn observer_failure_rolls_back_proposal() {
		let registry = PreviewRegistry::new();
		registry.bind_observer(Arc::new(|_| Box::pin(async { Err(PreviewObserverError::Rejected) })));
		let error = registry
			.stage(
				sf!("ast_edit"),
				sf!("one file changed"),
				RecordingAction(Arc::new(Mutex::new(Vec::new()))),
			)
			.await
			.err()
			.expect("observer rejects");
		assert_eq!(error, PreviewObserverError::Rejected);
	}
	#[test]
	fn resolution_invoke_parses_paths_and_requires_reason() {
		assert!(matches!(
			parse_resolution_invoke(&json!({
				"do_": "invoke/reject",
				"reason": "Wrong file."
			})),
			Ok(PreviewDecision::Reject(PreviewRejection::Requested { .. }))
		));
		assert!(matches!(
			parse_resolution_invoke(&json!({
				"do_": "invoke/resolve",
				"reason": "  "
			})),
			Err(PreviewError::MissingReason)
		));
		assert!(matches!(
			parse_resolution_invoke(&json!({
				"do_": "invoke/format",
				"reason": "Apply."
			})),
			Err(PreviewError::NotResolution)
		));
	}
}
