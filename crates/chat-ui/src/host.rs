//! Interactive chat state and its terminal host for the host-agnostic
//! immediate-mode chat scene.

use std::{
	collections::VecDeque,
	future,
	io::{self, Write},
	time::{Duration, Instant},
};

use flume::{Receiver, Sender};
use omp_core::{Str, sf};
use omp_executor::Executor;
use omp_tui::{
	AltScreenUse, Chord, CursorStyle, DebugOp, DebugQuery, Frame, HistoryReplay, Icon, InputEvent,
	Key, Keymap, Layer, Mods, Mouse, MouseReport, Notification, Pasted, Renderer, Size, Terminal,
	TerminalEvent, TerminalOptions, TtyOut, UiContext, Urgency, detect,
	paste::{self, Clipboard, ClipboardRead},
};
use smallvec::SmallVec;

use crate::{
	AgentHub, AgentHubEvent, ApprovalAction, ApprovalTicketView, BackendEvent, Chat, ChatKey,
	CommandPalette, ExtensionInspector, ExtensionInspectorEvent, GitIntent, GitWorkbench,
	GitWorkbenchEvent, HistoryInspector, HistoryInspectorEvent, ImageOverlay, ImageOverlayEvent,
	Intent, ListPicker, ListRow, ModelPicker, ModelRow, PaletteAction, PaletteEntry, PaletteEvent,
	PickerEvent, PromptEvent, PromptOverlay, ProviderPicker, PtyEvent, PtyOverlay, RewindTargetRow,
	SelectionPurpose, SessionRow, SettingChange, SettingRow, Sidebar, SubmitMode, Welcome,
	WelcomeEvent,
	approval::{ApprovalEvent, ApprovalOverlay},
	ask::{self, AskDialog, AskDialogEvent, AskRequest},
	autoqa::{AutoQaConsent, ConsentRequest, Decision},
	modes::{GuidedGoalEvent, GuidedGoalInterview},
	plan_review::{PlanReviewEvent, PlanReviewOverlay, PlanReviewSection},
	selection_overlay::{SelectionEvent, SelectionOverlay},
	settings_overlay::{SettingsEvent, SettingsOverlay},
};

const RESIZE_SETTLE: Duration = Duration::from_millis(120);
const DOUBLE_ESC: Duration = Duration::from_millis(500);
const PASTE_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest interval before a retained host observes a background event.
const BACKEND_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Answers the chat-owned half of the terminal debug protocol.
fn answer_debug(query: DebugQuery, chat: &mut Chat) {
	if query.op != DebugOp::Slots {
		return;
	}
	let slots: Vec<_> = chat
		.slots_mut()
		.debug_mounts()
		.into_iter()
		.map(|mount| {
			serde_json::json!({
				"key": mount.key,
				"placement": mount.placement,
				"rect": {
					"x": mount.rect.x,
					"y": mount.rect.y,
					"width": mount.rect.width,
					"height": mount.rect.height,
				},
			})
		})
		.collect();
	omp_tui::respond_debug_query(query.id, serde_json::json!({ "ok": true, "slots": slots }));
}

mod paste_read {
	use tokio::sync::oneshot::Receiver;

	use super::{Clipboard, ClipboardRead, Instant, PASTE_READ_TIMEOUT, paste};

	pub(super) struct PasteRead {
		pub(super) clipboard:  Receiver<Option<Clipboard>>,
		pub(super) scope:      ClipboardRead,
		pub(super) abandon_at: Instant,
	}

	impl PasteRead {
		pub(super) fn start(scope: ClipboardRead) -> Self {
			Self {
				clipboard: paste::spawn_clipboard_read(scope),
				scope,
				abandon_at: Instant::now() + PASTE_READ_TIMEOUT,
			}
		}
	}
}
use paste_read::PasteRead;

/// Interactive chat-host lifecycle controls.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostOptions {
	/// Whether to show the welcome session index before entering chat.
	pub welcome:                bool,
	/// Whether session-changing actions return to the caller for reconstruction.
	pub exit_on_session_change: bool,
	/// Notify when a non-interrupted turn settles.
	pub completion_notify:      bool,
	/// Notify and retain an attention title for backend errors.
	pub error_notify:           bool,
	/// Permit generated terminal-title escape sequences.
	pub title_enabled:          bool,
	/// How a settled terminal width change refreshes retired scrollback rows.
	pub resize_scrollback:      ResizeScrollback,
}

impl Default for HostOptions {
	fn default() -> Self {
		Self {
			welcome:                true,
			exit_on_session_change: true,
			completion_notify:      true,
			error_notify:           true,
			title_enabled:          true,
			resize_scrollback:      ResizeScrollback::Rebuild,
		}
	}
}
/// How a settled terminal width change refreshes transcript rows already
/// retired into native scrollback (written at the old width).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ResizeScrollback {
	/// Replay the logical transcript at the new width below retained history
	/// in one buffered transaction.
	Append,
	/// Erase native scrollback and replay one current-width transcript in the
	/// same buffered transaction.
	#[default]
	Rebuild,
	/// Repaint only the mutable viewport; scrollback keeps its old width.
	Preserve,
}

const DOUBLE_LEFT_MIN: Duration = Duration::from_millis(40);
const DOUBLE_LEFT_MAX: Duration = Duration::from_millis(500);

/// Reason an interactive chat host returned to its production caller.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostExit {
	/// The user or backend closed the host.
	Quit,
	/// Rebuild the agent around this session.
	Resume(Str),
	/// Build a fresh agent session.
	NewSession,
	/// Terminal modes were restored so the process group can be suspended.
	Suspend,
}

/// Interactive host result with the final unsent composer draft.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostOutcome {
	/// Why the host returned.
	pub exit:  HostExit,
	/// Unsent composer text at the exact exit boundary.
	pub draft: Str,
}

/// Runs the example-style terminal host, handling session choices in-band.
#[expect(
	clippy::future_not_send,
	reason = "chat components remain confined to their terminal event-loop thread"
)]
pub async fn run(
	executor: Executor,
	chat: Chat,
	ctx: UiContext,
	events: Receiver<BackendEvent>,
	intents: Sender<Intent>,
) -> io::Result<()> {
	run_with_options(executor, chat, ctx, events, intents, HostOptions {
		welcome:                true,
		exit_on_session_change: false,
		completion_notify:      true,
		error_notify:           true,
		title_enabled:          true,
		resize_scrollback:      ResizeScrollback::Rebuild,
	})
	.await
	.map(|_| ())
}

/// Runs the terminal host with explicit boot and session-handoff behavior.
#[expect(
	clippy::future_not_send,
	reason = "chat components remain confined to their terminal event-loop thread"
)]
pub async fn run_with_options(
	executor: Executor,
	chat: Chat,
	ctx: UiContext,
	events: Receiver<BackendEvent>,
	intents: Sender<Intent>,
	options: HostOptions,
) -> io::Result<HostExit> {
	run_with_draft(executor, chat, ctx, events, intents, options, Str::default())
		.await
		.map(|outcome| outcome.exit)
}

/// Runs the terminal host with an owner-supplied draft and returns the final
/// unsent composer text without persisting it in the UI crate.
#[expect(
	clippy::future_not_send,
	reason = "chat components remain confined to their terminal event-loop thread"
)]
pub async fn run_with_draft(
	executor: Executor,
	mut chat: Chat,
	ctx: UiContext,
	events: Receiver<BackendEvent>,
	intents: Sender<Intent>,
	options: HostOptions,
	initial_draft: Str,
) -> io::Result<HostOutcome> {
	chat.set_composer_text(initial_draft.as_str());
	let caps = detect();
	let mut terminal = Terminal::enter(
		executor.clone(),
		TerminalOptions::new(caps).cursor_style(CursorStyle::BlinkingBar),
	)?;
	let mut renderer = Renderer::new(TtyOut::new()?);
	renderer.apply_caps(&caps)?;
	let result = run_with_terminal(
		&executor,
		&mut terminal,
		&mut renderer,
		chat,
		&ctx,
		&events,
		&intents,
		options,
	)
	.await;
	let scrub = terminal.leave_alt().and_then(|()| renderer.clear_layers());
	match (result, scrub) {
		(Err(error), _) | (Ok(_), Err(error)) => Err(error),
		(Ok(exit), Ok(())) => Ok(exit),
	}
}

#[expect(
	clippy::future_not_send,
	reason = "chat components remain confined to their terminal event-loop thread"
)]
async fn run_with_terminal(
	executor: &Executor,
	terminal: &mut Terminal,
	renderer: &mut Renderer<TtyOut>,
	mut chat: Chat,
	ctx: &UiContext,
	events: &Receiver<BackendEvent>,
	intents: &Sender<Intent>,
	options: HostOptions,
) -> io::Result<HostOutcome> {
	let mut viewport = terminal.size()?;
	let mut models = Vec::new();
	let mut current_model = 0;
	if options.welcome {
		match run_welcome(
			terminal,
			renderer,
			ctx,
			&mut viewport,
			&mut chat,
			events,
			intents,
			&mut models,
			&mut current_model,
			options.exit_on_session_change,
		)
		.await?
		{
			WelcomeOutcome::Proceed => terminal.leave_alt()?,
			WelcomeOutcome::Exit(exit) => {
				return Ok(HostOutcome { exit, draft: Str::from(chat.composer_text()) });
			},
		}
	}
	run_chat(
		executor,
		terminal,
		renderer,
		ctx,
		viewport,
		chat,
		models,
		current_model,
		events,
		intents,
		options,
	)
	.await
}

enum WelcomeOutcome {
	Proceed,
	Exit(HostExit),
}

#[expect(
	clippy::future_not_send,
	reason = "chat components remain confined to their terminal event-loop thread"
)]
async fn run_welcome(
	terminal: &mut Terminal,
	renderer: &mut Renderer<TtyOut>,
	ctx: &UiContext,
	viewport: &mut Size,
	chat: &mut Chat,
	events: &Receiver<BackendEvent>,
	intents: &Sender<Intent>,
	models: &mut Vec<ModelRow>,
	current_model: &mut usize,
	exit_on_session_change: bool,
) -> io::Result<WelcomeOutcome> {
	let mut alt_enter = terminal.stage_alt_enter(AltScreenUse::Interactive);
	let mut welcome = Welcome::new(ctx, Vec::new());
	let started = Instant::now();
	loop {
		if let Some(size) = terminal.take_resize()? {
			*viewport = size;
		}
		renderer.repaint(
			alt_enter.take().as_deref().unwrap_or(""),
			welcome.render(*viewport, started.elapsed()).clone(),
			viewport.height,
			&[],
		)?;
		tokio::select! {
					event = terminal.next() => match event? {
						TerminalEvent::Resize => if let Some(size) = terminal.take_resize()? { *viewport = size; },
						TerminalEvent::Debug(query) => answer_debug(query, chat),
						TerminalEvent::Effect(effect) => {
							let _ = chat.slots_mut().apply_serialized(effect);
						},
						TerminalEvent::Closed => return Ok(WelcomeOutcome::Exit(HostExit::Quit)),
						TerminalEvent::Input(event) => {
							let Some(event) = user_event(terminal, renderer, event)? else { continue };
							match event {
								InputEvent::Key(key) => match welcome.handle_key(key) {
									WelcomeEvent::Consumed => {},
									WelcomeEvent::NewSession => {
										send(intents, Intent::NewSession);
										return Ok(if exit_on_session_change {
											WelcomeOutcome::Exit(HostExit::NewSession)
										} else {
											WelcomeOutcome::Proceed
										});
									},
									WelcomeEvent::Resume(id) => {
										send(intents, Intent::Resume(Some(id.clone())));
										return Ok(if exit_on_session_change {
											WelcomeOutcome::Exit(HostExit::Resume(id))
										} else {
											WelcomeOutcome::Proceed
										});
									},
									WelcomeEvent::Quit => {
										send(intents, Intent::Quit);
										return Ok(WelcomeOutcome::Exit(HostExit::Quit));
									},
								},
								InputEvent::Mouse(report) if matches!(report.kind, Mouse::Move | Mouse::Drag) => {
									welcome.point_at(report.col, report.row);
								},
								InputEvent::Mouse(_) | InputEvent::Paste(_) | InputEvent::Focus(_)
								| InputEvent::Response(_) => {},
							}
						},
					},
			backend = events.recv_async() => match backend {
						Ok(BackendEvent::Sessions(rows)) => welcome.set_sessions(rows),
										Ok(BackendEvent::ModelDownloadProgress(progress)) => {
							welcome.set_download_progress(progress, started.elapsed());
						},
		Ok(BackendEvent::OpenModelPicker { rows, current }
							| BackendEvent::ModelsUpdated { rows, current }) => {
							*models = rows;
							*current_model = current.min(models.len().saturating_sub(1));
						},
						Ok(event) => { let _ = chat.apply_backend_event(event); },
						Err(_) => return Ok(WelcomeOutcome::Exit(HostExit::Quit)),
					},
				}
	}
}

