//! Line terminators shared by builtins with zero-terminated output modes.

use std::{fmt, fmt::Display};
/// A newline or NUL record terminator.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum LineEnding {
	/// A newline (`\n`) terminator.
	#[default]
	Newline = b'\n',
	/// A NUL (`\0`) terminator.
	Nul     = 0,
}

impl LineEnding {
	/// Selects NUL when a `-z`/`--zero` flag is set, and newline otherwise.
	#[inline]
	pub(crate) const fn from_zero_flag(is_zero_terminated: bool) -> Self {
		if is_zero_terminated {
			Self::Nul
		} else {
			Self::Newline
		}
	}
}

impl From<LineEnding> for u8 {
	#[inline]
	fn from(line_ending: LineEnding) -> Self {
		line_ending as Self
	}
}

impl Display for LineEnding {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(match self {
			Self::Newline => "\n",
			Self::Nul => "\0",
		})
	}
}

#[cfg(test)]
mod tests {
	use super::LineEnding;

	#[test]
	fn zero_flag_and_byte_mapping() {
		assert_eq!(LineEnding::from_zero_flag(false), LineEnding::Newline);
		assert_eq!(LineEnding::from_zero_flag(true), LineEnding::Nul);
		assert_eq!(u8::from(LineEnding::Newline), b'\n');
		assert_eq!(u8::from(LineEnding::Nul), 0);
		assert_eq!(LineEnding::Newline.to_string(), "\n");
		assert_eq!(LineEnding::Nul.to_string(), "\0");
	}
}
