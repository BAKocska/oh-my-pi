//! Shell-safe diagnostic quoting and GNU filename quoting styles.

#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::{
	borrow::Cow,
	env,
	ffi::{OsStr, OsString},
	fmt::{self, Write as _},
	path::Path,
};

/// A lazily formatted, shell-safe string.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Quoted<'a> {
	text:        &'a OsStr,
	force_quote: bool,
}

/// Adds shell-safe diagnostic quoting to strings and paths.
pub(crate) trait Quotable {
	/// Returns a display wrapper that always quotes its value.
	fn quote(&self) -> Quoted<'_>;

	/// Returns a display wrapper that quotes only when shell syntax or
	/// whitespace requires it.
	fn maybe_quote(&self) -> Quoted<'_> {
		let mut quoted = self.quote();
		quoted.force_quote = false;
		quoted
	}
}

impl Quotable for str {
	fn quote(&self) -> Quoted<'_> {
		Quoted { text: OsStr::new(self), force_quote: true }
	}
}

impl Quotable for OsStr {
	fn quote(&self) -> Quoted<'_> {
		Quoted { text: self, force_quote: true }
	}
}

impl Quotable for Path {
	fn quote(&self) -> Quoted<'_> {
		Quoted { text: self.as_os_str(), force_quote: true }
	}
}

impl<T> Quotable for Cow<'_, T>
where
	T: ToOwned + Quotable + ?Sized,
{
	fn quote(&self) -> Quoted<'_> {
		self.as_ref().quote()
	}
}

impl fmt::Display for Quoted<'_> {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		#[cfg(unix)]
		{
			let bytes = self.text.as_bytes();
			match std::str::from_utf8(bytes) {
				Ok(text) => write_shell_diagnostic(formatter, text, self.force_quote),
				Err(_) => write_ansi_c_escaped(formatter, bytes),
			}
		}
		#[cfg(not(unix))]
		{
			write_shell_diagnostic(formatter, &self.text.to_string_lossy(), self.force_quote)
		}
	}
}

const DIAGNOSTIC_SPECIAL: &[u8] = b"|&;<>()$`\\\"'*?[]=^{} ";
const DIAGNOSTIC_DOUBLE_UNSAFE: &[u8] = b"\"`$\\";

fn write_shell_diagnostic(
	formatter: &mut fmt::Formatter<'_>,
	text: &str,
	force_quote: bool,
) -> fmt::Result {
	let mut single_safe = true;
	let mut double_safe = true;
	let mut requires_quote = force_quote;
	let mut has_bidi = false;

	if !requires_quote {
		match text.chars().next() {
			Some(first) => {
				requires_quote = matches!(first, '~' | '#' | '!') || xutf::width_char(first) == 0;
			},
			None => requires_quote = true,
		}
	}

	for character in text.chars() {
		if character.is_ascii() {
			let byte = character as u8;
			single_safe &= byte != b'\'';
			double_safe &= !DIAGNOSTIC_DOUBLE_UNSAFE.contains(&byte);
			requires_quote |= DIAGNOSTIC_SPECIAL.contains(&byte);
			if byte.is_ascii_control() {
				return write_ansi_c_escaped(formatter, text.as_bytes());
			}
		} else {
			requires_quote |= character.is_whitespace() || character == '\u{2800}';
			has_bidi |= is_bidi(character);
			if requires_terminal_escape(character) {
				return write_ansi_c_escaped(formatter, text.as_bytes());
			}
		}
	}

	if has_bidi && suspicious_bidi(text) {
		return write_ansi_c_escaped(formatter, text.as_bytes());
	}
	if !requires_quote {
		return formatter.write_str(text);
	}
	if single_safe {
		return write_simple_quote(formatter, text, '\'');
	}
	if double_safe {
		return write_simple_quote(formatter, text, '"');
	}

	let mut chunks = text.split('\'');
	if let Some(chunk) = chunks.next()
		&& !chunk.is_empty()
	{
		write_simple_quote(formatter, chunk, '\'')?;
	}
	for chunk in chunks {
		formatter.write_str("\\'")?;
		if !chunk.is_empty() {
			write_simple_quote(formatter, chunk, '\'')?;
		}
	}
	Ok(())
}

