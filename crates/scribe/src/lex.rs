//! Template segmentation, whitespace control, and in-tag tokenization.

use omp_core::Str;

use crate::error::{Error, Span, SyntaxErrorKind};

/// What a [`Segment`] contributes to the template.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegKind {
	/// Literal output (including `{% raw %}` payloads).
	Text,
	/// A `{{ expr }}` emission tag.
	Expr,
	/// A `{% statement %}` tag.
	Stmt,
	/// A `{# comment #}` tag.
	Comment,
	/// A `{% raw %}` / `{% endraw %}` marker: participates in whitespace
	/// control like a statement, produces nothing.
	Marker,
}

/// One slice of the template: literal text or a tag.
#[derive(Clone, Copy, Debug)]
pub struct Segment {
	/// Segment role.
	pub kind:        SegKind,
	/// Content start (inside delimiters for tags; whitespace-adjusted for
	/// text).
	pub start:       u32,
	/// Content end (exclusive).
	pub end:         u32,
	/// Tag start including delimiters; equals `start` for text.
	pub outer_start: u32,
	/// Tag end including delimiters; equals `end` for text.
	pub outer_end:   u32,
	/// Explicit `-` trim marker on the opening delimiter.
	pub trim_left:   bool,
	/// Explicit `-` trim marker on the closing delimiter.
	pub trim_right:  bool,
}

impl Segment {
	const fn text(start: usize, end: usize) -> Self {
		Self {
			kind:        SegKind::Text,
			start:       start as u32,
			end:         end as u32,
			outer_start: start as u32,
			outer_end:   end as u32,
			trim_left:   false,
			trim_right:  false,
		}
	}

	pub(crate) fn span(&self) -> Span {
		Span::new(self.outer_start as usize, (self.outer_end - self.outer_start) as usize)
	}
}

/// Splits `source` (the template with one trailing newline already removed)
/// into text and tag segments. `{% raw %}` payloads come back as
/// [`SegKind::Text`] framed by [`SegKind::Marker`] segments.
pub fn segment(template: &Str, source: &str) -> Result<Vec<Segment>, Error> {
	let bytes = source.as_bytes();
	let mut segments = Vec::new();
	let mut pos = 0usize;
	while pos < bytes.len() {
		let Some(open) = find_open(bytes, pos) else {
			segments.push(Segment::text(pos, bytes.len()));
			break;
		};
		if open > pos {
			segments.push(Segment::text(pos, open));
		}
		let kind = match bytes[open + 1] {
			b'{' => SegKind::Expr,
			b'%' => SegKind::Stmt,
			_ => SegKind::Comment,
		};
		let seg = read_tag(template, source, open, kind)?;
		pos = seg.outer_end as usize;
		if kind == SegKind::Stmt && tag_keyword(source, &seg) == "raw" {
			let raw = Segment { kind: SegKind::Marker, ..seg };
			let endraw = find_endraw(template, source, pos, &raw)?;
			segments.push(raw);
			if (endraw.outer_start as usize) > pos {
				segments.push(Segment::text(pos, endraw.outer_start as usize));
			}
			segments.push(endraw);
			pos = segments.last().expect("just pushed").outer_end as usize;
		} else {
			segments.push(seg);
		}
	}
	Ok(segments)
}

/// Next `{{` / `{%` / `{#` at or after `pos`.
const fn find_open(bytes: &[u8], mut pos: usize) -> Option<usize> {
	while pos + 1 < bytes.len() {
		if bytes[pos] == b'{' && matches!(bytes[pos + 1], b'{' | b'%' | b'#') {
			return Some(pos);
		}
		pos += 1;
	}
	None
}

