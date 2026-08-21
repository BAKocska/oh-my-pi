//! Immediate-mode chat scene with append-only transcript and mutable tail
//! chrome.

use std::{
	cell::RefCell,
	collections::VecDeque,
	fmt::Write as _,
	rc::Rc,
	time::{Duration, Instant},
};

use omp_core::{IntoStr, Str, StrMut, fmts_mut, sf};
use omp_tui::{
	Border, Charset, Color, Command, Component, Decor, DecorKind, Frame, Icon, Key, MouseReport,
	PaintCtx, Prop, Props, Rect, Size, SlashCommands, Slot, Style, Theme, Ui, UiContext, UiEvent,
	anim::{Easing, Shimmer, Tween},
	components::{
		Attachment, AttachmentContent, Attachments, ComposerStatusAttachment, ComposerStyle,
		ContextGaugeMode, EditorPane, Segment, Status, TextLeaf, advisor_spend_label,
		boundary_layout, collapse_hud_line, compaction_threshold_color, context_gauge_cells,
		hr::truncate_to_width, spend_label,
	},
	next_slot,
};
use smallvec::SmallVec;

use crate::{
	ActivityWaveform, AgentRow, BackendEvent, CompactionSpeculationStatus, StatusFacts, SubmitMode,
	TranscriptFrame, TranscriptFrameKind,
	slots::{Mount, Slots},
};

const MAX_LIVE_PANEL_CONTENT_ROWS: u16 = 12;
/// Column cap for inline tool-result images inside committed cards.
const TOOL_IMAGE_MAX_COLS: u16 = 64;
/// Row cap for inline tool-result images inside committed cards.
const TOOL_IMAGE_MAX_ROWS: u16 = 12;
const SHIMMER_PERIOD: Duration = Duration::from_millis(1900);
const BRAND_FADE: Duration = Duration::from_millis(450);
const FADE_FRAME: Duration = Duration::from_millis(40);
const SPECULATION_PULSE: Duration = Duration::from_millis(600);
const STATUS_ID: &str = "status";
const INPUT_ID: &str = "input";

/// One retained chat document update and its exact repainted row ranges.
pub struct RenderedFrame<'a> {
	/// Complete logical document frame.
	pub frame:       &'a Frame,
	/// Final transcript prefix safe for native scrollback commits.
	pub stable_rows: u16,
	/// Half-open logical row ranges changed since the previous render.
	pub damage:      SmallVec<(u16, u16), 4>,
}

/// Protocol placements used by [`Bands`] without allocating a per-frame
/// collection of rendered rows.
pub mod placement {
	/// Extension content above the transcript.
	pub const HEADER: i32 = 1;
	/// Extension content below the transcript.
	pub const FOOTER: i32 = 2;
	/// A left out-of-tree rail.
	pub const LEFT_RAIL: i32 = 3;
	/// A right out-of-tree rail.
	pub const RIGHT_RAIL: i32 = 4;
	/// Extension content above the editor.
	pub const ABOVE_EDITOR: i32 = 5;
	/// Extension content below the editor.
	pub const BELOW_EDITOR: i32 = 6;
}

/// Total columns consumed by all visible out-of-tree rails.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RailWidths {
	/// Sum of left rail widths.
	pub left:  u16,
	/// Sum of right rail widths.
	pub right: u16,
}

impl RailWidths {
	/// Adds one rail width to the requested side, saturating at terminal size.
	pub const fn accumulate(mut self, left: bool, width: u16) -> Self {
		if left {
			self.left = self.left.saturating_add(width);
		} else {
			self.right = self.right.saturating_add(width);
		}
		self
	}

	/// Returns columns remaining for the transcript.
	pub const fn content_width(self, viewport: u16) -> u16 {
		viewport.saturating_sub(self.left.saturating_add(self.right))
	}
}

/// Streaming compositor for extension bands and rails.
///
/// Each mount owns a retained [`Ui`]. Composition measures it and blits one
/// row at a time into the supplied [`RenderedFrame`] backing frame without a
/// frame-local line collection.
pub struct Bands;

impl Bands {
	/// Streams all visible extension mounts over `frame` and returns total rail
	/// reservation. Core callers pass their retained [`Slots`] registry.
	pub fn compose(frame: &mut Frame, slots: &mut Slots, viewport: Size) -> RailWidths {
		let mut rails = RailWidths::default();
		for mount in slots.mounts_at_mut(placement::LEFT_RAIL) {
			if mount.visible() {
				rails = rails.accumulate(true, mount.preferred_width().unwrap_or(0));
			}
		}
		for mount in slots.mounts_at_mut(placement::RIGHT_RAIL) {
			if mount.visible() {
				rails = rails.accumulate(false, mount.preferred_width().unwrap_or(0));
			}
		}
		let content = Rect::new(
			rails.left.min(viewport.width),
			0,
			rails.content_width(viewport.width),
			viewport.height,
		);
		let mut left = 0;
		Self::stream_rail(
			frame,
			slots.mounts_at_mut(placement::LEFT_RAIL),
			&mut left,
			true,
			viewport,
		);
		let mut right = viewport.width;
		Self::stream_rail(
			frame,
			slots.mounts_at_mut(placement::RIGHT_RAIL),
			&mut right,
			false,
			viewport,
		);
		let mut top = 0;
		Self::stream_stack(frame, slots.mounts_at_mut(placement::HEADER), &mut top, content);
		Self::stream_stack(frame, slots.mounts_at_mut(placement::ABOVE_EDITOR), &mut top, content);
		let mut bottom = viewport.height;
		Self::stream_stack_up(frame, slots.mounts_at_mut(placement::FOOTER), &mut bottom, content);
		Self::stream_stack_up(
			frame,
			slots.mounts_at_mut(placement::BELOW_EDITOR),
			&mut bottom,
			content,
		);
		rails
	}

	/// Composes extension layers, then paints core attribution in the reserved
	/// z-band above them.
	pub fn compose_with_attribution(
		frame: &mut Frame,
		slots: &mut Slots,
		viewport: Size,
		attribution: &Attribution,
		theme: Theme,
	) -> RailWidths {
		let rails = Self::compose(frame, slots, viewport);
		attribution.render(frame, viewport.width, theme);
		rails
	}

	fn stream_rail<'a>(
		frame: &mut Frame,
		mounts: impl Iterator<Item = &'a mut Mount>,
		cursor: &mut u16,
		left: bool,
		viewport: Size,
	) {
		for mount in mounts {
			if !mount.visible() {
				continue;
			}
			let width = mount
				.preferred_width()
				.unwrap_or(0)
				.min(viewport.width.saturating_sub(*cursor));
			if width == 0 {
				continue;
			}
			let x = if left {
				*cursor
			} else {
				cursor.saturating_sub(width)
			};
			mount.ui_mut().resize(width.max(1));
			let height = mount.ui_mut().frame().size().height.min(viewport.height);
			let rect = Rect::new(x, 0, width, height);
			mount.resolve(rect);
			Self::stream(frame, mount, rect);
			if left {
				*cursor = cursor.saturating_add(width);
			} else {
				*cursor = x;
			}
		}
	}

	fn stream_stack<'a>(
		frame: &mut Frame,
		mounts: impl Iterator<Item = &'a mut Mount>,
		cursor: &mut u16,
		content: Rect,
	) {
		for mount in mounts {
			if !mount.visible() || content.width == 0 {
				continue;
			}
			mount.ui_mut().resize(content.width);
			let height = mount
				.preferred_height()
				.unwrap_or(mount.ui_mut().frame().size().height);
			let height = height.min(content.height.saturating_sub(*cursor));
			let rect = Rect::new(content.x, *cursor, content.width, height);
			mount.resolve(rect);
			Self::stream(frame, mount, rect);
			*cursor = cursor.saturating_add(height);
		}
	}

	fn stream_stack_up<'a>(
		frame: &mut Frame,
		mounts: impl Iterator<Item = &'a mut Mount>,
		cursor: &mut u16,
		content: Rect,
	) {
		for mount in mounts {
			if !mount.visible() || content.width == 0 {
				continue;
			}
			mount.ui_mut().resize(content.width);
			let height = mount
				.preferred_height()
				.unwrap_or(mount.ui_mut().frame().size().height)
				.min(*cursor);
			*cursor = cursor.saturating_sub(height);
			let rect = Rect::new(content.x, *cursor, content.width, height);
			mount.resolve(rect);
			Self::stream(frame, mount, rect);
		}
	}

	fn stream(frame: &mut Frame, mount: &mut Mount, rect: Rect) {
		for row in 0..rect.height {
			frame.blit(mount.ui_mut().frame(), row, 1, rect.x, rect.y.saturating_add(row));
		}
	}
}

/// Core-owned provenance labels rendered above every extension layer.
///
/// This deliberately lives outside extension markup: `<approval>` and
/// `<attribution>` authored by extensions degrade through `MarkupOrigin`.
pub struct Attribution {
	septet: [Str; 7],
}

impl Attribution {
	/// Creates the reserved attribution band from its seven provenance fields.
	#[must_use]
	pub const fn new(septet: [Str; 7]) -> Self {
		Self { septet }
	}

	/// Streams the provenance septet into the reserved top z-band.
	pub fn render(&self, frame: &mut Frame, width: u16, theme: Theme) {
		let mut line = String::new();
		for item in &self.septet {
			if item.is_empty() {
				continue;
			}
			if !line.is_empty() {
				line.push_str(" · ");
			}
			line.push_str(item.as_str());
		}
		let _ = draw_line(frame, 0, 0, width, &[Span::new(&line, prose_style(theme))]);
	}
}

/// Result of routing one key through the focused composer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatKey {
	/// The composer handled the key.
	Consumed,
	/// The composer did not handle the key.
	Ignored,
	/// The scene requested host shutdown.
	Quit,
}

#[derive(Clone, Copy)]
struct Span<'a> {
	text:  &'a str,
	style: Style,
}

impl<'a> Span<'a> {
	const fn new(text: &'a str, style: Style) -> Self {
		Self { text, style }
	}
}

struct RichText {
	text:  String,
	width: u16,
	view:  Option<Ui>,
}

impl RichText {
	fn new(text: impl Into<String>, width: u16, ctx: &UiContext) -> Self {
		let text = text.into();
		let view = Self::view(&text, width, ctx);
		Self { text, width, view }
	}

	fn view(text: &str, width: u16, ctx: &UiContext) -> Option<Ui> {
		(!text.contains("</md>"))
			.then(|| Ui::from_markup(format!("<md>{text}</md>"), width, ctx.clone()).ok())
			.flatten()
	}

	fn resize(&mut self, width: u16, ctx: &UiContext) {
		if self.width != width {
			self.width = width;
			self.view = Self::view(&self.text, width, ctx);
		}
	}

	fn height(&self) -> u16 {
		self
			.view
			.as_ref()
			.map_or_else(|| explicit_line_count(&self.text), Ui::height)
	}
}

struct UserEntry {
	body:  RichText,
	chips: Vec<Str>,
}

/// One persisted tool-result image with its probed pixel dimensions.
struct ToolImageEntry {
	source: Str,
	px:     omp_tui::imagefmt::ImageDimensions,
}

struct ToolView {
	source:   Str,
	width:    u16,
	rendered: Ui,
}

impl ToolView {
	fn structured(source: Str, width: u16, ctx: &UiContext) -> Self {
		let rendered = Self::render(&source, width, ctx);
		Self { source, width, rendered }
	}

	fn render(source: &Str, width: u16, ctx: &UiContext) -> Ui {
		Ui::from_markup(source.clone(), width.max(1), ctx.clone()).unwrap_or_else(|_| {
			Ui::from_root(TextLeaf::new().text(source.clone()), width.max(1), ctx.clone())
		})
	}

	fn replace(&mut self, source: Str, ctx: &UiContext) {
		if self.source == source {
			return;
		}
		self.rendered = Self::render(&source, self.width, ctx);
		self.source = source;
	}

	fn append_plain(&mut self, chunk: &str, ctx: &UiContext) {
		let mut source = self.source.to_string();
		source.push_str(chunk);
		let source = Str::new(source);
		self.rendered =
			Ui::from_root(TextLeaf::new().text(source.clone()), self.width.max(1), ctx.clone());
		self.source = source;
	}

	fn resize(&mut self, width: u16, ctx: &UiContext) {
		let width = width.max(1);
		if self.width != width {
			self.width = width;
			self.rendered = Self::render(&self.source, width, ctx);
		}
	}

	const fn height(&self) -> u16 {
		self.rendered.height()
	}
}

struct ToolEntry {
	name:   Str,
	label:  Str,
	ok:     bool,
	view:   ToolView,
	images: Vec<ToolImageEntry>,
}

struct ToolGroup {
	label: Str,
	tools: Vec<ToolEntry>,
}

impl ToolGroup {
	fn new(tools: Vec<ToolEntry>) -> Self {
		let label = read_group_label(tools.len());
		Self { label, tools }
	}

	fn push(&mut self, tool: ToolEntry) {
		self.tools.push(tool);
		self.label = read_group_label(self.tools.len());
	}
}

struct CompactionEntry {
	label: Str,
}

struct LiveAssistant {
	id:   Str,
	text: StrMut,
}
struct LiveTool {
	id:     Str,
	name:   Str,
	title:  Str,
	view:   ToolView,
	images: Vec<ToolImageEntry>,
}

enum Entry {
	User(UserEntry),
	Assistant(RichText),
	Tool(ToolEntry),
	ToolGroup(ToolGroup),
	Compaction(CompactionEntry),
	Notice { text: Str, error: bool },
}