fn write_simple_quote(formatter: &mut fmt::Formatter<'_>, text: &str, quote: char) -> fmt::Result {
	formatter.write_char(quote)?;
	formatter.write_str(text)?;
	formatter.write_char(quote)
}

fn write_ansi_c_escaped(formatter: &mut fmt::Formatter<'_>, bytes: &[u8]) -> fmt::Result {
	formatter.write_str("$'")?;
	let mut trailing_hex_escape = false;
	for chunk in Utf8Chunks::new(bytes) {
		match chunk {
			Ok(text) => {
				for character in text.chars() {
					let followed_hex_escape = trailing_hex_escape;
					trailing_hex_escape = false;
					match character {
						'\n' => formatter.write_str("\\n")?,
						'\t' => formatter.write_str("\\t")?,
						'\r' => formatter.write_str("\\r")?,
						character if requires_terminal_escape(character) || is_bidi(character) => {
							for byte in character.encode_utf8(&mut [0; 4]).as_bytes() {
								write!(formatter, "\\x{byte:02X}")?;
							}
							trailing_hex_escape = true;
						},
						'\\' | '\'' => {
							formatter.write_char('\\')?;
							formatter.write_char(character)?;
						},
						character if followed_hex_escape && character.is_ascii_hexdigit() => {
							formatter.write_str("'$'")?;
							formatter.write_char(character)?;
						},
						character => formatter.write_char(character)?,
					}
				}
			},
			Err(byte) => {
				write!(formatter, "\\x{byte:02X}")?;
				trailing_hex_escape = true;
			},
		}
	}
	formatter.write_char('\'')
}

fn requires_terminal_escape(character: char) -> bool {
	character.is_control() || matches!(character, '\u{2028}' | '\u{2029}')
}

fn is_bidi(character: char) -> bool {
	matches!(character, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

fn suspicious_bidi(text: &str) -> bool {
	#[derive(Clone, Copy, Eq, PartialEq)]
	enum Kind {
		Formatting,
		Isolate,
	}

	let mut stack = [None; 16];
	let mut depth = 0;
	for character in text.chars() {
		let (opening, closing) = match character {
			'\u{202A}' | '\u{202B}' | '\u{202D}' | '\u{202E}' => (Some(Kind::Formatting), None),
			'\u{202C}' => (None, Some(Kind::Formatting)),
			'\u{2066}' | '\u{2067}' | '\u{2068}' => (Some(Kind::Isolate), None),
			'\u{2069}' => (None, Some(Kind::Isolate)),
			_ => (None, None),
		};
		if let Some(kind) = opening {
			if depth == stack.len() {
				return true;
			}
			stack[depth] = Some(kind);
			depth += 1;
		} else if let Some(kind) = closing {
			if depth == 0 {
				return true;
			}
			depth -= 1;
			if stack[depth] != Some(kind) {
				return true;
			}
		}
	}
	depth != 0
}

struct Utf8Chunks<'a> {
	remaining: &'a [u8],
}

impl<'a> Utf8Chunks<'a> {
	fn new(bytes: &'a [u8]) -> Self {
		Self { remaining: bytes }
	}
}

impl<'a> Iterator for Utf8Chunks<'a> {
	type Item = Result<&'a str, u8>;

	fn next(&mut self) -> Option<Self::Item> {
		if self.remaining.is_empty() {
			return None;
		}
		match std::str::from_utf8(self.remaining) {
			Ok(text) => {
				self.remaining = &[];
				Some(Ok(text))
			},
			Err(error) if error.valid_up_to() == 0 => {
				let byte = self.remaining[0];
				self.remaining = &self.remaining[1..];
				Some(Err(byte))
			},
			Err(error) => {
				let (valid, remaining) = self.remaining.split_at(error.valid_up_to());
				self.remaining = remaining;
				Some(Ok(std::str::from_utf8(valid).expect("prefix was validated")))
			},
		}
	}
}

/// Delimiters used by C-style filename quoting.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display)]
pub(crate) enum Quotes {
	/// No surrounding quotes.
	None,
	/// Single quotes.
	Single,
	/// Double quotes.
	Double,
}

/// GNU filename quoting behavior.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum QuotingStyle {
	/// Shell quoting, optionally escaping controls and forcing quotes.
	Shell {
		/// Whether controls use ANSI-C shell escapes.
		escape:       bool,
		/// Whether ordinary names are quoted too.
		always_quote: bool,
		/// Whether unescaped control bytes remain visible instead of becoming
		/// `?`.
		show_control: bool,
	},
	/// C-language escaping with a chosen delimiter.
	C {
		/// Delimiter selection.
		quotes: Quotes,
	},
	/// Literal output, optionally retaining control and invalid bytes.
	Literal {
		/// Whether control and invalid bytes remain visible instead of becoming
		/// `?`.
		show_control: bool,
	},
}