struct ChatHost {
	chat:                    Chat,
	session_title:           Str,
	sidebar:                 Sidebar,
	overlay:                 Option<Overlay>,
	models:                  Vec<ModelRow>,
	current_model:           usize,
	last_esc:                Option<Instant>,
	last_left:               Option<Instant>,
	left_taps:               u8,
	pending_approvals:       usize,
	approval_queue:          VecDeque<ApprovalTicketView>,
	autoqa_queue:            VecDeque<ConsentRequest>,
	suppress_history_replay: bool,
	saved_git_keymap:        Option<Keymap>,
}

impl ChatHost {
	fn new(
		mut chat: Chat,
		ctx: &UiContext,
		viewport: Size,
		models: Vec<ModelRow>,
		current_model: usize,
		sidebar_open: bool,
	) -> Self {
		let status = chat.status();
		let sidebar = if sidebar_open {
			Sidebar::new(&status, ctx)
		} else {
			Sidebar::new_hidden(&status, ctx)
		};
		chat.set_right_inset(sidebar.reserved(viewport));
		Self {
			chat,
			session_title: sf!("omp"),
			sidebar,
			overlay: None,
			models,
			current_model,
			last_esc: None,
			last_left: None,
			left_taps: 0,
			pending_approvals: 0,
			approval_queue: VecDeque::new(),
			autoqa_queue: VecDeque::new(),
			suppress_history_replay: false,
			saved_git_keymap: None,
		}
	}

	fn open_models(&mut self, ctx: &UiContext) {
		if !self.models.is_empty() {
			self.overlay = Some(Overlay::Models(ModelPicker::open(
				&self.models,
				self.current_model.min(self.models.len() - 1),
				ctx,
			)));
		}
	}

	fn cycle_model(&mut self, backward: bool, intents: &Sender<Intent>) {
		if self.models.is_empty() {
			return;
		}
		self.current_model = if backward {
			(self.current_model + self.models.len() - 1) % self.models.len()
		} else {
			(self.current_model + 1) % self.models.len()
		};
		send(intents, Intent::SwitchModel(self.models[self.current_model].key.clone()));
	}

	fn left_double_tap(&mut self) -> bool {
		let now = Instant::now();
		let Some(last) = self.last_left.replace(now) else {
			self.left_taps = 1;
			return false;
		};
		let gap = now.duration_since(last);
		if gap >= DOUBLE_LEFT_MAX {
			self.left_taps = 1;
			return false;
		}
		self.left_taps = self.left_taps.saturating_add(1);
		if self.left_taps == 2 && gap >= DOUBLE_LEFT_MIN {
			self.last_left = None;
			self.left_taps = 0;
			return true;
		}
		false
	}
}

/// Host-neutral retained chat state for native application hosts.
///
/// It owns the same overlays, input routing, backend protocol, and draft
/// boundary as the terminal host while exposing retained frames directly.
pub struct RetainedChat {
	host:                   ChatHost,
	ctx:                    UiContext,
	events:                 Receiver<BackendEvent>,
	intents:                Sender<Intent>,
	exit_on_session_change: bool,
	ask_binding:            ask::AskBinding,
	viewport:               Size,
	pending_exit:           Option<HostExit>,
	pending_clipboard:      Option<Str>,
}

/// One retained chat paint for a non-terminal host.
pub struct RetainedChatFrame<'a> {
	/// Exactly viewport-sized live presentation grid.
	pub frame:       &'a Frame,
	/// Live viewport dimensions in cells.
	pub viewport:    Size,
	/// Viewport rows reserved for the composer.
	pub editor_rows: u16,
	/// Viewport-anchored overlays in paint order.
	pub layers:      SmallVec<Layer<'a>, 4>,
}

/// A host operation requested by retained chat state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RetainedChatEffect {
	/// No host action is needed.
	Ignored,
	/// State changed and the host should repaint.
	Consumed,
	/// Close the active chat surface with this lifecycle result.
	Quit(HostExit),
	/// Read a matching system clipboard representation.
	Clipboard(ClipboardRead),
	/// Copy text through the host's clipboard authority.
	SetClipboard(Str),
}

impl RetainedChat {
	/// Creates an active chat surface after session selection.
	pub fn new(
		mut chat: Chat,
		ctx: UiContext,
		events: Receiver<BackendEvent>,
		intents: Sender<Intent>,
		options: HostOptions,
		initial_draft: Str,
	) -> Self {
		chat.set_composer_text(initial_draft.as_str());
		let viewport = Size::new(0, 0);
		Self {
			host: ChatHost::new(chat, &ctx, viewport, Vec::new(), 0, false),
			ctx,
			events,
			intents,
			exit_on_session_change: options.exit_on_session_change,
			ask_binding: ask::bind(),
			viewport,
			pending_exit: None,
			pending_clipboard: None,
		}
	}

	/// Updates the fixed cell viewport.
	pub fn resize(&mut self, viewport: Size, _settled: bool) {
		self.viewport = viewport;
		self
			.host
			.chat
			.set_right_inset(self.host.sidebar.reserved(viewport));
		send_pty_resize(&mut self.host, viewport, &self.intents);
	}

	/// Pumps backend and dialog events, returning any required host operation.
	pub fn poll(&mut self) -> RetainedChatEffect {
		let changed = self.drain();
		if let Some(exit) = self.pending_exit.take() {
			return RetainedChatEffect::Quit(exit);
		}
		if let Some(text) = self.pending_clipboard.take() {
			return RetainedChatEffect::SetClipboard(text);
		}
		if changed {
			RetainedChatEffect::Consumed
		} else {
			RetainedChatEffect::Ignored
		}
	}

	/// Renders the active chat and its viewport overlays.
	pub fn render(&mut self) -> RetainedChatFrame<'_> {
		let viewport = self.viewport;
		let editor_rows = self.host.chat.composer_rows();
		let rendered = self.host.chat.render(viewport);
		let mut layers = rail_layers(&mut self.host.sidebar, viewport);
		if let Some(overlay) = self.host.overlay.as_mut() {
			layers.push(overlay.layer(viewport));
		}
		RetainedChatFrame { frame: rendered.frame, viewport, editor_rows, layers }
	}

	/// Routes one keyboard event through the active overlay or chat composer.
	pub fn key(&mut self, key: Key) -> RetainedChatEffect {
		if let Some(overlay) = self.host.overlay.as_mut() {
			if key == Key::Ctrl('c') {
				send(&self.intents, Intent::Quit);
				return RetainedChatEffect::Quit(HostExit::Quit);
			}
			let event = overlay.handle_key(key);
			return self.apply_overlay(event);
		}
		if key == Key::Ctrl('b') {
			self.host.sidebar.toggle();
			self
				.host
				.chat
				.set_right_inset(self.host.sidebar.reserved(self.viewport));
			return RetainedChatEffect::Consumed;
		}
		if key == Key::RestoreQueue {
			send(&self.intents, Intent::Dequeue);
			return RetainedChatEffect::Consumed;
		}
		if key == Key::CyclePrevious {
			self.host.cycle_model(true, &self.intents);
			return RetainedChatEffect::Consumed;
		}
		if key == Key::Ctrl('p') {
			self.host.cycle_model(false, &self.intents);
			return RetainedChatEffect::Consumed;
		}
		if key == Key::BackTab {
			send(&self.intents, Intent::CycleThinking);
			return RetainedChatEffect::Consumed;
		}
		if key == Key::Ctrl('t') {
			let _ = self.host.chat.handle_key(key);
			send(&self.intents, Intent::ToggleThinking);
			return RetainedChatEffect::Consumed;
		}
		if key == Key::Alt('r') {
			send(&self.intents, Intent::Retry);
			return RetainedChatEffect::Consumed;
		}
		if key == Key::PlanToggle {
			send(&self.intents, Intent::TogglePlan);
			return RetainedChatEffect::Consumed;
		}
		if key == Key::CtrlAlt('l') {
			send(&self.intents, Intent::ToggleLive);
			return RetainedChatEffect::Consumed;
		}
		if key == Key::CtrlAlt('s') {
			send(&self.intents, Intent::ToggleStt);
			return RetainedChatEffect::Consumed;
		}
		if key == Key::Alt('h') {
			send(&self.intents, Intent::InspectHistory);
			return RetainedChatEffect::Consumed;
		}
		if key == Key::Ctrl('k') {
			self.host.overlay =
				Some(Overlay::Palette(CommandPalette::open(palette_entries(), &self.ctx)));
			return RetainedChatEffect::Consumed;
		}
		if self.host.sidebar.focused() {
			if key == Key::Ctrl('c') {
				send(&self.intents, Intent::Quit);
				return RetainedChatEffect::Quit(HostExit::Quit);
			}
			self.host.sidebar.handle_key(key);
			return RetainedChatEffect::Consumed;
		}
		if key == Key::Alt('a') || key == Key::Ctrl('s') {
			open_agents(&mut self.host, &self.ctx);
			return RetainedChatEffect::Consumed;
		}
		if key == Key::Alt('m') || key == Key::Alt('p') {
			self.host.open_models(&self.ctx);
			return RetainedChatEffect::Consumed;
		}
		if let Some(scope) = ClipboardRead::for_key(key) {
			return RetainedChatEffect::Clipboard(scope);
		}
		if key == Key::Esc && self.host.chat.is_working() {
			self.host.last_esc = None;
			send(&self.intents, Intent::Abort);
			return RetainedChatEffect::Consumed;
		}
		if key == Key::Esc && self.host.chat.composer_empty() {
			let now = Instant::now();
			if self
				.host
				.last_esc
				.is_some_and(|last| now.duration_since(last) <= DOUBLE_ESC)
			{
				self.host.last_esc = None;
				send(&self.intents, Intent::RewindRequest);
			} else {
				self.host.last_esc = Some(now);
			}
			return RetainedChatEffect::Consumed;
		}
		if key == Key::Left
			&& self.host.chat.composer_empty()
			&& !self.host.chat.agent_roster().is_empty()
			&& self.host.left_double_tap()
		{
			open_agents_armed(&mut self.host, &self.ctx);
			return RetainedChatEffect::Consumed;
		}
		self.host.last_esc = None;
		let result = self.host.chat.handle_key(key);
		let copied = self.host.chat.take_copied();
		while let Some((text, attachments, mode)) = self.host.chat.take_submission() {
			if text.trim() == "/images" {
				self.host.overlay = Some(Overlay::Images(ImageOverlay::open(
					&self.host.chat.composer_attachments(),
					&self.ctx,
				)));
				continue;
			}
			send(&self.intents, Intent::Submit { text, attachments, mode });
		}
		if result == ChatKey::Quit {
			send(&self.intents, Intent::Quit);
			return RetainedChatEffect::Quit(HostExit::Quit);
		}
		if let Some(text) = copied {
			return RetainedChatEffect::SetClipboard(text);
		}
		match result {
			ChatKey::Consumed => RetainedChatEffect::Consumed,
			ChatKey::Ignored => RetainedChatEffect::Ignored,
			ChatKey::Quit => unreachable!("quit returned above"),
		}
	}

	/// Routes one pointer event through the active overlay or chat surface.
	pub fn mouse(&mut self, report: MouseReport) -> RetainedChatEffect {
		if let Some(overlay) = self.host.overlay.as_mut() {
			let event = overlay.handle_mouse(report.col, report.row, report.kind, self.viewport);
			return self.apply_overlay(event);
		}
		if !self
			.host
			.sidebar
			.handle_mouse(report.col, report.row, report.kind, self.viewport)
		{
			self.host.chat.handle_mouse(&report);
		}
		RetainedChatEffect::Consumed
	}

	/// Routes clipboard text into the active overlay or chat composer.
	pub fn paste(&mut self, text: &str, raw: bool) -> RetainedChatEffect {
		if let Some(overlay) = self.host.overlay.as_mut() {
			let event = overlay.handle_paste(text);
			return self.apply_overlay(event);
		}
		if !self.host.sidebar.focused() {
			if raw {
				self.host.chat.handle_paste_raw(text);
			} else {
				self.host.chat.handle_paste(text);
			}
		}
		RetainedChatEffect::Consumed
	}

	/// Returns the lifecycle outcome with the current unsent composer draft.
	pub fn outcome(&self, exit: HostExit) -> HostOutcome {
		host_outcome(&self.host, exit)
	}

	/// Returns the next paint deadline while preserving background event
	/// latency.
	pub fn tick(&self) -> Duration {
		self
			.host
			.chat
			.next_wake()
			.map_or(BACKEND_POLL_INTERVAL, |wake| wake.min(BACKEND_POLL_INTERVAL))
	}

	fn apply_overlay(&mut self, event: OverlayEvent) -> RetainedChatEffect {
		match apply_overlay_event(
			&mut self.host,
			event,
			&self.ctx,
			self.viewport,
			&self.intents,
			self.exit_on_session_change,
		) {
			Some(exit) => RetainedChatEffect::Quit(exit),
			None => RetainedChatEffect::Consumed,
		}
	}

	fn drain(&mut self) -> bool {
		let mut changed = false;
		loop {
			match self.events.try_recv() {
				Ok(BackendEvent::NewSessionRequested) if self.exit_on_session_change => {
					self.pending_exit = Some(HostExit::NewSession);
					changed = true;
				},
				Ok(BackendEvent::CopyToClipboard(text)) => {
					self.pending_clipboard = Some(text);
					changed = true;
				},
				Ok(event) => {
					if let Some(intent) = apply_backend(&mut self.host, event, &self.ctx) {
						send(&self.intents, Intent::Git(intent));
					}
					send_pty_resize(&mut self.host, self.viewport, &self.intents);
					changed = true;
				},
				Err(flume::TryRecvError::Empty) => break,
				Err(flume::TryRecvError::Disconnected) => {
					self.pending_exit = Some(HostExit::Quit);
					changed = true;
					break;
				},
			}
		}
		while let Some(request) = self.ask_binding.try_recv() {
			if self.host.overlay.is_some() {
				request.fail("another modal dialog is already active");
			} else {
				let dialog = AskDialog::open(request.question.clone(), &self.ctx);
				self.host.overlay = Some(Overlay::Ask { dialog, request });
				changed = true;
			}
		}
		changed
	}
}

