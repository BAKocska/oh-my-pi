//! Provider-scoped model hub backed exclusively by host-supplied catalog rows.

use std::fmt::{self, Write as _};

use omp_core::{Str, StrMut, sf};
use omp_tui::{
	Dim, Key, Layer, Mouse, OverlayAnchor, OverlayOptions, Prop, Size, Ui, UiContext, UiEvent,
	assets::provider_logo, dom,
};

use crate::{
	ModelRow,
	overlays::{OverlayPanel, panel_divider},
};

const SIDEBAR_HINT: &str = "↑/↓ providers · → models · type to search · Esc close";
const MODEL_HINT: &str = "↑/↓ models · ← providers · Enter switch · type to search · Esc close";
const FRAME_ROWS: u16 = 6;
const CONTEXT_WIDTH: u16 = 62;
const INPUT_PRICE_WIDTH: u16 = 76;
const OUTPUT_PRICE_WIDTH: u16 = 88;

/// What a routed picker event did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PickerEvent {
	/// The picker consumed the event and remains open.
	Consumed,
	/// Close without choosing a row.
	Close,
	/// Choose the row at this index.
	Pick(usize),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PickerFocus {
	Sidebar,
	Models,
}

/// Retained two-pane provider/model picker overlay.
pub struct ModelPicker {
	ui:              Ui,
	rows:            Vec<ModelRow>,
	current:         usize,
	ctx:             UiContext,
	options:         OverlayOptions,
	query:           Str,
	active_provider: Option<Str>,
	focus:           PickerFocus,
	list_rows:       u16,
}

impl ModelPicker {
	/// Opens the picker over host-supplied rows with `current` preselected.
	pub fn open(rows: &[ModelRow], current: usize, ctx: &UiContext) -> Self {
		let rows = rows.to_vec();
		let current = current.min(rows.len().saturating_sub(1));
		let ui = build(&rows, current, "", None, PickerFocus::Sidebar, 6, 100, ctx);
		let mut picker = Self {
			ui,
			rows,
			current,
			ctx: ctx.clone(),
			options: OverlayOptions::default()
				.anchor(OverlayAnchor::Bottom)
				.width(Dim::Pct(100))
				.z(10),
			query: Str::default(),
			active_provider: None,
			focus: PickerFocus::Sidebar,
			list_rows: 6,
		};
		picker.restore_focus();
		picker.show_detail((!picker.rows.is_empty()).then_some(current));
		picker
	}

	/// Routes a key into sidebar navigation or the focused model filter.
	pub fn handle_key(&mut self, key: Key) -> PickerEvent {
		match key {
			Key::Left => {
				self.set_focus(PickerFocus::Sidebar);
				PickerEvent::Consumed
			},
			Key::Right => {
				self.set_focus(PickerFocus::Models);
				PickerEvent::Consumed
			},
			Key::Tab => {
				let next = if self.focus == PickerFocus::Sidebar {
					PickerFocus::Models
				} else {
					PickerFocus::Sidebar
				};
				self.set_focus(next);
				PickerEvent::Consumed
			},
			Key::Char(ch) if self.focus == PickerFocus::Sidebar && !ch.is_control() => {
				self.active_provider = None;
				self.focus = PickerFocus::Models;
				self.rebuild(self.ui.frame().size().width);
				let event = self.ui.handle_key(Key::Char(ch));
				self.route(event)
			},
			key => {
				let event = self.ui.handle_key(key);
				self.route(event)
			},
		}
	}

	/// Routes pasted query text into the model filter, focusing it first.
	pub fn handle_paste(&mut self, text: &str) -> PickerEvent {
		if self.focus == PickerFocus::Sidebar && text.chars().any(|ch| !ch.is_control()) {
			self.active_provider = None;
			self.focus = PickerFocus::Models;
			self.rebuild(self.ui.frame().size().width);
		}
		let event = self.ui.handle_paste(text);
		self.route(event)
	}

	/// Routes a pointer event; clicking outside dismisses the overlay.
	pub fn handle_mouse(&mut self, col: u16, row: u16, kind: Mouse, viewport: Size) -> PickerEvent {
		match self
			.ui
			.handle_mouse_as_layer(&self.options, viewport, col, row, kind)
		{
			Some(event) => self.route(event),
			None if kind == Mouse::Click => PickerEvent::Close,
			None => PickerEvent::Consumed,
		}
	}

