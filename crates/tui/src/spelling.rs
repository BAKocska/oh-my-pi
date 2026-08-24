//! Non-blocking platform spelling assistance for editor components.

use std::{ops::Range, sync::Arc, thread};

use flume::{Receiver, Sender};
use omp_core::{Str, str::IntoStr};
use parking_lot::Mutex;
use smallvec::SmallVec;

const MAX_CHECK_BYTES: usize = 32 * 1024;
#[cfg(target_os = "macos")]
const MAX_SUGGESTIONS: usize = 8;
/// Independently configurable native spelling features.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpellingFeatures {
	/// Decorate misspelled words.
	pub typo_detection: bool,
	/// Offer native word completion.
	pub autocomplete:   bool,
	/// Apply confident native corrections at word boundaries.
	pub autocorrect:    bool,
}

impl Default for SpellingFeatures {
	fn default() -> Self {
		Self { typo_detection: true, autocomplete: true, autocorrect: false }
	}
}

/// One misspelled UTF-8 byte range.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TypoRange {
	/// Inclusive byte offset.
	pub start: usize,
	/// Exclusive byte offset.
	pub end:   usize,
}

/// A spelling result paired with the dictionary language selected by the host.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SpellingResult {
	/// Misspelled ranges, sorted and non-overlapping.
	pub typos:    Vec<TypoRange>,
	/// Active dictionary language, when identified.
	pub language: Option<Str>,
}

#[derive(Debug)]
enum Request {
	Check { generation: u64, source: Str, masked: Str },
	Guesses { generation: u64, text: Str, range: Range<usize> },
}

#[derive(Debug)]
enum Response {
	Checked { generation: u64, text: Str, result: SpellingResult },
	Guesses { generation: u64, text: Str, range: Range<usize>, items: SmallVec<Str, 8> },
}

/// Latest-only asynchronous spelling client. Platform calls always execute on
/// one dedicated worker and the bounded mailbox coalesces bursts.
pub struct SpellingAssist {
	request:       Sender<Request>,
	response:      Receiver<Response>,
	pending_check: Arc<Mutex<Option<Request>>>,
	generation:    u64,
	checked_text:  Str,
	typos:         Vec<TypoRange>,
	projected:     Vec<TypoRange>,
	language:      Option<Str>,
	guesses:       Option<(Str, Range<usize>, SmallVec<Str, 8>)>,
	awaiting:      bool,
}

impl SpellingAssist {
	/// Starts the platform worker. Unsupported hosts return an inert client.
	pub fn new() -> Self {
		let (request_tx, request_rx) = flume::bounded(1);
		let (response_tx, response_rx) = flume::bounded(4);
		let pending = Arc::new(Mutex::new(None));
		let worker_pending = Arc::clone(&pending);
		thread::Builder::new()
			.name("omp-spelling".into())
			.spawn(move || worker(request_rx, response_tx, worker_pending))
			.expect("spelling worker thread");
		Self {
			request: request_tx,
			response: response_rx,
			pending_check: pending,
			generation: 0,
			checked_text: Str::default(),
			typos: Vec::new(),
			projected: Vec::new(),
			language: None,
			guesses: None,
			awaiting: false,
		}
	}

	/// Schedules a latest-only check and keeps prior ranges projected while it runs.
	pub fn check(&mut self, text: &str, masked: &[Range<usize>]) {
		if text.len() > MAX_CHECK_BYTES || text == self.checked_text.as_str() {
			return;
		}
		self.generation = self.generation.wrapping_add(1);
		self.awaiting = true;
		self.projected = project_ranges(&self.checked_text, text, &self.typos);
		let masked = mask_ranges(text, masked);
		let request = Request::Check { generation: self.generation, source: text.into_str(), masked: masked.into_str() };
		if let Err(flume::TrySendError::Full(request)) = self.request.try_send(request) {
			*self.pending_check.lock() = Some(request);
		}
	}

	/// Requests replacements for the word under the cursor.
	pub fn request_guesses(&mut self, text: &str, range: Range<usize>) {
		if range.start >= range.end || range.end > text.len() {
			return;
		}
		self.generation = self.generation.wrapping_add(1);
		self.awaiting = true;
		let request = Request::Guesses {
			generation: self.generation,
			text: text.into_str(),
			range,
		};
		if self.request.try_send(request).is_err() {
			self.awaiting = false;
		}
	}

