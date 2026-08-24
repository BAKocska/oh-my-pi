//! Immutable, aligned source documents used by the interactive diff pane.

use std::{fmt::Write as _, ops::Range, path::Path};

use omp_core::{IntoStr, Str, StrMut};
use similar::{Algorithm, DiffOp, capture_diff_slices};
use smallvec::SmallVec;
use xutf::{Text, width_char};

use crate::{
	Theme,
	frame::Style,
	markdown::highlight::{self, HighlightStyles},
	rich::RichText,
};

const HIGHLIGHT_LIMIT_BYTES: usize = 512 * 1024;
const INTRALINE_PAIR_LIMIT: usize = 1_500;
const HUNK_CONTEXT: usize = 3;

/// A row's relationship between the old and new documents.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiffRowKind {
	/// The same line occurs on both sides.
	Context,
	/// One old line is paired with one changed new line.
	Change,
	/// A line exists only on the new side.
	Add,
	/// A line exists only on the old side.
	Del,
}

/// A display-column range carrying intraline emphasis.
pub type DiffMark = Range<u16>;

/// One syntax-highlighted display-column run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiffStyleRun {
	/// First display column in the run.
	pub start: u16,
	/// First display column after the run.
	pub end:   u16,
	/// Style applied to the run.
	pub style: Style,
}

/// One present side of an aligned diff row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffSide {
	/// One-based source line number.
	pub number: u32,
	/// Tab-expanded display text. It never contains terminal escapes.
	pub text:   Str,
	/// Display width in terminal cells.
	pub width:  u16,
	/// Cached, padded line-number gutter.
	pub gutter: Str,
	/// Syntax style runs, expressed in display columns.
	pub styles: Box<[DiffStyleRun]>,
	/// Intraline emphasis ranges, expressed in display columns.
	pub marks:  Box<[DiffMark]>,
}

/// One aligned row in a [`DiffDocument`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffRow {
	/// Relationship between the two sides.
	pub kind: DiffRowKind,
	/// Old side, absent for an added-only row.
	pub old:  Option<DiffSide>,
	/// New side, absent for a deleted-only row.
	pub new:  Option<DiffSide>,
}

/// One source line in file view.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffFileLine {
	/// One-based source line number.
	pub number: u32,
	/// Tab-expanded display text.
	pub text:   Str,
	/// Display width in terminal cells.
	pub width:  u16,
	/// Cached, padded line-number gutter.
	pub gutter: Str,
	/// Syntax style runs, expressed in display columns.
	pub styles: Box<[DiffStyleRun]>,
}

/// A tight changed region with three surrounding context lines.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffHunk {
	/// Cached `@@ -a,b +c,d @@` display header.
	pub header:    Str,
	/// Old-side inclusive start and line count.
	pub old_range: (u32, u32),
	/// New-side inclusive start and line count.
	pub new_range: (u32, u32),
	/// Range into [`DiffDocument::rows`] shown by this hunk.
	pub rows:      Range<usize>,
}

/// Options controlling construction of a [`DiffDocument`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DiffBuildOptions {
	/// Align lines by their trimmed contents while retaining raw numbering.
	pub ignore_whitespace: bool,
	/// Explicit syntax token or extension; the path extension is used when
	/// absent.
	pub language:          Option<Str>,
}

/// Immutable aligned old/new source text and cached display metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffDocument {
	/// Display path associated with the source.
	pub path:            Str,
	/// Full-file aligned rows.
	pub rows:            Vec<DiffRow>,
	/// Tight changed regions.
	pub hunks:           Vec<DiffHunk>,
	/// New-side source lines for file view.
	pub file_lines:      Vec<DiffFileLine>,
	/// Number of added source lines.
	pub additions:       u32,
	/// Number of deleted source lines.
	pub deletions:       u32,
	/// Width reserved for one line-number gutter.
	pub gutter_width:    u16,
	/// Widest source line in display cells.
	pub max_line_width:  u16,
	/// Global aligned row for each one-based new-side line; index zero is
	/// unused.
	pub row_by_new_line: Vec<Option<usize>>,
}

