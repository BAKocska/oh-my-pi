//! Compile-time prompt assets used by live auxiliary prompt consumers.
//!
//! Core system policy remains in the typed slot renderers. This catalog owns
//! only immutable auxiliary text and declares the slot stability each consumer
//! must preserve.

use std::fmt::Write as _;

use crate::{PromptMode, SlotClass, SlotId};

/// Semantic family of an immutable prompt asset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromptAssetFamily {
	/// Configured communication personality.
	Personality,
	/// Agent lifecycle continuation text.
	Lifecycle,
	/// Parent or user steering text.
	Steering,
	/// Provider-loop recovery text.
	Recovery,
	/// Auxiliary title completion text.
	Title,
	/// Built-in agent definition.
	Agent,
	/// Built-in execution mode.
	Mode,
}

/// Prompt behavior activated by an explicit user keyword.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromptKeywordBehavior {
	/// Requests the extended-reasoning presentation and policy path.
	ExtendedThinking,
	/// Requests multi-agent orchestration.
	Orchestration,
	/// Requests the guided workflow policy.
	Workflow,
}

/// One canonical user keyword and the prompt behavior it activates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PromptKeyword {
	/// ASCII keyword matched case-insensitively at word boundaries.
	pub text:     &'static str,
	/// Policy behavior selected by the keyword.
	pub behavior: PromptKeywordBehavior,
}

/// Canonical user-keyword policy consumed by prompt and presentation layers.
pub const PROMPT_KEYWORDS: &[PromptKeyword] = &[
	PromptKeyword { text: "ultrathink", behavior: PromptKeywordBehavior::ExtendedThinking },
	PromptKeyword { text: "orchestrate", behavior: PromptKeywordBehavior::Orchestration },
	PromptKeyword { text: "workflowz", behavior: PromptKeywordBehavior::Workflow },
];

/// Typed identity of a compile-time prompt asset.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum PromptAssetId {
	/// Default terse personality.
	PersonalityDefault,
	/// Friendly collaborative personality.
	PersonalityFriendly,
	/// Pragmatic senior-engineer personality.
	PersonalityPragmatic,
	/// Automatic continuation reminder.
	AutoContinue,
	/// User steering interjection.
	UserInterjection,
	/// Parent-agent IRC steering interjection.
	ParentIrc,
	/// Empty-stop retry recovery.
	EmptyStopRetry,
	/// Unexpected-stop retry recovery.
	UnexpectedStopRetry,
	/// Repeated tool-call loop redirect.
	ToolCallLoopRedirect,
	/// Repeated thinking loop redirect.
	ThinkingLoopRedirect,
	/// Gemini tool-call reminder.
	GeminiToolCallReminder,
	/// Auxiliary title-generation system prompt.
	TitleSystem,
	/// Built-in read-only scout definition.
	AgentScout,
	/// Built-in reviewer definition.
	AgentReviewer,
	/// Built-in security reviewer definition.
	AgentSecurityReviewer,
	/// Built-in general task definition.
	AgentTask,
	/// Built-in librarian definition.
	AgentLibrarian,
	/// Built-in designer definition.
	AgentDesigner,
	/// Built-in repository initializer definition.
	AgentInit,
	/// Read-only planning mode.
	ModePlan,
	/// Plan-validation prewalk mode.
	ModePrewalk,
	/// Durable lesson extraction mode.
	ModeAutolearn,
	/// Autonomous goal mode.
	ModeGoal,
	/// Multi-agent orchestration mode.
	ModeVibe,
	/// Durable memory pipeline mode.
	ModeMemoryPipeline,
	/// Read-only advisor mode.
	ModeAdvisor,
	/// Autonomous experiment/research mode.
	ModeAutoresearch,
	/// Security audit mode.
	ModeSecurityAudit,
	/// Reproducible benchmark mode.
	ModeBench,
	/// Change review mode.
	ModeReview,
	/// Generated-residue cleanse mode.
	ModeCleanse,
	/// Context compression mode.
	ModeCompress,
	/// Live collaborator coordination mode.
	ModeLiveCollab,
}

/// Immutable asset bytes and their declared prompt placement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PromptAsset {
	/// Typed identity.
	pub id:      PromptAssetId,
	/// Semantic asset family.
	pub family:  PromptAssetFamily,
	/// Destination slot when injected into a provider prompt.
	pub slot:    SlotId,
	/// Stability band required of the consumer.
	pub class:   SlotClass,
	/// Immutable UTF-8 source bytes.
	pub content: &'static str,
}

macro_rules! asset {
	($id:ident, $family:ident, $slot:ident, $class:ident, $path:literal) => {
		PromptAsset {
			id:      PromptAssetId::$id,
			family:  PromptAssetFamily::$family,
			slot:    SlotId::$slot,
			class:   SlotClass::$class,
			content: include_str!($path),
		}
	};
}

