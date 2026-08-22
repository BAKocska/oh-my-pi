//! Parsers for GNU size, signed-count, and duration arguments.

use std::{num::IntErrorKind, time::Duration};

use bigdecimal::BigDecimal;
use num_traits::{FromPrimitive, Signed, ToPrimitive, Zero};
use thiserror::Error;

use super::num::{ExtendedBigDecimal, ExtendedParserError, parse_duration_number};

const SI_BASES: [u128; 11] = [
	1,
	1_000,
	1_000_000,
	1_000_000_000,
	1_000_000_000_000,
	1_000_000_000_000_000,
	1_000_000_000_000_000_000,
	1_000_000_000_000_000_000_000,
	1_000_000_000_000_000_000_000_000,
	1_000_000_000_000_000_000_000_000_000,
	1_000_000_000_000_000_000_000_000_000_000,
];

const IEC_BASES: [u128; 11] = [
	1,
	1_024,
	1_048_576,
	1_073_741_824,
	1_099_511_627_776,
	1_125_899_906_842_624,
	1_152_921_504_606_846_976,
	1_180_591_620_717_411_303_424,
	1_208_925_819_614_629_174_706_176,
	1_237_940_039_285_380_274_899_124_224,
	1_267_650_600_228_229_401_496_703_205_376,
];

/// An error encountered while parsing a GNU size argument.
#[derive(Debug, PartialEq, Eq, Error)]
pub(crate) enum ParseSizeError {
	/// The numeric prefix is followed by an unsupported suffix.
	#[error("{0}")]
	InvalidSuffix(String),
	/// The input does not contain a valid number.
	#[error("{0}")]
	ParseFailure(String),
	/// The represented size does not fit in the requested integer type.
	#[error("{0}")]
	SizeTooBig(String),
	/// Physical memory could not be determined for a percentage size.
	#[error("{0}")]
	PhysicalMem(String),
}

impl ParseSizeError {
	fn quoted(input: &str) -> String {
		format!("'{input}'")
	}

	fn invalid_suffix(input: &str) -> Self {
		Self::InvalidSuffix(Self::quoted(input))
	}

	fn parse_failure(input: &str) -> Self {
		Self::ParseFailure(Self::quoted(input))
	}

	fn size_too_big(input: &str) -> Self {
		Self::SizeTooBig(format!("{}: Value too large for defined data type", Self::quoted(input)))
	}
}

#[derive(Clone, Copy)]
enum NumberSystem {
	Decimal,
	Octal,
	Hexadecimal,
	Binary,
}

/// Configurable parser for SI and IEC byte-size arguments.
#[derive(Default)]
pub(crate) struct Parser<'parser> {
	/// Whether an omitted numeric portion is rejected.
	pub(crate) no_empty_numeric: bool,
	/// Whether an uppercase `B` suffix means bytes.
	pub(crate) capital_b_bytes:  bool,
	/// Whether a trailing lowercase `b` is removed rather than treated as
	/// 512-byte blocks.
	pub(crate) b_byte_count:     bool,
	/// Optional suffix whitelist.
	pub(crate) allow_list:       Option<&'parser [&'parser str]>,
	/// Unit applied when the input has no suffix.
	pub(crate) default_unit:     Option<&'parser str>,
}

impl<'parser> Parser<'parser> {
	/// Restricts accepted non-empty suffixes to `allow_list`.
	pub(crate) fn with_allow_list(&mut self, allow_list: &'parser [&str]) -> &mut Self {
		self.allow_list = Some(allow_list);
		self
	}

	/// Sets the unit used when no suffix is present.
	pub(crate) fn with_default_unit(&mut self, default_unit: &'parser str) -> &mut Self {
		self.default_unit = Some(default_unit);
		self
	}

	/// Selects whether lowercase `b` is a byte-count marker instead of a block
	/// suffix.
	pub(crate) fn with_b_byte_count(&mut self, value: bool) -> &mut Self {
		self.b_byte_count = value;
		self
	}