impl QuotingStyle {
	/// C escaping with double quotes.
	pub(crate) const C_DOUBLE: Self = Self::C { quotes: Quotes::Double };
	/// C escaping without surrounding quotes.
	pub(crate) const C_NO_QUOTES: Self = Self::C { quotes: Quotes::None };
	/// Shell quoting without control escapes or forced delimiters.
	pub(crate) const SHELL: Self =
		Self::Shell { escape: false, always_quote: false, show_control: false };
	/// Shell quoting with ANSI-C control escapes.
	pub(crate) const SHELL_ESCAPE: Self =
		Self::Shell { escape: true, always_quote: false, show_control: false };
	/// Shell quoting with control escapes and mandatory delimiters.
	pub(crate) const SHELL_ESCAPE_QUOTE: Self =
		Self::Shell { escape: true, always_quote: true, show_control: false };
	/// Shell quoting that always emits delimiters.
	pub(crate) const SHELL_QUOTE: Self =
		Self::Shell { escape: false, always_quote: true, show_control: false };

	/// Changes whether literal and non-escaping shell modes retain control
	/// bytes.
	pub(crate) const fn show_control(self, show_control: bool) -> Self {
		match self {
			Self::Shell { escape, always_quote, .. } => {
				Self::Shell { escape, always_quote, show_control }
			},
			Self::Literal { .. } => Self::Literal { show_control },
			Self::C { .. } => self,
		}
	}
}

impl fmt::Display for QuotingStyle {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		match *self {
			Self::Shell { escape, always_quote, show_control } => {
				formatter.write_str("shell")?;
				if escape {
					formatter.write_str("-escape")?;
				}
				if always_quote {
					formatter.write_str("-always-quote")?;
				}
				if show_control {
					formatter.write_str("-show-control")?;
				}
				Ok(())
			},
			Self::C { .. } => formatter.write_str("C"),
			Self::Literal { .. } => formatter.write_str("literal"),
		}
	}
}

/// Escapes a filename according to `style` and the active locale's encoding.
pub(crate) fn locale_aware_escape_name(name: &OsStr, style: QuotingStyle) -> OsString {
	escape_os_str(name, style, false)
}

/// Escapes a directory heading according to `style` and the active locale's
/// encoding.
pub(crate) fn locale_aware_escape_dir_name(name: &OsStr, style: QuotingStyle) -> OsString {
	escape_os_str(name, style, true)
}

fn escape_os_str(name: &OsStr, style: QuotingStyle, dirname: bool) -> OsString {
	#[cfg(unix)]
	let bytes = name.as_bytes();
	#[cfg(not(unix))]
	let owned = name.to_string_lossy();
	#[cfg(not(unix))]
	let bytes = owned.as_bytes();

	let escaped = escape_name_inner(bytes, style, dirname, locale_is_utf8());
	#[cfg(unix)]
	{
		OsString::from_vec(escaped)
	}
	#[cfg(not(unix))]
	{
		OsString::from(String::from_utf8_lossy(&escaped).into_owned())
	}
}

