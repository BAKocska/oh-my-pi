//! Retained presentation for the fullscreen Git workbench.

mod commit_view;
mod diff;
mod sidebar;

use std::{
	collections::BTreeSet,
	time::{Duration, Instant},
};

use diff::{DIFF_ID, VIEW_ID};
use omp_core::{IntoStr, Str, sf};
use strum::EnumProperty as _;
use omp_tui::{
	DiffActionKind, DiffBuildOptions, DiffDocument, DiffPane, DiffPaneState, DiffPatchTarget,
	DiffTarget, DiffWhitespaceMode, Dim, Key, Layer, Mouse, OverlayOptions, Prop, Size, Ui,
	UiContext, UiEvent, ViewMode, cell_width,
	components::{Col, EditorPane},
};
use sidebar::{
	AMEND_ID, COMMIT_ID, DESCRIPTION_ID, DESCRIPTION_PANE_ID, SUMMARY_ID, SidebarRow,
	SidebarTarget, VIEW_STYLE_ID, directory_key, sidebar_rows,
};

/// Kind of change reported for one Git path.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::IntoStaticStr)]
pub enum GitChangeKind {
	/// Existing file contents changed.
	#[strum(to_string = "M")]
	Modified,
	/// New tracked file.
	#[strum(to_string = "A")]
	Added,
	/// Removed tracked file.
	#[strum(to_string = "D")]
	Deleted,
	/// Path renamed from [`GitFileRow::orig_path`].
	#[strum(to_string = "R")]
	Renamed,
	/// New untracked file.
	#[strum(to_string = "?")]
	Untracked,
	/// File with unresolved conflicts.
	#[strum(to_string = "U")]
	Conflicted,
}

/// Repository area containing a Git file row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitArea {
	/// Working-tree changes not present in the index.
	Unstaged,
	/// Changes present in the index.
	Staged,
	/// Changes belonging to the pinned commit.
	Commit,
}

/// One changed file shown by the Git workbench.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitFileRow {
	/// Current repository-relative path.
	pub path:      Str,
	/// Previous path for a rename.
	pub orig_path: Option<Str>,
	/// Kind of file change.
	pub kind:      GitChangeKind,
	/// Repository area containing the change.
	pub area:      GitArea,
	/// Added line count, when available.
	pub additions: Option<u64>,
	/// Deleted line count, when available.
	pub deletions: Option<u64>,
}

/// Metadata and file changes for one pinned commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitCommitInfo {
	/// Full commit object id.
	pub sha:          Str,
	/// First line of the commit message.
	pub subject:      Str,
	/// Remaining commit message body.
	pub body:         Str,
	/// Commit author's display name.
	pub author_name:  Str,
	/// Commit author's email address.
	pub author_email: Str,
	/// Commit author's strict ISO-8601 date.
	pub author_date:  Str,
	/// Full parent commit object ids.
	pub parents:      Vec<Str>,
	/// Files changed by this commit.
	pub files:        Vec<GitFileRow>,
}

/// Complete backend-owned repository snapshot for the workbench.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitSnapshot {
	/// Current branch name, or `None` for detached/unborn HEAD.
	pub branch:   Option<Str>,
	/// Working-tree changes not present in the index.
	pub unstaged: Vec<GitFileRow>,
	/// Changes present in the index.
	pub staged:   Vec<GitFileRow>,
	/// Current or pinned commit metadata, when available.
	pub head:     Option<GitCommitInfo>,
	/// Whether the workbench is pinned to a revision.
	pub pinned:   bool,
}

/// Old and new file content loaded for the diff pane.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitFileContents {
	/// File text on the old side.
	pub old_text:  Str,
	/// File text on the new side.
	pub new_text:  Str,
	/// Whether Git reported binary contents.
	pub binary:    bool,
	/// Whether the file exceeded the presentation size limit.
	pub too_large: bool,
	/// Raw old-side bytes when media preview applies.
	pub old_bytes: Option<bytes::Bytes>,
	/// Raw new-side bytes when media preview applies.
	pub new_bytes: Option<bytes::Bytes>,
	/// Lowercase media format token when the file is a previewable image.
	pub media:     Option<Str>,
}

/// Patch mutation requested from an interactive diff selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitPatchOp {
	/// Add selected changes to the index.
	Stage,
	/// Remove selected changes from the index.
	Unstage,
	/// Discard selected working-tree changes.
	Discard,
}

/// Outbound workbench request for the Git backend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitIntent {
	/// Refresh the repository snapshot.
	Refresh,
	/// Load both sides of one file into the diff pane.
	Load {
		/// Repository area containing the file.
		area:      GitArea,
		/// Current repository-relative path.
		path:      Str,
		/// Previous path for a rename.
		orig_path: Option<Str>,
		/// Monotonic request sequence used to reject stale contents.
		seq:       u64,
	},
	/// Stage one path, or every unstaged path when absent.
	StageFile(Option<Str>),
	/// Unstage one path, or every staged path when absent.
	UnstageFile(Option<Str>),
	/// Apply an operation to inclusive one-based line ranges.
	ApplyLines {
		/// Requested patch operation.
		op:   GitPatchOp,
		/// Current repository-relative path.
		path: Str,
		/// Inclusive old-side range, or `(0, 0)` when absent.
		old:  (u32, u32),
		/// Inclusive new-side range, or `(0, 0)` when absent.
		new:  (u32, u32),
	},
	/// Create a commit from the composer.
	Commit {
		/// Subject and optional body entered by the user.
		message:   Str,
		/// Whether to amend HEAD.
		amend:     bool,
		/// Whether to stage all working-tree changes first.
		stage_all: bool,
	},
	/// Resolve an avatar image for an author email.
	Avatar {
		/// Lower- or mixed-case author email.
		email: Str,
	},
	/// Close the workbench and release backend refresh state.
	Close,
}

/// Inbound workbench mutation emitted by the Git backend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitUpdate {
	/// Replace repository and commit state.
	Snapshot(GitSnapshot),
	/// Supply loaded file contents for one request sequence.
	Contents {
		/// Sequence from the corresponding [`GitIntent::Load`].
		seq:      u64,
		/// Loaded old and new file contents.
		contents: GitFileContents,
	},
	/// Report a successful mutation.
	ActionDone {
		/// Human-readable success message.
		message: Str,
	},
	/// Report a failed mutation.
	ActionFailed {
		/// Human-readable failure message.
		message: Str,
	},
	/// Supply an optional author avatar PNG.
	Avatar {
		/// Author email associated with the result.
		email: Str,
		/// Normalized PNG bytes, or `None` when unavailable.
		png:   Option<bytes::Bytes>,
	},
}

const STATUS_TTL: Duration = Duration::from_secs(6);
const SIDEBAR_MIN: u16 = 30;
const SIDEBAR_MAX: u16 = 48;
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, strum::EnumProperty)]
pub(super) enum Focus {
	#[strum(props(Hint = "alt+↓/↑ hunk · ]/[ file · shift+↑/↓ select · s/u stage · x discard · v view · c commit · q quit"))]
	Diff,
	#[default]
	#[strum(props(Hint = "↑/↓ move · ←/→ fold · space stage · enter open · alt+↓/↑ hunk · c commit · t tree · q quit"))]
	Sidebar,
}

impl Focus {
	fn hint(self) -> &'static str {
		self.get_str("Hint").expect("every focus has a hint")
	}
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingDiscard {
	path:   Str,
	target: DiffTarget,
}

/// Result of routing one interaction through a [`GitWorkbench`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitWorkbenchEvent {
	/// Input was consumed without a backend request.
	Consumed,
	/// Forward one request to the Git backend.
	Intent(GitIntent),
	/// Close the workbench and stop backend refresh.
	Close,
}

/// Retained fullscreen Git workbench presentation.
pub struct GitWorkbench {
	pub(super) ui: Ui,
	pub(super) ctx: UiContext,
	options: OverlayOptions,
	pub(super) snapshot: GitSnapshot,
	pub(super) selected: Option<(GitArea, Str)>,
	pub(in crate::git) sidebar_rows: Vec<SidebarRow>,
	pub(super) sidebar_selected: usize,
	pub(super) sidebar_scroll_top: usize,
	pub(super) focus: Focus,
	pub(super) tree: bool,
	pub(super) collapsed: BTreeSet<Str>,
	pub(super) contents: Option<GitFileContents>,
	load_seq: u64,
	pub(super) whitespace: DiffWhitespaceMode,
	pub(super) view_mode: ViewMode,
	pub(super) wrap: bool,
	pub(super) amend: bool,
	pub(super) status: Option<(Str, omp_tui::Color, Instant)>,
	pending_discard: Option<PendingDiscard>,
	commit_pending: bool,
	pub(super) avatar: Option<(Str, bytes::Bytes)>,
	avatar_requested: Option<Str>,
	pending_last_hunk: bool,
	width: u16,
	height: u16,
}

