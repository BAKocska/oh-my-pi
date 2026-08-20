//! Queue and steering shorthand parsing.

use omp_core::Str;

/// One message extracted from a composer submission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueItem {
	/// Message body without queue syntax.
	pub text:             Str,
	/// Whether `->`/`=>` requested yield/follow-up delivery.
	pub yield_after_turn: bool,
}

/// Splits delimiter or sequential-list shorthand into queued messages.
/// Ordinary prose always returns exactly one item.
#[must_use]
pub fn split(text: &str) -> Vec<QueueItem> {
	let trimmed = text.trim();
	if let Some(body) = trimmed
		.strip_prefix("->")
		.or_else(|| trimmed.strip_prefix("=>"))
		.filter(|body| body.starts_with(char::is_whitespace))
	{
		return vec![QueueItem { text: Str::new(body.trim()), yield_after_turn: true }];
	}
	let delimited = split_delimiters(trimmed);
	if delimited.len() > 1 {
		return delimited.into_iter().map(item).collect();
	}
	if let Some(list) = split_sequential_list(trimmed) {
		return list.into_iter().map(item).collect();
	}
	vec![item(trimmed)]
}

fn item(text: &str) -> QueueItem {
	QueueItem { text: Str::new(text.trim()), yield_after_turn: false }
}

fn split_delimiters(text: &str) -> Vec<&str> {
	let mut items = Vec::new();
	let mut start = 0;
	let mut offset = 0;
	for line in text.split_inclusive('\n') {
		let body = line.trim();
		if body == "---" || body == "///" {
			let candidate = text[start..offset].trim();
			if !candidate.is_empty() {
				items.push(candidate);
			}
			start = offset + line.len();
		}
		offset += line.len();
	}
	let tail = text[start..].trim();
	if !tail.is_empty() {
		items.push(tail);
	}
	items
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Marker {
	Decimal(u16),
	Alpha(u16),
	Roman(u16),
}

impl Marker {
	const fn ordinal(self) -> u16 {
		match self {
			Self::Decimal(n) | Self::Alpha(n) | Self::Roman(n) => n,
		}
	}

	const fn family(self) -> u8 {
		match self {
			Self::Decimal(_) => 0,
			Self::Alpha(_) => 1,
			Self::Roman(_) => 2,
		}
	}
}

fn split_sequential_list(text: &str) -> Option<Vec<&str>> {
	let mut starts = Vec::new();
	let mut family = None;
	let mut expected = None;
	let mut offset = 0;
	for line in text.split_inclusive('\n') {
		let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
		if indent == 0
			&& let Some((marker, body_at)) = marker(line)
		{
			if let Some(wanted) = family
				&& (wanted != marker.family() || expected != Some(marker.ordinal()))
			{
				return None;
			}
			family.get_or_insert(marker.family());
			expected = Some(marker.ordinal().saturating_add(1));
			starts.push((offset, body_at));
		}
		offset += line.len();
	}
	if starts.len() < 2 {
		return None;
	}
	let mut items = Vec::with_capacity(starts.len());
	for (index, &(line_start, body_at)) in starts.iter().enumerate() {
		let end = starts.get(index + 1).map_or(text.len(), |(next, _)| *next);
		items.push(text[line_start + body_at..end].trim());
	}
	Some(items)
}

fn marker(line: &str) -> Option<(Marker, usize)> {
	let token_end = line.find(['.', ')'])?;
	let punctuation = line.as_bytes().get(token_end)?;
	if !matches!(punctuation, b'.' | b')') {
		return None;
	}
	let body_at = token_end + 1;
	if !line[body_at..].starts_with(char::is_whitespace) {
		return None;
	}
	let token = &line[..token_end];
	let marker = if let Ok(number) = token.parse::<u16>() {
		Marker::Decimal(number)
	} else if token
		.chars()
		.all(|ch| matches!(ch.to_ascii_lowercase(), 'i' | 'v' | 'x' | 'l' | 'c'))
	{
		Marker::Roman(parse_roman(token)?)
	} else if token.len() == 1 && token.as_bytes()[0].is_ascii_alphabetic() {
		Marker::Alpha(u16::from(token.as_bytes()[0].to_ascii_lowercase() - b'a' + 1))
	} else {
		return None;
	};
	Some((marker, body_at))
}

fn parse_roman(token: &str) -> Option<u16> {
	let mut total = 0_u16;
	let mut previous = 0_u16;
	for ch in token.chars().rev() {
		let value = match ch.to_ascii_lowercase() {
			'i' => 1,
			'v' => 5,
			'x' => 10,
			'l' => 50,
			'c' => 100,
			_ => return None,
		};
		if value < previous {
			total = total.checked_sub(value)?;
		} else {
			total = total.checked_add(value)?;
		}
		previous = value;
	}
	(total > 0).then_some(total)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn texts(items: &[QueueItem]) -> Vec<&str> {
		items.iter().map(|item| item.text.as_str()).collect()
	}

	#[test]
	fn delimiters_and_yield_prefix_split_without_leaking_syntax() {
		assert_eq!(texts(&split("one\n---\ntwo\n///\nthree")), ["one", "two", "three"]);
		let yielded = split("->\nwait for the turn");
		assert_eq!(texts(&yielded), ["wait for the turn"]);
		assert!(yielded[0].yield_after_turn);
	}

	#[test]
	fn sequential_decimal_alpha_and_roman_lists_split() {
		assert_eq!(texts(&split("1. first\n2) second")), ["first", "second"]);
		assert_eq!(texts(&split("a) first\nb. second")), ["first", "second"]);
		assert_eq!(texts(&split("i. first\nii) second\niii. third")), ["first", "second", "third"]);
		assert_eq!(texts(&split("1. first\n3. not sequential")), ["1. first\n3. not sequential"]);
	}
}
