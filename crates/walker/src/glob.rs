//! Compiled glob matching for names and path sets.
//!
//! [`CompiledPattern`] uses fnmatch-style path semantics: `*`, `?`, and
//! bracket classes do not match path separators, while `**` can span them.
//! Match a basename by passing the basename itself rather than its full path.
//! [`CompiledGlobSet`] matches any of several patterns and defaults to
//! `globset`-style separator handling; use
//! [`GlobSetBuilder::literal_separator`] when path separators must be matched
//! literally.

use std::{
	fmt,
	hash::{Hash, Hasher},
	path::Path,
	sync::Arc,
};

use globset::{GlobBuilder as EngineGlobBuilder, GlobSet as EngineGlobSet};
use thiserror::Error;

/// An error compiling a glob pattern or pattern set.
#[derive(Debug, Error)]
#[error("invalid glob pattern: {source}")]
pub struct GlobError {
	#[source]
	source: globset::Error,
}

impl GlobError {
	fn from_source(source: globset::Error) -> Self {
		Self { source }
	}
}

#[derive(Debug)]
struct PatternInner {
	pattern:           String,
	literal_separator: bool,
	case_insensitive:  bool,
	matcher:           globset::GlobMatcher,
}

/// One compiled fnmatch-style glob pattern.
///
/// Cloning is constant-time. Equality and hashing use the source pattern and
/// compilation options rather than engine internals.
#[derive(Clone)]
pub struct CompiledPattern(Arc<PatternInner>);

impl CompiledPattern {
	/// Compiles `pattern` with fnmatch-style separator handling.
	pub fn new(pattern: &str) -> Result<Self, GlobError> {
		PatternBuilder::new(pattern).build()
	}

	/// Creates a builder for `pattern` with fnmatch-style defaults.
	pub const fn builder(pattern: &str) -> PatternBuilder<'_> {
		PatternBuilder::new(pattern)
	}

	/// Escapes all glob metacharacters so `text` is matched literally.
	pub fn escape(text: &str) -> String {
		globset::escape(text)
	}

	/// Returns whether this pattern matches `candidate`.
	pub fn matches(&self, candidate: &str) -> bool {
		self.0.matcher.is_match(candidate)
	}

	/// Returns whether this pattern matches `candidate` as a filesystem path.
	pub fn matches_path(&self, candidate: &Path) -> bool {
		self.0.matcher.is_match(candidate)
	}

	/// Returns the source pattern.
	pub fn pattern(&self) -> &str {
		&self.0.pattern
	}
}

impl fmt::Debug for CompiledPattern {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("CompiledPattern")
			.field("pattern", &self.0.pattern)
			.field("literal_separator", &self.0.literal_separator)
			.field("case_insensitive", &self.0.case_insensitive)
			.finish()
	}
}

impl PartialEq for CompiledPattern {
	fn eq(&self, other: &Self) -> bool {
		self.0.pattern == other.0.pattern
			&& self.0.literal_separator == other.0.literal_separator
			&& self.0.case_insensitive == other.0.case_insensitive
	}
}

impl Eq for CompiledPattern {}

impl Hash for CompiledPattern {
	fn hash<H: Hasher>(&self, state: &mut H) {
		self.0.pattern.hash(state);
		self.0.literal_separator.hash(state);
		self.0.case_insensitive.hash(state);
	}
}

/// Builder for one [`CompiledPattern`].
#[derive(Clone, Copy, Debug)]
pub struct PatternBuilder<'a> {
	pattern:           &'a str,
	literal_separator: bool,
	case_insensitive:  bool,
}

impl<'a> PatternBuilder<'a> {
	/// Creates a builder with fnmatch-style literal separators and
	/// case-sensitive matching.
	pub const fn new(pattern: &'a str) -> Self {
		Self { pattern, literal_separator: true, case_insensitive: false }
	}

	/// Sets whether wildcards may match path separators.
	pub const fn literal_separator(mut self, enabled: bool) -> Self {
		self.literal_separator = enabled;
		self
	}

