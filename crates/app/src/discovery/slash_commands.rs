//! Native Markdown slash-command loading and deterministic template merging.

use std::{collections::BTreeMap, path::PathBuf};

use omp_core::Str;
use serde::Deserialize;
use thiserror::Error;
use xutf::graphemes_str;

use super::manifest::CommandPayload;

/// Embedded native command template used only when no file claims its name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EmbeddedCommand {
	/// Name without `/`.
	pub name:        Str,
	/// Command-list description.
	pub description: Str,
	/// Prompt template.
	pub content:     Str,
}

/// Winning command source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandSource {
	/// Native Markdown file.
	File,
	/// Build-time embedded fallback.
	Embedded,
}

/// One merged native command declaration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SlashCommand {
	/// Name without `/`.
	pub name:        Str,
	/// Display description.
	pub description: Str,
	/// Prompt template.
	pub content:     Str,
	/// Canonical source path for file declarations.
	pub path:        Option<PathBuf>,
	/// Winning source kind.
	pub source:      CommandSource,
}

/// Markdown command parse failure.
#[derive(Debug, Error)]
pub enum SlashCommandError {
	/// YAML frontmatter was malformed.
	#[error("slash command frontmatter is malformed")]
	Yaml(#[source] serde_yaml::Error),
	/// YAML frontmatter was opened but not terminated.
	#[error("slash command frontmatter is not terminated")]
	UnterminatedFrontmatter,
	/// Command body and frontmatter had no usable description.
	#[error("slash command has no description")]
	MissingDescription,
}

#[derive(Default, Deserialize)]
struct Frontmatter {
	description: Option<Str>,
}

/// Parses optional YAML frontmatter and derives a description from the first
/// non-empty body line when frontmatter omits it.
pub fn parse_markdown(
	name: Str,
	path: PathBuf,
	markdown: &str,
) -> Result<CommandPayload, SlashCommandError> {
	let (frontmatter, body) = if let Some(rest) = markdown.strip_prefix("---\n") {
		let Some((frontmatter, body)) = rest.split_once("\n---") else {
			return Err(SlashCommandError::UnterminatedFrontmatter);
		};
		(
			Some(serde_yaml::from_str::<Frontmatter>(frontmatter).map_err(SlashCommandError::Yaml)?),
			body.trim_start_matches(|character| matches!(character, '\r' | '\n')),
		)
	} else {
		(None, markdown)
	};
	let description = frontmatter
		.and_then(|frontmatter| frontmatter.description)
		.or_else(|| {
			body
				.lines()
				.map(str::trim)
				.find(|line| !line.is_empty())
				.map(|line| Str::new(line.trim_start_matches('#').trim()))
		})
		.filter(|description| !description.as_str().is_empty())
		.ok_or(SlashCommandError::MissingDescription)?;
	let description = bounded_description(description.as_str());
	Ok(CommandPayload { name, path, description, content: Str::new(body) })
}

fn bounded_description(description: &str) -> Str {
	const MAX_GRAPHEMES: usize = 160;
	let mut end = description.len();
	let mut graphemes = graphemes_str(description);
	if graphemes.by_ref().take(MAX_GRAPHEMES).count() == MAX_GRAPHEMES {
		if let Some(extra) = graphemes.next() {
			end = extra.as_ptr() as usize - description.as_ptr() as usize;
		}
	}
	Str::new(description[..end].trim_end())
}

/// Merges complete file declarations over embedded templates. File order is
/// caller precedence order; the first file with a canonical name wins.
pub fn merge(
	files: impl IntoIterator<Item = CommandPayload>,
	embedded: impl IntoIterator<Item = EmbeddedCommand>,
) -> BTreeMap<Str, SlashCommand> {
	let mut commands = BTreeMap::new();
	for file in files {
		commands
			.entry(file.name.clone())
			.or_insert_with(|| SlashCommand {
				name:        file.name,
				description: file.description,
				content:     file.content,
				path:        Some(file.path),
				source:      CommandSource::File,
			});
	}
	for embedded in embedded {
		commands
			.entry(embedded.name.clone())
			.or_insert_with(|| SlashCommand {
				name:        embedded.name,
				description: embedded.description,
				content:     embedded.content,
				path:        None,
				source:      CommandSource::Embedded,
			});
	}
	commands
}
