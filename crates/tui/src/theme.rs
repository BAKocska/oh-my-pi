//! JSON theme loading with automatic dark/light palette selection.

use omp_core::Str;
use serde::Deserialize;

use crate::{Appearance, Color, Theme};

/// Parsed named theme with dark and optional light variants.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JsonTheme {
	/// Human-readable theme name.
	pub name: Str,
	dark:     Theme,
	light:    Theme,
}

impl JsonTheme {
	/// Parses a strict JSON theme. Colors accept every [`Color::parse`] syntax;
	/// omitted tokens inherit the built-in palette for that appearance.
	pub fn parse(source: &str) -> Result<Self, ThemeError> {
		let file: ThemeFile = serde_json::from_str(source)
			.map_err(|error| ThemeError::Json(Str::from(error.to_string())))?;
		let dark = file.dark.apply(Theme::for_appearance(Appearance::Dark))?;
		let light = file
			.light
			.unwrap_or_else(|| file.dark.clone())
			.apply(Theme::for_appearance(Appearance::Light))?;
		Ok(Self { name: Str::from(file.name), dark, light })
	}

	/// Selects the palette matching the terminal's current appearance.
	#[must_use]
	pub const fn for_appearance(&self, appearance: Appearance) -> Theme {
		match appearance {
			Appearance::Dark => self.dark,
			Appearance::Light => self.light,
		}
	}
}

/// Theme parsing failure with a stable diagnostic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ThemeError {
	/// Invalid JSON shape.
	Json(Str),
	/// A named token did not contain a supported color.
	Color {
		/// Semantic token containing the bad value.
		token: &'static str,
		/// Unparsed color source.
		value: Str,
	},
}

impl std::fmt::Display for ThemeError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Json(message) => write!(f, "invalid theme JSON: {message}"),
			Self::Color { token, value } => write!(f, "invalid theme color `{token}`: {value}"),
		}
	}
}
impl std::error::Error for ThemeError {}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ThemeFile {
	name:  String,
	dark:  ThemePatch,
	light: Option<ThemePatch>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ThemePatch {
	fg:        Option<String>,
	accent:    Option<String>,
	info:      Option<String>,
	ok:        Option<String>,
	warn:      Option<String>,
	err:       Option<String>,
	muted:     Option<String>,
	border:    Option<String>,
	surface:   Option<String>,
	hover:     Option<String>,
	selection: Option<String>,
	shadow:    Option<String>,
	panel:     Option<String>,
	secondary: Option<String>,
	contrast:  Option<String>,
}

impl ThemePatch {
	fn apply(&self, mut theme: Theme) -> Result<Theme, ThemeError> {
		macro_rules! apply {
			($field:ident) => {
				if let Some(value) = &self.$field {
					theme.$field = Color::parse(value).ok_or_else(|| ThemeError::Color {
						token: stringify!($field),
						value: Str::new(value.as_str()),
					})?;
				}
			};
		}
		apply!(fg);
		apply!(accent);
		apply!(info);
		apply!(ok);
		apply!(warn);
		apply!(err);
		apply!(muted);
		apply!(border);
		apply!(surface);
		apply!(hover);
		apply!(selection);
		apply!(shadow);
		apply!(panel);
		apply!(secondary);
		apply!(contrast);
		Ok(theme)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn json_theme_selects_dark_and_light_variants() {
		let theme = JsonTheme::parse(
			r##"{
			"name":"cyanotype",
			"dark":{"accent":"#00ffff","panel":"rgb(1 2 3)"},
			"light":{"accent":"#005faf"}
		}"##,
		)
		.unwrap();
		assert_eq!(theme.name, "cyanotype");
		assert_eq!(theme.for_appearance(Appearance::Dark).accent, Color::Rgb(0, 255, 255));
		assert_eq!(theme.for_appearance(Appearance::Dark).panel, Color::Rgb(1, 2, 3));
		assert_eq!(theme.for_appearance(Appearance::Light).accent, Color::Rgb(0, 95, 175));
		assert_eq!(
			theme.for_appearance(Appearance::Light).panel,
			Theme::for_appearance(Appearance::Light).panel
		);
	}

	#[test]
	fn missing_light_variant_reuses_tokens_over_light_defaults() {
		let theme = JsonTheme::parse(r##"{"name":"one","dark":{"accent":"#123456"}}"##).unwrap();
		assert_eq!(theme.for_appearance(Appearance::Light).accent, Color::Rgb(0x12, 0x34, 0x56));
		assert_eq!(
			theme.for_appearance(Appearance::Light).fg,
			Theme::for_appearance(Appearance::Light).fg
		);
	}
}