	/// Sets whether matching ignores case.
	pub const fn case_insensitive(mut self, enabled: bool) -> Self {
		self.case_insensitive = enabled;
		self
	}

	/// Compiles the pattern.
	pub fn build(self) -> Result<CompiledPattern, GlobError> {
		let glob = EngineGlobBuilder::new(self.pattern)
			.literal_separator(self.literal_separator)
			.case_insensitive(self.case_insensitive)
			.build()
			.map_err(GlobError::from_source)?;
		Ok(CompiledPattern(Arc::new(PatternInner {
			pattern:           self.pattern.to_owned(),
			literal_separator: self.literal_separator,
			case_insensitive:  self.case_insensitive,
			matcher:           glob.compile_matcher(),
		})))
	}
}

#[derive(Debug)]
struct GlobSetInner {
	patterns:          Arc<[String]>,
	literal_separator: bool,
	case_insensitive:  bool,
	matcher:           EngineGlobSet,
}

/// A compiled matcher that accepts a candidate matching any source pattern.
///
/// Cloning is constant-time. Equality and hashing use the ordered source
/// patterns and compilation options rather than engine internals.
#[derive(Clone)]
pub struct CompiledGlobSet(Arc<GlobSetInner>);

impl CompiledGlobSet {
	/// Compiles patterns with standard set semantics, where wildcards may cross
	/// separators and matching is case-sensitive.
	pub fn new<P, I>(patterns: I) -> Result<Self, GlobError>
	where
		P: AsRef<str>,
		I: IntoIterator<Item = P>,
	{
		GlobSetBuilder::new().build(patterns)
	}

	/// Creates a builder for a compiled pattern set.
	pub const fn builder() -> GlobSetBuilder {
		GlobSetBuilder::new()
	}

	/// Returns whether any pattern matches `candidate`.
	pub fn matches(&self, candidate: &str) -> bool {
		self.0.matcher.is_match(candidate)
	}

	/// Returns whether any pattern matches `candidate` as a filesystem path.
	pub fn matches_path(&self, candidate: &Path) -> bool {
		self.0.matcher.is_match(candidate)
	}

	/// Returns the ordered source patterns.
	pub fn patterns(&self) -> &[String] {
		&self.0.patterns
	}
}

impl fmt::Debug for CompiledGlobSet {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("CompiledGlobSet")
			.field("patterns", &self.0.patterns)
			.field("literal_separator", &self.0.literal_separator)
			.field("case_insensitive", &self.0.case_insensitive)
			.finish()
	}
}

impl PartialEq for CompiledGlobSet {
	fn eq(&self, other: &Self) -> bool {
		self.0.patterns == other.0.patterns
			&& self.0.literal_separator == other.0.literal_separator
			&& self.0.case_insensitive == other.0.case_insensitive
	}
}

impl Eq for CompiledGlobSet {}

impl Hash for CompiledGlobSet {
	fn hash<H: Hasher>(&self, state: &mut H) {
		self.0.patterns.hash(state);
		self.0.literal_separator.hash(state);
		self.0.case_insensitive.hash(state);
	}
}

/// Builder for a [`CompiledGlobSet`].
#[derive(Clone, Copy, Debug, Default)]
pub struct GlobSetBuilder {
	literal_separator: bool,
	case_insensitive:  bool,
}

impl GlobSetBuilder {
	/// Creates a case-sensitive builder whose wildcards may cross separators.
	pub const fn new() -> Self {
		Self { literal_separator: false, case_insensitive: false }
	}

	/// Sets whether wildcards may match path separators.
	pub const fn literal_separator(mut self, enabled: bool) -> Self {
		self.literal_separator = enabled;
		self
	}

	/// Sets whether matching ignores case.
	pub const fn case_insensitive(mut self, enabled: bool) -> Self {
		self.case_insensitive = enabled;
		self
	}

