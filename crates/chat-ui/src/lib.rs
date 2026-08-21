//! Host-agnostic immediate-mode chat scene and matching overlays.
//!
//! The crate owns presentation state only. A host forwards [`Intent`] values
//! to its backend and applies [`BackendEvent`] values to [`Chat`].

#![forbid(unsafe_code)]

pub mod actions;
pub mod ask;
pub mod completion;
pub mod gradient;
pub mod host;
mod overlays;
pub mod palette;
pub mod picker;
pub mod provider_picker;
pub mod queue;
pub mod scene;
pub mod sidebar;
pub mod welcome;

pub mod slots;
use std::time::Instant;

pub use gradient::{EditorGradient, EditorHighlight, GradientStop};
use omp_core::Str;
pub use omp_tui::components::Attachment;
pub use overlays::{ListPicker, ListRow, OverlayPanel, PromptEvent, PromptOverlay, panel_divider};
pub use palette::{CommandPalette, PaletteAction, PaletteEntry, PaletteEvent};
pub use picker::{ModelPicker, PickerEvent};
pub use provider_picker::ProviderPicker;
pub use scene::{Chat, ChatKey, RenderedFrame};
pub use sidebar::Sidebar;
pub use welcome::{Welcome, WelcomeEvent};

/// One model shown by the model picker.
#[derive(Clone, Debug)]
pub struct ModelRow {
	/// Stable backend model key.
	pub key:         Str,
	/// Human-readable model name.
	pub name:        Str,
	/// Stable provider identifier used to resolve its packaged logo.
	pub provider_id: Str,
	/// Human-readable provider name.
	pub provider:    Str,
	/// Context-window size in tokens, when known.
	pub context:     Option<u64>,
	/// Input price in dollars per million tokens, when known.
	pub input_mtok:  Option<f64>,
	/// Output price in dollars per million tokens, when known.
	pub output_mtok: Option<f64>,
}

/// One resumable session shown by a list picker.
#[derive(Clone, Debug)]
pub struct SessionRow {
	/// Stable session identifier.
	pub id:     Str,
	/// Primary display label.
	pub label:  Str,
	/// Secondary display detail.
	pub detail: Str,
}
/// One live node projected from the core-owned `AgentTree` roster.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentRow {
	/// Stable agent identity.
	pub id:     Str,
	/// User-facing agent name.
	pub name:   Str,
	/// Parent identity, absent for a root.
	pub parent: Option<Str>,
	/// Hierarchy depth.
	pub depth:  u16,
	/// Allocation-free lifecycle status snapshot.
	pub status: Str,
	/// Currently executing tool, when known.
	pub tool:   Option<Str>,
	/// Token consumption, when known.
	pub tokens: Option<u64>,
}

/// Core-owned transcript frame category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TranscriptFrameKind {
	/// Context compaction summary.
	Compaction,
	/// Session branch boundary.
	Branch,
	/// Session handoff boundary.
	Handoff,
	/// Prompt-cache invalidation.
	CacheBreak,
	/// Automatic context recovery.
	Recovery,
	/// Turn-ending error.
	Error,
}

/// One backend-authored transcript frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptFrame {
	/// Semantic frame category.
	pub kind:   TranscriptFrameKind,
	/// Compact frame heading.
	pub title:  Str,
	/// Optional explanatory body.
	pub detail: Option<Str>,
}

/// Optional repository facts for the status line.
#[derive(Clone, Debug)]
pub struct GitFacts {
	/// Current branch name.
	pub branch: Str,
	/// Number of dirty paths.
	pub dirty:  u32,
	/// Number of staged paths.
	pub staged: u32,
}

/// Background compaction state rendered by status surfaces.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CompactionSpeculationStatus {
	/// No background compaction exists.
	#[default]
	Idle,
	/// A detached snapshot is being compacted.
	Running,
	/// A speculative summary is ready for the threshold boundary.
	Armed,
}

/// Fixed-capacity activity bands rendered by the `/live` status sparkline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActivityWaveform {
	bands: [u8; 24],
	len:   u8,
}

impl ActivityWaveform {
	/// Creates an empty activity history.
	#[must_use]
	pub const fn new() -> Self {
		Self { bands: [0; 24], len: 0 }
	}