enum PreviewEntry<'a> {
	User(RichText, &'a [Str]),
	Assistant(RichText),
	Other(&'a Entry),
}

impl<'a> PreviewEntry<'a> {
	fn new(entry: &'a Entry, width: u16, ctx: &UiContext) -> Self {
		match entry {
			Entry::User(user) => Self::User(
				RichText::new(user.body.text.as_str(), Chat::message_width(width), ctx),
				&user.chips,
			),
			Entry::Assistant(body) => {
				Self::Assistant(RichText::new(body.text.as_str(), width.max(1), ctx))
			},
			Entry::Tool(_) | Entry::ToolGroup(_) | Entry::Compaction(_) | Entry::Notice { .. } => {
				Self::Other(entry)
			},
		}
	}

	fn height(&self, width: u16) -> u16 {
		match self {
			Self::User(body, chips) => body
				.height()
				.saturating_add(u16::from(!chips.is_empty()))
				.saturating_add(1),
			Self::Assistant(body) => body.height().saturating_add(1),
			Self::Other(entry) => Chat::entry_height(entry, width),
		}
	}

	fn draw(&self, frame: &mut Frame, y: u16, width: u16, ctx: &UiContext) {
		match self {
			Self::User(body, chips) => {
				draw_user_body(frame, y, body, chips, ctx);
			},
			Self::Assistant(body) => {
				draw_rich(frame, y, body, 0, width, ctx.theme);
			},
			Self::Other(entry) => {
				Chat::draw_entry(frame, entry, y, width, ctx);
			},
		}
	}
}

fn activity_waveform_label(waveform: &ActivityWaveform, charset: Charset) -> Str {
	let glyphs = match charset {
		Charset::Ascii => ['.', ':', '-', '*', '#'],
		Charset::Unicode | Charset::NerdFont => ['▁', '▂', '▄', '▆', '█'],
	};
	let mut label = String::with_capacity(5 + waveform.bands().len().saturating_mul(3));
	label.push_str("live ");
	if waveform.bands().is_empty() {
		label.push(glyphs[0]);
	} else {
		for band in waveform.bands() {
			label.push(glyphs[usize::from(*band).min(glyphs.len() - 1)]);
		}
	}
	label.into()
}

struct StatusLabels {
	model:    Str,
	activity: Option<Str>,
	git:      Option<Str>,
	context:  Option<(Str, bool)>,
	queued:   Option<Str>,
	jobs:     Option<Str>,
	attempt:  Option<Str>,
	dropped:  Option<Str>,
}

impl StatusLabels {
	fn new(facts: &StatusFacts, charset: Charset) -> Self {
		let mut model = fmts_mut!("{} {}", charset.icon(Icon::Model), facts.model);
		if let Some(advisor) = &facts.advisor_model {
			let _ = write!(model, " {} {advisor}", charset.icon(Icon::Advisor));
		}
		let activity = facts
			.live_activity
			.as_ref()
			.map(|waveform| activity_waveform_label(waveform, charset));
		let git = facts.git.as_ref().map(|git| {
			let mut label = fmts_mut!("{} {}", charset.icon(Icon::Branch), git.branch);
			if git.dirty > 0 {
				let _ = write!(label, " *{}", git.dirty);
			}
			if git.staged > 0 {
				let _ = write!(label, " +{}", git.staged);
			}
			label.freeze()
		});
		let context = (facts.context_tokens > 0 || facts.context_window.is_some()).then(|| {
			let (usage, overflow) = context_usage_label(facts.context_tokens, facts.context_window);
			let mut label = fmts_mut!("{} {usage}", charset.icon(Icon::Context));
			if !matches!(facts.compaction_speculation, CompactionSpeculationStatus::Idle) {
				let _ = write!(label, " {}", charset.icon(Icon::Auto));
			}
			(label.freeze(), overflow)
		});
		Self {
			model: model.freeze(),
			activity,
			git,
			context,
			queued: (facts.queued > 0).then(|| fmts_mut!("queued {}", facts.queued).freeze()),
			jobs: (facts.jobs > 0).then(|| fmts_mut!("jobs {}", facts.jobs).freeze()),
			attempt: (facts.attempt > 0).then(|| fmts_mut!("retry {}", facts.attempt).freeze()),
			dropped: (facts.dropped > 0).then(|| fmts_mut!("dropped {}", facts.dropped).freeze()),
		}
	}
}

struct WorkState {
	facts:         StatusFacts,
	labels:        StatusLabels,
	elapsed_label: Option<(u64, Str)>,
	active_brand:  StrMut,
	fade:          Tween<Color>,
}

impl WorkState {
	fn update_active_brand(&mut self, now: Duration, charset: Charset) {
		if !self.facts.working {
			return;
		}
		let elapsed = self
			.facts
			.turn_started
			.map_or(Duration::ZERO, |started| Instant::now().saturating_duration_since(started));
		let key = elapsed_label_key(elapsed);
		if self
			.elapsed_label
			.as_ref()
			.is_none_or(|(cached, _)| *cached != key)
		{
			self.elapsed_label = Some((key, elapsed_label(elapsed)));
		}
		self.active_brand.truncate(0);
		self.active_brand.push_str(charset.spinner().at(now));
		self.active_brand.push(' ');
		if let Some((_, label)) = &self.elapsed_label {
			self.active_brand.push_str(label);
		}
	}
}

struct ChatStatus {
	props:      Props,
	slot:       Slot,
	work:       Rc<RefCell<WorkState>>,
	idle_brand: Str,
	charset:    Charset,
	theme:      Theme,
	style:      ComposerStyle,
}

impl ChatStatus {
	fn new(
		work: Rc<RefCell<WorkState>>,
		charset: Charset,
		theme: Theme,
		style: ComposerStyle,
	) -> Self {
		let mut props = Props::new();
		props.set(Prop::Id, STATUS_ID);
		props.set(Prop::NoSelect, true);
		let idle_brand = fmts_mut!("{} omp", charset.icon(Icon::Omp)).freeze();
		Self { props, slot: next_slot(), work, idle_brand, charset, theme, style }
	}

	const fn set_composer_style(&mut self, style: ComposerStyle) {
		self.style = style;
	}

	fn group(&self) -> Status {
		Status::new()
			.with(Prop::Bg, self.theme.panel)
			.with(Prop::Fg, self.theme.fg)
	}

	fn brand_segment(&self, now: Duration) -> Segment {
		let work = self.work.borrow();
		let label = if work.facts.working {
			work.active_brand.clone().freeze()
		} else {
			self.idle_brand.clone()
		};
		Segment::new()
			.label(label)
			.with(Prop::Fg, work.fade.sample(now))
	}

	fn left_group(&self, now: Duration) -> Status {
		let work = self.work.borrow();
		let model = work.labels.model.clone();
		drop(work);
		self
			.group()
			.segment(self.brand_segment(now))
			.segment(Segment::new().label(model).with(Prop::Fg, self.theme.ok))
	}

	fn right_group(&self, context_gauge: ContextGaugeMode, now: Duration) -> Status {
		let work = self.work.borrow();
		let facts = &work.facts;
		let mut status = self.group().with_str(Prop::Align, "right");
		if let Some(activity) = &work.labels.activity {
			status = status.segment(
				Segment::new()
					.label(activity.clone())
					.with(Prop::Fg, self.theme.accent),
			);
		}
		if let Some(git) = &work.labels.git {
			status = status.segment(
				Segment::new()
					.label(git.clone())
					.with(Prop::Fg, self.theme.info),
			);
		}
		if matches!(context_gauge, ContextGaugeMode::Numeric)
			&& (facts.context_tokens > 0 || facts.context_window.is_some())
		{
			let Some((label, overflow)) = &work.labels.context else {
				unreachable!("visible numeric context has a cached label")
			};
			let color = if *overflow {
				self.theme.err
			} else {
				compaction_threshold_color(&self.theme)
			};
			let speculation_color = match facts.compaction_speculation {
				CompactionSpeculationStatus::Idle => None,
				CompactionSpeculationStatus::Running => {
					let phase = (now.as_millis() / SPECULATION_PULSE.as_millis()).is_multiple_of(2);
					Some(if phase {
						self.theme.accent
					} else {
						self.theme.muted
					})
				},
				CompactionSpeculationStatus::Armed => Some(self.theme.accent),
			};
			status = status.segment(
				Segment::new()
					.label(label.clone())
					.with(Prop::Fg, speculation_color.unwrap_or(color)),
			);
		}
		let spend = spend_label(facts.cost_nanos, facts.model_subscription, self.charset);
		if !spend.is_empty() {
			status = status.segment(
				Segment::new()
					.label(spend)
					.with(Prop::Fg, self.theme.secondary),
			);
		}
		let advisor_spend =
			advisor_spend_label(facts.advisor_cost_nanos, facts.advisor_subscription, self.charset);
		if !advisor_spend.is_empty() {
			status = status.segment(
				Segment::new()
					.label(advisor_spend)
					.with(Prop::Fg, self.theme.secondary),
			);
		}
		if let Some(queued) = &work.labels.queued {
			status = status.segment(
				Segment::new()
					.label(queued.clone())
					.with(Prop::Fg, self.theme.warn),
			);
		}
		if let Some(jobs) = &work.labels.jobs {
			status = status.segment(
				Segment::new()
					.label(jobs.clone())
					.with(Prop::Fg, self.theme.info),
			);
		}
		if let Some(attempt) = &work.labels.attempt {
			status = status.segment(
				Segment::new()
					.label(attempt.clone())
					.with(Prop::Fg, self.theme.warn),
			);
		}
		if let Some(dropped) = &work.labels.dropped {
			status = status.segment(
				Segment::new()
					.label(dropped.clone())
					.with(Prop::Fg, self.theme.err),
			);
		}
		status
	}

	fn has_more(&self) -> bool {
		let facts = &self.work.borrow().facts;
		facts.live_activity.is_some()
			|| facts.git.is_some()
			|| facts.context_tokens > 0
			|| facts.cost_nanos > 0
			|| facts.model_subscription
			|| facts.advisor_cost_nanos > 0
			|| facts.advisor_subscription
	}

	fn paint_left(&self, pc: &mut PaintCtx<'_>, rect: Rect) {
		let mut left = self.left_group(pc.now);
		let (_, width) = left.measure(pc.ctx);
		left.paint(pc, Rect::new(rect.x, rect.y, width.min(rect.width), 1));
	}

	fn paint_right(&self, pc: &mut PaintCtx<'_>, rect: Rect, gauge: ContextGaugeMode) {
		let mut right = self.right_group(gauge, pc.now);
		let (_, width) = right.measure(pc.ctx);
		let width = width.min(rect.width);
		let x = rect.x.saturating_add(rect.width.saturating_sub(width));
		right.paint(pc, Rect::new(x, rect.y, width, 1));
	}

	fn paint_full(&self, pc: &mut PaintCtx<'_>, rect: Rect, gauge: ContextGaugeMode) {
		let mut left = self.left_group(pc.now);
		let mut right = self.right_group(gauge, pc.now);
		let (_, left_width) = left.measure(pc.ctx);
		let (_, right_width) = right.measure(pc.ctx);
		if let Some(layout) = boundary_layout(rect.x, rect.width, left_width, right_width, 2) {
			left.paint(pc, Rect::new(layout.left_x, rect.y, left_width, 1));
			if matches!(gauge, ContextGaugeMode::Bar) {
				let facts = &self.work.borrow().facts;
				let total = facts.context_window.unwrap_or_default();
				let used = context_gauge_cells(layout.boundary_width, facts.context_tokens, total);
				let (_, _, _, _, horizontal, _) = pc.ctx.charset.border(Border::Round);
				let mut bytes = [0_u8; 4];
				let glyph = horizontal.encode_utf8(&mut bytes);
				for offset in 0..layout.boundary_width {
					let color = if offset < used {
						compaction_threshold_color(&self.theme)
					} else {
						self.theme.border
					};
					pc.frame.put(
						layout.boundary_x.saturating_add(offset),
						rect.y,
						glyph,
						Style::new().fg(color),
					);
				}
			}
			right.paint(pc, Rect::new(layout.right_x, rect.y, right_width, 1));
		} else {
			let mut combined = self.left_group(pc.now);
			if self.has_more() {
				combined = combined.segment(Segment::new().label("…").with(Prop::Fg, self.theme.muted));
			}
			combined.paint(pc, rect);
		}
	}
}

impl Component for ChatStatus {
	fn props(&self) -> &Props {
		&self.props
	}

	fn props_mut(&mut self) -> &mut Props {
		&mut self.props
	}

	fn slot(&self) -> Slot {
		self.slot
	}

	fn measure(&mut self, ctx: &UiContext) -> (u16, u16) {
		let mut left = self.left_group(Duration::ZERO);
		left.measure(ctx)
	}

	fn height(&mut self, _ctx: &UiContext, _width: u16) -> u16 {
		1
	}

	fn paint(&mut self, pc: &mut PaintCtx<'_>, rect: Rect) {
		if rect.width == 0 || rect.height == 0 {
			return;
		}
		self
			.work
			.borrow_mut()
			.update_active_brand(pc.now, self.charset);
		let layout = self.style.layout(self.charset);
		match layout.status_attachment {
			ComposerStatusAttachment::TopBorder => {
				self.paint_full(
					pc,
					Rect::new(rect.x.saturating_add(1), rect.y, rect.width.saturating_sub(2), 1),
					layout.context_gauge,
				);
			},
			ComposerStatusAttachment::TopRuleChip => {
				self.paint_right(
					pc,
					Rect::new(rect.x, rect.y, rect.width.saturating_sub(1), 1),
					ContextGaugeMode::Numeric,
				);
				self.paint_left(
					pc,
					Rect::new(
						rect.x,
						rect.y.saturating_add(rect.height.saturating_sub(1)),
						rect.width,
						1,
					),
				);
			},
			ComposerStatusAttachment::Standalone => {
				self.paint_full(
					pc,
					Rect::new(
						rect.x,
						rect.y.saturating_add(rect.height.saturating_sub(1)),
						rect.width,
						1,
					),
					ContextGaugeMode::Numeric,
				);
			},
		}
		let work = self.work.borrow();
		let fade_frame = work
			.fade
			.settles_at()
			.min(pc.now.saturating_add(FADE_FRAME));
		let animation_deadline = match (work.facts.working, work.fade.is_settled(pc.now)) {
			(true, true) => Some(pc.ctx.charset.spinner().next_change(pc.now)),
			(true, false) => Some(pc.ctx.charset.spinner().next_change(pc.now).min(fade_frame)),
			(false, false) => Some(fade_frame),
			(false, true) => None,
		};
		let speculation_deadline =
			matches!(work.facts.compaction_speculation, CompactionSpeculationStatus::Running)
				.then(|| pc.now.saturating_add(SPECULATION_PULSE));
		if let Some(at) = match (animation_deadline, speculation_deadline) {
			(Some(animation), Some(speculation)) => Some(animation.min(speculation)),
			(Some(animation), None) => Some(animation),
			(None, speculation) => speculation,
		} {
			pc.wake(self.slot, at);
		}
	}

	fn paints_background(&self) -> bool {
		false
	}
}

/// Immediate-mode designed chat scene driven entirely by host data.
pub struct Chat {
	started_at:         Instant,
	ctx:                UiContext,
	editor_ui:          Ui,
	attachments:        Attachments,
	pending_submit:     VecDeque<(String, Vec<Attachment>, SubmitMode)>,
	copied:             Option<Str>,
	work:               Rc<RefCell<WorkState>>,
	session_title:      Str,
	transcript:         Vec<Entry>,
	drawn_entries:      usize,
	transcript_rows:    u16,
	last_viewport:      Size,
	height_floor:       u16,
	last_editor_height: u16,
	last_panel_height:  u16,
	frame:              Frame,
	live_assistant:     Option<LiveAssistant>,
	live_tools:         Vec<LiveTool>,
	live_revision:      u64,
	drawn_live:         u64,
	last_working:       bool,
	host_right_inset:   u16,
	slot_right_inset:   u16,
	layout_width:       u16,
	slots:              Slots,
	agents:             Vec<AgentRow>,
	agent_labels:       Vec<Str>,
	attribution:        Option<Attribution>,
}

impl Chat {
	/// Creates an empty scene using the host's detected presentation context.
	pub fn new(ctx: &UiContext) -> Self {
		let facts = StatusFacts::default();
		let labels = StatusLabels::new(&facts, ctx.charset);
		let work = Rc::new(RefCell::new(WorkState {
			facts,
			labels,
			elapsed_label: None,
			active_brand: StrMut::new(""),
			fade: Tween::settled(ctx.theme.muted),
		}));
		let style = ComposerStyle::default();
		let pane = EditorPane::new()
			.composer_style(style)
			.with(Prop::Id, INPUT_ID)
			.with(Prop::Submit, true)
			.with(Prop::Placeholder, "Ask anything…")
			.status(ChatStatus::new(Rc::clone(&work), ctx.charset, ctx.theme, style));
		let attachments = pane.attachments();
		let mut editor_ui = Ui::from_root(pane, 0, ctx.clone());
		editor_ui.focus_first();
		Self {
			started_at: Instant::now(),
			ctx: ctx.clone(),
			editor_ui,
			attachments,
			pending_submit: VecDeque::new(),
			copied: None,
			work,
			session_title: Str::default(),
			transcript: Vec::new(),
			drawn_entries: 0,
			transcript_rows: 0,
			last_viewport: Size::new(0, 0),
			height_floor: 0,
			last_editor_height: 0,
			last_panel_height: 0,
			frame: Frame::new(Size::new(0, 0)),
			live_assistant: None,
			live_tools: Vec::new(),
			live_revision: 0,
			drawn_live: 0,
			last_working: false,
			host_right_inset: 0,
			slot_right_inset: 0,
			layout_width: 0,
			agents: Vec::new(),
			agent_labels: Vec::new(),
			slots: Slots::new(ctx.clone()),
			attribution: None,
		}
	}

	/// Switches the built-in composer chrome and its status attachment.
	pub fn set_composer_style(&mut self, style: ComposerStyle) {
		self
			.editor_ui
			.update_component::<EditorPane>(INPUT_ID, |pane| {
				pane.set_composer_style(style);
				true
			});
		self
			.editor_ui
			.update_component::<ChatStatus>(STATUS_ID, |status| {
				status.set_composer_style(style);
				true
			});
		self.refresh_composer();
	}

	/// Borrows retained extension slots for composition or headless inspection.
	pub const fn slots_mut(&mut self) -> &mut Slots {
		&mut self.slots
	}

	/// Applies an extension UI effect synchronously and repaints its retained
	/// slot surface on the next frame.
	pub fn apply_ui_effect(
		&mut self,
		effect: &omp_proto::omp::ui::v1::UiEffect,
	) -> crate::slots::Damage {
		let damage = self.slots.apply(effect);
		if !damage.is_empty() {
			self.bump_live();
		}
		damage
	}

	/// Sets the core-owned provenance septet shown above extension layers.
	pub fn set_attribution(&mut self, septet: [Str; 7]) {
		self.attribution = Some(Attribution::new(septet));
		self.bump_live();
	}

	/// Routes a key through the composer.
	pub fn handle_key(&mut self, key: Key) -> ChatKey {
		if key == Key::Enter && self.composer_empty() && self.is_working() {
			self
				.pending_submit
				.push_back((String::new(), Vec::new(), SubmitMode::Steer));
			return ChatKey::Consumed;
		}
		if key == Key::FollowUp {
			self.stage_submission(SubmitMode::FollowUp);
			return ChatKey::Consumed;
		}
		match self.editor_ui.handle_key(key) {
			UiEvent::Submit => {
				self.stage_submission(SubmitMode::Steer);
				ChatKey::Consumed
			},
			UiEvent::Copied(text) => {
				self.copied = Some(text);
				ChatKey::Consumed
			},
			UiEvent::None if key == Key::Ctrl('c') => ChatKey::Quit,
			UiEvent::None if key == Key::Esc => ChatKey::Ignored,
			UiEvent::None => ChatKey::Consumed,
			_ => ChatKey::Consumed,
		}
	}

	/// Stages the composer's non-empty text as a pending submission and
	/// clears the input; staged attachments ride along unless the text's
	/// slash command preserves them.
	fn stage_submission(&mut self, mode: SubmitMode) {
		let text = self.composer_text();
		if text.trim().is_empty() {
			return;
		}
		let mut attachments = if preserves_attachments(&text) {
			Vec::new()
		} else {
			self.attachments.take()
		};
		let items = if text.trim_start().starts_with('/') {
			vec![crate::queue::QueueItem {
				text:             Str::new(text.as_str()),
				yield_after_turn: false,
			}]
		} else {
			crate::queue::split(&text)
		};
		for (index, item) in items.into_iter().enumerate() {
			let item_mode = if index > 0 || item.yield_after_turn {
				SubmitMode::FollowUp
			} else {
				mode
			};
			let item_attachments = if index == 0 {
				std::mem::take(&mut attachments)
			} else {
				Vec::new()
			};
			self
				.pending_submit
				.push_back((item.text.to_string(), item_attachments, item_mode));
		}
		self.editor_ui.set_text(INPUT_ID, "");
		self.refresh_composer();
	}

	/// Routes sanitized bracketed-paste text through the composer.
	pub fn handle_paste(&mut self, text: &str) {
		let _ = self.editor_ui.handle_paste(text);
		self.refresh_composer();
	}

	/// Routes clipboard text verbatim, bypassing attachment staging.
	pub fn handle_paste_raw(&mut self, text: &str) {
		let _ = self.editor_ui.handle_paste_raw(text);
		self.refresh_composer();
	}

	/// Routes a document-space mouse report into the composer.
	pub fn handle_mouse(&mut self, report: &MouseReport) {
		let rows = self.composer_rows();
		let y = self.frame.size().height.saturating_sub(rows);
		if report.row >= y && report.row < y.saturating_add(rows) {
			let _ = self
				.editor_ui
				.handle_mouse(report.col, report.row - y, report.kind);
		}
	}

	/// Takes text copied or cut by the composer.
	pub const fn take_copied(&mut self) -> Option<Str> {
		self.copied.take()
	}

	/// Takes the next composer submission: its text, staged attachments,
	/// and active-turn delivery mode.
	pub fn take_submission(&mut self) -> Option<(String, Vec<Attachment>, SubmitMode)> {
		self.pending_submit.pop_front()
	}

	/// Returns whether the composer contains no non-whitespace text.
	pub fn composer_empty(&self) -> bool {
		self.composer_text().trim().is_empty()
	}

	/// Replaces composer text, preserving staged attachments.
	pub fn set_composer_text(&mut self, text: &str) {
		self.editor_ui.set_text(INPUT_ID, text);
		self.refresh_composer();
	}

	/// Returns the composer block height used for pointer hit testing.
	pub const fn composer_rows(&self) -> u16 {
		self.editor_ui.height()
	}

	/// Returns whether the latest status snapshot says a turn is active.
	pub fn is_working(&self) -> bool {
		self.work.borrow().facts.working
	}

	/// Returns a copy of the latest status snapshot.
	pub fn status(&self) -> StatusFacts {
		self.work.borrow().facts.clone()
	}

	/// Replaces slash-command completion data.
	pub fn set_slash_commands(&mut self, commands: Vec<Command>) {
		self
			.editor_ui
			.update_component::<EditorPane>(INPUT_ID, |pane| {
				pane.set_completion(Box::new(SlashCommands::new(commands)));
				true
			});
	}

	/// Reserves right-edge columns for host-composited chrome.
	pub const fn set_right_inset(&mut self, cols: u16) {
		self.host_right_inset = cols;
	}

	/// Appends a committed user message.
	pub fn push_user(&mut self, text: impl Into<String>, chips: Vec<Str>) {
		self.transcript.push(Entry::User(UserEntry {
			body: RichText::new(text, Self::message_width(self.layout_width), &self.ctx),
			chips,
		}));
	}

	/// Begins a live assistant message.
	pub fn begin_assistant(&mut self, id: impl Into<Str>) {
		self.live_assistant = Some(LiveAssistant { id: id.into(), text: StrMut::new("") });
		self.bump_live();
	}

	/// Appends a delta to a matching live assistant message.
	pub fn append_assistant(&mut self, id: &str, text: &str) {
		if let Some(message) = &mut self.live_assistant
			&& message.id.as_str() == id
		{
			message.text.push_str(text);
			self.bump_live();
		}
	}

	/// Commits a matching live assistant message into stable transcript rows.
	pub fn end_assistant(&mut self, id: &str) {
		if self
			.live_assistant
			.as_ref()
			.is_some_and(|message| message.id.as_str() == id)
		{
			let message = self
				.live_assistant
				.take()
				.expect("matching live assistant exists");
			self.transcript.push(Entry::Assistant(RichText::new(
				message.text.as_str(),
				Self::message_width(self.layout_width),
				&self.ctx,
			)));
			self.bump_live();
		}
	}

	/// Begins a live tool card.
	pub fn tool_started(&mut self, id: impl Into<Str>, name: impl Into<Str>, title: impl Into<Str>) {
		let title = hud_line(title.into(), self.ctx.charset);
		self.live_tools.push(LiveTool {
			id: id.into(),
			name: name.into(),
			title,
			view: ToolView::structured(
				Default::default(),
				Self::tool_view_width(self.layout_width),
				&self.ctx,
			),
			images: Vec::new(),
		});
		self.bump_live();
	}

	/// Appends unstructured output to a matching live tool card.
	pub fn tool_output(&mut self, id: &str, chunk: &str) {
		let ctx = self.ctx.clone();
		if let Some(tool) = self
			.live_tools
			.iter_mut()
			.find(|tool| tool.id.as_str() == id)
		{
			tool.view.append_plain(chunk, &ctx);
			self.bump_live();
		}
	}

	/// Replaces a matching live tool card's renderer-produced view.
	pub fn tool_view(&mut self, id: &str, view: Str) {
		let ctx = self.ctx.clone();
		if let Some(tool) = self
			.live_tools
			.iter_mut()
			.find(|tool| tool.id.as_str() == id)
		{
			tool.view.replace(view, &ctx);
			self.bump_live();
		}
	}

	/// Attaches a persisted PNG to a matching live tool card; the committed
	/// card renders it inline. Sources whose headers fail to probe are
	/// ignored, keeping the text fallback.
	pub fn tool_image(&mut self, id: &str, source: impl Into<Str>) {
		let source = source.into();
		let Some(px) = std::fs::read(source.as_str())
			.ok()
			.and_then(|bytes| omp_tui::imagefmt::dimensions(&bytes))
		else {
			return;
		};
		if let Some(tool) = self
			.live_tools
			.iter_mut()
			.find(|tool| tool.id.as_str() == id)
		{
			tool.images.push(ToolImageEntry { source, px });
			self.bump_live();
		}
	}

	/// Commits a matching live tool card with its terminal branch and view.
	pub fn tool_finished(&mut self, id: &str, ok: bool, view: Str) {
		if let Some(index) = self
			.live_tools
			.iter()
			.position(|tool| tool.id.as_str() == id)
		{
			let mut tool = self.live_tools.remove(index);
			tool.view.replace(view, &self.ctx);
			let icon = if ok {
				self.ctx.charset.check()
			} else {
				self.ctx.charset.icon(Icon::Error)
			};
			let label = fmts_mut!("{icon} {}", tool.title).freeze();
			let entry = ToolEntry { name: tool.name, label, ok, view: tool.view, images: tool.images };
			if entry.name.as_str() == "read" {
				let prior_is_read = matches!(self.transcript.last(), Some(Entry::Tool(prior)) if prior.name.as_str() == "read");
				if let Some(Entry::ToolGroup(group)) = self.transcript.last_mut() {
					group.push(entry);
				} else if prior_is_read {
					let Some(Entry::Tool(prior)) = self.transcript.pop() else {
						unreachable!("matched a read tool entry")
					};
					self
						.transcript
						.push(Entry::ToolGroup(ToolGroup::new(vec![prior, entry])));
				} else {
					self.transcript.push(Entry::Tool(entry));
				}
			} else {
				self.transcript.push(Entry::Tool(entry));
			}
			self.bump_live();
		}
	}

	/// Appends an in-place compaction divider with method, token delta, and
	/// optional preview title.
	pub fn push_compaction(
		&mut self,
		summary: Str,
		title: Option<Str>,
		method: Option<Str>,
		tokens_before: u64,
		tokens_after: Option<u64>,
	) {
		let preview = title
			.as_deref()
			.filter(|title| !title.trim().is_empty())
			.or_else(|| summary.lines().find(|line| !line.trim().is_empty()));
		let method = compaction_method_label(method.as_deref());
		let mut label = fmts_mut!("{} {method}", self.ctx.charset.icon(Icon::Camera));
		if let Some(tokens_after) = tokens_after.filter(|_| tokens_before > 0) {
			let arrow = if self.ctx.charset == Charset::Ascii {
				"->"
			} else {
				"→"
			};
			let _ = write!(
				label,
				" · {}{arrow}{}",
				compact_count(tokens_before),
				compact_count(tokens_after),
			);
		}
		if let Some(preview) = preview {
			let _ = write!(label, " · {preview}");
		}
		self
			.transcript
			.push(Entry::Compaction(CompactionEntry { label: label.freeze() }));
	}

	/// Appends an informational transcript notice.
	pub fn push_notice(&mut self, text: impl IntoStr) {
		self
			.transcript
			.push(Entry::Notice { text: text.into_str(), error: false });
	}

	/// Appends an error transcript notice.
	pub fn push_error(&mut self, text: impl IntoStr) {
		self
			.transcript
			.push(Entry::Notice { text: text.into_str(), error: true });
	}

	/// Appends a semantic transcript boundary with core-owned styling.
	pub fn push_transcript_frame(&mut self, frame: TranscriptFrame) {
		let marker = match frame.kind {
			TranscriptFrameKind::Compaction => "compact",
			TranscriptFrameKind::Branch => "branch",
			TranscriptFrameKind::Handoff => "handoff",
			TranscriptFrameKind::CacheBreak => "cache break",
			TranscriptFrameKind::Recovery => "recovery",
			TranscriptFrameKind::Error => "error",
		};
		let text = match frame.detail {
			Some(detail) if !detail.is_empty() => sf!("{marker} · {} — {detail}", frame.title),
			_ => sf!("{marker} · {}", frame.title),
		};
		if frame.kind == TranscriptFrameKind::Error {
			self.push_error(text);
		} else {
			self.push_notice(text);
		}
	}

	/// Replaces the anchored `AgentTree` HUD projection.
	pub fn set_agent_roster(&mut self, rows: Vec<AgentRow>) {
		self.agent_labels = rows
			.iter()
			.map(|agent| agent_label(agent, self.ctx.charset))
			.collect();
		self.agents = rows;
		self.bump_live();
	}

	/// Borrows the current `AgentTree` roster projection.
	pub fn agent_roster(&self) -> &[AgentRow] {
		&self.agents
	}

	/// Replaces the complete status snapshot.
	pub fn set_status(&mut self, facts: StatusFacts) {
		let now = self.started_at.elapsed();
		let labels = StatusLabels::new(&facts, self.ctx.charset);
		let mut work = self.work.borrow_mut();
		if work.facts.working != facts.working {
			work.fade.retarget(
				now,
				if facts.working {
					self.ctx.theme.ok
				} else {
					self.ctx.theme.muted
				},
				BRAND_FADE,
				Easing::EaseInOut,
			);
		}
		work.facts = facts;
		work.labels = labels;
		work.elapsed_label = None;
		work.update_active_brand(now, self.ctx.charset);
		drop(work);
		self
			.editor_ui
			.update_component::<EditorPane>(INPUT_ID, |_| true);
		self.bump_live();
	}

	/// Replaces the session title shown in the air row.
	pub fn set_session_title(&mut self, title: impl Into<Str>) {
		self.session_title = hud_line(title.into(), self.ctx.charset);
		self.bump_live();
	}

	/// Restores a prompt that the backend dropped before committing its first
	/// turn, without overwriting a draft started while cancellation settled.
	pub fn restore_dropped_prompt(&mut self, text: Str, attachments: Vec<Attachment>) {
		if let Some(index) = self.transcript.iter().rposition(
			|entry| matches!(entry, Entry::User(user) if user.body.text.as_str() == text.as_str()),
		) {
			self.transcript.remove(index);
			self.drawn_entries = 0;
			self.transcript_rows = 0;
			self.height_floor = 0;
			self.last_viewport = Size::new(0, 0);
			self.bump_live();
		}
		if !self.composer_empty() || !self.attachments.is_empty() {
			return;
		}
		for attachment in attachments {
			match attachment.content {
				AttachmentContent::Image { source, .. } => {
					self.attachments.push_image(source);
				},
				AttachmentContent::Text { text, .. } => {
					self.attachments.push_text(text.as_str());
				},
			}
		}
		self.set_composer_text(text.as_str());
	}

	/// Removes committed and live transcript content.
	pub fn clear_history(&mut self) {
		self.transcript.clear();
		self.live_assistant = None;
		self.live_tools.clear();
		self.drawn_entries = 0;
		self.transcript_rows = 0;
		self.height_floor = 0;
		self.last_viewport = Size::new(0, 0);
		self.bump_live();
	}

	/// Applies scene-owned backend mutations and returns events owned by host
	/// overlays.
	#[must_use]
	pub fn apply_backend_event(&mut self, event: BackendEvent) -> Option<BackendEvent> {
		match event {
			BackendEvent::UserReplayed { text, chips } => self.push_user(text.as_str(), chips),
			BackendEvent::PromptDropped { text, attachments } => {
				self.restore_dropped_prompt(text, attachments);
			},
			BackendEvent::AssistantBegin { id } => self.begin_assistant(id),
			BackendEvent::AssistantDelta { id, text } => {
				self.append_assistant(id.as_str(), text.as_str());
			},
			BackendEvent::AssistantEnd { id } => self.end_assistant(id.as_str()),
			BackendEvent::ToolStarted { id, name, title } => self.tool_started(id, name, title),
			BackendEvent::ToolOutput { id, chunk } => self.tool_output(id.as_str(), chunk.as_str()),
			BackendEvent::ToolView { id, view } => self.tool_view(id.as_str(), view),
			BackendEvent::ToolImage { id, source } => self.tool_image(id.as_str(), source),
			BackendEvent::ToolFinished { id, ok, view } => self.tool_finished(id.as_str(), ok, view),
			BackendEvent::Compacted { summary, title, method, tokens_before, tokens_after } => {
				self.push_compaction(summary, title, method, tokens_before, tokens_after);
			},
			BackendEvent::TranscriptFrame(frame) => self.push_transcript_frame(frame),
			BackendEvent::AgentRoster(rows) => self.set_agent_roster(rows),
			BackendEvent::Notice(text) => self.push_notice(text),
			BackendEvent::Error(text) => self.push_error(text),
			BackendEvent::Status(facts) => self.set_status(facts),
			BackendEvent::SessionTitle(title) => self.set_session_title(title),
			BackendEvent::HistoryCleared => self.clear_history(),
			BackendEvent::Ack { interrupted } => {
				if interrupted {
					self.push_notice("Interrupted.");
				}
			},
			event @ (BackendEvent::OpenModelPicker { .. }
			| BackendEvent::ModelsUpdated { .. }
			| BackendEvent::Sessions(_)
			| BackendEvent::LoginProviders(_)
			| BackendEvent::RewindTargets(_)
			| BackendEvent::AuthPrompt { .. }
			| BackendEvent::AuthPromptClose
			| BackendEvent::OpenSettings
			| BackendEvent::OpenAgentTree
			| BackendEvent::Pause
			| BackendEvent::NewSessionRequested) => return Some(event),
		}
		None
	}

	/// Updates the retained logical document and reports exact changed rows.
	pub fn render(&mut self, viewport: Size) -> RenderedFrame<'_> {
		self.render_at(viewport, self.started_at.elapsed())
	}

	/// Returns the delay until the composer's next requested animation frame.
	///
	/// A settled idle chat returns `None`, allowing custom hosts to block on
	/// input and backend events without polling.
	pub fn next_wake(&self) -> Option<Duration> {
		if self.layout_width != self.content_width(self.last_viewport) {
			return Some(Duration::ZERO);
		}
		let elapsed = self.started_at.elapsed();
		self
			.editor_ui
			.next_wake()
			.map(|deadline| deadline.saturating_sub(elapsed))
	}

	/// Produces one throwaway viewport during an active resize gesture.
	pub fn render_resize_preview(&mut self, viewport: Size) -> Frame {
		let elapsed = self.started_at.elapsed();
		let mut frame = Frame::new(viewport);
		if viewport.width == 0 || viewport.height == 0 {
			return frame;
		}
		frame.fill(Rect::new(0, 0, viewport.width, viewport.height), base_style(self.ctx.theme));
		let content_width = self.content_width(viewport);
		let composer_width = content_width;
		if self.editor_ui.frame().size().width != composer_width {
			self.editor_ui.resize(composer_width);
		}
		self.editor_ui.tick(elapsed);
		let editor_height = self.composer_rows();
		let panel_height = self.live_panel_height(content_width);
		let editor_y = viewport.height.saturating_sub(editor_height);
		let title_y = editor_y.saturating_sub(1);
		let working_y = title_y.saturating_sub(1);
		let panel_y = working_y
			.saturating_sub(u16::from(panel_height > 0))
			.saturating_sub(panel_height);
		self.draw_live_panel(&mut frame, Rect::new(0, panel_y, content_width, panel_height), elapsed);
		if self.is_working() {
			self.draw_working(&mut frame, working_y, elapsed);
		}
		self.draw_session_title(&mut frame, title_y);
		frame.blit(self.editor_ui.frame(), 0, editor_height, 0, editor_y);
		let mut remaining = panel_y;
		for entry in self.transcript.iter().rev() {
			if remaining == 0 {
				break;
			}
			let preview = PreviewEntry::new(entry, content_width, &self.ctx);
			let height = preview.height(content_width);
			if height <= remaining {
				remaining -= height;
				preview.draw(&mut frame, remaining, content_width, &self.ctx);
			} else {
				let mut scratch = Frame::new(Size::new(content_width, height));
				preview.draw(&mut scratch, 0, content_width, &self.ctx);
				frame.blit(&scratch, height - remaining, remaining, 0, 0);
				remaining = 0;
			}
		}
		frame
	}

	fn composer_text(&self) -> String {
		self.editor_ui.values()[INPUT_ID]
			.as_str()
			.unwrap_or_default()
			.to_owned()
	}

	fn refresh_composer(&mut self) {
		let width = self.editor_ui.frame().size().width;
		if width > 0 {
			self.editor_ui.resize(width);
		}
	}

	const fn bump_live(&mut self) {
		self.live_revision = self.live_revision.wrapping_add(1);
	}

	const fn right_inset(&self) -> u16 {
		self.host_right_inset.saturating_add(self.slot_right_inset)
	}

	fn content_width(&self, viewport: Size) -> u16 {
		viewport.width.saturating_sub(self.right_inset()).max(1)
	}

	fn render_at(&mut self, viewport: Size, elapsed: Duration) -> RenderedFrame<'_> {
		if viewport.width == 0 || viewport.height == 0 {
			self.last_viewport = viewport;
			self.height_floor = 0;
			self.last_editor_height = 0;
			self.last_panel_height = 0;
			self.drawn_entries = 0;
			self.transcript_rows = 0;
			self.frame = Frame::new(viewport);
			return RenderedFrame {
				frame:       &self.frame,
				stable_rows: 0,
				damage:      SmallVec::new(),
			};
		}
		let content_width = self.content_width(viewport);
		if self.editor_ui.frame().size().width != content_width {
			self.editor_ui.resize(content_width);
		}
		self.editor_ui.tick(elapsed);
		let editor_changed = self.editor_ui.take_frame_damage();
		let viewport_rebuild = self.last_viewport != viewport;
		let content_reflow = self.layout_width != content_width;
		if viewport_rebuild {
			self.last_viewport = viewport;
			self.layout_width = content_width;
			self.height_floor = 0;
			self.drawn_entries = 0;
			self.transcript_rows = 0;
			for entry in &mut self.transcript {
				Self::resize_entry(entry, content_width, &self.ctx);
			}
		} else if content_reflow {
			self.layout_width = content_width;
			// Drawn entries are already part of the declared-stable prefix. Keeping
			// them at their original width also preserves an entry whose rows cross
			// the renderer's committed boundary; only wholly-undrawn entries reflow.
			for entry in &mut self.transcript[self.drawn_entries..] {
				Self::resize_entry(entry, content_width, &self.ctx);
			}
		}
		if viewport_rebuild || content_reflow {
			let view_width = Self::tool_view_width(content_width);
			for tool in &mut self.live_tools {
				tool.view.resize(view_width, &self.ctx);
			}
		}
		let new_rows = self.transcript[self.drawn_entries..]
			.iter()
			.fold(0_u16, |rows, entry| rows.saturating_add(Self::entry_height(entry, content_width)));
		let transcript_rows = self.transcript_rows.saturating_add(new_rows);
		let editor_height = self.composer_rows();
		let panel_height = self.live_panel_height(content_width);
		let natural_height =
			transcript_rows.saturating_add(Self::band_height(editor_height, panel_height));
		self.height_floor = self.height_floor.max(natural_height);
		let document_height = self.height_floor.max(viewport.height);
		let transcript_damage_start = if viewport_rebuild {
			0
		} else {
			self.transcript_rows
		};
		let editor_y = document_height.saturating_sub(editor_height);
		let title_y = editor_y.saturating_sub(1);
		let working_y = title_y.saturating_sub(1);
		let panel_y = working_y
			.saturating_sub(u16::from(panel_height > 0))
			.saturating_sub(panel_height);
		let panel = Rect::new(0, panel_y, content_width, panel_height);
		let band_reflow = !viewport_rebuild
			&& ((self.last_editor_height != 0 && editor_height != self.last_editor_height)
				|| panel_height != self.last_panel_height);
		let repaint_suffix = viewport_rebuild || content_reflow || new_rows > 0 || band_reflow;
		if viewport_rebuild {
			self.frame = Frame::new(Size::new(viewport.width, document_height));
		} else {
			self
				.frame
				.resize_height(document_height, base_style(self.ctx.theme));
		}
		if repaint_suffix {
			self.frame.fill(
				Rect::new(
					0,
					transcript_damage_start,
					viewport.width,
					document_height.saturating_sub(transcript_damage_start),
				),
				base_style(self.ctx.theme),
			);
		}
		let mut y = self.transcript_rows;
		for index in self.drawn_entries..self.transcript.len() {
			let used =
				Self::draw_entry(&mut self.frame, &self.transcript[index], y, content_width, &self.ctx);
			y = y.saturating_add(used);
		}
		self.drawn_entries = self.transcript.len();
		self.transcript_rows = y;
		let spinner_active = !self.live_tools.is_empty();
		let live_changed = repaint_suffix || self.drawn_live != self.live_revision || spinner_active;
		if live_changed {
			self.draw_live_panel_owned(panel, elapsed);
		}
		let working = self.is_working();
		let working_changed = working != self.last_working;
		if !repaint_suffix && self.last_working && !working {
			self
				.frame
				.fill(Rect::new(0, working_y, viewport.width, 1), base_style(self.ctx.theme));
		}
		if working {
			self.draw_working_owned(working_y, elapsed);
		}
		if repaint_suffix || live_changed {
			self.draw_session_title_owned(title_y);
		}
		let hud = Rect::new(0, working_y, content_width, 2);
		if !self.frame.noselect().contains(&hud) {
			self.frame.push_noselect(hud);
		}
		if repaint_suffix || editor_changed {
			self
				.frame
				.blit(self.editor_ui.frame(), 0, editor_height, 0, editor_y);
		}
		let frame_size = self.frame.size();
		let rails = if let Some(attribution) = self.attribution.as_ref() {
			Bands::compose_with_attribution(
				&mut self.frame,
				&mut self.slots,
				frame_size,
				attribution,
				self.ctx.theme,
			)
		} else {
			Bands::compose(&mut self.frame, &mut self.slots, frame_size)
		};
		self.slot_right_inset = rails.right;
		let mut damage = SmallVec::new();
		if repaint_suffix {
			damage.push((transcript_damage_start, document_height));
		} else {
			if live_changed {
				damage.push((panel_y, panel_y.saturating_add(panel_height)));
			}
			if working || working_changed {
				damage.push((working_y, working_y.saturating_add(1)));
			}
			if editor_changed {
				damage.push((editor_y, document_height));
			}
		}
		self.drawn_live = self.live_revision;
		self.last_working = working;
		self.last_editor_height = editor_height;
		self.last_panel_height = panel_height;
		RenderedFrame { frame: &self.frame, stable_rows: self.transcript_rows, damage }
	}

	const fn band_height(editor_height: u16, panel_height: u16) -> u16 {
		let panel_gap = if panel_height > 0 { 1 } else { 0 };
		editor_height
			.saturating_add(2)
			.saturating_add(panel_gap)
			.saturating_add(panel_height)
	}

	fn live_panel_height(&self, width: u16) -> u16 {
		let inner_width = width.saturating_sub(2).max(1);
		let agent_rows = self.agent_labels.len().min(8) as u16;
		let assistant_rows = self
			.live_assistant
			.as_ref()
			.map_or(0, |assistant| flowed_height(assistant.text.as_str(), inner_width));
		let tool_rows = self
			.live_tools
			.iter()
			.fold(0_u16, |rows, tool| rows.saturating_add(1).saturating_add(tool.view.height()));
		let content_rows = agent_rows
			.saturating_add(assistant_rows)
			.saturating_add(tool_rows)
			.min(MAX_LIVE_PANEL_CONTENT_ROWS);
		if content_rows == 0 {
			0
		} else {
			content_rows.saturating_add(2)
		}
	}

	const fn message_width(width: u16) -> u16 {
		let narrowed = width.saturating_sub(3);
		if narrowed == 0 { 1 } else { narrowed }
	}

	const fn tool_view_width(width: u16) -> u16 {
		let inset = if width >= 50 { 2 } else { 0 };
		let narrowed = width.saturating_sub(inset).saturating_sub(4);
		if narrowed == 0 { 1 } else { narrowed }
	}

	fn resize_entry(entry: &mut Entry, width: u16, ctx: &UiContext) {
		let message_width = Self::message_width(width);
		match entry {
			Entry::User(user) => user.body.resize(message_width, ctx),
			Entry::Assistant(body) => body.resize(width.max(1), ctx),
			Entry::Tool(tool) => tool.view.resize(Self::tool_view_width(width), ctx),
			Entry::ToolGroup(group) => {
				for tool in &mut group.tools {
					tool.view.resize(Self::tool_view_width(width), ctx);
				}
			},
			Entry::Compaction(_) | Entry::Notice { .. } => {},
		}
	}

	fn entry_height(entry: &Entry, width: u16) -> u16 {
		match entry {
			Entry::User(user) => user
				.body
				.height()
				.saturating_add(u16::from(!user.chips.is_empty()))
				.saturating_add(1),
			Entry::Assistant(body) => body.height().saturating_add(1),
			Entry::Tool(tool) => tool_height(tool, width).saturating_add(1),
			Entry::ToolGroup(group) => group.tools.iter().fold(1_u16, |height, tool| {
				height
					.saturating_add(tool_height(tool, width))
					.saturating_add(1)
			}),
			Entry::Compaction(compaction) => {
				flowed_height(&compaction.label, width.saturating_sub(2)).saturating_add(1)
			},
			Entry::Notice { text, .. } => {
				flowed_height(text, width.saturating_sub(2)).saturating_add(1)
			},
		}
	}

	fn draw_entry(frame: &mut Frame, entry: &Entry, y: u16, width: u16, ctx: &UiContext) -> u16 {
		match entry {
			Entry::User(user) => draw_user(frame, y, user, ctx),
			Entry::Assistant(body) => draw_rich(frame, y, body, 0, width, ctx.theme).saturating_add(1),
			Entry::Tool(tool) => draw_tool(frame, y, width, tool, ctx).saturating_add(1),
			Entry::ToolGroup(group) => {
				draw_line(frame, 1, y, width.saturating_sub(2), &[Span::new(
					&group.label,
					ink(ctx.theme.muted).bold(),
				)]);
				group.tools.iter().fold(1_u16, |rows, tool| {
					rows
						.saturating_add(draw_tool(frame, y.saturating_add(rows), width, tool, ctx))
						.saturating_add(1)
				})
			},
			Entry::Compaction(compaction) => draw_flowed(
				frame,
				Rect::new(1, y, width.saturating_sub(2), frame.size().height.saturating_sub(y)),
				&[Span::new(&compaction.label, ink(ctx.theme.info).bold())],
			)
			.saturating_add(1),
			Entry::Notice { text, error } => {
				let style = if *error {
					ink(ctx.theme.err)
				} else {
					ink(ctx.theme.muted).italic()
				};
				draw_flowed(
					frame,
					Rect::new(1, y, width.saturating_sub(2), frame.size().height.saturating_sub(y)),
					&[Span::new(text, style)],
				)
				.saturating_add(1)
			},
		}
	}

	fn draw_live_panel_owned(&mut self, rect: Rect, elapsed: Duration) {
		let ctx = self.ctx.clone();
		draw_live_panel_impl(
			&mut self.frame,
			rect,
			self.live_assistant.as_ref(),
			&self.live_tools,
			&self.agent_labels,
			&ctx,
			elapsed,
		);
	}

	fn draw_live_panel(&self, frame: &mut Frame, rect: Rect, elapsed: Duration) {
		draw_live_panel_impl(
			frame,
			rect,
			self.live_assistant.as_ref(),
			&self.live_tools,
			&self.agent_labels,
			&self.ctx,
			elapsed,
		);
	}

	fn draw_working_owned(&mut self, y: u16, elapsed: Duration) {
		draw_working_impl(
			&mut self.frame,
			y,
			elapsed,
			self.ctx.charset.icon(Icon::Cancellable),
			self.ctx.native_decor,
			self.ctx.theme,
		);
	}

	fn draw_working(&self, frame: &mut Frame, y: u16, elapsed: Duration) {
		draw_working_impl(
			frame,
			y,
			elapsed,
			self.ctx.charset.icon(Icon::Cancellable),
			self.ctx.native_decor,
			self.ctx.theme,
		);
	}

	fn draw_session_title_owned(&mut self, y: u16) {
		let right_inset = self.right_inset();
		draw_session_title_impl(&mut self.frame, y, right_inset, &self.session_title, self.ctx.theme);
	}

	fn draw_session_title(&self, frame: &mut Frame, y: u16) {
		draw_session_title_impl(frame, y, self.right_inset(), &self.session_title, self.ctx.theme);
	}
}

