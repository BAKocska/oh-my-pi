//! Interactive projection of the core-owned agent hierarchy.
//!
//! This module owns selection and presentation only. Lifecycle and message
//! actions are returned to the host and must be decided by the backend's
//! `AgentTree` authority.

use omp_core::{Str, sf};
use omp_tui::{
	Dim, Key, Layer, Mouse, OverlayAnchor, OverlayOptions, Prop, Size, Ui, UiContext, UiEvent, dom,
};

use crate::{
	AgentRow,
	overlays::{OverlayPanel, panel_divider},
};

const FRAME_ROWS: u16 = 7;
const WIDE_INSPECTOR: u16 = 72;

/// Action requested from the selected agent row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentHubEvent {
	/// The overlay consumed input and remains open.
	Consumed,
	/// Close the hub and restore root composer focus.
	Close,
	/// Open prompt input for immediate steering.
	Steer(Str),
	/// Ask the backend to revive a cold agent.
	Revive(Str),
	/// Ask the backend to kill a live child agent.
	Kill(Str),
}

/// Whether the left pane presents a flat roster or hierarchy labels.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum HubView {
	Roster,
	#[default]
	Tree,
}

/// Retained responsive Agent Hub overlay.
pub struct AgentHub {
	ui:        Ui,
	rows:      Vec<AgentRow>,
	selected:  usize,
	view:      HubView,
	ctx:       UiContext,
	options:   OverlayOptions,
	list_rows: u16,
	width:     u16,
}

impl AgentHub {
	/// Opens a hub over a snapshot projected from the sole-authority agent tree.
	#[must_use]
	pub fn open(rows: &[AgentRow], ctx: &UiContext) -> Self {
		let rows = rows.to_vec();
		let selected = 0;
		let width = 100;
		let list_rows = 8;
		let ui = build(&rows, selected, HubView::Tree, list_rows, width, ctx);
		let mut hub = Self {
			ui,
			rows,
			selected,
			view: HubView::Tree,
			ctx: ctx.clone(),
			options: OverlayOptions::default()
				.anchor(OverlayAnchor::Bottom)
				.width(Dim::Pct(100))
				.z(10),
			list_rows,
			width,
		};
		hub.ui.focus_first();
		hub.refresh_inspector();
		hub
	}

	/// Replaces the live projection while preserving selection by stable id.
	pub fn update_rows(&mut self, rows: &[AgentRow]) {
		let selected_id = self.rows.get(self.selected).map(|row| row.id.clone());
		self.rows = rows.to_vec();
		self.selected = selected_id
			.as_ref()
			.and_then(|id| self.rows.iter().position(|row| row.id == *id))
			.unwrap_or(0)
			.min(self.rows.len().saturating_sub(1));
		self.rebuild();
	}

	/// Routes keyboard selection, view toggles, and lifecycle requests.
	pub fn handle_key(&mut self, key: Key) -> AgentHubEvent {
		match key {
			Key::Esc => return AgentHubEvent::Close,
			Key::Char('t') => {
				self.view = match self.view {
					HubView::Roster => HubView::Tree,
					HubView::Tree => HubView::Roster,
				};
				self.rebuild();
				return AgentHubEvent::Consumed;
			},
			Key::Char('s') | Key::Enter => {
				return self.capability_event(|row| row.can_steer, AgentHubEvent::Steer);
			},
			Key::Char('r') => {
				return self.capability_event(|row| row.can_revive, AgentHubEvent::Revive);
			},
			Key::Char('k') => return self.capability_event(|row| row.can_kill, AgentHubEvent::Kill),
			_ => {},
		}
		let routed = self.ui.handle_key(key);
		self.route(routed)
	}

	/// Routes pointer selection and outside-click dismissal.
	pub fn handle_mouse(
		&mut self,
		col: u16,
		row: u16,
		kind: Mouse,
		viewport: Size,
	) -> AgentHubEvent {
		match self
			.ui
			.handle_mouse_as_layer(&self.options, viewport, col, row, kind)
		{
			Some(event) => self.route(event),
			None if kind == Mouse::Click => AgentHubEvent::Close,
			None => AgentHubEvent::Consumed,
		}
	}