impl GitWorkbench {
	/// Opens a workbench over a backend-owned repository snapshot.
	pub fn open(snapshot: GitSnapshot, ctx: &UiContext) -> Self {
		let selected = first_file(&snapshot).map(|file| (file.area, file.path.clone()));
		let mut workbench = Self {
			ui: Ui::from_root(Col::new(), 1, ctx.clone()),
			ctx: ctx.clone(),
			options: OverlayOptions::default().width(Dim::Pct(100)).z(40),
			snapshot,
			selected,
			sidebar_rows: Vec::new(),
			sidebar_selected: 0,
			sidebar_scroll_top: 0,
			focus: Focus::Sidebar,
			tree: true,
			collapsed: BTreeSet::new(),
			contents: None,
			load_seq: 0,
			whitespace: DiffWhitespaceMode::Off,
			view_mode: ViewMode::Split,
			wrap: false,
			amend: false,
			status: None,
			pending_discard: None,
			commit_pending: false,
			avatar: None,
			avatar_requested: None,
			pending_last_hunk: false,
			width: 100,
			height: 30,
		};
		workbench.rebuild();
		workbench
	}

	/// Returns the load request for the initially selected file, when present.
	pub fn initial_intent(&mut self) -> Option<GitIntent> {
		self
			.request_selected_load()
			.or_else(|| self.request_avatar())
	}

	/// Applies one backend update and returns a load needed by changed
	/// selection.
	pub fn apply(&mut self, update: GitUpdate) -> Option<GitIntent> {
		match update {
			GitUpdate::Snapshot(snapshot) => self.apply_snapshot(snapshot),
			GitUpdate::Contents { seq, contents } => {
				if seq != self.load_seq {
					return None;
				}
				self.contents = Some(contents);
				self.install_document();
				self.request_avatar()
			},
			GitUpdate::ActionDone { message } => {
				let clear_form = self.commit_pending;
				if clear_form {
					self.amend = false;
					self.commit_pending = false;
					self.sidebar_selected = self.first_file_target().unwrap_or(0);
				}
				self.status = Some((message, self.ctx.theme.ok, Instant::now()));
				self.pending_discard = None;
				if clear_form {
					self.rebuild_with_form("", "");
				} else {
					self.rebuild();
				}
				None
			},
			GitUpdate::ActionFailed { message } => {
				self.commit_pending = false;
				self.status = Some((message, self.ctx.theme.err, Instant::now()));
				self.pending_discard = None;
				self.rebuild();
				None
			},
			GitUpdate::Avatar { email, png } => {
				self.avatar_requested = Some(email.clone());
				if let Some(png) = png {
					self.avatar = Some((email, png));
				}
				self.rebuild();
				None
			},
		}
	}

	/// Routes one keyboard event.
	pub fn handle_key(&mut self, key: Key) -> GitWorkbenchEvent {
		if key != Key::Char('x') {
			self.pending_discard = None;
		}
		if matches!(key, Key::Tab | Key::BackTab) {
			self.focus = if self.focus == Focus::Diff {
				Focus::Sidebar
			} else {
				Focus::Diff
			};
			self.focus_current();
			return GitWorkbenchEvent::Consumed;
		}
		if key == Key::Esc {
			if self.focus == Focus::Sidebar && self.editing() {
				self.select_target_kind(SidebarTarget::Commit);
				return GitWorkbenchEvent::Consumed;
			}
			if self.focus == Focus::Diff && self.clear_diff_selection() {
				return GitWorkbenchEvent::Consumed;
			}
			return GitWorkbenchEvent::Close;
		}
		if !self.editing() {
			match key {
				Key::Char('q') => return GitWorkbenchEvent::Close,
				Key::JumpPrevious => return self.jump_hunk_or_file(-1),
				Key::JumpNext => return self.jump_hunk_or_file(1),
				Key::Char('[') => return self.select_adjacent_file(-1, false),
				Key::Char(']') => return self.select_adjacent_file(1, false),
				Key::Char('v') => {
					self.with_pane(|pane| pane.cycle_mode());
					self.view_mode = match self.view_mode {
						ViewMode::File => ViewMode::Split,
						ViewMode::Split => ViewMode::Inline,
						ViewMode::Inline => ViewMode::Hunk,
						ViewMode::Hunk => ViewMode::File,
					};
					self.sync_view_value();
					return GitWorkbenchEvent::Consumed;
				},
				Key::Char('1') => return self.set_mode(ViewMode::File),
				Key::Char('2') => return self.set_mode(ViewMode::Split),
				Key::Char('3') => return self.set_mode(ViewMode::Inline),
				Key::Char('4') => return self.set_mode(ViewMode::Hunk),
				Key::Char('w') => {
					self.with_pane(|pane| pane.toggle_wrap());
					self.wrap = !self.wrap;
					self.sync_toggle_props();
					return GitWorkbenchEvent::Consumed;
				},
				Key::Char('b') => return self.cycle_whitespace(),
				Key::Char('r') => return GitWorkbenchEvent::Intent(GitIntent::Refresh),
				Key::Char('c') if !self.is_commit_view() => {
					self.focus = Focus::Sidebar;
					self.select_target_kind(SidebarTarget::Summary);
					return GitWorkbenchEvent::Consumed;
				},
				_ => {},
			}
		}
		match self.focus {
			Focus::Diff => self.handle_diff_key(key),
			Focus::Sidebar => self.handle_sidebar_key(key),
		}
	}

	/// Routes pasted text into the active commit text field.
	pub fn handle_paste(&mut self, text: &str) -> GitWorkbenchEvent {
		if !self.editing() {
			return GitWorkbenchEvent::Consumed;
		}
		let _ = self.ui.handle_paste(text);
		self.sync_commit_button();
		GitWorkbenchEvent::Consumed
	}

	/// Routes a viewport-space mouse gesture through the fullscreen retained UI.
	pub fn handle_mouse(
		&mut self,
		col: u16,
		row: u16,
		kind: Mouse,
		viewport: Size,
	) -> GitWorkbenchEvent {
		let sidebar_width = (viewport.width * 3 / 10).clamp(SIDEBAR_MIN, SIDEBAR_MAX);
		let in_content = row >= 2;
		let in_sidebar =
			in_content && col >= viewport.width.saturating_sub(sidebar_width);
		if in_sidebar && matches!(kind, Mouse::WheelUp | Mouse::WheelDown) {
			let delta = if kind == Mouse::WheelUp { -3 } else { 3 };
			self.pending_discard = None;
			self.scroll_sidebar(delta);
			return GitWorkbenchEvent::Consumed;
		}
		if in_content && matches!(kind, Mouse::Click | Mouse::RightClick) {
			self.focus = if in_sidebar { Focus::Sidebar } else { Focus::Diff };
			if self.focus == Focus::Sidebar && kind == Mouse::Click {
				self.select_sidebar_form_at(row.saturating_sub(2), viewport.height.saturating_sub(2));
			}
			self.focus_current();
		}
		let routed = self
			.ui
			.handle_mouse_as_layer(&self.options, viewport, col, row, kind)
			.unwrap_or(UiEvent::None);
		self.sync_control_values();
		if !matches!(routed, UiEvent::DiffAction { action: DiffActionKind::Discard, .. }) {
			self.pending_discard = None;
		}
		self.route_ui(routed)
	}