fn rail_layers(sidebar: &mut Sidebar, viewport: Size) -> SmallVec<Layer<'_>, 4> {
	sidebar
		.layer(viewport, Instant::now().into())
		.into_iter()
		.collect()
}
enum ListPurpose {
	Resume,
	Rewind,
	Logout,
	Pause,
}

enum Overlay {
	Git(GitWorkbench),
	Models(ModelPicker),
	GuidedGoal(GuidedGoalInterview),
	PlanReview(PlanReviewOverlay),
	PlanSave { prompt: PromptOverlay, content: Str },
	Extensions(ExtensionInspector),
	Pty(PtyOverlay),
	Palette(CommandPalette),
	List { picker: ListPicker, rows: Vec<ListRow>, prefill: Vec<Str>, purpose: ListPurpose },
	AgentHub(AgentHub),
	Settings(SettingsOverlay),
	Selection(SelectionOverlay),
	Images(ImageOverlay),
	History(HistoryInspector),
	AgentPrompt { prompt: PromptOverlay, agent_id: Str, revive: bool },
	Providers(ProviderPicker),
	Prompt(PromptOverlay),
	Approval(ApprovalOverlay),
	ApprovalAmend { prompt: PromptOverlay, ticket_id: Str },
	Ask { dialog: AskDialog, request: AskRequest },
	AutoQaConsent { dialog: AskDialog, consent: AutoQaConsent },
}

enum OverlayEvent {
	Consumed,
	Git(GitIntent),
	GoalComplete { objective: Str, token_budget: Option<u64> },
	PlanReviewComplete(Str),
	PlanSavePathRequest(Str),
	PlanSaveSubmit { path: Str, content: Str },
	ExtensionToggle { id: Str, enabled: bool },
	PtyInput { id: Str, data: bytes::Bytes },
	PtyKill { id: Str },
	Close,
	Pick(usize),
	Palette(PaletteAction),
	PromptCancel,
	Prompt(Str),
	OpenAgentPrompt(Str),
	OpenAgentRevivePrompt(Str),
	AgentSteerPrompt { agent_id: Str, prompt: Str },
	AgentRevivePrompt { agent_id: Str, prompt: Str },
	AgentKill(Str),
	ApprovalCancel,
	ApprovalDecide { ticket_id: Str, action: ApprovalAction },
	ApprovalAmend(Str),
	AskCancel,
	AskSubmit(Vec<Str>),
	AutoQaConsent(Decision),
	SettingsPreview(Vec<SettingChange>),
	SettingsCommit(Vec<SettingChange>),
	Selection(SelectionPurpose, Str),
}

impl Overlay {
	fn handle_key(&mut self, key: Key) -> OverlayEvent {
		match self {
			Self::Git(workbench) => git_workbench_event(workbench.handle_key(key)),
			Self::Models(picker) => picker_event(picker.handle_key(key)),
			Self::GuidedGoal(interview) => guided_goal_event(interview.handle_key(key)),
			Self::PlanReview(review) => {
				let event = review.handle_key(key);
				plan_review_event(event, review.sections())
			},
			Self::PlanSave { prompt, content } => match prompt_event(prompt.handle_key(key)) {
				OverlayEvent::Prompt(path) => {
					OverlayEvent::PlanSaveSubmit { path, content: content.clone() }
				},
				OverlayEvent::PromptCancel => OverlayEvent::Close,
				event => event,
			},
			Self::Extensions(inspector) => extension_inspector_event(inspector.handle_key(key)),
			Self::Pty(pty) => match pty.handle_key(key) {
				PtyEvent::Input(data) => OverlayEvent::PtyInput { id: pty.id().clone(), data },
				PtyEvent::ForceKill => OverlayEvent::PtyKill { id: pty.id().clone() },
				PtyEvent::Close => OverlayEvent::Close,
				PtyEvent::Consumed => OverlayEvent::Consumed,
			},
			Self::Palette(palette) => palette_event(palette.handle_key(key)),
			Self::List { picker, .. } => picker_event(picker.handle_key(key)),
			Self::AgentHub(hub) => agent_hub_event(hub.handle_key(key)),
			Self::Settings(settings) => settings_event(settings.handle_key(key)),
			Self::Selection(selection) => selection_event(selection.handle_key(key)),
			Self::Images(images) => image_overlay_event(images.handle_key(key)),
			Self::History(inspector) => history_inspector_event(inspector.handle_key(key)),
			Self::AgentPrompt { prompt, agent_id, revive } => {
				match prompt_event(prompt.handle_key(key)) {
					OverlayEvent::Prompt(value) if *revive => {
						OverlayEvent::AgentRevivePrompt { agent_id: agent_id.clone(), prompt: value }
					},
					OverlayEvent::Prompt(value) => {
						OverlayEvent::AgentSteerPrompt { agent_id: agent_id.clone(), prompt: value }
					},
					OverlayEvent::PromptCancel => OverlayEvent::Close,
					event => event,
				}
			},
			Self::Providers(picker) => picker_event(picker.handle_key(key)),
			Self::Prompt(prompt) => prompt_event(prompt.handle_key(key)),
			Self::Approval(approval) => {
				approval_event(approval.ticket_id().clone(), approval.handle_key(key))
			},
			Self::ApprovalAmend { prompt, .. } => match prompt_event(prompt.handle_key(key)) {
				OverlayEvent::Prompt(value) => OverlayEvent::ApprovalAmend(value),
				OverlayEvent::PromptCancel => OverlayEvent::ApprovalCancel,
				event => event,
			},
			Self::Ask { dialog, .. } => ask_event(dialog.handle_key(key)),
			Self::AutoQaConsent { dialog, .. } => autoqa_event(dialog.handle_key(key)),
		}
	}

	fn handle_paste(&mut self, text: &str) -> OverlayEvent {
		match self {
			Self::Git(workbench) => git_workbench_event(workbench.handle_paste(text)),
			Self::Models(picker) => picker_event(picker.handle_paste(text)),
			Self::GuidedGoal(interview) => guided_goal_event(interview.handle_paste(text)),
			Self::PlanReview(review) => {
				let event = review.handle_paste(text);
				plan_review_event(event, review.sections())
			},
			Self::PlanSave { prompt, content } => match prompt_event(prompt.handle_paste(text)) {
				OverlayEvent::Prompt(path) => {
					OverlayEvent::PlanSaveSubmit { path, content: content.clone() }
				},
				OverlayEvent::PromptCancel => OverlayEvent::Close,
				event => event,
			},
			Self::Extensions(_) => OverlayEvent::Consumed,
			Self::Pty(pty) => match pty.handle_paste(text) {
				PtyEvent::Input(data) => OverlayEvent::PtyInput { id: pty.id().clone(), data },
				PtyEvent::ForceKill => OverlayEvent::PtyKill { id: pty.id().clone() },
				PtyEvent::Close => OverlayEvent::Close,
				PtyEvent::Consumed => OverlayEvent::Consumed,
			},
			Self::Palette(palette) => palette_event(palette.handle_paste(text)),
			Self::List { picker, .. } => picker_event(picker.handle_paste(text)),
			Self::Settings(settings) => settings_event(settings.handle_paste(text)),
			Self::Selection(selection) => selection_event(selection.handle_paste(text)),
			Self::AgentHub(_) | Self::Images(_) => OverlayEvent::Consumed,
			Self::History(_) => OverlayEvent::Consumed,
			Self::AgentPrompt { prompt, agent_id, revive } => {
				match prompt_event(prompt.handle_paste(text)) {
					OverlayEvent::Prompt(value) if *revive => {
						OverlayEvent::AgentRevivePrompt { agent_id: agent_id.clone(), prompt: value }
					},
					OverlayEvent::Prompt(value) => {
						OverlayEvent::AgentSteerPrompt { agent_id: agent_id.clone(), prompt: value }
					},
					OverlayEvent::PromptCancel => OverlayEvent::Close,
					event => event,
				}
			},
			Self::Providers(picker) => picker_event(picker.handle_paste(text)),
			Self::Prompt(prompt) => prompt_event(prompt.handle_paste(text)),
			Self::Approval(approval) => {
				approval_event(approval.ticket_id().clone(), approval.handle_paste(text))
			},
			Self::ApprovalAmend { prompt, .. } => match prompt_event(prompt.handle_paste(text)) {
				OverlayEvent::Prompt(value) => OverlayEvent::ApprovalAmend(value),
				OverlayEvent::PromptCancel => OverlayEvent::ApprovalCancel,
				event => event,
			},
			Self::Ask { dialog, .. } => ask_event(dialog.handle_paste(text)),
			Self::AutoQaConsent { dialog, .. } => autoqa_event(dialog.handle_paste(text)),
		}
	}