	/// Returns the responsive bottom-anchored layer.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		let rows = (viewport.height * 3 / 5).saturating_sub(FRAME_ROWS).max(5);
		if rows != self.list_rows || viewport.width != self.width {
			self.list_rows = rows;
			self.width = viewport.width;
			self.rebuild();
		}
		Layer { frame: self.ui.frame(), options: &self.options, active: true }
	}

	fn selected_event(&self, event: impl FnOnce(Str) -> AgentHubEvent) -> AgentHubEvent {
		self
			.rows
			.get(self.selected)
			.map_or(AgentHubEvent::Consumed, |row| event(row.id.clone()))
	}

	fn capability_event(
		&self,
		allowed: impl FnOnce(&AgentRow) -> bool,
		event: impl FnOnce(Str) -> AgentHubEvent,
	) -> AgentHubEvent {
		self
			.rows
			.get(self.selected)
			.filter(|row| allowed(row))
			.map_or(AgentHubEvent::Consumed, |row| event(row.id.clone()))
	}

	fn route(&mut self, event: UiEvent) -> AgentHubEvent {
		match event {
			UiEvent::Cancel => AgentHubEvent::Close,
			UiEvent::Changed { id, value } if id.as_str() == "agent-hub-list" => {
				if let Ok(index) = value.as_str().parse() {
					self.selected = index;
				}
				self.refresh_inspector();
				AgentHubEvent::Consumed
			},
			UiEvent::Highlighted { id, value } if id.as_str() == "agent-hub-list" => {
				if let Ok(index) = value.as_str().parse() {
					self.selected = index;
				}
				self.refresh_inspector();
				AgentHubEvent::Consumed
			},
			_ => AgentHubEvent::Consumed,
		}
	}

	fn rebuild(&mut self) {
		self.ui = build(&self.rows, self.selected, self.view, self.list_rows, self.width, &self.ctx);
		self.ui.focus_first();
		self.refresh_inspector();
	}

	fn refresh_inspector(&mut self) {
		let detail = self.rows.get(self.selected).map_or_else(
			|| sf!("No agents in this session."),
			|row| {
				let tool = row.tool.as_deref().unwrap_or("idle");
				let tokens = row
					.tokens
					.map_or_else(|| sf!("unknown"), |tokens| sf!("{tokens}"));
				let definition = row.definition.as_deref().unwrap_or("native");
				let model = row
					.serving_model
					.as_deref()
					.or(row.model.as_deref())
					.unwrap_or("default");
				sf!(
					"{} · {} · {} · {} · {} tokens\ncurrent: {}\n{}",
					row.name,
					definition,
					model,
					row.status,
					tokens,
					tool,
					row.transcript
				)
			},
		);
		self.ui.set_text("agent-hub-inspector", detail);
	}
}

fn build(
	rows: &[AgentRow],
	selected: usize,
	view: HubView,
	list_rows: u16,
	width: u16,
	ctx: &UiContext,
) -> Ui {
	let labels = rows
		.iter()
		.enumerate()
		.map(|(index, row)| {
			let indent = if view == HubView::Tree {
				"  ".repeat(usize::from(row.depth))
			} else {
				String::new()
			};
			(index, sf!("{indent}{}", row.name), row)
		})
		.collect::<Vec<_>>();
	let title = match view {
		HubView::Roster => "Agent Hub · roster",
		HubView::Tree => "Agent Hub · tree",
	};
	let height = list_rows.saturating_add(1);
	let list_width = if width >= WIDE_INSPECTOR {
		width.saturating_mul(2) / 5
	} else {
		width.saturating_sub(4)
	};
	let root = if width >= WIDE_INSPECTOR {
		OverlayPanel::new(title).child(dom! {
			<col>
				<row gap=2>
					<select id="agent-hub-list" w={list_width} h={height}>
						for (index, label, row) in labels {
							<option value={sf!("{index}")} label={label} recommended={index == selected}>
								<td truncate grow><pre fg=fg>{row.status.clone()}</pre></td>
							</option>
						}
					</select>
					<text id="agent-hub-inspector" grow wrap>{" "}</text>
				</row>
				{panel_divider()}
				<text fg=muted truncate>{"t roster/tree · Enter/s steer · r revive · k kill · Esc root"}</text>
			</col>
		})
	} else {
		OverlayPanel::new(title).child(dom! {
			<col>
				<select id="agent-hub-list" w={list_width} h={height}>
					for (index, label, row) in labels {
						<option value={sf!("{index}")} label={label} recommended={index == selected}>
							<td truncate grow><pre fg=fg>{row.status.clone()}</pre></td>
						</option>
					}
				</select>
				{panel_divider()}
				<text id="agent-hub-inspector" h=4 wrap>{" "}</text>
				<text fg=muted truncate>{"t view · Enter/s steer · r revive · k kill · Esc"}</text>
			</col>
		})
	};
	Ui::from_root(root, width, ctx.clone())
}