	/// Parses a size as an unsigned 128-bit byte count.
	pub(crate) fn parse(&self, size: &str) -> Result<u128, ParseSizeError> {
		if size.is_empty() {
			return Err(ParseSizeError::parse_failure(size));
		}
		let number_system = Self::determine_number_system(size);
		let numeric_len = match number_system {
			NumberSystem::Hexadecimal => {
				2 + size.as_bytes()[2..]
					.iter()
					.take_while(|byte| byte.is_ascii_hexdigit())
					.count()
			},
			NumberSystem::Binary => {
				2 + size.as_bytes()[2..]
					.iter()
					.take_while(|byte| matches!(byte, b'0' | b'1'))
					.count()
			},
			NumberSystem::Decimal | NumberSystem::Octal => size
				.as_bytes()
				.iter()
				.take_while(|byte| byte.is_ascii_digit())
				.count(),
		};
		let numeric = &size[..numeric_len];
		let mut unit = &size[numeric_len..];
		if unit.is_empty()
			&& let Some(default_unit) = self.default_unit
		{
			unit = default_unit;
		}
		if self.b_byte_count && unit.ends_with('b') {
			if numeric.is_empty() {
				return Err(ParseSizeError::parse_failure(size));
			}
			unit = &unit[..unit.len() - 1];
		}
		if let Some(allow_list) = self.allow_list
			&& !unit.is_empty()
			&& !allow_list.contains(&unit)
		{
			return Err(if numeric.is_empty() {
				ParseSizeError::parse_failure(size)
			} else {
				ParseSizeError::invalid_suffix(size)
			});
		}
		if unit == "%" {
			let percent = Self::parse_number(numeric, 10, size)?;
			let total = total_physical_memory_bytes()
				.ok_or_else(|| ParseSizeError::PhysicalMem(size.to_owned()))?;
			return Ok((percent / 100).saturating_mul(total));
		}
		let factor = if unit == "B" && self.capital_b_bytes {
			Some(1)
		} else {
			suffix_factor(unit)
		}
		.ok_or_else(|| {
			if numeric.is_empty() {
				ParseSizeError::parse_failure(size)
			} else {
				ParseSizeError::invalid_suffix(size)
			}
		})?;
		let number = match number_system {
			NumberSystem::Decimal if numeric.is_empty() && !self.no_empty_numeric => 1,
			NumberSystem::Decimal => Self::parse_number(numeric, 10, size)?,
			NumberSystem::Octal => Self::parse_number(numeric.trim_start_matches('0'), 8, size)?,
			NumberSystem::Hexadecimal => {
				Self::parse_number(numeric.trim_start_matches("0x"), 16, size)?
			},
			NumberSystem::Binary => Self::parse_number(numeric.trim_start_matches("0b"), 2, size)?,
		};
		number
			.checked_mul(factor)
			.ok_or_else(|| ParseSizeError::size_too_big(size))
	}

	/// Parses a size as an unsigned 64-bit byte count.
	pub(crate) fn parse_u64(&self, size: &str) -> Result<u64, ParseSizeError> {
		let value = self.parse(size)?;
		u64::try_from(value).map_err(|_| ParseSizeError::size_too_big(size))
	}

	/// Parses a 64-bit size, saturating overflow to [`u64::MAX`].
	pub(crate) fn parse_u64_max(&self, size: &str) -> Result<u64, ParseSizeError> {
		match self.parse_u64(size) {
			Err(ParseSizeError::SizeTooBig(_)) => Ok(u64::MAX),
			result => result,
		}
	}

	fn determine_number_system(size: &str) -> NumberSystem {
		if size.len() <= 1 {
			return NumberSystem::Decimal;
		}
		if size.starts_with("0x") {
			return NumberSystem::Hexadecimal;
		}
		if size.strip_prefix("0b").is_some_and(|rest| !rest.is_empty()) {
			return NumberSystem::Binary;
		}
		let digit_count = size
			.as_bytes()
			.iter()
			.take_while(|byte| byte.is_ascii_digit())
			.count();
		let all_zeroes = size.as_bytes().iter().all(|byte| *byte == b'0');
		if size.starts_with('0') && digit_count > 1 && !all_zeroes {
			NumberSystem::Octal
		} else {
			NumberSystem::Decimal
		}
	}

	fn parse_number(numeric: &str, radix: u32, original: &str) -> Result<u128, ParseSizeError> {
		u128::from_str_radix(numeric, radix).map_err(|error| match error.kind() {
			IntErrorKind::PosOverflow => ParseSizeError::size_too_big(original),
			_ => ParseSizeError::parse_failure(original),
		})
	}
}

fn suffix_factor(unit: &str) -> Option<u128> {
	if unit.is_empty() {
		return Some(1);
	}
	if unit == "b" {
		return Some(512);
	}
	let bytes = unit.as_bytes();
	let first = *bytes.first()?;
	let exponent = match first.to_ascii_uppercase() {
		b'K' => 1,
		b'M' => 2,
		b'G' => 3,
		b'T' => 4,
		b'P' => 5,
		b'E' => 6,
		b'Z' => 7,
		b'Y' => 8,
		b'R' => 9,
		b'Q' => 10,
		_ => return None,
	};
	match &bytes[1..] {
		[] => Some(IEC_BASES[exponent]),
		[b'i', b'B'] => Some(IEC_BASES[exponent]),
		[b'B' | b'D'] => Some(SI_BASES[exponent]),
		_ => None,
	}
}