fn locale_is_utf8() -> bool {
	let locale = ["LC_ALL", "LC_COLLATE", "LANG"]
		.into_iter()
		.find_map(|name| env::var(name).ok());
	let Some(locale) = locale else {
		return false;
	};
	let mut fields = locale.split(['.', '@']);
	let base = fields.next().unwrap_or_default();
	if matches!(base, "C" | "POSIX") {
		return false;
	}
	!base.is_empty()
		&& fields.next().is_some_and(|encoding| {
			encoding.eq_ignore_ascii_case("utf-8") || encoding.eq_ignore_ascii_case("utf8")
		})
}

fn escape_name_inner(
	name: &[u8],
	style: QuotingStyle,
	dirname: bool,
	utf8_locale: bool,
) -> Vec<u8> {
	if style == (QuotingStyle::Literal { show_control: true }) {
		return name.to_vec();
	}

	match style {
		QuotingStyle::Literal { .. } => quote_literal(name, utf8_locale),
		QuotingStyle::C { quotes } => quote_c(name, quotes, dirname, utf8_locale),
		QuotingStyle::Shell { escape: false, always_quote, show_control } => {
			quote_shell_plain(name, always_quote, show_control, dirname, utf8_locale)
		},
		QuotingStyle::Shell { escape: true, always_quote, .. } => {
			quote_shell_escaped(name, always_quote, dirname, utf8_locale)
		},
	}
}

enum Decoded {
	Character(char),
	Invalid(u8),
}

fn for_decoded(bytes: &[u8], utf8_locale: bool, mut consume: impl FnMut(Decoded)) {
	if utf8_locale {
		for chunk in Utf8Chunks::new(bytes) {
			match chunk {
				Ok(text) => {
					text
						.chars()
						.for_each(|character| consume(Decoded::Character(character)));
				},
				Err(byte) => consume(Decoded::Invalid(byte)),
			}
		}
	} else {
		for &byte in bytes {
			if byte.is_ascii() {
				consume(Decoded::Character(char::from(byte)));
			} else {
				consume(Decoded::Invalid(byte));
			}
		}
	}
}

fn push_character(output: &mut Vec<u8>, character: char) {
	let mut encoded = [0; 4];
	output.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
}

fn quote_literal(name: &[u8], utf8_locale: bool) -> Vec<u8> {
	let mut output = Vec::with_capacity(name.len());
	for_decoded(name, utf8_locale, |decoded| match decoded {
		Decoded::Character(character) => {
			push_character(
				&mut output,
				if character.is_control() {
					'?'
				} else {
					character
				},
			);
		},
		Decoded::Invalid(_) => output.push(b'?'),
	});
	output
}

fn push_octal_byte(output: &mut Vec<u8>, byte: u8) {
	output.push(b'\\');
	output.push(b'0' + ((byte >> 6) & 7));
	output.push(b'0' + ((byte >> 3) & 7));
	output.push(b'0' + (byte & 7));
}

fn push_octal_character(output: &mut Vec<u8>, character: char) {
	let mut encoded = [0; 4];
	for &byte in character.encode_utf8(&mut encoded).as_bytes() {
		push_octal_byte(output, byte);
	}
}

fn push_c_character(output: &mut Vec<u8>, character: char, quotes: Quotes, dirname: bool) {
	let escaped = match character {
		'\x07' => Some(b'a'),
		'\x08' => Some(b'b'),
		'\t' => Some(b't'),
		'\n' => Some(b'n'),
		'\x0B' => Some(b'v'),
		'\x0C' => Some(b'f'),
		'\r' => Some(b'r'),
		'\\' => Some(b'\\'),
		'\'' if quotes == Quotes::Single => Some(b'\''),
		'"' if quotes == Quotes::Double => Some(b'"'),
		' ' if !dirname && quotes == Quotes::None => Some(b' '),
		':' if dirname => Some(b':'),
		_ => None,
	};
	if let Some(escaped) = escaped {
		output.extend_from_slice(&[b'\\', escaped]);
	} else if character.is_control() {
		push_octal_character(output, character);
	} else {
		push_character(output, character);
	}
}

