//! Project-scoped runtime state kept outside tool-writable workspaces.

use std::{
	io,
	path::{Path, PathBuf},
};

use omp_core::{Hash32, Str, encoding::hex};
use omp_storage::{atomic, index::SessionInfo, transcript::SessionId};
use thiserror::Error;

/// Journal relocation failure.
#[derive(Debug, Error)]
pub enum RelocateError {
	/// Destination already exists and must never be overwritten.
	#[error("session journal destination already exists: {0}")]
	DestinationExists(PathBuf),
	/// A filesystem operation failed.
	#[error("failed to relocate session journal from {source_path} to {destination_path}")]
	Io {
		/// Existing journal path.
		source_path:      PathBuf,
		/// New journal path.
		destination_path: PathBuf,
		/// Underlying filesystem failure.
		#[source]
		source:           std::io::Error,
	},
}

/// Failure to persist or resolve owner-local session breadcrumbs.
#[derive(Debug, Error)]
pub enum SessionResolveError {
	/// Breadcrumb storage failed.
	#[error("failed to update terminal session breadcrumb")]
	Breadcrumb(#[from] atomic::Error),
	/// Breadcrumb directory setup or reading failed.
	#[error("terminal session breadcrumb I/O failed")]
	Io(#[from] io::Error),
	/// No indexed session matched the selector.
	#[error("no session matches selector {selector}")]
	NotFound {
		/// Rejected selector.
		selector: Str,
	},
	/// More than one indexed session matched a UUID fragment or prefix.
	#[error("session selector {selector} is ambiguous")]
	Ambiguous {
		/// Ambiguous selector.
		selector: Str,
		/// Matching stable session identifiers.
		matches:  Vec<SessionId>,
	},
}

/// Owner-local per-terminal pointer used by interactive `--continue`.
pub struct TerminalBreadcrumbs {
	directory: PathBuf,
}

impl TerminalBreadcrumbs {
	/// Creates a breadcrumb store below the owner's private data directory.
	pub fn new(data_dir: &Path) -> Result<Self, SessionResolveError> {
		let directory = data_dir.join("terminals");
		std::fs::create_dir_all(&directory)?;
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt as _;
			std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
		}
		Ok(Self { directory })
	}

	fn path(&self, terminal: &str) -> PathBuf {
		let digest = Hash32::sum(terminal.as_bytes());
		let short: &[u8; 16] = digest.as_bytes()[..16]
			.try_into()
			.expect("a Blake3 digest contains 16 prefix bytes");
		self.directory.join(hex::encode_n(short).as_str())
	}

	/// Atomically points `terminal` at the newly active session.
	pub fn restamp(&self, terminal: &str, session: &SessionId) -> Result<(), SessionResolveError> {
		atomic::commit(&self.path(terminal), session.0.as_bytes(), || true)?;
		Ok(())
	}

	/// Reads the active session previously stamped for `terminal`.
	pub fn read(&self, terminal: &str) -> Result<Option<SessionId>, SessionResolveError> {
		match std::fs::read_to_string(self.path(terminal)) {
			Ok(value) => Ok(Some(SessionId(Str::from(value.trim())))),
			Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
			Err(error) => Err(error.into()),
		}
	}
}

/// Resolves `@latest`, exact IDs, unique title prefixes, and unique UUID
/// fragments against an already project-filtered newest-first index page.
pub fn resolve_session_selector(
	sessions: &[SessionInfo],
	selector: &str,
) -> Result<SessionId, SessionResolveError> {
	if selector == "@latest" {
		return sessions
			.first()
			.map(|session| session.id.clone())
			.ok_or_else(|| SessionResolveError::NotFound { selector: Str::from(selector) });
	}
	if let Some(exact) = sessions
		.iter()
		.find(|session| session.id.0.as_str() == selector)
	{
		return Ok(exact.id.clone());
	}
	let mut matches = sessions
		.iter()
		.filter(|session| {
			session.id.0.as_str().contains(selector)
				|| session
					.title
					.as_ref()
					.is_some_and(|title| title.as_str().starts_with(selector))
		})
		.map(|session| session.id.clone());
	let Some(first) = matches.next() else {
		return Err(SessionResolveError::NotFound { selector: Str::from(selector) });
	};
	let Some(second) = matches.next() else {
		return Ok(first);
	};
	let mut ambiguous = vec![first, second];
	ambiguous.extend(matches);
	Err(SessionResolveError::Ambiguous { selector: Str::from(selector), matches: ambiguous })
}

/// Relocates exact journal bytes without rewriting historical workspace state.
///
/// A fileless untouched session remains fileless and reports `Ok(false)`.
/// Existing journals are renamed on the same filesystem; their v4 header and
/// every historical workspace event remain byte-identical.
pub fn relocate_journal(source: &Path, destination: &Path) -> Result<bool, RelocateError> {
	if !source.exists() {
		return Ok(false);
	}
	if destination.exists() {
		return Err(RelocateError::DestinationExists(destination.to_owned()));
	}
	if let Some(parent) = destination.parent() {
		std::fs::create_dir_all(parent).map_err(|source_error| RelocateError::Io {
			source_path:      source.to_owned(),
			destination_path: destination.to_owned(),
			source:           source_error,
		})?;
	}
	std::fs::rename(source, destination).map_err(|source_error| RelocateError::Io {
		source_path:      source.to_owned(),
		destination_path: destination.to_owned(),
		source:           source_error,
	})?;
	Ok(true)
}
/// data directory.
///
/// Canonicalizing the project root gives aliases and symlinked paths one stable
/// state identity.
pub fn directory(data_dir: &Path, project_root: &Path) -> io::Result<PathBuf> {
	let root = std::fs::canonicalize(project_root)?;
	let digest = Hash32::sum(root.as_os_str().as_encoded_bytes());
	Ok(data_dir
		.join("projects")
		.join(hex::encode_n(digest.as_bytes()).as_str()))
}

/// Returns the short owner-local environment socket path for `state_dir`.
///
/// The path is keyed by the running executable's filesystem generation: a
/// rebuilt `omp` binds its own listener immediately while stale-build listeners
/// drain and idle-exit, with no takeover protocol. The document socket stays
/// build-stable because its authority must remain singular per project.
#[cfg(unix)]
pub(crate) fn environment_socket(state_dir: &Path) -> PathBuf {
	let build = crate::build_id::current();
	let key = if build.is_empty() {
		"unknown"
	} else {
		&build[..8]
	};
	socket_path(state_dir, &format!("{key}-env"))
}

/// Returns the deterministic current-user environment named pipe.
///
/// The executable-generation key lets rebuilt owners bind immediately while
/// older listeners drain independently.
#[cfg(windows)]
pub(crate) fn environment_socket(state_dir: &Path) -> PathBuf {
	let build = crate::build_id::current();
	let key = if build.is_empty() {
		"unknown"
	} else {
		&build[..8]
	};
	windows_pipe_path(state_dir, &format!("{key}-env"))
}

/// Returns the short owner-local document socket path for `state_dir`.
#[cfg(unix)]
pub fn document_socket(state_dir: &Path) -> PathBuf {
	socket_path(state_dir, "doc")
}

/// Returns the deterministic current-user document-authority named pipe.
#[cfg(windows)]
pub fn document_socket(state_dir: &Path) -> PathBuf {
	windows_pipe_path(state_dir, "doc")
}

#[cfg(unix)]
fn socket_path(state_dir: &Path, kind: &str) -> PathBuf {
	let digest = Hash32::sum(state_dir.as_os_str().as_encoded_bytes());
	let short: [u8; 16] = digest.as_bytes()[..16]
		.try_into()
		.expect("a Blake3 digest contains 16 prefix bytes");
	PathBuf::from("/tmp").join(format!(
		"omp-{}-{}-{kind}.sock",
		nix::unistd::geteuid().as_raw(),
		hex::encode_n(&short)
	))
}

#[cfg(windows)]
fn windows_pipe_path(state_dir: &Path, kind: &str) -> PathBuf {
	let owner = omp_env::windows::current_user_pipe_scope()
		.expect("the process has an authenticated Windows user SID");
	let mut digest = Hash32::hasher();
	digest.update(b"omp/project-owner-pipe/v1");
	digest.update(&(owner.len() as u64).to_le_bytes());
	digest.update(owner.as_bytes());
	let state = state_dir.as_os_str().as_encoded_bytes();
	digest.update(&(state.len() as u64).to_le_bytes());
	digest.update(state);
	digest.update(&(kind.len() as u64).to_le_bytes());
	digest.update(kind.as_bytes());
	let digest = hex::encode_n(digest.finalize().as_bytes());
	PathBuf::from(format!(r"\\.\pipe\omp-{}-{kind}", &digest[..32]))
}

#[cfg(all(test, unix))]
mod tests {
	use std::path::PathBuf;