	/// Returns the bottom-anchored composited layer for this frame.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		let rows = (viewport.height * 2 / 5).saturating_sub(FRAME_ROWS).max(5);
		if rows != self.list_rows {
			self.list_rows = rows;
			self.ui.set_prop("models", Prop::H, rows.saturating_add(1));
		}
		if self.ui.frame().size().width != viewport.width {
			self.rebuild(viewport.width);
		}
		Layer { frame: self.ui.frame(), options: &self.options, active: true }
	}

	/// Replaces catalog rows while preserving the active provider scope, query,
	/// and pane focus.
	///
	/// The visible provider slice is rebuilt immediately; if that provider was
	/// removed, the hub falls back to the complete model list.
	pub fn update_rows(&mut self, rows: &[ModelRow], current: usize) {
		let width = self.ui.frame().size().width;
		self.rows = rows.to_vec();
		self.current = current.min(self.rows.len().saturating_sub(1));
		if self.active_provider.as_ref().is_some_and(|provider| {
			!self
				.rows
				.iter()
				.any(|row| row.provider_id.as_str() == provider.as_str())
		}) {
			self.active_provider = None;
		}
		self.rebuild(width);
	}

	fn route(&mut self, event: UiEvent) -> PickerEvent {
		match event {
			UiEvent::Cancel => PickerEvent::Close,
			UiEvent::Changed { id, value } if id.as_str() == "models" => value
				.as_str()
				.parse()
				.map_or(PickerEvent::Consumed, PickerEvent::Pick),
			UiEvent::Changed { id, value } if id.as_str() == "model-providers" => {
				self.set_provider(value);
				self.set_focus(PickerFocus::Models);
				PickerEvent::Consumed
			},
			UiEvent::Highlighted { id, value } if id.as_str() == "model-providers" => {
				self.focus = PickerFocus::Sidebar;
				self.set_provider(value);
				PickerEvent::Consumed
			},
			UiEvent::Highlighted { id, value } if id.as_str() == "models" => {
				self.focus = PickerFocus::Models;
				self.show_detail(value.as_str().parse().ok());
				PickerEvent::Consumed
			},
			UiEvent::Filtered { id, query, value } if id.as_str() == "models" => {
				self.focus = PickerFocus::Models;
				self.query = query;
				self.show_detail(value.and_then(|value| value.as_str().parse().ok()));
				PickerEvent::Consumed
			},
			UiEvent::None
			| UiEvent::Submit
			| UiEvent::Pressed(_)
			| UiEvent::Copied(_)
			| UiEvent::Changed { .. }
			| UiEvent::Highlighted { .. }
			| UiEvent::Filtered { .. } => PickerEvent::Consumed,
		}
	}

	fn rebuild(&mut self, width: u16) {
		self.ui = build(
			&self.rows,
			self.current,
			&self.query,
			self.active_provider.as_deref(),
			self.focus,
			self.list_rows,
			width,
			&self.ctx,
		);
		self.restore_focus();
		self.show_detail((!self.rows.is_empty()).then_some(self.current));
	}

	fn restore_focus(&mut self) {
		self.ui.focus_first();
		if self.focus == PickerFocus::Models {
			let _ = self.ui.handle_key(Key::Tab);
		}
	}

	fn set_focus(&mut self, focus: PickerFocus) {
		if focus == self.focus {
			return;
		}
		let _ = self.ui.handle_key(Key::Tab);
		self.focus = focus;
	}

	fn set_provider(&mut self, value: Str) {
		let provider = (!value.is_empty()).then_some(value);
		if provider == self.active_provider {
			return;
		}
		self.active_provider = provider;
		self.rebuild(self.ui.frame().size().width);
	}

	fn show_detail(&mut self, model: Option<usize>) {
		let text = model
			.and_then(|index| self.rows.get(index))
			.map_or_else(|| sf!(" "), facts);
		self.ui.set_text("model-facts", text);
	}
}

struct DisplayRow {
	value:    Str,
	label:    Str,
	logo_src: Option<Str>,
	provider: Str,
	name:     Str,
	current:  bool,
	context:  Str,
	input:    Str,
	output:   Str,
}
struct ProviderChoice {
	id:      Str,
	label:   Str,
	current: bool,
}