fn draw_live_panel_impl(
	frame: &mut Frame,
	rect: Rect,
	assistant: Option<&LiveAssistant>,
	tools: &[LiveTool],
	agent_labels: &[Str],
	ctx: &UiContext,
	elapsed: Duration,
) {
	frame.fill(rect, base_style(ctx.theme));
	if assistant.is_none() && tools.is_empty() && agent_labels.is_empty() {
		return;
	}
	draw_box(
		frame,
		rect,
		ink(ctx.theme.border),
		panel_style(ctx.theme),
		ctx.charset,
		ctx.native_decor,
	);
	let mut y = rect.y.saturating_add(1);
	let bottom = rect.y.saturating_add(rect.height).saturating_sub(1);
	for label in agent_labels.iter().take(8) {
		if y >= bottom {
			break;
		}
		draw_line(frame, rect.x.saturating_add(1), y, rect.width.saturating_sub(2), &[Span::new(
			label,
			ink(ctx.theme.secondary),
		)]);
		y = y.saturating_add(1);
	}
	if let Some(message) = assistant {
		let used = draw_flowed(
			frame,
			Rect::new(
				rect.x.saturating_add(1),
				y,
				rect.width.saturating_sub(2),
				bottom.saturating_sub(y),
			),
			&[Span::new(message.text.as_str(), prose_style(ctx.theme))],
		);
		y = y.saturating_add(used).min(bottom);
	}
	for tool in tools {
		if y >= bottom {
			break;
		}
		let style = ink(ctx.theme.info);
		draw_line(frame, rect.x.saturating_add(1), y, rect.width.saturating_sub(2), &[
			Span::new(ctx.charset.spinner().at(elapsed), style),
			Span::new(" ", style),
			Span::new(&tool.title, style),
		]);
		y = y.saturating_add(1);
		let available = bottom.saturating_sub(y);
		let height = tool.view.height().min(available);
		if height > 0 {
			frame.blit(tool.view.rendered.frame(), 0, height, rect.x.saturating_add(2), y);
			y = y.saturating_add(height);
		}
	}
}