/// Produces every bare, IEC, SI, and decimal suffix for the characters in
/// `units`.
pub(crate) fn allow_list_with_all_suffixes(units: &str) -> Vec<String> {
	let mut allow_list = Vec::with_capacity(4 * units.len());
	for unit in units.chars() {
		for suffix in ["", "iB", "B", "D"] {
			allow_list.push(format!("{unit}{suffix}"));
		}
	}
	allow_list
}

/// Parses a 64-bit size with the default parser.
pub(crate) fn parse_size_u64(size: &str) -> Result<u64, ParseSizeError> {
	Parser::default().parse_u64(size)
}

/// Parses a 64-bit size, saturating overflow to [`u64::MAX`].
pub(crate) fn parse_size_u64_max(size: &str) -> Result<u64, ParseSizeError> {
	Parser::default().parse_u64_max(size)
}

/// Parses a nonzero 64-bit size with the default parser.
pub(crate) fn parse_size_non_zero_u64(size: &str) -> Result<u64, ParseSizeError> {
	let value = parse_size_u64(size)?;
	if value == 0 {
		Err(ParseSizeError::ParseFailure("0".to_owned()))
	} else {
		Ok(value)
	}
}

/// The optional direction sign on a head/tail count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignPrefix {
	/// A leading plus sign.
	Plus,
	/// A leading minus sign.
	Minus,
}

/// A parsed count and its optional direction sign.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SignedNum {
	/// The unsigned count.
	pub(crate) value: u64,
	/// The sign present in the source, if any.
	pub(crate) sign:  Option<SignPrefix>,
}

fn strip_sign_prefix(source: &str) -> (Option<SignPrefix>, &str) {
	let source = source.trim();
	if let Some(rest) = source.strip_prefix('+') {
		(Some(SignPrefix::Plus), rest)
	} else if let Some(rest) = source.strip_prefix('-') {
		(Some(SignPrefix::Minus), rest)
	} else {
		(None, source)
	}
}

/// Parses a signed count and saturates overflow to [`u64::MAX`].
pub(crate) fn parse_signed_num_max(source: &str) -> Result<SignedNum, ParseSizeError> {
	let (sign, size) = strip_sign_prefix(source);
	if size.is_empty() {
		return Err(ParseSizeError::ParseFailure(source.to_owned()));
	}
	let trimmed = size.trim_start_matches('0');
	let value = if trimmed.is_empty() {
		0
	} else {
		parse_size_u64_max(trimmed)?
	};
	Ok(SignedNum { value, sign })
}

/// An invalid duration argument.
#[derive(Debug, PartialEq, Eq, Error)]
#[error("invalid time interval '{input}'")]
pub(crate) struct ParseTimeError {
	input: String,
}

/// Duration-string parsing compatible with GNU interval arguments.
pub(crate) mod parse_time {
	use super::*;

	/// Parses seconds with optional `s`, `m`, `h`, or `d` suffixes.
	///
	/// Overflow saturates to [`Duration::MAX`], while a positive value below
	/// one nanosecond rounds up to one nanosecond.
	pub(crate) fn from_str(input: &str, allow_suffixes: bool) -> Result<Duration, ParseTimeError> {
		let invalid = || ParseTimeError { input: input.to_owned() };
		if input.is_empty() {
			return Err(invalid());
		}
		let parsed = parse_duration_number(
			input,
			if allow_suffixes {
				&[('s', 1), ('m', 60), ('h', 3_600), ('d', 86_400)]
			} else {
				&[]
			},
		);
		let number = match parsed {
			Ok(value) | Err(ExtendedParserError::Overflow(value)) => value,
			Err(ExtendedParserError::Underflow(_)) => return Ok(Duration::from_nanos(1)),
			Err(ExtendedParserError::NotNumeric | ExtendedParserError::PartialMatch(..)) => {
				return Err(invalid());
			},
		};
		let decimal = match number {
			ExtendedBigDecimal::BigDecimal(value) if !value.is_negative() => {
				if value.fractional_digit_count() <= -20 {
					return Ok(Duration::MAX);
				}
				let tenth_nanosecond =
					BigDecimal::from_f64(0.000_000_000_1).expect("constant converts to BigDecimal");
				if !value.is_zero() && value < tenth_nanosecond {
					return Ok(Duration::from_nanos(1));
				}
				value
			},
			ExtendedBigDecimal::MinusZero => BigDecimal::zero(),
			ExtendedBigDecimal::Infinity => return Ok(Duration::MAX),
			ExtendedBigDecimal::MinusInfinity
			| ExtendedBigDecimal::Nan
			| ExtendedBigDecimal::MinusNan
			| ExtendedBigDecimal::BigDecimal(_) => return Err(invalid()),
		};
		let (nanoseconds, _) = decimal.with_scale(9).into_bigint_and_scale();
		if nanoseconds.is_zero() && !decimal.is_zero() {
			return Ok(Duration::from_nanos(1));
		}
		const NANOS_PER_SECOND: u32 = 1_000_000_000;
		let Ok(seconds) = u64::try_from(&nanoseconds / NANOS_PER_SECOND) else {
			return Ok(Duration::MAX);
		};
		let nanos = (&nanoseconds % NANOS_PER_SECOND)
			.to_u32()
			.expect("non-negative subsecond remainder fits u32");
		Ok(Duration::new(seconds, nanos))
	}
}