	fn handle_mouse(&mut self, col: u16, row: u16, kind: Mouse, viewport: Size) -> OverlayEvent {
		match self {
			Self::Git(workbench) => {
				git_workbench_event(workbench.handle_mouse(col, row, kind, viewport))
			},
			Self::Models(picker) => picker_event(picker.handle_mouse(col, row, kind, viewport)),
			Self::GuidedGoal(interview) => {
				guided_goal_event(interview.handle_mouse(col, row, kind, viewport))
			},
			Self::PlanReview(review) => {
				let event = review.handle_mouse(col, row, kind, viewport);
				plan_review_event(event, review.sections())
			},
			Self::PlanSave { prompt, content } => {
				match prompt_event(prompt.handle_mouse(col, row, kind, viewport)) {
					OverlayEvent::Prompt(path) => {
						OverlayEvent::PlanSaveSubmit { path, content: content.clone() }
					},
					OverlayEvent::PromptCancel => OverlayEvent::Close,
					event => event,
				}
			},
			Self::Extensions(inspector) => {
				extension_inspector_event(inspector.handle_mouse(col, row, kind, viewport))
			},
			Self::Pty(_) => OverlayEvent::Consumed,
			Self::Palette(palette) => palette_event(palette.handle_mouse(col, row, kind, viewport)),
			Self::List { picker, .. } => picker_event(picker.handle_mouse(col, row, kind, viewport)),
			Self::AgentHub(hub) => agent_hub_event(hub.handle_mouse(col, row, kind, viewport)),
			Self::Settings(settings) => {
				settings_event(settings.handle_mouse(col, row, kind, viewport))
			},
			Self::Selection(selection) => {
				selection_event(selection.handle_mouse(col, row, kind, viewport))
			},
			Self::Images(images) => image_overlay_event(images.handle_mouse(col, row, kind, viewport)),
			Self::History(inspector) => history_inspector_event(inspector.handle_mouse(kind)),
			Self::AgentPrompt { .. } => OverlayEvent::Consumed,
			Self::Providers(picker) => picker_event(picker.handle_mouse(col, row, kind, viewport)),
			Self::Prompt(prompt) => prompt_event(prompt.handle_mouse(col, row, kind, viewport)),
			Self::Approval(approval) => approval_event(
				approval.ticket_id().clone(),
				approval.handle_mouse(col, row, kind, viewport),
			),
			Self::ApprovalAmend { prompt, .. } => {
				match prompt_event(prompt.handle_mouse(col, row, kind, viewport)) {
					OverlayEvent::Prompt(value) => OverlayEvent::ApprovalAmend(value),
					OverlayEvent::PromptCancel => OverlayEvent::ApprovalCancel,
					event => event,
				}
			},
			Self::Ask { dialog, .. } => ask_event(dialog.handle_mouse(col, row, kind, viewport)),
			Self::AutoQaConsent { dialog, .. } => {
				autoqa_event(dialog.handle_mouse(col, row, kind, viewport))
			},
		}
	}

	fn layer(&mut self, viewport: Size) -> Layer<'_> {
		match self {
			Self::Git(workbench) => workbench.layer(viewport),
			Self::Models(picker) => picker.layer(viewport),
			Self::GuidedGoal(interview) => interview.layer(viewport),
			Self::PlanReview(review) => review.layer(viewport),
			Self::PlanSave { prompt, .. } => prompt.layer(viewport),
			Self::Extensions(inspector) => inspector.layer(viewport),
			Self::Pty(pty) => pty.layer(viewport),
			Self::Palette(palette) => palette.layer(viewport),
			Self::List { picker, .. } => picker.layer(viewport),
			Self::AgentHub(hub) => hub.layer(viewport),
			Self::Settings(settings) => settings.layer(viewport),
			Self::Selection(selection) => selection.layer(viewport),
			Self::Images(images) => images.layer(viewport),
			Self::History(inspector) => inspector.layer(viewport),
			Self::AgentPrompt { prompt, .. } => prompt.layer(viewport),
			Self::Providers(picker) => picker.layer(viewport),
			Self::Prompt(prompt) => prompt.layer(viewport),
			Self::Approval(approval) => approval.layer(viewport),
			Self::ApprovalAmend { prompt, .. } => prompt.layer(viewport),
			Self::Ask { dialog, .. } => dialog.layer(viewport),
			Self::AutoQaConsent { dialog, .. } => dialog.layer(viewport),
		}
	}
}

fn git_workbench_event(event: GitWorkbenchEvent) -> OverlayEvent {
	match event {
		GitWorkbenchEvent::Consumed => OverlayEvent::Consumed,
		GitWorkbenchEvent::Intent(intent) => OverlayEvent::Git(intent),
		GitWorkbenchEvent::Close => OverlayEvent::Close,
	}
}

const fn picker_event(event: PickerEvent) -> OverlayEvent {
	match event {
		PickerEvent::Consumed => OverlayEvent::Consumed,
		PickerEvent::Close => OverlayEvent::Close,
		PickerEvent::Pick(index) => OverlayEvent::Pick(index),
	}
}
fn guided_goal_event(event: GuidedGoalEvent) -> OverlayEvent {
	match event {
		GuidedGoalEvent::Consumed => OverlayEvent::Consumed,
		GuidedGoalEvent::Cancel => OverlayEvent::Close,
		GuidedGoalEvent::Complete(values) => OverlayEvent::GoalComplete {
			objective:    values.objective,
			token_budget: values.token_budget,
		},
	}
}

fn plan_review_event(event: PlanReviewEvent, sections: &[PlanReviewSection]) -> OverlayEvent {
	match event {
		PlanReviewEvent::Consumed
		| PlanReviewEvent::SectionChanged(_)
		| PlanReviewEvent::AnnotationsChanged(_) => OverlayEvent::Consumed,
		PlanReviewEvent::Submit(annotations) => {
			OverlayEvent::PlanReviewComplete(annotations.prompt(sections))
		},
		PlanReviewEvent::SaveAndQuit(content) => OverlayEvent::PlanSavePathRequest(content),
		PlanReviewEvent::Cancel => OverlayEvent::Close,
	}
}
fn extension_inspector_event(event: ExtensionInspectorEvent) -> OverlayEvent {
	match event {
		ExtensionInspectorEvent::Consumed => OverlayEvent::Consumed,
		ExtensionInspectorEvent::Close => OverlayEvent::Close,
		ExtensionInspectorEvent::Toggle { id, enabled } => {
			OverlayEvent::ExtensionToggle { id, enabled }
		},
	}
}

fn agent_hub_event(event: AgentHubEvent) -> OverlayEvent {
	match event {
		AgentHubEvent::Consumed => OverlayEvent::Consumed,
		AgentHubEvent::Close => OverlayEvent::Close,
		AgentHubEvent::Steer(id) => OverlayEvent::OpenAgentPrompt(id),
		AgentHubEvent::Revive(id) => OverlayEvent::OpenAgentRevivePrompt(id),
		AgentHubEvent::Kill(id) => OverlayEvent::AgentKill(id),
	}
}
const fn image_overlay_event(event: ImageOverlayEvent) -> OverlayEvent {
	match event {
		ImageOverlayEvent::Consumed => OverlayEvent::Consumed,
		ImageOverlayEvent::Close => OverlayEvent::Close,
	}
}
const fn history_inspector_event(event: HistoryInspectorEvent) -> OverlayEvent {
	match event {
		HistoryInspectorEvent::Consumed => OverlayEvent::Consumed,
		HistoryInspectorEvent::Close => OverlayEvent::Close,
	}
}

fn settings_event(event: SettingsEvent) -> OverlayEvent {
	match event {
		SettingsEvent::Consumed => OverlayEvent::Consumed,
		SettingsEvent::Close => OverlayEvent::Close,
		SettingsEvent::Preview(changes) => OverlayEvent::SettingsPreview(changes),
		SettingsEvent::Commit(changes) => OverlayEvent::SettingsCommit(changes),
	}
}

fn selection_event(event: SelectionEvent) -> OverlayEvent {
	match event {
		SelectionEvent::Consumed => OverlayEvent::Consumed,
		SelectionEvent::Close => OverlayEvent::Close,
		SelectionEvent::Pick { purpose, key } => OverlayEvent::Selection(purpose, key),
	}
}

fn palette_event(event: PaletteEvent) -> OverlayEvent {
	match event {
		PaletteEvent::Consumed => OverlayEvent::Consumed,
		PaletteEvent::Close => OverlayEvent::Close,
		PaletteEvent::Run(action) => OverlayEvent::Palette(action),
	}
}

fn prompt_event(event: PromptEvent) -> OverlayEvent {
	match event {
		PromptEvent::Consumed => OverlayEvent::Consumed,
		PromptEvent::Cancel => OverlayEvent::PromptCancel,
		PromptEvent::Submit(value) => OverlayEvent::Prompt(value),
	}
}

fn approval_event(ticket_id: Str, event: ApprovalEvent) -> OverlayEvent {
	match event {
		ApprovalEvent::Consumed => OverlayEvent::Consumed,
		ApprovalEvent::Cancel => OverlayEvent::ApprovalCancel,
		ApprovalEvent::Decide(action) => OverlayEvent::ApprovalDecide { ticket_id, action },
		ApprovalEvent::Amend => OverlayEvent::ApprovalAmend(ticket_id),
	}
}
fn ask_event(event: AskDialogEvent) -> OverlayEvent {
	match event {
		AskDialogEvent::Consumed => OverlayEvent::Consumed,
		AskDialogEvent::Cancel => OverlayEvent::AskCancel,
		AskDialogEvent::Submit(values) => OverlayEvent::AskSubmit(values),
	}
}
fn autoqa_event(event: AskDialogEvent) -> OverlayEvent {
	match event {
		AskDialogEvent::Consumed => OverlayEvent::Consumed,
		AskDialogEvent::Cancel => OverlayEvent::AutoQaConsent(Decision::LocalOnly),
		AskDialogEvent::Submit(values) => {
			OverlayEvent::AutoQaConsent(if values.iter().any(|value| value == "Upload") {
				Decision::Upload
			} else {
				Decision::LocalOnly
			})
		},
	}
}

#[derive(Clone, Copy)]
struct ResizeState {
	last_event: Instant,
}

impl ResizeState {
	const fn new(last_event: Instant) -> Self {
		Self { last_event }
	}

	const fn observe(&mut self, observed_at: Instant) {
		self.last_event = observed_at;
	}

	fn deadline(self) -> Instant {
		self.last_event + RESIZE_SETTLE
	}

	fn settled(self, now: Instant) -> bool {
		now >= self.deadline()
	}
}

