//! One-based selection ranges used by column-oriented builtins.

use std::{cmp::max, str::FromStr};

use super::quote::Quotable;

/// An inclusive, one-based range of positions.
#[derive(PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct Range {
	/// The lower bound of the range.
	pub(crate) low:  usize,
	/// The upper bound of the range.
	pub(crate) high: usize,
}

impl FromStr for Range {
	type Err = &'static str;

	fn from_str(source: &str) -> Result<Self, Self::Err> {
		fn parse(endpoint: &str) -> Result<usize, &'static str> {
			match endpoint.parse::<usize>() {
				Ok(0) => Err("fields and positions are numbered from 1"),
				Ok(usize::MAX) => Err("byte/character offset is too large"),
				Ok(value) => Ok(value),
				Err(_) => Err("failed to parse range"),
			}
		}

		match source.split_once('-') {
			None => {
				let value = parse(source)?;
				Ok(Self { low: value, high: value })
			},
			Some(("", "")) => Err("invalid range with no endpoint"),
			Some((low, "")) => Ok(Self { low: parse(low)?, high: usize::MAX - 1 }),
			Some(("", high)) => Ok(Self { low: 1, high: parse(high)? }),
			Some((low, high)) => {
				let low = parse(low)?;
				let high = parse(high)?;
				if low > high {
					return Err("high end of range less than low end");
				}
				Ok(Self { low, high })
			},
		}
	}
}

impl Range {
	/// Parses a comma- or space-separated list and merges overlapping ranges.
	pub(crate) fn from_list(list: &str) -> Result<Vec<Self>, String> {
		let ranges = list
			.split([',', ' '])
			.map(|item| {
				item
					.parse()
					.map_err(|error| format!("range {} was invalid: {error}", item.quote()))
			})
			.collect::<Result<Vec<_>, _>>()?;
		Ok(Self::merge(ranges))
	}

	fn merge(mut ranges: Vec<Self>) -> Vec<Self> {
		ranges.sort();
		let mut index = 0;
		while index < ranges.len() {
			let next = index + 1;
			while next < ranges.len() && ranges[next].low <= ranges[index].high {
				let next_high = ranges.remove(next).high;
				ranges[index].high = max(ranges[index].high, next_high);
			}
			index += 1;
		}
		ranges
	}
}

/// Returns the complement of sorted, disjoint ranges over valid positions.
pub(crate) fn complement(ranges: &[Range]) -> Vec<Range> {
	let mut previous_high = 0;
	let mut result = Vec::with_capacity(ranges.len() + 1);
	for range in ranges {
		if range.low > previous_high + 1 {
			result.push(Range { low: previous_high + 1, high: range.low - 1 });
		}
		previous_high = range.high;
	}
	if previous_high < usize::MAX - 1 {
		result.push(Range { low: previous_high + 1, high: usize::MAX - 1 });
	}
	result
}

#[cfg(test)]
mod tests {
	use super::{Range, complement};

	fn range(low: usize, high: usize) -> Range {
		Range { low, high }
	}

	#[test]
	fn parses_all_supported_forms() {
		assert_eq!("5".parse(), Ok(range(5, 5)));
		assert_eq!("4-".parse(), Ok(range(4, usize::MAX - 1)));
		assert_eq!("-4".parse(), Ok(range(1, 4)));
		assert_eq!("2-4".parse(), Ok(range(2, 4)));
		assert!("0-4".parse::<Range>().is_err());
		assert!("4-2".parse::<Range>().is_err());
		assert!("-".parse::<Range>().is_err());
	}

	#[test]
	fn lists_merge_overlaps_but_not_adjacencies() {
		assert_eq!(Range::from_list("6-7,1-3 2-4").unwrap(), vec![range(1, 4), range(6, 7)]);
		assert_eq!(Range::from_list("1-3,4-6").unwrap(), vec![range(1, 3), range(4, 6)]);
	}

	#[test]
	fn computes_complements() {
		assert_eq!(complement(&[range(1, 3), range(6, 10)]), vec![
			range(4, 5),
			range(11, usize::MAX - 1)
		]);
		assert_eq!(complement(&[range(2, 4), range(6, usize::MAX - 1)]), vec![
			range(1, 1),
			range(5, 5)
		]);
	}
}