#[cfg(target_os = "linux")]
fn available_pages_and_size() -> Option<(u128, u128)> {
	// SAFETY: `sysconf` has no pointer arguments and these selectors are valid.
	let pages = unsafe { libc::sysconf(libc::_SC_AVPHYS_PAGES) };
	// SAFETY: See above.
	let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
	(pages > 0 && page_size > 0).then_some((pages as u128, page_size as u128))
}

/// Returns the operating system's estimate of currently available memory.
#[cfg(target_os = "linux")]
pub(crate) fn available_memory_bytes() -> Option<u128> {
	let (pages, page_size) = available_pages_and_size()?;
	Some(pages.saturating_mul(page_size))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn total_physical_memory_bytes() -> Option<u128> {
	// SAFETY: `sysconf` has no pointer arguments and these selectors are valid.
	let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
	// SAFETY: See above.
	let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
	(pages > 0 && page_size > 0).then_some((pages as u128).saturating_mul(page_size as u128))
}

#[cfg(target_os = "macos")]
fn total_physical_memory_bytes() -> Option<u128> {
	let mut size = std::mem::size_of::<u64>();
	let mut bytes = 0_u64;
	// SAFETY: The output buffer is a `u64`, and `size` accurately describes it.
	let result = unsafe {
		libc::sysctlbyname(
			c"hw.memsize".as_ptr(),
			std::ptr::from_mut(&mut bytes).cast(),
			&mut size,
			std::ptr::null_mut(),
			0,
		)
	};
	(result == 0).then_some(u128::from(bytes))
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
fn total_physical_memory_bytes() -> Option<u128> {
	None
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_suffix_table() {
		let parser = Parser::default();
		let cases = [
			("1b", 512),
			("1K", 1_024),
			("1KiB", 1_024),
			("1KB", 1_000),
			("2M", 2 * 1_048_576),
			("2MB", 2_000_000),
			("1G", 1_073_741_824),
			("1T", 1_099_511_627_776),
			("1P", 1_125_899_906_842_624),
			("1E", 1_152_921_504_606_846_976),
		];
		for (source, expected) in cases {
			assert_eq!(parser.parse(source), Ok(expected), "{source}");
		}
	}

	#[test]
	fn rejects_bad_sizes_and_zero() {
		assert!(matches!(Parser::default().parse("12XB"), Err(ParseSizeError::InvalidSuffix(_))));
		assert!(matches!(Parser::default().parse("hello"), Err(ParseSizeError::ParseFailure(_))));
		assert!(matches!(Parser::default().parse_u64("16E"), Err(ParseSizeError::SizeTooBig(_))));
		assert!(parse_size_non_zero_u64("0").is_err());
	}

	#[test]
	fn builder_applies_defaults_and_allow_lists() {
		let mut parser = Parser::default();
		assert_eq!(parser.with_default_unit("K").parse("2"), Ok(2_048));
		let mut parser = Parser::default();
		assert_eq!(
			parser.with_allow_list(&["K"]).parse("2M"),
			Err(ParseSizeError::InvalidSuffix("'2M'".to_owned()))
		);
		assert_eq!(allow_list_with_all_suffixes("K"), ["K", "KiB", "KB", "KD"]);
	}

	#[test]
	fn parses_signed_prefixes_as_decimal() {
		let plain = parse_signed_num_max("007").unwrap();
		assert_eq!(plain, SignedNum { value: 7, sign: None });
		assert_eq!(parse_signed_num_max("+5K").unwrap().value, 5 * 1_024);
		assert_eq!(parse_signed_num_max("-5").unwrap().sign, Some(SignPrefix::Minus));
		assert_eq!(
			parse_signed_num_max("999999999999999999999999")
				.unwrap()
				.value,
			u64::MAX
		);
		assert!(parse_signed_num_max("+").is_err());
	}

	#[test]
	fn parses_duration_units_and_fractional_seconds() {
		assert_eq!(parse_time::from_str("1.5", true), Ok(Duration::from_millis(1_500)));
		assert_eq!(parse_time::from_str("2m", true), Ok(Duration::from_secs(120)));
		assert_eq!(parse_time::from_str("0.0000000001", true), Ok(Duration::from_nanos(1)));
		assert_eq!(parse_time::from_str("inf", true), Ok(Duration::MAX));
		assert!(parse_time::from_str("2d", false).is_err());
		assert!(parse_time::from_str("-1", true).is_err());
	}
}