	/// Drains completed work, dropping results stale against `text`.
	pub fn poll(&mut self, text: &str) -> bool {
		let mut changed = false;
		while let Ok(response) = self.response.try_recv() {
			match response {
				Response::Checked { generation, text: checked, result }
					if generation == self.generation && checked.as_str() == text =>
				{
					self.checked_text = checked;
					self.typos = result.typos;
					self.language = result.language;
					self.projected.clear();
					self.awaiting = false;
					changed = true;
				},
				Response::Guesses { generation, text: source, range, items }
					if generation == self.generation && source.as_str() == text =>
				{
					self.guesses = Some((source, range, items));
					self.awaiting = false;
					changed = true;
				},
				_ => {},
			}
		}
		changed
	}

	/// Current typo ranges, including edit-projected ranges during recheck.
	pub fn typo_ranges(&self) -> &[TypoRange] {
		if self.projected.is_empty() { &self.typos } else { &self.projected }
	}
	/// Clears cached decorations and invalidates outstanding results.
	pub fn clear(&mut self) {
		self.generation = self.generation.wrapping_add(1);
		self.typos.clear();
		self.projected.clear();
		self.guesses = None;
		self.awaiting = false;
	}

	/// Active dictionary language reported by the platform.
	pub fn language(&self) -> Option<&str> {
		self.language.as_deref()
	}
	/// Whether a platform response is still outstanding.
	pub const fn awaiting(&self) -> bool {
		self.awaiting
	}

	/// Takes the latest replacement candidates.
	pub fn take_guesses(&mut self) -> Option<(Range<usize>, SmallVec<Str, 8>)> {
		self.guesses.take().map(|(_, range, items)| (range, items))
	}
}

impl Default for SpellingAssist {
	fn default() -> Self {
		Self::new()
	}
}

fn worker(rx: Receiver<Request>, tx: Sender<Response>, pending: Arc<Mutex<Option<Request>>>) {
	while let Ok(mut request) = rx.recv() {
		loop {
			let response = match request {
				Request::Check { generation, ref source, ref masked } => Response::Checked {
					generation,
					text: source.clone(),
					result: platform::check(masked),
				},
				Request::Guesses { generation, ref text, ref range } => Response::Guesses {
					generation,
					text: text.clone(),
					range: range.clone(),
					items: platform::guesses(text, range.clone()),
				},
			};
			let _ = tx.try_send(response);
			let Some(next) = pending.lock().take() else { break };
			request = next;
		}
	}
}

fn mask_ranges(text: &str, ranges: &[Range<usize>]) -> String {
	let mut bytes = text.as_bytes().to_vec();
	for range in ranges {
		let start = range.start.min(bytes.len());
		let end = range.end.min(bytes.len());
		bytes[start..end].fill(b' ');
	}
	String::from_utf8(bytes).unwrap_or_else(|_| text.to_owned())
}

fn project_ranges(previous: &str, next: &str, ranges: &[TypoRange]) -> Vec<TypoRange> {
	if previous.is_empty() || ranges.is_empty() {
		return Vec::new();
	}
	let prefix = previous.bytes().zip(next.bytes()).take_while(|(a, b)| a == b).count();
	let suffix = previous[prefix..]
		.bytes()
		.rev()
		.zip(next[prefix..].bytes().rev())
		.take_while(|(a, b)| a == b)
		.count();
	if prefix + suffix + 1 < previous.len() {
		return Vec::new();
	}
	let old_end = previous.len().saturating_sub(suffix);
	let delta = next.len() as isize - previous.len() as isize;
	ranges
		.iter()
		.filter_map(|range| {
			let (start, end) = if range.end <= prefix {
				(range.start, range.end)
			} else if range.start >= old_end {
				(range.start.checked_add_signed(delta)?, range.end.checked_add_signed(delta)?)
			} else {
				(range.start.min(prefix), range.end.checked_add_signed(delta)?.max(next.len() - suffix))
			};
			(start < end && end <= next.len()).then_some(TypoRange { start, end })
		})
		.collect()
}

#[cfg(target_os = "macos")]
mod platform {
	use std::sync::LazyLock;

