//! Git workbench sidebar state, file tree, and commit composer.

use std::collections::{BTreeMap, BTreeSet};

use omp_core::{IntoStr, Str, sf};
use omp_tui::{
	Color, Prop, UiContext,
	components::{Col, EditInput, EditorPane},
	dom,
};
use xutf::Text as _;

use super::{GitArea, GitChangeKind, GitFileRow, GitSnapshot, GitWorkbench, split_path};

pub(super) const SUMMARY_ID: &str = "git-commit-summary";
pub(super) const DESCRIPTION_ID: &str = "git-commit-description";
// Keep the shell lookup separate from its focusable, value-owning editor leaf.
pub(super) const DESCRIPTION_PANE_ID: &str = "git-commit-description-pane";
pub(super) const AMEND_ID: &str = "git-amend";
pub(super) const COMMIT_ID: &str = "git-commit";
pub(super) const VIEW_STYLE_ID: &str = "git-sidebar-view";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SidebarTarget {
	StageAll,
	UnstageAll,
	Directory { area: GitArea, path: Str, depth: usize },
	File { area: GitArea, path: Str, depth: usize },
	Amend,
	Summary,
	Description,
	Commit,
}

#[derive(Clone)]
pub(super) struct SidebarRow {
	pub target:       SidebarTarget,
	pub status:       Option<Str>,
	pub status_color: Color,
	pub directory:    Str,
	pub basename:     Str,
	pub additions:    Option<u64>,
	pub deletions:    Option<u64>,
	pub collapsed:    bool,
}

#[derive(Default)]
struct TreeNode {
	files:    Vec<(GitArea, GitFileRow)>,
	children: BTreeMap<Str, TreeNode>,
}

impl SidebarTarget {
	pub(super) fn key(&self) -> Str {
		match self {
			Self::StageAll => Str::new_static("stage-all"),
			Self::UnstageAll => Str::new_static("unstage-all"),
			Self::Directory { area, path, .. } => sf!("dir:{area:?}:{path}"),
			Self::File { area, path, .. } => sf!("file:{area:?}:{path}"),
			Self::Amend => Str::new_static("amend"),
			Self::Summary => Str::new_static("summary"),
			Self::Description => Str::new_static("description"),
			Self::Commit => Str::new_static("commit"),
		}
	}

	pub(super) const fn is_file_or_directory(&self) -> bool {
		matches!(self, Self::Directory { .. } | Self::File { .. })
	}

	pub(super) const fn depth(&self) -> Option<usize> {
		match self {
			Self::Directory { depth, .. } | Self::File { depth, .. } => Some(*depth),
			_ => None,
		}
	}
}

impl GitWorkbench {
	pub(super) fn rebuild_sidebar_rows(&mut self) {
		self.sidebar_rows = sidebar_rows(&self.snapshot, self.tree, &self.collapsed, &self.ctx);
	}