impl DiffDocument {
	/// Builds an aligned document from old and new source text.
	pub fn build(old: &str, new: &str, path: &str, options: &DiffBuildOptions) -> Self {
		let old_raw = source_lines(old);
		let new_raw = source_lines(new);
		let old_display: Vec<Str> = old_raw.iter().map(|line| expand_tabs(line)).collect();
		let new_display: Vec<Str> = new_raw.iter().map(|line| expand_tabs(line)).collect();
		let old_basis: Vec<&str> = if options.ignore_whitespace {
			old_raw.iter().map(|line| line.trim()).collect()
		} else {
			old_raw.clone()
		};
		let new_basis: Vec<&str> = if options.ignore_whitespace {
			new_raw.iter().map(|line| line.trim()).collect()
		} else {
			new_raw.clone()
		};
		let line_max = old_raw.len().max(new_raw.len()).max(1);
		let gutter_width = decimal_width(line_max).max(3) as u16;
		let language = options.language.as_deref().or_else(|| {
			Path::new(path)
				.extension()
				.and_then(|extension| extension.to_str())
		});
		let highlightable = old.len().saturating_add(new.len()) <= HIGHLIGHT_LIMIT_BYTES;
		let old_styles = if highlightable {
			language.and_then(|language| syntax_runs(&old_display, language))
		} else {
			None
		};
		let new_styles = if highlightable {
			language.and_then(|language| syntax_runs(&new_display, language))
		} else {
			None
		};

		let mut rows = Vec::new();
		let mut additions = 0u32;
		let mut deletions = 0u32;
		let mut intraline_pairs = 0usize;
		for operation in capture_diff_slices(Algorithm::Myers, &old_basis, &new_basis) {
			match operation {
				DiffOp::Equal { old_index, new_index, len } => {
					for offset in 0..len {
						rows.push(make_row(
							DiffRowKind::Context,
							Some(side(
								old_index + offset,
								&old_display,
								old_styles.as_deref(),
								gutter_width,
							)),
							Some(side(
								new_index + offset,
								&new_display,
								new_styles.as_deref(),
								gutter_width,
							)),
						));
					}
				},
				DiffOp::Delete { old_index, old_len, .. } => {
					deletions = deletions.saturating_add(old_len as u32);
					for offset in 0..old_len {
						rows.push(make_row(
							DiffRowKind::Del,
							Some(side(
								old_index + offset,
								&old_display,
								old_styles.as_deref(),
								gutter_width,
							)),
							None,
						));
					}
				},
				DiffOp::Insert { new_index, new_len, .. } => {
					additions = additions.saturating_add(new_len as u32);
					for offset in 0..new_len {
						rows.push(make_row(
							DiffRowKind::Add,
							None,
							Some(side(
								new_index + offset,
								&new_display,
								new_styles.as_deref(),
								gutter_width,
							)),
						));
					}
				},
				DiffOp::Replace { old_index, old_len, new_index, new_len } => {
					deletions = deletions.saturating_add(old_len as u32);
					additions = additions.saturating_add(new_len as u32);
					let paired = old_len.min(new_len);
					for offset in 0..paired {
						let mut old_side =
							side(old_index + offset, &old_display, old_styles.as_deref(), gutter_width);
						let mut new_side =
							side(new_index + offset, &new_display, new_styles.as_deref(), gutter_width);
						if intraline_pairs < INTRALINE_PAIR_LIMIT {
							let (old_marks, new_marks) = intraline_marks(&old_side.text, &new_side.text);
							old_side.marks = old_marks;
							new_side.marks = new_marks;
							intraline_pairs += 1;
						}
						rows.push(make_row(DiffRowKind::Change, Some(old_side), Some(new_side)));
					}
					for offset in paired..old_len {
						rows.push(make_row(
							DiffRowKind::Del,
							Some(side(
								old_index + offset,
								&old_display,
								old_styles.as_deref(),
								gutter_width,
							)),
							None,
						));
					}
					for offset in paired..new_len {
						rows.push(make_row(
							DiffRowKind::Add,
							None,
							Some(side(
								new_index + offset,
								&new_display,
								new_styles.as_deref(),
								gutter_width,
							)),
						));
					}
				},
			}
		}

		let hunks = build_hunks(&rows);
		let mut row_by_new_line = vec![None; new_display.len().saturating_add(1)];
		for (index, row) in rows.iter().enumerate() {
			if let Some(side) = &row.new
				&& let Some(slot) = row_by_new_line.get_mut(side.number as usize)
			{
				*slot = Some(index);
			}
		}
		let file_lines = new_display
			.into_iter()
			.enumerate()
			.map(|(index, text)| DiffFileLine {
				number: (index + 1) as u32,
				width: cell_width(&text),
				gutter: gutter_label(index + 1, gutter_width),
				styles: new_styles
					.as_deref()
					.and_then(|lines| lines.get(index))
					.cloned()
					.unwrap_or_default(),
				text,
			})
			.collect();
		let max_line_width = rows
			.iter()
			.flat_map(|row| [row.old.as_ref(), row.new.as_ref()])
			.flatten()
			.map(|side| side.width)
			.max()
			.unwrap_or(0);
		Self {
			path: path.into_str(),
			rows,
			hunks,
			file_lines,
			additions,
			deletions,
			gutter_width,
			max_line_width,
			row_by_new_line,
		}
	}
}