	/// Appends one normalized activity band, dropping the oldest when full.
	pub fn push(&mut self, band: u8) {
		let band = band.min(4);
		let len = usize::from(self.len);
		if len < self.bands.len() {
			self.bands[len] = band;
			self.len = self.len.saturating_add(1);
		} else {
			self.bands.copy_within(1.., 0);
			let last = self.bands.len() - 1;
			self.bands[last] = band;
		}
	}

	/// Borrows populated bands from oldest to newest.
	#[must_use]
	pub fn bands(&self) -> &[u8] {
		&self.bands[..usize::from(self.len)]
	}
}

impl Default for ActivityWaveform {
	fn default() -> Self {
		Self::new()
	}
}

/// Complete host-supplied status snapshot.
#[derive(Clone, Debug, Default)]
pub struct StatusFacts {
	/// Model label shown in the status line.
	pub model:                  Str,
	/// Whether the primary model uses subscription billing.
	pub model_subscription:     bool,
	/// Advisor model label, when an advisor is configured or active.
	pub advisor_model:          Option<Str>,
	/// Whether the advisor model uses subscription billing.
	pub advisor_subscription:   bool,
	/// Whether a backend turn is active.
	pub working:                bool,
	/// Wall-clock start of the active turn, when available.
	pub turn_started:           Option<Instant>,
	/// Context tokens currently in use.
	pub context_tokens:         u64,
	/// Model context window, when known.
	pub context_window:         Option<u64>,
	/// Background speculative-compaction lifecycle.
	pub compaction_speculation: CompactionSpeculationStatus,
	/// Accumulated cost in billionths of a dollar.
	pub cost_nanos:             u64,
	/// Accumulated advisor-model cost in billionths of a dollar.
	pub advisor_cost_nanos:     u64,
	/// Number of queued user submissions.
	pub queued:                 usize,
	/// Number of active background jobs.
	pub jobs:                   usize,
	/// Current retry attempt.
	pub attempt:                u32,
	/// Number of dropped backend events.
	pub dropped:                u64,
	/// Repository facts, omitted when unavailable.
	pub git:                    Option<GitFacts>,
	/// `/live` firehose activity history, absent when live display is disabled.
	pub live_activity:          Option<ActivityWaveform>,
}

/// How a composer submission interacts with an active turn.
///
/// Idle backends treat both modes as a plain submission; the distinction
/// only matters while a turn is running.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitMode {
	/// Enter: steer the active turn by delivering the message immediately.
	Steer,
	/// Alt+Enter: queue the message as a follow-up after the active turn.
	FollowUp,
}

/// Outbound intent for the host to forward to its backend.
#[derive(Clone)]
pub enum Intent {
	/// Submit composer text and staged attachments.
	Submit {
		/// User-authored composer text.
		text:        String,
		/// Attachments staged with the submission.
		attachments: Vec<Attachment>,
		/// Active-turn delivery discipline for this submission.
		mode:        SubmitMode,
	},
	/// Abort the active turn.
	Abort,
	/// Ask the backend for rewind targets.
	RewindRequest,
	/// Rewind the durable transcript to an event.
	Rewind {
		/// Event to keep as the new live-chain tail.
		event: u64,
	},
	/// Switch the active model.
	SwitchModel(Str),
	/// Start login, optionally for a specific provider.
	Login(Option<Str>),
	/// Answer the active authentication prompt.
	AuthAnswer {
		/// Unmasked value entered by the user.
		value: String,
	},
	/// Cancel the active authentication prompt.
	AuthCancel,
	/// Resume a session, or request the session picker when absent.
	Resume(Option<Str>),
	/// Start a fresh session.
	NewSession,
	/// Show help.
	Help,
	/// Quit the host.
	Quit,
}

/// One user-message target offered by history rewind.
#[derive(Clone, Debug)]
pub struct RewindTargetRow {
	/// Durable event index to keep.
	pub event: u64,
	/// Full user message text.
	pub text:  Str,
}

