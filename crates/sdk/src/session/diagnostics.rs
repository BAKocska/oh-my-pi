//! Typed, credential-free diagnostics produced during session construction.

use std::sync::Arc;

use omp_core::Str;
use omp_docserver::lsp_registry::LspBindingHandle;
use omp_llm_inference::transport::http::PreconnectLaunch;
use parking_lot::RwLock;

/// Why one model fallback candidate may be selected at call time.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum ModelCandidateState {
	/// The candidate has a concrete catalog model and route plan.
	Catalog,
	/// The configured exact selector is intentionally retained for call-time
	/// discovery failure.
	ConfiguredUndiscoverable,
}

/// One credential-blind candidate visible to an embedder before dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelFallbackDiagnostic {
	/// Zero-based dispatch position.
	pub ordinal:  u32,
	/// Stable selector retained by the fallback planner.
	pub selector: Str,
	/// Whether this is the initially preferred candidate or a fallback.
	pub fallback: bool,
	/// Catalog/discovery state at construction time.
	pub state:    ModelCandidateState,
}

/// Effective thinking selection after applying the session ceiling.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ThinkingDiagnostic {
	/// Requested thinking selector, when the selected model supplied one.
	pub requested: Option<Str>,
	/// Effective thinking selector after clamping.
	pub effective: Option<Str>,
	/// Whether the requested selector was reduced by the ceiling.
	pub clamped:   bool,
}

/// Effective service-tier selection retained for request construction.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ServiceTierDiagnostic {
	/// Caller-requested semantic service tier.
	pub requested: Option<Str>,
	/// Tier retained after session policy application.
	pub effective: Option<Str>,
	/// Whether the caller request was changed by policy.
	pub clamped:   bool,
}

/// Construction and first-dispatch latency facts that never contain prompt
/// bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchDiagnostic {
	/// Credential-free host-preconnect scheduling outcome.
	pub preconnect:        PreconnectLaunch,
	/// Milliseconds from construction start to the first observed provider
	/// event.
	pub first_dispatch_ms: Option<u64>,
}

/// Host-visible language-server warmup state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum LspWarmupStatus {
	/// The binding is known but starts lazily on first use.
	#[default]
	Available,
	/// Startup is currently running.
	Starting,
	/// The selected server lane is ready.
	Ready,
	/// Startup failed; the binding may be retried by its document authority.
	Failed,
}

/// Opaque generation-fenced language-server binding.
///
/// The process handle, server transport, and mutable registry remain owned by
/// `omp-docserver`; embedders receive only stable identity and warmup state.
#[derive(Clone)]
pub struct LspSessionBinding {
	name:   Str,
	handle: LspBindingHandle,
	status: Arc<RwLock<LspWarmupStatus>>,
}

impl LspSessionBinding {
	/// Wraps a registry-issued generation-fenced binding.
	#[must_use]
	pub fn new(name: impl Into<Str>, handle: LspBindingHandle, status: LspWarmupStatus) -> Self {
		Self { name: name.into(), handle, status: Arc::new(RwLock::new(status)) }
	}

	/// Returns the host-visible server name.
	#[must_use]
	pub fn name(&self) -> &str {
		self.name.as_str()
	}

	/// Returns the stable registry-local binding identity.
	#[must_use]
	pub fn binding_id(&self) -> u64 {
		self.handle.binding_id().get()
	}

	/// Returns the latest warmup state.
	#[must_use]
	pub fn status(&self) -> LspWarmupStatus {
		*self.status.read()
	}

	/// Publishes a warmup transition without exposing the mutable docserver
	/// registry.
	pub fn set_status(&self, status: LspWarmupStatus) {
		*self.status.write() = status;
	}
}

impl std::fmt::Debug for LspSessionBinding {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter
			.debug_struct("LspSessionBinding")
			.field("name", &self.name)
			.field("binding_id", &self.binding_id())
			.field("status", &self.status())
			.finish()
	}
}

/// Complete typed construction diagnostics retained by a session handle.
#[derive(Clone, Debug)]
pub struct SessionDiagnostics {
	pub(crate) models:       Box<[ModelFallbackDiagnostic]>,
	pub(crate) thinking:     ThinkingDiagnostic,
	pub(crate) service_tier: ServiceTierDiagnostic,
	pub(crate) launch:       Arc<RwLock<LaunchDiagnostic>>,
	pub(crate) lsp:          Box<[LspSessionBinding]>,
}

impl SessionDiagnostics {
	/// Returns the ordered primary and fallback candidate diagnostics.
	#[must_use]
	pub fn models(&self) -> &[ModelFallbackDiagnostic] {
		&self.models
	}

	/// Returns the effective thinking clamp.
	#[must_use]
	pub const fn thinking(&self) -> &ThinkingDiagnostic {
		&self.thinking
	}

	/// Returns the effective service-tier clamp.
	#[must_use]
	pub const fn service_tier(&self) -> &ServiceTierDiagnostic {
		&self.service_tier
	}

	/// Returns the latest launch/preconnect facts.
	#[must_use]
	pub fn launch(&self) -> LaunchDiagnostic {
		self.launch.read().clone()
	}

	/// Returns opaque language-server bindings and their live warmup states.
	#[must_use]
	pub fn lsp_bindings(&self) -> &[LspSessionBinding] {
		&self.lsp
	}

	pub(crate) fn record_first_dispatch(&self, elapsed_ms: u64) {
		let mut launch = self.launch.write();
		if launch.first_dispatch_ms.is_none() {
			launch.first_dispatch_ms = Some(elapsed_ms);
		}
	}
}
