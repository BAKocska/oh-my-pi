//! Isolated advisor policy, bounded primary-history delivery, and emission
//! admission.

mod guard;
mod rules;
mod runtime;

pub use guard::{
	ADVICE_DEDUPE_LIMIT, AdvisorEmissionGuard, AdvisorQuarantineReason, AdvisorSuppression,
	GuardedAdvice, normalize_advice, quarantine_advisor_turn,
};
pub use rules::{
	AdvisorRoster, AdvisorRule, AdvisorRuleError, AdvisorRuleWarning, DEFAULT_ADVISOR_TOOLS,
	EvaluatedAdvisorTools, WatchdogRuleSet, evaluate_advisor_tools, merge_watchdog_rules,
	parse_watchdog_yaml, slugify_advisor_name,
};
pub use runtime::{
	AdviceDelivery, AdviceSeverity, AdvisorHistoryDelta, AdvisorHistoryEntry, AdvisorRuntimeState,
	BoundedAdvisorHistory, DEFAULT_HISTORY_BYTE_LIMIT, DEFAULT_HISTORY_ENTRY_LIMIT, DeliveryContext,
	ImmuneTurnAccount,
};