/// Inbound mutation emitted by a backend.
#[derive(Clone)]
pub enum BackendEvent {
	/// Replay a user message from durable history.
	UserReplayed {
		/// Message text.
		text:  Str,
		/// Display labels for replayed attachments.
		chips: Vec<Str>,
	},
	/// Return a user prompt that was dropped before the first turn committed.
	PromptDropped {
		/// Prompt text exactly as submitted.
		text:        Str,
		/// Attachments submitted with the prompt.
		attachments: Vec<Attachment>,
	},
	/// Begin a streamed assistant message.
	AssistantBegin {
		/// Stable message identifier.
		id: Str,
	},
	/// Append text to a streamed assistant message.
	AssistantDelta {
		/// Stable message identifier.
		id:   Str,
		/// Delta text.
		text: Str,
	},
	/// Finish a streamed assistant message.
	AssistantEnd {
		/// Stable message identifier.
		id: Str,
	},
	/// Begin a streamed tool invocation.
	ToolStarted {
		/// Stable tool-call identifier.
		id:    Str,
		/// Backend tool name.
		name:  Str,
		/// Human-readable tool title.
		title: Str,
	},
	/// Append output to a live tool invocation.
	ToolOutput {
		/// Stable tool-call identifier.
		id:    Str,
		/// Output chunk.
		chunk: Str,
	},
	/// Replace the retained structured view of a live tool invocation.
	ToolView {
		/// Stable tool-call identifier.
		id:   Str,
		/// Renderer-produced TML or structured generic fallback text.
		view: Str,
	},
	/// Attach an inline image to a live tool invocation.
	///
	/// `source` is a filesystem path to persisted PNG bytes; the scene
	/// renders it inline in the committed card on graphics-capable
	/// terminals and ignores undecodable sources.
	ToolImage {
		/// Stable tool-call identifier.
		id:     Str,
		/// Path to the persisted PNG payload.
		source: Str,
	},
	/// Finish a tool invocation.
	ToolFinished {
		/// Stable tool-call identifier.
		id:   Str,
		/// Whether the invocation succeeded.
		ok:   bool,
		/// Renderer-produced TML or structured generic fallback text.
		view: Str,
	},
	/// Append an in-place compaction summary divider.
	Compacted {
		/// Full summary used when no short preview title was recorded.
		summary:       Str,
		/// Optional short preview title.
		title:         Option<Str>,
		/// Ladder method that produced the entry.
		method:        Option<Str>,
		/// Context tokens before the rewrite.
		tokens_before: u64,
		/// Estimated context tokens after the rewrite.
		tokens_after:  Option<u64>,
	},
	/// Append a semantic transcript boundary or error frame.
	TranscriptFrame(TranscriptFrame),
	/// Replace the live `AgentTree` roster projection.
	AgentRoster(Vec<AgentRow>),
	/// Request the core-owned settings seam.
	OpenSettings,
	/// Request the live agent hierarchy overlay.
	OpenAgentTree,
	/// Request the pause overlay.
	Pause,
	/// Request a host-level fresh session transition.
	NewSessionRequested,
	/// Append an informational notice.
	Notice(Str),
	/// Append an error notice.
	Error(Str),
	/// Replace status facts.
	Status(StatusFacts),
	/// Replace the session title.
	SessionTitle(Str),
	/// Open the model picker with these rows and current selection.
	OpenModelPicker {
		/// Available models.
		rows:    Vec<ModelRow>,
		/// Current model index.
		current: usize,
	},
	/// Silently refresh cached model rows and the current selection.
	ModelsUpdated {
		/// Available models.
		rows:    Vec<ModelRow>,
		/// Current model index.
		current: usize,
	},
	/// Replace resumable sessions.
	Sessions(Vec<SessionRow>),
	/// Replace provider-login choices; each row's `id` is the provider key.
	LoginProviders(Vec<SessionRow>),
	/// Replace rewind choices.
	RewindTargets(Vec<RewindTargetRow>),
	/// Open a backend authentication prompt.
	AuthPrompt {
		/// Prompt title or message.
		message: Str,
		/// Whether input must be masked.
		masked:  bool,
	},
	/// Close the active authentication prompt.
	AuthPromptClose,
	/// Remove all transcript history.
	HistoryCleared,
	/// Acknowledge the active submission.
	Ack {
		/// Whether the submission ended by interruption.
		interrupted: bool,
	},
}