	/// Returns the full-viewport active layer.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		if self
			.status
			.as_ref()
			.is_some_and(|(_, _, at)| at.elapsed() >= STATUS_TTL)
		{
			self.status = None;
			let hint = self.focus.hint();
			let _ = self.ui.set_text("git-status", hint);
			let _ = self
				.ui
				.set_prop("git-status", Prop::Fg, self.ctx.theme.muted);
		}
		if viewport.width != self.width || viewport.height != self.height {
			self.width = viewport.width;
			self.height = viewport.height;
			self.rebuild();
		}
		Layer { frame: self.ui.frame(), options: &self.options, active: true }
	}

	fn apply_snapshot(&mut self, snapshot: GitSnapshot) -> Option<GitIntent> {
		self.pending_discard = None;
		let previous_rows = self.sidebar_rows.clone();
		let previous_target = self.current_sidebar_target().cloned();
		let previous_selected = self.selected.clone();
		self.snapshot = snapshot;
		self.sidebar_rows = sidebar_rows(&self.snapshot, self.tree, &self.collapsed, &self.ctx);
		if let Some(target) = previous_target {
			let key = target.key();
			if let Some(index) = self
				.sidebar_rows
				.iter()
				.position(|row| row.target.key() == key)
			{
				self.sidebar_selected = index;
			} else if let Some(index) = nearest_survivor(&previous_rows, &self.sidebar_rows, &key) {
				self.sidebar_selected = index;
			}
		}
		self.selected = previous_selected
			.clone()
			.filter(|(area, path)| find_file(&self.snapshot, *area, path.as_str()).is_some());
		if self.selected.is_none() {
			self.selected = self
				.current_sidebar_target()
				.and_then(|target| match target {
					SidebarTarget::File { area, path, .. } => Some((*area, path.clone())),
					_ => None,
				})
				.or_else(|| first_file(&self.snapshot).map(|file| (file.area, file.path.clone())));
		}
		let changed = self.selected != previous_selected;
		if changed {
			self.contents = None;
			self.install_document();
		}
		self.rebuild();
		if changed {
			self
				.request_selected_load()
				.or_else(|| self.request_avatar())
		} else {
			self.request_avatar()
		}
	}

	fn handle_diff_key(&mut self, key: Key) -> GitWorkbenchEvent {
		match key {
			Key::Char('s') => self.request_diff_action(DiffActionKind::Stage),
			Key::Char('u') => self.request_diff_action(DiffActionKind::Unstage),
			Key::Char('x') => self.request_discard(),
			Key::Enter => self.jump_hunk_or_file(1),
			Key::Char('n') => self.jump_hunk_or_file(1),
			Key::Char('p') => self.jump_hunk_or_file(-1),
			Key::Char('j') => self.route_diff_navigation(Key::Down),
			Key::Char('k') => self.route_diff_navigation(Key::Up),
			Key::Char('h') => self.route_diff_navigation(Key::Left),
			Key::Char('l') => self.route_diff_navigation(Key::Right),
			Key::Char('g') => self.route_diff_navigation(Key::Home),
			Key::Char('G') => self.route_diff_navigation(Key::End),
			Key::Space => self.route_diff_navigation(Key::PageDown),
			_ => self.route_diff_navigation(key),
		}
	}

	fn route_diff_navigation(&mut self, key: Key) -> GitWorkbenchEvent {
		self.focus_current();
		let event = self.ui.handle_key(key);
		self.sync_control_values();
		self.route_ui(event)
	}

	fn handle_sidebar_key(&mut self, key: Key) -> GitWorkbenchEvent {
		if self.editing() {
			return self.handle_editor_key(key);
		}
		match key {
			Key::Up | Key::Char('k') => self.move_sidebar_event(-1),
			Key::Down | Key::Char('j') => self.move_sidebar_event(1),
			Key::PageUp => self.move_sidebar_event(-(isize::try_from(self.height.saturating_sub(6).max(1)).unwrap_or(isize::MAX))),
			Key::PageDown => {
				self.move_sidebar_event(isize::try_from(self.height.saturating_sub(6).max(1)).unwrap_or(isize::MAX))
			},
			Key::Home | Key::Char('g') => self.move_sidebar_to(0),
			Key::End | Key::Char('G') => {
				self.move_sidebar_to(self.sidebar_rows.len().saturating_sub(1))
			},
			Key::Left | Key::Char('h') => self.collapse_or_parent(),
			Key::Right | Key::Char('l') => self.expand_or_open(),
			Key::Char('t') => {
				self.tree = !self.tree;
				self.rebuild();
				GitWorkbenchEvent::Consumed
			},
			Key::Enter => self.activate_sidebar(false),
			Key::Space => self.activate_sidebar(true),
			Key::Char('s') => self.explicit_sidebar_stage(true),
			Key::Char('u') => self.explicit_sidebar_stage(false),
			_ => {
				let event = self.ui.handle_key(key);
				self.sync_control_values();
				self.route_ui(event)
			},
		}
	}

	fn handle_editor_key(&mut self, key: Key) -> GitWorkbenchEvent {
		match (self.current_sidebar_target().cloned(), key) {
			(Some(SidebarTarget::Summary), Key::Up) => return self.move_sidebar_event(-1),
			(Some(SidebarTarget::Summary), Key::Down | Key::Enter) => {
				return self.move_sidebar_event(1);
			},
			(Some(SidebarTarget::Description), Key::Up) if self.editor_on_first_line() => {
				return self.move_sidebar_event(-1);
			},
			(Some(SidebarTarget::Description), Key::Down) if self.editor_on_last_line() => {
				return self.move_sidebar_event(1);
			},
			_ => {},
		}
		let event = self.ui.handle_key(key);
		self.sync_commit_button();
		self.route_ui(event)
	}

	fn activate_sidebar(&mut self, stage: bool) -> GitWorkbenchEvent {
		let Some(target) = self.current_sidebar_target().cloned() else {
			return GitWorkbenchEvent::Consumed;
		};
		match target {
			SidebarTarget::StageAll => GitWorkbenchEvent::Intent(GitIntent::StageFile(None)),
			SidebarTarget::UnstageAll => GitWorkbenchEvent::Intent(GitIntent::UnstageFile(None)),
			SidebarTarget::Directory { area, path, .. } if stage => self.stage_target(area, path),
			SidebarTarget::Directory { area, path, .. } => {
				let key = directory_key(area, path.as_str());
				if !self.collapsed.remove(&key) {
					self.collapsed.insert(key);
				}
				self.rebuild();
				GitWorkbenchEvent::Consumed
			},
			SidebarTarget::File { area, path, .. } if stage => self.stage_target(area, path),
			SidebarTarget::File { .. } => {
				self.focus = Focus::Diff;
				self.focus_current();
				GitWorkbenchEvent::Consumed
			},
			SidebarTarget::Amend => self.toggle_amend(),
			SidebarTarget::Summary | SidebarTarget::Description => {
				self.focus_current();
				GitWorkbenchEvent::Consumed
			},
			SidebarTarget::Commit => self.submit_commit(),
		}
	}

	fn explicit_sidebar_stage(&mut self, stage: bool) -> GitWorkbenchEvent {
		let Some(target) = self.current_sidebar_target().cloned() else {
			return GitWorkbenchEvent::Consumed;
		};
		match target {
			SidebarTarget::File { area, path, .. } | SidebarTarget::Directory { area, path, .. }
				if (stage && area == GitArea::Unstaged) || (!stage && area == GitArea::Staged) =>
			{
				self.stage_target(area, path)
			},
			_ => GitWorkbenchEvent::Consumed,
		}
	}

	fn stage_target(&self, area: GitArea, path: Str) -> GitWorkbenchEvent {
		match area {
			GitArea::Unstaged => GitWorkbenchEvent::Intent(GitIntent::StageFile(Some(path))),
			GitArea::Staged => GitWorkbenchEvent::Intent(GitIntent::UnstageFile(Some(path))),
			GitArea::Commit => GitWorkbenchEvent::Consumed,
		}
	}

	fn toggle_amend(&mut self) -> GitWorkbenchEvent {
		self.amend = !self.amend;
		let (summary, description) = self.form_values();
		if self.amend && summary.is_empty() && description.is_empty() {
			if let Some(head) = &self.snapshot.head {
				let subject = head.subject.clone();
				let body = head.body.clone();
				self.rebuild_with_form(subject.as_str(), body.as_str());
				return GitWorkbenchEvent::Consumed;
			}
		}
		let _ = self.ui.set_prop(AMEND_ID, Prop::Checked, self.amend);
		self.sync_commit_button();
		GitWorkbenchEvent::Consumed
	}

	fn submit_commit(&mut self) -> GitWorkbenchEvent {
		let (summary, description) = self.form_values();
		if !self.commit_enabled_with(summary.as_str()) {
			return GitWorkbenchEvent::Consumed;
		}
		let summary = summary.as_str().trim();
		let body = description.as_str().trim();
		let message = if body.is_empty() {
			summary.to_str()
		} else {
			sf!("{summary}\n\n{body}")
		};
		let stage_all = self.snapshot.staged.is_empty();
		self.commit_pending = true;
		GitWorkbenchEvent::Intent(GitIntent::Commit { message, amend: self.amend, stage_all })
	}

	fn request_diff_action(&mut self, action: DiffActionKind) -> GitWorkbenchEvent {
		let event = self
			.ui
			.with_component_mut::<DiffPane, _>(DIFF_ID, |pane| pane.request_action(action))
			.flatten();
		event.map_or(GitWorkbenchEvent::Consumed, |event| self.route_ui(event))
	}

	fn request_discard(&mut self) -> GitWorkbenchEvent {
		let event = self
			.ui
			.with_component_mut::<DiffPane, _>(DIFF_ID, |pane| {
				pane.request_action(DiffActionKind::Discard)
			})
			.flatten();
		let Some(UiEvent::DiffAction { target, .. }) = event else {
			return GitWorkbenchEvent::Consumed;
		};
		self.confirm_discard(target)
	}

	fn confirm_discard(&mut self, target: DiffTarget) -> GitWorkbenchEvent {
		if target == DiffTarget::File {
			return GitWorkbenchEvent::Consumed;
		}
		let Some((GitArea::Unstaged, path)) = self.selected.clone() else {
			return GitWorkbenchEvent::Consumed;
		};
		let identity = PendingDiscard { path: path.clone(), target: target.clone() };
		if self.pending_discard.as_ref() != Some(&identity) {
			let label = if matches!(target, DiffTarget::Lines { .. }) {
				"Discard selected lines? Press x again to confirm"
			} else {
				"Discard hunk? Press x (or click) again to confirm"
			};
			self.pending_discard = Some(identity);
			self.status = Some((Str::new_static(label), self.ctx.theme.warn, Instant::now()));
			let _ = self.ui.set_text("git-status", label);
			let _ = self
				.ui
				.set_prop("git-status", Prop::Fg, self.ctx.theme.warn);
			return GitWorkbenchEvent::Consumed;
		}
		self.pending_discard = None;
		self.map_diff_action(DiffActionKind::Discard, target)
	}

	fn route_ui(&mut self, event: UiEvent) -> GitWorkbenchEvent {
		match event {
			UiEvent::DiffAction { action: DiffActionKind::Discard, target, .. } => {
				self.confirm_discard(target)
			},
			UiEvent::DiffAction { action, target, .. } => self.map_diff_action(action, target),
			UiEvent::Pressed(id) => self.activate_chrome(id.as_str()),
			UiEvent::Changed { id, value } if id.as_str() == VIEW_STYLE_ID => {
				self.tree = value.as_str() == "tree";
				self.rebuild();
				GitWorkbenchEvent::Consumed
			},
			UiEvent::Changed { id, value } if id.as_str() == VIEW_ID => {
				let Ok(mode) = value.as_str().parse::<ViewMode>() else {
					return GitWorkbenchEvent::Consumed;
				};
				self.set_mode(mode)
			},
			UiEvent::Changed { id, value } if id.as_str() == AMEND_ID => {
				let checked = value.as_str() == "true";
				if checked != self.amend {
					self.toggle_amend()
				} else {
					GitWorkbenchEvent::Consumed
				}
			},
			UiEvent::Cancel => GitWorkbenchEvent::Close,
			_ => GitWorkbenchEvent::Consumed,
		}
	}

	fn map_diff_action(&mut self, action: DiffActionKind, target: DiffTarget) -> GitWorkbenchEvent {
		let Some((area, path)) = self.selected.clone() else {
			return GitWorkbenchEvent::Consumed;
		};
		let valid = matches!(
			(action, area),
			(DiffActionKind::Stage | DiffActionKind::Discard, GitArea::Unstaged)
				| (DiffActionKind::Unstage, GitArea::Staged)
		);
		if !valid || (action == DiffActionKind::Discard && target == DiffTarget::File) {
			return GitWorkbenchEvent::Consumed;
		}
		let op = match action {
			DiffActionKind::Stage => GitPatchOp::Stage,
			DiffActionKind::Unstage => GitPatchOp::Unstage,
			DiffActionKind::Discard => GitPatchOp::Discard,
		};
		match target {
			DiffTarget::File => match op {
				GitPatchOp::Stage => GitWorkbenchEvent::Intent(GitIntent::StageFile(Some(path))),
				GitPatchOp::Unstage => GitWorkbenchEvent::Intent(GitIntent::UnstageFile(Some(path))),
				GitPatchOp::Discard => GitWorkbenchEvent::Consumed,
			},
			DiffTarget::Lines { old, new } => {
				GitWorkbenchEvent::Intent(GitIntent::ApplyLines { op, path, old, new })
			},
			DiffTarget::Hunk(index) => {
				let ranges = self
					.ui
					.with_component_mut::<DiffPane, _>(DIFF_ID, |pane| {
						pane.document().and_then(|document| {
							document.hunks.get(index).map(|hunk| {
								(inclusive_range(hunk.old_range), inclusive_range(hunk.new_range))
							})
						})
					})
					.flatten();
				let Some((old, new)) = ranges else {
					return GitWorkbenchEvent::Consumed;
				};
				GitWorkbenchEvent::Intent(GitIntent::ApplyLines { op, path, old, new })
			},
		}
	}

	fn activate_chrome(&mut self, id: &str) -> GitWorkbenchEvent {
		if let Some(index) = id
			.strip_prefix("git-sidebar-row-")
			.and_then(|index| index.parse::<usize>().ok())
		{
			let was_selected = index == self.sidebar_selected;
			if let Some(intent) = self.select_sidebar(index) {
				return GitWorkbenchEvent::Intent(intent);
			}
			return if was_selected
				&& matches!(self.current_sidebar_target(), Some(SidebarTarget::File { .. }))
			{
				self.activate_sidebar(true)
			} else if matches!(self.current_sidebar_target(), Some(SidebarTarget::Directory { .. })) {
				self.activate_sidebar(false)
			} else if was_selected {
				self.activate_sidebar(false)
			} else {
				GitWorkbenchEvent::Consumed
			};
		}
		match id {
			"git-close" => GitWorkbenchEvent::Close,
			"git-stage-all" => GitWorkbenchEvent::Intent(GitIntent::StageFile(None)),
			"git-unstage-all" => GitWorkbenchEvent::Intent(GitIntent::UnstageFile(None)),
			"git-stage-file" => self
				.selected
				.as_ref()
				.map_or(GitWorkbenchEvent::Consumed, |(_, path)| {
					GitWorkbenchEvent::Intent(GitIntent::StageFile(Some(path.clone())))
				}),
			"git-unstage-file" => self
				.selected
				.as_ref()
				.map_or(GitWorkbenchEvent::Consumed, |(_, path)| {
					GitWorkbenchEvent::Intent(GitIntent::UnstageFile(Some(path.clone())))
				}),
			"git-up" => self.jump_hunk_or_file(-1),
			"git-down" => self.jump_hunk_or_file(1),
			"git-ws" => self.cycle_whitespace(),
			"git-wrap" => {
				self.with_pane(|pane| pane.toggle_wrap());
				self.wrap = !self.wrap;
				self.sync_toggle_props();
				GitWorkbenchEvent::Consumed
			},
			COMMIT_ID => self.submit_commit(),
			_ => GitWorkbenchEvent::Consumed,
		}
	}

	fn set_mode(&mut self, mode: ViewMode) -> GitWorkbenchEvent {
		self.with_pane(|pane| pane.set_mode(mode));
		self.view_mode = mode;
		self.sync_view_value();
		GitWorkbenchEvent::Consumed
	}

	fn cycle_whitespace(&mut self) -> GitWorkbenchEvent {
		self.whitespace = match self.whitespace {
			DiffWhitespaceMode::Off => DiffWhitespaceMode::Whitespace,
			DiffWhitespaceMode::Whitespace => DiffWhitespaceMode::Formatting,
			DiffWhitespaceMode::Formatting => DiffWhitespaceMode::Off,
		};
		self.status =
			Some((Str::new_static("Whitespace mode changed"), self.ctx.theme.muted, Instant::now()));
		self.install_document();
		self.rebuild();
		GitWorkbenchEvent::Consumed
	}

	fn jump_hunk_or_file(&mut self, direction: i8) -> GitWorkbenchEvent {
		let moved = self
			.ui
			.with_component_mut::<DiffPane, _>(DIFF_ID, |pane| pane.jump_hunk(direction))
			.unwrap_or(false);
		if moved {
			return GitWorkbenchEvent::Consumed;
		}
		self.select_adjacent_file(if direction < 0 { -1 } else { 1 }, direction < 0)
	}

	fn select_adjacent_file(&mut self, direction: isize, land_last: bool) -> GitWorkbenchEvent {
		let Some((area, path)) = self.selected.as_ref() else {
			return GitWorkbenchEvent::Consumed;
		};
		let start = self.sidebar_rows.iter().position(|row| matches!(&row.target, SidebarTarget::File { area: row_area, path: row_path, .. } if row_area == area && row_path == path)).unwrap_or(self.sidebar_selected);
		let Some(mut index) = start.checked_add_signed(direction) else {
			return GitWorkbenchEvent::Consumed;
		};
		while index < self.sidebar_rows.len() {
			if matches!(self.sidebar_rows[index].target, SidebarTarget::File { .. }) {
				self.pending_last_hunk = land_last;
				return self
					.select_sidebar(index)
					.map_or(GitWorkbenchEvent::Consumed, GitWorkbenchEvent::Intent);
			}
			let Some(next) = index.checked_add_signed(direction) else {
				break;
			};
			index = next;
		}
		GitWorkbenchEvent::Consumed
	}

	fn collapse_or_parent(&mut self) -> GitWorkbenchEvent {
		let Some(target) = self.current_sidebar_target().cloned() else {
			return GitWorkbenchEvent::Consumed;
		};
		let is_directory = matches!(&target, SidebarTarget::Directory { .. });
		let (area, path, depth) = match target {
			SidebarTarget::Directory { area, path, depth } => (area, path, depth),
			SidebarTarget::File { area, path, depth } => (area, path, depth),
			_ => return GitWorkbenchEvent::Consumed,
		};
		if is_directory {
			let key = directory_key(area, path.as_str());
			if !self.collapsed.contains(&key) {
				self.collapsed.insert(key);
				self.rebuild();
				return GitWorkbenchEvent::Consumed;
			}
		}
		for index in (0..self.sidebar_selected).rev() {
			let candidate = &self.sidebar_rows[index].target;
			if !candidate.is_file_or_directory() {
				break;
			}
			if matches!(candidate, SidebarTarget::Directory { area: candidate_area, .. } if *candidate_area == area)
				&& candidate
					.depth()
					.is_some_and(|candidate_depth| candidate_depth < depth)
			{
				return self.move_sidebar_to(index);
			}
		}
		GitWorkbenchEvent::Consumed
	}

	fn expand_or_open(&mut self) -> GitWorkbenchEvent {
		let Some(target) = self.current_sidebar_target().cloned() else {
			return GitWorkbenchEvent::Consumed;
		};
		match target {
			SidebarTarget::Directory { area, path, .. } => {
				let key = directory_key(area, path.as_str());
				if self.collapsed.remove(&key) {
					self.rebuild();
					GitWorkbenchEvent::Consumed
				} else {
					self.move_sidebar_event(1)
				}
			},
			SidebarTarget::File { .. } => {
				self.focus = Focus::Diff;
				self.focus_current();
				GitWorkbenchEvent::Consumed
			},
			_ => GitWorkbenchEvent::Consumed,
		}
	}

	fn select_sidebar(&mut self, index: usize) -> Option<GitIntent> {
		self.sidebar_selected = index.min(self.sidebar_rows.len().saturating_sub(1));
		self.keep_sidebar_selection_visible();
		let next = self
			.current_sidebar_target()
			.and_then(|target| match target {
				SidebarTarget::File { area, path, .. } => Some((*area, path.clone())),
				_ => None,
			});
		self.focus_current();
		if let Some(next) = next
			&& self.selected.as_ref() != Some(&next)
		{
			self.selected = Some(next);
			self.contents = None;
			self.install_document();
			self.rebuild();
			return self.request_selected_load();
		}
		None
	}

	fn move_sidebar_event(&mut self, delta: isize) -> GitWorkbenchEvent {
		let index = self
			.sidebar_selected
			.saturating_add_signed(delta)
			.min(self.sidebar_rows.len().saturating_sub(1));
		self.move_sidebar_to(index)
	}

	fn move_sidebar_to(&mut self, index: usize) -> GitWorkbenchEvent {
		let previous = self.sidebar_selected;
		if let Some(intent) = self.select_sidebar(index) {
			return GitWorkbenchEvent::Intent(intent);
		}
		if self.sidebar_selected != previous {
			self.rebuild();
		}
		GitWorkbenchEvent::Consumed
	}

	fn current_sidebar_target(&self) -> Option<&SidebarTarget> {
		self
			.sidebar_rows
			.get(self.sidebar_selected)
			.map(|row| &row.target)
	}

	fn select_target_kind(&mut self, desired: SidebarTarget) {
		if let Some(index) = self
			.sidebar_rows
			.iter()
			.position(|row| std::mem::discriminant(&row.target) == std::mem::discriminant(&desired))
		{
			self.sidebar_selected = index;
		}
		self.keep_sidebar_selection_visible();
		self.focus_current();
	}

	fn first_file_target(&self) -> Option<usize> {
		self
			.sidebar_rows
			.iter()
			.position(|row| matches!(row.target, SidebarTarget::File { .. }))
	}

	fn focus_current(&mut self) {
		let id = match self.focus {
			Focus::Diff => DIFF_ID.to_str(),
			Focus::Sidebar => match self.current_sidebar_target() {
				Some(SidebarTarget::StageAll) => "git-stage-all".to_str(),
				Some(SidebarTarget::UnstageAll) => "git-unstage-all".to_str(),
				Some(SidebarTarget::Amend) => AMEND_ID.to_str(),
				Some(SidebarTarget::Summary) => SUMMARY_ID.to_str(),
				Some(SidebarTarget::Description) => DESCRIPTION_ID.to_str(),
				Some(SidebarTarget::Commit) => COMMIT_ID.to_str(),
				_ => sf!("git-sidebar-row-{}", self.sidebar_selected),
			},
		};
		let _ = self.ui.focus_id(id.as_str());
		let color = if self.focus == Focus::Sidebar {
			self.ctx.theme.accent
		} else {
			self.ctx.theme.border
		};
		let _ = self.ui.set_prop("git-separator", Prop::Fg, color);
		if self.status.is_none() {
			let hint = self.focus.hint();
			let _ = self.ui.set_text("git-status", hint);
		}
	}

	fn editing(&self) -> bool {
		self.focus == Focus::Sidebar
			&& matches!(
				self.current_sidebar_target(),
				Some(SidebarTarget::Summary | SidebarTarget::Description)
			)
	}

	fn editor_on_first_line(&mut self) -> bool {
		self
			.ui
			.with_component_mut::<EditorPane, _>(DESCRIPTION_PANE_ID, |editor| {
				editor.cursor_on_first_line()
			})
			.unwrap_or(true)
	}

	fn editor_on_last_line(&mut self) -> bool {
		self
			.ui
			.with_component_mut::<EditorPane, _>(DESCRIPTION_PANE_ID, |editor| editor.cursor_on_last_line())
			.unwrap_or(true)
	}

	fn select_sidebar_form_at(&mut self, row: u16, content_height: u16) {
		if self.is_commit_view() || content_height == 0 {
			return;
		}
		let (_, description) = self.form_values();
		let description_rows = description.lines().count().clamp(1, 5) as u16;
		let commit_row = content_height.saturating_sub(1);
		let description_start = commit_row.saturating_sub(description_rows);
		let summary_row = description_start.saturating_sub(1);
		let amend_row = summary_row.saturating_sub(1);
		let target = if row == commit_row {
			Some(SidebarTarget::Commit)
		} else if row >= description_start && row < commit_row {
			Some(SidebarTarget::Description)
		} else if row == summary_row {
			Some(SidebarTarget::Summary)
		} else if row == amend_row {
			Some(SidebarTarget::Amend)
		} else {
			None
		};
		if let Some(target) = target {
			self.select_target_kind(target);
		}
	}
	fn sidebar_visible_rows(&self, description: &str) -> usize {
		let content_rows = self.height.saturating_sub(2).max(1);
		if self.is_commit_view() {
			let sidebar_width = (self.width * 3 / 10).clamp(SIDEBAR_MIN, SIDEBAR_MAX);
			let Some(head) = &self.snapshot.head else {
				return 1;
			};
			let text_rows = |text: &str| {
				cell_width(text)
					.div_ceil(sidebar_width.max(1))
					.max(1)
			};
			let body_rows = head
				.body
				.lines()
				.take(8)
				.fold(0_u16, |rows, line| rows.saturating_add(text_rows(line)));
			let metadata_rows = text_rows(head.subject.as_str())
				.saturating_add(9)
				.saturating_add(u16::from(body_rows > 0).saturating_add(body_rows))
				.saturating_add(u16::from(!head.parents.is_empty()));
			return usize::from(content_rows.saturating_sub(metadata_rows).max(1));
		}
		let description_rows =
			u16::try_from(description.lines().count().clamp(1, 5)).unwrap_or(5);
		usize::from(content_rows.saturating_sub(7 + description_rows).max(1))
	}

	fn clamp_sidebar_scroll(&mut self, visible: usize) {
		self.sidebar_scroll_top =
			clamp_sidebar_scroll(self.sidebar_scroll_top, self.sidebar_file_row_count(), visible);
	}

	fn sidebar_file_row_count(&self) -> usize {
		if self.is_commit_view() {
			self.sidebar_rows.len()
		} else {
			self.sidebar_rows.len().saturating_sub(4)
		}
	}

	fn keep_sidebar_selection_visible(&mut self) {
		let file_rows = self.sidebar_file_row_count();
		if self.sidebar_selected >= file_rows {
			return;
		}
		let (_, description) = self.form_values();
		let visible = self.sidebar_visible_rows(description.as_str());
		self.sidebar_scroll_top = chase_sidebar_selection(
			self.sidebar_scroll_top,
			self.sidebar_selected,
			file_rows,
			visible,
		);
	}

	fn scroll_sidebar(&mut self, delta: isize) {
		let (_, description) = self.form_values();
		let visible = self.sidebar_visible_rows(description.as_str());
		let previous = self.sidebar_scroll_top;
		self.sidebar_scroll_top = self.sidebar_scroll_top.saturating_add_signed(delta);
		self.clamp_sidebar_scroll(visible);
		if self.sidebar_scroll_top == previous {
			return;
		}
		self.rebuild_window();
	}

	fn clear_diff_selection(&mut self) -> bool {
		self
			.ui
			.with_component_mut::<DiffPane, _>(DIFF_ID, |pane| pane.clear_selection())
			.unwrap_or(false)
	}

	fn form_values(&self) -> (Str, Str) {
		let values = self.ui.values();
		let summary = values
			.get(SUMMARY_ID)
			.and_then(serde_json::Value::as_str)
			.map_or_else(Str::default, Str::new);
		let description = values
			.get(DESCRIPTION_ID)
			.and_then(serde_json::Value::as_str)
			.map_or_else(Str::default, Str::new);
		(summary, description)
	}

	pub(super) fn commit_enabled_with(&self, summary: &str) -> bool {
		!summary.trim().is_empty()
			&& (!self.snapshot.staged.is_empty()
				|| !self.snapshot.unstaged.is_empty()
				|| (self.amend && self.snapshot.head.is_some()))
	}

	pub(super) fn commit_button_label(&self) -> &'static str {
		if self.snapshot.staged.is_empty() {
			"Stage all & commit"
		} else {
			"Commit staged changes"
		}
	}

	fn sync_commit_button(&mut self) {
		let (summary, _) = self.form_values();
		let disabled = !self.commit_enabled_with(summary.as_str());
		let _ = self.ui.set_prop(COMMIT_ID, Prop::Dim, disabled);
	}

	fn sync_control_values(&mut self) {
		let values = self.ui.values();
		let view_style = values
			.get(VIEW_STYLE_ID)
			.and_then(serde_json::Value::as_str)
			.map(str::to_owned);
		let diff_view = values
			.get(VIEW_ID)
			.and_then(serde_json::Value::as_str)
			.map(str::to_owned);
		let amend = values.get(AMEND_ID).and_then(serde_json::Value::as_bool);
		if let Some(style) = view_style {
			let tree = style == "tree";
			if tree != self.tree {
				self.tree = tree;
				self.rebuild();
				return;
			}
		}
		if let Some(value) = diff_view
			&& let Ok(mode) = value.parse::<ViewMode>()
			&& mode != self.view_mode
		{
			self.view_mode = mode;
			self.with_pane(|pane| pane.set_mode(mode));
		}
		if let Some(checked) = amend
			&& checked != self.amend
		{
			let _ = self.toggle_amend();
		}
	}

	fn sync_view_value(&mut self) {
		self.rebuild_window();
	}

	fn sync_toggle_props(&mut self) {
		let active = self.wrap;
		let _ = self.ui.set_prop("git-wrap", Prop::Active, active);
	}

	pub(super) const fn pane_mode(&self) -> ViewMode {
		self.view_mode
	}

	pub(super) const fn pane_wraps(&self) -> bool {
		self.wrap
	}

	fn with_pane(&mut self, action: impl FnOnce(&mut DiffPane)) {
		let _ = self.ui.with_component_mut::<DiffPane, _>(DIFF_ID, action);
	}

	fn request_selected_load(&mut self) -> Option<GitIntent> {
		let (area, path) = self.selected.clone()?;
		let orig_path = find_file(&self.snapshot, area, path.as_str())?
			.orig_path
			.clone();
		self.load_seq = self.load_seq.wrapping_add(1);
		self.install_document();
		Some(GitIntent::Load { area, path, orig_path, seq: self.load_seq })
	}

	fn request_avatar(&mut self) -> Option<GitIntent> {
		if !self.is_commit_view() {
			return None;
		}
		let email = self.snapshot.head.as_ref()?.author_email.clone();
		if self.avatar_requested.as_ref() == Some(&email)
			|| self
				.avatar
				.as_ref()
				.is_some_and(|(cached, _)| cached == &email)
		{
			return None;
		}
		self.avatar_requested = Some(email.clone());
		Some(GitIntent::Avatar { email })
	}

	fn install_document(&mut self) {
		let contents = self.contents.clone();
		let loaded = contents.is_some();
		let selected = self.selected.clone();
		let whitespace = self.whitespace;
		let empty = if self.snapshot.pinned && self.snapshot.head.is_none() {
			"No commits yet"
		} else {
			"No changes"
		};
		let pending_last = self.pending_last_hunk;
		let _ = self.ui.with_component_mut::<DiffPane, _>(DIFF_ID, |pane| {
			pane.set_empty_message(empty);
			match (selected, contents) {
				(None, _) => pane.set_document(None, DiffPaneState::Empty),
				(_, None) => pane.set_document(None, DiffPaneState::Loading),
				(Some(_), Some(contents)) if contents.too_large => {
					pane.set_document(None, DiffPaneState::TooLarge)
				},
				(Some(_), Some(contents)) if contents.media.is_some() => pane.set_asset(
					contents.old_bytes,
					contents.new_bytes,
					contents.media.unwrap_or_default(),
				),
				(Some(_), Some(contents)) if contents.binary => {
					pane.set_document(None, DiffPaneState::Binary)
				},
				(Some((_, path)), Some(contents)) => {
					let options = DiffBuildOptions { whitespace, language: None };
					let document = DiffDocument::build(
						contents.old_text.as_str(),
						contents.new_text.as_str(),
						path.as_str(),
						&options,
					);
					pane.set_document(Some(document), DiffPaneState::Ready);
					if pending_last {
						while pane.jump_hunk(1) {}
					}
				},
			}
		});
		if loaded {
			self.pending_last_hunk = false;
		}
	}

	fn rebuild(&mut self) {
		let (summary, description) = self.form_values();
		self.rebuild_with_form(summary.as_str(), description.as_str());
	}

	fn rebuild_with_form(&mut self, summary: &str, description: &str) {
		let previous_target = self.current_sidebar_target().cloned();
		self.rebuild_sidebar_rows();
		if let Some(target) = previous_target {
			if let Some(index) = self
				.sidebar_rows
				.iter()
				.position(|row| row.target.key() == target.key())
			{
				self.sidebar_selected = index;
			}
		}
		self.sidebar_selected = self
			.sidebar_selected
			.min(self.sidebar_rows.len().saturating_sub(1));
		let file_rows = self.sidebar_file_row_count();
		if self.sidebar_selected < file_rows {
			self.sidebar_scroll_top = chase_sidebar_selection(
				self.sidebar_scroll_top,
				self.sidebar_selected,
				file_rows,
				self.sidebar_visible_rows(description),
			);
		}
		self.rebuild_retained(summary, description);
	}

	fn rebuild_window(&mut self) {
		let (summary, description) = self.form_values();
		self.rebuild_retained(summary.as_str(), description.as_str());
	}

	fn rebuild_retained(&mut self, summary: &str, description: &str) {
		let old_mode = self.view_mode;
		let old_wrap = self.wrap;
		self.clamp_sidebar_scroll(self.sidebar_visible_rows(description));
		let content_rows = self.height.saturating_sub(2).max(1);
		let sidebar_width = (self.width * 3 / 10).clamp(SIDEBAR_MIN, SIDEBAR_MAX);
		let retained = self
			.ui
			.with_component_mut::<DiffPane, _>(DIFF_ID, std::mem::take);
		let fresh = retained.is_none();
		let mut pane = retained
			.unwrap_or_default()
			.with(Prop::Id, DIFF_ID)
			.with(Prop::H, content_rows)
			.with(Prop::Minimap, true);
		pane.set_mode(old_mode);
		if fresh && old_wrap {
			pane.toggle_wrap();
		}
		pane.set_patch_target(self.patch_target());
		let sidebar = self.sidebar_component(sidebar_width, summary, description);
		let root = self.root_component(pane, sidebar, sidebar_width, content_rows);
		self.ui = Ui::from_root(root, self.width.max(1), self.ctx.clone());
		self.focus_current();
		if fresh {
			self.install_document();
		}
	}

	fn patch_target(&self) -> Option<DiffPatchTarget> {
		let (area, path) = self.selected.as_ref()?;
		let file = find_file(&self.snapshot, *area, path.as_str())?;
		match area {
			GitArea::Unstaged
				if !matches!(file.kind, GitChangeKind::Untracked | GitChangeKind::Conflicted) =>
			{
				Some(DiffPatchTarget::Stage)
			},
			GitArea::Staged => Some(DiffPatchTarget::Unstage),
			GitArea::Unstaged | GitArea::Commit => None,
		}
	}

	pub(super) fn current_counts(&self) -> (u64, u64) {
		self
			.selected
			.as_ref()
			.and_then(|(area, path)| find_file(&self.snapshot, *area, path.as_str()))
			.map_or((0, 0), |file| (file.additions.unwrap_or(0), file.deletions.unwrap_or(0)))
	}

	pub(super) fn scope_label(&self) -> Str {
		match self.selected.as_ref().map(|(area, _)| area) {
			Some(GitArea::Unstaged)
				if self.selected.as_ref().is_some_and(|(_, path)| {
					find_file(&self.snapshot, GitArea::Unstaged, path.as_str())
						.is_some_and(|file| file.kind == GitChangeKind::Untracked)
				}) =>
			{
				Str::new_static("Untracked")
			},
			Some(GitArea::Unstaged) => Str::new_static("Unstaged"),
			Some(GitArea::Staged) => Str::new_static("Staged"),
			Some(GitArea::Commit) => self
				.snapshot
				.head
				.as_ref()
				.map_or_else(|| Str::new_static("Commit"), |head| short_sha(&head.sha)),
			None => self
				.snapshot
				.branch
				.clone()
				.unwrap_or_else(|| Str::new_static("HEAD")),
		}
	}

	pub(super) fn is_commit_view(&self) -> bool {
		self.snapshot.pinned || (self.snapshot.unstaged.is_empty() && self.snapshot.staged.is_empty())
	}
}