fn draw_working_impl(
	frame: &mut Frame,
	y: u16,
	elapsed: Duration,
	hint: &str,
	native: bool,
	theme: Theme,
) {
	if y >= frame.size().height || frame.size().width < 4 {
		return;
	}
	let label = "Working";
	let start = u16::from(frame.size().width >= 50);
	let mut column = start;
	let length = visible_width(hint)
		.saturating_add(visible_width(label))
		.saturating_add(1);
	let shimmer = Shimmer::new(elapsed, SHIMMER_PERIOD, length);
	let right = frame.size().width.saturating_sub(1);
	for (text, high) in [(hint, theme.info), (" ", theme.ok), (label, theme.ok)] {
		for grapheme in xutf::graphemes_str(text) {
			if column >= right {
				break;
			}
			let style = if native {
				ink(high)
			} else {
				shimmer.pick(column - start, ink(theme.border), ink(theme.muted), ink(high))
			};
			let next = frame.put(column, y, grapheme, style);
			if next == column {
				break;
			}
			column = next;
		}
	}
	if native {
		frame.push_decor(Decor {
			rect: Rect::new(start, y, column.saturating_sub(start), 1),
			kind: DecorKind::Shimmer { period: SHIMMER_PERIOD },
		});
	}
}

fn draw_session_title_impl(frame: &mut Frame, y: u16, right_inset: u16, title: &str, theme: Theme) {
	if title.is_empty() || y >= frame.size().height {
		return;
	}
	let width = frame.size().width.saturating_sub(right_inset);
	let title = truncate_to_width(title, width.saturating_sub(2));
	if title.width == 0 {
		return;
	}
	let x = width.saturating_sub(title.width.saturating_add(1));
	let style = ink(theme.border).italic();
	draw_line(frame, x, y, title.width, &[
		Span::new(title.text, style),
		Span::new(if title.ellipsis { "…" } else { "" }, style),
	]);
}

