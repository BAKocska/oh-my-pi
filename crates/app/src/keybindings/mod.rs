//! Native keybinding configuration.

pub mod config;

use omp_core::Str;

/// Host platform used for fallback chords and user-facing modifier labels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyPlatform {
	/// Apple terminals.
	MacOs,
	/// Windows terminals.
	Windows,
	/// Linux and other Unix terminals.
	Unix,
}

impl KeyPlatform {
	/// Returns the platform of the current application build.
	pub const fn current() -> Self {
		#[cfg(target_os = "macos")]
		{
			Self::MacOs
		}
		#[cfg(target_os = "windows")]
		{
			Self::Windows
		}
		#[cfg(not(any(target_os = "macos", target_os = "windows")))]
		{
			Self::Unix
		}
	}
}

const FOLLOW_UP_DEFAULT: &[&str] = &["alt+enter"];
const FOLLOW_UP_WINDOWS: &[&str] = &["alt+enter", "ctrl+q"];
const DEQUEUE_MACOS: &[&str] = &["shift+up"];
const DEQUEUE_DEFAULT: &[&str] = &["ctrl+up"];
const NO_FALLBACK: &[&str] = &[];

/// Resolves platform-specific fallback chords for an unconfigured action.
pub fn fallback_chords(action: &str, platform: KeyPlatform) -> &'static [&'static str] {
	match (action, platform) {
		("app.message.follow_up", KeyPlatform::Windows) => FOLLOW_UP_WINDOWS,
		("app.message.follow_up", _) => FOLLOW_UP_DEFAULT,
		("app.message.dequeue", KeyPlatform::MacOs) => DEQUEUE_MACOS,
		("app.message.dequeue", _) => DEQUEUE_DEFAULT,
		_ => NO_FALLBACK,
	}
}

/// Formats a canonical chord with platform-native modifier names.
pub fn format_chord_label(
	chord: &str,
	platform: KeyPlatform,
) -> Result<Str, config::KeybindingsConfigError> {
	let chord = config::normalize_chord(chord)?;
	let mut output = String::with_capacity(chord.len() + 8);
	for (index, part) in chord.as_str().split('+').enumerate() {
		if index > 0 {
			output.push('+');
		}
		let label = match (part, platform) {
			("alt", KeyPlatform::MacOs) => "Option",
			("alt", _) => "Alt",
			("super", KeyPlatform::MacOs) => "Cmd",
			("super", KeyPlatform::Windows | KeyPlatform::Unix) => "Super",
			("ctrl", _) => "Ctrl",
			("shift", _) => "Shift",
			("enter", _) => "Enter",
			("escape", _) => "Esc",
			("pageup", _) => "PageUp",
			("pagedown", _) => "PageDown",
			("up", _) => "Up",
			("down", _) => "Down",
			("left", _) => "Left",
			("right", _) => "Right",
			("tab", _) => "Tab",
			("space", _) => "Space",
			("backspace", _) => "Backspace",
			("delete", _) => "Delete",
			("home", _) => "Home",
			("end", _) => "End",
			(key, _) => key,
		};
		output.push_str(label);
	}
	Ok(Str::from(output))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn platform_fallbacks_and_labels_are_native() {
		assert_eq!(fallback_chords("app.message.follow_up", KeyPlatform::Windows), [
			"alt+enter",
			"ctrl+q"
		]);
		assert_eq!(fallback_chords("app.message.dequeue", KeyPlatform::MacOs), ["shift+up"]);
		assert_eq!(
			format_chord_label("cmd+option+p", KeyPlatform::MacOs).expect("label"),
			"Option+Cmd+p"
		);
		assert_eq!(
			format_chord_label("super+alt+p", KeyPlatform::Unix).expect("label"),
			"Alt+Super+p"
		);
	}
}