#[expect(
	clippy::future_not_send,
	reason = "chat components remain confined to their terminal event-loop thread"
)]
async fn run_chat(
	executor: &Executor,
	terminal: &mut Terminal,
	renderer: &mut Renderer<TtyOut>,
	ctx: &UiContext,
	mut viewport: Size,
	chat: Chat,
	models: Vec<ModelRow>,
	current_model: usize,
	events: &Receiver<BackendEvent>,
	intents: &Sender<Intent>,
	options: HostOptions,
) -> io::Result<HostOutcome> {
	let mut host = ChatHost::new(chat, ctx, viewport, models, current_model, true);
	let ask_binding = ask::bind();
	paint_host(renderer, &mut host, viewport, Retirement::Disabled)?;

	let mut resize = None;
	let mut settled_width = viewport.width;
	let mut pending_replay: Option<ResizeScrollback> = None;
	let mut paste_read: Option<PasteRead> = None;
	let mut next_frame = chat_deadline(&host.chat);
	let mut requested_exit = HostExit::Quit;
	let HostOptions {
		exit_on_session_change,
		completion_notify,
		error_notify,
		title_enabled,
		resize_scrollback,
		..
	} = options;
	loop {
		let paste_deadline = paste_read.as_ref().map(|read| read.abandon_at);
		tokio::select! {
							event = terminal.next(), if paste_read.is_none() => match event? {
								TerminalEvent::Resize => {
									observe_resize(terminal, &mut viewport, &mut resize, Instant::now())?;
									host.chat.set_right_inset(host.sidebar.reserved(viewport));
									send_pty_resize(&mut host, viewport, intents);
									next_frame = Some(Instant::now());
								},
								TerminalEvent::Debug(query) => answer_debug(query, &mut host.chat),
								TerminalEvent::Effect(effect) => {
									let _ = host.chat.slots_mut().apply_serialized(effect);
								},
								TerminalEvent::Closed => return Ok(host_outcome(&host, HostExit::Quit)),
								TerminalEvent::Input(event) => {
									let Some(event) = user_event(terminal, renderer, event)? else { continue };
									match event {
										InputEvent::Key(key) => {
											if host.overlay.is_some() {
												if key == Key::Ctrl('c') {
													send(intents, Intent::Quit);
													break;
												}
												let event = host.overlay.as_mut().expect("overlay present").handle_key(key);
												if let Some(exit) = apply_overlay_event(
													&mut host,
													event,
													ctx,
													viewport,
													intents,
													exit_on_session_change,
												) {
													return Ok(host_outcome(&host, exit));
												}
												if host.overlay.is_none() {
													close_overlay(terminal, renderer, &mut host, viewport, &mut resize)?;
												}
											} else if key == Key::Ctrl('b') {
												host.sidebar.toggle();
												host.chat.set_right_inset(host.sidebar.reserved(viewport));
																				} else if key == Key::Ctrl('z') {
												requested_exit = HostExit::Suspend;
												break;
											} else if key == Key::Alt('l') {
												terminal.refresh_appearance()?;
												// Rebuild directly, or append inside multiplexers where
												// ED3 would irreversibly erase pane history.
												pending_replay = Some(if terminal.inside_multiplexer() {
													ResizeScrollback::Append
												} else {
													ResizeScrollback::Rebuild
												});
												start_pending_replay(
													renderer,
													&mut host,
													&mut pending_replay,
												)?;
											} else if key == Key::RestoreQueue {
												send(intents, Intent::Dequeue);
											} else if key == Key::CyclePrevious {
												host.cycle_model(true, intents);
											} else if key == Key::Ctrl('p') {
												host.cycle_model(false, intents);
											} else if key == Key::BackTab {
												send(intents, Intent::CycleThinking);
											} else if key == Key::Ctrl('t') {
												let _ = host.chat.handle_key(key);
												send(intents, Intent::ToggleThinking);
											} else if key == Key::Alt('r') {
												send(intents, Intent::Retry);
											} else if key == Key::PlanToggle {
												send(intents, Intent::TogglePlan);
											} else if key == Key::CtrlAlt('l') {
												send(intents, Intent::ToggleLive);
											} else if key == Key::CtrlAlt('s') {
												send(intents, Intent::ToggleStt);
											} else if key == Key::Alt('h') {
												send(intents, Intent::InspectHistory);
											} else if key == Key::Ctrl('k') {
												host.overlay = Some(Overlay::Palette(CommandPalette::open(palette_entries(), ctx)));
												open_overlay(terminal, renderer, &mut host, viewport, &mut resize)?;
											} else if host.sidebar.focused() {
												if key == Key::Ctrl('c') {
													send(intents, Intent::Quit);
													break;
												}
												host.sidebar.handle_key(key);
											} else if key == Key::Alt('a') || key == Key::Ctrl('s') {
												open_agents(&mut host, ctx);
												open_overlay(
													terminal,
													renderer,
													&mut host,
													viewport,
													&mut resize,
												)?;
											} else if key == Key::Alt('m') || key == Key::Alt('p') {
												host.open_models(ctx);
												if host.overlay.is_some() {
													open_overlay(terminal, renderer, &mut host, viewport, &mut resize)?;
												}
											} else if let Some(scope) = ClipboardRead::for_key(key) {
												paste_read = Some(PasteRead::start(scope));
											} else if key == Key::Esc && host.chat.is_working() {
												host.last_esc = None;
												send(intents, Intent::Abort);
											} else if key == Key::Esc && host.chat.composer_empty() {
												let now = Instant::now();
												if host.last_esc.is_some_and(|last| now.duration_since(last) <= DOUBLE_ESC) {
													host.last_esc = None;
													send(intents, Intent::RewindRequest);
												} else {
													host.last_esc = Some(now);
												}
																				} else if key == Key::Left
												&& host.chat.composer_empty()
												&& !host.chat.agent_roster().is_empty()
												&& host.left_double_tap()
											{
												open_agents_armed(&mut host, ctx);
												open_overlay(
													terminal,
													renderer,
													&mut host,
													viewport,
													&mut resize,
												)?;
		} else {
												host.last_esc = None;
												let result = host.chat.handle_key(key);
												if let Some(text) = host.chat.take_copied() { terminal.copy_to_clipboard(&text)?; }
												while let Some((text, attachments, mode)) = host.chat.take_submission() {
																						if text.trim() == "/images" {
														host.overlay = Some(Overlay::Images(ImageOverlay::open(
															&host.chat.composer_attachments(),
															ctx,
														)));
														open_overlay(
															terminal,
															renderer,
															&mut host,
															viewport,
															&mut resize,
														)?;
														continue;
													}
				send(intents, Intent::Submit { text, attachments, mode });
												}
												if result == ChatKey::Quit {
													send(intents, Intent::Quit);
													break;
												}
											}
											next_frame = Some(Instant::now());
										},
										InputEvent::Paste(text) => {
											if let Some(active) = host.overlay.as_mut() {
												let event = active.handle_paste(&text);
												if let Some(exit) = apply_overlay_event(
													&mut host,
													event,
													ctx,
													viewport,
													intents,
													exit_on_session_change,
												) {
													return Ok(host_outcome(&host, exit));
												}
												if host.overlay.is_none() {
													close_overlay(terminal, renderer, &mut host, viewport, &mut resize)?;
												}
											} else if !host.sidebar.focused() {
												host.chat.handle_paste(&text);
											}
											next_frame = Some(Instant::now());
										},
										InputEvent::Mouse(report) => {
											if let Some(active) = host.overlay.as_mut() {
												let event = active.handle_mouse(report.col, report.row, report.kind, viewport);
												if let Some(exit) = apply_overlay_event(
													&mut host,
													event,
													ctx,
													viewport,
													intents,
													exit_on_session_change,
												) {
													return Ok(host_outcome(&host, exit));
												}
												if host.overlay.is_none() {
													close_overlay(terminal, renderer, &mut host, viewport, &mut resize)?;
												}
											} else if !host.sidebar.handle_mouse(report.col, report.row, report.kind, viewport) {
												host.chat.handle_mouse(&report);
											}
											next_frame = Some(Instant::now());
										},
										InputEvent::Focus(_) | InputEvent::Response(_) => {},
									}
								},
							},
							request = ask_binding.recv() => {
								if let Ok(request) = request {
									if host.overlay.is_some() {
										request.fail("another modal dialog is already active");
									} else {
										let dialog = AskDialog::open(request.question.clone(), ctx);
										host.overlay = Some(Overlay::Ask { dialog, request });
										open_overlay(
											terminal,
											renderer,
											&mut host,
											viewport,
											&mut resize,
										)?;
										next_frame = Some(Instant::now());
									}
								}
							},
							backend = events.recv_async() => match backend {
								Ok(BackendEvent::NewSessionRequested) if exit_on_session_change => {
									return Ok(host_outcome(&host, HostExit::NewSession));
								},
								Ok(event) => {
									match &event {
										BackendEvent::ApprovalPending(_) if title_enabled => {
											terminal.set_title("Approval required · omp")?;
										},
										BackendEvent::ApprovalSettled { .. }
											if title_enabled && host.pending_approvals <= 1 =>
										{
											terminal.set_title(host.session_title.as_str())?;
										},
										BackendEvent::Error(message) => {
											if title_enabled {
												terminal.set_title("Error · omp")?;
											}
											if error_notify {
												terminal.notify(
													&Notification::builder()
														.title("omp error")
														.body(message.clone())
														.id("omp-error")
														.urgency(Urgency::Critical)
														.build(),
												)?;
											}
										},
										BackendEvent::Ack { interrupted: false } => {
											if title_enabled {
												terminal.set_title(host.session_title.as_str())?;
											}
											if completion_notify {
												terminal.notify(
													&Notification::builder()
														.title("omp")
														.body("Turn complete")
														.id("omp-complete")
														.build(),
												)?;
											}
										},
										BackendEvent::SessionTitle(title) => {
											host.session_title = sf!("{title} · omp");
											if title_enabled && host.pending_approvals == 0 {
												terminal.set_title(host.session_title.as_str())?;
											}
										},
										BackendEvent::CopyToClipboard(text) => {
											terminal.copy_to_clipboard(text)?;
										},
										_ => {},
									}
									let had_overlay = host.overlay.is_some();
									if let Some(intent) = apply_terminal_backend(&mut host, event, ctx) {
										send(intents, Intent::Git(intent));
									}
									send_pty_resize(&mut host, viewport, intents);
									if !had_overlay && host.overlay.is_some() {
										open_overlay(terminal, renderer, &mut host, viewport, &mut resize)?;
									} else if had_overlay && host.overlay.is_none() {
										close_overlay(terminal, renderer, &mut host, viewport, &mut resize)?;
									}
									next_frame = Some(Instant::now());
								},
								Err(_) => break,
							},
							clipboard = async { (&mut paste_read.as_mut().expect("branch gated").clipboard).await }, if paste_read.is_some() => {
								let read = paste_read.take().expect("branch gated");
								if let Ok(Some(clipboard)) = clipboard
									&& let Some(text) = clipboard_paste_text(clipboard)
									&& host.overlay.is_none()
									&& !host.sidebar.focused()
								{
									match read.scope {
										ClipboardRead::Text => host.chat.handle_paste_raw(&text),
										ClipboardRead::Smart => host.chat.handle_paste(&text),
									}
									next_frame = Some(Instant::now());
								}
							},
							() = deadline(executor, paste_deadline) => paste_read = None,
							() = deadline(executor, next_frame) => {
								observe_resize(terminal, &mut viewport, &mut resize, Instant::now())?;
								host.chat.set_right_inset(host.sidebar.reserved(viewport));
								start_pending_replay(renderer, &mut host, &mut pending_replay)?;
								// A retired batch may leave further finalized prefixes
								// (or replay batches) ready: repaint immediately to
								// drain them instead of waiting for the next event.
								next_frame = match paint_host(renderer, &mut host, viewport, Retirement::Pressure)? {
									PaintKind::Retired | PaintKind::Deferred => Some(Instant::now()),
									PaintKind::Presented => chat_deadline(&host.chat),
								};
							},
							() = deadline(executor, resize.map(ResizeState::deadline)) => {
								let now = Instant::now();
								if !resize.is_some_and(|state| state.settled(now)) { continue; }
								host.chat.set_right_inset(host.sidebar.reserved(viewport));
								// A settled width change leaves native scrollback rows
								// wrapped at the old width; refresh them through one
								// buffered replay without changing commit state.
								if viewport.width != settled_width {
									settled_width = viewport.width;
									let mode = if resize_scrollback == ResizeScrollback::Rebuild
										&& terminal.inside_multiplexer()
									{
										// ED3 wipes multiplexer pane history irrecoverably;
										// degrade to an append replay.
										ResizeScrollback::Append
									} else {
										resize_scrollback
									};
									pending_replay = (mode != ResizeScrollback::Preserve).then_some(mode);
								}
								resize = None;
								start_pending_replay(renderer, &mut host, &mut pending_replay)?;
								next_frame = match paint_host(renderer, &mut host, viewport, Retirement::Pressure)? {
									PaintKind::Retired | PaintKind::Deferred => Some(now),
									PaintKind::Presented => chat_deadline(&host.chat),
								};
							},
						}
	}
	if host.overlay.take().is_some() {
		close_overlay(terminal, renderer, &mut host, viewport, &mut resize)?;
	}
	start_pending_replay(renderer, &mut host, &mut pending_replay)?;
	if requested_exit == HostExit::Quit {
		host.chat.cancel_active("Host closed");
		loop {
			match paint_host(renderer, &mut host, viewport, Retirement::Flush)? {
				PaintKind::Retired | PaintKind::Deferred => {},
				PaintKind::Presented => break,
			}
		}
	}
	renderer.repaint("", Frame::new(viewport), viewport.height, &[])?;
	Ok(host_outcome(&host, requested_exit))
}

fn host_outcome(host: &ChatHost, exit: HostExit) -> HostOutcome {
	HostOutcome { exit, draft: Str::from(host.chat.composer_text()) }
}

