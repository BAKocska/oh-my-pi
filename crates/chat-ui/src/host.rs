//! Terminal host for the host-agnostic immediate-mode chat scene.

use std::{collections::VecDeque, io, io::Write, time::Duration};

use flume::{Receiver, Sender};
use omp_core::{Str, sf};
use omp_tui::{
	AltScreenUse, CursorStyle, DebugOp, DebugQuery, InputEvent, Key, Layer, Mouse, PaintStats,
	Pasted, Renderer, Size, Terminal, TerminalEvent, TerminalOptions, TtyOut, UiContext, detect,
	paste::{self, Clipboard, ClipboardRead},
};
use smallvec::SmallVec;
use tokio::{sync::oneshot, time::Instant};

use crate::{
	ApprovalAction, BackendEvent, Chat, ChatKey, CommandPalette, Intent, ListPicker, ListRow,
	ModelPicker, ModelRow, PaletteAction, PaletteEntry, PaletteEvent, PickerEvent, PromptEvent,
	PromptOverlay, ProviderPicker, PtyEvent, PtyOverlay, RenderedFrame, RewindTargetRow, SessionRow,
	Sidebar, Welcome, WelcomeEvent,
	approval::{ApprovalEvent, ApprovalOverlay},
	ask::{self, AskDialog, AskDialogEvent, AskRequest},
};

const RESIZE_SETTLE: Duration = Duration::from_millis(120);
const DOUBLE_ESC: Duration = Duration::from_millis(500);
const PASTE_READ_TIMEOUT: Duration = Duration::from_secs(10);

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

struct PasteRead {
	clipboard:  oneshot::Receiver<Option<Clipboard>>,
	scope:      ClipboardRead,
	abandon_at: Instant,
}

impl PasteRead {
	fn start(scope: ClipboardRead) -> Self {
		Self {
			clipboard: paste::spawn_clipboard_read(scope),
			scope,
			abandon_at: Instant::now() + PASTE_READ_TIMEOUT,
		}
	}
}

/// Terminal-host lifecycle controls.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostOptions {
	/// Whether to show the welcome session index before entering chat.
	pub welcome:                bool,
	/// Whether session-changing actions return to the caller for reconstruction.
	pub exit_on_session_change: bool,
}

impl Default for HostOptions {
	fn default() -> Self {
		Self { welcome: true, exit_on_session_change: true }
	}
}

/// Reason the terminal host returned to its production caller.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostExit {
	/// The user or backend closed the host.
	Quit,
	/// Rebuild the agent around this session.
	Resume(Str),
	/// Build a fresh agent session.
	NewSession,
}

