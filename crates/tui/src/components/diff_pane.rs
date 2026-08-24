//! Focusable, scrollable presentation and interaction for [`DiffDocument`].

use std::ops::Range;

use omp_core::{IntoStr, Str};
use smallvec::SmallVec;
use strum::{EnumString, IntoStaticStr};
use xutf::Text;

use super::{
	diff_doc::{DiffDocument, DiffMark, DiffRow, DiffRowKind, DiffSide, DiffStyleRun},
	radio::pill,
};
use crate::{
	Appearance, Theme,
	component::{Component, EventCtx, Flow, Hit, HitTag, PaintCtx, Slot, next_slot},
	context::UiContext,
	frame::{Color, Rect, Style},
	input::{Key, Mouse, UiEvent},
	props::{Prop, PropValue, Props},
};

/// How a [`DiffPane`] presents its document.
#[derive(Clone, Copy, Debug, Default, EnumString, Eq, IntoStaticStr, PartialEq)]
#[strum(serialize_all = "lowercase")]
pub enum ViewMode {
	/// Aligned old and new columns.
	#[default]
	Split,
	/// A full unified stream with replacements shown deletion first.
	Inline,
	/// Tight changed regions with hunk headers and action buttons.
	Hunk,
	/// The new-side file only.
	File,
}

/// Placeholder or ready state of a [`DiffPane`].
#[derive(Clone, Copy, Debug, Default, EnumString, Eq, IntoStaticStr, PartialEq)]
#[strum(serialize_all = "snake_case")]
pub enum DiffPaneState {
	/// No document is selected.
	#[default]
	Empty,
	/// A host is constructing or loading the document.
	Loading,
	/// The selected input is binary.
	Binary,
	/// The selected input exceeds the host's diff limit.
	TooLarge,
	/// The document is ready for display.
	Ready,
}

/// Which mutation family hunk buttons expose.
#[derive(Clone, Copy, Debug, EnumString, Eq, IntoStaticStr, PartialEq)]
#[strum(serialize_all = "lowercase")]
pub enum DiffPatchTarget {
	/// Stage changes, with an additional discard action.
	Stage,
	/// Unstage changes.
	Unstage,
}

/// A host-defined mutation requested from a diff pane.
#[derive(Clone, Copy, Debug, EnumString, Eq, IntoStaticStr, PartialEq)]
#[strum(serialize_all = "lowercase")]
pub enum DiffActionKind {
	/// Stage the target.
	Stage,
	/// Unstage the target.
	Unstage,
	/// Discard the target.
	Discard,
}

/// Scope resolved for a [`DiffActionKind`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiffTarget {
	/// The complete file.
	File,
	/// A zero-based tight hunk index.
	Hunk(usize),
	/// Inclusive old/new line ranges; `(0, 0)` means that side is absent.
	Lines {
		/// Inclusive old-side line range.
		old: (u32, u32),
		/// Inclusive new-side line range.
		new: (u32, u32),
	},
}

