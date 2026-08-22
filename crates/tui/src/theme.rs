//! JSON theme loading with rich-slot lowering and appearance selection.

use std::collections::BTreeMap;

use omp_core::{IntoStr, Str};
use serde::Deserialize;
use thiserror::Error;

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
	/// Parses either omp's compact semantic palette or pi's richer `colors`
	/// palette. Rich component slots lower onto omp's semantic tokens once at
	/// load time, never during paint.
	pub fn parse(source: &str) -> Result<Self, ThemeError> {
		let file: ThemeFile = serde_json::from_str(source).map_err(ThemeError::Json)?;
		let (dark, light) = if let Some(colors) = &file.colors {
			(
				apply_rich(colors, &file.vars, Theme::for_appearance(Appearance::Dark))?,
				apply_rich(colors, &file.vars, Theme::for_appearance(Appearance::Light))?,
			)
		} else {
			let dark = file.dark.apply(Theme::for_appearance(Appearance::Dark))?;
			let light = file
				.light
				.unwrap_or_else(|| file.dark.clone())
				.apply(Theme::for_appearance(Appearance::Light))?;
			(dark, light)
		};
		Ok(Self { name: file.name.into_str(), dark, light })
	}

	/// Selects the palette matching the terminal's current appearance.
	#[must_use]
	pub const fn for_appearance(&self, appearance: Appearance) -> Theme {
		match appearance {
			Appearance::Dark => self.dark,
			Appearance::Light => self.light,
		}
	}

	/// Selects and quantizes the palette for an indexed-color terminal.
	#[must_use]
	pub const fn for_appearance_256(&self, appearance: Appearance) -> Theme {
		self.for_appearance(appearance).quantized_256()
	}
}

/// Theme parsing failure with a stable diagnostic.
#[derive(Debug, Error)]
pub enum ThemeError {
	/// Invalid JSON shape.
	#[error("invalid theme JSON")]
	Json(#[source] serde_json::Error),
	/// A named token did not contain a supported color.
	#[error("invalid theme color `{token}`: {value}")]
	Color {
		/// Semantic token containing the bad value.
		token: Str,
		/// Unparsed color source.
		value: Str,
	},
	/// An indexed color exceeded the terminal palette.
	#[error("theme color `{token}` has an index above 255")]
	Index {
		/// Semantic token containing the bad value.
		token: Str,
	},
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ThemeFile {
	#[serde(rename = "$schema")]
	_schema: Option<String>,
	name:    String,
	vars:    BTreeMap<String, ColorValue>,
	colors:  Option<BTreeMap<String, ColorValue>>,
	dark:    ThemePatch,
	light:   Option<ThemePatch>,
	export:  Option<serde_json::Value>,
	symbols: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum ColorValue {
	Text(String),
	Index(u16),
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
						token: Str::new_static(stringify!($field)),
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

fn apply_rich(
	colors: &BTreeMap<String, ColorValue>,
	vars: &BTreeMap<String, ColorValue>,
	mut theme: Theme,
) -> Result<Theme, ThemeError> {
	for (slot, value) in colors {
		let Some(color) = resolve_color(slot, value, vars)? else {
			continue;
		};
		match slot.as_str() {
			"text" => theme.fg = color,
			"accent" | "borderAccent" | "mdLink" | "statusLineModel" => theme.accent = color,
			"mdCodeBlock" | "bashMode" | "statusLinePath" => theme.info = color,
			"success" | "toolDiffAdded" | "statusLineGitClean" => theme.ok = color,
			"warning" | "statusLineGitDirty" | "statusLineDirty" => theme.warn = color,
			"error" | "toolDiffRemoved" | "toolErrorBg" => theme.err = color,
			"muted" | "dim" | "thinkingText" | "toolOutput" | "toolDiffContext" | "statusLineSep" => {
				theme.muted = color
			},
			"border" | "borderMuted" | "mdCodeBlockBorder" | "mdQuoteBorder" | "mdHr" => {
				theme.border = color;
			},
			"selectedBg" => theme.selection = color,
			"toolPendingBg" => theme.surface = color,
			"userMessageBg" | "customMessageBg" | "toolSuccessBg" | "statusLineBg" => {
				theme.panel = color;
			},
			"customMessageLabel" | "pythonMode" | "statusLineSpend" | "statusLineCost" => {
				theme.secondary = color;
			},
			"userMessageText" | "customMessageText" => theme.contrast = color,
			_ => {},
		}
	}
	Ok(theme)
}

fn resolve_color(
	token: &str,
	value: &ColorValue,
	vars: &BTreeMap<String, ColorValue>,
) -> Result<Option<Color>, ThemeError> {
	let mut value = value;
	for _ in 0..16 {
		match value {
			ColorValue::Index(index) => {
				let index =
					u8::try_from(*index).map_err(|_| ThemeError::Index { token: Str::new(token) })?;
				return Ok(Some(Color::Indexed(index)));
			},
			ColorValue::Text(source) if source.is_empty() => return Ok(None),
			ColorValue::Text(source) => {
				if let Some(variable) = vars.get(source) {
					value = variable;
					continue;
				}
				return Color::parse(source)
					.map(Some)
					.ok_or_else(|| ThemeError::Color {
						token: Str::new(token),
						value: Str::new(source.as_str()),
					});
			},
		}
	}
	Err(ThemeError::Color { token: Str::new(token), value: Str::new_static("variable cycle") })
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
	}

	#[test]
	fn rich_pi_slots_and_variables_lower_to_semantic_tokens() {
		let theme = JsonTheme::parse(
			r##"{
			"name":"rich",
			"vars":{"violet":"#8855ee"},
			"colors":{
				"text":"#eeeeee","borderAccent":"violet","success":40,
				"toolDiffRemoved":"#ff3344","statusLineBg":"#101216",
				"syntaxKeyword":"#abcdef"
			}
		}"##,
		)
		.expect("rich theme");
		let dark = theme.for_appearance(Appearance::Dark);
		assert_eq!(dark.fg, Color::Rgb(0xee, 0xee, 0xee));
		assert_eq!(dark.accent, Color::Rgb(0x88, 0x55, 0xee));
		assert_eq!(dark.ok, Color::Indexed(40));
		assert_eq!(dark.err, Color::Rgb(0xff, 0x33, 0x44));
		assert_eq!(dark.panel, Color::Rgb(0x10, 0x12, 0x16));
		assert!(matches!(theme.for_appearance_256(Appearance::Dark).accent, Color::Indexed(_)));
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
