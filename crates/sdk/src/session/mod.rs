//! Owned session options and callback-capable construction.

mod builder;
mod diagnostics;
mod handle;
mod options;

pub use builder::{SessionBlueprint, SessionBuildError, SessionBuilder, WorkspaceRootDescriptor};
pub use diagnostics::{
	LaunchDiagnostic, LspSessionBinding, LspWarmupStatus, ModelCandidateState,
	ModelFallbackDiagnostic, ServiceTierDiagnostic, SessionDiagnostics, ThinkingDiagnostic,
};
pub use handle::{
	SessionHandle, SessionHandleError, SessionIdentity, SessionLifecycle,
	SessionLifecycleSubscription, SessionRevivalError, SessionRevivalFactory, SessionRevivalFuture,
	SessionRevivalRequest, SessionRuntime,
};
pub use options::{
	AgentIdentity, DiscoveryPolicy, SessionOptions, SessionPolicies, SubsystemToggles,
	ThinkingCeiling,
};
