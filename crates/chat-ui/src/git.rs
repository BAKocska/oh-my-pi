//! Retained presentation for the fullscreen Git workbench.

use std::{
	collections::{BTreeMap, BTreeSet},
	time::{Duration, Instant},
};

use omp_core::{IntoStr, Str, StrMut, sf};
use omp_tui::{
	DiffActionKind, DiffBuildOptions, DiffDocument, DiffPane, DiffPaneState, DiffPatchTarget,
	DiffTarget, Dim, Icon, Key, Layer, Mouse, OverlayOptions, Prop, Size, Ui, UiContext, UiEvent,
	ViewMode, components::Col, dom,
};
use xxhash_rust::xxh3::xxh3_64;

/// Kind of change reported for one Git path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitChangeKind {
	/// Existing file contents changed.
	Modified,
	/// New tracked file.
	Added,
	/// Removed tracked file.
	Deleted,
	/// Path renamed from [`GitFileRow::orig_path`].
	Renamed,
	/// New untracked file.
	Untracked,
	/// File with unresolved conflicts.
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
	/// Commit author's formatted date.
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

/// Old and new file text loaded for the diff pane.
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
const DISCARD_CONFIRM_TTL: Duration = Duration::from_secs(4);
const SIDEBAR_MIN: u16 = 30;
const SIDEBAR_MAX: u16 = 48;
const DIFF_ID: &str = "git-diff";
const SIDEBAR_ID: &str = "git-sidebar";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Focus {
	#[default]
	Diff,
	Sidebar,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SidebarTarget {
	Info(Str),
	StageAll,
	UnstageAll,
	Directory(Str),
	File { area: GitArea, path: Str },
	Amend,
	Summary,
	Description,
	Commit,
}

#[derive(Clone)]
struct SidebarRow {
	target: SidebarTarget,
	label:  Str,
}

#[derive(Default)]
struct TreeNode {
	files:    Vec<(GitArea, GitFileRow)>,
	children: BTreeMap<Str, TreeNode>,
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
	ui:               Ui,
	ctx:              UiContext,
	options:          OverlayOptions,
	snapshot:         GitSnapshot,
	selected:         Option<(GitArea, Str)>,
	sidebar_rows:     Vec<SidebarRow>,
	sidebar_selected: usize,
	focus:            Focus,
	tree:             bool,
	collapsed:        BTreeSet<Str>,
	contents:         Option<GitFileContents>,
	load_seq:         u64,
	ignore_ws:        bool,
	amend:            bool,
	summary:          StrMut,
	description:      StrMut,
	status:           Option<(Str, bool, Instant)>,
	discard_confirm:  Option<(Str, Instant)>,
	commit_pending:   bool,
	avatar:           Option<(Str, bytes::Bytes)>,
	avatar_requested: Option<Str>,
	width:            u16,
	height:           u16,
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
			focus: Focus::Diff,
			tree: false,
			collapsed: BTreeSet::new(),
			contents: None,
			load_seq: 0,
			ignore_ws: false,
			amend: false,
			summary: StrMut::new(""),
			description: StrMut::new(""),
			status: None,
			discard_confirm: None,
			commit_pending: false,
			avatar: None,
			avatar_requested: None,
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
			GitUpdate::Snapshot(snapshot) => {
				let previous = self.selected.clone();
				self.snapshot = snapshot;
				self.selected = previous
					.clone()
					.filter(|(area, path)| find_file(&self.snapshot, *area, path.as_str()).is_some());
				if self.selected.is_none() {
					self.selected =
						first_file(&self.snapshot).map(|file| (file.area, file.path.clone()));
				}
				let changed = self.selected != previous;
				if changed {
					self.contents = None;
				}
				self.rebuild();
				if changed {
					self
						.request_selected_load()
						.or_else(|| self.request_avatar())
				} else {
					self.request_avatar()
				}
			},
			GitUpdate::Contents { seq, contents } => {
				if seq != self.load_seq {
					return None;
				}
				self.contents = Some(contents);
				self.install_document();
				self.request_avatar()
			},
			GitUpdate::ActionDone { message } => {
				if self.commit_pending {
					self.summary.truncate(0);
					self.description.truncate(0);
					self.commit_pending = false;
				}
				self.status = Some((message, false, Instant::now()));
				self.rebuild();
				None
			},
			GitUpdate::ActionFailed { message } => {
				self.commit_pending = false;
				self.status = Some((message, true, Instant::now()));
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
		if matches!(key, Key::Tab | Key::BackTab) {
			self.focus = match self.focus {
				Focus::Diff => Focus::Sidebar,
				Focus::Sidebar => Focus::Diff,
			};
			self.rebuild();
			return GitWorkbenchEvent::Consumed;
		}
		match self.focus {
			Focus::Diff => self.handle_diff_key(key),
			Focus::Sidebar => self.handle_sidebar_key(key),
		}
	}

	/// Routes pasted text into the active commit text field.
	pub fn handle_paste(&mut self, text: &str) -> GitWorkbenchEvent {
		if self.focus != Focus::Sidebar {
			return GitWorkbenchEvent::Consumed;
		}
		match self.current_sidebar_target() {
			Some(SidebarTarget::Summary) => self.summary.push_str(&text.replace(['\r', '\n'], " ")),
			Some(SidebarTarget::Description) => self.description.push_str(text),
			_ => return GitWorkbenchEvent::Consumed,
		}
		self.rebuild();
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
		if row >= 2 {
			let sidebar_width = (viewport.width * 3 / 10).clamp(SIDEBAR_MIN, SIDEBAR_MAX);
			self.focus = if col < viewport.width.saturating_sub(sidebar_width) {
				Focus::Diff
			} else {
				Focus::Sidebar
			};
		}
		let event = self
			.ui
			.handle_mouse_as_layer(&self.options, viewport, col, row, kind)
			.unwrap_or(UiEvent::None);
		self.route_ui(event)
	}

	/// Returns the full-viewport active layer.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		if self
			.status
			.as_ref()
			.is_some_and(|(_, _, at)| at.elapsed() >= STATUS_TTL)
		{
			self.status = None;
			self.rebuild();
		}
		if viewport.width != self.width || viewport.height != self.height {
			self.width = viewport.width;
			self.height = viewport.height;
			self.rebuild();
		}
		Layer { frame: self.ui.frame(), options: &self.options, active: true }
	}

	fn handle_diff_key(&mut self, key: Key) -> GitWorkbenchEvent {
		match key {
			Key::Esc | Key::Char('q') => GitWorkbenchEvent::Close,
			Key::Char('v') => {
				self.with_pane(|pane| pane.cycle_mode());
				GitWorkbenchEvent::Consumed
			},
			Key::Char('n') => {
				self.with_pane(|pane| {
					pane.jump_hunk(1);
				});
				GitWorkbenchEvent::Consumed
			},
			Key::Char('p') => {
				self.with_pane(|pane| {
					pane.jump_hunk(-1);
				});
				GitWorkbenchEvent::Consumed
			},
			Key::Char('w') => {
				self.with_pane(|pane| pane.toggle_wrap());
				GitWorkbenchEvent::Consumed
			},
			Key::Char('b') => {
				self.ignore_ws = !self.ignore_ws;
				self.install_document();
				self.rebuild();
				GitWorkbenchEvent::Consumed
			},
			Key::Char('s') => self.request_diff_action(DiffActionKind::Stage),
			Key::Char('u') => self.request_diff_action(DiffActionKind::Unstage),
			Key::Char('x') => self.request_discard(),
			_ => {
				let event = self.ui.handle_key(key);
				self.route_ui(event)
			},
		}
	}

	fn handle_sidebar_key(&mut self, key: Key) -> GitWorkbenchEvent {
		let editing = matches!(
			self.current_sidebar_target(),
			Some(SidebarTarget::Summary | SidebarTarget::Description)
		);
		if editing {
			match key {
				Key::Esc => {
					return self.move_sidebar_event(-1);
				},
				Key::Backspace => {
					match self.current_sidebar_target() {
						Some(SidebarTarget::Summary) => {
							pop_char(&mut self.summary);
						},
						Some(SidebarTarget::Description) => {
							pop_char(&mut self.description);
						},
						_ => {},
					}
					self.rebuild();
					return GitWorkbenchEvent::Consumed;
				},
				Key::Char(character) => {
					match self.current_sidebar_target() {
						Some(SidebarTarget::Summary) => self.summary.push(character),
						Some(SidebarTarget::Description) => self.description.push(character),
						_ => {},
					}
					self.rebuild();
					return GitWorkbenchEvent::Consumed;
				},
				Key::Space => {
					match self.current_sidebar_target() {
						Some(SidebarTarget::Summary) => self.summary.push(' '),
						Some(SidebarTarget::Description) => self.description.push(' '),
						_ => {},
					}
					self.rebuild();
					return GitWorkbenchEvent::Consumed;
				},
				Key::ShiftEnter
					if matches!(self.current_sidebar_target(), Some(SidebarTarget::Description)) =>
				{
					self.description.push('\n');
					self.rebuild();
					return GitWorkbenchEvent::Consumed;
				},
				Key::Enter | Key::Down => {
					return self.move_sidebar_event(1);
				},
				Key::Up => {
					return self.move_sidebar_event(-1);
				},
				_ => {},
			}
		}
		match key {
			Key::Esc | Key::Char('q') => GitWorkbenchEvent::Close,
			Key::Up => self.move_sidebar_event(-1),
			Key::Down => self.move_sidebar_event(1),
			Key::PageUp => self.move_sidebar_event(-8),
			Key::PageDown => self.move_sidebar_event(8),
			Key::Char('t') => {
				self.tree = !self.tree;
				self.rebuild();
				GitWorkbenchEvent::Consumed
			},
			Key::Enter | Key::Space => self.activate_sidebar(),
			_ => GitWorkbenchEvent::Consumed,
		}
	}

	fn activate_sidebar(&mut self) -> GitWorkbenchEvent {
		let Some(target) = self.current_sidebar_target().cloned() else {
			return GitWorkbenchEvent::Consumed;
		};
		match target {
			SidebarTarget::Info(_) => GitWorkbenchEvent::Consumed,
			SidebarTarget::StageAll => GitWorkbenchEvent::Intent(GitIntent::StageFile(None)),
			SidebarTarget::UnstageAll => GitWorkbenchEvent::Intent(GitIntent::UnstageFile(None)),
			SidebarTarget::Directory(path) => {
				if !self.collapsed.remove(&path) {
					self.collapsed.insert(path);
				}
				self.rebuild();
				GitWorkbenchEvent::Consumed
			},
			SidebarTarget::File { area, path } => match area {
				GitArea::Unstaged => GitWorkbenchEvent::Intent(GitIntent::StageFile(Some(path))),
				GitArea::Staged => GitWorkbenchEvent::Intent(GitIntent::UnstageFile(Some(path))),
				GitArea::Commit => GitWorkbenchEvent::Consumed,
			},
			SidebarTarget::Amend => {
				self.amend = !self.amend;
				if self.amend
					&& self.summary.is_empty()
					&& let Some(head) = &self.snapshot.head
				{
					self.summary.push_str(head.subject.as_str());
					self.description.push_str(head.body.as_str());
				}
				self.rebuild();
				GitWorkbenchEvent::Consumed
			},
			SidebarTarget::Summary | SidebarTarget::Description => GitWorkbenchEvent::Consumed,
			SidebarTarget::Commit if self.commit_enabled() => {
				let message = self.commit_message();
				let stage_all = self.snapshot.staged.is_empty();
				self.commit_pending = true;
				GitWorkbenchEvent::Intent(GitIntent::Commit { message, amend: self.amend, stage_all })
			},
			SidebarTarget::Commit => GitWorkbenchEvent::Consumed,
		}
	}

	fn request_diff_action(&mut self, action: DiffActionKind) -> GitWorkbenchEvent {
		let event = self
			.ui
			.with_component_mut::<DiffPane, _>(DIFF_ID, |pane| pane.request_action(action))
			.flatten();
		event.map_or(GitWorkbenchEvent::Consumed, |event| self.route_ui(event))
	}

	fn request_discard(&mut self) -> GitWorkbenchEvent {
		let Some((GitArea::Unstaged, path)) = self.selected.as_ref() else {
			return GitWorkbenchEvent::Consumed;
		};
		let Some(file) = find_file(&self.snapshot, GitArea::Unstaged, path.as_str()) else {
			return GitWorkbenchEvent::Consumed;
		};
		if matches!(file.kind, GitChangeKind::Untracked | GitChangeKind::Conflicted) {
			return GitWorkbenchEvent::Consumed;
		}
		let confirmed = self
			.discard_confirm
			.as_ref()
			.is_some_and(|(armed, at)| armed == path && at.elapsed() <= DISCARD_CONFIRM_TTL);
		if !confirmed {
			self.discard_confirm = Some((path.clone(), Instant::now()));
			self.status = Some((
				Str::new_static("Press x again to discard selected changes"),
				true,
				Instant::now(),
			));
			self.rebuild();
			return GitWorkbenchEvent::Consumed;
		}
		self.discard_confirm = None;
		self.request_diff_action(DiffActionKind::Discard)
	}

	fn route_ui(&mut self, event: UiEvent) -> GitWorkbenchEvent {
		match event {
			UiEvent::DiffAction { action, target, .. } => self.map_diff_action(action, target),
			UiEvent::Highlighted { id, value } | UiEvent::Changed { id, value }
				if id.as_str() == SIDEBAR_ID =>
			{
				if let Ok(index) = value.as_str().parse::<usize>()
					&& let Some(intent) = self.select_sidebar(index)
				{
					return GitWorkbenchEvent::Intent(intent);
				}
				GitWorkbenchEvent::Consumed
			},
			UiEvent::Pressed(id) => self.activate_chrome(id.as_str()),
			UiEvent::Cancel => GitWorkbenchEvent::Close,
			_ => GitWorkbenchEvent::Consumed,
		}
	}

	fn map_diff_action(&mut self, action: DiffActionKind, target: DiffTarget) -> GitWorkbenchEvent {
		let Some((area, path)) = self.selected.clone() else {
			return GitWorkbenchEvent::Consumed;
		};
		let valid = matches!(
			(action, &area),
			(DiffActionKind::Stage | DiffActionKind::Discard, GitArea::Unstaged)
				| (DiffActionKind::Unstage, GitArea::Staged)
		);
		if !valid {
			return GitWorkbenchEvent::Consumed;
		}
		let op = match action {
			DiffActionKind::Stage => GitPatchOp::Stage,
			DiffActionKind::Unstage => GitPatchOp::Unstage,
			DiffActionKind::Discard => GitPatchOp::Discard,
		};
		match target {
			DiffTarget::File => match op {
				GitPatchOp::Stage => {
					GitWorkbenchEvent::Intent(GitIntent::StageFile(Some(path.clone())))
				},
				GitPatchOp::Unstage => {
					GitWorkbenchEvent::Intent(GitIntent::UnstageFile(Some(path.clone())))
				},
				GitPatchOp::Discard => {
					let ranges = self
						.ui
						.with_component_mut::<DiffPane, _>(DIFF_ID, |pane| {
							pane.document().map(document_ranges)
						})
						.flatten()
						.unwrap_or(((0, 0), (0, 0)));
					GitWorkbenchEvent::Intent(GitIntent::ApplyLines {
						op,
						path: path.clone(),
						old: ranges.0,
						new: ranges.1,
					})
				},
			},
			DiffTarget::Lines { old, new } => {
				GitWorkbenchEvent::Intent(GitIntent::ApplyLines { op, path: path.clone(), old, new })
			},
			DiffTarget::Hunk(index) => {
				let Some((old, new)) = self
					.ui
					.with_component_mut::<DiffPane, _>(DIFF_ID, |pane| {
						pane.document().and_then(|document| {
							document.hunks.get(index).map(|hunk| {
								(inclusive_range(hunk.old_range), inclusive_range(hunk.new_range))
							})
						})
					})
					.flatten()
				else {
					return GitWorkbenchEvent::Consumed;
				};
				GitWorkbenchEvent::Intent(GitIntent::ApplyLines { op, path: path.clone(), old, new })
			},
		}
	}

	fn activate_chrome(&mut self, id: &str) -> GitWorkbenchEvent {
		match id {
			"git-close" => GitWorkbenchEvent::Close,
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
			"git-view-file" => self.set_mode(ViewMode::File),
			"git-view-split" => self.set_mode(ViewMode::Split),
			"git-view-inline" => self.set_mode(ViewMode::Inline),
			"git-view-hunk" => self.set_mode(ViewMode::Hunk),
			"git-up" => {
				self.with_pane(|pane| {
					pane.jump_hunk(-1);
				});
				GitWorkbenchEvent::Consumed
			},
			"git-down" => {
				self.with_pane(|pane| {
					pane.jump_hunk(1);
				});
				GitWorkbenchEvent::Consumed
			},
			"git-ws" => {
				self.ignore_ws = !self.ignore_ws;
				self.install_document();
				self.rebuild();
				GitWorkbenchEvent::Consumed
			},
			"git-wrap" => {
				self.with_pane(|pane| pane.toggle_wrap());
				GitWorkbenchEvent::Consumed
			},
			"git-tree" | "git-path" => {
				self.tree = id == "git-tree";
				self.rebuild();
				GitWorkbenchEvent::Consumed
			},
			_ => GitWorkbenchEvent::Consumed,
		}
	}

	fn set_mode(&mut self, mode: ViewMode) -> GitWorkbenchEvent {
		self.with_pane(|pane| pane.set_mode(mode));
		GitWorkbenchEvent::Consumed
	}

	fn with_pane(&mut self, action: impl FnOnce(&mut DiffPane)) {
		let _ = self.ui.with_component_mut::<DiffPane, _>(DIFF_ID, action);
	}

	fn select_sidebar(&mut self, index: usize) -> Option<GitIntent> {
		self.sidebar_selected = index.min(self.sidebar_rows.len().saturating_sub(1));
		if let Some(SidebarTarget::File { area, path }) = self.current_sidebar_target().cloned() {
			let next = (area, path);
			if self.selected.as_ref() != Some(&next) {
				self.selected = Some(next);
				self.contents = None;
				return self.request_selected_load();
			}
		}
		None
	}

	fn move_sidebar(&mut self, delta: isize) -> Option<GitIntent> {
		self.sidebar_selected = self
			.sidebar_selected
			.saturating_add_signed(delta)
			.min(self.sidebar_rows.len().saturating_sub(1));
		let intent = self.select_sidebar(self.sidebar_selected);
		self.rebuild();
		intent
	}

	fn move_sidebar_event(&mut self, delta: isize) -> GitWorkbenchEvent {
		self
			.move_sidebar(delta)
			.map_or(GitWorkbenchEvent::Consumed, GitWorkbenchEvent::Intent)
	}

	fn current_sidebar_target(&self) -> Option<&SidebarTarget> {
		self
			.sidebar_rows
			.get(self.sidebar_selected)
			.map(|row| &row.target)
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
		if !self.snapshot.pinned
			&& (!self.snapshot.unstaged.is_empty() || !self.snapshot.staged.is_empty())
		{
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
		let selected = self.selected.clone();
		let ignore_ws = self.ignore_ws;
		let empty = if self.snapshot.pinned && self.snapshot.head.is_none() {
			"No commits yet"
		} else {
			"No changes"
		};
		let _ = self.ui.with_component_mut::<DiffPane, _>(DIFF_ID, |pane| {
			pane.set_empty_message(empty);
			match (selected, contents) {
				(None, _) => pane.set_document(None, DiffPaneState::Empty),
				(_, None) => pane.set_document(None, DiffPaneState::Loading),
				(Some((_, path)), Some(contents)) if contents.binary => {
					pane.set_document(None, DiffPaneState::Binary);
					let _ = path;
				},
				(Some((_, path)), Some(contents)) if contents.too_large => {
					pane.set_document(None, DiffPaneState::TooLarge);
					let _ = path;
				},
				(Some((_, path)), Some(contents)) => {
					let options =
						DiffBuildOptions { ignore_whitespace: ignore_ws, language: None };
					let document = DiffDocument::build(
						contents.old_text.as_str(),
						contents.new_text.as_str(),
						path.as_str(),
						&options,
					);
					pane.set_document(Some(document), DiffPaneState::Ready);
				},
			}
		});
	}

	fn commit_enabled(&self) -> bool {
		!self.summary.as_str().trim().is_empty()
			&& (!self.snapshot.staged.is_empty()
				|| !self.snapshot.unstaged.is_empty()
				|| (self.amend && self.snapshot.head.is_some()))
	}

	fn commit_message(&self) -> Str {
		let summary = self.summary.as_str().trim();
		let body = self.description.as_str().trim();
		if body.is_empty() {
			return summary.to_str();
		}
		sf!("{summary}\n\n{body}")
	}

	fn rebuild(&mut self) {
		let selected_target = self.current_sidebar_target().cloned();
		self.sidebar_rows = sidebar_rows(
			&self.snapshot,
			self.tree,
			&self.collapsed,
			self.amend,
			self.summary.as_str(),
			self.description.as_str(),
			self.commit_enabled(),
			&self.ctx,
		);
		self.sidebar_selected = selected_target
			.and_then(|target| {
				self
					.sidebar_rows
					.iter()
					.position(|row| row.target == target)
			})
			.unwrap_or_else(|| {
				self
					.selected
					.as_ref()
					.and_then(|(area, path)| {
						self.sidebar_rows.iter().position(|row| {
							row.target == SidebarTarget::File { area: *area, path: path.clone() }
						})
					})
					.unwrap_or(0)
			})
			.min(self.sidebar_rows.len().saturating_sub(1));
		let (old_mode, old_wrap) = self
			.ui
			.with_component_mut::<DiffPane, _>(DIFF_ID, |pane| (pane.mode(), pane.wraps()))
			.unwrap_or_default();
		let mut pane = DiffPane::new()
			.with(Prop::Id, DIFF_ID)
			.with(Prop::H, self.height.saturating_sub(2).max(1));
		pane.set_mode(old_mode);
		if old_wrap {
			pane.toggle_wrap();
		}
		pane.set_patch_target(self.patch_target());
		let sidebar_width = (self.width * 3 / 10).clamp(SIDEBAR_MIN, SIDEBAR_MAX);
		let path = self.selected.as_ref().map_or("", |(_, path)| path.as_str());
		let (directory, basename) = split_path(path);
		let counts = self.current_counts();
		let status = self.status.as_ref().map(|(message, failed, _)| {
			if *failed {
				sf!("error: {message}")
			} else {
				message.clone()
			}
		});
		let center = status.unwrap_or_else(|| {
			Str::new_static("Tab focus · n/p hunks · s/u stage · x discard · q close")
		});
		let action_id = match self.selected.as_ref().map(|(area, _)| area) {
			Some(GitArea::Unstaged) => "git-stage-file",
			Some(GitArea::Staged) => "git-unstage-file",
			_ => "git-no-action",
		};
		let action_label = match action_id {
			"git-stage-file" => "Stage File",
			"git-unstage-file" => "Unstage File",
			_ => "",
		};
		let scope = self.scope_label();
		let file_count = if self.snapshot.pinned
			|| (self.snapshot.unstaged.is_empty() && self.snapshot.staged.is_empty())
		{
			self
				.snapshot
				.head
				.as_ref()
				.map_or(0, |head| head.files.len())
		} else {
			self.snapshot.unstaged.len() + self.snapshot.staged.len()
		};
		let branch = self.snapshot.branch.as_deref().unwrap_or("HEAD");
		let sidebar_header =
			sf!("{file_count} file changes on {} {branch}", self.ctx.charset.icon(Icon::Branch));
		let up = self.ctx.charset.icon(Icon::Up);
		let down = self.ctx.charset.icon(Icon::Down);
		let file_icon = self.ctx.charset.icon(Icon::File);
		let diff_icon = self.ctx.charset.icon(Icon::Diff);
		let close = self.ctx.charset.icon(Icon::Close);
		let header_left = sf!("{directory}{basename}  +{} -{}", counts.0, counts.1);
		let toolbar_file = sf!("{file_icon} file");
		let toolbar_split = sf!("{diff_icon} split");
		let labels = self.sidebar_rows.clone();
		let selected = self.sidebar_selected;
		let center_rows = self.height.saturating_sub(2).max(1);
		let root = dom! {
			<col>
				<row h=1 bg=surface>
					<pre fg=fg bold grow truncate>{header_left}</pre>
					<pre fg=muted truncate>{center}</pre>
					<pre fg=muted>{" UTF-8 "}</pre>
					<button id={action_id}>{action_label}</button>
					<button id="git-close">{close}</button>
				</row>
				<row h=1 bg=surface gap=1>
					<pre fg=muted>{scope}</pre>
					<spacer grow />
					<button id="git-up">{up}</button>
					<button id="git-down">{down}</button>
					<button id="git-view-file">{toolbar_file}</button>
					<button id="git-view-split">{toolbar_split}</button>
					<button id="git-view-inline">{"inline"}</button>
					<button id="git-view-hunk">{"hunk"}</button>
					<spacer grow />
					<button id="git-ws">{"whitespace"}</button>
					<button id="git-wrap">{"wrap"}</button>
				</row>
				<row h={center_rows}>
					{pane}
					<pre fg=accent>{"│"}</pre>
					<col w={sidebar_width}>
						<pre h=1 fg=fg bold truncate>{sidebar_header}</pre>
						<row h=1>
							<button id="git-path">{"Path"}</button>
							<button id="git-tree">{"Tree"}</button>
						</row>
						<select id={SIDEBAR_ID} grow>
							for (index, row) in labels.iter().enumerate() {
								<option value={sf!("{index}")} label={row.label.clone()} recommended={index == selected} />
							}
						</select>
					</col>
				</row>
			</col>
		};
		self.ui = Ui::from_root(root, self.width.max(1), self.ctx.clone());
		self.ui.focus_first();
		let tabs = match self.focus {
			Focus::Diff => 10,
			Focus::Sidebar => 13,
		};
		for _ in 0..tabs {
			let _ = self.ui.handle_key(Key::Tab);
		}
		self.install_document();
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

	fn current_counts(&self) -> (u64, u64) {
		self
			.selected
			.as_ref()
			.and_then(|(area, path)| find_file(&self.snapshot, *area, path.as_str()))
			.map_or((0, 0), |file| (file.additions.unwrap_or(0), file.deletions.unwrap_or(0)))
	}

	fn scope_label(&self) -> Str {
		match self.selected.as_ref().map(|(area, _)| area) {
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

fn document_ranges(document: &DiffDocument) -> ((u32, u32), (u32, u32)) {
	let old = document
		.rows
		.iter()
		.filter_map(|row| row.old.as_ref().map(|line| line.number));
	let new = document
		.rows
		.iter()
		.filter_map(|row| row.new.as_ref().map(|line| line.number));
	(range_of(old), range_of(new))
}

fn pop_char(text: &mut StrMut) {
	if let Some((index, _)) = text.as_str().char_indices().next_back() {
		text.truncate(index);
	}
}

fn range_of(mut lines: impl Iterator<Item = u32>) -> (u32, u32) {
	let Some(first) = lines.next() else {
		return (0, 0);
	};
	lines.fold((first, first), |(start, end), line| (start.min(line), end.max(line)))
}

const fn inclusive_range((start, count): (u32, u32)) -> (u32, u32) {
	if count == 0 {
		(0, 0)
	} else {
		(start, start.saturating_add(count).saturating_sub(1))
	}
}

fn split_path(path: &str) -> (&str, &str) {
	path
		.rsplit_once('/')
		.map_or(("", path), |(directory, basename)| (&path[..directory.len() + 1], basename))
}

fn short_sha(sha: &Str) -> Str {
	let end = sha.len().min(8);
	sha.slice(..end)
}

fn sidebar_rows(
	snapshot: &GitSnapshot,
	tree: bool,
	collapsed: &BTreeSet<Str>,
	amend: bool,
	summary: &str,
	description: &str,
	commit_enabled: bool,
	ctx: &UiContext,
) -> Vec<SidebarRow> {
	let mut rows = Vec::new();
	let clean = snapshot.pinned || (snapshot.unstaged.is_empty() && snapshot.staged.is_empty());
	if clean {
		if let Some(head) = &snapshot.head {
			rows.push(SidebarRow {
				target: SidebarTarget::Info(Str::new_static("subject")),
				label:  sf!("{} {}", ctx.charset.icon(Icon::Commit), head.subject),
			});
			for (index, line) in head.body.lines().take(8).enumerate() {
				rows.push(SidebarRow {
					target: SidebarTarget::Info(sf!("body-{index}")),
					label:  line.to_str(),
				});
			}
			for (index, line) in identicon_lines(head.author_email.as_str(), ctx)
				.into_iter()
				.enumerate()
			{
				rows.push(SidebarRow {
					target: SidebarTarget::Info(sf!("avatar-{index}")),
					label:  line,
				});
			}
			rows.push(SidebarRow {
				target: SidebarTarget::Info(Str::new_static("author")),
				label:  sf!("{} <{}>", head.author_name, head.author_email),
			});
			rows.push(SidebarRow {
				target: SidebarTarget::Info(Str::new_static("date")),
				label:  head.author_date.clone(),
			});
			if !head.parents.is_empty() {
				let parents =
					head
						.parents
						.iter()
						.map(short_sha)
						.fold(String::new(), |mut out, parent| {
							if !out.is_empty() {
								out.push(' ');
							}
							out.push_str(parent.as_str());
							out
						});
				rows.push(SidebarRow {
					target: SidebarTarget::Info(Str::new_static("parents")),
					label:  sf!("Parents {parents}"),
				});
			}
			let additions = head
				.files
				.iter()
				.map(|file| file.additions.unwrap_or(0))
				.sum::<u64>();
			let deletions = head
				.files
				.iter()
				.map(|file| file.deletions.unwrap_or(0))
				.sum::<u64>();
			rows.push(SidebarRow {
				target: SidebarTarget::Info(Str::new_static("summary")),
				label:  sf!(
					"{} modified +{additions} -{deletions} · {}",
					head.files.len(),
					short_sha(&head.sha)
				),
			});
			let files = head
				.files
				.iter()
				.cloned()
				.map(|file| (GitArea::Commit, file))
				.collect::<Vec<_>>();
			append_files(&mut rows, &files, tree, collapsed, ctx);
		}
		return rows;
	}
	rows.push(SidebarRow {
		target: SidebarTarget::StageAll,
		label:  sf!("Unstaged Files ({}) · Stage All", snapshot.unstaged.len()),
	});
	let unstaged = snapshot
		.unstaged
		.iter()
		.cloned()
		.map(|file| (GitArea::Unstaged, file))
		.collect::<Vec<_>>();
	append_files(&mut rows, &unstaged, tree, collapsed, ctx);
	rows.push(SidebarRow {
		target: SidebarTarget::UnstageAll,
		label:  sf!("Staged Files ({}) · Unstage All", snapshot.staged.len()),
	});
	let staged = snapshot
		.staged
		.iter()
		.cloned()
		.map(|file| (GitArea::Staged, file))
		.collect::<Vec<_>>();
	append_files(&mut rows, &staged, tree, collapsed, ctx);
	rows.push(SidebarRow {
		target: SidebarTarget::Amend,
		label:  if amend {
			Str::new_static("[x] Amend HEAD")
		} else {
			Str::new_static("[ ] Amend HEAD")
		},
	});
	rows.push(SidebarRow {
		target: SidebarTarget::Summary,
		label:  sf!("Commit summary ({}) · {summary}", 72isize - summary.chars().count() as isize),
	});
	rows.push(SidebarRow {
		target: SidebarTarget::Description,
		label:  sf!("Description · {description}"),
	});
	rows.push(SidebarRow {
		target: SidebarTarget::Commit,
		label:  if !commit_enabled {
			Str::new_static("Commit staged changes [disabled]")
		} else if snapshot.staged.is_empty() {
			Str::new_static("Stage all & commit")
		} else {
			Str::new_static("Commit staged changes")
		},
	});
	rows
}

fn append_files(
	rows: &mut Vec<SidebarRow>,
	files: &[(GitArea, GitFileRow)],
	tree: bool,
	collapsed: &BTreeSet<Str>,
	ctx: &UiContext,
) {
	if !tree {
		for (area, file) in files {
			rows.push(file_sidebar_row(*area, file, 0));
		}
		return;
	}
	let mut root = TreeNode::default();
	for (area, file) in files {
		let mut node = &mut root;
		let mut parts = file.path.as_str().split('/').peekable();
		while let Some(part) = parts.next() {
			if parts.peek().is_none() {
				node.files.push((*area, file.clone()));
			} else {
				node = node.children.entry(part.to_str()).or_default();
			}
		}
	}
	append_tree(rows, &root, "", 0, collapsed, ctx);
}

fn append_tree(
	rows: &mut Vec<SidebarRow>,
	node: &TreeNode,
	prefix: &str,
	depth: usize,
	collapsed: &BTreeSet<Str>,
	ctx: &UiContext,
) {
	for (name, child) in &node.children {
		let mut path = if prefix.is_empty() {
			name.clone()
		} else {
			sf!("{prefix}/{name}")
		};
		let mut compressed = name.clone();
		let mut current = child;
		while current.files.is_empty() && current.children.len() == 1 {
			let (next, next_node) = current.children.first_key_value().expect("one child");
			compressed = sf!("{compressed}/{next}");
			path = sf!("{path}/{next}");
			current = next_node;
		}
		let closed = collapsed.contains(&path);
		let marker = if closed {
			ctx.charset.icon(Icon::Right)
		} else {
			ctx.charset.icon(Icon::Down)
		};
		rows.push(SidebarRow {
			target: SidebarTarget::Directory(path.clone()),
			label:  sf!("{}{marker} {compressed}/", "  ".repeat(depth)),
		});
		if !closed {
			append_tree(rows, current, path.as_str(), depth + 1, collapsed, ctx);
			for (area, file) in &current.files {
				rows.push(file_sidebar_row(*area, file, depth + 1));
			}
		}
	}
	for (area, file) in &node.files {
		rows.push(file_sidebar_row(*area, file, depth));
	}
}

fn file_sidebar_row(area: GitArea, file: &GitFileRow, depth: usize) -> SidebarRow {
	let status = match file.kind {
		GitChangeKind::Modified => "M",
		GitChangeKind::Added => "A",
		GitChangeKind::Deleted => "D",
		GitChangeKind::Renamed => "R",
		GitChangeKind::Untracked => "?",
		GitChangeKind::Conflicted => "U",
	};
	let (directory, basename) = split_path(file.path.as_str());
	let counts = match (file.additions, file.deletions) {
		(Some(additions), Some(deletions)) => sf!(" +{additions} -{deletions}"),
		(Some(additions), None) => sf!(" +{additions}"),
		(None, Some(deletions)) => sf!(" -{deletions}"),
		(None, None) => Str::default(),
	};
	SidebarRow {
		target: SidebarTarget::File { area, path: file.path.clone() },
		label:  sf!("{}{status} {directory}{basename}{counts}", "  ".repeat(depth)),
	}
}

/// Builds deterministic mirrored 5×5 identicon rows for an email address.
pub fn identicon_lines(email: &str, ctx: &UiContext) -> [Str; 3] {
	let lowercase = email.to_ascii_lowercase();
	let hash = xxh3_64(lowercase.as_bytes());
	let upper = ctx.charset.icon(Icon::UpperHalf);
	let lower = ctx.charset.icon(Icon::LowerHalf);
	let full = ctx.charset.icon(Icon::Block);
	let blank = " ";
	let bit = |row: usize, col: usize| {
		let mirrored = if col > 2 { 4 - col } else { col };
		(hash >> (row * 3 + mirrored)) & 1 != 0
	};
	std::array::from_fn(|pair| {
		let top = pair * 2;
		let bottom = top + 1;
		let mut line = String::with_capacity(5 * upper.len());
		for col in 0..5 {
			line.push_str(match (bit(top, col), bottom < 5 && bit(bottom, col)) {
				(true, true) => full,
				(true, false) => upper,
				(false, true) => lower,
				(false, false) => blank,
			});
		}
		line.into_str()
	})
}
#[cfg(test)]
mod tests {
	use omp_core::Str;
	use omp_tui::{DiffActionKind, DiffTarget, Key, UiContext};

	use super::{
		Focus, GitArea, GitChangeKind, GitCommitInfo, GitFileContents, GitFileRow, GitIntent,
		GitPatchOp, GitSnapshot, GitUpdate, GitWorkbench, GitWorkbenchEvent, SidebarTarget,
		identicon_lines,
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
			author_date:  Str::new_static("today"),
			parents:      vec![Str::new_static("parent")],
			files:        vec![file("src/old.rs", GitArea::Commit)],
		}
	}

	fn dirty() -> GitSnapshot {
		GitSnapshot {
			branch:   Some(Str::new_static("main")),
			unstaged: vec![
				file("src/a.rs", GitArea::Unstaged),
				file("src/deep/one/b.rs", GitArea::Unstaged),
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
		}
	}

	#[test]
	fn sidebar_target_order_places_actions_files_then_commit_form() {
		let mut workbench = GitWorkbench::open(dirty(), &UiContext::default());
		let targets = workbench
			.sidebar_rows
			.iter()
			.map(|row| &row.target)
			.collect::<Vec<_>>();
		assert_eq!(targets.first(), Some(&&SidebarTarget::StageAll));
		assert!(matches!(targets[1], SidebarTarget::File { area: GitArea::Unstaged, .. }));
		assert!(
			targets
				.iter()
				.any(|target| **target == SidebarTarget::UnstageAll)
		);
		assert_eq!(targets[targets.len() - 4], &SidebarTarget::Amend);
		assert_eq!(targets[targets.len() - 1], &SidebarTarget::Commit);
		workbench.tree = true;
		workbench.rebuild();
		assert!(workbench.sidebar_rows.iter().any(|row| {
			matches!(&row.target, SidebarTarget::Directory(path) if path.as_str() == "src/deep/one")
		}));
	}

	#[test]
	fn tree_compresses_single_children_and_honors_collapse() {
		let mut workbench = GitWorkbench::open(dirty(), &UiContext::default());
		workbench.tree = true;
		workbench.rebuild();
		let directory = Str::new_static("src/deep/one");
		assert!(workbench.sidebar_rows.iter().any(|row| {
			row.target == SidebarTarget::Directory(directory.clone())
				&& row.label.as_str().contains("deep/one")
		}));
		workbench.collapsed.insert(directory);
		workbench.rebuild();
		assert!(!workbench.sidebar_rows.iter().any(|row| {
			matches!(&row.target, SidebarTarget::File { path, .. } if path.as_str() == "src/deep/one/b.rs")
		}));
	}

	#[test]
	fn amend_prefills_and_successful_commit_clears_form() {
		let mut workbench = GitWorkbench::open(dirty(), &UiContext::default());
		workbench.focus = Focus::Sidebar;
		workbench.sidebar_selected = workbench
			.sidebar_rows
			.iter()
			.position(|row| row.target == SidebarTarget::Amend)
			.unwrap();
		assert_eq!(workbench.handle_key(Key::Enter), GitWorkbenchEvent::Consumed);
		assert_eq!(workbench.summary.as_str(), "existing subject");
		assert_eq!(workbench.description.as_str(), "existing body");
		assert!(workbench.commit_enabled());
		workbench.sidebar_selected = workbench
			.sidebar_rows
			.iter()
			.position(|row| row.target == SidebarTarget::Commit)
			.unwrap();
		assert!(matches!(
			workbench.handle_key(Key::Enter),
			GitWorkbenchEvent::Intent(GitIntent::Commit { amend: true, .. })
		));
		workbench.apply(GitUpdate::ActionDone { message: Str::new_static("committed") });
		assert!(workbench.summary.is_empty());
		assert!(workbench.description.is_empty());
	}

	#[test]
	fn snapshot_reconcile_preserves_identity_or_selects_first() {
		let mut workbench = GitWorkbench::open(dirty(), &UiContext::default());
		workbench.selected = Some((GitArea::Staged, Str::new_static("tests/a.rs")));
		let mut replacement = dirty();
		replacement.unstaged.remove(0);
		assert_eq!(workbench.apply(GitUpdate::Snapshot(replacement)), None);
		assert_eq!(workbench.selected, Some((GitArea::Staged, Str::new_static("tests/a.rs"))));
		let replacement = GitSnapshot {
			branch:   Some(Str::new_static("main")),
			unstaged: vec![file("new.rs", GitArea::Unstaged)],
			staged:   Vec::new(),
			head:     Some(head()),
			pinned:   false,
		};
		assert!(matches!(
			workbench.apply(GitUpdate::Snapshot(replacement)),
			Some(GitIntent::Load { path, .. }) if path.as_str() == "new.rs"
		));
	}

	#[test]
	fn diff_action_keys_respect_area_and_discard_confirmation() {
		let mut workbench = GitWorkbench::open(dirty(), &UiContext::default());
		let GitIntent::Load { seq, .. } = workbench.initial_intent().unwrap() else {
			panic!("initial file load")
		};
		workbench.apply(GitUpdate::Contents { seq, contents: contents("old\n", "new\n") });
		assert_eq!(workbench.handle_key(Key::SelectDown), GitWorkbenchEvent::Consumed);
		let explicit = workbench
			.ui
			.with_component_mut::<omp_tui::DiffPane, _>("git-diff", |pane| {
				pane.selection().is_some_and(|selection| selection.explicit)
			})
			.unwrap();
		assert!(explicit, "diff focus routes shift-selection into DiffPane");
		assert!(matches!(
			workbench.handle_key(Key::Char('s')),
			GitWorkbenchEvent::Intent(GitIntent::ApplyLines {
				op: GitPatchOp::Stage,
				path,
				..
			}) if path.as_str() == "src/a.rs"
		));
		assert_eq!(
			workbench
				.map_diff_action(DiffActionKind::Stage, DiffTarget::Lines { old: (1, 1), new: (1, 1) },),
			GitWorkbenchEvent::Intent(GitIntent::ApplyLines {
				op:   GitPatchOp::Stage,
				path: Str::new_static("src/a.rs"),
				old:  (1, 1),
				new:  (1, 1),
			})
		);
		assert_eq!(
			workbench.map_diff_action(DiffActionKind::Stage, DiffTarget::File),
			GitWorkbenchEvent::Intent(GitIntent::StageFile(Some(Str::new_static("src/a.rs"))))
		);
		assert!(matches!(
			workbench.map_diff_action(DiffActionKind::Stage, DiffTarget::Hunk(0)),
			GitWorkbenchEvent::Intent(GitIntent::ApplyLines { op: GitPatchOp::Stage, .. })
		));
		assert_eq!(workbench.handle_key(Key::Char('x')), GitWorkbenchEvent::Consumed);
		assert!(matches!(
			workbench.handle_key(Key::Char('x')),
			GitWorkbenchEvent::Intent(GitIntent::ApplyLines { op: GitPatchOp::Discard, .. })
		));
		workbench.selected = Some((GitArea::Staged, Str::new_static("tests/a.rs")));
		workbench.contents = Some(contents("old\n", "new\n"));
		workbench.rebuild();
		assert!(matches!(
			workbench.handle_key(Key::Char('u')),
			GitWorkbenchEvent::Intent(GitIntent::UnstageFile(Some(path)))
				if path.as_str() == "tests/a.rs"
		));
		assert_eq!(workbench.handle_key(Key::Char('s')), GitWorkbenchEvent::Consumed);
	}

	#[test]
	fn stale_contents_sequence_is_ignored() {
		let mut workbench = GitWorkbench::open(dirty(), &UiContext::default());
		let _ = workbench.initial_intent();
		let current = workbench.load_seq;
		workbench.apply(GitUpdate::Contents {
			seq:      current.saturating_sub(1),
			contents: contents("", "stale"),
		});
		assert!(workbench.contents.is_none());
		workbench.apply(GitUpdate::Contents { seq: current, contents: contents("", "fresh") });
		assert_eq!(workbench.contents.as_ref().unwrap().new_text.as_str(), "fresh");
	}

	#[test]
	fn identicon_is_case_folded_deterministic_and_mirrored() {
		let ctx = UiContext::default();
		let first = identicon_lines("Ada@Example.COM", &ctx);
		assert_eq!(first, identicon_lines("ada@example.com", &ctx));
		assert_eq!(first, identicon_lines("Ada@Example.COM", &ctx));
		for line in first {
			let cells = line.chars().collect::<Vec<_>>();
			assert_eq!(cells.first(), cells.last());
			assert_eq!(cells.get(1), cells.get(3));
		}
	}
}
