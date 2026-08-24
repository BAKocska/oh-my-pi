//! Git workbench sidebar state, file tree, and commit composer.

use std::collections::{BTreeMap, BTreeSet};

use omp_core::{IntoStr, Str, sf};
use omp_tui::{
	Color, Prop, UiContext, cell_width,
	components::{Col, EditInput, EditorPane, Tree, TreeAnnotation, TreeNode},
	dom,
};

use super::{GitArea, GitChangeKind, GitFileRow, GitSnapshot, GitWorkbench, split_path};

pub(super) const SIDEBAR_ID: &str = "git-sidebar";
pub(super) const SUMMARY_ID: &str = "git-commit-summary";
pub(super) const DESCRIPTION_ID: &str = "git-commit-description";
// Keep the shell lookup separate from its focusable, value-owning editor leaf.
pub(super) const DESCRIPTION_PANE_ID: &str = "git-commit-description-pane";
pub(super) const AMEND_ID: &str = "git-amend";
pub(super) const COMMIT_ID: &str = "git-commit";
pub(super) const VIEW_STYLE_ID: &str = "git-sidebar-view";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SidebarTarget {
	ViewStyle,
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
}

#[derive(Default)]
struct FileTreeNode {
	files:    Vec<(GitArea, GitFileRow)>,
	children: BTreeMap<Str, FileTreeNode>,
}

impl SidebarTarget {
	pub(super) fn key(&self) -> Str {
		match self {
			Self::ViewStyle => Str::new_static("sidebar-view"),
			Self::StageAll => Str::new_static("unstaged-section"),
			Self::UnstageAll => Str::new_static("staged-section"),
			Self::Directory { area, path, .. } => sf!("dir:{area:?}:{path}"),
			Self::File { area, path, .. } => sf!("file:{area:?}:{path}"),
			Self::Amend => Str::new_static("amend"),
			Self::Summary => Str::new_static("summary"),
			Self::Description => Str::new_static("description"),
			Self::Commit => Str::new_static("commit"),
		}
	}

	pub(super) const fn is_tree_node(&self) -> bool {
		matches!(
			self,
			Self::StageAll | Self::UnstageAll | Self::Directory { .. } | Self::File { .. }
		)
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
		self.sidebar_rows = sidebar_rows(&self.snapshot, self.tree, &self.ctx);
	}

