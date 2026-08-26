//! Isolated advisor policy, bounded primary-history delivery, and emission
//! admission.

mod advise;
mod guard;
mod rules;
mod runtime;

pub use advise::{
	AdviceAdmission, AdviseFault, AdviseParams, AdvisePayload, AdviseTool, AdviseUpdate,
	AdvisorAdviceQueue, QueuedAdvice, tool as advise_tool,
};
pub use guard::{
	ADVICE_DEDUPE_LIMIT, ADVISOR_TOOL_LOOP_THRESHOLD, AdvisorEmissionGuard, AdvisorQuarantineReason,
	AdvisorSuppression, AdvisorToolLoopAction, AdvisorToolLoopGuard, GuardedAdvice,
	normalize_advice, quarantine_advisor_turn,
};
pub use rules::{
	AdvisorRoster, AdvisorRule, AdvisorRuleError, AdvisorRuleWarning, DEFAULT_ADVISOR_TOOLS,
	EvaluatedAdvisorTools, WatchdogRuleSet, evaluate_advisor_tools, merge_watchdog_rules,
	parse_watchdog_yaml, slugify_advisor_name,
};
pub use runtime::{
	ADVISOR_FINGERPRINT_CHUNK_BYTES, AdviceDelivery, AdviceSeverity, AdvisorDeliveryRouter,
	AdvisorDeltaBatch, AdvisorDeltaChunk, AdvisorDeltaSync, AdvisorHistoryDelta,
	AdvisorHistoryEntry, AdvisorRouteError, AdvisorRuntimeState, BoundedAdvisorHistory,
	DEFAULT_HISTORY_BYTE_LIMIT, DEFAULT_HISTORY_ENTRY_LIMIT, DeliveryContext, ImmuneTurnAccount,
	MAX_DELTA_COALESCE_ROUNDS, RoutedAdvice,
};