	/// Compiles `patterns` into one matcher.
	pub fn build<P, I>(self, patterns: I) -> Result<CompiledGlobSet, GlobError>
	where
		P: AsRef<str>,
		I: IntoIterator<Item = P>,
	{
		let mut engine = globset::GlobSetBuilder::new();
		let mut sources = Vec::new();
		for pattern in patterns {
			let pattern = pattern.as_ref();
			let glob = EngineGlobBuilder::new(pattern)
				.literal_separator(self.literal_separator)
				.case_insensitive(self.case_insensitive)
				.build()
				.map_err(GlobError::from_source)?;
			engine.add(glob);
			sources.push(pattern.to_owned());
		}
		let matcher = engine.build().map_err(GlobError::from_source)?;
		Ok(CompiledGlobSet(Arc::new(GlobSetInner {
			patterns: sources.into(),
			literal_separator: self.literal_separator,
			case_insensitive: self.case_insensitive,
			matcher,
		})))
	}
}

/// A walk-relative glob set whose wildcards do not cross separators.
///
/// This compatibility wrapper preserves the traversal filter's established
/// construction, matching, equality, and hashing behavior.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CompiledWalkGlob(CompiledGlobSet);

impl CompiledWalkGlob {
	/// Compiles normalized patterns for walk-relative paths.
	pub fn new<P, I>(patterns: I) -> Result<Self, GlobError>
	where
		P: AsRef<str>,
		I: IntoIterator<Item = P>,
	{
		GlobSetBuilder::new()
			.literal_separator(true)
			.build(patterns)
			.map(Self)
	}

	/// Returns whether `relative` matches any compiled pattern.
	pub fn is_match(&self, relative: &str) -> bool {
		self.0.matches(relative)
	}

	/// Returns the normalized source patterns.
	pub fn patterns(&self) -> &[String] {
		self.0.patterns()
	}
}

#[cfg(test)]
mod tests {
	use std::path::Path;

	use super::{CompiledGlobSet, CompiledPattern, GlobSetBuilder, PatternBuilder};

	#[test]
	fn fnmatch_wildcards_respect_separators_and_double_star_crosses_them() {
		let shallow = CompiledPattern::new("*.rs").expect("valid pattern");
		assert!(shallow.matches("lib.rs"));
		assert!(!shallow.matches("src/lib.rs"));
		assert!(!shallow.matches_path(Path::new("src/lib.rs")));

		let recursive = CompiledPattern::new("**/*.rs").expect("valid pattern");
		assert!(recursive.matches("src/lib.rs"));
	}

	#[test]
	fn fnmatch_classes_and_basename_candidates_match() {
		let pattern = CompiledPattern::new("file[0-9].rs").expect("valid class");
		assert!(pattern.matches("file7.rs"));
		assert!(!pattern.matches("filex.rs"));
		assert!(pattern.matches_path(Path::new("file2.rs")));
		assert!(!pattern.matches_path(Path::new("src/file2.rs")));
	}

	#[test]
	fn escaped_pattern_matches_metacharacters_literally() {
		let escaped = CompiledPattern::escape("bad[pattern");
		let pattern = CompiledPattern::new(&escaped).expect("escaped pattern");
		assert!(pattern.matches("bad[pattern"));
	}

	#[test]
	fn set_literal_separator_option_changes_path_matching() {
		let loose = CompiledGlobSet::new(["*.rs"]).expect("valid set");
		let literal = GlobSetBuilder::new()
			.literal_separator(true)
			.build(["*.rs"])
			.expect("valid set");
		assert!(loose.matches("src/lib.rs"));
		assert!(!literal.matches("src/lib.rs"));
	}

	#[test]
	fn builders_support_case_insensitive_matching() {
		let pattern = PatternBuilder::new("README.*")
			.case_insensitive(true)
			.build()
			.expect("valid pattern");
		assert!(pattern.matches("readme.md"));

		let set = GlobSetBuilder::new()
			.case_insensitive(true)
			.build(["SRC/**"])
			.expect("valid set");
		assert!(set.matches("src/lib.rs"));
	}

	#[test]
	fn invalid_pattern_returns_walker_error() {
		assert!(CompiledPattern::new("[").is_err());
		assert!(CompiledGlobSet::new(["ok", "["]).is_err());
	}
}