fn quote_c(name: &[u8], quotes: Quotes, dirname: bool, utf8_locale: bool) -> Vec<u8> {
	let mut output = Vec::with_capacity(name.len() + 2);
	if quotes != Quotes::None {
		output.push(if quotes == Quotes::Single {
			b'\''
		} else {
			b'"'
		});
	}
	for_decoded(name, utf8_locale, |decoded| match decoded {
		Decoded::Character(character) => {
			push_c_character(&mut output, character, quotes, dirname);
		},
		Decoded::Invalid(byte) => push_octal_byte(&mut output, byte),
	});
	if quotes != Quotes::None {
		output.push(if quotes == Quotes::Single {
			b'\''
		} else {
			b'"'
		});
	}
	output
}

fn shell_quotes(name: &[u8], always_quote: bool, dirname: bool, controls: bool) -> (Quotes, bool) {
	let special = if dirname {
		b":\"`$\\^\n\t\r=".as_slice()
	} else {
		b"\"`$\\^\n\t\r=".as_slice()
	};
	if name
		.iter()
		.any(|byte| special.contains(byte) || (controls && byte.is_ascii_control()))
	{
		(Quotes::Single, true)
	} else if name.contains(&b'\'') {
		(Quotes::Double, true)
	} else {
		(Quotes::Single, always_quote || name.is_empty())
	}
}

fn finalize_shell(mut output: Vec<u8>, name: &[u8], must_quote: bool, quotes: Quotes) -> Vec<u8> {
	if must_quote || name.first().is_some_and(|byte| matches!(byte, b'~' | b'#')) {
		let delimiter = if quotes == Quotes::Single {
			b'\''
		} else {
			b'"'
		};
		let mut quoted = Vec::with_capacity(output.len() + 2);
		quoted.push(delimiter);
		quoted.append(&mut output);
		quoted.push(delimiter);
		quoted
	} else {
		output
	}
}

fn quote_shell_plain(
	name: &[u8],
	always_quote: bool,
	show_control: bool,
	dirname: bool,
	utf8_locale: bool,
) -> Vec<u8> {
	let (quotes, mut must_quote) = shell_quotes(name, always_quote, dirname, false);
	let mut output = Vec::with_capacity(name.len());
	for_decoded(name, utf8_locale, |decoded| match decoded {
		Decoded::Character(character) => {
			if character.is_control() {
				push_character(&mut output, if show_control { character } else { '?' });
			} else if character == '\'' && quotes == Quotes::Single {
				output.extend_from_slice(b"'\\''");
			} else {
				must_quote |= "`$&*()|[;\\'\"<>?! ".contains(character);
				push_character(&mut output, character);
			}
		},
		Decoded::Invalid(byte) => output.push(if show_control { byte } else { b'?' }),
	});
	finalize_shell(output, name, must_quote, quotes)
}

fn enter_dollar(output: &mut Vec<u8>, in_dollar: &mut bool) {
	if !*in_dollar {
		output.extend_from_slice(b"'$'");
		*in_dollar = true;
	}
}

fn exit_dollar(output: &mut Vec<u8>, in_dollar: &mut bool) {
	if *in_dollar {
		output.extend_from_slice(b"''");
		*in_dollar = false;
	}
}

fn push_shell_escape(output: &mut Vec<u8>, character: char) {
	match character {
		'\x07' => output.extend_from_slice(b"\\a"),
		'\x08' => output.extend_from_slice(b"\\b"),
		'\t' => output.extend_from_slice(b"\\t"),
		'\n' => output.extend_from_slice(b"\\n"),
		'\x0B' => output.extend_from_slice(b"\\v"),
		'\x0C' => output.extend_from_slice(b"\\f"),
		'\r' => output.extend_from_slice(b"\\r"),
		_ => push_octal_character(output, character),
	}
}