	pub(super) fn sidebar_component(&self, width: u16, summary: &str, description: &str) -> Col {
		let selected = self.sidebar_selected;
		let selection_bg = self.ctx.theme.selection_bg(false);
		let visible = self.sidebar_visible_rows(description);
		let start = self.sidebar_scroll_top;
		let end = start
			.saturating_add(visible)
			.min(self.sidebar_file_row_count());
		let rows = &self.sidebar_rows[start..end];
		if self.is_commit_view() {
			return super::commit_view::component(
				self.snapshot.head.as_ref(),
				self.avatar.as_ref().map(|(_, bytes)| bytes.clone()),
				&self.ctx,
				rows,
				selected,
				selection_bg,
				self.tree,
				width,
				start,
			);
		}
		let file_count = self.snapshot.unstaged.len() + self.snapshot.staged.len();
		let change_word = if file_count == 1 { "change" } else { "changes" };
		let branch = self.snapshot.branch.as_deref().unwrap_or("HEAD");
		let view = if self.tree { "tree" } else { "path" };
		let amend = self.amend;
		let disabled = !self.commit_enabled_with(summary);
		let commit_label = self.commit_button_label();
		let commit_text =
			sf!("{} {commit_label}", self.ctx.charset.icon_named("commit-node").unwrap_or(""));
		let description_editor = EditorPane::new()
			.with(Prop::Id, DESCRIPTION_PANE_ID)
			.input(
				EditInput::new()
					.with(Prop::Id, DESCRIPTION_ID)
					.with(Prop::Value, description)
					.with(Prop::Rail, true)
					.with(Prop::Placeholder, "Description")
					.with(Prop::MaxRows, 5_u16),
			);
		let rendered_rows = rows.iter().enumerate().map(|(offset, row)| {
			let index = start + offset;
			let (directory, basename) = fit_sidebar_path(row, width);
			(index, row, directory, basename)
		});
		dom! {
			<col w={width}>
				<row h=1 gap=1>
					<text bold truncate grow>{sf!("{file_count} file {change_word} on")}</text>
					<button variant=tint color=accent active>{branch}</button>
				</row>
				<row h=1 justify=center>
					<segmented id={VIEW_STYLE_ID} value={view}>
						<option value="path" icon="view-path" label="Path"/>
						<option value="tree" icon="view-tree" label="Tree"/>
					</segmented>
				</row>
				<hr fg=border/>
				<col h={u16::try_from(visible).unwrap_or(u16::MAX)}>
					for (index, row, directory, basename) in rendered_rows {
						if matches!(row.target, SidebarTarget::StageAll) {
							<row w={width} h=1 bg={if index == selected { selection_bg } else { Color::Default }}>
								<text bold truncate grow>{sf!("▾ Unstaged Files ({})", self.snapshot.unstaged.len())}</text>
								<button id="git-stage-all" variant=soft active>{"Stage All"}</button>
							</row>
						} else if matches!(row.target, SidebarTarget::UnstageAll) {
							<row w={width} h=1 bg={if index == selected { selection_bg } else { Color::Default }}>
								<text bold truncate grow>{sf!("▾ Staged Files ({})", self.snapshot.staged.len())}</text>
								<button id="git-unstage-all" variant=soft active>{"Unstage All"}</button>
							</row>
						} else if row.target.is_file_or_directory() {
							<row w={width} h=1 bg={if index == selected { selection_bg } else { Color::Default }}>
								<pre fg=accent>{if index == selected { "▎" } else { " " }}</pre>
								<pre>{" ".repeat(row.target.depth().unwrap_or(0))}</pre>
								if let Some(status) = &row.status { <text fg={row.status_color}>{status}</text> }
								if matches!(row.target, SidebarTarget::Directory { .. }) {
									<text fg=muted>{if row.collapsed { "▸" } else { "▾" }}</text>
								}
								<text dim>{directory}</text>
								<button id={sf!("git-sidebar-row-{index}")} variant=ghost grow>
									{basename}
								</button>
								<spacer grow/>
								if let Some(additions) = row.additions { <text fg=ok>{sf!("+{additions}")}</text> }
								if let Some(deletions) = row.deletions { <text fg=err>{sf!("−{deletions}")}</text> }
							</row>
						}
					}
				</col>
				<hr fg=border/>
				<checkbox id={AMEND_ID} checked={amend} label="Amend previous commit"/>
				<input id={SUMMARY_ID} value={summary} limit=72 rail placeholder="Commit summary"/>
				{description_editor}
				<row justify=center>
					<button id={COMMIT_ID} variant=pill color=accent dim={disabled}>{commit_text}</button>
				</row>
			</col>
		}
	}
}

pub(super) fn fit_sidebar_path(row: &SidebarRow, width: u16) -> (Str, Str) {
	let depth = u16::try_from(row.target.depth().unwrap_or(0)).unwrap_or(u16::MAX);
	let status = u16::from(row.status.is_some());
	let chevron = u16::from(matches!(row.target, SidebarTarget::Directory { .. }));
	let counts = row
		.additions
		.map_or(0, |count| decimal_width(count).saturating_add(1))
		.saturating_add(
			row
				.deletions
				.map_or(0, |count| decimal_width(count).saturating_add(1)),
		);
	let budget = width
		.saturating_sub(1)
		.saturating_sub(depth)
		.saturating_sub(status)
		.saturating_sub(chevron)
		.saturating_sub(counts);
	let basename_width = cell_width(row.basename.as_str());
	if row.directory.is_empty() || basename_width >= budget {
		return (Str::default(), truncate_end(row.basename.as_str(), budget));
	}
	let directory_budget = budget.saturating_sub(basename_width);
	(
		truncate_start(row.directory.as_str(), directory_budget),
		row.basename.clone(),
	)
}

fn truncate_start(text: &str, width: u16) -> Str {
	if cell_width(text) <= width {
		return text.to_str();
	}
	if width == 0 {
		return Str::default();
	}
	let budget = usize::from(width - 1);
	let mut used = 0_usize;
	let mut start = text.len();
	for (offset, grapheme) in text.grapheme_indices().rev() {
		let next = used.saturating_add(grapheme.visible_width());
		if next > budget {
			break;
		}
		used = next;
		start = offset;
	}
	sf!("…{}", &text[start..])
}