fn draw_user(frame: &mut Frame, y: u16, user: &UserEntry, ctx: &UiContext) -> u16 {
	draw_user_body(frame, y, &user.body, &user.chips, ctx)
}

fn draw_user_body(
	frame: &mut Frame,
	y: u16,
	body: &RichText,
	chips: &[Str],
	ctx: &UiContext,
) -> u16 {
	let mut at = y;
	if !chips.is_empty() {
		let mut x = frame.put(1, at, ctx.charset.icon(Icon::Image), ink(ctx.theme.warn));
		for chip in chips {
			x = frame.put(x, at, " ", ink(ctx.theme.muted));
			x = frame.put(x, at, chip, ink(ctx.theme.warn).bold());
		}
		at = at.saturating_add(1);
	}
	let x = frame.put(0, at, ctx.charset.cursor(), ink(ctx.theme.ok));
	frame.put(x, at, " ", ink(ctx.theme.ok));
	let used = draw_rich(frame, at, body, 3, body.width, ctx.theme);
	at.saturating_sub(y).saturating_add(used).saturating_add(1)
}

fn draw_rich(frame: &mut Frame, y: u16, body: &RichText, x: u16, width: u16, theme: Theme) -> u16 {
	if let Some(view) = &body.view {
		let height = view.height();
		frame.blit(view.frame(), 0, height, x, y);
		height
	} else {
		draw_flowed(frame, Rect::new(x, y, width, frame.size().height.saturating_sub(y)), &[
			Span::new(&body.text, prose_style(theme)),
		])
	}
}