/// Terminal host result with the final unsent composer draft.
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
	chat: Chat,
	ctx: UiContext,
	events: Receiver<BackendEvent>,
	intents: Sender<Intent>,
) -> io::Result<()> {
	run_with_options(chat, ctx, events, intents, HostOptions {
		welcome:                true,
		exit_on_session_change: false,
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
	chat: Chat,
	ctx: UiContext,
	events: Receiver<BackendEvent>,
	intents: Sender<Intent>,
	options: HostOptions,
) -> io::Result<HostExit> {
	run_with_draft(chat, ctx, events, intents, options, Str::default())
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
	mut chat: Chat,
	ctx: UiContext,
	events: Receiver<BackendEvent>,
	intents: Sender<Intent>,
	options: HostOptions,
	initial_draft: Str,
) -> io::Result<HostOutcome> {
	chat.set_composer_text(initial_draft.as_str());
	let caps = detect();
	let mut terminal =
		Terminal::enter(TerminalOptions::new(caps).cursor_style(CursorStyle::BlinkingBar))?;
	let mut renderer = Renderer::new(TtyOut::new()?);
	renderer.apply_caps(&caps)?;
	let result =
		run_with_terminal(&mut terminal, &mut renderer, chat, &ctx, &events, &intents, options).await;
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
		terminal,
		renderer,
		ctx,
		viewport,
		chat,
		models,
		current_model,
		events,
		intents,
		options.exit_on_session_change,
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
	loop {
		if let Some(size) = terminal.take_resize()? {
			*viewport = size;
		}
		renderer.preview(
			welcome.render(*viewport, Duration::ZERO),
			viewport.height,
			alt_enter.take().as_deref().unwrap_or(""),
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
	chat:              Chat,
	sidebar:           Sidebar,
	overlay:           Option<Overlay>,
	models:            Vec<ModelRow>,
	current_model:     usize,
	last_esc:          Option<Instant>,
	pending_approvals: usize,
	approval_queue:    VecDeque<crate::ApprovalTicketView>,
}

impl ChatHost {
	fn new(
		mut chat: Chat,
		ctx: &UiContext,
		viewport: Size,
		models: Vec<ModelRow>,
		current_model: usize,
	) -> Self {
		let status = chat.status();
		let sidebar = Sidebar::new(&status, ctx);
		chat.set_right_inset(sidebar.reserved(viewport));
		Self {
			chat,
			sidebar,
			overlay: None,
			models,
			current_model,
			last_esc: None,
			pending_approvals: 0,
			approval_queue: VecDeque::new(),
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
}

fn rail_layers(sidebar: &mut Sidebar, viewport: Size) -> SmallVec<Layer<'_>, 2> {
	sidebar
		.layer(viewport, Instant::now().into())
		.into_iter()
		.collect()
}
enum ListPurpose {
	Resume,
	Rewind,
	Settings,
	Agents,
	Pause,
}

enum Overlay {
	Models(ModelPicker),
	Pty(PtyOverlay),
	Palette(CommandPalette),
	List { picker: ListPicker, rows: Vec<ListRow>, prefill: Vec<Str>, purpose: ListPurpose },
	Providers(ProviderPicker),
	Prompt(PromptOverlay),
	Approval(ApprovalOverlay),
	ApprovalAmend { prompt: PromptOverlay, ticket_id: Str },
	Ask { dialog: AskDialog, request: AskRequest },
}

enum OverlayEvent {
	Consumed,
	PtyInput { id: Str, data: bytes::Bytes },
	PtyKill { id: Str },
	Close,
	Pick(usize),
	Palette(PaletteAction),
	PromptCancel,
	Prompt(Str),
	ApprovalCancel,
	ApprovalDecide { ticket_id: Str, action: ApprovalAction },
	ApprovalAmend(Str),
	AskCancel,
	AskSubmit(Vec<Str>),
}

impl Overlay {
	fn handle_key(&mut self, key: Key) -> OverlayEvent {
		match self {
			Self::Models(picker) => picker_event(picker.handle_key(key)),
			Self::Pty(pty) => match pty.handle_key(key) {
				PtyEvent::Input(data) => OverlayEvent::PtyInput { id: pty.id().clone(), data },
				PtyEvent::ForceKill => OverlayEvent::PtyKill { id: pty.id().clone() },
				PtyEvent::Close => OverlayEvent::Close,
				PtyEvent::Consumed => OverlayEvent::Consumed,
			},
			Self::Palette(palette) => palette_event(palette.handle_key(key)),
			Self::List { picker, .. } => picker_event(picker.handle_key(key)),
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
		}
	}

	fn handle_paste(&mut self, text: &str) -> OverlayEvent {
		match self {
			Self::Models(picker) => picker_event(picker.handle_paste(text)),
			Self::Pty(pty) => match pty.handle_paste(text) {
				PtyEvent::Input(data) => OverlayEvent::PtyInput { id: pty.id().clone(), data },
				PtyEvent::ForceKill => OverlayEvent::PtyKill { id: pty.id().clone() },
				PtyEvent::Close => OverlayEvent::Close,
				PtyEvent::Consumed => OverlayEvent::Consumed,
			},
			Self::Palette(palette) => palette_event(palette.handle_paste(text)),
			Self::List { picker, .. } => picker_event(picker.handle_paste(text)),
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
		}
	}

	fn handle_mouse(&mut self, col: u16, row: u16, kind: Mouse, viewport: Size) -> OverlayEvent {
		match self {
			Self::Models(picker) => picker_event(picker.handle_mouse(col, row, kind, viewport)),
			Self::Pty(_) => OverlayEvent::Consumed,
			Self::Palette(palette) => palette_event(palette.handle_mouse(col, row, kind, viewport)),
			Self::List { picker, .. } => picker_event(picker.handle_mouse(col, row, kind, viewport)),
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
		}
	}

	fn layer(&mut self, viewport: Size) -> Layer<'_> {
		match self {
			Self::Models(picker) => picker.layer(viewport),
			Self::Pty(pty) => pty.layer(viewport),
			Self::Palette(palette) => palette.layer(viewport),
			Self::List { picker, .. } => picker.layer(viewport),
			Self::Providers(picker) => picker.layer(viewport),
			Self::Prompt(prompt) => prompt.layer(viewport),
			Self::Approval(approval) => approval.layer(viewport),
			Self::ApprovalAmend { prompt, .. } => prompt.layer(viewport),
			Self::Ask { dialog, .. } => dialog.layer(viewport),
		}
	}
}

const fn picker_event(event: PickerEvent) -> OverlayEvent {
	match event {
		PickerEvent::Consumed => OverlayEvent::Consumed,
		PickerEvent::Close => OverlayEvent::Close,
		PickerEvent::Pick(index) => OverlayEvent::Pick(index),
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

#[derive(Clone, Copy)]
struct ResizeState {
	last_event:    Instant,
	width_changed: bool,
}

impl ResizeState {
	const fn new(last_event: Instant, width_changed: bool) -> Self {
		Self { last_event, width_changed }
	}

	const fn observe(&mut self, observed_at: Instant, width_changed: bool) {
		self.last_event = observed_at;
		self.width_changed |= width_changed;
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
	terminal: &mut Terminal,
	renderer: &mut Renderer<TtyOut>,
	ctx: &UiContext,
	mut viewport: Size,
	chat: Chat,
	models: Vec<ModelRow>,
	current_model: usize,
	events: &Receiver<BackendEvent>,
	intents: &Sender<Intent>,
	exit_on_session_change: bool,
) -> io::Result<HostOutcome> {
	let mut host = ChatHost::new(chat, ctx, viewport, models, current_model);
	let ask_binding = ask::bind();
	{
		let rendered = host.chat.render(viewport);
		let layers = rail_layers(&mut host.sidebar, viewport);
		present(renderer, rendered, viewport, &layers)?;
	}

	let mut drag_alt = false;
	let mut overlay_stale = false;
	let mut resize = None;
	let mut paste_read: Option<PasteRead> = None;
	let mut next_frame = chat_deadline(&host.chat);
	loop {
		let paste_deadline = paste_read.as_ref().map(|read| read.abandon_at);
		tokio::select! {
			event = terminal.next(), if paste_read.is_none() => match event? {
				TerminalEvent::Resize => {
					let resized = observe_resize(terminal, &mut viewport, &mut resize, Instant::now())?;
					host.chat.set_right_inset(host.sidebar.reserved(viewport));
					if host.overlay.is_some() && resized { overlay_stale = true; }
					send_pty_resize(&mut host, viewport, intents);
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
									close_overlay(terminal, renderer, &mut host, viewport, &mut overlay_stale, &mut resize)?;
								}
							} else if key == Key::Ctrl('b') {
								host.sidebar.toggle();
								host.chat.set_right_inset(host.sidebar.reserved(viewport));
							} else if key == Key::Ctrl('k') {
								host.overlay = Some(Overlay::Palette(CommandPalette::open(palette_entries(), ctx)));
								open_overlay(terminal, renderer, &mut host, viewport, &mut drag_alt, &mut overlay_stale, &mut resize)?;
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
									&mut drag_alt,
									&mut overlay_stale,
									&mut resize,
								)?;
							} else if key == Key::Ctrl('p') || key == Key::Alt('p') {
								host.open_models(ctx);
								if host.overlay.is_some() {
									open_overlay(terminal, renderer, &mut host, viewport, &mut drag_alt, &mut overlay_stale, &mut resize)?;
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
							} else {
								host.last_esc = None;
								let result = host.chat.handle_key(key);
								if let Some(text) = host.chat.take_copied() { terminal.copy_to_clipboard(&text)?; }
								while let Some((text, attachments, mode)) = host.chat.take_submission() {
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
									close_overlay(terminal, renderer, &mut host, viewport, &mut overlay_stale, &mut resize)?;
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
									close_overlay(terminal, renderer, &mut host, viewport, &mut overlay_stale, &mut resize)?;
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
							&mut drag_alt,
							&mut overlay_stale,
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
						BackendEvent::ApprovalPending(_) => terminal.set_title("Approval required · omp")?,
						BackendEvent::ApprovalSettled { .. } if host.pending_approvals <= 1 => {
							terminal.set_title("omp")?;
						},
						_ => {},
					}
					let had_overlay = host.overlay.is_some();
					apply_backend(&mut host, event, ctx);
					send_pty_resize(&mut host, viewport, intents);
					if !had_overlay && host.overlay.is_some() {
						open_overlay(terminal, renderer, &mut host, viewport, &mut drag_alt, &mut overlay_stale, &mut resize)?;
					} else if had_overlay && host.overlay.is_none() {
						close_overlay(terminal, renderer, &mut host, viewport, &mut overlay_stale, &mut resize)?;
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
			() = deadline(paste_deadline) => paste_read = None,
			() = deadline(next_frame) => {
				let now = Instant::now();
				let resized = observe_resize(terminal, &mut viewport, &mut resize, now)?;
				host.chat.set_right_inset(host.sidebar.reserved(viewport));
				if host.overlay.is_some() && resized { overlay_stale = true; }
				if resize.is_some() {
					let preview = host.chat.render_resize_preview(viewport);
					let mut layers = rail_layers(&mut host.sidebar, viewport);
					if let Some(active) = host.overlay.as_mut() {
						layers.push(active.layer(viewport));
						renderer.preview_overlaid(&preview, &layers, viewport.height, "")?;
					} else {
						let width_changed = resize.is_some_and(|state| state.width_changed);
						let alt_enter = if drag_alt || !width_changed { None } else {
							let staged = terminal.stage_alt_enter(AltScreenUse::Resize);
							drag_alt = staged.is_some();
							staged
						};
						renderer.preview_overlaid(&preview, &layers, viewport.height, alt_enter.as_deref().unwrap_or(""))?;
					}
				} else if host.overlay.is_some() {
					let rendered = host.chat.render(viewport);
					let mut layers = rail_layers(&mut host.sidebar, viewport);
					layers.push(host.overlay.as_mut().expect("overlay present").layer(viewport));
					renderer.preview_overlaid(rendered.frame, &layers, viewport.height, "")?;
				} else {
					let rendered = host.chat.render(viewport);
					let layers = rail_layers(&mut host.sidebar, viewport);
					present(renderer, rendered, viewport, &layers)?;
				}
				next_frame = chat_deadline(&host.chat);
			},
			() = deadline(resize.map(ResizeState::deadline)) => {
				let now = Instant::now();
				if !resize.is_some_and(|state| state.settled(now)) { continue; }
				if host.overlay.is_some() {
					overlay_stale = true;
					resize = None;
					continue;
				}
				host.chat.set_right_inset(host.sidebar.reserved(viewport));
				let rendered = host.chat.render(viewport);
				let alt_exit = if drag_alt {
					drag_alt = false;
					terminal.stage_alt_leave().unwrap_or("")
				} else { "" };
				renderer.rebuild(rendered.frame.clone(), viewport.height, rendered.stable_rows, alt_exit)?;
				let layers = rail_layers(&mut host.sidebar, viewport);
				if !layers.is_empty() {
					renderer.present_overlaid(rendered.frame, &[], viewport.height, rendered.stable_rows, &layers)?;
				}
				resize = None;
				next_frame = chat_deadline(&host.chat);
			},
		}
	}
	Ok(host_outcome(&host, HostExit::Quit))
}

fn host_outcome(host: &ChatHost, exit: HostExit) -> HostOutcome {
	HostOutcome { exit, draft: Str::from(host.chat.composer_text()) }
}

fn apply_backend(host: &mut ChatHost, event: BackendEvent, ctx: &UiContext) {
	match event {
		BackendEvent::ApprovalPending(ticket) => {
			host.pending_approvals = host.pending_approvals.saturating_add(1);
			if matches!(host.overlay, Some(Overlay::Approval(_) | Overlay::ApprovalAmend { .. })) {
				host.approval_queue.push_back(ticket);
			} else {
				host.overlay = Some(Overlay::Approval(ApprovalOverlay::open(ticket, ctx)));
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
		BackendEvent::RewindTargets(rows) => open_rewind(host, rows, ctx),
		BackendEvent::AgentRoster(rows) => host.chat.set_agent_roster(rows),
		BackendEvent::SettingsSchema(rows) => open_settings(host, rows, ctx),
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
		.map(|row| ListRow { key: row.id, label: row.label, detail: row.detail })
		.collect();
	let picker = ListPicker::open("Resume session", &rows, 0, ctx);
	host.overlay =
		Some(Overlay::List { picker, rows, prefill: Vec::new(), purpose: ListPurpose::Resume });
}

fn open_login_providers(host: &mut ChatHost, providers: Vec<SessionRow>, ctx: &UiContext) {
	host.overlay = Some(Overlay::Providers(ProviderPicker::open(providers, ctx)));
}

fn open_settings(host: &mut ChatHost, fields: Vec<crate::SettingRow>, ctx: &UiContext) {
	let rows: Vec<ListRow> = fields
		.into_iter()
		.map(|field| {
			let detail = if field.secret {
				sf!("{} · secret value managed by inference", field.description)
			} else {
				sf!("{} · {}", field.kind, field.description)
			};
			ListRow { key: field.path, label: sf!("{} / {}", field.domain, field.label), detail }
		})
		.collect();
	let picker = ListPicker::open("Settings", &rows, 0, ctx);
	host.overlay =
		Some(Overlay::List { picker, rows, prefill: Vec::new(), purpose: ListPurpose::Settings });
}

fn open_agents(host: &mut ChatHost, ctx: &UiContext) {
	let rows: Vec<ListRow> = host
		.chat
		.agent_roster()
		.iter()
		.map(|agent| {
			let indent = "  ".repeat(usize::from(agent.depth));
			let detail = match (&agent.tool, agent.tokens) {
				(Some(tool), Some(tokens)) => {
					Str::new(format!("{} · {tool} · {tokens} tokens", agent.status))
				},
				(Some(tool), None) => Str::new(format!("{} · {tool}", agent.status)),
				(None, Some(tokens)) => Str::new(format!("{} · {tokens} tokens", agent.status)),
				(None, None) => agent.status.clone(),
			};
			ListRow {
				key: agent.id.clone(),
				label: Str::new(format!("{indent}{}", agent.name)),
				detail,
			}
		})
		.collect();
	let picker = ListPicker::open("Agents · live hierarchy", &rows, 0, ctx);
	host.overlay =
		Some(Overlay::List { picker, rows, prefill: Vec::new(), purpose: ListPurpose::Agents });
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
		OverlayEvent::PtyInput { id, data } => send(intents, Intent::PtyInput { id, data }),
		OverlayEvent::PtyKill { id } => send(intents, Intent::PtyKill { id }),
		OverlayEvent::Close => host.overlay = None,
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
						ListPurpose::Settings | ListPurpose::Agents | ListPurpose::Pause => {},
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
		.key("Ctrl+P"),
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
) -> io::Result<bool> {
	let Some(size) = terminal.take_resize()? else {
		return Ok(false);
	};
	if size == *viewport && resize.is_none() {
		return Ok(false);
	}
	let width_changed = size.width != viewport.width;
	*viewport = size;
	match resize {
		Some(state) => state.observe(observed_at, width_changed),
		None => *resize = Some(ResizeState::new(observed_at, width_changed)),
	}
	Ok(true)
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

fn present<W: Write>(
	renderer: &mut Renderer<W>,
	rendered: RenderedFrame<'_>,
	viewport: Size,
	layers: &[Layer<'_>],
) -> io::Result<PaintStats> {
	if rendered.stable_rows < renderer.committed_rows() {
		let stats =
			renderer.rebuild(rendered.frame.clone(), viewport.height, rendered.stable_rows, "")?;
		if !layers.is_empty() {
			renderer.present_overlaid(
				rendered.frame,
				&[],
				viewport.height,
				rendered.stable_rows,
				layers,
			)?;
		}
		return Ok(stats);
	}
	match renderer.present_overlaid(
		rendered.frame,
		rendered.damage.as_slice(),
		viewport.height,
		rendered.stable_rows,
		layers,
	) {
		Ok(stats) => Ok(stats),
		Err(error) if Renderer::<W>::is_stable_mutation_error(&error) => {
			tracing::warn!(
				error = %error,
				"chat scene mutated stable rows; rebuilding the renderer epoch once"
			);
			let stats =
				renderer.rebuild(rendered.frame.clone(), viewport.height, rendered.stable_rows, "")?;
			if !layers.is_empty() {
				renderer.present_overlaid(
					rendered.frame,
					&[],
					viewport.height,
					rendered.stable_rows,
					layers,
				)?;
			}
			Ok(stats)
		},
		Err(error) => Err(error),
	}
}

fn open_overlay(
	terminal: &mut Terminal,
	renderer: &mut Renderer<TtyOut>,
	host: &mut ChatHost,
	viewport: Size,
	drag_alt: &mut bool,
	overlay_stale: &mut bool,
	resize: &mut Option<ResizeState>,
) -> io::Result<()> {
	if *drag_alt || resize.take().is_some() {
		*drag_alt = false;
		*overlay_stale = true;
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
		.preview_overlaid(
			rendered.frame,
			&layers,
			viewport.height,
			alt_enter.as_deref().unwrap_or(""),
		)
		.map(|_| ())
}

fn close_overlay(
	terminal: &mut Terminal,
	renderer: &mut Renderer<TtyOut>,
	host: &mut ChatHost,
	viewport: Size,
	overlay_stale: &mut bool,
	resize: &mut Option<ResizeState>,
) -> io::Result<()> {
	*resize = None;
	let rendered = host.chat.render(viewport);
	let layers = rail_layers(&mut host.sidebar, viewport);
	if *overlay_stale || rendered.stable_rows < renderer.committed_rows() {
		*overlay_stale = false;
		let alt_exit = terminal.stage_alt_leave().unwrap_or("");
		renderer.rebuild(rendered.frame.clone(), viewport.height, rendered.stable_rows, alt_exit)?;
		if !layers.is_empty() {
			renderer.present_overlaid(
				rendered.frame,
				&[],
				viewport.height,
				rendered.stable_rows,
				&layers,
			)?;
		}
	} else {
		terminal.leave_alt()?;
		renderer.present_overlaid(
			rendered.frame,
			&[(0, rendered.frame.size().height)],
			viewport.height,
			rendered.stable_rows,
			&layers,
		)?;
	}
	Ok(())
}

fn chat_deadline(chat: &Chat) -> Option<Instant> {
	chat.next_wake().map(|delay| Instant::now() + delay)
}

async fn deadline(at: Option<Instant>) {
	match at {
		Some(at) => tokio::time::sleep_until(at).await,
		None => std::future::pending().await,
	}
}

#[cfg(test)]
mod tests {
	use omp_tui::{Rect, Renderer, Size, Style, UiContext};
	use smallvec::SmallVec;

	use super::{Duration, Instant, ResizeState, present};
	use crate::{Chat, RenderedFrame};

	#[test]
	fn resize_settle_window_restarts_at_each_event() {
		let started_at = Instant::now();
		let mut state = ResizeState::new(started_at, false);
		state.observe(started_at + Duration::from_millis(100), true);
		assert!(!state.settled(started_at + Duration::from_millis(219)));
		assert!(state.settled(started_at + Duration::from_millis(220)));
	}

	#[test]
	fn stable_mutation_rebuilds_once_instead_of_escaping_the_host() {
		let viewport = Size::new(40, 8);
		let mut chat = Chat::new(&UiContext::default());
		for index in 0..12 {
			chat.push_notice(format!("notice {index}"));
		}
		let mut renderer = Renderer::new(Vec::new());
		present(&mut renderer, chat.render(viewport), viewport, &[]).unwrap();
		assert!(renderer.committed_rows() > 0);

		let initial = chat.render(viewport);
		let mut changed = initial.frame.clone();
		changed.fill(Rect::new(0, 0, viewport.width, 1), Style::default());
		let mut damage = SmallVec::new();
		damage.push((0, 1));
		let rendered = RenderedFrame { frame: &changed, stable_rows: initial.stable_rows, damage };
		let recovered = present(&mut renderer, rendered, viewport, &[])
			.expect("stable mutation should trigger the one-time rebuild fallback");

		assert!(recovered.full_repaint);
	}

	#[test]
	fn stable_prefix_regression_rebuilds_visible_document() {
		let viewport = Size::new(40, 8);
		let mut chat = Chat::new(&UiContext::default());
		for index in 0..12 {
			chat.push_notice(format!("notice {index}"));
		}
		let mut renderer = Renderer::new(Vec::new());
		let initial = present(&mut renderer, chat.render(viewport), viewport, &[]).unwrap();
		assert!(initial.committed_rows > 0);
		assert!(renderer.committed_rows() > 0);

		chat.clear_history();
		let rendered = chat.render(viewport);
		assert_eq!(rendered.stable_rows, 0);
		let repainted = present(&mut renderer, rendered, viewport, &[]).unwrap();

		assert!(repainted.full_repaint);
		assert_eq!(renderer.committed_rows(), 0);
	}
}
