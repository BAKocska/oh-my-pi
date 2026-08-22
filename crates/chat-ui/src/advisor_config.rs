//! Full-screen WATCHDOG YAML editor and block-scalar serialization.

use std::sync::Arc;

use omp_core::{Str, StrMut};
use omp_tui::{Dim, Key, Layer, Mouse, OverlayAnchor, OverlayOptions, Size, Ui, UiContext, dom};

use crate::{OverlayPanel, panel_divider};

const SAVE_HINT: &str = "Ctrl+S save · Esc cancel · Enter newline";

/// One advisor declaration edited by the structured serializer.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WatchdogAdvisorDocument {
	/// Human-facing name.
	pub name:         Str,
	/// Optional `model:thinking` selector.
	pub model:        Option<Str>,
	/// Explicit tool subset.
	pub tools:        Arc<[Str]>,
	/// Whether this advisor is enabled.
	pub enabled:      bool,
	/// Multiline advisor instructions.
	pub instructions: Option<Str>,
}

/// Structured WATCHDOG document accepted by the block-scalar serializer.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WatchdogEditorDocument {
	/// Shared multiline instructions.
	pub instructions: Option<Str>,
	/// Ordered advisor roster.
	pub advisors:     Arc<[WatchdogAdvisorDocument]>,
}

/// Host filesystem action selected by the editor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WatchdogEditorAction {
	/// Input was consumed while editing remains active.
	Consumed,
	/// Persist exact YAML to the selected WATCHDOG path.
	Write { path: Str, yaml: Str },
	/// Delete all known config paths after an empty document was saved.
	Delete { paths: Arc<[Str]> },
	/// Close without changing files.
	Cancel,
}

/// Full-viewport retained WATCHDOG editor.
pub struct WatchdogConfigEditor {
	ui:          Ui,
	ctx:         UiContext,
	options:     OverlayOptions,
	path:        Str,
	legacy_path: Option<Str>,
	width:       u16,
	height:      u16,
}

impl WatchdogConfigEditor {
	/// Opens a full-screen editor over existing YAML.
	///
	/// `legacy_path` is also removed when the saved document is empty,
	/// preventing an old single-advisor file from silently reviving fallback
	/// configuration.
	pub fn open(
		path: impl Into<Str>,
		legacy_path: Option<Str>,
		yaml: &str,
		ctx: &UiContext,
	) -> Self {
		let width = 80;
		let height = 20;
		let mut ui = build_editor(yaml, width, height, ctx);
		ui.focus_first();
		Self {
			ui,
			ctx: ctx.clone(),
			options: OverlayOptions::default()
				.anchor(OverlayAnchor::Center)
				.width(Dim::Pct(100))
				.max_height(Dim::Pct(100))
				.fill_height()
				.z(30),
			path: path.into(),
			legacy_path,
			width,
			height,
		}
	}

	/// Routes editor keys. Ctrl+S commits; Esc cancels.
	pub fn handle_key(&mut self, key: Key) -> WatchdogEditorAction {
		match key {
			Key::Esc => WatchdogEditorAction::Cancel,
			Key::Ctrl('s') => self.save_action(),
			_ => {
				let _ = self.ui.handle_key(key);
				WatchdogEditorAction::Consumed
			},
		}
	}

	/// Routes pasted YAML into the multiline editor.
	pub fn handle_paste(&mut self, text: &str) -> WatchdogEditorAction {
		let _ = self.ui.handle_paste(text);
		WatchdogEditorAction::Consumed
	}

	/// Routes pointer input; clicking outside does not dismiss a full-screen
	/// editor.
	pub fn handle_mouse(
		&mut self,
		col: u16,
		row: u16,
		kind: Mouse,
		viewport: Size,
	) -> WatchdogEditorAction {
		let _ = self
			.ui
			.handle_mouse_as_layer(&self.options, viewport, col, row, kind);
		WatchdogEditorAction::Consumed
	}

