//! Session-title generation policy and tiny-model consumer.

use std::{future::Future, pin::Pin};

use omp_core::Str;
#[cfg(feature = "local-text")]
use omp_inference::local::{
	LocalCancellation,
	text::{ChatMessage, ChatRole, GenerationOptions, TextAdapter},
};
use omp_storage::transcript::TitleSource;

/// Pi-parity online role chain. Role resolution performs one completion using
/// the first available assignment; it must not issue one request per role.
pub const ONLINE_TITLE_ROLE_CHAIN: [&str; 3] = ["tiny", "commit", "smol"];
/// System instruction used by production title completions.
pub const TITLE_SYSTEM_PROMPT: &str = "Generate a concise session title of at most 80 characters. \
                                       Return only the title, without analysis, quotes, Markdown, \
                                       or a trailing period.";

const FILLER: &[&str] = &[
	"hi",
	"hii",
	"hiii",
	"hiya",
	"hey",
	"heya",
	"hello",
	"helo",
	"hullo",
	"yo",
	"ya",
	"sup",
	"wassup",
	"whatsup",
	"howdy",
	"greetings",
	"hola",
	"ciao",
	"aloha",
	"gm",
	"gn",
	"good",
	"morning",
	"afternoon",
	"evening",
	"night",
	"day",
	"thanks",
	"thank",
	"thx",
	"ty",
	"tysm",
	"cheers",
	"please",
	"pls",
	"plz",
	"ok",
	"okay",
	"okey",
	"k",
	"kk",
	"yep",
	"yes",
	"yeah",
	"yup",
	"nope",
	"no",
	"nah",
	"sure",
	"cool",
	"nice",
	"great",
	"awesome",
	"perfect",
	"lol",
	"lmao",
	"haha",
	"hehe",
	"test",
	"tests",
	"testing",
	"ping",
	"pong",
	"there",
	"you",
	"u",
	"hmm",
	"hmmm",
	"um",
	"uh",
	"so",
	"well",
	"anyway",
];

/// One online completion boundary. Implementations resolve
/// [`ONLINE_TITLE_ROLE_CHAIN`] once and perform exactly one background request.
pub trait OnlineTitleCompletion: Send + Sync {
	/// Returns raw visible completion text. Errors are fail-open so an untitled
	/// session retries after the next eligible user message.
	fn complete_title<'a>(
		&'a self,
		roles: &'static [&'static str],
		system_prompt: &'a str,
		input: &'a str,
	) -> Pin<Box<dyn Future<Output = Result<Option<Str>, Str>> + Send + 'a>>;
}

/// Durable title authority projected from `Kind::Title` events.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionTitleState {
	/// Current projected title.
	pub title:  Option<Str>,
	/// Authority that assigned the current title.
	pub source: Option<TitleSource>,
}

impl SessionTitleState {
	/// Returns whether this turn may start title generation. User titles are
	/// immutable to automatic refreshes; assistant titles refresh after replans.
	pub fn should_generate(&self, input: &str, replanned: bool) -> bool {
		if self.source == Some(TitleSource::User) || is_low_signal_title_input(input) {
			return false;
		}
		self.title.is_none() || (replanned && self.source == Some(TitleSource::Assistant))
	}

	/// Projects an accepted generated title without overriding a user title.
	pub fn accept_generated(&mut self, title: Str) -> bool {
		if self.source == Some(TitleSource::User) {
			return false;
		}
		self.title = Some(title);
		self.source = Some(TitleSource::Assistant);
		true
	}

	/// Runs the online tiny→commit→smol lane when this state admits automatic
	/// generation, then projects its accepted assistant title.
	pub async fn generate_online(
		&mut self,
		completion: &dyn OnlineTitleCompletion,
		input: &str,
		system_prompt: &str,
		replanned: bool,
	) -> bool {
		if !self.should_generate(input, replanned) {
			return false;
		}
		generate_online_title(completion, input, system_prompt)
			.await
			.is_some_and(|title| self.accept_generated(title))
	}

	/// Runs the configured local tiny-text adapter when this state admits
	/// automatic generation, then projects its accepted assistant title.
	#[cfg(feature = "local-text")]
	pub fn generate_local(
		&mut self,
		adapter: &TextAdapter,
		input: &str,
		system_prompt: &str,
		replanned: bool,
		cancel: &LocalCancellation,
	) -> bool {
		if !self.should_generate(input, replanned) {
			return false;
		}
		generate_local_title(adapter, input, system_prompt, cancel)
			.is_some_and(|title| self.accept_generated(title))
	}
}

/// Runs the landed local tiny-text adapter with bounded title options.
#[cfg(feature = "local-text")]
pub fn generate_local_title(
	adapter: &TextAdapter,
	input: &str,
	system_prompt: &str,
	cancel: &LocalCancellation,
) -> Option<Str> {
	if is_low_signal_title_input(input) {
		return None;
	}
	let messages =
		[ChatMessage { role: ChatRole::System, content: Str::new(system_prompt) }, ChatMessage {
			role:    ChatRole::User,
			content: Str::new(input),
		}];
	let generated = adapter
		.generate(&messages, GenerationOptions::title(), cancel, |_| true)
		.ok()?;
	normalize_generated_title(generated.content.as_str())
}