fn apply_terminal_backend(
	host: &mut ChatHost,
	event: BackendEvent,
	ctx: &UiContext,
) -> Option<GitIntent> {
	match event {
		BackendEvent::HistoryRewind { user_index, text } => {
			let _ = host.chat.rewind_user(user_index, text.as_str());
			host.suppress_history_replay = true;
			None
		},
		BackendEvent::HistoryReplayFinished => {
			host.suppress_history_replay = false;
			None
		},
		BackendEvent::HistoryCleared => {
			host.chat.clear_history();
			None
		},
		_ if host.suppress_history_replay => None,
		event => apply_backend(host, event, ctx),
	}
}

fn apply_backend(host: &mut ChatHost, event: BackendEvent, ctx: &UiContext) -> Option<GitIntent> {
	match event {
		BackendEvent::OpenGitWorkbench(snapshot) => {
			let mut workbench = GitWorkbench::open(snapshot, ctx);
			let intent = workbench.initial_intent();
			host.overlay = Some(Overlay::Git(workbench));
			return intent;
		},
		BackendEvent::Git(update) => {
			return match host.overlay.as_mut() {
				Some(Overlay::Git(workbench)) => workbench.apply(update),
				_ => None,
			};
		},
		BackendEvent::OpenGuidedGoal => {
			host.overlay = Some(Overlay::GuidedGoal(GuidedGoalInterview::open(ctx)));
		},
		BackendEvent::OpenPlanReview { content } => {
			host.overlay = Some(Overlay::PlanReview(PlanReviewOverlay::open(
				content.as_str(),
				Default::default(),
				ctx,
			)));
		},
		BackendEvent::OpenPlanSavePrompt { content, suggested_path } => {
			host.overlay = Some(Overlay::PlanSave {
				prompt: PromptOverlay::open_prefilled("Save plan and quit", suggested_path, ctx),
				content,
			});
		},
		BackendEvent::OpenExtensionInspector(snapshot) => {
			host.overlay = Some(Overlay::Extensions(ExtensionInspector::open(snapshot, ctx)));
		},
		BackendEvent::ExtensionSnapshotUpdated(snapshot) => {
			if let Some(Overlay::Extensions(inspector)) = host.overlay.as_mut() {
				inspector.update_snapshot(snapshot);
			}
		},
		BackendEvent::ExtensionMcpUpdated(snapshot) => {
			if let Some(Overlay::Extensions(inspector)) = host.overlay.as_mut() {
				inspector.update_mcp(snapshot);
			}
		},
		BackendEvent::ExtensionProviderDisabled(provider_id) => {
			if let Some(Overlay::Extensions(inspector)) = host.overlay.as_mut() {
				inspector.provider_disabled(provider_id.as_str());
			}
		},
		BackendEvent::HistoryInspect { frame } => {
			host.overlay = Some(Overlay::History(HistoryInspector::open(frame)));
		},
		BackendEvent::ApprovalPending(ticket) => {
			host.pending_approvals = host.pending_approvals.saturating_add(1);
			if matches!(host.overlay, Some(Overlay::Approval(_) | Overlay::ApprovalAmend { .. })) {
				host.approval_queue.push_back(ticket);
			} else {
				host.overlay = Some(Overlay::Approval(ApprovalOverlay::open(ticket, ctx)));
			}
		},
		BackendEvent::AutoQaConsent(request) => {
			if host.overlay.is_some() {
				host.autoqa_queue.push_back(request);
			} else {
				open_autoqa_consent(host, request, ctx);
			}
		},
		BackendEvent::ApprovalSettled { ticket_id } => {
			host.pending_approvals = host.pending_approvals.saturating_sub(1);
			let closes = matches!(
				&host.overlay,
				Some(Overlay::Approval(approval)) if approval.ticket_id() == &ticket_id
			) || matches!(
				&host.overlay,
				Some(Overlay::ApprovalAmend { ticket_id: active, .. }) if active == &ticket_id
			);
			if closes {
				host.overlay = host
					.approval_queue
					.pop_front()
					.map(|ticket| Overlay::Approval(ApprovalOverlay::open(ticket, ctx)));
			} else {
				host
					.approval_queue
					.retain(|ticket| ticket.ticket_id != ticket_id);
			}
		},
		BackendEvent::PtyStarted { id, command } => {
			host.overlay = Some(Overlay::Pty(PtyOverlay::open(id, command, ctx)));
		},
		BackendEvent::PtyOutput { id, chunk } => {
			if let Some(Overlay::Pty(pty)) = &mut host.overlay
				&& pty.id() == &id
			{
				pty.append_output(chunk);
			}
		},
		BackendEvent::PtyFinished { id, status, exit_code } => {
			if let Some(Overlay::Pty(pty)) = &mut host.overlay
				&& pty.id() == &id
			{
				pty.finish(status, exit_code);
			}
		},
		BackendEvent::Status(facts) => {
			host.sidebar.set_status(&facts);
			let _ = host.chat.apply_backend_event(BackendEvent::Status(facts));
		},
		BackendEvent::OpenModelPicker { rows, current } => {
			update_models(host, rows, current);
			host.open_models(ctx);
		},
		BackendEvent::ModelsUpdated { rows, current } => {
			update_models(host, rows, current);
		},
		BackendEvent::Sessions(rows) => open_sessions(host, rows, ctx),
		BackendEvent::LoginProviders(rows) => open_login_providers(host, rows, ctx),
		BackendEvent::LogoutChoices { title, rows } => open_logout_choices(host, title, rows, ctx),
		BackendEvent::RewindTargets(rows) => open_rewind(host, rows, ctx),
		BackendEvent::AgentRoster(rows) => {
			if let Some(Overlay::AgentHub(hub)) = &mut host.overlay {
				hub.update_rows(&rows);
			}
			host.chat.set_agent_roster(rows);
		},
		BackendEvent::SettingsSchema(rows) => open_settings(host, rows, ctx),
		BackendEvent::OpenSelection { title, purpose, rows } => {
			host.overlay = Some(Overlay::Selection(SelectionOverlay::open(title, purpose, rows, ctx)));
		},
		BackendEvent::OpenAgentTree => open_agents(host, ctx),
		BackendEvent::Pause => open_pause(host, ctx),
		BackendEvent::NewSessionRequested => {},
		BackendEvent::AuthPrompt { message, masked } => {
			host.overlay = Some(Overlay::Prompt(PromptOverlay::open(message, masked, ctx)));
		},
		BackendEvent::AuthPromptClose => {
			if matches!(host.overlay, Some(Overlay::Prompt(_))) {
				host.overlay = None;
			}
			let _ = host.chat.apply_backend_event(BackendEvent::AuthPromptClose);
		},
		event => {
			let _ = host.chat.apply_backend_event(event);
		},
	}
	None
}
fn open_autoqa_consent(host: &mut ChatHost, request: ConsentRequest, ctx: &UiContext) {
	let consent = AutoQaConsent::new(request);
	let dialog = AskDialog::open(consent.question(), ctx);
	host.overlay = Some(Overlay::AutoQaConsent { dialog, consent });
}

fn update_models(host: &mut ChatHost, rows: Vec<ModelRow>, current: usize) {
	host.current_model = current.min(rows.len().saturating_sub(1));
	if let Some(Overlay::Models(picker)) = &mut host.overlay {
		picker.update_rows(&rows, host.current_model);
	}
	host.models = rows;
	if let Some(model) = host.models.get(host.current_model) {
		let mut facts = host.chat.status();
		facts.model = if model.name.is_empty() {
			model.key.clone()
		} else {
			model.name.clone()
		};
		host.sidebar.set_status(&facts);
		host.chat.set_status(facts);
	}
}

fn open_sessions(host: &mut ChatHost, sessions: Vec<SessionRow>, ctx: &UiContext) {
	let rows: Vec<ListRow> = sessions
		.into_iter()
		.map(|row| ListRow {
			key:    row.id,
			label:  if row.pinned {
				sf!("{} {}", ctx.charset.icon(Icon::Pin), row.label)
			} else {
				row.label
			},
			detail: row.detail,
		})
		.collect();
	let picker = ListPicker::open("Resume session", &rows, 0, ctx);
	host.overlay =
		Some(Overlay::List { picker, rows, prefill: Vec::new(), purpose: ListPurpose::Resume });
}

fn open_login_providers(host: &mut ChatHost, providers: Vec<SessionRow>, ctx: &UiContext) {
	host.overlay = Some(Overlay::Providers(ProviderPicker::open(providers, ctx)));
}

fn open_logout_choices(host: &mut ChatHost, title: Str, choices: Vec<SessionRow>, ctx: &UiContext) {
	let rows = choices
		.into_iter()
		.map(|row| ListRow { key: row.id, label: row.label, detail: row.detail })
		.collect::<Vec<_>>();
	let picker = ListPicker::open(title.as_str(), &rows, 0, ctx);
	host.overlay =
		Some(Overlay::List { picker, rows, prefill: Vec::new(), purpose: ListPurpose::Logout });
}

fn open_settings(host: &mut ChatHost, fields: Vec<SettingRow>, ctx: &UiContext) {
	host.overlay = Some(Overlay::Settings(SettingsOverlay::open(fields, ctx)));
}

fn open_agents(host: &mut ChatHost, ctx: &UiContext) {
	host.overlay = Some(Overlay::AgentHub(AgentHub::open(host.chat.agent_roster(), ctx)));
}
fn open_agents_armed(host: &mut ChatHost, ctx: &UiContext) {
	let mut hub = AgentHub::open(host.chat.agent_roster(), ctx);
	hub.arm_close_tap();
	host.overlay = Some(Overlay::AgentHub(hub));
}

fn open_pause(host: &mut ChatHost, ctx: &UiContext) {
	let rows = vec![ListRow {
		key:    sf!("resume"),
		label:  sf!("Resume"),
		detail: sf!("Press Enter or Esc to return to the session"),
	}];
	let picker = ListPicker::open("Paused", &rows, 0, ctx);
	host.overlay =
		Some(Overlay::List { picker, rows, prefill: Vec::new(), purpose: ListPurpose::Pause });
}

fn open_rewind(host: &mut ChatHost, targets: Vec<RewindTargetRow>, ctx: &UiContext) {
	let mut prefill = Vec::with_capacity(targets.len());
	let rows: Vec<ListRow> = targets
		.into_iter()
		.rev()
		.map(|row| {
			prefill.push(row.text.clone());
			ListRow {
				key:    Str::new(row.event.to_string()),
				label:  Str::new(row.text.lines().next().unwrap_or("")),
				detail: sf!("rewind here"),
			}
		})
		.collect();
	let picker = ListPicker::open("Rewind history", &rows, 0, ctx);
	host.overlay = Some(Overlay::List { picker, rows, prefill, purpose: ListPurpose::Rewind });
}

fn send_pty_resize(host: &mut ChatHost, viewport: Size, intents: &Sender<Intent>) {
	let Some(Overlay::Pty(pty)) = &mut host.overlay else {
		return;
	};
	let _ = pty.layer(viewport);
	let (rows, columns) = pty.dimensions();
	send(intents, Intent::PtyResize { id: pty.id().clone(), rows, columns });
}

