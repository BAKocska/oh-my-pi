//! Extension shortcut registration for the scene's unclaimed-key path.

use omp_core::Str;
use omp_tui::{Chord, Key, Mods};
use smallvec::SmallVec;

/// Stable extension action identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionId(Str);

impl ActionId {
	/// Creates an action identity supplied by an extension declaration.
	pub fn new(id: impl Into<Str>) -> Self {
		Self(id.into())
	}

	/// Returns the action identifier.
	pub fn as_str(&self) -> &str {
		self.0.as_str()
	}
}

/// Refusal returned when an extension tries to claim a core shortcut.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShortcutRefusal {
	/// Core input, quit, or clipboard handling owns the chord.
	CoreOwned,
}

/// Retained extension shortcut table, consulted only after widgets decline a
/// key through the runtime's unclaimed-key path.
pub struct Actions {
	shortcuts: SmallVec<(Chord, ActionId), 8>,
}

impl Actions {
	/// Creates an empty shortcut table.
	pub const fn new() -> Self {
		Self { shortcuts: SmallVec::new() }
	}

	/// Registers or replaces an extension shortcut.
	/// The shared core refusal predicate ensures a control-side declaration
	/// cannot steal navigation or input chords.
	pub fn register(&mut self, chord: Chord, action: ActionId) -> Result<(), ShortcutRefusal> {
		if omp_tui::is_core_chord(chord) {
			return Err(ShortcutRefusal::CoreOwned);
		}
		if let Some((_, existing)) = self
			.shortcuts
			.iter_mut()
			.find(|(candidate, _)| *candidate == chord)
		{
			*existing = action;
		} else {
			self.shortcuts.push((chord, action));
		}
		Ok(())
	}

	/// Resolves an already-unclaimed chord to its extension action.
	pub fn dispatch(&self, chord: Chord) -> Option<&ActionId> {
		self
			.shortcuts
			.iter()
			.find_map(|(candidate, action)| (*candidate == chord).then_some(action))
	}

	/// Resolves a key emitted by [`omp_tui::AppEvent::Key`], the runtime's
	/// unclaimed-key path. Semantic modifier keys are already encoded in
	/// [`Key`]; plain decoded keys use an empty modifier set.
	pub fn dispatch_key(&self, key: Key) -> Option<&ActionId> {
		self.dispatch(Chord::new(key, Mods::default()))
	}
}

impl Default for Actions {
	fn default() -> Self {
		Self::new()
	}
}

#[cfg(test)]
mod tests {
	use omp_tui::{Chord, Key, Mods};

	use super::{ActionId, Actions, ShortcutRefusal};

	#[test]
	fn core_chords_are_refused() {
		let mut actions = Actions::new();
		let copy = Chord::new(Key::Copy, Mods::default());
		assert_eq!(actions.register(copy, ActionId::new("copy")), Err(ShortcutRefusal::CoreOwned));
	}
}