fn nearest_survivor(
	previous: &[SidebarRow],
	current: &[SidebarRow],
	missing: &Str,
) -> Option<usize> {
	let index = previous
		.iter()
		.position(|row| row.target.key() == *missing)?;
	let current_index = |target: &SidebarTarget| {
		if !target.is_file_or_directory() {
			return None;
		}
		let key = target.key();
		current.iter().position(|row| row.target.key() == key)
	};
	for row in &previous[index + 1..] {
		if let Some(index) = current_index(&row.target) {
			return Some(index);
		}
	}
	for row in previous[..index].iter().rev() {
		if let Some(index) = current_index(&row.target) {
			return Some(index);
		}
	}
	current
		.iter()
		.position(|row| matches!(row.target, SidebarTarget::File { .. }))
}

fn clamp_sidebar_scroll(top: usize, len: usize, visible: usize) -> usize {
	top.min(len.saturating_sub(visible.max(1)))
}

fn chase_sidebar_selection(
	top: usize,
	selected: usize,
	len: usize,
	visible: usize,
) -> usize {
	let visible = visible.max(1);
	let chased = if selected < top {
		selected
	} else if selected >= top.saturating_add(visible) {
		selected.saturating_sub(visible - 1)
	} else {
		top
	};
	clamp_sidebar_scroll(chased, len, visible)
}

