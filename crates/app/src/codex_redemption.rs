//! Adapts the inference-owned Codex saved-reset redemption service onto the
//! agent's [`RedemptionAuthority`] boundary.
//!
//! Tokens never cross this boundary: the agent supplies typed evidence and the
//! inference crate leases credentials internally per attempt.

use std::sync::Arc;

use omp_agent::{RedemptionAuthority, RedemptionEvidence, RedemptionFuture};
use omp_llm_inference::operation::usage::openai_codex::{CodexRedemption, CodexRedemptionReason};

/// Production redemption authority over the shared Codex service.
pub(crate) struct CodexRedemptionAuthority {
	service: Arc<CodexRedemption>,
}

impl CodexRedemptionAuthority {
	/// Wraps the inference-owned service for agent installation.
	pub(crate) const fn new(service: Arc<CodexRedemption>) -> Self {
		Self { service }
	}
}

impl RedemptionAuthority for CodexRedemptionAuthority {
	fn redeem(&self, evidence: RedemptionEvidence) -> RedemptionFuture<'_, bool> {
		Box::pin(async move {
			match evidence {
				RedemptionEvidence::Salvage { .. } => {
					self.service.redeem(CodexRedemptionReason::Salvage).await
				},
				RedemptionEvidence::Restore { .. } => {
					self.service.redeem(CodexRedemptionReason::Restore).await
				},
				RedemptionEvidence::PostCompaction { .. } => {
					self.service.history_reseeded().await;
					false
				},
			}
		})
	}

	fn reseed_history(&self) -> RedemptionFuture<'_, ()> {
		Box::pin(async move {
			self.service.history_reseeded().await;
		})
	}
}