	/// Returns a viewport-filling retained layer.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		let width = viewport.width.saturating_sub(2).max(1);
		let height = viewport.height.saturating_sub(2).max(6);
		if width != self.width || height != self.height {
			let document = self.document();
			self.ui = build_editor(&document, width, height, &self.ctx);
			self.ui.focus_first();
			self.width = width;
			self.height = height;
		}
		Layer { frame: self.ui.frame(), options: &self.options, active: true }
	}

	/// Returns the exact current editor buffer.
	pub fn document(&self) -> String {
		self.ui.values()["watchdog-editor"]
			.as_str()
			.unwrap_or_default()
			.to_owned()
	}

	fn save_action(&self) -> WatchdogEditorAction {
		let yaml = self.document();
		if yaml.trim().is_empty() {
			let mut paths = vec![self.path.clone()];
			if let Some(legacy) = self.legacy_path.as_ref()
				&& legacy != &self.path
			{
				paths.push(legacy.clone());
			}
			WatchdogEditorAction::Delete { paths: paths.into() }
		} else {
			WatchdogEditorAction::Write { path: self.path.clone(), yaml: Str::new(yaml) }
		}
	}
}

fn build_editor(document: &str, width: u16, height: u16, ctx: &UiContext) -> Ui {
	let value = Str::new(document);
	let editor_height = height.saturating_sub(4).max(3);
	Ui::from_root(
		OverlayPanel::new("WATCHDOG configuration").child(dom! {
			<col h={height}>
				<editor id="watchdog-editor" value={value} h={editor_height} grow/>
				{panel_divider()}
				<text dim truncate>{SAVE_HINT}</text>
			</col>
		}),
		width,
		ctx.clone(),
	)
}

/// Serializes structured WATCHDOG YAML with literal block scalars.
///
/// Returns `None` for an empty document so the host can apply the deletion
/// fallback instead of writing a semantically empty file.
pub fn serialize_watchdog_yaml(document: &WatchdogEditorDocument) -> Option<Str> {
	if document
		.instructions
		.as_ref()
		.is_none_or(|value| value.trim().is_empty())
		&& document.advisors.is_empty()
	{
		return None;
	}
	let mut yaml = StrMut::new("");
	if let Some(instructions) = document
		.instructions
		.as_ref()
		.filter(|value| !value.trim().is_empty())
	{
		yaml.push_str("instructions: |-\n");
		push_block(&mut yaml, instructions, 2);
	}
	if !document.advisors.is_empty() {
		yaml.push_str("advisors:\n");
		for advisor in document.advisors.iter() {
			yaml.push_str("  - name: ");
			push_yaml_scalar(&mut yaml, advisor.name.as_str());
			yaml.push('\n');
			if let Some(model) = advisor.model.as_ref() {
				yaml.push_str("    model: ");
				push_yaml_scalar(&mut yaml, model.as_str());
				yaml.push('\n');
			}
			yaml.push_str(if advisor.enabled {
				"    enabled: true\n"
			} else {
				"    enabled: false\n"
			});
			if !advisor.tools.is_empty() {
				yaml.push_str("    tools:\n");
				for tool in advisor.tools.iter() {
					yaml.push_str("      - ");
					push_yaml_scalar(&mut yaml, tool.as_str());
					yaml.push('\n');
				}
			}
			if let Some(instructions) = advisor
				.instructions
				.as_ref()
				.filter(|value| !value.trim().is_empty())
			{
				yaml.push_str("    instructions: |-\n");
				push_block(&mut yaml, instructions, 6);
			}
		}
	}
	Some(yaml.freeze())
}

fn push_block(output: &mut StrMut, value: &str, indent: usize) {
	for line in value.trim().lines() {
		for _ in 0..indent {
			output.push(' ');
		}
		output.push_str(line);
		output.push('\n');
	}
}

fn push_yaml_scalar(output: &mut StrMut, value: &str) {
	output.push('\'');
	for character in value.chars() {
		if character == '\'' {
			output.push_str("''");
		} else {
			output.push(character);
		}
	}
	output.push('\'');
}