fn draw_tool(frame: &mut Frame, y: u16, width: u16, tool: &ToolEntry, ctx: &UiContext) -> u16 {
	let margin = u16::from(width >= 50);
	let height = tool_height(tool, width);
	let rect = Rect::new(margin, y, width.saturating_sub(margin * 2), height);
	let state = if tool.ok { ctx.theme.ok } else { ctx.theme.err };
	draw_box(frame, rect, ink(state), panel_style(ctx.theme), ctx.charset, ctx.native_decor);
	draw_line(frame, rect.x.saturating_add(1), y, rect.width.saturating_sub(2), &[Span::new(
		&tool.label,
		ink(state).bold(),
	)]);
	let mut row = y.saturating_add(1);
	let bottom = y.saturating_add(height).saturating_sub(1);
	let view_height = tool.view.height().min(bottom.saturating_sub(row));
	if view_height > 0 {
		frame.blit(tool.view.rendered.frame(), 0, view_height, rect.x.saturating_add(2), row);
		row = row.saturating_add(view_height);
	}
	for image in &tool.images {
		let (cols, rows) = tool_image_box(image, width);
		if rows == 0 || row.saturating_add(rows) > bottom {
			break;
		}
		omp_tui::components::draw_image_inline(
			frame,
			ctx,
			rect.x.saturating_add(2),
			row,
			image.source.as_str(),
			cols,
			rows,
		);
		row = row.saturating_add(rows);
	}
	height
}

/// Aspect-fit cell box for one tool image inside a card of `width` columns.
fn tool_image_box(image: &ToolImageEntry, width: u16) -> (u16, u16) {
	let margin = u16::from(width >= 50);
	let interior = width
		.saturating_sub(margin * 2)
		.saturating_sub(4)
		.min(TOOL_IMAGE_MAX_COLS);
	if interior == 0 {
		return (0, 0);
	}
	omp_tui::components::image_cell_box(image.px, interior, TOOL_IMAGE_MAX_ROWS)
}

fn tool_height(tool: &ToolEntry, width: u16) -> u16 {
	let image_rows = tool
		.images
		.iter()
		.fold(0_u16, |rows, image| rows.saturating_add(tool_image_box(image, width).1));
	tool
		.view
		.height()
		.saturating_add(image_rows)
		.saturating_add(2)
		.max(3)
}

fn preserves_attachments(text: &str) -> bool {
	let first = text.split_whitespace().next().unwrap_or_default();
	first.starts_with('/') && first.get(1..).is_some_and(|command| !command.contains('/'))
}

fn draw_box(
	frame: &mut Frame,
	rect: Rect,
	border: Style,
	fill: Style,
	charset: Charset,
	native: bool,
) {
	if rect.width < 2 || rect.height < 2 {
		return;
	}
	let (tl, tr, bl, br, h, v) = charset.border(Border::Round);
	let mut glyph = [0_u8; 4];
	frame.fill(rect, fill);
	frame.put(rect.x, rect.y, tl.encode_utf8(&mut glyph), border);
	frame.put(rect.x + rect.width - 1, rect.y, tr.encode_utf8(&mut glyph), border);
	frame.put(rect.x, rect.y + rect.height - 1, bl.encode_utf8(&mut glyph), border);
	frame.put(rect.x + rect.width - 1, rect.y + rect.height - 1, br.encode_utf8(&mut glyph), border);
	for x in rect.x + 1..rect.x + rect.width - 1 {
		frame.put(x, rect.y, h.encode_utf8(&mut glyph), border);
		frame.put(x, rect.y + rect.height - 1, h.encode_utf8(&mut glyph), border);
	}
	for y in rect.y + 1..rect.y + rect.height - 1 {
		frame.put(rect.x, y, v.encode_utf8(&mut glyph), border);
		frame.put(rect.x + rect.width - 1, y, v.encode_utf8(&mut glyph), border);
	}
	if native {
		frame.push_noselect(rect);
	}
}

