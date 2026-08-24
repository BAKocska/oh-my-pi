//! Shared staged-proposal lifecycle for regime-mediated tools.

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
pub const PROPOSAL_PENDING_NOTICE: &str = "A staged proposal is pending. Finalize it with dyn \
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
pub enum ProposalRejection {
	/// The model explicitly rejected the proposal.
	Requested {
		/// One-sentence rejection reason.
		reason: Str,
	},
	/// The proposal regime reached its finite step bound.
	RegimeLimitReached,
}

/// A final decision for one staged proposal.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalDecision {
	/// Apply the proposal.
	Resolve {
		/// One-sentence application reason.
		reason: Str,
	},
	/// Discard the proposal.
	Reject(ProposalRejection),
}

/// Successful terminal result from a staged action.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ProposalOutcome {
	/// Unique proposal identity.
	pub id:       Str,
	/// Applied or rejected decision.
	pub decision: ProposalDecision,
	/// Action-specific result payload.
	pub payload:  Value,
}

/// Failure to resolve a staged proposal.
#[derive(Debug, thiserror::Error)]
pub enum ProposalError {
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
	Action(#[from] ProposalActionError),
}
/// Typed failure produced while finalizing a staged action.
#[derive(Debug, thiserror::Error)]
pub enum ProposalActionError {
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
pub enum ProposalActivationError {
	/// No active agent owns the proposal activation hook.
	#[error("staged proposal cannot be announced because the agent owner is unavailable")]
	Unavailable,
	/// The active agent rejected regime activation.
	#[error("staged proposal regime activation was rejected")]
	Rejected,
}

/// Action retained until an explicit resolution arrives.
pub trait StagedProposalAction: Send + 'static {
	/// Applies or rejects this action. An error retains it for a corrected
	/// retry.
	fn finalize(&mut self, decision: &ProposalDecision) -> Result<Value, ProposalError>;
}

/// Synchronous resolver installed for the active proposal regime.
pub type ProposalResolver =
	Arc<dyn Fn(ProposalDecision) -> Result<ProposalOutcome, ProposalError> + Send + Sync + 'static>;

/// Metadata and resolver for one newly staged proposal.
#[derive(Clone)]
pub struct StagedProposal {
	/// Unique proposal identity.
	pub id:          Str,
	/// Tool that produced the proposal.
	pub source_tool: Str,
	/// Bounded model-facing proposal summary.
	pub summary:     Str,
	/// Single-settlement resolver.
	pub resolver:    ProposalResolver,
}

/// Future returned by the late-bound agent observer.
pub type ActivationObserverFuture =
	Pin<Box<dyn Future<Output = Result<(), ProposalActivationError>> + Send + 'static>>;
/// Callback that registers the resolver and starts its proposal regime.
pub type ActivationObserver =
	Arc<dyn Fn(StagedProposal) -> ActivationObserverFuture + Send + Sync + 'static>;

struct Entry {
	action: Box<dyn StagedProposalAction>,
}

struct Inner {
	entries:  Mutex<BTreeMap<Str, Entry>>,
	observer: Mutex<Option<ActivationObserver>>,
	next_id:  AtomicU64,
}

/// Shared proposal registry used by every staging-capable tool in one
/// environment.
#[derive(Clone)]
pub struct StagedProposalRegistry(Arc<Inner>);

impl Default for StagedProposalRegistry {
	fn default() -> Self {
		Self(Arc::new(Inner {
			entries:  Mutex::new(BTreeMap::new()),
			observer: Mutex::new(None),
			next_id:  AtomicU64::new(1),
		}))
	}
}

impl StagedProposalRegistry {
	/// Creates an empty registry with no active agent observer.
	pub fn new() -> Self {
		Self::default()
	}

	/// Replaces the observer used for subsequently staged proposals.
	pub fn install_activation_observer(&self, observer: ActivationObserver) {
		*self.0.observer.lock() = Some(observer);
	}