fn apply_overlay_event(
	host: &mut ChatHost,
	event: OverlayEvent,
	ctx: &UiContext,
	viewport: Size,
	intents: &Sender<Intent>,
	exit_on_session_change: bool,
) -> Option<HostExit> {
	match event {
		OverlayEvent::Consumed => {},
		OverlayEvent::Git(intent) => send(intents, Intent::Git(intent)),
		OverlayEvent::GoalComplete { objective, token_budget } => {
			send(intents, Intent::SetGoal { objective, token_budget });
			host.overlay = None;
		},
		OverlayEvent::PlanReviewComplete(feedback) => {
			send(intents, Intent::Submit {
				text:        feedback.to_string(),
				attachments: Vec::new(),
				mode:        SubmitMode::Steer,
			});
			host.overlay = None;
		},
		OverlayEvent::PlanSavePathRequest(content) => {
			send(intents, Intent::PlanSavePathRequest { content });
			host.overlay = None;
		},
		OverlayEvent::PlanSaveSubmit { path, content } => {
			send(intents, Intent::SavePlanAndQuit { path, content });
			host.overlay = None;
		},
		OverlayEvent::ExtensionToggle { id, enabled } => {
			send(intents, Intent::ToggleExtension { id, enabled });
		},
		OverlayEvent::PtyInput { id, data } => send(intents, Intent::PtyInput { id, data }),
		OverlayEvent::PtyKill { id } => send(intents, Intent::PtyKill { id }),
		OverlayEvent::Close => {
			if matches!(host.overlay, Some(Overlay::Extensions(_))) {
				send(intents, Intent::CloseExtensionInspector);
			}
			if matches!(host.overlay, Some(Overlay::Git(_))) {
				send(intents, Intent::Git(GitIntent::Close));
			}
			host.overlay = None;
		},
		OverlayEvent::OpenAgentPrompt(agent_id) => {
			host.overlay = Some(Overlay::AgentPrompt {
				prompt: PromptOverlay::open("Steer selected agent", false, ctx),
				agent_id,
				revive: false,
			});
		},
		OverlayEvent::OpenAgentRevivePrompt(agent_id) => {
			host.overlay = Some(Overlay::AgentPrompt {
				prompt: PromptOverlay::open("Revive selected agent", false, ctx),
				agent_id,
				revive: true,
			});
		},
		OverlayEvent::AgentSteerPrompt { agent_id, prompt } => {
			send(intents, Intent::AgentSteer { id: agent_id, prompt });
			host.overlay = None;
		},
		OverlayEvent::AgentRevivePrompt { agent_id, prompt } => {
			send(intents, Intent::AgentRevive { id: agent_id, prompt });
			host.overlay = None;
		},
		OverlayEvent::AgentKill(id) => {
			send(intents, Intent::AgentKill { id });
			host.overlay = None;
		},
		OverlayEvent::Pick(index) => match host.overlay.as_ref() {
			Some(Overlay::Models(_)) => {
				if let Some(model) = host.models.get(index) {
					host.current_model = index;
					send(intents, Intent::SwitchModel(model.key.clone()));
				}
				host.overlay = None;
			},
			Some(Overlay::List { rows, prefill, purpose, .. }) => {
				if let Some(row) = rows.get(index) {
					match purpose {
						ListPurpose::Resume => {
							let id = row.key.clone();
							send(intents, Intent::Resume(Some(id.clone())));
							if exit_on_session_change {
								return Some(HostExit::Resume(id));
							}
						},
						ListPurpose::Rewind => {
							if let Ok(event) = row.key.parse::<u64>() {
								if let Some(text) = prefill.get(index) {
									host.chat.set_composer_text(text);
								}
								send(intents, Intent::Rewind { event });
							}
						},
						ListPurpose::Pause => {},
						ListPurpose::Logout => {
							send(intents, Intent::Logout(Some(row.key.clone())));
						},
					}
				}
				host.overlay = None;
			},
			Some(Overlay::Providers(picker)) => {
				if let Some(provider) = picker.key(index) {
					send(intents, Intent::Login(Some(provider.clone())));
				}
				host.overlay = None;
			},
			_ => {},
		},
		OverlayEvent::Palette(action) => match action {
			PaletteAction::Intent(intent) => {
				let exit = match &intent {
					Intent::Quit => Some(HostExit::Quit),
					Intent::Resume(Some(id)) if exit_on_session_change => {
						Some(HostExit::Resume(id.clone()))
					},
					Intent::NewSession if exit_on_session_change => Some(HostExit::NewSession),
					_ => None,
				};
				send(intents, intent);
				host.overlay = None;
				if exit.is_some() {
					return exit;
				}
			},
			PaletteAction::OpenModelPicker => host.open_models(ctx),
			PaletteAction::ToggleSidebar => {
				host.sidebar.toggle();
				host.chat.set_right_inset(host.sidebar.reserved(viewport));
				host.overlay = None;
			},
			PaletteAction::Insert(text) => {
				host.chat.set_composer_text(&text);
				host.overlay = None;
			},
		},
		OverlayEvent::Prompt(value) => {
			send(intents, Intent::AuthAnswer { value: value.to_string() });
			host.overlay = None;
		},
		OverlayEvent::PromptCancel => {
			send(intents, Intent::AuthCancel);
			host.overlay = None;
		},
		OverlayEvent::ApprovalCancel => {
			host.overlay = None;
		},
		OverlayEvent::ApprovalDecide { ticket_id, action } => {
			send(intents, Intent::Approval { ticket_id, action });
			host.overlay = None;
		},
		OverlayEvent::ApprovalAmend(value) => match host.overlay.take() {
			Some(Overlay::Approval(approval)) => {
				let ticket_id = approval.ticket_id().clone();
				host.overlay = Some(Overlay::ApprovalAmend {
					prompt: PromptOverlay::open("Amended exact command or subject", false, ctx),
					ticket_id,
				});
			},
			Some(Overlay::ApprovalAmend { ticket_id, .. }) => {
				send(intents, Intent::Approval { ticket_id, action: ApprovalAction::Amend(value) });
			},
			overlay => host.overlay = overlay,
		},
		OverlayEvent::AskSubmit(values) => {
			if let Some(Overlay::Ask { request, .. }) = host.overlay.take() {
				let id = request.question.id.clone();
				request.answer(omp_tools::ask::Answer { id, selected: values, timed_out: false });
			}
		},
		OverlayEvent::AskCancel => {
			if let Some(Overlay::Ask { request, .. }) = host.overlay.take() {
				request.fail("Ask dialog cancelled");
			}
		},
		OverlayEvent::AutoQaConsent(decision) => {
			if let Some(Overlay::AutoQaConsent { consent, .. }) = host.overlay.take() {
				send(intents, Intent::AutoQaConsent(consent.decide(decision)));
			}
		},
		OverlayEvent::SettingsPreview(changes) => {
			send(intents, Intent::ApplySettings { changes, commit: false });
		},
		OverlayEvent::SettingsCommit(changes) => {
			send(intents, Intent::ApplySettings { changes, commit: true });
			host.overlay = None;
		},
		OverlayEvent::Selection(purpose, key) => {
			send(intents, Intent::Select { purpose, key });
			host.overlay = None;
		},
	}
	if host.overlay.is_none()
		&& let Some(request) = host.autoqa_queue.pop_front()
	{
		open_autoqa_consent(host, request, ctx);
	}
	None
}

fn palette_entries() -> Vec<PaletteEntry> {
	vec![
		PaletteEntry::new(
			"Switch model",
			"Choose the model for the next turn",
			PaletteAction::OpenModelPicker,
		)
		.key("Alt+P"),
		PaletteEntry::new(
			"Toggle sidebar",
			"Show or hide session facts",
			PaletteAction::ToggleSidebar,
		)
		.key("Ctrl+B"),
		PaletteEntry::new(
			"Resume session",
			"Open recent sessions",
			PaletteAction::Intent(Intent::Resume(None)),
		),
		PaletteEntry::new(
			"Login",
			"Authenticate a provider",
			PaletteAction::Intent(Intent::Login(None)),
		),
		PaletteEntry::new(
			"Inspect history",
			"Search or scroll canonical committed history",
			PaletteAction::Intent(Intent::InspectHistory),
		)
		.key("Alt+H"),
		PaletteEntry::new("Help", "Show chat controls", PaletteAction::Intent(Intent::Help)),
		PaletteEntry::new("Quit", "Leave chat", PaletteAction::Intent(Intent::Quit)),
	]
}

fn send(intents: &Sender<Intent>, intent: Intent) {
	let _ = intents.send(intent);
}

fn observe_resize(
	terminal: &mut Terminal,
	viewport: &mut Size,
	resize: &mut Option<ResizeState>,
	observed_at: Instant,
) -> io::Result<()> {
	let Some(size) = terminal.take_resize()? else {
		return Ok(());
	};
	if size == *viewport && resize.is_none() {
		return Ok(());
	}
	*viewport = size;
	match resize {
		Some(state) => state.observe(observed_at),
		None => *resize = Some(ResizeState::new(observed_at)),
	}
	Ok(())
}

fn user_event(
	terminal: &mut Terminal,
	renderer: &mut Renderer<TtyOut>,
	event: InputEvent,
) -> io::Result<Option<InputEvent>> {
	if terminal.handle_input_event(&event, renderer)? {
		return Ok(terminal.take_paste().and_then(|pasted| {
			let text = match pasted {
				Pasted::Text(text) => text,
				Pasted::Image(image) => image.persist().ok()?.display().to_string().into(),
			};
			Some(InputEvent::Paste(text))
		}));
	}
	Ok(Some(event))
}

