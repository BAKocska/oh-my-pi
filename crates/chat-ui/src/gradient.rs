//! Editor-owned gradient highlighting seam.
//!
//! Backends provide semantic byte ranges; the UI resolves stable RGB colors at
//! render time without coupling protocol adapters to a terminal color type.

use omp_core::Str;

/// An sRGB color stop used by editor highlighting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GradientStop {
	/// Position in the inclusive 0–255 gradient domain.
	pub at:    u8,
	/// Red channel.
	pub red:   u8,
	/// Green channel.
	pub green: u8,
	/// Blue channel.
	pub blue:  u8,
}

/// A semantic editor highlight resolved against a named gradient.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EditorHighlight {
	/// Stable editor or document identifier.
	pub editor: Str,
	/// UTF-8 byte offset in the editor snapshot.
	pub start:  usize,
	/// UTF-8 byte length.
	pub len:    usize,
	/// Position in the gradient domain.
	pub level:  u8,
}

/// Ordered color stops supplied by the host theme.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EditorGradient {
	stops: Vec<GradientStop>,
}

impl EditorGradient {
	/// Builds a gradient after sorting stops by position. Empty gradients remain
	/// transparent to callers through [`Self::color`].
	#[must_use]
	pub fn new(mut stops: Vec<GradientStop>) -> Self {
		stops.sort_unstable_by_key(|stop| stop.at);
		stops.dedup_by_key(|stop| stop.at);
		Self { stops }
	}

	/// Resolves one level to an interpolated RGB triple.
	#[must_use]
	pub fn color(&self, level: u8) -> Option<(u8, u8, u8)> {
		let first = *self.stops.first()?;
		if level <= first.at {
			return Some((first.red, first.green, first.blue));
		}
		let last = *self.stops.last().expect("non-empty gradient");
		if level >= last.at {
			return Some((last.red, last.green, last.blue));
		}
		let upper = self.stops.partition_point(|stop| stop.at < level);
		let low = self.stops[upper - 1];
		let high = self.stops[upper];
		let width = u16::from(high.at - low.at);
		let offset = u16::from(level - low.at);
		Some((
			mix(low.red, high.red, offset, width),
			mix(low.green, high.green, offset, width),
			mix(low.blue, high.blue, offset, width),
		))
	}
}

fn mix(low: u8, high: u8, offset: u16, width: u16) -> u8 {
	let low = i32::from(low);
	let delta = i32::from(high) - low;
	(low + (delta * i32::from(offset) + i32::from(width / 2)) / i32::from(width)).clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn resolves_sorted_interpolated_stops() {
		let gradient = EditorGradient::new(vec![
			GradientStop { at: 255, red: 100, green: 50, blue: 0 },
			GradientStop { at: 0, red: 0, green: 0, blue: 0 },
		]);
		assert_eq!(gradient.color(128), Some((50, 25, 0)));
	}
}
