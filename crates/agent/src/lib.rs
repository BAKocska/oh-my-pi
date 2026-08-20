#![feature(integer_atomics)]
//! Transport-neutral foundations for durable, interruptible OMP agent loops.
//!
//! The crate composes immutable configuration snapshots, deterministic system
//! prompts, ordered interrupts, event fan-out, journal projection, tool-batch
//! supervision, detached jobs, and the live turn transport. Durable history is
//! canonical [`Item`] data; provider, application, and UI types stay outside
//! this boundary. [`Agent`] is the durable policy loop tying these foundations
//! into complete N-turn conversations.

mod batch;
pub mod control;
pub(crate) mod duplex;
mod events;
mod inproc;
mod jobs;
mod journal;
pub mod journal_kinds;
mod r#loop;
mod mailbox;
mod phases;
mod project;
mod prompt;
mod state;
mod turn;

pub use batch::{
	BatchError, BatchResult, CommittedCall, InvocationAdmission, InvocationHookBus,
	InvocationHookRequest, SpeculativeCall, ToolBatch, hook_event_mask,
};
pub use events::{AgentEvent, AgentPhase, EventBus, EventSubscription, LossyEventSubscription};
pub use inproc::{InProcTurnClient, RpcTurnClient, RpcTurnSession};
pub use jobs::{JobBoard, PendingJobs};
pub use journal::{
	AbortDisposition, Journal, JournalAuthor, JournalCustomEntry, JournalError, JournalGenerations,
	JournalOperation, JournalQuery, JournalReply, JournalRequest, JournalRequestStamp,
	PendingCustomEntry, SessionStateValue, SessionStateWatchEvent, SessionStateWatchTerminal,
	TurnInputRecord, TurnOptionsRecord, TurnReceipt, TurnStart,
};
pub use journal_kinds::{EntryKindDecl, EntryKindError, EntryKindRegistry, KindRecord, LiftHook};
pub use r#loop::{AbortHandle, Agent, AgentError, AgentRunSummary, RewindTarget};
pub use mailbox::{DrainPoint, Interrupt, InterruptClass, InterruptSource, Mailbox, MailboxSender};
pub use omp_llm_inference::TurnId;
pub use omp_proto::{
	inference::v1::{
		Accepted, ChatParams, ContextRef, ExecStatus, Executor, Invoke, InvokeCancel, InvokeComplete,
		InvokeInput, Outcome, ThreadDelta, TurnError, TurnEvent,
	},
	thread::v1::{Item, Thread},
};
pub use phases::{
	ActivateReason, HookDecision, HookPhase, InvocationPhase, LifecyclePhase, RestartReason,
};
pub use project::{
	ProjectionError, project_journal, project_thread_history, tool_result_item,
	tool_result_item_canonical_parts,
};
pub use prompt::{
	ContextFile, PromptError, PromptHash, PromptSource, RenderedPrompt, VcsIdentity, WorkspaceInput,
	WorkspacePromptSource, render_prompt,
};
pub use state::{AgentSnapshot, AgentState, RetryPolicy, RetryPolicyError};
pub use turn::{
	Error, InvokeFrame, Recovery, TurnClient, TurnInput, TurnOptions, TurnSession, empty_stop,
};