/// Reads one tag starting at `open`, honoring string literals when scanning
/// for the closing delimiter.
fn read_tag(template: &Str, source: &str, open: usize, kind: SegKind) -> Result<Segment, Error> {
	let bytes = source.as_bytes();
	let close_first = match kind {
		SegKind::Expr => b'}',
		SegKind::Stmt => b'%',
		_ => b'#',
	};
	let trim_left = bytes.get(open + 2) == Some(&b'-');
	let content_start = open + 2 + usize::from(trim_left);
	let mut index = content_start;
	let close = loop {
		if index + 1 >= bytes.len() {
			return Err(Error::syntax(
				template,
				source,
				Span::new(open, 2),
				SyntaxErrorKind::UnclosedTag,
			));
		}
		let byte = bytes[index];
		if kind != SegKind::Comment && matches!(byte, b'"' | b'\'') {
			index = skip_string(bytes, index);
			continue;
		}
		if byte == close_first && bytes[index + 1] == b'}' {
			break index;
		}
		index += 1;
	};
	let trim_right = close > content_start && bytes[close - 1] == b'-';
	Ok(Segment {
		kind,
		start: (content_start) as u32,
		end: (close - usize::from(trim_right)) as u32,
		outer_start: open as u32,
		outer_end: (close + 2) as u32,
		trim_left,
		trim_right,
	})
}

/// Advances past a quoted string starting at `index` (which holds the quote).
/// Returns the position after the closing quote, or `bytes.len()` when the
/// string never closes (the caller then reports the unclosed tag).
const fn skip_string(bytes: &[u8], index: usize) -> usize {
	let quote = bytes[index];
	let mut index = index + 1;
	while index < bytes.len() {
		match bytes[index] {
			b'\\' => index += 2,
			byte if byte == quote => return index + 1,
			_ => index += 1,
		}
	}
	bytes.len()
}

/// First identifier of a statement tag (empty when the tag has none).
fn tag_keyword<'s>(source: &'s str, seg: &Segment) -> &'s str {
	let content = source[seg.start as usize..seg.end as usize].trim();
	let end = content
		.char_indices()
		.find(|(_, character)| !character.is_ascii_alphanumeric() && *character != '_')
		.map_or(content.len(), |(index, _)| index);
	&content[..end]
}

/// Finds the `{% endraw %}` matching a `{% raw %}` at `raw`.
fn find_endraw(
	template: &Str,
	source: &str,
	mut pos: usize,
	raw: &Segment,
) -> Result<Segment, Error> {
	let bytes = source.as_bytes();
	loop {
		let Some(open) = find_open(bytes, pos) else {
			return Err(Error::syntax(template, source, raw.span(), SyntaxErrorKind::UnclosedRaw));
		};
		if bytes[open + 1] == b'%' {
			let seg = read_tag(template, source, open, SegKind::Stmt).map_err(|_| {
				Error::syntax(template, source, raw.span(), SyntaxErrorKind::UnclosedRaw)
			})?;
			if tag_keyword(source, &seg) == "endraw" {
				return Ok(Segment { kind: SegKind::Marker, ..seg });
			}
		}
		pos = open + 2;
	}
}

/// Per-side whitespace handling around a tag.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TrimMode {
	/// Leave whitespace alone.
	Keep,
	/// Strip all adjacent whitespace, newlines included (`{{- -}}`, `{%- -%}`).
	All,
	/// Standalone statement line: strip the line's indent and its newline.
	Line,
}

/// Applies whitespace control: explicit `-` markers trim all adjacent
/// whitespace; a statement/comment tag alone on its line is removed with the
/// whole line (mustache "standalone line" semantics, always on).
pub fn apply_whitespace(source: &str, segments: &mut [Segment]) {
	let bytes = source.as_bytes();
	for index in 0..segments.len() {
		let seg = segments[index];
		if seg.kind == SegKind::Text {
			continue;
		}
		let standalone = seg.kind != SegKind::Expr && is_standalone(bytes, &seg);
		let left = if seg.trim_left {
			TrimMode::All
		} else if standalone {
			TrimMode::Line
		} else {
			TrimMode::Keep
		};
		let right = if seg.trim_right {
			TrimMode::All
		} else if standalone {
			TrimMode::Line
		} else {
			TrimMode::Keep
		};
		if left != TrimMode::Keep
			&& let Some(prev) = index.checked_sub(1).map(|prev| &mut segments[prev])
			&& prev.kind == SegKind::Text
		{
			trim_tail(bytes, prev, left);
		}
		if right != TrimMode::Keep
			&& let Some(next) = segments.get_mut(index + 1)
			&& next.kind == SegKind::Text
		{
			trim_head(bytes, next, right);
		}
	}
}