	use super::{document_socket, environment_socket};

	#[test]
	fn socket_paths_fit_the_platform_address_limit() {
		let state_dir = PathBuf::from("/").join("long-project-state-segment".repeat(32));
		let env = environment_socket(&state_dir);
		let docs = document_socket(&state_dir);
		// SAFETY: every all-zero bit pattern is valid for libc's sockaddr_un
		// integer fields and fixed-size character array.
		let address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
		let capacity = address.sun_path.len();

		assert_ne!(env, docs);
		assert!(env.as_os_str().as_encoded_bytes().len() < capacity);
		assert!(docs.as_os_str().as_encoded_bytes().len() < capacity);
	}
}

#[cfg(all(test, windows))]
mod windows_tests {
	use super::{document_socket, environment_socket};

	#[test]
	fn pipe_names_are_local_deterministic_and_domain_separated() {
		let state = std::path::Path::new(r"C:\Users\owner\AppData\Local\omp\project");
		let first = environment_socket(state);
		assert_eq!(first, environment_socket(state));
		assert_ne!(first, document_socket(state));
		assert!(first.to_string_lossy().starts_with(r"\\.\pipe\omp-"));
	}

	#[test]
	fn project_identity_changes_the_pipe_name() {
		let first = std::path::Path::new(r"C:\omp\projects\one");
		let second = std::path::Path::new(r"C:\omp\projects\two");
		assert_ne!(environment_socket(first), environment_socket(second));
		assert_ne!(document_socket(first), document_socket(second));
	}
}