fn build(
	rows: &[ModelRow],
	current: usize,
	query: &str,
	active_provider: Option<&str>,
	focus: PickerFocus,
	list_rows: u16,
	width: u16,
	ctx: &UiContext,
) -> Ui {
	let sidebar_width = if width >= 42 {
		22
	} else {
		width.saturating_div(3).max(1)
	};
	let model_width = width.saturating_sub(sidebar_width.saturating_add(5));
	let show_context = model_width >= CONTEXT_WIDTH && rows.iter().any(|row| row.context.is_some());
	let show_input =
		model_width >= INPUT_PRICE_WIDTH && rows.iter().any(|row| row.input_mtok.is_some());
	let show_output =
		model_width >= OUTPUT_PRICE_WIDTH && rows.iter().any(|row| row.output_mtok.is_some());
	let display: Vec<_> = rows
		.iter()
		.enumerate()
		.filter(|(_, row)| {
			active_provider.is_none_or(|provider| row.provider_id.as_str() == provider)
		})
		.map(|(index, row)| DisplayRow {
			value:    sf!("{index}"),
			label:    sf!("{} {} {}", row.provider, row.name, row.key),
			logo_src: provider_logo(row.provider_id.as_str())
				.is_some()
				.then(|| sf!("asset://login/{}", row.provider_id)),
			provider: if row.provider.is_empty() {
				row.provider_id.clone()
			} else {
				row.provider.clone()
			},
			name:     if row.name.is_empty() {
				row.key.clone()
			} else {
				row.name.clone()
			},
			current:  index == current,
			context:  row
				.context
				.map_or_else(Str::default, |tokens| sf!("{} ctx", compact_count(tokens))),
			input:    row
				.input_mtok
				.map_or_else(Str::default, |cost| sf!("${cost} in")),
			output:   row
				.output_mtok
				.map_or_else(Str::default, |cost| sf!("${cost} out")),
		})
		.collect();
	let mut providers = Vec::<ProviderChoice>::new();
	for row in rows {
		if providers
			.iter()
			.any(|provider| provider.id == row.provider_id)
		{
			continue;
		}
		providers.push(ProviderChoice {
			id:      row.provider_id.clone(),
			label:   if row.provider.is_empty() {
				row.provider_id.clone()
			} else {
				row.provider.clone()
			},
			current: active_provider == Some(row.provider_id.as_str()),
		});
	}
	let all_active = active_provider.is_none();
	let seed = Str::new(query);
	let current_mark = sf!(" current");
	let height = list_rows.saturating_add(1);
	let hint = match focus {
		PickerFocus::Sidebar => SIDEBAR_HINT,
		PickerFocus::Models => MODEL_HINT,
	};
	Ui::from_root(
		OverlayPanel::new("Switch Model").child(dom! {
			<col>
				<row gap=1>
					<select id="model-providers" w={sidebar_width} h={height}>
						<option value="" label="All models" recommended={all_active}>
							<text bold={all_active}>{"All models"}</text>
						</option>
						for provider in providers {
							<option value={provider.id.clone()} label={provider.label.clone()}
								recommended={provider.current}>
								<text bold={provider.current} truncate>{provider.label}</text>
							</option>
						}
					</select>
					<select id="models" filter={seed} h={height} grow>
						for row in display {
							<option value={row.value} label={row.label} recommended={row.current}>
								<td>
									if let Some(src) = row.logo_src.clone() { <img src={src} w=2 h=1/> }
								</td>
								<td truncate>
									<pre fg=fg bg=border>{" "}{row.provider}{" "}</pre>
								</td>
								<td truncate=start grow>
									<pre fg=fg>{row.name}</pre>
									if row.current { <pre fg=ok>{current_mark.clone()}</pre> }
								</td>
								if show_context { <td align=end><pre fg=muted>{row.context}</pre></td> }
								if show_input { <td align=end><pre fg=muted>{row.input}</pre></td> }
								if show_output { <td align=end><pre fg=muted>{row.output}</pre></td> }
							</option>
						}
					</select>
				</row>
				{panel_divider()}
				<text id="model-facts" fg=muted truncate>{" "}</text>
				<text fg=muted truncate>{hint}</text>
			</col>
		}),
		width,
		ctx.clone(),
	)
}