fn source_lines(source: &str) -> Vec<&str> {
	if source.is_empty() {
		return Vec::new();
	}
	let source = source.strip_suffix('\n').unwrap_or(source);
	source
		.split('\n')
		.map(|line| line.strip_suffix('\r').unwrap_or(line))
		.collect()
}

fn expand_tabs(line: &str) -> Str {
	if !line.contains('\t') {
		return Str::new(line);
	}
	let mut out = StrMut::with_capacity(line.len().saturating_add(8));
	for part in line.split_inclusive('\t') {
		if let Some(text) = part.strip_suffix('\t') {
			out.push_str(text);
			out.push_str("   ");
		} else {
			out.push_str(part);
		}
	}
	out.freeze()
}

fn decimal_width(mut value: usize) -> usize {
	let mut width = 1;
	while value >= 10 {
		value /= 10;
		width += 1;
	}
	width
}

fn gutter_label(number: usize, width: u16) -> Str {
	let mut out = StrMut::with_capacity(usize::from(width));
	for _ in decimal_width(number)..usize::from(width) {
		out.push(' ');
	}
	write!(out, "{number}").expect("writing a line number to memory cannot fail");
	out.freeze()
}

fn cell_width(text: &str) -> u16 {
	u16::try_from(text.visible_width()).unwrap_or(u16::MAX)
}

fn make_row(kind: DiffRowKind, old: Option<DiffSide>, new: Option<DiffSide>) -> DiffRow {
	DiffRow { kind, old, new }
}

fn side(
	index: usize,
	lines: &[Str],
	styles: Option<&[Box<[DiffStyleRun]>]>,
	gutter: u16,
) -> DiffSide {
	let text = lines[index].clone();
	DiffSide {
		number: (index + 1) as u32,
		width: cell_width(&text),
		gutter: gutter_label(index + 1, gutter),
		styles: styles
			.and_then(|lines| lines.get(index))
			.cloned()
			.unwrap_or_default(),
		text,
		marks: Box::default(),
	}
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum WordClass {
	Space,
	Word,
	Punctuation,
}

struct WordToken<'a> {
	text:  &'a str,
	start: u16,
	end:   u16,
}

fn word_tokens(text: &str) -> Vec<WordToken<'_>> {
	let mut tokens = Vec::new();
	let mut iter = text.char_indices().peekable();
	let mut column = 0u16;
	while let Some((start, character)) = iter.next() {
		let start_col = column;
		let character_width = u16::try_from(width_char(character)).unwrap_or(u16::MAX);
		column = column.saturating_add(character_width);
		let class = if character.is_whitespace() {
			WordClass::Space
		} else if character.is_alphanumeric() || character == '_' {
			WordClass::Word
		} else {
			WordClass::Punctuation
		};
		let mut end = start + character.len_utf8();
		if character_width <= 1 {
			while let Some(&(offset, next)) = iter.peek() {
				let next_width = u16::try_from(width_char(next)).unwrap_or(u16::MAX);
				let next_class = if next.is_whitespace() {
					WordClass::Space
				} else if next.is_alphanumeric() || next == '_' {
					WordClass::Word
				} else {
					WordClass::Punctuation
				};
				if next_width > 1 || next_class != class {
					break;
				}
				iter.next();
				end = offset + next.len_utf8();
				column = column.saturating_add(next_width);
			}
		}
		tokens.push(WordToken { text: &text[start..end], start: start_col, end: column });
	}
	tokens
}