/// Whether the tag sits alone on its line (only blanks to the previous and
/// next newline or template boundary).
fn is_standalone(bytes: &[u8], seg: &Segment) -> bool {
	let mut index = seg.outer_start as usize;
	while index > 0 {
		match bytes[index - 1] {
			b' ' | b'\t' => index -= 1,
			b'\n' => break,
			_ => return false,
		}
	}
	let mut index = seg.outer_end as usize;
	while index < bytes.len() {
		match bytes[index] {
			b' ' | b'\t' => index += 1,
			b'\n' => break,
			b'\r' if bytes.get(index + 1) == Some(&b'\n') => break,
			_ => return false,
		}
	}
	true
}

const fn trim_tail(bytes: &[u8], text: &mut Segment, mode: TrimMode) {
	let mut end = text.end as usize;
	let start = text.start as usize;
	while end > start {
		let byte = bytes[end - 1];
		let cut = match mode {
			TrimMode::All => byte.is_ascii_whitespace(),
			_ => matches!(byte, b' ' | b'\t'),
		};
		if !cut {
			break;
		}
		end -= 1;
	}
	text.end = end as u32;
}

fn trim_head(bytes: &[u8], text: &mut Segment, mode: TrimMode) {
	let mut start = text.start as usize;
	let end = text.end as usize;
	if mode == TrimMode::All {
		while start < end && bytes[start].is_ascii_whitespace() {
			start += 1;
		}
	} else {
		while start < end && matches!(bytes[start], b' ' | b'\t') {
			start += 1;
		}
		if start < end && bytes[start] == b'\r' {
			start += 1;
		}
		if start < end && bytes[start] == b'\n' {
			start += 1;
		}
	}
	text.start = (start as u32).min(text.end);
}

// ============================
// In-tag tokens
// ============================

/// Token payload produced by [`tokenize`].
#[derive(Clone, Debug, PartialEq)]
pub enum TokKind {
	/// Identifier or keyword; text recovered by slicing the source span.
	Ident,
	/// Integer literal.
	Int(i64),
	/// Float literal.
	Float(f64),
	/// String literal with escapes resolved.
	Str(Str),
	/// `.`
	Dot,
	/// `?.`
	QDot,
	/// `[`
	LBracket,
	/// `]`
	RBracket,
	/// `(`
	LParen,
	/// `)`
	RParen,
	/// `,`
	Comma,
	/// `|`
	Pipe,
	/// `~`
	Tilde,
	/// `+`
	Plus,
	/// `-`
	Minus,
	/// `=`
	Assign,
	/// `==`
	EqEq,
	/// `!=`
	Ne,
	/// `<`
	Lt,
	/// `<=`
	Le,
	/// `>`
	Gt,
	/// `>=`
	Ge,
}

/// One token with its source span.
#[derive(Clone, Debug)]
pub struct Tok {
	pub kind: TokKind,
	pub span: Span,
}