/// Resolves the tiny→commit→smol lane once and normalizes its one completion.
pub async fn generate_online_title(
	completion: &dyn OnlineTitleCompletion,
	input: &str,
	system_prompt: &str,
) -> Option<Str> {
	if is_low_signal_title_input(input) {
		return None;
	}
	completion
		.complete_title(&ONLINE_TITLE_ROLE_CHAIN, system_prompt, input)
		.await
		.ok()
		.flatten()
		.and_then(|value| normalize_generated_title(value.as_str()))
}

/// Collapses a user-invoked skill expansion back to its stable title chip.
pub fn skill_title_input(name: &str, args: &str) -> Str {
	let name = name.trim();
	let args = args.trim();
	match (name.is_empty(), args.is_empty()) {
		(false, false) => Str::from(format!("/skill:{name} {args}")),
		(false, true) => Str::from(format!("/skill:{name}")),
		(true, false) => Str::new(args),
		(true, true) => Str::default(),
	}
}

/// Deterministically rejects greetings, acknowledgements, bare numbers, and
/// punctuation-only input before any model request.
pub fn is_low_signal_title_input(input: &str) -> bool {
	for token in input
		.split(|character: char| !character.is_alphanumeric())
		.filter(|token| !token.is_empty())
	{
		if !token.chars().all(|character| character.is_ascii_digit())
			&& !FILLER
				.iter()
				.any(|filler| token.eq_ignore_ascii_case(filler))
		{
			return false;
		}
	}
	true
}

/// Normalizes marker/plain/JSON title responses and rejects leaked reasoning,
/// overlong answers, punctuation junk, and the `none` sentinel.
pub fn normalize_generated_title(raw: &str) -> Option<Str> {
	let visible = extract_visible_title(raw)?;
	let mut title = unwrap_json_title(visible.trim());
	title = title.trim_matches(['\"', '\'', ' ', '\t']).trim();
	title = title.trim_end_matches(['.', '!', '?']).trim();
	if title.is_empty() || title.eq_ignore_ascii_case("none") || title == "<title/>" {
		return None;
	}
	let words = title
		.split(|character: char| !character.is_alphanumeric())
		.filter(|word| !word.is_empty())
		.count();
	if words == 0 || words > 12 || title.chars().count() > 80 {
		return None;
	}
	Some(Str::new(title))
}

fn extract_visible_title(raw: &str) -> Option<&str> {
	let mut rest = raw.trim();
	loop {
		let lower = rest.to_ascii_lowercase();
		let envelope =
			[("<think>", "</think>"), ("<thinking>", "</thinking>"), ("<reasoning>", "</reasoning>")]
				.into_iter()
				.find(|(open, _)| lower.starts_with(open));
		let Some((_, close)) = envelope else { break };
		let end = lower.find(close)? + close.len();
		rest = rest.get(end..)?.trim_start();
	}
	let lower = rest.to_ascii_lowercase();
	if lower.starts_with("```thinking") || lower.starts_with("```reasoning") {
		let end = rest.get(3..)?.find("```")? + 6;
		return extract_visible_title(rest.get(end..)?.trim_start());
	}
	if let Some(start) = lower.find("<title>") {
		let content = rest.get(start + "<title>".len()..)?;
		let lower_content = content.to_ascii_lowercase();
		let end = lower_content.find("</title>").unwrap_or(content.len());
		return content.get(..end);
	}
	if lower.contains("thinking process:") || lower.contains("reasoning process:") {
		return None;
	}
	Some(rest.lines().next().unwrap_or_default())
}

fn unwrap_json_title(candidate: &str) -> &str {
	let mut text = candidate.trim();
	if let Some(unfenced) = text
		.strip_prefix("```json")
		.or_else(|| text.strip_prefix("```"))
	{
		text = unfenced.trim();
	}
	if let Some(unfenced) = text.strip_suffix("```") {
		text = unfenced.trim();
	}
	let Some(key) = text.find("\"title\"") else {
		return text;
	};
	let Some(colon) = text.get(key + 7..).and_then(|tail| tail.find(':')) else {
		return text;
	};
	let value = text
		.get(key + 7 + colon + 1..)
		.unwrap_or_default()
		.trim_start();
	let Some(value) = value.strip_prefix('\"') else {
		return text;
	};
	let mut escaped = false;
	for (index, character) in value.char_indices() {
		if character == '\"' && !escaped {
			return &value[..index];
		}
		escaped = character == '\\' && !escaped;
		if character != '\\' {
			escaped = false;
		}
	}
	text
}
#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn low_signal_defers_until_a_concrete_task() {
		assert!(is_low_signal_title_input("hi, thanks 123"));
		assert!(is_low_signal_title_input("..."));
		assert!(!is_low_signal_title_input("fix the OAuth callback"));
	}

	#[test]
	fn normalization_ignores_leaked_reasoning_and_unwraps_json() {
		assert_eq!(
			normalize_generated_title(
				"<thinking>draft <title>Wrong</title></thinking><title>Fix OAuth callback</title>"
			),
			Some(Str::new_static("Fix OAuth callback"))
		);
		assert_eq!(
			normalize_generated_title("```json {\"title\":\"Repair session index\"} ```"),
			Some(Str::new_static("Repair session index"))
		);
	}

	#[test]
	fn user_title_blocks_automatic_refresh() {
		let mut state = SessionTitleState {
			title:  Some(Str::new_static("Chosen")),
			source: Some(TitleSource::User),
		};
		assert!(!state.should_generate("replan the storage migration", true));
		assert!(!state.accept_generated(Str::new_static("Generated")));
		assert_eq!(state.title, Some(Str::new_static("Chosen")));
	}
}
