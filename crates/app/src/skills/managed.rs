//! Environment-authorized publication of model-generated managed skills.

use std::{
	collections::BTreeSet,
	io,
	path::{Path, PathBuf},
};

use omp_core::Str;

use crate::discovery::skills::safe_skill_name;

/// Sanitized managed-skill candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedSkillCandidate {
	/// Safe directory and invocation name.
	pub name:        Str,
	/// Single-line bounded description.
	pub description: Str,
	/// Markdown body.
	pub body:        Str,
}

/// Successful Environment-owned publication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedSkillPublication {
	/// Canonical published `SKILL.md` path returned by Environment.
	pub path:     PathBuf,
	/// Environment document revision.
	pub revision: u64,
}

/// Managed skill rejection or authority failure.
#[derive(Debug, thiserror::Error)]
pub enum ManagedSkillError {
	/// Candidate name is unsafe or malformed.
	#[error("managed skill name is invalid")]
	InvalidName,
	/// Candidate description is empty after sanitization.
	#[error("managed skill description is invalid")]
	InvalidDescription,
	/// An authored skill already claims the requested name.
	#[error("managed skill name is claimed by an authored skill")]
	AuthoredCollision,
	/// Environment refused or failed the authorized write.
	#[error("Environment failed to publish managed skill {path}")]
	Write {
		/// Requested managed skill path.
		path:   PathBuf,
		/// Authority error.
		#[source]
		source: io::Error,
	},
}

/// Narrow Environment write authority. Implementations must validate the
/// durable approval/grant represented by the caller before committing bytes.
pub trait ManagedSkillWriter {
	/// Atomically writes one new/replaced managed skill and returns its
	/// committed revision. Discovery itself never calls `std::fs::write`.
	fn write_managed_skill(&self, path: &Path, bytes: &[u8]) -> io::Result<ManagedSkillPublication>;
}

/// Sanitizes a model-generated candidate. Names are lowercase directory-style
/// identifiers; descriptions collapse control/line whitespace and are bounded
/// to 240 bytes on a UTF-8 boundary.
pub fn sanitize(
	name: &str,
	description: &str,
	body: &str,
) -> Result<ManagedSkillCandidate, ManagedSkillError> {
	let mut safe_name = String::with_capacity(name.len());
	let mut separator = false;
	for character in name.trim().chars().flat_map(char::to_lowercase) {
		if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
			safe_name.push(character);
			separator = false;
		} else if !separator && !safe_name.is_empty() {
			safe_name.push('-');
			separator = true;
		}
	}
	while safe_name.ends_with('-') {
		safe_name.pop();
	}
	if !safe_skill_name(&safe_name) {
		return Err(ManagedSkillError::InvalidName);
	}
	let mut safe_description = description.split_whitespace().collect::<Vec<_>>().join(" ");
	if safe_description.is_empty() {
		return Err(ManagedSkillError::InvalidDescription);
	}
	if safe_description.len() > 240 {
		safe_description.truncate(safe_description.floor_char_boundary(240));
	}
	Ok(ManagedSkillCandidate {
		name:        Str::from(safe_name),
		description: Str::from(safe_description),
		body:        Str::from(body.trim()),
	})
}

/// Publishes a managed skill dead-last in precedence, refusing every authored
/// claim and routing the only mutation through Environment authority.
pub fn publish(
	writer: &impl ManagedSkillWriter,
	managed_root: &Path,
	candidate: &ManagedSkillCandidate,
	authored_names: &BTreeSet<Str>,
) -> Result<ManagedSkillPublication, ManagedSkillError> {
	if authored_names.contains(&candidate.name) {
		return Err(ManagedSkillError::AuthoredCollision);
	}
	let path = managed_root.join(candidate.name.as_str()).join("SKILL.md");
	let mut bytes = String::new();
	bytes.push_str("---\nname: ");
	bytes.push_str(candidate.name.as_str());
	bytes.push_str("\ndescription: ");
	// YAML single quotes are deterministic and escape embedded quotes by doubling.
	bytes.push('\'');
	bytes.push_str(&candidate.description.replace('\'', "''"));
	bytes.push('\'');
	bytes.push_str("\n---\n");
	bytes.push_str(candidate.body.as_str());
	bytes.push('\n');
	writer
		.write_managed_skill(&path, bytes.as_bytes())
		.map_err(|source| ManagedSkillError::Write { path, source })
}

#[cfg(test)]
mod tests {
	use std::{cell::RefCell, fs};

	use super::*;

	struct Writer {
		root:   PathBuf,
		writes: RefCell<usize>,
	}
	impl ManagedSkillWriter for Writer {
		fn write_managed_skill(
			&self,
			path: &Path,
			bytes: &[u8],
		) -> io::Result<ManagedSkillPublication> {
			assert!(path.starts_with(&self.root));
			fs::create_dir_all(path.parent().unwrap())?;
			fs::write(path, bytes)?;
			*self.writes.borrow_mut() += 1;
			Ok(ManagedSkillPublication { path: path.to_path_buf(), revision: 1 })
		}
	}

	#[test]
	fn sanitizes_and_refuses_authored_claims_before_write() {
		let tree = tempfile::tempdir().unwrap();
		let writer = Writer { root: tree.path().to_path_buf(), writes: RefCell::new(0) };
		let candidate = sanitize(" Rust Review!! ", " useful\n review ", "body").unwrap();
		assert_eq!(candidate.name, "rust-review");
		let claims = BTreeSet::from([candidate.name.clone()]);
		assert!(matches!(
			publish(&writer, tree.path(), &candidate, &claims),
			Err(ManagedSkillError::AuthoredCollision)
		));
		assert_eq!(*writer.writes.borrow(), 0);
	}
}
