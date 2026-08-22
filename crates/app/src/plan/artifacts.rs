//! Canonical session-local plan artifact authority.

use std::{
	fs, io,
	path::{Component, Path, PathBuf},
	time::SystemTime,
};

use omp_core::{Str, sf};
use thiserror::Error;

use super::state::DEFAULT_PLAN_URL;

/// Source selected while deriving a display title.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum PlanTitleSource {
	/// Explicit caller-supplied title.
	Supplied,
	/// First level-one Markdown heading.
	Heading,
	/// Active artifact filename stem.
	Filename,
	/// Literal `plan` fallback.
	Default,
}

/// One resolved session-local plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanArtifact {
	/// Canonical `local://` reference.
	pub url:     Str,
	/// Normalized display title.
	pub title:   Str,
	/// Source used to derive `title`.
	pub source:  PlanTitleSource,
	/// Plan Markdown.
	pub content: Str,
}

/// Plan artifact validation or I/O failure.
#[derive(Debug, Error)]
pub enum PlanArtifactError {
	/// A supplied title could not produce a safe artifact name.
	#[error(
		"plan title must contain a safe letter, number, underscore, or hyphen and must not contain \
		 a path separator or '..'"
	)]
	InvalidTitle,
	/// A reference was outside the session-local namespace.
	#[error("plan references must be relative local:// URLs")]
	InvalidReference,
	/// No candidate plan artifact exists.
	#[error(
		"no plan artifact exists; write the finalized plan to {target} before requesting approval"
	)]
	NotFound {
		/// First location considered by resolution.
		target: Str,
	},
	/// The session-local artifact authority failed.
	#[error("plan artifact I/O failed for {path}")]
	Io {
		/// Addressed filesystem path.
		path:   PathBuf,
		/// Underlying filesystem failure.
		#[source]
		source: io::Error,
	},
}

/// Filesystem projection of one session's `local://` namespace.
#[derive(Clone, Debug)]
pub struct PlanArtifactStore {
	root: PathBuf,
}

impl PlanArtifactStore {
	/// Creates an authority rooted at the session's already-authorized local
	/// artifact directory.
	#[must_use]
	pub fn new(root: PathBuf) -> Self {
		Self { root }
	}

	/// Returns the local artifact root.
	#[must_use]
	pub fn root(&self) -> &Path {
		&self.root
	}

	/// Writes the canonical `local://PLAN.md` artifact.
	pub fn write_canonical(&self, content: &str) -> Result<PlanArtifact, PlanArtifactError> {
		self.write_url(DEFAULT_PLAN_URL, content)?;
		Ok(self.finalize(DEFAULT_PLAN_URL, content, None))
	}

	/// Writes a normalized `local://<slug>-plan.md` artifact. A redundant
	/// trailing `-plan` or `_plan` suffix is removed before adding `-plan.md`.
	pub fn write_named(
		&self,
		title: &str,
		content: &str,
	) -> Result<PlanArtifact, PlanArtifactError> {
		let normalized = normalize_title(title)?;
		let slug = normalized
			.strip_suffix("-plan")
			.or_else(|| normalized.strip_suffix("_plan"))
			.unwrap_or(normalized.as_str());
		let url = sf!("local://{slug}-plan.md");
		self.write_url(url.as_str(), content)?;
		Ok(self.finalize(url.as_str(), content, Some(title)))
	}