/// Source line ranges represented by the current cursor or shift selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiffSelection {
	/// Inclusive old-side line range; `(0, 0)` means absent.
	pub old:      (u32, u32),
	/// Inclusive new-side line range; `(0, 0)` means absent.
	pub new:      (u32, u32),
	/// Whether shift created an explicit multi-row selection.
	pub explicit: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SideKind {
	Old,
	New,
	Both,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Visual {
	Split { row: usize, segment: u16 },
	Line { row: usize, side: SideKind, segment: u16 },
	File { line: usize, row: Option<usize>, segment: u16 },
	Header { hunk: usize },
	Blank,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MapKind {
	Context,
	Change,
	Add,
	Del,
	Hunk,
}

#[derive(Default)]
struct LayoutCache {
	version: u64,
	mode:    ViewMode,
	wrap:    bool,
	width:   u16,
	visuals: Vec<Visual>,
}

#[derive(Clone, Copy)]
struct Palette {
	add_soft:   Color,
	add_strong: Color,
	del_soft:   Color,
	del_strong: Color,
	fill_add:   Color,
	fill_del:   Color,
	selection:  Color,
}

impl Palette {
	fn new(ctx: &UiContext) -> Self {
		let dark = ctx.appearance != Appearance::Light;
		let soft = if dark { 0.18 } else { 0.24 };
		let strong = if dark { 0.42 } else { 0.48 };
		let canvas = ctx.theme.panel;
		Self {
			add_soft:   canvas.mix(ctx.theme.ok, soft),
			add_strong: canvas.mix(ctx.theme.ok, strong),
			del_soft:   canvas.mix(ctx.theme.err, soft),
			del_strong: canvas.mix(ctx.theme.err, strong),
			fill_add:   canvas.mix(ctx.theme.ok, 0.07),
			fill_del:   canvas.mix(ctx.theme.err, 0.07),
			selection:  canvas.mix(ctx.theme.fg, 0.14),
		}
	}
}

/// General-purpose interactive old/new text diff viewer.
pub struct DiffPane {
	props:         Props,
	slot:          Slot,
	document:      Option<DiffDocument>,
	state:         DiffPaneState,
	empty_message: Str,
	mode:          ViewMode,
	wrap:          bool,
	patch_target:  Option<DiffPatchTarget>,
	selected_hunk: usize,
	scroll_top:    usize,
	scroll_left:   u16,
	cursor:        usize,
	anchor:        Option<usize>,
	last_width:    u16,
	last_height:   u16,
	version:       u64,
	layout:        LayoutCache,
}

impl DiffPane {
	/// Creates an empty diff pane.
	pub fn new() -> Self {
		Self {
			props:         Props::new(),
			slot:          next_slot(),
			document:      None,
			state:         DiffPaneState::Empty,
			empty_message: Str::new_static("No changes"),
			mode:          ViewMode::Split,
			wrap:          false,
			patch_target:  None,
			selected_hunk: 0,
			scroll_top:    0,
			scroll_left:   0,
			cursor:        0,
			anchor:        None,
			last_width:    80,
			last_height:   1,
			version:       0,
			layout:        LayoutCache::default(),
		}
	}

	/// Sets one component property, including its stable `id`.
	pub fn with(mut self, prop: Prop, value: impl Into<PropValue>) -> Self {
		self.props.set(prop, value);
		self
	}

	/// Replaces the message shown in [`DiffPaneState::Empty`].
	pub fn set_empty_message(&mut self, message: impl IntoStr) {
		self.empty_message = message.into_str();
	}

	/// Returns the current immutable document, if any.
	pub const fn document(&self) -> Option<&DiffDocument> {
		self.document.as_ref()
	}

	/// Replaces the document and state, resetting navigation to the first
	/// change.
	pub fn set_document(&mut self, document: Option<DiffDocument>, state: DiffPaneState) {
		self.document = document;
		self.state = state;
		self.version = self.version.wrapping_add(1);
		self.scroll_top = 0;
		self.scroll_left = 0;
		self.cursor = 0;
		self.anchor = None;
		self.selected_hunk = 0;
		self.rebuild_layout(self.last_width);
		if !matches!(self.mode, ViewMode::File | ViewMode::Hunk)
			&& let Some(first) = self.layout.visuals.iter().position(|visual| {
				self
					.visual_kind(*visual)
					.is_some_and(|kind| kind != MapKind::Context)
			}) {
			self.focus_change(first);
		}
	}

	/// Current presentation mode.
	pub const fn mode(&self) -> ViewMode {
		self.mode
	}

	/// Selects a presentation mode and resets mode-local navigation.
	pub fn set_mode(&mut self, mode: ViewMode) {
		if self.mode == mode {
			return;
		}
		self.mode = mode;
		self.scroll_top = 0;
		self.scroll_left = 0;
		self.cursor = 0;
		self.anchor = None;
		self.selected_hunk = 0;
		self.rebuild_layout(self.last_width);
	}

	/// Advances split → inline → hunk → file → split.
	pub fn cycle_mode(&mut self) {
		let next = match self.mode {
			ViewMode::Split => ViewMode::Inline,
			ViewMode::Inline => ViewMode::Hunk,
			ViewMode::Hunk => ViewMode::File,
			ViewMode::File => ViewMode::Split,
		};
		self.set_mode(next);
	}

	/// Whether soft wrapping is enabled.
	pub const fn wraps(&self) -> bool {
		self.wrap
	}

	/// Toggles soft wrapping and resets horizontal pan.
	pub fn toggle_wrap(&mut self) {
		self.wrap = !self.wrap;
		self.scroll_left = 0;
		self.rebuild_layout(self.last_width);
	}

	/// Gates hunk action buttons for staged or unstaged content.
	pub const fn set_patch_target(&mut self, target: Option<DiffPatchTarget>) {
		self.patch_target = target;
	}

	/// Current source-line selection, if the visual cursor maps to source rows.
	pub fn selection(&self) -> Option<DiffSelection> {
		let range = self.selected_row_range()?;
		let document = self.document.as_ref()?;
		let mut old = (u32::MAX, 0u32);
		let mut new = (u32::MAX, 0u32);
		for row in &document.rows[range] {
			if let Some(side) = &row.old {
				old.0 = old.0.min(side.number);
				old.1 = old.1.max(side.number);
			}
			if let Some(side) = &row.new {
				new.0 = new.0.min(side.number);
				new.1 = new.1.max(side.number);
			}
		}
		if old.1 == 0 {
			old.0 = 0;
		}
		if new.1 == 0 {
			new.0 = 0;
		}
		Some(DiffSelection { old, new, explicit: self.anchor.is_some() })
	}

	/// Resolves an action using explicit selection, current hunk, then file
	/// precedence.
	pub fn request_action(&self, action: DiffActionKind) -> Option<UiEvent> {
		if !self.action_allowed(action) || self.document.is_none() {
			return None;
		}
		let id = self
			.props
			.id()
			.map_or_else(|| Str::new_static(""), Str::new);
		let target = if self.anchor.is_some() {
			let selection = self.selection()?;
			DiffTarget::Lines { old: selection.old, new: selection.new }
		} else if self.mode == ViewMode::Hunk {
			let hunks = &self.document.as_ref()?.hunks;
			if self.selected_hunk >= hunks.len() {
				DiffTarget::File
			} else {
				DiffTarget::Hunk(self.selected_hunk)
			}
		} else {
			DiffTarget::File
		};
		Some(UiEvent::DiffAction { id, action, target })
	}

	/// Jumps to the next (`1`) or previous (`-1`) changed region.
	pub fn jump_hunk(&mut self, direction: i8) -> bool {
		let Some(document) = &self.document else {
			return false;
		};
		if document.hunks.is_empty() || direction == 0 {
			return false;
		}
		if self.mode == ViewMode::Hunk {
			let next = if direction > 0 {
				self.selected_hunk.checked_add(1)
			} else {
				self.selected_hunk.checked_sub(1)
			};
			let Some(next) = next.filter(|index| *index < document.hunks.len()) else {
				return false;
			};
			self.selected_hunk = next;
			if let Some(header) = self
				.layout
				.visuals
				.iter()
				.position(|visual| *visual == Visual::Header { hunk: next })
			{
				self.cursor = header;
				self.anchor = None;
				self.scroll_top = header.saturating_sub(1);
				self.clamp_scroll();
			}
			return true;
		}
		let mut starts: SmallVec<usize, 32> = SmallVec::new();
		let mut changed = false;
		for (index, visual) in self.layout.visuals.iter().copied().enumerate() {
			let here = self
				.visual_kind(visual)
				.is_some_and(|kind| kind != MapKind::Context);
			if here && !changed {
				starts.push(index);
			}
			changed = here;
		}
		let next = if direction > 0 {
			starts.iter().copied().find(|start| *start > self.cursor)
		} else {
			starts
				.iter()
				.rev()
				.copied()
				.find(|start| *start < self.cursor)
		};
		let Some(next) = next else {
			return false;
		};
		self.focus_change(next);
		true
	}

	fn action_allowed(&self, action: DiffActionKind) -> bool {
		matches!(
			(self.patch_target, action),
			(Some(DiffPatchTarget::Stage), DiffActionKind::Stage | DiffActionKind::Discard)
				| (Some(DiffPatchTarget::Unstage), DiffActionKind::Unstage)
		)
	}

	fn body_width(width: u16) -> u16 {
		width.saturating_sub(1).max(1)
	}

	fn split_text_width(&self, width: u16) -> u16 {
		let gutter = self
			.document
			.as_ref()
			.map_or(3, |document| document.gutter_width);
		Self::body_width(width)
			.saturating_sub(gutter.saturating_add(1).saturating_mul(2).saturating_add(1))
			.checked_div(2)
			.unwrap_or(1)
			.max(1)
	}

	fn line_text_width(&self, width: u16) -> u16 {
		let gutter = self
			.document
			.as_ref()
			.map_or(3, |document| document.gutter_width);
		Self::body_width(width)
			.saturating_sub(gutter.saturating_mul(2).saturating_add(3))
			.max(1)
	}

	fn file_text_width(&self, width: u16) -> u16 {
		let gutter = self
			.document
			.as_ref()
			.map_or(3, |document| document.gutter_width);
		Self::body_width(width)
			.saturating_sub(gutter.saturating_add(1))
			.max(1)
	}

	fn segments(&self, width: u16, content: u16) -> u16 {
		if !self.wrap {
			1
		} else {
			content
				.max(1)
				.saturating_add(width - 1)
				.checked_div(width)
				.unwrap_or(1)
				.max(1)
		}
	}

	fn rebuild_layout(&mut self, width: u16) {
		if self.layout.version == self.version
			&& self.layout.mode == self.mode
			&& self.layout.wrap == self.wrap
			&& self.layout.width == width
		{
			return;
		}
		self.layout.visuals.clear();
		let Some(document) = &self.document else {
			self.layout = LayoutCache {
				version: self.version,
				mode: self.mode,
				wrap: self.wrap,
				width,
				visuals: Vec::new(),
			};
			return;
		};
		match self.mode {
			ViewMode::Split => {
				let text_width = self.split_text_width(width);
				for (row_index, row) in document.rows.iter().enumerate() {
					let content = row
						.old
						.as_ref()
						.map_or(0, |side| side.width)
						.max(row.new.as_ref().map_or(0, |side| side.width));
					for segment in 0..self.segments(text_width, content) {
						self
							.layout
							.visuals
							.push(Visual::Split { row: row_index, segment });
					}
				}
			},
			ViewMode::Inline => {
				let text_width = self.line_text_width(width);
				Self::push_inline_rows(
					document,
					&mut self.layout.visuals,
					0..document.rows.len(),
					text_width,
					self.wrap,
				);
			},
			ViewMode::Hunk => {
				let text_width = self.line_text_width(width);
				for hunk in 0..document.hunks.len() {
					self.layout.visuals.push(Visual::Header { hunk });
					let rows = document.hunks[hunk].rows.clone();
					Self::push_inline_rows(
						document,
						&mut self.layout.visuals,
						rows,
						text_width,
						self.wrap,
					);
					self.layout.visuals.push(Visual::Blank);
				}
			},
			ViewMode::File => {
				let text_width = self.file_text_width(width);
				for (line, source) in document.file_lines.iter().enumerate() {
					let row = document.row_by_new_line.get(line + 1).copied().flatten();
					for segment in 0..self.segments(text_width, source.width) {
						self
							.layout
							.visuals
							.push(Visual::File { line, row, segment });
					}
				}
			},
		}
		self.layout.version = self.version;
		self.layout.mode = self.mode;
		self.layout.wrap = self.wrap;
		self.layout.width = width;
		self.clamp_scroll();
	}

	fn push_inline_rows(
		document: &DiffDocument,
		visuals: &mut Vec<Visual>,
		rows: Range<usize>,
		text_width: u16,
		wrap: bool,
	) {
		for row_index in rows {
			let row = &document.rows[row_index];
			if row.kind == DiffRowKind::Change {
				for segment in
					0..segments(text_width, row.old.as_ref().map_or(0, |side| side.width), wrap)
				{
					visuals.push(Visual::Line { row: row_index, side: SideKind::Old, segment });
				}
				for segment in
					0..segments(text_width, row.new.as_ref().map_or(0, |side| side.width), wrap)
				{
					visuals.push(Visual::Line { row: row_index, side: SideKind::New, segment });
				}
			} else {
				let side = match row.kind {
					DiffRowKind::Del => SideKind::Old,
					DiffRowKind::Add => SideKind::New,
					DiffRowKind::Context | DiffRowKind::Change => SideKind::Both,
				};
				let content = if side == SideKind::Old {
					row.old.as_ref().map_or(0, |value| value.width)
				} else {
					row.new.as_ref().map_or(0, |value| value.width)
				};
				for segment in 0..segments(text_width, content, wrap) {
					visuals.push(Visual::Line { row: row_index, side, segment });
				}
			}
		}
	}

	fn selected_row_range(&self) -> Option<Range<usize>> {
		let from = self
			.anchor
			.map_or(self.cursor, |anchor| anchor.min(self.cursor));
		let to = self
			.anchor
			.map_or(self.cursor, |anchor| anchor.max(self.cursor));
		let mut first = usize::MAX;
		let mut last = 0usize;
		let mut found = false;
		for visual in self.layout.visuals.get(from..=to)? {
			if let Some(row) = visual_row(*visual) {
				first = first.min(row);
				last = last.max(row);
				found = true;
			}
		}
		found.then_some(first..last.saturating_add(1))
	}

	fn clamp_scroll(&mut self) {
		let max_top = self
			.layout
			.visuals
			.len()
			.saturating_sub(usize::from(self.last_height.max(1)));
		self.scroll_top = self.scroll_top.min(max_top);
		let max_left = if self.wrap {
			0
		} else {
			self
				.document
				.as_ref()
				.map_or(0, |document| document.max_line_width.saturating_sub(1))
		};
		self.scroll_left = self.scroll_left.min(max_left);
		self.cursor = self.cursor.min(self.layout.visuals.len().saturating_sub(1));
	}

	fn scroll_by(&mut self, delta: i32) -> bool {
		let before = self.scroll_top;
		let max = self
			.layout
			.visuals
			.len()
			.saturating_sub(usize::from(self.last_height.max(1)));
		self.scroll_top = (self.scroll_top as i64 + i64::from(delta)).clamp(0, max as i64) as usize;
		self.scroll_top != before
	}

	fn move_cursor(&mut self, delta: i32, extend: bool) -> bool {
		if self.layout.visuals.is_empty() {
			return false;
		}
		if extend {
			self.anchor.get_or_insert(self.cursor);
		} else {
			self.anchor = None;
		}
		let before = self.cursor;
		self.cursor = (self.cursor as i64 + i64::from(delta))
			.clamp(0, self.layout.visuals.len().saturating_sub(1) as i64) as usize;
		if self.cursor < self.scroll_top {
			self.scroll_top = self.cursor;
		} else if self.cursor
			>= self
				.scroll_top
				.saturating_add(usize::from(self.last_height))
		{
			self.scroll_top = self
				.cursor
				.saturating_sub(usize::from(self.last_height.saturating_sub(1)));
		}
		before != self.cursor
	}

	fn cursor_edge(&mut self, end: bool, extend: bool) -> bool {
		if self.layout.visuals.is_empty() {
			return false;
		}
		if extend {
			self.anchor.get_or_insert(self.cursor);
		} else {
			self.anchor = None;
		}
		let next = if end {
			self.layout.visuals.len() - 1
		} else {
			0
		};
		let changed = next != self.cursor;
		self.cursor = next;
		self.scroll_top = if end {
			next.saturating_sub(usize::from(self.last_height.saturating_sub(1)))
		} else {
			0
		};
		changed
	}

	fn focus_change(&mut self, visual: usize) {
		self.cursor = visual;
		self.anchor = None;
		self.scroll_top = visual.saturating_sub(usize::from(self.last_height / 3));
		self.clamp_scroll();
	}

	fn visual_kind(&self, visual: Visual) -> Option<MapKind> {
		let document = self.document.as_ref()?;
		match visual {
			Visual::Split { row, .. } => Some(row_map_kind(document.rows[row].kind)),
			Visual::Line { row, side, .. } => Some(match (document.rows[row].kind, side) {
				(DiffRowKind::Change, SideKind::Old) => MapKind::Del,
				(DiffRowKind::Change, SideKind::New) => MapKind::Add,
				(kind, _) => row_map_kind(kind),
			}),
			Visual::File { row, .. } => Some(
				row.and_then(|row| document.rows.get(row))
					.map_or(MapKind::Context, |row| row_map_kind(row.kind)),
			),
			Visual::Header { .. } => Some(MapKind::Hunk),
			Visual::Blank => None,
		}
	}

	fn paint_placeholder(&self, pc: &mut PaintCtx<'_>, rect: Rect) {
		let message = match self.state {
			DiffPaneState::Empty | DiffPaneState::Ready => self.empty_message.as_str(),
			DiffPaneState::Loading => "Loading diff…",
			DiffPaneState::Binary => "Binary file",
			DiffPaneState::TooLarge => "File too large to diff",
		};
		let width = u16::try_from(message.visible_width()).unwrap_or(u16::MAX);
		let x = rect.x.saturating_add(rect.width.saturating_sub(width) / 2);
		let y = rect.y.saturating_add(rect.height / 2);
		pc.frame.put_clipped(
			x,
			y,
			rect.width.saturating_sub(x - rect.x),
			message,
			Style::new().fg(pc.ctx.theme.muted),
		);
	}

	fn paint_visual(
		&self,
		pc: &mut PaintCtx<'_>,
		rect: Rect,
		y: u16,
		visual: Visual,
		selected: bool,
		palette: Palette,
	) {
		let document = self.document.as_ref().expect("ready pane has a document");
		match visual {
			Visual::Blank => {},
			Visual::Header { hunk } => self.paint_header(pc, rect, y, hunk, selected),
			Visual::Split { row, segment } => {
				let row = &document.rows[row];
				let text_width = self.split_text_width(rect.width);
				let start = if self.wrap {
					segment.saturating_mul(text_width)
				} else {
					self.scroll_left
				};
				let mut x = rect.x;
				x = self.paint_side(
					pc,
					x,
					y,
					row.old.as_ref(),
					text_width,
					start,
					row.kind,
					true,
					segment == 0,
					selected,
					palette,
				);
				x = pc.frame.put(
					x,
					y,
					"│",
					Style::new().fg(pc.ctx.theme.border).bg(
						selected
							.then_some(palette.selection)
							.unwrap_or(Color::Default),
					),
				);
				self.paint_side(
					pc,
					x,
					y,
					row.new.as_ref(),
					text_width,
					start,
					row.kind,
					false,
					segment == 0,
					selected,
					palette,
				);
			},
			Visual::Line { row, side, segment } => {
				self.paint_line(pc, rect.x, y, &document.rows[row], side, segment, selected, palette);
			},
			Visual::File { line, segment, .. } => {
				let source = &document.file_lines[line];
				let gutter = document.gutter_width;
				let text_width = self.file_text_width(rect.width);
				let bg = selected
					.then_some(palette.selection)
					.unwrap_or(Color::Default);
				pc.frame
					.fill(Rect::new(rect.x, y, Self::body_width(rect.width), 1), Style::new().bg(bg));
				if segment == 0 {
					pc.frame
						.put(rect.x, y, &source.gutter, Style::new().fg(pc.ctx.theme.muted).bg(bg));
				}
				let start = if self.wrap {
					segment.saturating_mul(text_width)
				} else {
					self.scroll_left
				};
				paint_source(
					pc,
					rect.x.saturating_add(gutter + 1),
					y,
					text_width,
					&source.text,
					&source.styles,
					&[],
					start,
					bg,
					bg,
				);
			},
		}
	}

	#[allow(clippy::too_many_arguments, reason = "one aligned side's cached paint inputs")]
	fn paint_side(
		&self,
		pc: &mut PaintCtx<'_>,
		x: u16,
		y: u16,
		side: Option<&DiffSide>,
		text_width: u16,
		start: u16,
		kind: DiffRowKind,
		old: bool,
		first: bool,
		selected: bool,
		palette: Palette,
	) -> u16 {
		let gutter = self.document.as_ref().expect("document").gutter_width;
		let selected_bg = selected.then_some(palette.selection);
		if let Some(side) = side {
			let changed = kind == DiffRowKind::Change
				|| (old && kind == DiffRowKind::Del)
				|| (!old && kind == DiffRowKind::Add);
			let soft = if changed {
				if old {
					palette.del_soft
				} else {
					palette.add_soft
				}
			} else {
				Color::Default
			};
			let strong = if old {
				palette.del_strong
			} else {
				palette.add_strong
			};
			let bg = selected_bg.map_or(soft, |selection| soft.mix(selection, 0.45));
			let strong = selected_bg.map_or(strong, |selection| strong.mix(selection, 0.35));
			pc.frame
				.fill(Rect::new(x, y, gutter + text_width + 1, 1), Style::new().bg(bg));
			if first {
				pc.frame.put(
					x,
					y,
					&side.gutter,
					Style::new()
						.fg(if changed {
							if old {
								pc.ctx.theme.err
							} else {
								pc.ctx.theme.ok
							}
						} else {
							pc.ctx.theme.muted
						})
						.bg(bg),
				);
			}
			paint_source(
				pc,
				x.saturating_add(gutter + 1),
				y,
				text_width,
				&side.text,
				&side.styles,
				&side.marks,
				start,
				bg,
				strong,
			);
		} else {
			let fill = if old {
				palette.fill_del
			} else {
				palette.fill_add
			};
			let bg = selected_bg.map_or(fill, |selection| fill.mix(selection, 0.45));
			pc.frame
				.fill(Rect::new(x, y, gutter + text_width + 1, 1), Style::new().bg(bg));
		}
		x.saturating_add(gutter + text_width + 1)
	}

	fn paint_line(
		&self,
		pc: &mut PaintCtx<'_>,
		x: u16,
		y: u16,
		row: &DiffRow,
		side_kind: SideKind,
		segment: u16,
		selected: bool,
		palette: Palette,
	) {
		let document = self.document.as_ref().expect("document");
		let gutter = document.gutter_width;
		let text_width = self.line_text_width(self.last_width);
		let side = if side_kind == SideKind::Old {
			row.old.as_ref()
		} else {
			row.new.as_ref()
		};
		let is_del = side_kind == SideKind::Old && row.kind != DiffRowKind::Context;
		let is_add = side_kind == SideKind::New && row.kind != DiffRowKind::Context;
		let base = if is_del {
			palette.del_soft
		} else if is_add {
			palette.add_soft
		} else {
			Color::Default
		};
		let strong = if is_del {
			palette.del_strong
		} else if is_add {
			palette.add_strong
		} else {
			base
		};
		let selection = selected.then_some(palette.selection);
		let bg = selection.map_or(base, |selection| base.mix(selection, 0.45));
		let strong = selection.map_or(strong, |selection| strong.mix(selection, 0.35));
		pc.frame
			.fill(Rect::new(x, y, Self::body_width(self.last_width), 1), Style::new().bg(bg));
		if segment == 0 {
			if side_kind != SideKind::New
				&& let Some(old) = &row.old
			{
				pc.frame.put(
					x,
					y,
					&old.gutter,
					Style::new()
						.fg(if is_del {
							pc.ctx.theme.err
						} else {
							pc.ctx.theme.muted
						})
						.bg(bg),
				);
			}
			if side_kind != SideKind::Old
				&& let Some(new) = &row.new
			{
				pc.frame.put(
					x.saturating_add(gutter),
					y,
					&new.gutter,
					Style::new()
						.fg(if is_add {
							pc.ctx.theme.ok
						} else {
							pc.ctx.theme.muted
						})
						.bg(bg),
				);
			}
		}
		if let Some(side) = side {
			let start = if self.wrap {
				segment.saturating_mul(text_width)
			} else {
				self.scroll_left
			};
			paint_source(
				pc,
				x.saturating_add(gutter.saturating_mul(2) + 1),
				y,
				text_width,
				&side.text,
				&side.styles,
				&side.marks,
				start,
				bg,
				strong,
			);
		}
	}

	fn paint_header(&self, pc: &mut PaintCtx<'_>, rect: Rect, y: u16, hunk: usize, selected: bool) {
		let document = self.document.as_ref().expect("document");
		let body = Self::body_width(rect.width);
		let style = Style::new().fg(pc.ctx.theme.accent).bg(
			selected
				.then_some(pc.ctx.theme.surface)
				.unwrap_or(Color::Default),
		);
		pc.frame.fill(Rect::new(rect.x, y, body, 1), style);
		pc.frame.put_clipped(
			rect.x,
			y,
			body,
			&document.hunks[hunk].header,
			if hunk == self.selected_hunk {
				style.bold()
			} else {
				style
			},
		);
		if self.patch_target.is_none() {
			return;
		}
		let (primary_label, primary_action, primary_color) = match self.patch_target {
			Some(DiffPatchTarget::Stage) => (" Stage Hunk ", DiffActionKind::Stage, pc.ctx.theme.ok),
			Some(DiffPatchTarget::Unstage) => {
				(" Unstage Hunk ", DiffActionKind::Unstage, pc.ctx.theme.warn)
			},
			None => return,
		};
		let caps = pc.ctx.charset.pill_caps();
		let primary_width = u16::try_from(primary_label.visible_width())
			.unwrap_or(u16::MAX)
			.saturating_add(
				u16::try_from(caps.0.visible_width() + caps.1.visible_width()).unwrap_or(0),
			);
		let discard_width = if self.patch_target == Some(DiffPatchTarget::Stage) {
			u16::try_from(" Discard Hunk ".visible_width())
				.unwrap_or(u16::MAX)
				.saturating_add(
					u16::try_from(caps.0.visible_width() + caps.1.visible_width()).unwrap_or(0),
				)
		} else {
			0
		};
		let total = primary_width
			.saturating_add(discard_width)
			.saturating_add(u16::from(discard_width > 0));
		if total > body {
			return;
		}
		let mut x = rect.x.saturating_add(body - total);
		if discard_width > 0 {
			let start = x;
			x = pill(
				pc.frame,
				x,
				y,
				" Discard Hunk ",
				pc.ctx.theme.err,
				pc.ctx.theme.contrast,
				caps,
				selected,
			);
			pc.hits.push(Hit {
				rect: Rect::new(start, y, x - start, 1),
				slot: self.slot,
				tag:  HitTag::DiffHunkDiscard(hunk as u32),
			});
			x = x.saturating_add(1);
		}
		let start = x;
		let end =
			pill(pc.frame, x, y, primary_label, primary_color, pc.ctx.theme.contrast, caps, selected);
		pc.hits.push(Hit {
			rect: Rect::new(start, y, end - start, 1),
			slot: self.slot,
			tag:  HitTag::DiffHunkPrimary(hunk as u32),
		});
		let _ = primary_action;
	}

	fn band_kind(&self, band: usize, bands: usize) -> Option<MapKind> {
		let total = self.layout.visuals.len();
		if total == 0 || bands == 0 {
			return None;
		}
		let from = band.saturating_mul(total) / bands;
		if from >= total {
			return None;
		}
		let to = ((band + 1).saturating_mul(total) / bands)
			.max(from + 1)
			.min(total);
		let mut best = MapKind::Context;
		for visual in &self.layout.visuals[from..to] {
			match self.visual_kind(*visual) {
				Some(MapKind::Del) => return Some(MapKind::Del),
				Some(MapKind::Add) => best = MapKind::Add,
				Some(MapKind::Change) if best != MapKind::Add => best = MapKind::Change,
				Some(MapKind::Hunk) if best == MapKind::Context => best = MapKind::Hunk,
				_ => {},
			}
		}
		Some(best)
	}

	fn paint_minimap(&self, pc: &mut PaintCtx<'_>, rect: Rect) {
		if rect.width == 0 || rect.height == 0 {
			return;
		}
		let x = rect.x.saturating_add(rect.width - 1);
		let bands = usize::from(rect.height).saturating_mul(2);
		let total = self.layout.visuals.len();
		for row in 0..rect.height {
			let top_band = usize::from(row) * 2;
			let bottom_band = top_band + 1;
			let color = |kind: Option<MapKind>, band: usize| {
				let base = match kind? {
					MapKind::Del => pc.ctx.theme.err,
					MapKind::Add => pc.ctx.theme.ok,
					MapKind::Change => pc.ctx.theme.err.mix(pc.ctx.theme.ok, 0.5),
					MapKind::Hunk => pc.ctx.theme.accent,
					MapKind::Context => pc.ctx.theme.panel.mix(pc.ctx.theme.fg, 0.2),
				};
				let visual = band.saturating_mul(total) / bands.max(1);
				Some(
					if visual >= self.scroll_top
						&& visual < self.scroll_top.saturating_add(usize::from(rect.height))
					{
						base.mix(pc.ctx.theme.fg, 0.25)
					} else {
						base
					},
				)
			};
			let top = color(self.band_kind(top_band, bands), top_band);
			let bottom = color(self.band_kind(bottom_band, bands), bottom_band);
			if top.is_none() && bottom.is_none() {
				continue;
			}
			let glyph = pc.ctx.charset.shadow().unwrap_or("|");
			pc.frame.put(
				x,
				rect.y + row,
				glyph,
				Style::new()
					.fg(top.or(bottom).unwrap_or(pc.ctx.theme.muted))
					.bg(bottom.unwrap_or(Color::Default)),
			);
		}
		pc.hits.push(Hit {
			rect: Rect::new(x, rect.y, 1, rect.height),
			slot: self.slot,
			tag:  HitTag::DiffMinimap,
		});
	}
}

impl Default for DiffPane {
	fn default() -> Self {
		Self::new()
	}
}

impl Component for DiffPane {
	fn props(&self) -> &Props {
		&self.props
	}

	fn props_mut(&mut self) -> &mut Props {
		&mut self.props
	}

	fn slot(&self) -> Slot {
		self.slot
	}

	fn measure(&mut self, _ctx: &UiContext) -> (u16, u16) {
		(20, u16::MAX)
	}

	fn height(&mut self, _ctx: &UiContext, _width: u16) -> u16 {
		12
	}

	fn paint(&mut self, pc: &mut PaintCtx<'_>, rect: Rect) {
		self.last_width = rect.width;
		self.last_height = rect.height.max(1);
		self.rebuild_layout(rect.width);
		pc.hits
			.push(Hit { rect, slot: self.slot, tag: HitTag::Wheel });
		if self.state != DiffPaneState::Ready || self.document.is_none() {
			self.paint_placeholder(pc, rect);
			return;
		}
		self.clamp_scroll();
		let palette = Palette::new(pc.ctx);
		let select_from = self
			.anchor
			.map_or(self.cursor, |anchor| anchor.min(self.cursor));
		let select_to = self
			.anchor
			.map_or(self.cursor, |anchor| anchor.max(self.cursor));
		for screen_row in 0..rect.height {
			let visual_index = self.scroll_top.saturating_add(usize::from(screen_row));
			let Some(visual) = self.layout.visuals.get(visual_index).copied() else {
				continue;
			};
			let y = rect.y.saturating_add(screen_row);
			let selected =
				pc.focus == Some(self.slot) && visual_index >= select_from && visual_index <= select_to;
			self.paint_visual(pc, rect, y, visual, selected, palette);
			if !matches!(visual, Visual::Header { .. }) {
				pc.hits.push(Hit {
					rect: Rect::new(rect.x, y, Self::body_width(rect.width), 1),
					slot: self.slot,
					tag:  HitTag::DiffRow(visual_index as u32),
				});
			}
		}
		self.paint_minimap(pc, rect);
	}

	fn focusable(&self) -> bool {
		true
	}

	fn key(&mut self, ec: &mut EventCtx<'_>, key: Key) -> Flow {
		self.last_width = ec.width;
		self.last_height = ec.view_rows.max(1);
		self.rebuild_layout(ec.width);
		let page = i32::from(ec.view_rows.saturating_sub(2).max(1));
		let consumed = match key {
			Key::Up => self.move_cursor(-1, false),
			Key::Down => self.move_cursor(1, false),
			Key::SelectUp => self.move_cursor(-1, true),
			Key::SelectDown => self.move_cursor(1, true),
			Key::PageUp => self.move_cursor(-page, false),
			Key::PageDown => self.move_cursor(page, false),
			Key::Home => self.cursor_edge(false, false),
			Key::End => self.cursor_edge(true, false),
			Key::SelectHome => self.cursor_edge(false, true),
			Key::SelectEnd => self.cursor_edge(true, true),
			Key::Left if !self.wrap => {
				let before = self.scroll_left;
				self.scroll_left = self.scroll_left.saturating_sub(8);
				self.scroll_left != before
			},
			Key::Right if !self.wrap => {
				let before = self.scroll_left;
				self.scroll_left = self.scroll_left.saturating_add(8);
				self.clamp_scroll();
				self.scroll_left != before
			},
			_ => return Flow::Skip,
		};
		if consumed { Flow::Consumed } else { Flow::Skip }
	}

	fn mouse(
		&mut self,
		ec: &mut EventCtx<'_>,
		tag: HitTag,
		at: (u16, u16),
		rect: Rect,
		mouse: Mouse,
	) -> Flow {
		self.last_width = ec.width;
		self.last_height = ec.view_rows.max(1);
		match (tag, mouse) {
			(HitTag::Wheel, Mouse::WheelUp | Mouse::WheelDown) => {
				let delta = if mouse == Mouse::WheelUp { -3 } else { 3 };
				if self.scroll_by(delta) {
					Flow::Consumed
				} else {
					Flow::Skip
				}
			},
			(HitTag::DiffRow(index), Mouse::Click) => {
				let index = index as usize;
				if ec.mods.shift {
					self.anchor.get_or_insert(self.cursor);
				} else {
					self.anchor = None;
				}
				self.cursor = index.min(self.layout.visuals.len().saturating_sub(1));
				Flow::Consumed
			},
			(HitTag::DiffMinimap, Mouse::Click | Mouse::Drag) => {
				if rect.height > 0 && !self.layout.visuals.is_empty() {
					let row = at.1.saturating_sub(rect.y).min(rect.height - 1);
					let target =
						(usize::from(row) * self.layout.visuals.len()) / usize::from(rect.height);
					self.scroll_top = target.saturating_sub(usize::from(self.last_height / 2));
					self.clamp_scroll();
				}
				Flow::Consumed
			},
			(HitTag::DiffHunkPrimary(hunk), Mouse::Click) => {
				self.selected_hunk = hunk as usize;
				let action = match self.patch_target {
					Some(DiffPatchTarget::Stage) => DiffActionKind::Stage,
					Some(DiffPatchTarget::Unstage) => DiffActionKind::Unstage,
					None => return Flow::Consumed,
				};
				self
					.request_action(action)
					.map_or(Flow::Consumed, Flow::Event)
			},
			(HitTag::DiffHunkDiscard(hunk), Mouse::Click) => {
				self.selected_hunk = hunk as usize;
				self
					.request_action(DiffActionKind::Discard)
					.map_or(Flow::Consumed, Flow::Event)
			},
			_ => Flow::Skip,
		}
	}
}

fn row_map_kind(kind: DiffRowKind) -> MapKind {
	match kind {
		DiffRowKind::Context => MapKind::Context,
		DiffRowKind::Change => MapKind::Change,
		DiffRowKind::Add => MapKind::Add,
		DiffRowKind::Del => MapKind::Del,
	}
}

fn segments(width: u16, content: u16, wrap: bool) -> u16 {
	if !wrap {
		1
	} else {
		content
			.max(1)
			.saturating_add(width - 1)
			.checked_div(width)
			.unwrap_or(1)
			.max(1)
	}
}

fn visual_row(visual: Visual) -> Option<usize> {
	match visual {
		Visual::Split { row, .. } | Visual::Line { row, .. } => Some(row),
		Visual::File { row, .. } => row,
		Visual::Header { .. } | Visual::Blank => None,
	}
}

fn semantic_style(style: Style, theme: &Theme) -> Style {
	let default = Theme::default();
	let source = style.foreground_color();
	let foreground = if source == default.fg {
		theme.fg
	} else if source == default.muted {
		theme.muted
	} else if source == default.accent {
		theme.accent
	} else if source == default.info {
		theme.info
	} else if source == default.ok {
		theme.ok
	} else if source == default.warn {
		theme.warn
	} else if source == default.err {
		theme.err
	} else {
		source
	};
	style.fg(foreground)
}

#[allow(clippy::too_many_arguments, reason = "cached styled source slice paint")]
fn paint_source(
	pc: &mut PaintCtx<'_>,
	x: u16,
	y: u16,
	width: u16,
	text: &str,
	styles: &[DiffStyleRun],
	marks: &[DiffMark],
	start: u16,
	background: Color,
	strong: Color,
) {
	pc.frame
		.fill(Rect::new(x, y, width, 1), Style::new().bg(background));
	let end = start.saturating_add(width);
	let mut column = 0u16;
	let mut output = x;
	for grapheme in text.graphemes() {
		let grapheme_width = u16::try_from(grapheme.visible_width()).unwrap_or(u16::MAX);
		let next = column.saturating_add(grapheme_width);
		if next <= start {
			column = next;
			continue;
		}
		if column >= end || grapheme_width > end.saturating_sub(column.max(start)) {
			break;
		}
		let mut style = styles
			.iter()
			.find(|run| column >= run.start && column < run.end)
			.map_or_else(
				|| Style::new().fg(pc.ctx.theme.fg),
				|run| semantic_style(run.style, &pc.ctx.theme),
			);
		let marked = marks
			.iter()
			.any(|mark| column < mark.end && next > mark.start);
		style = style.bg(if marked { strong } else { background });
		output = pc.frame.put_clipped(
			output,
			y,
			x.saturating_add(width).saturating_sub(output),
			grapheme,
			style,
		);
		column = next;
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		Frame, Size,
		component::PaintCtx,
		test_support::{frame_cell_style, frame_row_text},
	};

	fn document() -> DiffDocument {
		DiffDocument::build(
			"one\ntwo old\nthree\nfour\n",
			"one\ntwo new\nthree\nfour added\n",
			"sample.rs",
			&Default::default(),
		)
	}

	fn paint(pane: &mut DiffPane, width: u16, height: u16) -> (Frame, Vec<Hit>) {
		let ctx = UiContext::default();
		let mut frame = Frame::new(Size::new(width, height));
		let mut hits = Vec::new();
		let mut wakes = Vec::new();
		let mut pc = PaintCtx::new(&mut frame, &ctx, &mut hits, &mut wakes);
		pc.focus = Some(pane.slot());
		pane.paint(&mut pc, Rect::new(0, 0, width, height));
		(frame, hits)
	}

	#[test]
	fn modes_build_distinct_layouts() {
		let mut pane = DiffPane::new();
		pane.set_document(Some(document()), DiffPaneState::Ready);
		let split = paint(&mut pane, 60, 8).0;
		assert!(frame_row_text(&split, 0).contains('│'));
		pane.set_mode(ViewMode::Inline);
		let inline = paint(&mut pane, 60, 8).0;
		assert!(frame_row_text(&inline, 1).contains("two old"));
		assert!(frame_row_text(&inline, 2).contains("two new"));
		pane.set_mode(ViewMode::Hunk);
		let hunk = paint(&mut pane, 60, 8).0;
		assert!(frame_row_text(&hunk, 0).contains("@@ -"));
		pane.set_mode(ViewMode::File);
		let file = paint(&mut pane, 60, 8).0;
		assert!(frame_row_text(&file, 1).contains("two new"));
	}

	#[test]
	fn wrap_segments_long_rows() {
		let doc = DiffDocument::build("abcdefghij", "ABCDEFGHIJ", "x.txt", &Default::default());
		let mut pane = DiffPane::new();
		pane.set_document(Some(doc), DiffPaneState::Ready);
		paint(&mut pane, 24, 5);
		let unwrapped = pane.layout.visuals.len();
		pane.toggle_wrap();
		paint(&mut pane, 24, 5);
		assert!(pane.layout.visuals.len() > unwrapped);
	}

	#[test]
	fn minimap_prioritizes_deletions() {
		let mut pane = DiffPane::new();
		pane.set_document(
			Some(DiffDocument::build("a\nremoved\nb", "a\nb", "x.txt", &Default::default())),
			DiffPaneState::Ready,
		);
		paint(&mut pane, 40, 1);
		assert_eq!(pane.band_kind(0, 1), Some(MapKind::Del));
	}

	#[test]
	fn selection_maps_to_source_ranges() {
		let mut pane = DiffPane::new();
		pane.set_document(Some(document()), DiffPaneState::Ready);
		paint(&mut pane, 60, 8);
		pane.cursor = 1;
		pane.anchor = Some(1);
		assert_eq!(
			pane.selection(),
			Some(DiffSelection { old: (2, 2), new: (2, 2), explicit: true })
		);
	}

	#[test]
	fn request_action_uses_selection_hunk_file_precedence() {
		let mut pane = DiffPane::new().with(Prop::Id, "diff");
		pane.set_patch_target(Some(DiffPatchTarget::Stage));
		pane.set_document(Some(document()), DiffPaneState::Ready);
		paint(&mut pane, 60, 8);
		assert!(matches!(
			pane.request_action(DiffActionKind::Stage),
			Some(UiEvent::DiffAction { target: DiffTarget::File, .. })
		));
		pane.set_mode(ViewMode::Hunk);
		paint(&mut pane, 60, 8);
		assert!(matches!(
			pane.request_action(DiffActionKind::Stage),
			Some(UiEvent::DiffAction { target: DiffTarget::Hunk(0), .. })
		));
		pane.cursor = 1;
		pane.anchor = Some(1);
		assert!(matches!(
			pane.request_action(DiffActionKind::Stage),
			Some(UiEvent::DiffAction { target: DiffTarget::Lines { .. }, .. })
		));
	}

	#[test]
	fn hunk_button_click_emits_action() {
		let mut pane = DiffPane::new().with(Prop::Id, "diff");
		pane.set_mode(ViewMode::Hunk);
		pane.set_patch_target(Some(DiffPatchTarget::Stage));
		pane.set_document(Some(document()), DiffPaneState::Ready);
		let (_, hits) = paint(&mut pane, 80, 8);
		let hit = hits
			.iter()
			.find(|hit| matches!(hit.tag, HitTag::DiffHunkPrimary(0)))
			.unwrap();
		let ctx = UiContext::default();
		let mut ec = EventCtx::new(&ctx, 80, 8);
		let flow = pane.mouse(&mut ec, hit.tag, (hit.rect.x, hit.rect.y), hit.rect, Mouse::Click);
		assert!(matches!(
			flow,
			Flow::Event(UiEvent::DiffAction {
				action: DiffActionKind::Stage,
				target: DiffTarget::Hunk(0),
				..
			})
		));
	}

	#[test]
	fn placeholder_states_center_messages() {
		for (state, expected) in [
			(DiffPaneState::Empty, "No changes"),
			(DiffPaneState::Loading, "Loading diff…"),
			(DiffPaneState::Binary, "Binary file"),
			(DiffPaneState::TooLarge, "File too large to diff"),
		] {
			let mut pane = DiffPane::new();
			pane.set_document(None, state);
			let frame = paint(&mut pane, 40, 3).0;
			assert!(frame_row_text(&frame, 1).contains(expected));
		}
	}

	#[test]
	fn intraline_background_is_stronger() {
		let mut pane = DiffPane::new();
		pane.set_document(Some(document()), DiffPaneState::Ready);
		let frame = paint(&mut pane, 60, 8).0;
		let gutter = pane.document.as_ref().unwrap().gutter_width;
		let text_width = pane.split_text_width(60);
		let old_text_x = gutter + 1;
		let base = frame_cell_style(&frame, old_text_x, 1).background_color();
		let mark = frame_cell_style(&frame, old_text_x + 4, 1).background_color();
		assert_ne!(base, mark);
		assert!(text_width > 0);
	}
	#[test]
	fn syntax_runs_follow_the_active_theme() {
		let document =
			DiffDocument::build("fn main() {}", "fn main() {}", "x.rs", &Default::default());
		let mut pane = DiffPane::new();
		pane.set_document(Some(document), DiffPaneState::Ready);
		let mut ctx = UiContext::default();
		ctx.theme.accent = Color::Rgb(1, 2, 3);
		let mut frame = Frame::new(Size::new(40, 2));
		let mut hits = Vec::new();
		let mut wakes = Vec::new();
		let mut pc = PaintCtx::new(&mut frame, &ctx, &mut hits, &mut wakes);
		pane.paint(&mut pc, Rect::new(0, 0, 40, 2));
		let text_x = pane.document.as_ref().unwrap().gutter_width + 1;
		assert_eq!(frame_cell_style(&frame, text_x, 0).foreground_color(), ctx.theme.accent);
	}
}