fn intraline_marks(old: &str, new: &str) -> (Box<[DiffMark]>, Box<[DiffMark]>) {
	let old_tokens = word_tokens(old);
	let new_tokens = word_tokens(new);
	let old_basis: Vec<&str> = old_tokens.iter().map(|token| token.text).collect();
	let new_basis: Vec<&str> = new_tokens.iter().map(|token| token.text).collect();
	let mut old_ranges: SmallVec<DiffMark, 4> = SmallVec::new();
	let mut new_ranges: SmallVec<DiffMark, 4> = SmallVec::new();
	for operation in capture_diff_slices(Algorithm::Myers, &old_basis, &new_basis) {
		match operation {
			DiffOp::Equal { .. } => {},
			DiffOp::Delete { old_index, old_len, .. } => {
				push_token_range(&mut old_ranges, &old_tokens, old_index, old_len);
			},
			DiffOp::Insert { new_index, new_len, .. } => {
				push_token_range(&mut new_ranges, &new_tokens, new_index, new_len);
			},
			DiffOp::Replace { old_index, old_len, new_index, new_len } => {
				push_token_range(&mut old_ranges, &old_tokens, old_index, old_len);
				push_token_range(&mut new_ranges, &new_tokens, new_index, new_len);
			},
		}
	}
	(old_ranges.into_vec().into_boxed_slice(), new_ranges.into_vec().into_boxed_slice())
}

