//! GNU file-version comparison used by `sort -V` and `ls -v`.

use std::cmp::Ordering;

fn compare_non_digits(left: &[u8], right: &[u8]) -> Ordering {
	let mut left = left.iter();
	let mut right = right.iter();
	loop {
		match (left.next(), right.next()) {
			(Some(a), Some(b)) if a == b => {},
			(None, None) => return Ordering::Equal,
			(_, Some(b'~')) => return Ordering::Greater,
			(Some(b'~'), _) => return Ordering::Less,
			(None, Some(_)) => return Ordering::Less,
			(Some(_), None) => return Ordering::Greater,
			(Some(a), Some(b)) if a.is_ascii_alphabetic() && !b.is_ascii_alphabetic() => {
				return Ordering::Less;
			},
			(Some(a), Some(b)) if !a.is_ascii_alphabetic() && b.is_ascii_alphabetic() => {
				return Ordering::Greater;
			},
			(Some(a), Some(b)) => return a.cmp(b),
		}
	}
}

/// Removes file endings matching `(?:\.[A-Za-z~][A-Za-z0-9~]*)*$`.
fn remove_file_ending(input: &[u8]) -> &[u8] {
	let mut ending_start = None;
	let mut previous_was_dot = false;
	for (index, &byte) in input.iter().enumerate() {
		if byte == b'.' {
			if ending_start.is_none() || previous_was_dot {
				ending_start = Some(index);
			}
			previous_was_dot = true;
		} else if previous_was_dot {
			previous_was_dot = false;
			if !byte.is_ascii_alphabetic() && byte != b'~' {
				ending_start = None;
			}
		} else if !byte.is_ascii_alphanumeric() && byte != b'~' {
			ending_start = None;
		}
	}
	if previous_was_dot {
		ending_start = None;
	}
	ending_start.map_or(input, |start| &input[..start])
}

