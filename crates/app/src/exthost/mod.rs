//! Core-owned extension-host lifecycle, CONTROL accounting, and service
//! routing.
//!
//! Process ownership remains in [`crate::envd::worker::ExtHostSupervisor`].
//! This module adds no in-process Python authority and starts no child on its
//! own; an empty manifest set is therefore completely inert.

pub mod control;
pub mod lifecycle;
pub mod quota;
pub mod services;

pub use lifecycle::{
	ActivateReason, ActivationCause, ActivationDisposition, ActivationEvent, ActivationTrigger,
	DeclarationDrift, DeclarationSet, ExtensionManifest, GenerationFence, HookDeclarationKey,
	LifecycleError, LifecycleHost, LifecycleMachine, Principal, PrincipalAuthority,
	PrincipalMismatch, RestartReason, ToolDeclarationKey,
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

pub use crate::envd::worker::{ExtHostSupervisor, HostKey};
