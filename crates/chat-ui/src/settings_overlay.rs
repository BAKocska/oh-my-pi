//! Schema-driven retained settings editor.

use std::collections::BTreeMap;

use omp_core::{Str, sf};
use omp_tui::{
	Dim, Key, Layer, Mouse, OverlayAnchor, OverlayOptions, Prop, Size, Ui, UiContext, UiEvent,
	components::{Field, Form, Tabs},
	dom,
};
use serde_json::Value;

use crate::{SettingRow, panel_divider};

/// One value mutation emitted by the settings surface.
#[derive(Clone, Debug, PartialEq)]
pub struct SettingChange {
	/// Owning registered settings domain.
	pub domain: Str,
	/// Stable dotted field path.
	pub path:   Str,
	/// Typed JSON value produced by the reflected widget.
	pub value:  Value,
}

/// Action emitted by [`SettingsOverlay`].
#[derive(Clone, Debug, PartialEq)]
pub enum SettingsEvent {
	/// Input was consumed without changing a value.
	Consumed,
	/// Dismiss without committing the preview generation.
	Close,
	/// Preview changed values without persisting them.
	Preview(Vec<SettingChange>),
	/// Persist the complete visible settings generation.
	Commit(Vec<SettingChange>),
}

/// Retained tabbed settings modal built from registered field descriptors.
pub struct SettingsOverlay {
	ui:       Ui,
	rows:     Vec<SettingRow>,
	ctx:      UiContext,
	options:  OverlayOptions,
	query:    Str,
	width:    u16,
	baseline: BTreeMap<Str, Value>,
}

impl SettingsOverlay {
	/// Opens the editor over the backend's merged, secret-safe schema
	/// projection.
	pub fn open(rows: Vec<SettingRow>, ctx: &UiContext) -> Self {
		let width = 84;
		let query = Str::default();
		let mut ui = build(&rows, &query, width, ctx);
		ui.focus_first();
		let baseline = collect_values(&ui, &rows);
		Self {
			ui,
			rows,
			ctx: ctx.clone(),
			options: OverlayOptions::default()
				.anchor(OverlayAnchor::Center)
				.width(Dim::Cells(width))
				.z(20),
			query,
			width,
			baseline,
		}
	}

	/// Routes keyboard editing, popup navigation, preview, and commit.
	pub fn handle_key(&mut self, key: Key) -> SettingsEvent {
		if key == Key::Esc {
			return SettingsEvent::Close;
		}
		if key == Key::Ctrl('s') {
			return SettingsEvent::Commit(self.changes(true));
		}
		let event = self.ui.handle_key(key);
		if let UiEvent::Changed { id, value } = &event
			&& id.as_str() == "settings-search"
		{
			self.query = value.clone();
			self.rebuild();
			return SettingsEvent::Consumed;
		}
		let changes = self.changes(false);
		if changes.is_empty() {
			SettingsEvent::Consumed
		} else {
			SettingsEvent::Preview(changes)
		}
	}

	/// Routes pasted text to the focused search or secret-safe text field.
	pub fn handle_paste(&mut self, text: &str) -> SettingsEvent {
		let event = self.ui.handle_paste(text);
		if let UiEvent::Changed { id, value } = &event
			&& id.as_str() == "settings-search"
		{
			self.query = value.clone();
			self.rebuild();
			return SettingsEvent::Consumed;
		}
		let changes = self.changes(false);
		if changes.is_empty() {
			SettingsEvent::Consumed
		} else {
			SettingsEvent::Preview(changes)
		}
	}

	/// Routes pointer events; an outside click cancels the preview generation.
	pub fn handle_mouse(
		&mut self,
		col: u16,
		row: u16,
		kind: Mouse,
		viewport: Size,
	) -> SettingsEvent {
		match self
			.ui
			.handle_mouse_as_layer(&self.options, viewport, col, row, kind)
		{
			Some(_) => {
				let changes = self.changes(false);
				if changes.is_empty() {
					SettingsEvent::Consumed
				} else {
					SettingsEvent::Preview(changes)
				}
			},
			None if kind == Mouse::Click => SettingsEvent::Close,
			None => SettingsEvent::Consumed,
		}
	}