	use objc2::rc::Retained;
	use objc2_app_kit::NSSpellChecker;
	use objc2_foundation::{NSRange, NSString, NSTextCheckingType};
	use smallvec::SmallVec;

	use super::{MAX_SUGGESTIONS, Range, SpellingResult, Str, TypoRange};
	static APP_KIT_LOADED: LazyLock<bool> = LazyLock::new(|| unsafe { NSApplicationLoad() });

	#[link(name = "AppKit", kind = "framework")]
	unsafe extern "C" {
		fn NSApplicationLoad() -> bool;
	}

	fn checker() -> Option<Retained<NSSpellChecker>> {
		(*APP_KIT_LOADED).then(|| {
			let checker = NSSpellChecker::sharedSpellChecker();
			checker.setAutomaticallyIdentifiesLanguages(true);
			checker
		})
	}

	pub fn check(text: &str) -> SpellingResult {
		let Some(checker) = checker() else { return SpellingResult::default() };
		let string = NSString::from_str(text);
		let full = NSRange { location: 0, length: string.length() };
		let results = unsafe {
			checker.checkString_range_types_options_inSpellDocumentWithTag_orthography_wordCount(
				&string,
				full,
				NSTextCheckingType::Spelling.bits(),
				None,
				0,
				None,
				std::ptr::null_mut(),
			)
		};
		let mut typos = Vec::new();
		for result in results.iter() {
			if result.resultType() != NSTextCheckingType::Spelling { continue }
			let range = result.range();
			if let Some(range) = utf16_to_bytes(text, range) {
				if typos.last().is_none_or(|last: &TypoRange| last.end <= range.start) {
					typos.push(range);
				}
			}
		}
		let language = checker.languageForWordRange_inString_orthography(full, &string, None)
			.map(|value| Str::new(value.to_string()));
		SpellingResult { typos, language }
	}

	pub fn guesses(text: &str, range: Range<usize>) -> SmallVec<Str, 8> {
		let Some(checker) = checker() else { return SmallVec::new() };
		let string = NSString::from_str(text);
		let Some(ns_range) = bytes_to_utf16(text, range) else { return SmallVec::new() };
		let language = checker.languageForWordRange_inString_orthography(ns_range, &string, None)
			.unwrap_or_else(|| checker.language());
		checker.guessesForWordRange_inString_language_inSpellDocumentWithTag(
			ns_range, &string, Some(&*language), 0,
		).map(|values| values.iter().take(MAX_SUGGESTIONS).map(|value| Str::new(value.to_string())).collect())
		.unwrap_or_default()
	}

	fn utf16_to_bytes(text: &str, range: NSRange) -> Option<TypoRange> {
		let start = byte_at_utf16(text, range.location)?;
		let end = byte_at_utf16(text, range.location.checked_add(range.length)?)?;
		(start < end).then_some(TypoRange { start, end })
	}

	fn bytes_to_utf16(text: &str, range: Range<usize>) -> Option<NSRange> {
		if !text.is_char_boundary(range.start) || !text.is_char_boundary(range.end) { return None }
		Some(NSRange {
			location: text[..range.start].encode_utf16().count(),
			length: text[range.start..range.end].encode_utf16().count(),
		})
	}

	fn byte_at_utf16(text: &str, wanted: usize) -> Option<usize> {
		let mut units = 0;
		for (byte, character) in text.char_indices() {
			if units == wanted { return Some(byte) }
			units += character.len_utf16();
			if units > wanted { return None }
		}
		(units == wanted).then_some(text.len())
	}
}

#[cfg(not(target_os = "macos"))]
mod platform {
	use smallvec::SmallVec;
	use super::{Range, SpellingResult, Str};
	pub fn check(_text: &str) -> SpellingResult { SpellingResult::default() }
	pub fn guesses(_text: &str, _range: Range<usize>) -> SmallVec<Str, 8> { SmallVec::new() }
}

#[cfg(test)]
mod tests {
	use super::{TypoRange, mask_ranges, project_ranges};

	#[test]
	fn masking_preserves_offsets() {
		assert_eq!(mask_ranges("say `code` now", &[4..10]), "say        now");
	}

	#[test]
	fn typo_ranges_project_across_tail_edits() {
		let ranges = [TypoRange { start: 0, end: 8 }];
		assert_eq!(project_ranges("recieved", "recieved!", &ranges), ranges);
	}
}