fn draw_line(frame: &mut Frame, x: u16, y: u16, width: u16, spans: &[Span<'_>]) -> u16 {
	let right = x.saturating_add(width);
	let mut at = x;
	for span in spans {
		for grapheme in xutf::graphemes_str(span.text) {
			if at >= right {
				return at;
			}
			let next = frame.put(at, y, grapheme, span.style);
			if next == at {
				return at;
			}
			at = next;
		}
	}
	at
}

fn draw_flowed(frame: &mut Frame, rect: Rect, spans: &[Span<'_>]) -> u16 {
	if rect.width == 0 || rect.height == 0 {
		return 0;
	}
	let mut x = rect.x;
	let mut y = rect.y;
	let right = rect.x.saturating_add(rect.width);
	let bottom = rect.y.saturating_add(rect.height);
	for span in spans {
		for grapheme in xutf::graphemes_str(span.text) {
			if grapheme == "\n" {
				x = rect.x;
				y = y.saturating_add(1);
				if y >= bottom {
					return y.saturating_sub(rect.y);
				}
				continue;
			}
			let width = visible_width(grapheme);
			if x > rect.x && x.saturating_add(width) > right {
				frame.set_soft_wrap(y);
				x = rect.x;
				y = y.saturating_add(1);
			}
			if y >= bottom {
				return y.saturating_sub(rect.y);
			}
			x = frame.put(x, y, grapheme, span.style);
		}
	}
	y.saturating_sub(rect.y).saturating_add(1)
}

fn flowed_height(text: &str, width: u16) -> u16 {
	if width == 0 {
		return 0;
	}
	let mut rows = 1_u16;

	let mut column = 0_u16;
	for grapheme in xutf::graphemes_str(text) {
		if grapheme == "\n" {
			rows = rows.saturating_add(1);
			column = 0;
			continue;
		}
		let size = visible_width(grapheme);
		if column > 0 && column.saturating_add(size) > width {
			rows = rows.saturating_add(1);
			column = 0;
		}
		column = column.saturating_add(size);
	}
	rows
}
fn compaction_method_label(method: Option<&str>) -> &'static str {
	match method {
		Some("prune") => "pruned",
		Some("drop_media") => "media-dropped",
		Some("elide") => "elided",
		Some("local") => "locally-compacted",
		Some("remote") => "remote-compacted",
		Some("handoff") => "handed-off",
		Some(_) | None => "compacted",
	}
}

fn hud_line(text: Str, charset: Charset) -> Str {
	match collapse_hud_line(&text, charset) {
		std::borrow::Cow::Borrowed(_) => text,
		std::borrow::Cow::Owned(collapsed) => Str::new(collapsed),
	}
}

fn explicit_line_count(text: &str) -> u16 {
	u16::try_from(text.lines().count().max(1)).unwrap_or(u16::MAX)
}

fn read_group_label(count: usize) -> Str {
	fmts_mut!("Read {count} files").freeze()
}

fn agent_label(agent: &AgentRow, charset: Charset) -> Str {
	let mut label = StrMut::with_capacity(64);
	for _ in 0..agent.depth.min(4) {
		label.push_str("  ");
	}
	let _ = write!(label, "{} {} · {}", charset.icon(Icon::Task), agent.name, agent.status);
	if let Some(tool) = &agent.tool {
		let _ = write!(label, " · {tool}");
	}
	if let Some(tokens) = agent.tokens {
		let _ = write!(label, " · {}", compact_count(tokens));
	}
	label.freeze()
}

const fn elapsed_label_key(elapsed: Duration) -> u64 {
	let seconds = elapsed.as_secs();
	if seconds < 60 {
		seconds
	} else if seconds < 3_600 {
		60 + seconds / 60
	} else {
		let hours = seconds / 3_600;
		3_600 + if hours > 99 { 99 } else { hours }
	}
}

fn elapsed_label(elapsed: Duration) -> Str {
	let seconds = elapsed.as_secs();
	if seconds < 60 {
		fmts_mut!("{seconds}s").freeze()
	} else if seconds < 3_600 {
		fmts_mut!("{}m", seconds / 60).freeze()
	} else {
		fmts_mut!("{}h", (seconds / 3_600).min(99)).freeze()
	}
}

fn compact_count(value: u64) -> Str {
	if value >= 1_000_000 {
		sf!("{:.1}m", value as f64 / 1_000_000.0)
	} else if value >= 1_000 {
		sf!("{:.0}k", value as f64 / 1_000.0)
	} else {
		sf!("{value}")
	}
}
fn context_usage_label(tokens: u64, window: Option<u64>) -> (Str, bool) {
	let Some(window) = window.filter(|window| *window > 0) else {
		return (compact_count(tokens), false);
	};
	let overflow = tokens > window;
	let percent = tokens as f64 / window as f64 * 100.0;
	let window = compact_count(window);
	let label = if percent > 0.0 && percent < 1.0 {
		sf!("{percent:.1}%/{window}")
	} else {
		sf!("{percent:.0}%/{window}")
	};
	(label, overflow)
}

fn visible_width(text: &str) -> u16 {
	u16::try_from(xutf::width_str(text)).unwrap_or(u16::MAX)
}
const fn base_style(theme: Theme) -> Style {
	Style::new().fg(theme.fg)
}
const fn panel_style(theme: Theme) -> Style {
	Style::new().fg(theme.fg).bg(theme.panel)
}
const fn ink(color: Color) -> Style {
	Style::new().fg(color)
}
const fn prose_style(theme: Theme) -> Style {
	Style::new().fg(theme.muted).italic()
}

#[cfg(test)]
mod tests {
	use super::*;

	fn ctx() -> UiContext {
		UiContext::default()
	}

	#[test]
	fn live_waveform_uses_charset_specific_activity_bands() {
		let mut waveform = ActivityWaveform::new();
		for band in 0..=4 {
			waveform.push(band);
		}
		assert_eq!(activity_waveform_label(&waveform, Charset::Ascii), "live .:-*#");
		assert_eq!(activity_waveform_label(&waveform, Charset::Unicode), "live ▁▂▄▆█");
	}

	fn row_text(frame: &Frame, row: u16) -> String {
		omp_tui::test_support::frame_row_text(frame, row)
	}

	fn present_and_assert_terminal(
		chat: &mut Chat,
		renderer: &mut omp_tui::Renderer<Vec<u8>>,
		terminal: &mut omp_tui::test_support::TerminalModel,
		viewport: Size,
	) -> (u16, SmallVec<(u16, u16), 4>) {
		let rendered = chat.render(viewport);
		let height = rendered.frame.size().height;
		let damage = rendered.damage.clone();
		renderer
			.present_overlaid(
				rendered.frame,
				rendered.damage.as_slice(),
				viewport.height,
				rendered.stable_rows,
				&[],
			)
			.expect("chat frame presents");
		let window_top = height.saturating_sub(viewport.height);
		let expected = (window_top..height)
			.map(|row| row_text(rendered.frame, row))
			.collect::<Vec<_>>();
		let output = std::mem::take(renderer.writer_mut());
		terminal.apply(std::str::from_utf8(&output).expect("renderer output is UTF-8"));
		assert_eq!(terminal.visible_rows(), expected.as_slice());
		(height, damage)
	}

	fn text_color(frame: &Frame, needle: &str) -> Option<Color> {
		(0..frame.size().height).find_map(|row| {
			let text = row_text(frame, row);
			let byte = text.find(needle)?;
			let column = visible_width(&text[..byte]);
			Some(frame.cell(column, row).style().foreground_color())
		})
	}

	#[test]
	fn completion_reflow_damages_rows_vacated_when_popup_closes() {
		let viewport = Size::new(80, 24);
		let mut chat = Chat::new(&ctx());
		chat.set_slash_commands(vec![
			Command::new("model", "Choose a model", &[]),
			Command::new("models", "List available models", &[]),
			Command::new("mode", "Change interaction mode", &[]),
		]);
		let mut renderer = omp_tui::Renderer::new(Vec::new());
		let mut terminal = omp_tui::test_support::TerminalModel::new(
			usize::from(viewport.width),
			usize::from(viewport.height),
		);

		let _ = present_and_assert_terminal(&mut chat, &mut renderer, &mut terminal, viewport);
		let collapsed_height = chat.composer_rows();

		assert_eq!(chat.handle_key(Key::Char('/')), ChatKey::Consumed);
		let _ = present_and_assert_terminal(&mut chat, &mut renderer, &mut terminal, viewport);
		let expanded_height = chat.composer_rows();
		assert!(expanded_height > collapsed_height, "completion popup must grow the editor");

		assert_eq!(chat.handle_key(Key::Esc), ChatKey::Ignored);
		let (_, damage) =
			present_and_assert_terminal(&mut chat, &mut renderer, &mut terminal, viewport);
		assert_eq!(chat.composer_rows(), collapsed_height, "closing popup must shrink the editor");
		assert_eq!(
			damage.as_slice(),
			&[(chat.transcript_rows, chat.frame.size().height)],
			"the shrink frame must repaint the whole mutable suffix, including vacated popup rows",
		);
	}

	#[test]
	fn live_panel_is_content_sized_and_reflows_without_stale_rows() {
		let viewport = Size::new(80, 24);
		let mut chat = Chat::new(&ctx());
		let mut renderer = omp_tui::Renderer::new(Vec::new());
		let mut terminal = omp_tui::test_support::TerminalModel::new(
			usize::from(viewport.width),
			usize::from(viewport.height),
		);
		let _ = present_and_assert_terminal(&mut chat, &mut renderer, &mut terminal, viewport);
		assert_eq!(chat.live_panel_height(viewport.width), 0);

		chat.agent_labels.push(sf!("Main · running · 0"));
		chat.bump_live();
		let (_, damage) =
			present_and_assert_terminal(&mut chat, &mut renderer, &mut terminal, viewport);
		assert_eq!(chat.live_panel_height(viewport.width), 3, "one label needs one row plus border");
		assert_eq!(damage.as_slice(), &[(chat.transcript_rows, chat.frame.size().height)]);

		chat.tool_started("tool", "read", "Read source");
		let prior_height = chat.live_panel_height(viewport.width);
		let (_, damage) =
			present_and_assert_terminal(&mut chat, &mut renderer, &mut terminal, viewport);
		assert!(chat.live_panel_height(viewport.width) > 3, "tool content must grow the panel");
		assert_eq!(damage.as_slice(), &[(chat.transcript_rows, chat.frame.size().height)]);

		chat.live_tools.clear();
		chat.agent_labels.clear();
		chat.bump_live();
		let (_, damage) =
			present_and_assert_terminal(&mut chat, &mut renderer, &mut terminal, viewport);
		assert!(prior_height > 3);
		assert_eq!(chat.live_panel_height(viewport.width), 0, "empty panel must occupy no rows");
		assert_eq!(
			damage.as_slice(),
			&[(chat.transcript_rows, chat.frame.size().height)],
			"shrinking the panel must repaint its vacated rows",
		);
	}

	#[test]
	fn composer_style_switch_updates_editor_and_status_geometry() {
		let mut chat = Chat::new(&ctx());
		let _ = chat.render(Size::new(80, 24));
		let cases = [
			(ComposerStyle::Box, 6),
			(ComposerStyle::Claude, 7),
			(ComposerStyle::Pi, 7),
			(ComposerStyle::Borderless, 5),
			(ComposerStyle::Rule, 7),
			(ComposerStyle::Field, 6),
			(ComposerStyle::Rail, 6),
		];
		for (style, rows) in cases {
			chat.set_composer_style(style);
			let pane = chat
				.editor_ui
				.root()
				.comp()
				.downcast_ref::<EditorPane>()
				.expect("chat root is editor pane");
			assert_eq!(pane.style(), style);
			assert_eq!(chat.composer_rows(), rows, "{style}");
		}
	}
	#[test]
	fn composer_status_moves_between_embedded_split_and_standalone_rows() {
		let mut chat = Chat::new(&ctx());
		chat.set_status(StatusFacts {
			model: sf!("model-a"),
			context_tokens: 50,
			context_window: Some(100),
			..StatusFacts::default()
		});
		let viewport = Size::new(80, 24);
		let _ = chat.render(viewport);

		chat.set_composer_style(ComposerStyle::Box);
		let rows = chat.composer_rows();
		let embedded = {
			let rendered = chat.render(viewport);
			row_text(rendered.frame, viewport.height - rows)
		};
		assert!(embedded.contains("omp"));
		assert!(!embedded.contains('%'), "box uses its boundary as the context gauge");

		chat.set_composer_style(ComposerStyle::Borderless);
		let rows = chat.composer_rows();
		let (top, bottom) = {
			let rendered = chat.render(viewport);
			(
				row_text(rendered.frame, viewport.height - rows),
				row_text(rendered.frame, viewport.height - 1),
			)
		};
		assert!(!top.contains("omp"));
		assert!(bottom.contains("omp"));
		assert!(bottom.contains('%'));

		chat.set_composer_style(ComposerStyle::Claude);
		let rows = chat.composer_rows();
		let (top, bottom) = {
			let rendered = chat.render(viewport);
			(
				row_text(rendered.frame, viewport.height - rows),
				row_text(rendered.frame, viewport.height - 1),
			)
		};
		assert!(top.contains('%'), "claude docks the right group onto its top rule");
		assert!(bottom.contains("omp"), "claude leaves the left group standalone");
	}

	#[test]
	fn mutation_api_commits_stable_rows_and_keeps_tail_anchored() {
		let mut chat = Chat::new(&ctx());
		chat.set_status(StatusFacts { model: sf!("model-a"), ..StatusFacts::default() });
		chat.push_user("hello", vec![]);
		chat.begin_assistant("a");
		chat.append_assistant("a", "world");
		let before = chat.render(Size::new(80, 24)).stable_rows;
		chat.end_assistant("a");
		let composer_rows = chat.composer_rows();
		let rendered = chat.render(Size::new(80, 24));
		assert!(rendered.stable_rows > before);
		assert!(row_text(rendered.frame, rendered.stable_rows - 2).contains("world"));
		let bottom = rendered.frame.size().height;
		assert!(
			(bottom - composer_rows..bottom)
				.any(|row| row_text(rendered.frame, row).contains("model-a"))
		);
	}

	#[test]
	fn right_rail_inset_reflows_errors_and_preserves_composer_border() {
		let mut chat = Chat::new(&ctx());
		chat.set_composer_style(ComposerStyle::Box);
		chat.set_right_inset(30);
		let viewport = Size::new(120, 40);
		let composer_rows = chat.composer_rows();
		let _ = chat.render(viewport);
		assert!(chat.next_wake().is_none());
		chat.set_status(StatusFacts { working: true, ..StatusFacts::default() });
		{
			let rendered = chat.render(viewport);
			let composer_top = rendered.frame.size().height - composer_rows;
			assert!(row_text(rendered.frame, composer_top).ends_with('╮'));
		}
		chat.set_status(StatusFacts::default());
		chat.push_error("x".repeat(100));
		let rendered = chat.render(viewport);
		assert_eq!(rendered.stable_rows, 3);
		let error_text = (0..rendered.stable_rows)
			.map(|row| row_text(rendered.frame, row))
			.collect::<String>();
		assert_eq!(error_text.matches('x').count(), 100);
		let composer_top = rendered.frame.size().height - composer_rows;
		assert!(row_text(rendered.frame, composer_top).ends_with('╮'));
	}

	#[test]
	fn inset_reflow_preserves_the_declared_stable_prefix() {
		let viewport = Size::new(48, 10);
		let mut chat = Chat::new(&ctx());
		chat.set_right_inset(18);
		for index in 0..12 {
			chat.push_notice(format!("committed notice {index}"));
		}
		let mut renderer = omp_tui::Renderer::new(Vec::new());
		let (stable_rows, retained) = {
			let rendered = chat.render(viewport);
			let stable_rows = rendered.stable_rows;
			let retained = rendered.frame.clone();
			renderer
				.present_overlaid(
					rendered.frame,
					rendered.damage.as_slice(),
					viewport.height,
					stable_rows,
					&[],
				)
				.expect("initial narrow frame presents");
			(stable_rows, retained)
		};
		assert!(renderer.committed_rows() > 0, "the fixture must establish native history");

		chat.set_right_inset(0);
		chat.push_user(
			"a newly committed message that should use all newly available columns",
			vec![],
		);
		let rendered = chat.render(viewport);
		assert_eq!(
			rendered.damage.first().map(|damage| damage.0),
			Some(stable_rows),
			"only the suffix at the prior stable seam may reflow",
		);
		for row in 0..stable_rows {
			for column in 0..viewport.width {
				assert_eq!(
					rendered.frame.cell(column, row),
					retained.cell(column, row),
					"stable cell changed at ({column}, {row})",
				);
			}
		}
		renderer
			.present_overlaid(
				rendered.frame,
				rendered.damage.as_slice(),
				viewport.height,
				rendered.stable_rows,
				&[],
			)
			.expect("widened suffix must satisfy the renderer's stable-row contract");
	}

	#[test]
	fn live_delta_damages_only_mutable_band() {
		let mut chat = Chat::new(&ctx());
		chat.push_user("stable", vec![]);
		let stable = chat.render(Size::new(80, 30)).stable_rows;
		chat.begin_assistant("a");
		chat.append_assistant("a", "stream");
		let rendered = chat.render(Size::new(80, 30));
		assert_eq!(rendered.stable_rows, stable);
		assert!(rendered.damage.iter().all(|(start, _)| *start >= stable));
	}

	#[test]
	fn hud_titles_collapse_multiline_previews_with_return_glyph() {
		let mut chat = Chat::new(&ctx());
		chat.tool_started(
			"tool",
			"task",
			"Complete assignment thoroughly:\n\n  # Target\nFiles: src/foo.rs",
		);
		chat.set_session_title("First line\n\nSecond line");
		let rendered = chat.render(Size::new(100, 30));
		let text = (0..rendered.frame.size().height)
			.map(|row| row_text(rendered.frame, row))
			.collect::<Vec<_>>()
			.join("\n");
		assert!(
			text.contains("Complete assignment thoroughly: ↵ # Target ↵ Files: src/foo.rs"),
			"{text}",
		);
		assert!(text.contains("First line ↵ Second line"), "{text}");
	}

	#[test]
	fn consecutive_reads_group_until_another_transcript_entry() {
		let mut chat = Chat::new(&ctx());
		chat.tool_started("read-a", "read", "src/a.rs");
		chat.tool_finished("read-a", true, sf!("a"));
		chat.tool_started("read-b", "read", "src/b.rs");
		chat.tool_finished("read-b", true, sf!("b"));
		assert!(matches!(&chat.transcript[..], [Entry::ToolGroup(group)] if group.tools.len() == 2));

		chat.tool_started("shell", "bash", "cargo metadata");
		chat.tool_finished("shell", true, sf!("ok"));
		chat.tool_started("read-c", "read", "src/c.rs");
		chat.tool_finished("read-c", true, sf!("c"));
		assert!(matches!(
			&chat.transcript[..],
			[Entry::ToolGroup(_), Entry::Tool(shell), Entry::Tool(read)]
				if shell.name.as_str() == "bash" && read.name.as_str() == "read"
		));
	}

	#[test]
	fn resize_preview_does_not_mutate_retained_geometry() {
		let mut chat = Chat::new(&ctx());
		chat.push_user("a line that wraps when narrow", vec![]);
		let original = chat.render(Size::new(80, 24)).frame.size();
		let original_width = match &chat.transcript[0] {
			Entry::User(user) => user.body.width,
			_ => unreachable!(),
		};
		let preview = chat.render_resize_preview(Size::new(30, 12));
		assert_eq!(preview.size(), Size::new(30, 12));
		assert_eq!(chat.frame.size(), original);
		let retained_width = match &chat.transcript[0] {
			Entry::User(user) => user.body.width,
			_ => unreachable!(),
		};
		assert_eq!(retained_width, original_width);
	}

	#[test]
	fn status_uses_only_supplied_facts_and_git_is_optional() {
		let mut chat = Chat::new(&ctx());
		chat.set_status(StatusFacts { model: sf!("real/model"), ..StatusFacts::default() });
		let composer_rows = chat.composer_rows();
		let frame = chat.render(Size::new(100, 24)).frame;
		let bottom = frame.size().height;
		let status = (bottom - composer_rows..bottom)
			.map(|row| row_text(frame, row))
			.collect::<Vec<_>>()
			.join(" ");
		assert!(status.contains("real/model"));
		assert!(!status.contains("main"));
	}

	#[test]
	fn context_usage_label_preserves_overflow_past_one_hundred_percent() {
		let (label, overflow) = context_usage_label(240_000, Some(200_000));
		assert_eq!(label, "120%/200k");
		assert!(overflow);
	}

	#[test]
	fn compaction_preview_renders_method_token_badge_and_title() {
		let mut chat = Chat::new(&ctx());
		chat.push_compaction(
			sf!("full summary body"),
			Some(sf!("Fixing login TTL")),
			Some(sf!("remote")),
			256_000,
			Some(20_000),
		);
		let frame = chat.render(Size::new(100, 24)).frame;
		let rendered = (0..frame.size().height)
			.map(|row| row_text(frame, row))
			.collect::<Vec<_>>()
			.join(" ");
		assert!(rendered.contains("remote-compacted"));
		assert!(rendered.contains("256k→20k"));
		assert!(rendered.contains("Fixing login TTL"));
	}

	#[test]
	fn status_pulses_running_speculation_and_holds_armed_in_accent() {
		let context = ctx();
		let mut chat = Chat::new(&context);
		chat.set_composer_style(ComposerStyle::Claude);
		chat.set_status(StatusFacts {
			model: sf!("model"),
			context_tokens: 42,
			context_window: Some(1_000),
			compaction_speculation: CompactionSpeculationStatus::Running,
			..StatusFacts::default()
		});
		let icon = context.charset.icon(Icon::Auto);
		let frame = chat.render_at(Size::new(100, 24), Duration::ZERO).frame;
		assert_eq!(text_color(frame, icon), Some(context.theme.accent));
		let frame = chat.render_at(Size::new(100, 24), SPECULATION_PULSE).frame;
		assert_eq!(text_color(frame, icon), Some(context.theme.muted));

		chat.set_status(StatusFacts {
			compaction_speculation: CompactionSpeculationStatus::Armed,
			..chat.status()
		});
		let frame = chat.render_at(Size::new(100, 24), SPECULATION_PULSE).frame;
		assert_eq!(text_color(frame, icon), Some(context.theme.accent));
	}

	#[test]
	fn chips_are_rendered_from_public_user_mutation() {
		let mut chat = Chat::new(&ctx());
		chat.push_user("inspect", vec![sf!("image.png")]);
		let frame = chat.render(Size::new(80, 24)).frame;
		assert!((0..frame.size().height).any(|row| row_text(frame, row).contains("image.png")));
	}

	#[test]
	fn composer_selection_restyles_xml_without_losing_selection_background() {
		let context = ctx();
		let mut chat = Chat::new(&context);
		let viewport = Size::new(80, 24);
		let _ = chat.render(viewport);
		for character in "<tag>value</tag>".chars() {
			assert_eq!(chat.handle_key(Key::Char(character)), ChatKey::Consumed);
		}
		assert_eq!(chat.handle_key(Key::SelectAll), ChatKey::Consumed);
		let composer_rows = chat.composer_rows();
		let frame = chat.render(viewport).frame;
		let input_y = frame
			.size()
			.height
			.saturating_sub(composer_rows)
			.saturating_add(1);
		let input_x = row_text(frame, input_y)
			.find('<')
			.and_then(|column| u16::try_from(column).ok())
			.expect("selected XML opening tag is visible");
		assert_eq!(frame.cell(input_x, input_y).style().background_color(), context.theme.selection,);
	}

	#[test]
	fn slash_detours_preserve_staged_attachments_but_paths_submit_them() {
		let mut chat = Chat::new(&ctx());
		chat.attachments.push_text("payload");
		chat.set_composer_text("/models");
		assert_eq!(chat.handle_key(Key::Enter), ChatKey::Consumed);
		let (_, submitted, _) = chat.take_submission().expect("slash command submitted");
		assert!(submitted.is_empty());
		assert_eq!(chat.attachments.take().len(), 1);

		chat.attachments.push_text("payload");
		chat.set_composer_text("/wat");
		assert_eq!(chat.handle_key(Key::Enter), ChatKey::Consumed);
		let (_, submitted, _) = chat
			.take_submission()
			.expect("unknown slash command submitted");
		assert!(submitted.is_empty());
		assert_eq!(chat.attachments.take().len(), 1);

		chat.attachments.push_text("payload");
		chat.set_composer_text("/tmp/input.txt");
		assert_eq!(chat.handle_key(Key::Enter), ChatKey::Consumed);
		let (_, submitted, _) = chat.take_submission().expect("path submitted");
		assert_eq!(submitted.len(), 1);
	}

	#[test]
	fn empty_enter_while_working_emits_abort_signal_without_draining_chips() {
		let mut chat = Chat::new(&ctx());
		chat.attachments.push_text("payload");
		chat.set_status(StatusFacts { working: true, ..StatusFacts::default() });
		assert_eq!(chat.handle_key(Key::Enter), ChatKey::Consumed);
		let (text, submitted, mode) = chat
			.take_submission()
			.expect("working empty enter submitted");
		assert_eq!(text, "");
		assert!(submitted.is_empty());
		assert_eq!(mode, SubmitMode::Steer);
		assert_eq!(chat.attachments.take().len(), 1);
	}

	#[test]
	fn enter_steers_and_follow_up_queues() {
		let mut chat = Chat::new(&ctx());
		chat.set_composer_text("steer this");
		assert_eq!(chat.handle_key(Key::Enter), ChatKey::Consumed);
		let (text, _, mode) = chat.take_submission().expect("enter submits");
		assert_eq!(text, "steer this");
		assert_eq!(mode, SubmitMode::Steer);

		chat.set_composer_text("later please");
		assert_eq!(chat.handle_key(Key::FollowUp), ChatKey::Consumed);
		let (text, _, mode) = chat.take_submission().expect("follow-up submits");
		assert_eq!(text, "later please");
		assert_eq!(mode, SubmitMode::FollowUp);

		assert_eq!(chat.handle_key(Key::FollowUp), ChatKey::Consumed);
		assert!(chat.take_submission().is_none(), "empty follow-up stages nothing");
	}

	#[test]
	fn split_submission_steers_first_and_queues_followups() {
		let mut chat = Chat::new(&ctx());
		chat.set_composer_text("first\n---\nsecond\n///\nthird");
		assert_eq!(chat.handle_key(Key::Enter), ChatKey::Consumed);
		for (expected, mode) in [
			("first", SubmitMode::Steer),
			("second", SubmitMode::FollowUp),
			("third", SubmitMode::FollowUp),
		] {
			let (text, _, actual) = chat.take_submission().expect("split item");
			assert_eq!(text, expected);
			assert_eq!(actual, mode);
		}
		assert!(chat.take_submission().is_none());
	}

	#[test]
	fn semantic_transcript_frames_keep_kind_and_detail_visible() {
		let mut chat = Chat::new(&ctx());
		chat.push_transcript_frame(TranscriptFrame {
			kind:   TranscriptFrameKind::Handoff,
			title:  sf!("Transferred session"),
			detail: Some(sf!("focus on UI")),
		});
		let Some(Entry::Notice { text, error }) = chat.transcript.last() else {
			panic!("notice")
		};
		assert!(!error);
		assert!(text.contains("handoff"));
		assert!(text.contains("focus on UI"));
	}

	#[test]
	fn raw_scene_chrome_uses_the_supplied_theme() {
		let mut context = ctx();
		context.theme = Theme::for_appearance(omp_tui::Appearance::Light);
		let mut frame = Frame::new(Size::new(20, 5));
		let assistant = Some(LiveAssistant { id: sf!("a"), text: StrMut::new("stream") });
		draw_live_panel_impl(
			&mut frame,
			Rect::new(0, 0, 20, 5),
			assistant.as_ref(),
			&[],
			&[],
			&context,
			Duration::ZERO,
		);

		let border = frame.cell(0, 0).style();
		assert_eq!(border.foreground_color(), context.theme.border);
		assert_eq!(frame.cell(10, 3).style().background_color(), context.theme.panel);
		assert_eq!(frame.cell(2, 1).style().foreground_color(), context.theme.muted);
	}

	#[test]
	fn clear_history_resets_stable_prefix() {
		let mut chat = Chat::new(&ctx());
		chat.push_notice("notice");
		assert!(chat.render(Size::new(60, 20)).stable_rows > 0);
		chat.clear_history();
		assert_eq!(chat.render(Size::new(60, 20)).stable_rows, 0);
	}

	#[test]
	fn tool_result_images_render_inline_in_committed_cards() {
		// pi UI-06/UI-20: image payloads returned by tools (including PDF
		// page screenshots) render inline in the committed card instead of
		// only appearing as a text label.
		let path =
			std::env::temp_dir().join(format!("omp-chat-tool-image-{}.png", std::process::id()));
		omp_tui::test_support::write_test_png(&path, 8, 8, [255, 0, 0]);
		let source = Str::new(path.to_string_lossy().as_ref());

		let mut chat = Chat::new(&ctx());
		chat.tool_started("t1", "read", "read page.pdf:p1.png");
		chat.tool_image("t1", source);
		chat.tool_finished("t1", true, sf!("<row>rendered page 1</row>"));
		let frame = chat.render(Size::new(80, 40)).frame;
		std::fs::remove_file(&path).ok();
		assert!(
			(0..frame.size().height).any(|row| row_text(frame, row).contains('▀')),
			"committed tool card renders half-block image rows"
		);
		assert!(
			(0..frame.size().height).any(|row| row_text(frame, row).contains("rendered page 1")),
			"renderer view stays alongside the inline image"
		);
	}

	#[test]
	fn undecodable_tool_image_keeps_the_text_card() {
		let mut chat = Chat::new(&ctx());
		chat.tool_started("t1", "shell", "shell ls");
		chat.tool_image("t1", "/nonexistent/omp-tool-image.png");
		chat.tool_finished("t1", true, sf!("<row>done</row>"));
		let frame = chat.render(Size::new(80, 24)).frame;
		assert!((0..frame.size().height).any(|row| row_text(frame, row).contains("done")));
		assert!((0..frame.size().height).all(|row| !row_text(frame, row).contains('▀')));
	}

	#[test]
	fn live_tool_view_replaces_retained_markup_without_line_vectors() {
		let mut chat = Chat::new(&ctx());
		chat.tool_started("t1", "same-name", "same-name");
		chat.tool_view("t1", sf!("<row>first update</row>"));
		chat.tool_view("t1", sf!("<row>second update</row>"));
		let live = chat.render(Size::new(80, 24)).frame;
		assert!((0..live.size().height).any(|row| row_text(live, row).contains("second update")));
		assert!((0..live.size().height).all(|row| !row_text(live, row).contains("first update")));

		chat.tool_finished("t1", false, sf!("<row>fault branch</row>"));
		let settled = chat.render(Size::new(80, 24)).frame;
		assert!(
			(0..settled.size().height).any(|row| row_text(settled, row).contains("fault branch"))
		);
	}

	#[test]
	fn session_title_truncates_between_boundary_cells() {
		for (width, expected) in [(7, " alph…"), (3, " …")] {
			let mut frame = Frame::new(Size::new(width, 1));
			draw_session_title_impl(&mut frame, 0, 0, "alphabet", Theme::default());
			assert_eq!(row_text(&frame, 0), expected);
			assert!(visible_width(&row_text(&frame, 0)) < width);
		}
	}

	#[test]
	fn context_status_uses_the_themed_compaction_threshold_color() {
		let accent = Color::Rgb(12, 34, 56);
		let mut context = ctx();
		context.theme.accent = accent;
		let mut chat = Chat::new(&context);
		chat.set_composer_style(ComposerStyle::Claude);
		chat.set_status(StatusFacts {
			model: sf!("model"),
			context_tokens: 42,
			context_window: Some(1_000),
			..StatusFacts::default()
		});
		let frame = chat.render(Size::new(100, 24)).frame;
		let (row, column) = (0..frame.size().height)
			.find_map(|row| {
				let text = row_text(frame, row);
				let byte = text.find("4%/1k")?;
				Some((row, visible_width(&text[..byte])))
			})
			.expect("context status visible");
		assert_eq!(frame.cell(column, row).style().foreground_color(), accent);
	}

	#[test]
	fn dropped_prompt_restores_text_and_attachments_without_history_row() {
		let mut chat = Chat::new(&ctx());
		chat.attachments.push_text("attached payload");
		chat.set_composer_text("retry this #1");
		assert_eq!(chat.handle_key(Key::Enter), ChatKey::Consumed);
		let (text, attachments, _) = chat.take_submission().expect("submission staged");
		let _ = chat.apply_backend_event(BackendEvent::UserReplayed {
			text:  Str::new(text.as_str()),
			chips: vec![sf!("#1 pasted text")],
		});
		assert_eq!(chat.transcript.len(), 1);

		let _ = chat.apply_backend_event(BackendEvent::PromptDropped {
			text: Str::new(text.as_str()),
			attachments,
		});

		assert_eq!(chat.composer_text(), text);
		assert_eq!(chat.attachments.len(), 1);
		assert!(chat.transcript.is_empty());
	}

	#[test]
	fn dropped_prompt_does_not_overwrite_a_new_draft() {
		let mut chat = Chat::new(&ctx());
		chat.push_user("old prompt", Vec::new());
		chat.set_composer_text("new draft");

		let _ = chat.apply_backend_event(BackendEvent::PromptDropped {
			text:        sf!("old prompt"),
			attachments: Vec::new(),
		});

		assert_eq!(chat.composer_text(), "new draft");
		assert!(chat.transcript.is_empty());
	}

	#[test]
	fn rail_width_accumulates_all_visible_rails() {
		let rails = RailWidths::default()
			.accumulate(true, 12)
			.accumulate(false, 30)
			.accumulate(true, 8);
		assert_eq!(rails, RailWidths { left: 20, right: 30 });
		assert_eq!(rails.content_width(80), 30);
	}
}
