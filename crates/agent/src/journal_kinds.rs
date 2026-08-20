//! Declared extension journal-kind registry and Rust-enforced read scoping.

use std::{collections::HashMap, fmt};

use omp_core::{IntoStr, Str};
use omp_tool::{Rev, RevParseError};
use thiserror::Error;

/// Statically dispatched migration hook for a declared entry kind.
///
/// The hook receives the recorded revision and canonical JSON bytes. Returning
/// `None` preserves the record at its original revision.
pub type LiftHook = fn(from_rev: &Rev, raw: &[u8]) -> Option<Box<serde_json::value::RawValue>>;

/// One entry-kind declaration received while loading an extension.
#[derive(Clone)]
pub struct EntryKindDecl {
	/// Globally unique reverse-DNS kind name.
	pub name:     Str,
	/// Live `family.n` schema revision.
	pub rev:      Rev,
	/// Whether entries display by default.
	pub display:  bool,
	/// Whether this kind has a model-context projection.
	pub projects: bool,
	/// Optional static revision lift hook.
	pub lift:     Option<LiftHook>,
}

impl fmt::Debug for EntryKindDecl {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("EntryKindDecl")
			.field("name", &self.name)
			.field("rev", &self.rev)
			.field("display", &self.display)
			.field("projects", &self.projects)
			.field("lift", &self.lift.map(|_| "fn"))
			.finish()
	}
}

impl EntryKindDecl {
	/// Parses a declaration's canonical revision spelling.
	pub fn parse(
		name: impl IntoStr,
		rev: &str,
		display: bool,
		projects: bool,
		lift: Option<LiftHook>,
	) -> Result<Self, RevParseError> {
		Ok(Self { name: name.into_str(), rev: rev.parse()?, display, projects, lift })
	}
}

/// Live registry record for one declared entry kind.
#[derive(Clone)]
pub struct KindRecord {
	/// Live schema revision.
	pub rev:       Rev,
	/// Default display policy.
	pub display:   bool,
	/// Whether the kind can project into model context.
	pub projects:  bool,
	/// Authenticated extension that owns the kind namespace.
	pub extension: Str,
	/// Optional static revision lift hook.
	pub lift:      Option<LiftHook>,
}

impl fmt::Debug for KindRecord {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter
			.debug_struct("KindRecord")
			.field("rev", &self.rev)
			.field("display", &self.display)
			.field("projects", &self.projects)
			.field("extension", &self.extension)
			.field("lift", &self.lift.map(|_| "fn"))
			.finish()
	}
}

impl KindRecord {
	fn matches(&self, extension: &str, declaration: &EntryKindDecl) -> bool {
		self.extension == extension
			&& self.rev == declaration.rev
			&& self.display == declaration.display
			&& self.projects == declaration.projects
			&& match (self.lift, declaration.lift) {
				(None, None) => true,
				(Some(left), Some(right)) => std::ptr::fn_addr_eq(left, right),
				_ => false,
			}
	}
}

/// Entry-kind declaration or access failure.
#[derive(Debug, Error)]
pub enum EntryKindError {
	/// A declaration tried to claim a core-reserved or non-namespaced name.
	#[error("entry kind `{0}` is reserved for the core")]
	ReservedName(Str),
	/// A name was already declared with different ownership or metadata.
	#[error("entry kind `{0}` conflicts with an existing declaration")]
	Conflict(Str),
	/// An append referenced a kind absent from the live declaration registry.
	#[error("entry kind `{0}` is not declared")]
	Unknown(Str),
	/// A caller attempted to read another extension's namespace without a grant.
	#[error("extension `{extension}` may not read entry kind `{kind}`")]
	AccessDenied {
		/// Calling extension identity.
		extension: Str,
		/// Denied entry kind.
		kind:      Str,
	},
}

/// Live, core-owned registry of extension journal kinds.
#[derive(Debug, Default)]
pub struct EntryKindRegistry {
	kinds: HashMap<Str, KindRecord>,
}

impl EntryKindRegistry {
	/// Creates an empty registry.
	#[must_use]
	pub fn new() -> Self {
		Self::default()
	}

	/// Declares one extension's complete kind set atomically.
	///
	/// Exact repeats are idempotent. Any conflict rejects the entire set without
	/// changing the registry, allowing the extension loader to fail closed.
	pub fn declare_extension(
		&mut self,
		extension: &str,
		declarations: impl IntoIterator<Item = EntryKindDecl>,
	) -> Result<(), EntryKindError> {
		let declarations = declarations.into_iter().collect::<Vec<_>>();
		let mut staged = HashMap::<&str, &EntryKindDecl>::with_capacity(declarations.len());
		for declaration in &declarations {
			if !declaration.name.contains('.') || declaration.name.starts_with("omp.") {
				return Err(EntryKindError::ReservedName(declaration.name.clone()));
			}
			if let Some(previous) = staged.insert(declaration.name.as_str(), declaration)
				&& (!same_declaration(previous, declaration))
			{
				return Err(EntryKindError::Conflict(declaration.name.clone()));
			}
			if let Some(record) = self.kinds.get(declaration.name.as_str())
				&& !record.matches(extension, declaration)
			{
				return Err(EntryKindError::Conflict(declaration.name.clone()));
			}
		}
		for declaration in declarations {
			self
				.kinds
				.entry(declaration.name)
				.or_insert_with(|| KindRecord {
					rev:       declaration.rev,
					display:   declaration.display,
					projects:  declaration.projects,
					extension: Str::new(extension),
					lift:      declaration.lift,
				});
		}
		Ok(())
	}

	/// Returns the live record required to append `kind`.
	pub fn require_declared(&self, kind: &str) -> Result<&KindRecord, EntryKindError> {
		self
			.kinds
			.get(kind)
			.ok_or_else(|| EntryKindError::Unknown(Str::new(kind)))
	}

	/// Returns a kind record when the caller may read its namespace.
	///
	/// Core kinds are outside this registry and are accepted by
	/// [`Self::can_read_core`]. `granted_extensions` contains authenticated
	/// extension identities from the caller's manifest grants.
	pub fn authorize_read<'a, 'grant>(
		&'a self,
		caller_extension: &str,
		granted_extensions: impl IntoIterator<Item = &'grant str>,
		kind: &str,
	) -> Result<&'a KindRecord, EntryKindError> {
		let record = self.require_declared(kind)?;
		if record.extension == caller_extension
			|| granted_extensions
				.into_iter()
				.any(|extension| record.extension == extension)
		{
			return Ok(record);
		}
		Err(EntryKindError::AccessDenied {
			extension: Str::new(caller_extension),
			kind:      Str::new(kind),
		})
	}

	/// Reports whether a name belongs to the core-readable namespace.
	#[must_use]
	pub fn can_read_core(kind: &str) -> bool {
		!kind.contains('.') || kind.starts_with("omp.")
	}

	/// Iterates declared names and records without allocation.
	pub fn iter(&self) -> impl Iterator<Item = (&str, &KindRecord)> + '_ {
		self
			.kinds
			.iter()
			.map(|(name, record)| (name.as_str(), record))
	}
}

fn same_declaration(left: &EntryKindDecl, right: &EntryKindDecl) -> bool {
	left.rev == right.rev
		&& left.display == right.display
		&& left.projects == right.projects
		&& match (left.lift, right.lift) {
			(None, None) => true,
			(Some(left), Some(right)) => std::ptr::fn_addr_eq(left, right),
			_ => false,
		}
}
