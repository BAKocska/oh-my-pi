//! Typed callback seams for embedded sessions.
//!
//! Callbacks can contribute prompt patches, context projection operations,
//! opaque credential leases, and read-only events. They cannot replace provider
//! message arrays or observe secret material after lease construction.

use std::{pin::Pin, sync::Arc, time::Duration};

use futures::Future;
use omp_agent::{
	AgentEvent, ContextView, EventBus, PatchOp, PromptError, PromptPatchSet, WorkspaceInput,
};
pub use omp_core::SecretString;
use omp_core::Str;
use omp_inference::auth::{AuthRejection, CredentialSource};
pub use omp_inference::{
	AccountId, PrincipalId,
	auth::{CredentialError, CredentialLease, CredentialNeed, LeaseMeta},
};
use thiserror::Error;
use url::Url;

/// Rejected context callback output.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ContextPatchError {
	/// A committed patch must advance to a nonzero derived-IR revision.
	#[error("context patch derived IR revision must be nonzero")]
	InvalidRevision,
	/// Synthetic context bytes exceed the per-snapshot expansion ceiling.
	#[error("context patch expansion {expansion} bytes exceeds budget {budget} bytes")]
	BudgetExceeded {
		/// Maximum accepted callback bytes.
		budget:    usize,
		/// Requested callback bytes.
		expansion: usize,
	},
}

/// Context projection callback output tied to one immutable snapshot.
#[derive(Clone, Debug)]
pub struct ContextPatchCommit {
	base_snapshot_rev:   u64,
	derived_ir_revision: u32,
	patches:             Box<[PatchOp]>,
}

impl ContextPatchCommit {
	/// Default maximum synthetic context bytes per snapshot.
	pub const DEFAULT_MAX_BYTE_EXPANSION: usize = 64 * 1024;

	/// Validates one stable-id context patch commit.
	pub fn new(
		base_snapshot_rev: u64,
		derived_ir_revision: u32,
		patches: Vec<PatchOp>,
		max_byte_expansion: usize,
	) -> Result<Self, ContextPatchError> {
		if derived_ir_revision == 0 {
			return Err(ContextPatchError::InvalidRevision);
		}
		let expansion = patches.iter().fold(0usize, |total, patch| {
			total.saturating_add(match patch {
				PatchOp::Replace { text, .. } | PatchOp::Insert { text, .. } => text.len(),
				PatchOp::Prune { .. } | PatchOp::DropParts { .. } | PatchOp::Reorder { .. } => 0,
			})
		});
		if expansion > max_byte_expansion {
			return Err(ContextPatchError::BudgetExceeded { budget: max_byte_expansion, expansion });
		}
		Ok(Self { base_snapshot_rev, derived_ir_revision, patches: patches.into_boxed_slice() })
	}

	/// Returns the immutable snapshot revision observed by the callback.
	pub const fn base_snapshot_rev(&self) -> u64 {
		self.base_snapshot_rev
	}

	/// Returns the journaled derived-IR revision.
	pub const fn derived_ir_revision(&self) -> u32 {
		self.derived_ir_revision
	}

	/// Returns stable-id projection operations.
	pub fn patches(&self) -> &[PatchOp] {
		&self.patches
	}
}

/// Invalid provider-request tuning.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum RequestTuningError {
	/// Authorization, cookie, and proxy-authorization headers belong to the
	/// credential lease authority.
	#[error("request tuning contains a credential-bearing header")]
	SensitiveHeader,
	/// Header syntax is not a lowercase public HTTP token.
	#[error("request tuning contains an invalid public header name")]
	InvalidHeader,
	/// A public header value contains a line break.
	#[error("request tuning contains an invalid public header value")]
	InvalidHeaderValue,
	/// Sampling temperature is non-finite or outside the provider-neutral range.
	#[error("request tuning temperature must be finite and between 0 and 2")]
	InvalidTemperature,
	/// Generated-token ceiling is zero.
	#[error("request tuning max tokens must be greater than zero")]
	InvalidMaxTokens,
	/// Stop strings and public headers exceed the bounded tuning budget.
	#[error("request tuning exceeds the 16 KiB public metadata budget")]
	BudgetExceeded,
}