/// Compares file names using GNU's file-version ordering.
///
/// Both UTF-8 strings and raw platform filename bytes are accepted without
/// allocation. Digit runs compare by magnitude without integer conversion,
/// leading zeroes are insignificant, tilde sorts before every non-empty
/// component, and a common extension class is ignored when the remaining
/// stems differ.
pub(crate) fn version_cmp(left: impl AsRef<[u8]>, right: impl AsRef<[u8]>) -> Ordering {
	let mut left = left.as_ref();
	let mut right = right.as_ref();
	let lexical = left.cmp(right);
	if lexical == Ordering::Equal {
		return Ordering::Equal;
	}
	match (left.is_empty(), right.is_empty()) {
		(true, false) => return Ordering::Less,
		(false, true) => return Ordering::Greater,
		(true, true) => unreachable!(),
		(false, false) => {},
	}
	match (left == b".", right == b".") {
		(true, false) => return Ordering::Less,
		(false, true) => return Ordering::Greater,
		_ => {},
	}
	match (left == b"..", right == b"..") {
		(true, false) => return Ordering::Less,
		(false, true) => return Ordering::Greater,
		_ => {},
	}
	match (left.starts_with(b"."), right.starts_with(b".")) {
		(true, false) => return Ordering::Less,
		(false, true) => return Ordering::Greater,
		(true, true) => {
			left = &left[1..];
			right = &right[1..];
		},
		(false, false) => {},
	}
	(left, right) = match (remove_file_ending(left), remove_file_ending(right)) {
		(stripped_left, stripped_right) if stripped_left == stripped_right => (left, right),
		stripped => stripped,
	};
	while !left.is_empty() || !right.is_empty() {
		let left_digit = left
			.iter()
			.position(u8::is_ascii_digit)
			.unwrap_or(left.len());
		let right_digit = right
			.iter()
			.position(u8::is_ascii_digit)
			.unwrap_or(right.len());
		match compare_non_digits(&left[..left_digit], &right[..right_digit]) {
			Ordering::Equal => {},
			ordering => return ordering,
		}
		left = &left[left_digit..];
		right = &right[right_digit..];
		let left_end = left
			.iter()
			.position(|byte| !byte.is_ascii_digit())
			.unwrap_or(left.len());
		let right_end = right
			.iter()
			.position(|byte| !byte.is_ascii_digit())
			.unwrap_or(right.len());
		let left_number = &left[left[..left_end]
			.iter()
			.position(|byte| *byte != b'0')
			.unwrap_or(left_end)..left_end];
		let right_number = &right[right[..right_end]
			.iter()
			.position(|byte| *byte != b'0')
			.unwrap_or(right_end)..right_end];
		match left_number.len().cmp(&right_number.len()) {
			Ordering::Equal => {},
			ordering => return ordering,
		}
		match left_number.cmp(right_number) {
			Ordering::Equal => {},
			ordering => return ordering,
		}
		left = &left[left_end..];
		right = &right[right_end..];
	}
	Ordering::Equal
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn gnu_filever_vectors() {
		let cases = [
			("hello", "hello", Ordering::Equal),
			("file12", "file12", Ordering::Equal),
			("file12-suffix", "file12-suffix", Ordering::Equal),
			("file12-suffix24", "file12-suffix24", Ordering::Equal),
			("world", "wo", Ordering::Greater),
			("hello10wo", "hello10world", Ordering::Less),
			("world", "hello", Ordering::Greater),
			("hello", "world", Ordering::Less),
			("apple", "ant", Ordering::Greater),
			("ant", "apple", Ordering::Less),
			("Beef", "apple", Ordering::Less),
			("Apple", "apple", Ordering::Less),
			("apple", "aPple", Ordering::Greater),
			("100", "20", Ordering::Greater),
			("20", "20", Ordering::Equal),
			("15", "200", Ordering::Less),
			("1000", "apple", Ordering::Less),
			("file1000", "fileapple", Ordering::Less),
			("012", "12", Ordering::Equal),
			("000800", "0000800", Ordering::Equal),
			("ab10", "aa11", Ordering::Greater),
			("aa10", "aa11", Ordering::Less),
			("aa2", "aa100", Ordering::Less),
			("aa10bb", "aa11aa", Ordering::Less),
			("aa10aa0010", "aa11aa1", Ordering::Less),
			("aa10aa0010", "aa10aa1", Ordering::Greater),
			("aa10aa0010", "aa00010aa1", Ordering::Greater),
			("aa10aa0022", "aa010aa022", Ordering::Equal),
			("file-1.4", "file-1.13", Ordering::Less),
			("aa2000000000000000000000bb", "aa002000000000000000000001bb", Ordering::Less),
			("aa2000000000000000000000bb", "aa002000000000000000000000bb", Ordering::Equal),
			("  a", "a", Ordering::Greater),
			("a~", "ab", Ordering::Less),
			("a~", "a", Ordering::Less),
			("~", "", Ordering::Greater),
			(".f", ".1", Ordering::Greater),
			("a..a", "a.+", Ordering::Less),
			("a.", "a+", Ordering::Greater),
			("a\0a", "a", Ordering::Greater),
		];
		for (left, right, expected) in cases {
			assert_eq!(version_cmp(left, right), expected, "{left:?} versus {right:?}");
		}
	}

	#[test]
	fn suffix_and_dot_extensions() {
		assert_eq!(version_cmp("foo-1.0.tar.gz", "foo-1.1.tar.gz"), Ordering::Less);
		assert_eq!(version_cmp(b"file\xff2", b"file\xff10"), Ordering::Less);
		assert_eq!(version_cmp("foo-1.0.tar.gz", "foo-1.0.tar.xz"), Ordering::Less);
		assert_eq!(version_cmp(".", ".."), Ordering::Less);
		assert_eq!(version_cmp("..", ".hidden"), Ordering::Less);
		assert_eq!(version_cmp(".hidden", "visible"), Ordering::Less);
	}
}