fn push_token_range(
	ranges: &mut SmallVec<DiffMark, 4>,
	tokens: &[WordToken<'_>],
	start: usize,
	len: usize,
) {
	let Some(first) = tokens.get(start) else {
		return;
	};
	let end = tokens[start..start.saturating_add(len)]
		.last()
		.map_or(first.end, |token| token.end);
	if let Some(last) = ranges.last_mut()
		&& first.start <= last.end
	{
		last.end = last.end.max(end);
	} else if end > first.start {
		ranges.push(first.start..end);
	}
}

fn syntax_runs(lines: &[Str], language: &str) -> Option<Vec<Box<[DiffStyleRun]>>> {
	if !highlight::supports_language(language) {
		return None;
	}
	let mut source = StrMut::new("");
	for (index, line) in lines.iter().enumerate() {
		if index > 0 {
			source.push('\n');
		}
		source.push_str(line);
	}
	let source = source.freeze();
	let mut rich = RichText::default();
	let styles = HighlightStyles::from_theme(&Theme::default());
	if !highlight::render(&source, language, lines.len(), &styles, &mut rich) {
		return None;
	}
	let mut output = Vec::with_capacity(lines.len());
	for row in 0..lines.len() {
		let mut column = 0u16;
		let mut runs: SmallVec<DiffStyleRun, 8> = SmallVec::new();
		for (style, text) in rich.row_runs(row as u16) {
			let end = column.saturating_add(cell_width(text));
			if end > column {
				runs.push(DiffStyleRun { start: column, end, style });
			}
			column = end;
		}
		output.push(runs.into_vec().into_boxed_slice());
	}
	Some(output)
}

fn build_hunks(rows: &[DiffRow]) -> Vec<DiffHunk> {
	let changes: Vec<usize> = rows
		.iter()
		.enumerate()
		.filter_map(|(index, row)| (row.kind != DiffRowKind::Context).then_some(index))
		.collect();
	if changes.is_empty() {
		return Vec::new();
	}
	let mut hunks = Vec::new();
	let mut group_start = changes[0];
	let mut group_end = changes[0];
	for &change in &changes[1..] {
		if change <= group_end.saturating_add(HUNK_CONTEXT * 2 + 1) {
			group_end = change;
		} else {
			hunks.push(make_hunk(rows, group_start, group_end));
			group_start = change;
			group_end = change;
		}
	}
	hunks.push(make_hunk(rows, group_start, group_end));
	hunks
}

fn make_hunk(rows: &[DiffRow], first_change: usize, last_change: usize) -> DiffHunk {
	let start = first_change.saturating_sub(HUNK_CONTEXT);
	let end = last_change.saturating_add(HUNK_CONTEXT + 1).min(rows.len());
	let old_before = rows[..start].iter().filter(|row| row.old.is_some()).count() as u32;
	let new_before = rows[..start].iter().filter(|row| row.new.is_some()).count() as u32;
	let old_count = rows[start..end]
		.iter()
		.filter(|row| row.old.is_some())
		.count() as u32;
	let new_count = rows[start..end]
		.iter()
		.filter(|row| row.new.is_some())
		.count() as u32;
	let old_start = old_before.saturating_add(u32::from(old_count > 0));
	let new_start = new_before.saturating_add(u32::from(new_count > 0));
	let mut header = StrMut::with_capacity(48);
	write!(header, "@@ -{old_start},{old_count} +{new_start},{new_count} @@")
		.expect("writing a hunk header to memory cannot fail");
	DiffHunk {
		header:    header.freeze(),
		old_range: (old_start, old_count),
		new_range: (new_start, new_count),
		rows:      start..end,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn options() -> DiffBuildOptions {
		DiffBuildOptions::default()
	}

	#[test]
	fn aligns_changes_and_one_sided_rows() {
		let doc = DiffDocument::build("a\nb\nc\n", "a\nB\nC\nd\n", "x.rs", &options());
		assert_eq!(doc.rows.iter().map(|row| row.kind).collect::<Vec<_>>(), [
			DiffRowKind::Context,
			DiffRowKind::Change,
			DiffRowKind::Change,
			DiffRowKind::Add,
		]);
		assert!(doc.rows[3].old.is_none());
		assert_eq!(doc.additions, 3);
		assert_eq!(doc.deletions, 2);
	}

	#[test]
	fn intraline_marks_use_display_columns() {
		let doc = DiffDocument::build("hello old world", "hello new world", "x.txt", &options());
		assert_eq!(doc.rows[0].old.as_ref().unwrap().marks.as_ref(), [6..9]);
		assert_eq!(doc.rows[0].new.as_ref().unwrap().marks.as_ref(), [6..9]);
	}

	#[test]
	fn tight_hunks_group_nearby_changes() {
		let old = (1..=20)
			.map(|n| n.to_string())
			.collect::<Vec<_>>()
			.join("\n");
		let mut new = (1..=20).map(|n| n.to_string()).collect::<Vec<_>>();
		new[4] = "five".into();
		new[16] = "seventeen".into();
		let doc = DiffDocument::build(&old, &new.join("\n"), "x.txt", &options());
		assert_eq!(doc.hunks.len(), 2);
		assert_eq!(doc.hunks[0].header, "@@ -2,7 +2,7 @@");
	}

	#[test]
	fn whitespace_ignore_preserves_raw_line_numbers() {
		let options = DiffBuildOptions { ignore_whitespace: true, language: None };
		let doc = DiffDocument::build(" a\nb", "a\n b", "x.txt", &options);
		assert!(doc.rows.iter().all(|row| row.kind == DiffRowKind::Context));
		assert_eq!(doc.rows[1].old.as_ref().unwrap().number, 2);
		assert_eq!(doc.rows[1].new.as_ref().unwrap().number, 2);
	}

	#[test]
	fn unicode_width_counts_terminal_cells() {
		let doc = DiffDocument::build("漢字", "漢語", "x.txt", &options());
		assert_eq!(doc.rows[0].old.as_ref().unwrap().width, 4);
		assert_eq!(doc.rows[0].new.as_ref().unwrap().marks.as_ref(), [2..4]);
	}
}