/// Tokenizes the tag content at `start..end` (absolute offsets into `source`).
pub fn tokenize(template: &Str, source: &str, start: u32, end: u32) -> Result<Vec<Tok>, Error> {
	let bytes = source.as_bytes();
	let mut toks = Vec::new();
	let mut pos = start as usize;
	let end = end as usize;
	while pos < end {
		let byte = bytes[pos];
		if byte.is_ascii_whitespace() {
			pos += 1;
			continue;
		}
		if byte == b'_' || byte.is_ascii_alphabetic() {
			let word_start = pos;
			while pos < end && (bytes[pos] == b'_' || bytes[pos].is_ascii_alphanumeric()) {
				pos += 1;
			}
			toks.push(Tok { kind: TokKind::Ident, span: Span::new(word_start, pos - word_start) });
			continue;
		}
		if byte.is_ascii_digit() {
			let number_start = pos;
			while pos < end && bytes[pos].is_ascii_digit() {
				pos += 1;
			}
			let mut float = false;
			if pos + 1 < end && bytes[pos] == b'.' && bytes[pos + 1].is_ascii_digit() {
				float = true;
				pos += 1;
				while pos < end && bytes[pos].is_ascii_digit() {
					pos += 1;
				}
			}
			let span = Span::new(number_start, pos - number_start);
			let text = &source[number_start..pos];
			let kind = if float {
				TokKind::Float(text.parse().map_err(|_| {
					Error::syntax(template, source, span, SyntaxErrorKind::InvalidNumber)
				})?)
			} else {
				TokKind::Int(text.parse().map_err(|_| {
					Error::syntax(template, source, span, SyntaxErrorKind::InvalidNumber)
				})?)
			};
			toks.push(Tok { kind, span });
			continue;
		}
		if matches!(byte, b'"' | b'\'') {
			let (kind, next) = read_string(template, source, pos, end)?;
			toks.push(Tok { kind, span: Span::new(pos, next - pos) });
			pos = next;
			continue;
		}
		let two = if pos + 1 < end { bytes[pos + 1] } else { 0 };
		let (kind, len) = match (byte, two) {
			(b'?', b'.') => (TokKind::QDot, 2),
			(b'=', b'=') => (TokKind::EqEq, 2),
			(b'!', b'=') => (TokKind::Ne, 2),
			(b'<', b'=') => (TokKind::Le, 2),
			(b'>', b'=') => (TokKind::Ge, 2),
			(b'.', _) => (TokKind::Dot, 1),
			(b'[', _) => (TokKind::LBracket, 1),
			(b']', _) => (TokKind::RBracket, 1),
			(b'(', _) => (TokKind::LParen, 1),
			(b')', _) => (TokKind::RParen, 1),
			(b',', _) => (TokKind::Comma, 1),
			(b'|', _) => (TokKind::Pipe, 1),
			(b'~', _) => (TokKind::Tilde, 1),
			(b'+', _) => (TokKind::Plus, 1),
			(b'-', _) => (TokKind::Minus, 1),
			(b'=', _) => (TokKind::Assign, 1),
			(b'<', _) => (TokKind::Lt, 1),
			(b'>', _) => (TokKind::Gt, 1),
			_ => {
				return Err(Error::syntax(
					template,
					source,
					Span::new(pos, 1),
					SyntaxErrorKind::UnexpectedToken,
				));
			},
		};
		toks.push(Tok { kind, span: Span::new(pos, len) });
		pos += len;
	}
	Ok(toks)
}

/// Reads a quoted string literal, resolving `\n \t \r \\ \" \'` escapes;
/// unknown escapes keep the escaped character.
fn read_string(
	template: &Str,
	source: &str,
	start: usize,
	end: usize,
) -> Result<(TokKind, usize), Error> {
	let bytes = source.as_bytes();
	let quote = bytes[start];
	let mut text = String::new();
	let mut pos = start + 1;
	while pos < end {
		match bytes[pos] {
			b'\\' if pos + 1 < end => {
				text.push(match bytes[pos + 1] {
					b'n' => '\n',
					b't' => '\t',
					b'r' => '\r',
					other => other as char,
				});
				pos += 2;
			},
			byte if byte == quote => return Ok((TokKind::Str(Str::from(text)), pos + 1)),
			_ => {
				let character_len = source[pos..].chars().next().map_or(1, char::len_utf8);
				text.push_str(&source[pos..pos + character_len]);
				pos += character_len;
			},
		}
	}
	Err(Error::syntax(template, source, Span::new(start, 1), SyntaxErrorKind::UnterminatedString))
}
