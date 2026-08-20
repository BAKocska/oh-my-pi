//! Core-owned extension-host lifecycle, CONTROL accounting, and service
//! routing.
//!
//! Process ownership remains in [`crate::envd::worker::ExtHostSupervisor`].
//! Children are spawned lazily at a declared surface's first reach.

pub mod cancel;
pub mod control;
pub mod dispatch;
pub mod lifecycle;
pub mod quota;
pub mod services;
pub mod spawn;
pub use cancel::{
	CANCEL_GRACE, CancelStage, CancellationError, CancellationJournal, CancellationLadder,
	CancellationOutcome, MAX_KILL_ESCALATIONS_PER_SESSION,
};
pub use dispatch::{
	CallbackConcurrency, DispatchError, DispatchPending, DispatchRequest, DispatchRouter,
	EventDeadline,
};
pub use lifecycle::{
	ActivateReason, ActivationCause, ActivationDisposition, ActivationEvent, ActivationTrigger,
	AvailabilityBatch, AvailabilitySink, DeclarationDrift, DeclarationSet, ExtensionManifest,
	GenerationFence, HookDeclarationKey, LifecycleError, LifecycleHost, LifecycleMachine, Principal,
	PrincipalAuthority, PrincipalMismatch, RegistryAvailabilitySink, RestartReason,
	ToolDeclarationKey,
};
pub use quota::{
	ChargeOutcome, ControlQuotaLedger, FairControlQueue, QuotaBehavior, QuotaError, QuotaExceeded,
	QuotaScope, QuotaSpec, QuotaStatus, ResourceReceipt,
};
pub use services::{
	PendingServiceCall, ServiceBroker, ServiceCallError, ServiceCallId, ServiceCancellation,
	ServiceConnection, ServiceDeclarationDrift, ServiceDispatch, ServiceError, ServiceKey,
	ServiceManifest, ServiceRequestMeta, ServiceResponse, ServiceRoute, ServiceTransport,
};
pub use spawn::{
	CONTROL_FD_ENV, ENV_SOCKET_ENV, EXT_HOST_ARG, HostChildLimit, HostLog, HostLogStream,
	PY_SITE_ENV, SpawnError, SpawnSpec, SpawnedHost, run_ext_host_entry,
};

pub use crate::envd::worker::{ExtHostSupervisor, HostKey};