	/// Returns a centered viewport-responsive retained layer.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		let width = viewport.width.saturating_sub(4).clamp(1, 96);
		if width != self.width {
			self.width = width;
			self.rebuild();
		}
		self.options = self.options.width(Dim::Cells(width));
		Layer { frame: self.ui.frame(), options: &self.options, active: true }
	}

	fn rebuild(&mut self) {
		let current = collect_values(&self.ui, &self.rows);
		self.ui = build(&self.rows, &self.query, self.width, &self.ctx);
		self.ui.set_text("settings-search", self.query.clone());
		self.ui.focus_first();
		self.baseline.extend(current);
	}

	fn changes(&mut self, include_unchanged: bool) -> Vec<SettingChange> {
		let current = collect_values(&self.ui, &self.rows);
		let mut changes = Vec::new();
		for row in &self.rows {
			let Some(value) = current.get(&row.path) else {
				continue;
			};
			if include_unchanged || self.baseline.get(&row.path) != Some(value) {
				changes.push(SettingChange {
					domain: row.domain.clone(),
					path:   row.path.clone(),
					value:  value.clone(),
				});
			}
		}
		if !include_unchanged {
			self.baseline = current;
		}
		changes
	}
}

fn build(rows: &[SettingRow], query: &str, width: u16, ctx: &UiContext) -> Ui {
	let query_folded = query.to_ascii_lowercase();
	let mut panels = Vec::<Str>::new();
	for row in rows.iter().filter(|row| row.visible) {
		let panel = if row.panel.is_empty() {
			row.domain.clone()
		} else {
			row.panel.clone()
		};
		if !panels.contains(&panel) {
			panels.push(panel);
		}
	}
	let mut tabs = Tabs::new().with(Prop::Id, "settings-tabs");
	for (index, panel) in panels.iter().enumerate() {
		let visible: Vec<_> = rows
			.iter()
			.filter(|row| {
				row.visible
					&& (row.panel == *panel || (row.panel.is_empty() && row.domain == *panel))
					&& (query_folded.is_empty()
						|| row.label.to_ascii_lowercase().contains(&query_folded)
						|| row.path.to_ascii_lowercase().contains(&query_folded)
						|| row.description.to_ascii_lowercase().contains(&query_folded))
			})
			.collect();
		let mut form = Form::new().with(Prop::Id, sf!("settings-form-{index}"));
		for row in &visible {
			let kind = match row.kind.as_str() {
				"bool" | "boolean" => "bool",
				"enum" => "select",
				"multi" | "string-list" => "multi",
				"number" | "integer" => "number",
				_ => "text",
			};
			let mut field = Field::new()
				.with(Prop::Id, row.path.clone())
				.with(Prop::Kind, kind)
				.with(Prop::Desc, row.description.clone())
				.with(Prop::Mask, row.secret)
				.label(row.label.clone());
			if let Some(value) = &row.value {
				field = field.with(Prop::Value, value.clone());
			}
			if !row.options.is_empty() {
				field = field.with(
					Prop::Options,
					Str::from(
						row.options
							.iter()
							.map(Str::as_str)
							.collect::<Vec<_>>()
							.join(" "),
					),
				);
			}
			form = form.field(field);
		}
		let title = sf!("{} ({})", panel, visible.len());
		tabs = tabs.pane(title, form);
	}
	let seed = Str::new(query);
	Ui::from_root(
		crate::OverlayPanel::new("Settings").child(dom! {
			<col>
				<input id="settings-search" value={seed} placeholder="Search settings"/>
				{panel_divider()}
				{tabs}
				{panel_divider()}
				<text dim truncate>"Tab panes · Enter edit · Left/Right change · Ctrl+S save · Esc cancel"</text>
			</col>
		}),
		width,
		ctx.clone(),
	)
}

fn collect_values(ui: &Ui, rows: &[SettingRow]) -> BTreeMap<Str, Value> {
	let values = ui.values();
	let mut collected = BTreeMap::new();
	let Some(root) = values.as_object() else {
		return collected;
	};
	for form in root.values().filter_map(Value::as_object) {
		for row in rows {
			if let Some(value) = form.get(row.path.as_str()) {
				collected.insert(row.path.clone(), value.clone());
			}
		}
	}
	collected
}
