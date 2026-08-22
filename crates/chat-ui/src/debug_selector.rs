//! Filterable debug action selector overlay.

use omp_core::Str;
use omp_tui::{Key, Layer, Mouse, Size, UiContext};

use crate::{ListPicker, ListRow, PickerEvent};

/// One app-owned debug action projected into the selector.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DebugActionRow {
	/// Stable action key returned on selection.
	pub key:         Str,
	/// Compact action label.
	pub label:       Str,
	/// Consequence description.
	pub description: Str,
}

/// Standard debug-tools selector built on the shared list overlay.
pub struct DebugSelector {
	picker: ListPicker,
}

impl DebugSelector {
	/// Opens the selector over the app-owned action catalog.
	#[must_use]
	pub fn open(actions: &[DebugActionRow], ctx: &UiContext) -> Self {
		let rows = actions
			.iter()
			.map(|action| ListRow {
				key:    action.key.clone(),
				label:  action.label.clone(),
				detail: action.description.clone(),
			})
			.collect::<Vec<_>>();
		Self { picker: ListPicker::open("Debug tools", &rows, 0, ctx) }
	}

	/// Routes keyboard navigation and filtering.
	pub fn handle_key(&mut self, key: Key) -> PickerEvent {
		self.picker.handle_key(key)
	}

	/// Routes pasted filter text.
	pub fn handle_paste(&mut self, text: &str) -> PickerEvent {
		self.picker.handle_paste(text)
	}

	/// Routes click and wheel interaction.
	pub fn handle_mouse(&mut self, col: u16, row: u16, kind: Mouse, viewport: Size) -> PickerEvent {
		self.picker.handle_mouse(col, row, kind, viewport)
	}

	/// Returns the selected stable action key.
	#[must_use]
	pub fn key(&self, index: usize) -> Option<&Str> {
		self.picker.key(index)
	}

	/// Returns the centered overlay layer.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		self.picker.layer(viewport)
	}
}
