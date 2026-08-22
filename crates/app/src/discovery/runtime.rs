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
	DiscoveryCacheKey, DiscoveryHttpClient, DiscoveryProbe, DiscoveryStore, DiscoveryStoreError,
	ProbeError, ProviderDiscoveryState, ProviderLifecycle,
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

	/// Hydrates exact provider/account cache namespaces without network access.
	///
	/// Callers pass the current opaque credential affinities and current route
	/// normalizers. Repeating this pass replaces the disk-cache layer, so a
	/// credential change is observed rather than hidden behind a process-wide
	/// once guard.
	pub fn hydrate_cached(
		&self,
		requests: &[CachedDiscoveryHydration],
		now_ms: u64,
	) -> Result<usize, DiscoveryRuntimeError> {
		let source = ProvenanceSource {
			kind:           ProvenanceKind::Discovered,
			origin:         sf!("discovery:disk-cache"),
			revision:       None,
			confidence:     EvidenceConfidence::Verified,
			observed_at_ms: Some(now_ms),
		};
		let mut builder = CatalogOverlayBuilder::new(source);
		let mut hydrated = 0;
		for request in requests {
			if self.disabled.contains(&request.key.provider) {
				continue;
			}
			let Some(cached) = self.cache.load_fresh(&request.key, now_ms)? else {
				continue;
			};
			let normalized = request
				.normalizer
				.normalize_batch(&cached.rows)
				.map_err(DiscoveryRuntimeError::Normalize)?;
			hydrated += normalized.len();
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
						.map(|definition| ScopedAlias {
							provider: item.provider.clone(),
							definition,
						}),
				);
			}
		}
		self
			.overlays
			.replace(OverlaySource::DiskCache, builder.build());
		Ok(hydrated)
	}

	/// Runs one due probe, writes its complete SQLite generation, and atomically
	/// publishes a credential-blind discovery overlay.
	pub async fn refresh(
		&self,
		key: DiscoveryPollKey,
		cache_key: &DiscoveryCacheKey,
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
		if cache_key.provider != key.provider {
			return Err(DiscoveryRuntimeError::CacheScopeMismatch {
				poll:  key.provider,
				cache: cache_key.provider.clone(),
			});
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
		self.cache.publish(cache_key, &rows, now_ms, ttl)?;
		self
			.overlays
			.replace(OverlaySource::Discovery, builder.build());
		Ok(RefreshOutcome::Published { models: rows.len() })
	}
}

/// One exact local-only cache hydration request.
#[derive(Clone)]
pub struct CachedDiscoveryHydration {
	/// Provider plus optional opaque credential affinity.
	pub key:        DiscoveryCacheKey,
	/// Current route-bound normalizer; current route auth/header configuration
	/// remains authoritative and is never read from SQLite.
	pub normalizer: DiscoveryNormalizer,
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
	/// Cache namespace belongs to another provider.
	#[error("discovery cache provider {cache} does not match poll provider {poll}")]
	CacheScopeMismatch {
		/// Scheduled poll provider.
		poll:  omp_llm_catalog::ProviderId,
		/// Supplied cache provider.
		cache: omp_llm_catalog::ProviderId,
	},
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

	#[test]	use omp_llm_catalog::{
		ContextStrategy, DiscoveredModel, DiscoveryDefaults, ModelAvailability, OperationBits,
		Pricing, RouteId, WireModelId, WirePolicyId,
	};

	fn row(provider: &omp_llm_catalog::ProviderId<str>, model: &str) -> DiscoveredModel {
		DiscoveredModel {
			provider:              provider.to_owned(),
			route:                 RouteId::from("configured-route"),
			wire_model:            WireModelId::from(model),
			aliases:               Box::new([]),
			display_name:          None,
			declared_class:        None,
			declared_operations:   OperationBits::empty(),
			declared_capabilities: None,
			declared_limits:       None,
			extended_context_mode: None,
			availability:          Some(ModelAvailability::Available),
			source:                Str::new_static("fixture"),
			observed_at_ms:        Some(100),
			updated_at_ms:         None,
			deprecated:            None,
		}
	}

	fn normalizer() -> DiscoveryNormalizer {
		DiscoveryNormalizer::new(DiscoveryDefaults {
			wire_policy:          WirePolicyId::from("configured-wire"),
			extended_wire_policy: None,
			context:              ContextStrategy::Replay,
			thinking:             None,
			pricing:              Pricing::default(),
		})
	}

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
	#[test]
	fn credential_cache_hydration_repeats_after_affinity_changes() {
		let directory = tempfile::tempdir().expect("directory");
		let cache =
			Arc::new(DiscoveryStore::open(&directory.path().join("models.db")).expect("store"));
		let overlays = Arc::new(OverlayStore::default());
		let runtime = DiscoveryRuntime::new(
			Arc::clone(&cache),
			Arc::clone(&overlays),
			std::iter::empty::<omp_llm_catalog::ProviderId>(),
		);
		let provider = omp_llm_catalog::ProviderId::from("opencode-go");
		let first = DiscoveryCacheKey::credential(provider.clone(), "affinity-first");
		let second = DiscoveryCacheKey::credential(provider.clone(), "affinity-second");
		cache
			.publish(&first, &[row(&provider, "first-model")], 100, Duration::from_secs(60))
			.expect("first cache");
		cache
			.publish(&second, &[row(&provider, "second-model")], 101, Duration::from_secs(60))
			.expect("second cache");
		let cached = cache
			.load_fresh(&first, 102)
			.expect("load cache")
			.expect("fresh cache");
		let restored = normalizer()
			.normalize(&cached.rows[0])
			.expect("current configured route policy reattaches");
		assert_eq!(restored.model.routes.as_ref(), [RouteId::from("configured-route")]);
		assert_eq!(restored.model.wire_policy, WirePolicyId::from("configured-wire"));

		assert_eq!(
			runtime
				.hydrate_cached(
					&[CachedDiscoveryHydration { key: first, normalizer: normalizer() }],
					102,
				)
				.expect("first hydration"),
			1
		);
		let first_generation = overlays.load().generation();
		assert_eq!(
			runtime
				.hydrate_cached(
					&[CachedDiscoveryHydration { key: second, normalizer: normalizer() }],
					102,
				)
				.expect("second hydration"),
			1
		);
		assert!(overlays.load().generation() > first_generation);
	}
}