fn first_file(snapshot: &GitSnapshot) -> Option<&GitFileRow> {
	if snapshot.pinned || (snapshot.unstaged.is_empty() && snapshot.staged.is_empty()) {
		snapshot.head.as_ref()?.files.first()
	} else {
		snapshot
			.unstaged
			.first()
			.or_else(|| snapshot.staged.first())
	}
}

fn find_file<'a>(snapshot: &'a GitSnapshot, area: GitArea, path: &str) -> Option<&'a GitFileRow> {
	let files: &[GitFileRow] = match area {
		GitArea::Unstaged => &snapshot.unstaged,
		GitArea::Staged => &snapshot.staged,
		GitArea::Commit => snapshot
			.head
			.as_ref()
			.map_or(&[], |head| head.files.as_slice()),
	};
	files.iter().find(|file| file.path.as_str() == path)
}

const fn inclusive_range((start, count): (u32, u32)) -> (u32, u32) {
	if count == 0 {
		(0, 0)
	} else {
		(start, start.saturating_add(count).saturating_sub(1))
	}
}

pub(super) fn split_path(path: &str) -> (&str, &str) {
	path
		.rsplit_once('/')
		.map_or(("", path), |(directory, basename)| (&path[..directory.len() + 1], basename))
}

pub(super) fn short_sha(sha: &Str) -> Str {
	sha.slice(..sha.len().min(8))
}

