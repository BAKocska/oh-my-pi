//! Daemon-scoped discovery refresh coordination and catalog publication.

use std::{
	collections::BTreeSet,
	sync::Arc,
	time::{Duration, Instant},
};

use omp_core::{Str, sf};
use omp_llm_catalog::{
	CatalogOverlayBuilder, DiscoveryNormalizer, DiscoveryPollGate, DiscoveryPollKey, DiscoverySpec,
	EvidenceConfidence, ModelOverlay, ModelPatch, OverlaySource, OverlayStore, ProvenanceKind,
	ProvenanceSource, ScopedAlias,
};
use omp_llm_inference::discovery::{
	DiscoveryHttpClient, DiscoveryProbe, DiscoveryStore, DiscoveryStoreError, ProbeError,
	ProviderDiscoveryState, ProviderLifecycle,
};
use tokio_util::sync::CancellationToken;

/// One daemon-wide discovery coordinator shared by every attached session.
pub struct DiscoveryRuntime {
	gate:     DiscoveryPollGate,
	cache:    Arc<DiscoveryStore>,
	overlays: Arc<OverlayStore>,
	disabled: BTreeSet<omp_llm_catalog::ProviderId>,
}

impl DiscoveryRuntime {
	/// Creates a coordinator with explicit disabled-provider precedence.
	pub fn new(
		cache: Arc<DiscoveryStore>,
		overlays: Arc<OverlayStore>,
		disabled: impl IntoIterator<Item = omp_llm_catalog::ProviderId>,
	) -> Self {
		Self {
			gate: DiscoveryPollGate::default(),
			cache,
			overlays,
			disabled: disabled.into_iter().collect(),
		}
	}

	/// Reports picker/call eligibility. Explicit disable is the only discovery
	/// state that erases a configured declaration; missing or failed discovery
	/// remains selectable.
	pub fn provider_selectable(&self, provider: &omp_llm_catalog::ProviderId<str>) -> bool {
		!self.disabled.contains(provider)
	}

	/// Runs one due probe, writes its complete SQLite generation, and atomically
	/// publishes a credential-blind discovery overlay.
	pub async fn refresh(
		&self,
		key: DiscoveryPollKey,
		spec: &DiscoverySpec,
		probe: &DiscoveryProbe,
		normalizer: &DiscoveryNormalizer,
		client: &dyn DiscoveryHttpClient,
		now: Instant,
		now_ms: u64,
		ttl: Duration,
		cancellation: CancellationToken,
	) -> Result<RefreshOutcome, DiscoveryRuntimeError> {
		if self.disabled.contains(&key.provider) {
			return Ok(RefreshOutcome::Disabled);
		}
		if !self.gate.claim_interval(key.clone(), spec, now) {
			return Ok(RefreshOutcome::NotDue);
		}
		self.cache.set_lifecycle(&ProviderLifecycle {
			provider:       key.provider.clone(),
			state:          ProviderDiscoveryState::Probing,
			error_code:     None,
			observed_at_ms: now_ms,
			retry_at_ms:    None,
		})?;
		let rows = match probe.probe(client, cancellation).await {
			Ok(rows) => rows,
			Err(error) => {
				self.gate.release(&key);
				self.cache.set_lifecycle(&ProviderLifecycle {
					provider:       key.provider,
					state:          ProviderDiscoveryState::Failed,
					error_code:     Some(probe_error_code(error)),
					observed_at_ms: now_ms,
					retry_at_ms:    Some(now_ms.saturating_add(5_000)),
				})?;
				return Err(DiscoveryRuntimeError::Probe(error));
			},
		};
		let normalized = normalizer
			.normalize_batch(&rows)
			.map_err(DiscoveryRuntimeError::Normalize)?;
		let source = ProvenanceSource {
			kind:           ProvenanceKind::Discovered,
			origin:         sf!("discovery:{}", key.provider),
			revision:       None,
			confidence:     EvidenceConfidence::Verified,
			observed_at_ms: Some(now_ms),
		};
		let mut builder = CatalogOverlayBuilder::new(source);
		for item in normalized {
			let selector =
				omp_llm_catalog::ExactSelector::new(item.provider.clone(), item.model.key.clone());
			builder = builder.with_model(ModelOverlay {
				selector,
				added: Some(item.model),
				patch: ModelPatch::default(),
			});
			builder = builder.with_aliases(
				item
					.aliases
					.into_vec()
					.into_iter()
					.map(|definition| ScopedAlias { provider: item.provider.clone(), definition }),
			);
		}
		self.cache.publish(&key.provider, &rows, now_ms, ttl)?;
		self
			.overlays
			.replace(OverlaySource::Discovery, builder.build());
		Ok(RefreshOutcome::Published { models: rows.len() })
	}
}

fn probe_error_code(error: ProbeError) -> Str {
	match error {
		ProbeError::Timeout => Str::new_static("timeout"),
		ProbeError::Cancelled => Str::new_static("cancelled"),
		ProbeError::Transport => Str::new_static("transport"),
		ProbeError::Protocol => Str::new_static("protocol"),
	}
}

/// Result of a refresh scheduling attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefreshOutcome {
	/// Explicit disabled-provider policy won.
	Disabled,
	/// Another session/process-local caller owns the interval.
	NotDue,
	/// A complete generation was published.
	Published {
		/// Number of normalized model rows.
		models: usize,
	},
}

/// Discovery orchestration failure.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryRuntimeError {
	/// Endpoint probing failed.
	#[error(transparent)]
	Probe(#[from] ProbeError),
	/// Discovery normalization failed.
	#[error(transparent)]
	Normalize(#[from] omp_llm_catalog::DiscoveryError),
	/// SQLite publication failed.
	#[error(transparent)]
	Store(#[from] DiscoveryStoreError),
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn explicit_disable_is_authoritative_but_failure_is_not() {
		let directory = tempfile::tempdir().expect("directory");
		let disabled = omp_llm_catalog::ProviderId::from("disabled");
		let runtime = DiscoveryRuntime::new(
			Arc::new(DiscoveryStore::open(&directory.path().join("models.db")).expect("store")),
			Arc::new(OverlayStore::default()),
			[disabled.clone()],
		);
		assert!(!runtime.provider_selectable(&disabled));
		assert!(runtime.provider_selectable(omp_llm_catalog::ProviderId::from_ref("offline")));
	}
}