fn quote_shell_escaped(
	name: &[u8],
	always_quote: bool,
	dirname: bool,
	utf8_locale: bool,
) -> Vec<u8> {
	let (quotes, mut must_quote) = shell_quotes(name, always_quote, dirname, true);
	let mut output = Vec::with_capacity(name.len());
	let mut in_dollar = false;
	for_decoded(name, utf8_locale, |decoded| match decoded {
		Decoded::Character(character) => {
			if character == '\'' && quotes == Quotes::Single {
				must_quote = true;
				in_dollar = false;
				output.extend_from_slice(b"'\\''");
			} else if character.is_control() {
				enter_dollar(&mut output, &mut in_dollar);
				must_quote = true;
				push_shell_escape(&mut output, character);
			} else if "`$&*()|[;\\'\"<>?! ".contains(character) {
				exit_dollar(&mut output, &mut in_dollar);
				must_quote = true;
				push_character(&mut output, character);
			} else {
				exit_dollar(&mut output, &mut in_dollar);
				push_character(&mut output, character);
			}
		},
		Decoded::Invalid(byte) => {
			enter_dollar(&mut output, &mut in_dollar);
			must_quote = true;
			push_octal_byte(&mut output, byte);
		},
	});
	finalize_shell(output, name, must_quote, quotes)
}

#[cfg(test)]
mod tests {
	use std::{borrow::Cow, ffi::OsStr, path::Path};

	use super::{Quotable, Quotes, QuotingStyle, escape_name_inner, locale_aware_escape_name};

	#[test]
	fn diagnostic_shell_quoting_matches_platform_rules() {
		assert_eq!("plain".quote().to_string(), "'plain'");
		assert_eq!("can't".quote().to_string(), "\"can't\"");
		assert_eq!("can'$t".quote().to_string(), "'can'\\''$t'");
		assert_eq!("line\nβ".quote().to_string(), "$'line\\nβ'");
		assert_eq!(Path::new("two words").quote().to_string(), "'two words'");
		let borrowed: Cow<'_, str> = Cow::Borrowed("cow value");
		assert_eq!(borrowed.quote().to_string(), "'cow value'");
	}

	#[test]
	fn maybe_quote_only_quotes_unsafe_bare_words() {
		assert_eq!("plain/path".maybe_quote().to_string(), "plain/path");
		assert_eq!("two words".maybe_quote().to_string(), "'two words'");
		assert_eq!("$name".maybe_quote().to_string(), "'$name'");
		assert_eq!("#comment".maybe_quote().to_string(), "'#comment'");
		assert_eq!("βeta".maybe_quote().to_string(), "βeta");
	}

	#[test]
	fn gnu_shell_and_c_vectors() {
		let shell = |name: &str, style| {
			String::from_utf8(escape_name_inner(name.as_bytes(), style, false, true)).unwrap()
		};
		assert_eq!(shell("one'two", QuotingStyle::SHELL), "\"one'two\"");
		assert_eq!(shell("one'two\"three", QuotingStyle::SHELL), "'one'\\''two\"three'");
		assert_eq!(shell("one\ntwo", QuotingStyle::SHELL), "'one?two'");
		assert_eq!(shell("one\ntwo", QuotingStyle::SHELL_ESCAPE), "'one'$'\\n''two'");
		assert_eq!(shell("one two", QuotingStyle::C_NO_QUOTES), "one\\ two");
		assert_eq!(shell("one\n\"two", QuotingStyle::C_DOUBLE), "\"one\\n\\\"two\"");
		assert_eq!(shell("é", QuotingStyle::SHELL_ESCAPE), "é");
	}

	#[test]
	fn control_visibility_and_public_wrapper() {
		let literal = QuotingStyle::Literal { show_control: false };
		assert_eq!(locale_aware_escape_name(OsStr::new("a\nb"), literal), OsStr::new("a?b"));
		assert_eq!(QuotingStyle::SHELL_ESCAPE.to_string(), "shell-escape");
		assert_eq!(Quotes::Double.to_string(), "Double");
	}
}