fn truncate_end(text: &str, width: u16) -> Str {
	if cell_width(text) <= width {
		return text.to_str();
	}
	if width == 0 {
		return Str::default();
	}
	let budget = usize::from(width - 1);
	let mut used = 0_usize;
	let mut end = 0;
	for (offset, grapheme) in text.grapheme_indices() {
		let next = used.saturating_add(grapheme.visible_width());
		if next > budget {
			break;
		}
		used = next;
		end = offset + grapheme.len();
	}
	sf!("{}…", &text[..end])
}

fn cell_width(text: &str) -> u16 {
	u16::try_from(text.visible_width()).unwrap_or(u16::MAX)
}

const fn decimal_width(mut value: u64) -> u16 {
	let mut width = 1;
	while value >= 10 {
		value /= 10;
		width += 1;
	}
	width
}

pub(super) fn sidebar_rows(
	snapshot: &GitSnapshot,
	tree: bool,
	collapsed: &BTreeSet<Str>,
	ctx: &UiContext,
) -> Vec<SidebarRow> {
	let mut rows = Vec::new();
	if snapshot.pinned || (snapshot.unstaged.is_empty() && snapshot.staged.is_empty()) {
		if let Some(head) = &snapshot.head {
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
	rows.push(action_row(SidebarTarget::StageAll));
	let unstaged = snapshot
		.unstaged
		.iter()
		.cloned()
		.map(|file| (GitArea::Unstaged, file))
		.collect::<Vec<_>>();
	append_files(&mut rows, &unstaged, tree, collapsed, ctx);
	rows.push(action_row(SidebarTarget::UnstageAll));
	let staged = snapshot
		.staged
		.iter()
		.cloned()
		.map(|file| (GitArea::Staged, file))
		.collect::<Vec<_>>();
	append_files(&mut rows, &staged, tree, collapsed, ctx);
	rows.push(action_row(SidebarTarget::Amend));
	rows.push(action_row(SidebarTarget::Summary));
	rows.push(action_row(SidebarTarget::Description));
	rows.push(action_row(SidebarTarget::Commit));
	rows
}

fn action_row(target: SidebarTarget) -> SidebarRow {
	SidebarRow {
		target,
		status: None,
		status_color: Color::Default,
		directory: Str::default(),
		basename: Str::default(),
		additions: None,
		deletions: None,
		collapsed: false,
	}
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
			rows.push(file_sidebar_row(*area, file, 0, false, ctx));
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
		let area = current
			.files
			.first()
			.map_or_else(|| subtree_area(current).unwrap_or(GitArea::Unstaged), |(area, _)| *area);
		let closed = collapsed.contains(&directory_key(area, path.as_str()));
		rows.push(SidebarRow {
			target:       SidebarTarget::Directory { area, path: path.clone(), depth },
			status:       None,
			status_color: ctx.theme.muted,
			directory:    Str::default(),
			basename:     sf!("{compressed}/"),
			additions:    None,
			deletions:    None,
			collapsed:    closed,
		});
		if !closed {
			append_tree(rows, current, path.as_str(), depth + 1, collapsed, ctx);
			}
	}
	for (area, file) in &node.files {
		rows.push(file_sidebar_row(*area, file, depth, true, ctx));
	}
}

fn subtree_area(node: &TreeNode) -> Option<GitArea> {
	node
		.files
		.first()
		.map(|(area, _)| *area)
		.or_else(|| node.children.values().find_map(subtree_area))
}

fn file_sidebar_row(
	area: GitArea,
	file: &GitFileRow,
	depth: usize,
	tree: bool,
	ctx: &UiContext,
) -> SidebarRow {
	let status: &'static str = file.kind.into();
	let status_color = match file.kind {
		GitChangeKind::Modified => ctx.theme.warn,
		GitChangeKind::Added => ctx.theme.ok,
		GitChangeKind::Deleted | GitChangeKind::Conflicted => ctx.theme.err,
		GitChangeKind::Renamed => ctx.theme.accent,
		GitChangeKind::Untracked => ctx.theme.muted,
	};
	let (directory, basename) = split_path(file.path.as_str());
	SidebarRow {
		target: SidebarTarget::File { area, path: file.path.clone(), depth },
		status: Some(status.to_str()),
		status_color,
		directory: if tree {
			Str::default()
		} else {
			directory.to_str()
		},
		basename: basename.to_str(),
		additions: file.additions.filter(|count| *count != 0),
		deletions: file.deletions.filter(|count| *count != 0),
		collapsed: false,
	}
}

pub(super) fn directory_key(area: GitArea, path: &str) -> Str {
	sf!("{area:?}:{path}")
}