fn clipboard_paste_text(clipboard: Clipboard) -> Option<String> {
	match clipboard {
		Clipboard::Text(text) => Some(text),
		Clipboard::Image(image) => Some(image.persist().ok()?.display().to_string()),
		Clipboard::Paths(paths) => Some(
			paths
				.iter()
				.map(|path| format!("\"{path}\""))
				.collect::<Vec<_>>()
				.join(" "),
		),
	}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PaintKind {
	Presented,
	Retired,
	/// A geometry change forced a history-neutral present before the pending
	/// retirement; repaint immediately to retire.
	Deferred,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Retirement {
	Disabled,
	Pressure,
	Flush,
}

fn start_pending_replay<W: Write>(
	_renderer: &mut Renderer<W>,
	host: &mut ChatHost,
	pending: &mut Option<ResizeScrollback>,
) -> io::Result<()> {
	if host.overlay.is_some() {
		return Ok(());
	}
	let Some(mode) = pending.take() else {
		return Ok(());
	};
	let mode = match mode {
		ResizeScrollback::Append => HistoryReplay::Append,
		ResizeScrollback::Rebuild => HistoryReplay::Rebuild,
		ResizeScrollback::Preserve => return Ok(()),
	};
	host.chat.begin_replay(mode);
	Ok(())
}

fn paint_host<W: Write>(
	renderer: &mut Renderer<W>,
	host: &mut ChatHost,
	viewport: Size,
	retirement: Retirement,
) -> io::Result<PaintKind> {
	let may_retire = retirement != Retirement::Disabled && host.overlay.is_none();
	let geometry_gate =
		may_retire && renderer.retire_requires_present(viewport.width, viewport.height);
	let batch = if geometry_gate {
		None
	} else {
		match retirement {
			Retirement::Disabled => None,
			Retirement::Pressure if host.overlay.is_none() => host.chat.retirement_batch(viewport),
			Retirement::Flush if host.overlay.is_none() => host.chat.flush_retirement_batch(viewport),
			Retirement::Pressure | Retirement::Flush => None,
		}
	};
	let rendered = match batch.as_ref() {
		Some(batch) => host.chat.render_after_retirement(viewport, batch),
		None => host.chat.render(viewport),
	};
	let mut layers = rail_layers(&mut host.sidebar, viewport);
	if let Some(overlay) = host.overlay.as_mut() {
		layers.push(overlay.layer(viewport));
	}
	let Some(batch) = batch else {
		renderer.present_damaged(
			rendered.frame,
			rendered.damage.as_slice(),
			viewport.height,
			&layers,
		)?;
		return Ok(if geometry_gate {
			PaintKind::Deferred
		} else {
			PaintKind::Presented
		});
	};
	if let Some((mode, frames)) = batch.replay_plan() {
		renderer.replay_frames(frames, rendered.frame, viewport.height, &layers, mode)?;
	} else if batch.frame.size().height == 0 {
		host.chat.mark_retired(&batch);
		return Ok(PaintKind::Retired);
	} else {
		renderer.retire(&batch.frame, rendered.frame, viewport.height, &layers)?;
	}
	host.chat.mark_retired(&batch);
	Ok(PaintKind::Retired)
}

fn open_overlay(
	terminal: &mut Terminal,
	renderer: &mut Renderer<TtyOut>,
	host: &mut ChatHost,
	viewport: Size,
	_resize: &mut Option<ResizeState>,
) -> io::Result<()> {
	if matches!(host.overlay.as_ref(), Some(Overlay::Git(_))) && host.saved_git_keymap.is_none() {
		host.saved_git_keymap = Some(terminal.keymap().clone());
		terminal.edit_keymap(|keymap| {
			for mods in [Mods { alt: true, ..Mods::default() }, Mods {
				alt: true,
				super_key: true,
				..Mods::default()
			}] {
				keymap.bind(Chord::new(Key::Up, mods), Key::JumpPrevious);
				keymap.bind(Chord::new(Key::Down, mods), Key::JumpNext);
			}
		});
	}
	let alt_enter = terminal.stage_alt_enter(AltScreenUse::Interactive);
	let rendered = host.chat.render(viewport);
	let mut layers = rail_layers(&mut host.sidebar, viewport);
	layers.push(
		host
			.overlay
			.as_mut()
			.expect("overlay opened")
			.layer(viewport),
	);
	renderer
		.repaint(alt_enter.as_deref().unwrap_or(""), rendered.frame.clone(), viewport.height, &layers)
		.map(|_| ())
}

fn close_overlay(
	terminal: &mut Terminal,
	renderer: &mut Renderer<TtyOut>,
	host: &mut ChatHost,
	viewport: Size,
	_resize: &mut Option<ResizeState>,
) -> io::Result<()> {
	if let Some(saved) = host.saved_git_keymap.take() {
		terminal.edit_keymap(|keymap| *keymap = saved);
	}
	let rendered = host.chat.render(viewport);
	let layers = rail_layers(&mut host.sidebar, viewport);
	let alt_exit = terminal.stage_alt_leave().unwrap_or("");
	renderer
		.repaint(alt_exit, rendered.frame.clone(), viewport.height, &layers)
		.map(|_| ())
}

fn chat_deadline(chat: &Chat) -> Option<Instant> {
	chat.next_wake().map(|delay| Instant::now() + delay)
}

async fn deadline(executor: &Executor, at: Option<Instant>) {
	match at {
		Some(at) => {
			executor
				.timer(at.saturating_duration_since(Instant::now()))
				.await
		},
		None => future::pending().await,
	}
}

#[cfg(test)]
mod tests {
	use std::{
		cell::Cell,
		io::{self, Write},
		rc::Rc,
	};

	use omp_core::sf;
	use omp_tui::{Frame, Key, Renderer, Size, UiContext};

	use super::{
		ChatHost, Duration, HostExit, HostOptions, Instant, Overlay, PaintKind, ResizeScrollback,
		ResizeState, RetainedChat, RetainedChatEffect, Retirement, paint_host, start_pending_replay,
	};
	use crate::{BackendEvent, Chat, HistoryInspector, Intent, ModelRow};

	#[test]
	fn resize_settle_window_restarts_at_each_event() {
		let started_at = Instant::now();
		let mut state = ResizeState::new(started_at);
		state.observe(started_at + Duration::from_millis(100));
		assert!(!state.settled(started_at + Duration::from_millis(219)));
		assert!(state.settled(started_at + Duration::from_millis(220)));
	}
	#[test]
	fn retained_chat_exits_for_a_backend_session_transition() {
		let ctx = UiContext::default();
		let (events, receiver) = flume::unbounded();
		let (intents, _requests) = flume::unbounded();
		let mut chat = RetainedChat::new(
			Chat::new(&ctx),
			ctx,
			receiver,
			intents,
			HostOptions::default(),
			Default::default(),
		);
		events
			.send(BackendEvent::NewSessionRequested)
			.expect("retained chat receiver remains connected");

		assert_eq!(chat.poll(), RetainedChatEffect::Quit(HostExit::NewSession));
	}
	#[test]
	fn retained_chat_opens_its_sidebar_only_on_ctrl_b() {
		let ctx = UiContext::default();
		let (_events, receiver) = flume::unbounded();
		let (intents, _requests) = flume::unbounded();
		let mut chat = RetainedChat::new(
			Chat::new(&ctx),
			ctx,
			receiver,
			intents,
			HostOptions::default(),
			Default::default(),
		);
		chat.resize(Size::new(120, 30), true);

		assert!(chat.render().layers.is_empty());
		assert_eq!(chat.key(Key::Ctrl('b')), RetainedChatEffect::Consumed);
		assert_eq!(chat.render().layers.len(), 1);
	}

	#[test]
	fn retained_model_picker_commits_the_next_model() {
		let ctx = UiContext::default();
		let (events, receiver) = flume::unbounded();
		let (intents, requests) = flume::unbounded();
		let mut chat = RetainedChat::new(
			Chat::new(&ctx),
			ctx,
			receiver,
			intents,
			HostOptions::default(),
			Default::default(),
		);
		let row = |key: &'static str, name: &'static str| ModelRow {
			key:         sf!(key),
			name:        sf!(name),
			provider_id: sf!("provider"),
			provider:    sf!("Provider"),
			context:     None,
			input_mtok:  None,
			output_mtok: None,
		};
		events
			.send(BackendEvent::ModelsUpdated {
				rows:    vec![row("provider/first", "First"), row("provider/second", "Second")],
				current: 0,
			})
			.expect("retained chat receiver remains connected");

		assert_eq!(chat.poll(), RetainedChatEffect::Consumed);
		assert_eq!(chat.key(Key::Alt('p')), RetainedChatEffect::Consumed);
		assert_eq!(chat.key(Key::Down), RetainedChatEffect::Consumed);
		assert_eq!(chat.key(Key::Enter), RetainedChatEffect::Consumed);

		let intent = requests.try_recv().expect("model pick emits an intent");
		let Intent::SwitchModel(model) = intent else {
			panic!("model pick emitted the wrong intent");
		};
		assert_eq!(model, "provider/second");
		assert!(chat.render().layers.is_empty());
	}

	fn finalized_host(ctx: &UiContext, viewport: Size) -> ChatHost {
		let mut chat = Chat::new(ctx);
		// Enough finalized rows to overflow the 40x8 live region, forcing a
		// capacity-pressure retirement offer.
		for index in 0..6 {
			chat.push_notice(format!("finalized {index}"));
		}
		ChatHost::new(chat, ctx, viewport, Vec::new(), 0, false)
	}

	#[test]
	fn present_only_tick_leaves_finalized_blocks_pending() {
		let viewport = Size::new(40, 8);
		let ctx = UiContext::default();
		let mut host = finalized_host(&ctx, viewport);
		let mut renderer = Renderer::new(Vec::new());

		assert_eq!(
			paint_host(&mut renderer, &mut host, viewport, Retirement::Disabled).unwrap(),
			PaintKind::Presented
		);
		assert!(host.chat.retirement_batch(viewport).is_some());
	}

	#[test]
	fn width_change_defers_retirement_until_the_viewport_repaints() {
		let viewport = Size::new(40, 8);
		let ctx = UiContext::default();
		let mut host = finalized_host(&ctx, viewport);
		let mut renderer = Renderer::new(Vec::new());
		paint_host(&mut renderer, &mut host, viewport, Retirement::Disabled).unwrap();

		// Retirement scrolls relative to the painted viewport, so a geometry
		// change presents once before the pending batch retires.
		let resized = Size::new(60, 8);
		assert_eq!(
			paint_host(&mut renderer, &mut host, resized, Retirement::Pressure).unwrap(),
			PaintKind::Deferred
		);
		assert_eq!(
			paint_host(&mut renderer, &mut host, resized, Retirement::Pressure).unwrap(),
			PaintKind::Retired
		);
	}

	#[test]
	fn finalized_prefix_retires_once_and_advances_frontier() {
		let viewport = Size::new(40, 8);
		let ctx = UiContext::default();
		let mut host = finalized_host(&ctx, viewport);
		let mut renderer = Renderer::new(Vec::new());
		paint_host(&mut renderer, &mut host, viewport, Retirement::Disabled).unwrap();

		assert_eq!(
			paint_host(&mut renderer, &mut host, viewport, Retirement::Pressure).unwrap(),
			PaintKind::Retired
		);
		assert!(host.chat.retirement_batch(viewport).is_none());
		assert_eq!(
			paint_host(&mut renderer, &mut host, viewport, Retirement::Pressure).unwrap(),
			PaintKind::Presented
		);
	}

	#[test]
	fn flush_retires_a_finalized_tail_without_pressure() {
		let viewport = Size::new(40, 20);
		let ctx = UiContext::default();
		let mut chat = Chat::new(&ctx);
		chat.push_notice("fits in the viewport");
		let mut host = ChatHost::new(chat, &ctx, viewport, Vec::new(), 0, false);
		let mut renderer = Renderer::new(Vec::new());
		paint_host(&mut renderer, &mut host, viewport, Retirement::Disabled).unwrap();

		assert_eq!(
			paint_host(&mut renderer, &mut host, viewport, Retirement::Pressure).unwrap(),
			PaintKind::Presented
		);
		assert_eq!(
			paint_host(&mut renderer, &mut host, viewport, Retirement::Flush).unwrap(),
			PaintKind::Retired
		);
	}

	#[test]
	fn replay_request_survives_an_overlay() {
		let viewport = Size::new(40, 20);
		let ctx = UiContext::default();
		let mut chat = Chat::new(&ctx);
		chat.push_notice("committed row");
		let mut host = ChatHost::new(chat, &ctx, viewport, Vec::new(), 0, false);
		let mut renderer = Renderer::new(Vec::new());
		paint_host(&mut renderer, &mut host, viewport, Retirement::Disabled).unwrap();
		paint_host(&mut renderer, &mut host, viewport, Retirement::Flush).unwrap();
		host.overlay = Some(Overlay::History(HistoryInspector::open(Frame::new(viewport))));
		let mut pending = Some(ResizeScrollback::Append);

		start_pending_replay(&mut renderer, &mut host, &mut pending).unwrap();
		assert_eq!(pending, Some(ResizeScrollback::Append));
		host.overlay = None;
		start_pending_replay(&mut renderer, &mut host, &mut pending).unwrap();
		assert_eq!(pending, None);
		assert_eq!(
			paint_host(&mut renderer, &mut host, viewport, Retirement::Pressure).unwrap(),
			PaintKind::Retired
		);
	}

	#[test]
	fn retirement_waits_for_alt_overlay_and_runs_after_close() {
		let viewport = Size::new(40, 8);
		let ctx = UiContext::default();
		let mut host = finalized_host(&ctx, viewport);
		let mut renderer = Renderer::new(Vec::new());
		paint_host(&mut renderer, &mut host, viewport, Retirement::Disabled).unwrap();
		host.overlay = Some(Overlay::History(HistoryInspector::open(Frame::new(viewport))));

		assert_eq!(
			paint_host(&mut renderer, &mut host, viewport, Retirement::Pressure).unwrap(),
			PaintKind::Presented
		);
		assert!(host.chat.retirement_batch(viewport).is_some());

		host.overlay = None;
		assert_eq!(
			paint_host(&mut renderer, &mut host, viewport, Retirement::Pressure).unwrap(),
			PaintKind::Retired
		);
		assert!(host.chat.retirement_batch(viewport).is_none());
	}

	#[derive(Clone, Default)]
	struct WriteControl {
		fail:   Rc<Cell<bool>>,
		writes: Rc<Cell<usize>>,
	}

	struct SwitchWriter(WriteControl);

	impl Write for SwitchWriter {
		fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
			self.0.writes.set(self.0.writes.get() + 1);
			if self.0.fail.get() {
				Err(io::Error::other("surface lost"))
			} else {
				Ok(bytes.len())
			}
		}

		fn flush(&mut self) -> io::Result<()> {
			Ok(())
		}
	}

	#[test]
	fn retirement_write_error_is_fatal_without_advancing_or_retrying() {
		let viewport = Size::new(40, 8);
		let ctx = UiContext::default();
		let mut host = finalized_host(&ctx, viewport);
		let control = WriteControl::default();
		let mut renderer = Renderer::new(SwitchWriter(control.clone()));
		paint_host(&mut renderer, &mut host, viewport, Retirement::Disabled).unwrap();
		control.fail.set(true);
		let writes_before = control.writes.get();

		let error = paint_host(&mut renderer, &mut host, viewport, Retirement::Pressure)
			.expect_err("retirement write failure must escape the host coordinator");

		assert_eq!(error.kind(), io::ErrorKind::Other);
		assert_eq!(control.writes.get(), writes_before + 1);
		assert!(host.chat.retirement_batch(viewport).is_some());
	}
}
