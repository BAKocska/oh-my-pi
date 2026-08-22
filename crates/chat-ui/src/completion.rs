//! Non-blocking extension completion adapter for the synchronous editor hook.

use std::{sync::Arc, thread};

use arc_swap::ArcSwapOption;
use flume::{Receiver, Sender};
use omp_core::Str;
use omp_tui::{EditorCompletion, SuggestionList, Suggestions};
use smallvec::SmallVec;

/// One completion trigger accepted by [`DeferredCompletion`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletionTrigger {
	/// A slash command prefix.
	Slash,
	/// A mention prefix.
	Mention,
	/// A hash/topic prefix.
	Hash,
	/// An extension-defined trigger.
	Custom,
}

/// A request delivered to the asynchronous completion worker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionQuery {
	/// Trigger byte offset in the editor buffer.
	pub prefix_start: usize,
	/// Trigger family selected by the typed sigil.
	pub trigger:      CompletionTrigger,
	/// Typed query after the trigger.
	pub query:        Str,
}

/// Extension-owned completion source. It runs outside the editor's key path.
pub trait CompletionSource: Send + Sync + 'static {
	/// Resolves one query in ranked order.
	fn complete(&self, query: CompletionQuery) -> SuggestionList;
}

struct CompletionResult {
	query: CompletionQuery,
	items: SuggestionList,
}

/// Ordered completion composition. The first source with visible rows wins.
pub struct CompletionChain {
	sources: SmallVec<Box<dyn EditorCompletion>, 2>,
}

impl CompletionChain {
	/// Builds an empty ordered source chain.
	pub const fn new() -> Self {
		Self { sources: SmallVec::new() }
	}

	/// Appends a lower-precedence source.
	pub fn source(mut self, source: Box<dyn EditorCompletion>) -> Self {
		self.sources.push(source);
		self
	}
}

impl Default for CompletionChain {
	fn default() -> Self {
		Self::new()
	}
}

impl EditorCompletion for CompletionChain {
	fn suggest(&mut self, text: &str, cursor: usize) -> Option<Suggestions> {
		self
			.sources
			.iter_mut()
			.find_map(|source| source.suggest(text, cursor))
	}

	fn hint(&mut self, text: &str, cursor: usize) -> Option<Str> {
		self
			.sources
			.iter_mut()
			.find_map(|source| source.hint(text, cursor))
	}
}

/// Bridges asynchronous extension completion to [`EditorCompletion`].
///
/// `suggest` only drains a flume receiver, locally reranks an already-visible
/// set, and `try_send`s work. It never waits for an extension response.
pub struct DeferredCompletion {
	triggers: SmallVec<(char, CompletionTrigger), 4>,
	request:  Sender<CompletionQuery>,
	response: Receiver<CompletionResult>,
	active:   Option<CompletionQuery>,
	shown:    Option<Suggestions>,
	ghost:    ArcSwapOption<Str>,
}

impl DeferredCompletion {
	/// Starts one worker for `source` with the supplied trigger table.
	pub fn new(
		triggers: impl IntoIterator<Item = (char, CompletionTrigger)>,
		source: Arc<dyn CompletionSource>,
	) -> Self {
		let (request, requests): (Sender<CompletionQuery>, Receiver<CompletionQuery>) =
			flume::bounded(1);
		let (responses, response): (Sender<CompletionResult>, Receiver<CompletionResult>) =
			flume::unbounded();
		thread::spawn(move || {
			while let Ok(query) = requests.recv() {
				let items = source.complete(query.clone());
				if responses.send(CompletionResult { query, items }).is_err() {
					return;
				}
			}
		});
		Self {
			triggers: triggers.into_iter().collect(),
			request,
			response,
			active: None,
			shown: None,
			ghost: ArcSwapOption::empty(),
		}
	}

	/// Updates the ghost hint without locking the keystroke path.
	pub fn set_ghost(&self, hint: Option<impl Into<Str>>) {
		self.ghost.store(hint.map(|hint| Arc::new(hint.into())));
	}

	fn query(&self, text: &str, cursor: usize) -> Option<CompletionQuery> {
		let before = text.get(..cursor)?;
		let (offset, trigger) = before.char_indices().rev().find(|(_, character)| {
			self
				.triggers
				.iter()
				.any(|(candidate, _)| candidate == character)
		})?;
		let kind = self
			.triggers
			.iter()
			.find(|(candidate, _)| candidate == &trigger)
			.map(|(_, kind)| *kind)?;
		let after = before.get(offset + trigger.len_utf8()..)?;
		if after.chars().any(char::is_whitespace) {
			return None;
		}
		if kind == CompletionTrigger::Custom {
			let token_start = before[..offset]
				.char_indices()
				.rev()
				.find(|(_, character)| character.is_whitespace())
				.map_or(0, |(at, character)| at + character.len_utf8());
			return Some(CompletionQuery {
				prefix_start: token_start,
				trigger:      kind,
				query:        Str::new(&before[token_start..]),
			});
		}
		Some(CompletionQuery {
			prefix_start: offset,
			trigger:      kind,
			query:        Str::new(after),
		})
	}

	fn drain(&mut self) {
		while let Ok(result) = self.response.try_recv() {
			if self
				.active
				.as_ref()
				.is_some_and(|active| active.query == result.query.query)
			{
				let hint = result.items.first().and_then(|item| {
					item
						.value()
						.strip_prefix(result.query.query.as_str())
						.filter(|hint| !hint.is_empty())
				});
				self.ghost.store(hint.map(|hint| Arc::new(Str::new(hint))));
				self.shown = Some(Suggestions {
					prefix_start: result.query.prefix_start,
					items:        result.items,
				});
			}
		}
	}

	fn rerank(&mut self, query: &CompletionQuery) {
		let Some(shown) = self.shown.as_mut() else {
			return;
		};
		shown.prefix_start = query.prefix_start;
		shown
			.items
			.retain(|item| item.value().contains(query.query.as_str()));
	}
}

impl EditorCompletion for DeferredCompletion {
	fn suggest(&mut self, text: &str, cursor: usize) -> Option<Suggestions> {
		self.drain();
		let query = self.query(text, cursor)?;
		let grew = self
			.active
			.as_ref()
			.is_some_and(|active| query.query.starts_with(active.query.as_str()));
		if grew {
			self.rerank(&query);
		}
		if self.active.as_ref() != Some(&query) {
			// A full request queue means the worker is already resolving an older
			// query. Keep the visible stale set instead of blocking or clearing it.
			let _ = self.request.try_send(query.clone());
			self.active = Some(query);
		}
		self.shown.clone().filter(|shown| !shown.items.is_empty())
	}

	fn hint(&mut self, _text: &str, _cursor: usize) -> Option<Str> {
		self.ghost.load_full().as_deref().cloned()
	}
}