/// Provider-request tuning that remains independent of wire codecs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RequestTuning {
	temperature:    Option<f32>,
	max_tokens:     Option<u32>,
	stop_sequences: Box<[Str]>,
	public_headers: Box<[(Str, Str)]>,
}

impl RequestTuning {
	/// Validates typed request tuning before installation.
	pub fn new(
		temperature: Option<f32>,
		max_tokens: Option<u32>,
		stop_sequences: Vec<Str>,
		public_headers: Vec<(Str, Str)>,
	) -> Result<Self, RequestTuningError> {
		if temperature.is_some_and(|value| !value.is_finite() || !(0.0..=2.0).contains(&value)) {
			return Err(RequestTuningError::InvalidTemperature);
		}
		if max_tokens == Some(0) {
			return Err(RequestTuningError::InvalidMaxTokens);
		}
		let mut bytes = stop_sequences
			.iter()
			.fold(0usize, |total, stop| total.saturating_add(stop.len()));
		for (name, value) in &public_headers {
			if matches!(name.as_str(), "authorization" | "cookie" | "proxy-authorization") {
				return Err(RequestTuningError::SensitiveHeader);
			}
			if name.is_empty()
				|| !name
					.bytes()
					.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
			{
				return Err(RequestTuningError::InvalidHeader);
			}
			if value.bytes().any(|byte| matches!(byte, b'\r' | b'\n')) {
				return Err(RequestTuningError::InvalidHeaderValue);
			}
			bytes = bytes.saturating_add(name.len()).saturating_add(value.len());
		}
		if bytes > 16 * 1024 {
			return Err(RequestTuningError::BudgetExceeded);
		}
		Ok(Self {
			temperature,
			max_tokens,
			stop_sequences: stop_sequences.into_boxed_slice(),
			public_headers: public_headers.into_boxed_slice(),
		})
	}

	/// Returns the sampling-temperature override.
	pub const fn temperature(&self) -> Option<f32> {
		self.temperature
	}

	/// Returns the generated-token ceiling.
	pub const fn max_tokens(&self) -> Option<u32> {
		self.max_tokens
	}

	/// Returns ordered stop strings.
	pub fn stop_sequences(&self) -> &[Str] {
		&self.stop_sequences
	}

	/// Returns sanitized public headers.
	pub fn public_headers(&self) -> &[(Str, Str)] {
		&self.public_headers
	}
}

/// Non-secret request facts visible to typed tuning callbacks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequestTuningInput {
	/// Canonical provider identity.
	pub provider: Str,
	/// Canonical model identity.
	pub model:    Str,
	/// Zero-based dispatch attempt.
	pub attempt:  u32,
}

/// Typed provider-request tuning callback.
pub type RequestTuningCallback =
	Arc<dyn Fn(&RequestTuningInput) -> RequestTuning + Send + Sync + 'static>;

/// Non-secret facts supplied to an SDK credential callback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialRequest {
	/// Inference-owned credential requirements, containing only opaque
	/// specification and affinity identities.
	pub need:       CredentialNeed,
	/// Process-local session identity.
	pub session_id: Str,
}

/// Boxed credential callback future.
///
/// Credential resolution is a cold boundary dominated by external secret or
/// OAuth I/O. The single allocation is never on token or event paths.
pub type CredentialFuture =
	Pin<Box<dyn Future<Output = Result<CredentialLease, CredentialError>> + Send + 'static>>;

/// Inference-owned opaque credential resolver.
pub type CredentialCallback =
	Arc<dyn Fn(CredentialRequest) -> CredentialFuture + Send + Sync + 'static>;

/// Adapter that installs an SDK callback at the inference credential-source
/// boundary without exposing credential stores or secret accessors.
pub struct SdkCredentialSource {
	session_id: Str,
	callback:   CredentialCallback,
}

impl SdkCredentialSource {
	/// Creates a process-local credential source for one session.
	pub const fn new(session_id: Str, callback: CredentialCallback) -> Self {
		Self { session_id, callback }
	}
}