#[cfg(test)]
mod tests {
	use omp_core::{Str, sf};
	use omp_tui::{DiffActionKind, DiffTarget, Key, Mouse, Size, UiContext, ViewMode, cell_width};

	use super::{
		Focus, GitArea, GitChangeKind, GitCommitInfo, GitFileContents, GitFileRow, GitIntent,
		GitPatchOp, GitSnapshot, GitUpdate, GitWorkbench, GitWorkbenchEvent, SidebarTarget,
		chase_sidebar_selection, clamp_sidebar_scroll,
	};
	use super::{
		commit_view::identicon_lines,
		sidebar::{fit_sidebar_path, sidebar_rows},
	};

	fn file(path: &'static str, area: GitArea) -> GitFileRow {
		GitFileRow {
			path: Str::new_static(path),
			orig_path: None,
			kind: GitChangeKind::Modified,
			area,
			additions: Some(2),
			deletions: Some(1),
		}
	}

	fn head() -> GitCommitInfo {
		GitCommitInfo {
			sha:          Str::new_static("1234567890abcdef"),
			subject:      Str::new_static("existing subject"),
			body:         Str::new_static("existing body"),
			author_name:  Str::new_static("Ada"),
			author_email: Str::new_static("ada@example.com"),
			author_date:  Str::new_static("2026-08-20T00:00:00Z"),
			parents:      vec![Str::new_static("parent")],
			files:        vec![file("src/old.rs", GitArea::Commit)],
		}
	}

