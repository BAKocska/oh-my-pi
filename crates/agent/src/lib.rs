#![feature(integer_atomics)]
//! Transport-neutral foundations for durable, interruptible OMP agent loops.
//!
//! The crate composes immutable configuration snapshots, deterministic system
//! prompts, ordered interrupts, event fan-out, journal projection, tool-batch
//! supervision, detached jobs, and the live turn transport. Durable history is
//! canonical [`Item`] data; provider, application, and UI types stay outside
//! this boundary. [`Agent`] is the durable policy loop tying these foundations
//! into complete N-turn conversations.

#[allow(unused_imports)]
#[macro_use]
extern crate omp_core;

mod approvals;
mod batch;
mod broker;
mod compact;
pub mod context;
mod continuation;
pub mod control;
pub(crate) mod duplex;
mod events;
mod hooks;
mod inproc;
mod jobs;
mod journal;
pub mod journal_kinds;
mod r#loop;
mod mailbox;
mod oneshot;
mod phases;
mod project;
mod prompt;
mod schedule;
mod state;
mod tree;
mod turn;

pub use approvals::{
	ApprovalBook, ApprovalDecision, ApprovalGuard, ApprovalSource, ApprovalSpec, ApprovalTicket,
	TicketState,
};
pub use batch::{
	BatchError, BatchResult, CommittedCall, EXECUTION_MODE_PROP, ExecutionMode, ExecutionModeHandle,
	InvocationAdmission, InvocationHookBus, InvocationHookRequest, PLAN_YOLO_PROP,
	PREWALK_REASON_PROP, SpeculativeCall, ToolBatch, effects_mutate_environment, hook_event_mask,
};
pub use broker::{
	AgentRegistry, Broker, BrokerError, BrokerInbox, DeliveryMode, PeerMessage, Receipt,
	now_ms as broker_now_ms, peer_item,
};
pub use compact::{
	COMPACTION_RECOVERY_BAND, CancelCompaction, CompactionCoordinator, CompactionDecision,
	CompactionEvent, CompactionHysteresis, CompactionMethodOrder, CompactionReason,
	CompactionResolution, CompactionSpeculationOptions, CompactionTier, CompactionVerdict,
	ContextUsage, CustomSummary, DelegateCompaction, ItemUsage, LosslessPlan, ProjectionItem,
	RemoteCheckpoint, SNAPCOMPACT_RESERVED_TIER, SPECULATION_LEAD_FRACTION,
	SPECULATION_LEAD_MAX_TOKENS, SPECULATION_LEAD_MIN_TOKENS, SpeculationRequest, SpeculationResult,
	SpeculationSnapshot, SpeculationState, SupersededCompaction, back_project_provider_usage,
	dispatch_tier, encode_domain_verdict, plan_lossless, resolve_verdicts, speculation_lead_tokens,
};
pub use context::{
	Anchor, ContextProjection, ContextView, InheritPosition, MessageKind, MessageRef, PatchOp,
	PatchOutcome, PatchRejected, RefFlags, apply_patches, project_context,
};
pub use continuation::{
	AgentSettledEvent, Continuation, ContinuationLedger, ContinuationPolicy, ContinuationSource,
	LoopSignal, continues_loop, from_hook,
};
pub use control::{ControlError, ControlSender, RewindAck};
pub use events::{AgentEvent, AgentPhase, EventBus, EventSubscription, LossyEventSubscription};
pub use hooks::{
	AgentSettled, Composition, ContextPatch, DomainReturn, GateDecision, GateError, GateEvent,
	GateOutcome, HookDispatch, HookEvent, HookGate, HookPatch, MODIFY_ROUNDS, OBSERVE_HANDLER_CAP,
	OnFailure, ProviderFailover, SourceRef, Subscription, TransformTrail, When,
};
pub use inproc::{InProcTurnClient, RpcTurnClient, RpcTurnSession};
pub use jobs::{
	CancelOutcome, JobBoard, JobClaimError, JobError, JobSettlement, JobWatch, PendingJobs,
	SettlementLease,
};
pub use journal::{
	AbortDisposition, Compact, Journal, JournalAuthor, JournalCustomEntry, JournalError,
	JournalGenerations, JournalOperation, JournalQuery, JournalReply, JournalRequest,
	JournalRequestStamp, PendingCustomEntry, SessionStateValue, SessionStateWatchEvent,
	SessionStateWatchTerminal, TurnInputRecord, TurnOptionsRecord, TurnReceipt, TurnStart,
};
pub use journal_kinds::{EntryKindDecl, EntryKindError, EntryKindRegistry, KindRecord, LiftHook};
pub use r#loop::{AbortHandle, Agent, AgentError, AgentRunSummary, RewindTarget};
pub use mailbox::{
	DrainPoint, Interrupt, InterruptClass, InterruptSource, Mailbox, MailboxSender,
	device_availability_interrupt,
};
pub use omp_llm_inference::TurnId;
pub use omp_proto::{
	inference::v1::{
		Accepted, ChatParams, ContextRef, ExecStatus, Executor, Invoke, InvokeCancel, InvokeComplete,
		InvokeInput, Outcome, ThreadDelta, TurnError, TurnEvent,
	},
	thread::v1::{Item, Thread},
};
pub use oneshot::{
	Completion, CompletionError, CompletionRequest, resolve_completion, select_choice,
};
pub use phases::{
	ActivateReason, HookDecision, HookPhase, InvocationPhase, LifecyclePhase, RestartReason,
};
pub use project::{
	ProjectionError, project_journal, project_thread_history, tool_result_item,
	tool_result_item_canonical_parts,
};
pub use prompt::{
	BandHash, CachedContribution, ConditionalPromptEntries, ContextFile, ModePromptSource,
	PromptError, PromptHash, PromptMode, PromptOut, PromptSource, RenderedPrompt, SlotAssembler,
	SlotClass, SlotDecl, SlotId, SlotRegistration, SlotSource, VcsIdentity, VolatilePrompt,
	VolatilePromptJournal, WorkspaceInput, WorkspacePromptSource, render_prompt,
};
pub use schedule::{
	Firing, FiringOutcome, MissedRunPolicy, Schedule, ScheduleBudget, ScheduleDelivery,
	ScheduleError, ScheduleJournal, ScheduleScope, Scheduler, Trigger, UpgradePolicy, firing_key,
};
pub use state::{AgentSnapshot, AgentState, RetryPolicy, RetryPolicyError};
pub use tree::{
	AgentDefinition, AgentDefinitionError, AgentKind, AgentNode, AgentStatus, AgentTree, Budget,
	BudgetCeiling, BudgetExceeded, BudgetRemainder, DEFAULT_MAX_ADMISSION_QUEUE,
	DEFAULT_MAX_CONCURRENCY, EffectsOperation, MAX_YIELD_SCHEMA_RETRIES, SpawnPermit, SpawnPolicy,
	SpawnRefusal, Usage, YieldPayload, YieldPayloadError, YieldPayloadValidator,
	enforce_minimum_phase,
};
pub use turn::{
	Error, InvokeFrame, Recovery, TurnClient, TurnInput, TurnOptions, TurnSession, empty_stop,
};
