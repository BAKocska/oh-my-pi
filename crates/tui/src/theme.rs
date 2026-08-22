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

/// Derives a stable TrueColor accent from a session name and active theme.
///
/// Dark surfaces use the warm 0–120° band. Supplying a light-surface
/// luminance selects the cool 180–300° band and lowers lightness until WCAG
/// 3:1 contrast is met. Occupied theme hues are avoided by at least 10° when
/// the selected band has room.
#[must_use]
pub fn session_accent_color(
	name: &str,
	theme_colors: &[Color],
	surface_luminance: Option<f64>,
) -> Color {
	let (hue_start, hue_end) = if surface_luminance.is_some() {
		(180_u32, 300_u32)
	} else {
		(0_u32, 120_u32)
	};
	let mut hash = 5_381_u32;
	for unit in name.encode_utf16() {
		hash = hash.wrapping_mul(33) ^ u32::from(unit);
	}
	let mut hue = hue_start + hash % (hue_end - hue_start);
	let occupied = theme_colors
		.iter()
		.filter_map(|color| match *color {
			Color::Rgb(red, green, blue) => rgb_hue(red, green, blue),
			_ => None,
		})
		.collect::<Vec<_>>();
	if occupied
		.iter()
		.any(|occupied| hue_distance(f64::from(hue), *occupied) < 10.0)
	{
		'search: for distance in 1..=hue_end - hue_start {
			for candidate in
				[hue.saturating_add(distance).min(hue_end), hue.saturating_sub(distance).max(hue_start)]
			{
				if occupied
					.iter()
					.all(|occupied| hue_distance(f64::from(candidate), *occupied) >= 10.0)
				{
					hue = candidate;
					break 'search;
				}
			}
		}
	}
	let mut lightness = 0.72;
	if let Some(surface_luminance) = surface_luminance {
		let cap = ((surface_luminance + 0.05) / 3.0 - 0.05).max(0.0);
		if relative_luminance(hsl_rgb(f64::from(hue), 0.9, lightness)) > cap {
			let mut low = 0.0;
			let mut high = lightness;
			for _ in 0..20 {
				let middle = (low + high) / 2.0;
				if relative_luminance(hsl_rgb(f64::from(hue), 0.9, middle)) > cap {
					high = middle;
				} else {
					low = middle;
				}
			}
			lightness = low;
		}
	}
	let [red, green, blue] = hsl_rgb(f64::from(hue), 0.9, lightness);
	Color::Rgb(red, green, blue)
}

fn rgb_hue(red: u8, green: u8, blue: u8) -> Option<f64> {
	let red = f64::from(red) / 255.0;
	let green = f64::from(green) / 255.0;
	let blue = f64::from(blue) / 255.0;
	let maximum = red.max(green).max(blue);
	let minimum = red.min(green).min(blue);
	let delta = maximum - minimum;
	if maximum == 0.0 || delta / maximum < 0.1 {
		return None;
	}
	let sector = if maximum == red {
		((green - blue) / delta).rem_euclid(6.0)
	} else if maximum == green {
		(blue - red) / delta + 2.0
	} else {
		(red - green) / delta + 4.0
	};
	Some(sector * 60.0)
}

fn hue_distance(left: f64, right: f64) -> f64 {
	let distance = (left - right).abs();
	distance.min(360.0 - distance)
}

fn hsl_rgb(hue: f64, saturation: f64, lightness: f64) -> [u8; 3] {
	let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
	let sector = hue / 60.0;
	let secondary = chroma * (1.0 - (sector.rem_euclid(2.0) - 1.0).abs());
	let (red, green, blue) = match sector as u8 {
		0 => (chroma, secondary, 0.0),
		1 => (secondary, chroma, 0.0),
		2 => (0.0, chroma, secondary),
		3 => (0.0, secondary, chroma),
		4 => (secondary, 0.0, chroma),
		_ => (chroma, 0.0, secondary),
	};
	let offset = lightness - chroma / 2.0;
	[
		((red + offset) * 255.0).round().clamp(0.0, 255.0) as u8,
		((green + offset) * 255.0).round().clamp(0.0, 255.0) as u8,
		((blue + offset) * 255.0).round().clamp(0.0, 255.0) as u8,
	]
}

fn relative_luminance([red, green, blue]: [u8; 3]) -> f64 {
	let linear = |value: u8| {
		let value = f64::from(value) / 255.0;
		if value <= 0.04045 {
			value / 12.92
		} else {
			((value + 0.055) / 1.055).powf(2.4)
		}
	};
	0.2126 * linear(red) + 0.7152 * linear(green) + 0.0722 * linear(blue)
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