const ASSETS: [PromptAsset; 33] = [
	asset!(PersonalityDefault, Personality, Runtime, Stable, "../prompts/personality/default.md"),
	asset!(PersonalityFriendly, Personality, Runtime, Stable, "../prompts/personality/friendly.md"),
	asset!(
		PersonalityPragmatic,
		Personality,
		Runtime,
		Stable,
		"../prompts/personality/pragmatic.md"
	),
	asset!(AutoContinue, Lifecycle, Status, Volatile, "../prompts/lifecycle/auto-continue.md"),
	asset!(UserInterjection, Steering, Status, Volatile, "../prompts/steering/user-interjection.md"),
	asset!(ParentIrc, Steering, Status, Volatile, "../prompts/steering/parent-irc.md"),
	asset!(EmptyStopRetry, Recovery, Status, Volatile, "../prompts/recovery/empty-stop-retry.md"),
	asset!(
		UnexpectedStopRetry,
		Recovery,
		Status,
		Volatile,
		"../prompts/recovery/unexpected-stop-retry.md"
	),
	asset!(
		ToolCallLoopRedirect,
		Recovery,
		Status,
		Volatile,
		"../prompts/recovery/tool-call-loop-redirect.md"
	),
	asset!(
		ThinkingLoopRedirect,
		Recovery,
		Status,
		Volatile,
		"../prompts/recovery/thinking-loop-redirect.md"
	),
	asset!(
		GeminiToolCallReminder,
		Recovery,
		Status,
		Volatile,
		"../prompts/recovery/gemini-tool-call-reminder.md"
	),
	asset!(TitleSystem, Title, Guidance, Stable, "../prompts/title/system.md"),
	asset!(AgentScout, Agent, Role, Frozen, "../prompts/roles/scout.md"),
	asset!(AgentReviewer, Agent, Role, Frozen, "../prompts/roles/reviewer.md"),
	asset!(AgentSecurityReviewer, Agent, Role, Frozen, "../prompts/roles/security-reviewer.md"),
	asset!(AgentTask, Agent, Role, Frozen, "../prompts/roles/task.md"),
	asset!(AgentLibrarian, Agent, Role, Frozen, "../prompts/roles/librarian.md"),
	asset!(AgentDesigner, Agent, Role, Frozen, "../prompts/roles/designer.md"),
	asset!(AgentInit, Agent, Role, Frozen, "../prompts/roles/init.md"),
	asset!(ModePlan, Mode, Status, Volatile, "../prompts/modes/plan.md"),
	asset!(ModePrewalk, Mode, Status, Volatile, "../prompts/modes/prewalk.md"),
	asset!(ModeAutolearn, Mode, Status, Volatile, "../prompts/modes/autolearn.md"),
	asset!(ModeGoal, Mode, Status, Volatile, "../prompts/modes/goal.md"),
	asset!(ModeVibe, Mode, Status, Volatile, "../prompts/modes/vibe.md"),
	asset!(ModeMemoryPipeline, Mode, Status, Volatile, "../prompts/modes/memory-pipeline.md"),
	asset!(ModeAdvisor, Mode, Status, Volatile, "../prompts/modes/advisor.md"),
	asset!(ModeAutoresearch, Mode, Status, Volatile, "../prompts/modes/autoresearch.md"),
	asset!(ModeSecurityAudit, Mode, Status, Volatile, "../prompts/modes/security-audit.md"),
	asset!(ModeBench, Mode, Status, Volatile, "../prompts/modes/bench.md"),
	asset!(ModeReview, Mode, Status, Volatile, "../prompts/modes/review.md"),
	asset!(ModeCleanse, Mode, Status, Volatile, "../prompts/modes/cleanse.md"),
	asset!(ModeCompress, Mode, Status, Volatile, "../prompts/modes/compress.md"),
	asset!(ModeLiveCollab, Mode, Status, Volatile, "../prompts/modes/live-collab.md"),
];

/// Returns one immutable asset without allocation.
#[must_use]
#[inline]
pub const fn prompt_asset(id: PromptAssetId) -> &'static PromptAsset {
	&ASSETS[id as usize]
}

/// Iterates over the complete deterministic built-in catalog.
#[must_use]
#[inline]
pub fn prompt_assets() -> impl ExactSizeIterator<Item = &'static PromptAsset> + Clone {
	ASSETS.iter()
}

/// Returns the rich asset selected by a live execution mode.
#[must_use]
pub const fn mode_prompt_asset(mode: PromptMode) -> &'static PromptAsset {
	let id = match mode {
		PromptMode::Plan => PromptAssetId::ModePlan,
		PromptMode::Prewalk => PromptAssetId::ModePrewalk,
		PromptMode::Autolearn => PromptAssetId::ModeAutolearn,
		PromptMode::Goal => PromptAssetId::ModeGoal,
		PromptMode::Vibe => PromptAssetId::ModeVibe,
		PromptMode::MemoryPipeline => PromptAssetId::ModeMemoryPipeline,
		PromptMode::Advisor => PromptAssetId::ModeAdvisor,
		PromptMode::Autoresearch => PromptAssetId::ModeAutoresearch,
		PromptMode::SecurityAudit => PromptAssetId::ModeSecurityAudit,
		PromptMode::Bench => PromptAssetId::ModeBench,
		PromptMode::Review => PromptAssetId::ModeReview,
		PromptMode::Cleanse => PromptAssetId::ModeCleanse,
		PromptMode::Compress => PromptAssetId::ModeCompress,
		PromptMode::LiveCollab => PromptAssetId::ModeLiveCollab,
	};
	prompt_asset(id)
}

/// Renders the typed retry count into the immutable empty-stop template.
pub fn render_empty_stop_retry(out: &mut String, retry_count: usize, max_retries: usize) {
	const RETRY: &str = "{{retryCount}}";
	const MAX: &str = "{{maxRetries}}";
	let template = prompt_asset(PromptAssetId::EmptyStopRetry).content;
	let (before_retry, after_retry) = template
		.split_once(RETRY)
		.expect("embedded empty-stop asset contains retryCount slot");
	let (between, after_max) = after_retry
		.split_once(MAX)
		.expect("embedded empty-stop asset contains maxRetries slot");
	out.push_str(before_retry);
	write!(out, "{retry_count}").expect("writing to String cannot fail");
	out.push_str(between);
	write!(out, "{max_retries}").expect("writing to String cannot fail");
	out.push_str(after_max);
}