	/// Lists regular plan artifacts newest first. Cwd files are never scanned.
	pub fn list(&self) -> Result<Vec<Str>, PlanArtifactError> {
		let entries = match fs::read_dir(&self.root) {
			Ok(entries) => entries,
			Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
			Err(source) => return Err(self.io_error(self.root.clone(), source)),
		};
		let mut plans = Vec::new();
		for entry in entries {
			let entry = entry.map_err(|source| self.io_error(self.root.clone(), source))?;
			let file_type = entry
				.file_type()
				.map_err(|source| self.io_error(entry.path(), source))?;
			if !file_type.is_file() {
				continue;
			}
			let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
				continue;
			};
			if !name
				.get(name.len().saturating_sub("plan.md".len())..)
				.is_some_and(|suffix| suffix.eq_ignore_ascii_case("plan.md"))
			{
				continue;
			}
			let modified = entry
				.metadata()
				.and_then(|metadata| metadata.modified())
				.unwrap_or(SystemTime::UNIX_EPOCH);
			plans.push((modified, sf!("local://{name}")));
		}
		plans.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
		Ok(plans.into_iter().map(|(_, url)| url).collect())
	}

	/// Resolves approval input in parity order: supplied slug, a state reference
	/// absent from the scan, newest scanned artifacts, then the state reference.
	pub fn resolve(
		&self,
		supplied_title: Option<&str>,
		state_reference: &str,
	) -> Result<PlanArtifact, PlanArtifactError> {
		let listed = self.list()?;
		let mut ordered = Vec::<Str>::new();
		if let Some(title) = supplied_title
			&& let Ok(normalized) = normalize_title(title)
		{
			let slug = normalized
				.strip_suffix("-plan")
				.or_else(|| normalized.strip_suffix("_plan"))
				.unwrap_or(normalized.as_str());
			push_unique(&mut ordered, sf!("local://{slug}-plan.md"));
		}
		let canonical_state = canonical_url(state_reference)?;
		if !listed
			.iter()
			.any(|candidate| canonical_url(candidate.as_str()).is_ok_and(|url| url == canonical_state))
		{
			push_unique(&mut ordered, canonical_state.clone());
		}
		for url in listed {
			push_unique(&mut ordered, canonical_url(url.as_str())?);
		}
		push_unique(&mut ordered, canonical_state);

		for url in &ordered {
			if let Some(content) = self.read_url(url.as_str())? {
				return Ok(self.finalize(url.as_str(), content.as_str(), supplied_title));
			}
		}
		Err(PlanArtifactError::NotFound {
			target: ordered
				.first()
				.cloned()
				.unwrap_or_else(|| sf!(DEFAULT_PLAN_URL)),
		})
	}

	fn finalize(&self, url: &str, content: &str, supplied: Option<&str>) -> PlanArtifact {
		let (title, source) = derive_title(supplied, content, url);
		PlanArtifact { url: Str::new(url), title, source, content: Str::new(content) }
	}

	fn write_url(&self, url: &str, content: &str) -> Result<(), PlanArtifactError> {
		let path = self.path_for(url)?;
		fs::create_dir_all(&self.root).map_err(|source| self.io_error(self.root.clone(), source))?;
		fs::write(&path, content).map_err(|source| self.io_error(path, source))
	}

	fn read_url(&self, url: &str) -> Result<Option<Str>, PlanArtifactError> {
		let path = self.path_for(url)?;
		match fs::read_to_string(&path) {
			Ok(content) => Ok(Some(Str::from(content))),
			Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
			Err(source) => Err(self.io_error(path, source)),
		}
	}

	fn path_for(&self, url: &str) -> Result<PathBuf, PlanArtifactError> {
		let canonical = canonical_url(url)?;
		let relative = canonical
			.strip_prefix("local://")
			.ok_or(PlanArtifactError::InvalidReference)?;
		let relative = Path::new(relative.as_str());
		if relative.as_os_str().is_empty()
			|| relative
				.components()
				.any(|part| !matches!(part, Component::Normal(_)))
		{
			return Err(PlanArtifactError::InvalidReference);
		}
		Ok(self.root.join(relative))
	}

	fn io_error(&self, path: PathBuf, source: io::Error) -> PlanArtifactError {
		PlanArtifactError::Io { path, source }
	}
}

/// Returns one canonical `local://` spelling for comparisons and reads.
pub fn canonical_url(reference: &str) -> Result<Str, PlanArtifactError> {
	let reference = reference.trim();
	let relative = reference
		.strip_prefix("local://")
		.or_else(|| reference.strip_prefix("local:/"))
		.or_else(|| reference.strip_prefix("local:"))
		.ok_or(PlanArtifactError::InvalidReference)?
		.trim_start_matches('/');
	if relative.is_empty() {
		return Err(PlanArtifactError::InvalidReference);
	}
	Ok(sf!("local://{relative}"))
}

fn normalize_title(title: &str) -> Result<String, PlanArtifactError> {
	let trimmed = title.trim();
	if trimmed.is_empty()
		|| trimmed.contains('/')
		|| trimmed.contains('\\')
		|| trimmed.contains("..")
	{
		return Err(PlanArtifactError::InvalidTitle);
	}
	let without_extension = trimmed
		.strip_suffix(".md")
		.or_else(|| trimmed.strip_suffix(".MD"))
		.unwrap_or(trimmed);
	let mut output = String::with_capacity(without_extension.len());
	let mut prior_dash = false;
	for character in without_extension.chars() {
		if character.is_ascii_alphanumeric() || character == '_' {
			output.push(character);
			prior_dash = false;
		} else if (character.is_ascii_whitespace() || character == '-') && !prior_dash {
			output.push('-');
			prior_dash = true;
		}
	}
	let normalized = output.trim_matches('-').to_owned();
	if normalized.is_empty() {
		return Err(PlanArtifactError::InvalidTitle);
	}
	Ok(normalized)
}

fn derive_title(supplied: Option<&str>, content: &str, url: &str) -> (Str, PlanTitleSource) {
	if let Some(title) = supplied
		&& let Ok(title) = normalize_title(title)
	{
		return (Str::from(title), PlanTitleSource::Supplied);
	}
	if let Some(heading) = content.lines().find_map(|line| {
		let line = line.trim();
		line
			.strip_prefix("# ")
			.map(str::trim)
			.filter(|value| !value.is_empty())
	}) && let Ok(title) = normalize_title(heading)
	{
		return (Str::from(title), PlanTitleSource::Heading);
	}
	let stem = url
		.rsplit('/')
		.next()
		.unwrap_or_default()
		.strip_suffix(".md")
		.or_else(|| {
			url.rsplit('/')
				.next()
				.unwrap_or_default()
				.strip_suffix(".MD")
		})
		.unwrap_or_default();
	if let Ok(title) = normalize_title(stem) {
		return (Str::from(title), PlanTitleSource::Filename);
	}
	(sf!("plan"), PlanTitleSource::Default)
}

fn push_unique(values: &mut Vec<Str>, value: Str) {
	if !values.contains(&value) {
		values.push(value);
	}
}