	fn dirty() -> GitSnapshot {
		GitSnapshot {
			branch:   Some(Str::new_static("main")),
			unstaged: vec![
				file("a/one.rs", GitArea::Unstaged),
				file("a/two.rs", GitArea::Unstaged),
				file("b/three.rs", GitArea::Unstaged),
			],
			staged:   vec![file("tests/a.rs", GitArea::Staged)],
			head:     Some(head()),
			pinned:   false,
		}
	}

	fn contents(old: &'static str, new: &'static str) -> GitFileContents {
		GitFileContents {
			old_text:  Str::new_static(old),
			new_text:  Str::new_static(new),
			binary:    false,
			too_large: false,
			old_bytes: None,
			new_bytes: None,
			media:     None,
		}
	}

	#[test]
	fn starts_in_sidebar_and_enter_opens_while_space_stages() {
		let mut workbench = GitWorkbench::open(dirty(), &UiContext::default());
		assert_eq!(workbench.focus, Focus::Sidebar);
		workbench.sidebar_selected = workbench.first_file_target().unwrap();
		assert_eq!(workbench.handle_key(Key::Enter), GitWorkbenchEvent::Consumed);
		assert_eq!(workbench.focus, Focus::Diff);
		workbench.focus = Focus::Sidebar;
		assert!(
			matches!(workbench.handle_key(Key::Space), GitWorkbenchEvent::Intent(GitIntent::StageFile(Some(path))) if path.as_str() == "a/one.rs")
		);
	}

	#[test]
	fn escape_ladders_from_editor_then_diff_selection_then_close() {
		let mut workbench = GitWorkbench::open(dirty(), &UiContext::default());
		workbench.select_target_kind(SidebarTarget::Summary);
		assert_eq!(workbench.handle_key(Key::Esc), GitWorkbenchEvent::Consumed);
		assert!(matches!(workbench.current_sidebar_target(), Some(SidebarTarget::Commit)));
		workbench.focus = Focus::Diff;
		let GitIntent::Load { seq, .. } = workbench.initial_intent().unwrap() else {
			panic!("load")
		};
		workbench.apply(GitUpdate::Contents { seq, contents: contents("old\n", "new\n") });
		workbench.route_diff_navigation(Key::SelectDown);
		assert_eq!(workbench.handle_key(Key::Esc), GitWorkbenchEvent::Consumed);
		assert_eq!(workbench.handle_key(Key::Esc), GitWorkbenchEvent::Close);
	}

	#[test]
	fn discard_is_scoped_and_identity_confirmed() {
		let mut workbench = GitWorkbench::open(dirty(), &UiContext::default());
		workbench.sidebar_selected = workbench.first_file_target().unwrap();
		assert_eq!(workbench.handle_key(Key::Enter), GitWorkbenchEvent::Consumed);
		let GitIntent::Load { seq, .. } = workbench.initial_intent().unwrap() else {
			panic!("load")
		};
		workbench.apply(GitUpdate::Contents { seq, contents: contents("old\n", "new\n") });
		assert_eq!(
			workbench.handle_key(Key::Char('x')),
			GitWorkbenchEvent::Consumed,
			"file-wide discard is forbidden"
		);
		workbench.set_mode(ViewMode::Hunk);
		assert_eq!(workbench.handle_key(Key::Char('x')), GitWorkbenchEvent::Consumed);
		workbench.handle_key(Key::Char('j'));
		assert_eq!(
			workbench.handle_key(Key::Char('x')),
			GitWorkbenchEvent::Consumed,
			"other action invalidates exact identity"
		);
		assert!(matches!(
			workbench.handle_key(Key::Char('x')),
			GitWorkbenchEvent::Intent(GitIntent::ApplyLines { op: GitPatchOp::Discard, .. })
		));
	}

	#[test]
	fn nearest_survivor_prefers_next_then_previous() {
		let mut workbench = GitWorkbench::open(dirty(), &UiContext::default());
		let first = workbench
			.sidebar_rows
			.iter()
			.position(
				|row| matches!(&row.target, SidebarTarget::File { path, .. } if path.as_str() == "a/one.rs"),
			)
			.unwrap();
		workbench.sidebar_selected = first;
		workbench.selected = Some((GitArea::Unstaged, Str::new_static("a/one.rs")));
		let mut next = dirty();
		next.unstaged.remove(0);
		let _ = workbench.apply(GitUpdate::Snapshot(next));
		assert!(
			matches!(workbench.current_sidebar_target(), Some(SidebarTarget::File { path, .. }) if path.as_str() == "a/two.rs")
		);
	}

	#[test]
	fn sidebar_window_chases_selection_and_clamps_at_both_ends() {
		assert_eq!(chase_sidebar_selection(10, 5, 100, 8), 5);
		assert_eq!(chase_sidebar_selection(5, 12, 100, 8), 5);
		assert_eq!(chase_sidebar_selection(5, 13, 100, 8), 6);
		assert_eq!(chase_sidebar_selection(0, 99, 100, 8), 92);
		assert_eq!(clamp_sidebar_scroll(90, 10, 8), 2);
		assert_eq!(clamp_sidebar_scroll(3, 4, 8), 0);
	}

