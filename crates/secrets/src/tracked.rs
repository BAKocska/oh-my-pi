//! Origin-aware text replacement used by the secret transform pipeline.

use std::ops::Range;

/// Origin of bytes in a transformed text buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Origin {
	/// Byte copied from the caller's input.
	Input,
	/// Byte inserted by the current transform.
	Fresh,
}

/// Text paired with one origin tag per byte.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackedText {
	text:   String,
	origin: Vec<Origin>,
}

impl TrackedText {
	/// Creates a tracked input whose bytes all originate with the caller.
	#[must_use]
	pub fn input(text: &str) -> Self {
		Self { text: text.to_owned(), origin: vec![Origin::Input; text.len()] }
	}

	/// Returns the transformed text.
	#[must_use]
	pub fn as_str(&self) -> &str {
		&self.text
	}

	/// Returns the origin tags over a byte range.
	#[must_use]
	pub fn origins(&self, range: Range<usize>) -> &[Origin] {
		&self.origin[range]
	}

	/// Replaces a byte range while retaining an explicit origin for inserted
	/// bytes.
	pub fn replace(&mut self, range: Range<usize>, replacement: &str, origin: Origin) {
		debug_assert!(self.text.is_char_boundary(range.start));
		debug_assert!(self.text.is_char_boundary(range.end));
		let origin_range = range.clone();
		self.text.replace_range(range, replacement);
		self
			.origin
			.splice(origin_range, std::iter::repeat_n(origin, replacement.len()));
	}

	/// Replaces non-overlapping ranges from right to left.
	pub fn replace_ranges(&mut self, replacements: &mut [(Range<usize>, String, Origin)]) {
		replacements.sort_unstable_by(|left, right| right.0.start.cmp(&left.0.start));
		for (range, replacement, origin) in replacements.iter() {
			self.replace(range.clone(), replacement, *origin);
		}
	}

	/// Consumes this value and returns its text.
	#[must_use]
	pub fn into_string(self) -> String {
		self.text
	}
}

/// Returns ranges outside syntactically complete `$$…$$` placeholders.
///
/// The predicate decides which complete tokens are trusted. Unknown tokens
/// remain ordinary text.
pub fn outside_placeholder_ranges<'a>(
	text: &'a str,
	mut trusted: impl FnMut(&'a str) -> bool,
) -> impl Iterator<Item = Range<usize>> + 'a {
	let mut ranges = Vec::new();
	let mut outside_start = 0;
	let mut scan = 0;
	while let Some(relative_start) = text[scan..].find("$$") {
		let start = scan + relative_start;
		let Some(relative_end) = text[start + 2..].find("$$") else {
			break;
		};
		let end = start + 2 + relative_end + 2;
		if trusted(&text[start..end]) {
			if outside_start < start {
				ranges.push(outside_start..start);
			}
			outside_start = end;
			scan = end;
		} else {
			scan = start + 2;
		}
	}
	if outside_start < text.len() {
		ranges.push(outside_start..text.len());
	}
	ranges.into_iter()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn maps_only_outside_trusted_placeholders() {
		let text = "a$$KNOWN$$b$$UNKNOWN$$c";
		let ranges =
			outside_placeholder_ranges(text, |token| token == "$$KNOWN$$").collect::<Vec<_>>();
		assert_eq!(
			ranges
				.iter()
				.map(|range| &text[range.clone()])
				.collect::<Vec<_>>(),
			["a", "b$$UNKNOWN$$c"]
		);
	}
}
