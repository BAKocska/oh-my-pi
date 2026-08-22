//! Human-readable byte-size formatting compatible with GNU coreutils.

/// The output unit system for [`human_readable`].
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum SizeFormat {
	/// Print the exact byte count without a suffix.
	Bytes,
	/// Use powers of 1024 and GNU's `K`, `M`, … suffixes.
	Binary,
	/// Use powers of 1000 and SI `k`, `M`, … suffixes.
	Decimal,
}

fn prefixed(size: u64, base: u64, suffixes: &[&str]) -> String {
	let mut exponent = 0_usize;
	let mut unit = 1_u64;
	while exponent + 1 < suffixes.len() && size >= unit.saturating_mul(base) {
		unit *= base;
		exponent += 1;
	}
	if exponent == 0 {
		return size.to_string();
	}
	let size = u128::from(size);
	let unit = u128::from(unit);
	let tenths = size.saturating_mul(10).div_ceil(unit);
	if tenths < 100 {
		format!("{}.{}{}", tenths / 10, tenths % 10, suffixes[exponent])
	} else {
		format!("{}{}", size.div_ceil(unit), suffixes[exponent])
	}
}

/// Formats a byte count using GNU's upward rounding and abbreviated suffixes.
///
/// Prefixed values below ten retain one decimal place; values at least ten
/// are rounded upward to a whole unit.
pub(crate) fn human_readable(size: u64, format: SizeFormat) -> String {
	match format {
		SizeFormat::Bytes => size.to_string(),
		SizeFormat::Binary => prefixed(size, 1_024, &["", "K", "M", "G", "T", "P", "E"]),
		SizeFormat::Decimal => prefixed(size, 1_000, &["", "k", "M", "G", "T", "P", "E"]),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn binary_boundaries_round_up_like_gnu() {
		let cases = [
			(0, "0"),
			(1_023, "1023"),
			(1_024, "1.0K"),
			(1_536, "1.5K"),
			(8_500, "8.4K"),
			(10 * 1_024, "10K"),
			(12 * 1_024 * 1_024, "12M"),
			(133_456_345, "128M"),
		];
		for (size, expected) in cases {
			assert_eq!(human_readable(size, SizeFormat::Binary), expected, "{size}");
		}
	}

	#[test]
	fn decimal_and_bytes_boundaries() {
		assert_eq!(human_readable(999, SizeFormat::Decimal), "999");
		assert_eq!(human_readable(1_000, SizeFormat::Decimal), "1.0k");
		assert_eq!(human_readable(1_501, SizeFormat::Decimal), "1.6k");
		assert_eq!(human_readable(10_000, SizeFormat::Decimal), "10k");
		assert_eq!(human_readable(1_024, SizeFormat::Bytes), "1024");
	}
}