fn facts(row: &ModelRow) -> Str {
	let mut line = StrMut::with_capacity(96);
	let name = if row.name.is_empty() {
		&row.key
	} else {
		&row.name
	};
	push_fact(&mut line, format_args!("{name}"));
	push_fact(&mut line, format_args!("{}", row.provider));
	if let Some(context) = row.context {
		push_fact(&mut line, format_args!("{} context", compact_count(context)));
	}
	match (row.input_mtok, row.output_mtok) {
		(Some(input), Some(output)) => {
			push_fact(&mut line, format_args!("${input}/${output} per Mtok"));
		},
		(Some(input), None) => push_fact(&mut line, format_args!("${input} in per Mtok")),
		(None, Some(output)) => push_fact(&mut line, format_args!("${output} out per Mtok")),
		(None, None) => {},
	}
	line.freeze()
}

fn push_fact(line: &mut StrMut, fact: fmt::Arguments<'_>) {
	if !line.is_empty() {
		line.push_str(" · ");
	}
	let _ = write!(line, "{fact}");
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

#[cfg(test)]
mod tests {
	use super::*;
	fn row(provider: &'static str, name: &'static str) -> ModelRow {
		ModelRow {
			key:         sf!("{provider}/{name}"),
			name:        sf!(name),
			provider_id: sf!(provider),
			provider:    sf!(provider),
			context:     None,
			input_mtok:  None,
			output_mtok: None,
		}
	}

	#[test]
	fn absent_facts_are_omitted() {
		let row = ModelRow {
			key:         sf!("p/m"),
			name:        sf!("Model"),
			provider_id: sf!("p"),
			provider:    sf!("Provider"),
			context:     None,
			input_mtok:  None,
			output_mtok: None,
		};
		let facts = facts(&row);
		assert!(!facts.contains("ctx"));
		assert!(!facts.contains('$'));
	}

	#[test]
	fn typing_from_sidebar_focuses_and_filters_models() {
		let rows = [row("alpha", "first"), row("beta", "second")];
		let mut picker = ModelPicker::open(&rows, 0, &UiContext::default());
		assert_eq!(picker.handle_key(Key::Char('b')), PickerEvent::Consumed);
		assert_eq!(picker.handle_key(Key::Enter), PickerEvent::Pick(1));
		assert_eq!(picker.focus, PickerFocus::Models);
		assert_eq!(picker.active_provider, None);
	}

	#[test]
	fn arrows_move_between_provider_sidebar_and_models() {
		let rows = [row("alpha", "first"), row("beta", "second")];
		let mut picker = ModelPicker::open(&rows, 0, &UiContext::default());
		assert_eq!(picker.handle_key(Key::Down), PickerEvent::Consumed);
		assert_eq!(picker.active_provider.as_deref(), Some("alpha"));
		assert_eq!(picker.handle_key(Key::Down), PickerEvent::Consumed);
		assert_eq!(picker.active_provider.as_deref(), Some("beta"));
		assert_eq!(picker.handle_key(Key::Right), PickerEvent::Consumed);
		assert_eq!(picker.handle_key(Key::Enter), PickerEvent::Pick(1));
		assert_eq!(picker.handle_key(Key::Left), PickerEvent::Consumed);
		assert_eq!(picker.focus, PickerFocus::Sidebar);
	}

	#[test]
	fn catalog_refresh_preserves_and_rebuilds_active_provider_view() {
		let rows = [row("alpha", "first"), row("beta", "second")];
		let mut picker = ModelPicker::open(&rows, 0, &UiContext::default());
		let _ = picker.handle_key(Key::Down);
		let _ = picker.handle_key(Key::Down);
		assert_eq!(picker.active_provider.as_deref(), Some("beta"));

		let refreshed = [row("alpha", "replacement"), row("beta", "new-second")];
		picker.update_rows(&refreshed, 0);
		assert_eq!(picker.active_provider.as_deref(), Some("beta"));
		assert_eq!(picker.handle_key(Key::Right), PickerEvent::Consumed);
		assert_eq!(picker.handle_key(Key::Enter), PickerEvent::Pick(1));
	}
}