	#[test]
	fn sidebar_wheel_scroll_does_not_flip_diff_focus() {
		let mut snapshot = dirty();
		snapshot.unstaged.extend((0..40).map(|index| GitFileRow {
			path:       sf!("bulk/file-{index:02}.rs"),
			orig_path:  None,
			kind:       GitChangeKind::Modified,
			area:       GitArea::Unstaged,
			additions:  Some(2),
			deletions:  Some(1),
		}));
		let mut workbench = GitWorkbench::open(snapshot, &UiContext::default());
		let viewport = Size::new(80, 12);
		let _ = workbench.layer(viewport);
		workbench.focus = Focus::Diff;
		workbench.focus_current();
		assert_eq!(
			workbench.handle_mouse(79, 4, Mouse::WheelDown, viewport),
			GitWorkbenchEvent::Consumed
		);
		assert_eq!(workbench.focus, Focus::Diff);
		assert_eq!(workbench.sidebar_scroll_top, 3);
		assert!(
			workbench
				.sidebar_scroll_top
				.saturating_add(workbench.sidebar_visible_rows(""))
				<= workbench.sidebar_file_row_count()
		);
	}

	#[test]
	fn sidebar_path_truncation_keeps_counts_and_directory_tail_inside_width() {
		let ctx = UiContext::default();
		let snapshot = GitSnapshot {
			branch:   Some(Str::new_static("main")),
			unstaged: vec![file(
				"very/long/directory/prefix/important.rs",
				GitArea::Unstaged,
			)],
			staged:   Vec::new(),
			head:     Some(head()),
			pinned:   false,
		};
		let rows = sidebar_rows(&snapshot, false, &Default::default(), &ctx);
		let row = rows
			.iter()
			.find(|row| matches!(row.target, SidebarTarget::File { .. }))
			.unwrap();
		let width = 20;
		let (directory, basename) = fit_sidebar_path(row, width);
		assert!(directory.starts_with('…'));
		assert_eq!(basename.as_str(), "important.rs");
		let rendered_width = 1_u16
			.saturating_add(1)
			.saturating_add(cell_width(directory.as_str()))
			.saturating_add(cell_width(basename.as_str()))
			.saturating_add(2)
			.saturating_add(2);
		assert!(rendered_width <= width);
	}

	#[test]
	fn commit_submission_includes_editor_description() {
		let mut workbench = GitWorkbench::open(dirty(), &UiContext::default());
		assert_eq!(workbench.handle_key(Key::Char('c')), GitWorkbenchEvent::Consumed);
		for ch in "subject".chars() {
			assert_eq!(workbench.handle_key(Key::Char(ch)), GitWorkbenchEvent::Consumed);
		}
		assert_eq!(workbench.handle_key(Key::Enter), GitWorkbenchEvent::Consumed);
		for ch in "body".chars() {
			assert_eq!(workbench.handle_key(Key::Char(ch)), GitWorkbenchEvent::Consumed);
		}
		assert_eq!(workbench.handle_key(Key::Down), GitWorkbenchEvent::Consumed);
		assert_eq!(
			workbench.handle_key(Key::Enter),
			GitWorkbenchEvent::Intent(GitIntent::Commit {
				message:   Str::new_static("subject\n\nbody"),
				amend:     false,
				stage_all: false,
			})
		);
	}

	#[test]
	fn commit_composer_keys_do_not_leak_staging_actions() {
		let mut snapshot = dirty();
		snapshot.unstaged.insert(0, file("logo.png", GitArea::Unstaged));
		let mut workbench = GitWorkbench::open(snapshot.clone(), &UiContext::default());
		let mut events = Vec::new();
		for key in [Key::Down, Key::Down, Key::Enter, Key::Tab, Key::Space] {
			events.push(workbench.handle_key(key));
		}
		let staged_path = events
			.iter()
			.find_map(|event| match event {
				GitWorkbenchEvent::Intent(GitIntent::StageFile(Some(path))) => Some(path.clone()),
				_ => None,
			})
			.expect("the deliberate Space should stage one file");
		let staged_index = snapshot
			.unstaged
			.iter()
			.position(|file| file.path == staged_path)
			.expect("staged path should be unstaged");
		let mut staged = snapshot.unstaged.remove(staged_index);
		staged.area = GitArea::Staged;
		snapshot.staged.push(staged);
		let _ = workbench.apply(GitUpdate::Snapshot(snapshot));
		let viewport = Size::new(120, 34);
		let _ = workbench.layer(viewport);
		events.push(workbench.handle_mouse(119, 4, Mouse::WheelDown, viewport));
		events.push(workbench.handle_key(Key::Char('c')));
		for ch in "added smoke assets".chars() {
			events.push(workbench.handle_key(if ch == ' ' { Key::Space } else { Key::Char(ch) }));
		}
		events.push(workbench.handle_key(Key::Enter));
		for ch in "body text here".chars() {
			events.push(workbench.handle_key(if ch == ' ' { Key::Space } else { Key::Char(ch) }));
		}
		events.push(workbench.handle_key(Key::Down));
		events.push(workbench.handle_key(Key::Enter));

		let staging = events
			.iter()
			.filter(|event| {
				matches!(
					event,
					GitWorkbenchEvent::Intent(
						GitIntent::StageFile(_) | GitIntent::ApplyLines { .. }
					)
				)
			})
			.count();
		assert_eq!(staging, 1, "only the deliberate Space may stage");
		assert!(matches!(
			events.last(),
			Some(GitWorkbenchEvent::Intent(GitIntent::Commit { message, stage_all: false, .. }))
				if message.as_str() == "added smoke assets\n\nbody text here"
		));
	}

	#[test]
	fn amend_prefills_and_success_clears_every_field() {
		let mut workbench = GitWorkbench::open(dirty(), &UiContext::default());
		workbench.select_target_kind(SidebarTarget::Amend);
		assert_eq!(workbench.handle_key(Key::Enter), GitWorkbenchEvent::Consumed);
		let (summary, description) = workbench.form_values();
		assert_eq!(summary.as_str(), "existing subject");
		assert_eq!(description.as_str(), "existing body");
		workbench.select_target_kind(SidebarTarget::Commit);
		assert!(matches!(
			workbench.handle_key(Key::Enter),
			GitWorkbenchEvent::Intent(GitIntent::Commit { amend: true, .. })
		));
		workbench.apply(GitUpdate::ActionDone { message: Str::new_static("committed") });
		let (summary, description) = workbench.form_values();
		assert!(summary.is_empty() && description.is_empty());
		assert!(!workbench.amend);
	}

	#[test]
	fn commit_button_label_and_enabled_state_follow_staging() {
		let mut snapshot = dirty();
		let mut workbench = GitWorkbench::open(snapshot.clone(), &UiContext::default());
		assert_eq!(workbench.commit_button_label(), "Commit staged changes");
		assert!(!workbench.commit_enabled_with("   "));
		assert!(workbench.commit_enabled_with("subject"));
		snapshot.staged.clear();
		let _ = workbench.apply(GitUpdate::Snapshot(snapshot));
		assert_eq!(workbench.commit_button_label(), "Stage all & commit");
	}

	#[test]
	fn directory_space_batches_path_and_explicit_wrong_area_is_noop() {
		let mut workbench = GitWorkbench::open(dirty(), &UiContext::default());
		let directory = workbench.sidebar_rows.iter().position(|row| matches!(&row.target, SidebarTarget::Directory { area: GitArea::Unstaged, path, .. } if path.as_str() == "a")).unwrap();
		workbench.sidebar_selected = directory;
		assert!(
			matches!(workbench.handle_key(Key::Space), GitWorkbenchEvent::Intent(GitIntent::StageFile(Some(path))) if path.as_str() == "a")
		);
		assert_eq!(workbench.handle_key(Key::Char('u')), GitWorkbenchEvent::Consumed);
	}

	#[test]
	fn mapped_diff_actions_keep_file_and_line_contracts() {
		let mut workbench = GitWorkbench::open(dirty(), &UiContext::default());
		assert_eq!(
			workbench.map_diff_action(DiffActionKind::Stage, DiffTarget::File),
			GitWorkbenchEvent::Intent(GitIntent::StageFile(Some(Str::new_static("a/one.rs"))))
		);
		assert_eq!(
			workbench
				.map_diff_action(DiffActionKind::Stage, DiffTarget::Lines { old: (1, 1), new: (1, 1) }),
			GitWorkbenchEvent::Intent(GitIntent::ApplyLines {
				op:   GitPatchOp::Stage,
				path: Str::new_static("a/one.rs"),
				old:  (1, 1),
				new:  (1, 1),
			})
		);
	}

	#[test]
	fn identicon_is_case_folded_mirrored_and_ten_cells_wide() {
		let ctx = UiContext::default();
		let first = identicon_lines("Ada@Example.COM", &ctx);
		assert_eq!(first, identicon_lines("ada@example.com", &ctx));
		for line in first {
			let cells = line.chars().collect::<Vec<_>>();
			assert_eq!(cells.len(), 10);
			assert_eq!(&cells[0..2], &cells[8..10]);
			assert_eq!(&cells[2..4], &cells[6..8]);
		}
	}
}
