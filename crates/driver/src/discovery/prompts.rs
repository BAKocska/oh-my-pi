//! Native user/project prompt-template discovery.

use std::{
	collections::BTreeMap,
	fs, io,
	path::{Path, PathBuf},
};

use omp_core::Str;
use serde::Deserialize;

/// Precedence/source label for a discovered template.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromptTemplateSource {
	/// Repository-local `.omp/prompts`.
	Project,
	/// Profile-wide `~/.omp/agent/prompts`.
	User,
}

/// A native Markdown prompt template.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptTemplate {
	/// Canonical filename stem.
	pub name:        Str,
	/// Frontmatter or body-derived description.
	pub description: Str,
	/// Markdown body without frontmatter.
	pub content:     Str,
	/// Winning source path.
	pub path:        PathBuf,
	/// Winning source scope.
	pub source:      PromptTemplateSource,
}

#[derive(Default, Deserialize)]
struct Frontmatter {
	description: Option<Str>,
}

/// Discovers `.omp/prompts` before `~/.omp/agent/prompts`; the first canonical
/// template name wins. Invalid files are returned as typed errors.
pub fn discover(
	project_root: &Path,
	home: &Path,
) -> Result<BTreeMap<Str, PromptTemplate>, PromptTemplateError> {
	let roots = [
		(project_root.join(".omp/prompts"), PromptTemplateSource::Project),
		(home.join(".omp/agent/prompts"), PromptTemplateSource::User),
	];
	let mut templates = BTreeMap::new();
	for (root, source) in roots {
		let entries = match fs::read_dir(&root) {
			Ok(entries) => entries,
			Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
			Err(source) => return Err(PromptTemplateError::ReadDirectory { path: root, source }),
		};
		let mut paths = entries
			.filter_map(Result::ok)
			.map(|entry| entry.path())
			.filter(|path| path.extension().is_some_and(|extension| extension == "md"))
			.collect::<Vec<_>>();
		paths.sort();
		for path in paths {
			let name = path
				.file_stem()
				.and_then(|name| name.to_str())
				.filter(|name| !name.is_empty())
				.ok_or_else(|| PromptTemplateError::InvalidName { path: path.clone() })?;
			if templates.contains_key(name) {
				continue;
			}
			let markdown = fs::read_to_string(&path)
				.map_err(|source| PromptTemplateError::Read { path: path.clone(), source })?;
			let (description, content) = parse_markdown(&markdown)?;
			templates.insert(Str::new(name), PromptTemplate {
				name: Str::new(name),
				description,
				content: Str::new(content),
				path,
				source,
			});
		}
	}
	Ok(templates)
}

fn parse_markdown(markdown: &str) -> Result<(Str, &str), PromptTemplateError> {
	let (frontmatter, body) = if let Some(rest) = markdown.strip_prefix("---\n") {
		let Some((frontmatter, body)) = rest.split_once("\n---") else {
			return Err(PromptTemplateError::UnterminatedFrontmatter);
		};
		(
			Some(serde_yaml::from_str::<Frontmatter>(frontmatter)?),
			body.trim_start_matches(['\r', '\n']),
		)
	} else {
		(None, markdown)
	};
	let description = frontmatter
		.and_then(|value| value.description)
		.or_else(|| {
			body
				.lines()
				.map(str::trim)
				.find(|line| !line.is_empty())
				.map(|line| Str::new(line.trim_start_matches('#').trim()))
		})
		.ok_or(PromptTemplateError::MissingDescription)?;
	Ok((description, body))
}

/// Failure while discovering or parsing a native Markdown prompt template.
#[derive(Debug, thiserror::Error)]
pub enum PromptTemplateError {
	/// The project- or user-scoped prompt directory could not be enumerated.
	#[error("failed to read prompt template directory {path}")]
	ReadDirectory {
		/// Prompt directory whose entries could not be read.
		path:   PathBuf,
		/// Filesystem error returned while opening the directory.
		#[source]
		source: io::Error,
	},
	/// A discovered Markdown template could not be read.
	#[error("failed to read prompt template {path}")]
	Read {
		/// Winning source file that could not be loaded.
		path:   PathBuf,
		/// Filesystem error returned while reading the template.
		#[source]
		source: io::Error,
	},
	/// A Markdown path has no non-empty UTF-8 filename stem to use as its
	/// canonical name.
	#[error("invalid prompt template filename {path}")]
	InvalidName {
		/// Discovered Markdown path with the invalid filename.
		path: PathBuf,
	},
	/// Opening `---` frontmatter has no closing delimiter.
	#[error("prompt template frontmatter is not terminated")]
	UnterminatedFrontmatter,
	/// YAML frontmatter could not be decoded.
	#[error("prompt template frontmatter is malformed")]
	Yaml(#[from] serde_yaml::Error),
	/// Neither frontmatter nor the first non-empty body line supplies a
	/// description.
	#[error("prompt template has no description")]
	MissingDescription,
}