impl CredentialSource for SdkCredentialSource {
	fn lease(
		&self,
		need: CredentialNeed,
	) -> futures::future::BoxFuture<'_, Result<CredentialLease, CredentialError>> {
		(self.callback)(CredentialRequest { need, session_id: self.session_id.clone() })
	}

	fn reject<'a>(
		&'a self,
		_lease: &'a CredentialLease,
		_evidence: AuthRejection,
	) -> futures::future::BoxFuture<'a, Result<(), CredentialError>> {
		Box::pin(std::future::ready(Ok(())))
	}
}

/// Deterministic system-prompt callback.
///
/// The assembler renders callback sources twice against the same immutable
/// workspace and rejects drift. The returned patch set has already enforced
/// its byte-expansion ceiling.
pub type SystemPromptCallback =
	Arc<dyn Fn(&WorkspaceInput) -> Result<PromptPatchSet, PromptError> + Send + Sync + 'static>;

/// Stable-id context projection callback.
pub type ContextPatchHandler = Arc<
	dyn Fn(&ContextView) -> Result<ContextPatchCommit, ContextPatchError> + Send + Sync + 'static,
>;

/// Read-only agent event subscriber.
pub type EventCallback = Arc<dyn Fn(&AgentEvent) + Send + Sync + 'static>;

/// First provider dispatch notification.
pub type FirstDispatchCallback = Arc<dyn Fn(Duration) + Send + Sync + 'static>;

/// Non-secret usage-reserve facts requiring a host decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageConfirmationRequest {
	/// Candidate provider identity.
	pub provider:        Str,
	/// Candidate model identity.
	pub model:           Str,
	/// Configured reserve percentage.
	pub reserve_percent: u8,
}

/// Host decision for a usage-reserve candidate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum UsageConfirmationDecision {
	/// Continue with the selected account and model.
	Continue,
	/// Skip to the next authenticated fallback candidate.
	UseFallback,
}

/// Cold host-confirmation future.
pub type UsageConfirmationFuture =
	Pin<Box<dyn Future<Output = UsageConfirmationDecision> + Send + 'static>>;

/// Deferred usage-reserve confirmation authority.
pub type UsageConfirmationCallback =
	Arc<dyn Fn(UsageConfirmationRequest) -> UsageConfirmationFuture + Send + Sync + 'static>;

/// Immutable UI context update supplied by a host embedder.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UiContextUpdate {
	/// Host-defined focus or surface identifier.
	pub surface:     Option<Str>,
	/// Whether interactive prompts can currently be presented.
	pub interactive: bool,
}

/// UI context subscriber.
pub type UiContextCallback = Arc<dyn Fn(&UiContextUpdate) + Send + Sync + 'static>;

/// Result of resolving a host-owned local protocol URL.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProtocolResolution {
	/// Canonical resolved URL.
	pub url:        Url,
	/// Optional media type supplied by the host.
	pub media_type: Option<Str>,
}

/// Resolver for a declared host-local URL scheme.
pub type LocalProtocolResolver =
	Arc<dyn Fn(&Url) -> Option<ProtocolResolution> + Send + Sync + 'static>;

/// Callback collection installed by [`crate::SessionBuilder`].
#[derive(Clone, Default)]
pub struct CallbackSet {
	/// Provider-system-prompt patches.
	pub system_prompt:      Option<SystemPromptCallback>,
	/// Optional title-system-prompt patches.
	pub title_prompt:       Option<SystemPromptCallback>,
	/// Provider-facing context projection.
	pub context:            Option<ContextPatchHandler>,
	/// Opaque inference credential resolution.
	pub credential:         Option<CredentialCallback>,
	/// Typed provider-request tuning.
	pub request_tuning:     Option<RequestTuningCallback>,
	/// Read-only event subscribers.
	pub events:             Vec<EventCallback>,
	/// First-dispatch notification.
	pub first_dispatch:     Option<FirstDispatchCallback>,
	/// Deferred usage-reserve confirmation.
	pub usage_confirmation: Option<UsageConfirmationCallback>,
	/// UI context subscriber.
	pub ui_context:         Option<UiContextCallback>,
	/// Declared host-local protocol resolvers.
	pub local_protocols:    Vec<(Str, LocalProtocolResolver)>,
	events_bus:             EventBus,
}

impl CallbackSet {
	/// Returns the handle-owned typed event fan-out.
	pub const fn events_bus(&self) -> &EventBus {
		&self.events_bus
	}
}