	pub(super) fn sidebar_component(
		&self,
		width: u16,
		summary: &str,
		description: &str,
		selected_key: Option<&str>,
		scroll_top: usize,
	) -> Col {
		let mut tree = sidebar_tree(&self.sidebar_rows, &self.collapsed);
		let content_rows = self.height.saturating_sub(2).max(1);
		let tree_rows = if self.is_commit_view() {
			self.snapshot.head.as_ref().map_or(1, |head| {
				let text_rows = |text: &str| cell_width(text).div_ceil(width.max(1)).max(1);
				let body_rows = head
					.body
					.lines()
					.take(8)
					.fold(0_u16, |rows, line| rows.saturating_add(text_rows(line)));
				let metadata_rows = text_rows(head.subject.as_str())
					.saturating_add(9)
					.saturating_add(u16::from(body_rows > 0).saturating_add(body_rows))
					.saturating_add(u16::from(!head.parents.is_empty()));
				content_rows.saturating_sub(metadata_rows).max(1)
			})
		} else {
			let description_rows =
				u16::try_from(description.lines().count().clamp(1, 5)).unwrap_or(5);
			content_rows
				.saturating_sub(7_u16.saturating_add(description_rows))
				.max(1)
		};
		tree = tree.with(Prop::H, tree_rows);
		if let Some(key) = selected_key {
			let _ = tree.select_key(key);
		}
		tree.set_scroll_top(scroll_top);
		if self.is_commit_view() {
			return super::commit_view::component(
				self.snapshot.head.as_ref(),
				self.avatar.as_ref().map(|(_, bytes)| bytes.clone()),
				&self.ctx,
				tree,
				self.tree,
				width,
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
				{tree}
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

pub(super) fn sidebar_rows(snapshot: &GitSnapshot, tree: bool, ctx: &UiContext) -> Vec<SidebarRow> {
	let mut rows = Vec::new();
	if snapshot.pinned || (snapshot.unstaged.is_empty() && snapshot.staged.is_empty()) {
		if let Some(head) = &snapshot.head {
			let files = head
				.files
				.iter()
				.cloned()
				.map(|file| (GitArea::Commit, file))
				.collect::<Vec<_>>();
			append_files(&mut rows, &files, tree, ctx);
		}
		rows.push(action_row(SidebarTarget::ViewStyle, Str::default()));
		return rows;
	}
	rows.push(action_row(
		SidebarTarget::StageAll,
		sf!("Unstaged Files ({})", snapshot.unstaged.len()),
	));
	let unstaged = snapshot
		.unstaged
		.iter()
		.cloned()
		.map(|file| (GitArea::Unstaged, file))
		.collect::<Vec<_>>();
	append_files(&mut rows, &unstaged, tree, ctx);
	rows.push(action_row(
		SidebarTarget::UnstageAll,
		sf!("Staged Files ({})", snapshot.staged.len()),
	));
	let staged = snapshot
		.staged
		.iter()
		.cloned()
		.map(|file| (GitArea::Staged, file))
		.collect::<Vec<_>>();
	append_files(&mut rows, &staged, tree, ctx);
	rows.push(action_row(SidebarTarget::ViewStyle, Str::default()));
	rows.push(action_row(SidebarTarget::Amend, Str::default()));
	rows.push(action_row(SidebarTarget::Summary, Str::default()));
	rows.push(action_row(SidebarTarget::Description, Str::default()));
	rows.push(action_row(SidebarTarget::Commit, Str::default()));
	rows
}

fn action_row(target: SidebarTarget, basename: Str) -> SidebarRow {
	SidebarRow {
		target,
		status: None,
		status_color: Color::Default,
		directory: Str::default(),
		basename,
		additions: None,
		deletions: None,
	}
}

fn append_files(
	rows: &mut Vec<SidebarRow>,
	files: &[(GitArea, GitFileRow)],
	tree: bool,
	ctx: &UiContext,
) {
	if !tree {
		for (area, file) in files {
			rows.push(file_sidebar_row(*area, file, 0, false, ctx));
		}
		return;
	}
	let mut root = FileTreeNode::default();
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
	append_tree(rows, &root, "", 0, ctx);
}

fn append_tree(
	rows: &mut Vec<SidebarRow>,
	node: &FileTreeNode,
	prefix: &str,
	depth: usize,
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
		rows.push(SidebarRow {
			target: SidebarTarget::Directory { area, path: path.clone(), depth },
			status: None,
			status_color: ctx.theme.muted,
			directory: Str::default(),
			basename: sf!("{compressed}/"),
			additions: None,
			deletions: None,
		});
		append_tree(rows, current, path.as_str(), depth + 1, ctx);
	}
	for (area, file) in &node.files {
		rows.push(file_sidebar_row(*area, file, depth, true, ctx));
	}
}

fn subtree_area(node: &FileTreeNode) -> Option<GitArea> {
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
		directory: if tree { Str::default() } else { directory.to_str() },
		basename: basename.to_str(),
		additions: file.additions.filter(|count| *count != 0),
		deletions: file.deletions.filter(|count| *count != 0),
	}
}

fn sidebar_tree(rows: &[SidebarRow], collapsed: &BTreeSet<Str>) -> Tree {
	let mut tree = Tree::new()
		.with(Prop::Id, SIDEBAR_ID)
		.with(Prop::Grow, true);
	let mut index = 0;
	while index < rows.len() {
		match rows[index].target {
			SidebarTarget::StageAll | SidebarTarget::UnstageAll => {
				let section = &rows[index];
				index += 1;
				let children = tree_level(rows, &mut index, 0, collapsed);
				let mut node = row_node(section, collapsed).with(
					Prop::Action,
					if matches!(section.target, SidebarTarget::StageAll) {
						"Stage All"
					} else {
						"Unstage All"
					},
				);
				for child in children {
					node = node.node(child);
				}
				tree = tree.node(node);
			},
			SidebarTarget::Directory { depth: 0, .. } | SidebarTarget::File { depth: 0, .. } => {
				for node in tree_level(rows, &mut index, 0, collapsed) {
					tree = tree.node(node);
				}
			},
			_ => index += 1,
		}
	}
	tree
}

fn tree_level(
	rows: &[SidebarRow],
	index: &mut usize,
	depth: usize,
	collapsed: &BTreeSet<Str>,
) -> Vec<TreeNode> {
	let mut nodes = Vec::new();
	while let Some(row) = rows.get(*index) {
		let Some(row_depth) = row.target.depth() else {
			break;
		};
		if row_depth < depth {
			break;
		}
		if row_depth > depth {
			break;
		}
		*index += 1;
		let mut node = row_node(row, collapsed);
		if matches!(row.target, SidebarTarget::Directory { .. }) {
			for child in tree_level(rows, index, depth + 1, collapsed) {
				node = node.node(child);
			}
		}
		nodes.push(node);
	}
	nodes
}

fn row_node(row: &SidebarRow, collapsed: &BTreeSet<Str>) -> TreeNode {
	let key = row.target.key();
	let mut node = TreeNode::new().key(key.clone()).label(row.basename.clone());
	match row.target {
		SidebarTarget::StageAll | SidebarTarget::UnstageAll => {
			node = node
				.with(Prop::Open, !collapsed.contains(&key))
				.with(Prop::Bold, true)
				.with(Prop::ActionColor, "accent");
		},
		SidebarTarget::Directory { .. } => {
			node = node
				.with(Prop::Open, !collapsed.contains(&key))
				.with(Prop::Dim, true);
		},
		SidebarTarget::File { .. } => {
			if let Some(status) = &row.status {
				node = node.badge(status.clone()).with(Prop::Color, row.status_color);
			}
			if !row.directory.is_empty() {
				node = node.prefix(row.directory.clone());
			}
			if let Some(additions) = row.additions {
				node = node.annotate(TreeAnnotation::new(sf!("+{additions}")).color("ok"));
			}
			if let Some(deletions) = row.deletions {
				node = node.annotate(TreeAnnotation::new(sf!("−{deletions}")).color("err"));
			}
		},
		_ => {},
	}
	node
}