	/// Removes the active observer without discarding already staged proposals.
	pub fn remove_activation_observer(&self) {
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
		action: impl StagedProposalAction,
	) -> Result<StagedProposal, ProposalActivationError> {
		let sequence = self.0.next_id.fetch_add(1, Ordering::Relaxed);
		let id = sf!("pending-action:{}:{sequence}", source_tool.as_str());
		self
			.0
			.entries
			.lock()
			.insert(id.clone(), Entry { action: Box::new(action) });
		let registry = self.clone();
		let invoke_id = id.clone();
		let resolver: ProposalResolver =
			Arc::new(move |decision| registry.finalize(invoke_id.as_str(), decision));
		let pending = StagedProposal { id: id.clone(), source_tool, summary, resolver };
		let observer = self.0.observer.lock().clone();
		let Some(observer) = observer else {
			self.0.entries.lock().remove(id.as_str());
			return Err(ProposalActivationError::Unavailable);
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
		decision: ProposalDecision,
	) -> Result<ProposalOutcome, ProposalError> {
		let mut entries = self.0.entries.lock();
		let entry = entries.get_mut(id).ok_or(ProposalError::Unknown)?;
		let payload = entry.action.finalize(&decision)?;
		entries.remove(id);
		Ok(ProposalOutcome { id: Str::new(id), decision, payload })
	}
}

/// Parses the exact `dyn` resolution invocation accepted by a staged proposal.
///
/// # Errors
///
/// [`ProposalError::NotResolution`] unless `do_` is exactly `invoke/resolve` or
/// `invoke/reject`; [`ProposalError::MissingReason`] when `reason` is absent,
/// non-string, or blank.
pub fn parse_resolution_invoke(input: &Value) -> Result<ProposalDecision, ProposalError> {
	let object = input.as_object().ok_or(ProposalError::NotResolution)?;
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
		.ok_or(ProposalError::MissingReason)?;
	match operation {
		RESOLVE_OPERATION => Ok(ProposalDecision::Resolve { reason }),
		REJECT_OPERATION => Ok(ProposalDecision::Reject(ProposalRejection::Requested { reason })),
		_ => Err(ProposalError::NotResolution),
	}
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;

	use parking_lot::Mutex;
	use serde_json::json;

	use super::*;

	struct RecordingAction(Arc<Mutex<Vec<ProposalDecision>>>);

	impl StagedProposalAction for RecordingAction {
		fn finalize(&mut self, decision: &ProposalDecision) -> Result<Value, ProposalError> {
			self.0.lock().push(decision.clone());
			Ok(json!({ "settled": true }))
		}
	}

	#[tokio::test]
	async fn stage_requires_observer_and_finalizes_once() {
		let registry = StagedProposalRegistry::new();
		let seen = Arc::new(Mutex::new(Vec::new()));
		let captured = Arc::new(Mutex::new(None));
		let captured_for_observer = Arc::clone(&captured);
		registry.install_activation_observer(Arc::new(move |pending| {
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
		let outcome = (captured.lock().take().expect("observer called").resolver)(decision.clone())
			.expect("proposal resolved");
		assert_eq!(outcome.decision, decision);
		assert_eq!(seen.lock().as_slice(), &[decision]);
		assert!(!registry.is_pending(pending.id.as_str()));
	}

	#[tokio::test]
	async fn observer_failure_rolls_back_proposal() {
		let registry = StagedProposalRegistry::new();
		registry.install_activation_observer(Arc::new(|_| {
			Box::pin(async { Err(ProposalActivationError::Rejected) })
		}));
		let error = registry
			.stage(
				sf!("ast_edit"),
				sf!("one file changed"),
				RecordingAction(Arc::new(Mutex::new(Vec::new()))),
			)
			.await
			.err()
			.expect("observer rejects");
		assert_eq!(error, ProposalActivationError::Rejected);
	}
	#[test]
	fn resolution_invoke_parses_paths_and_requires_reason() {
		assert!(matches!(
			parse_resolution_invoke(&json!({
				"do_": "invoke/reject",
				"reason": "Wrong file."
			})),
			Ok(ProposalDecision::Reject(ProposalRejection::Requested { .. }))
		));
		assert!(matches!(
			parse_resolution_invoke(&json!({
				"do_": "invoke/resolve",
				"reason": "  "
			})),
			Err(ProposalError::MissingReason)
		));
		assert!(matches!(
			parse_resolution_invoke(&json!({
				"do_": "invoke/format",
				"reason": "Apply."
			})),
			Err(ProposalError::NotResolution)
		));
	}
	#[test]
	fn regime_limit_rejection_has_stable_wire_name() {
		assert_eq!(
			serde_json::to_value(ProposalRejection::RegimeLimitReached).expect("rejection serializes"),
			json!("regime_limit_reached")
		);
	}
}
