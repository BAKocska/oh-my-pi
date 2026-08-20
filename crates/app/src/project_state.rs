//! Project-scoped runtime state kept outside tool-writable workspaces.

use std::{
	io,
	path::{Path, PathBuf},
};

use omp_core::encoding::hex;

/// Resolves the durable state directory for a project beneath the application
/// data directory.
///
/// Canonicalizing the project root gives aliases and symlinked paths one stable
/// state identity.
pub fn directory(data_dir: &Path, project_root: &Path) -> io::Result<PathBuf> {
	let root = std::fs::canonicalize(project_root)?;
	let digest = blake3::hash(root.as_os_str().as_encoded_bytes());
	Ok(data_dir
		.join("projects")
		.join(hex::encode_n(digest.as_bytes()).as_str()))
}

/// Returns the short owner-local environment socket path for `state_dir`.
///
/// The path is keyed by the running executable's build identity: a rebuilt
/// `omp` binds its own listener immediately while stale-build listeners drain
/// and idle-exit, with no takeover protocol. The document socket stays
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
/// The build key lets rebuilt owners bind immediately while older listeners
/// drain independently.
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
	let digest = blake3::hash(state_dir.as_os_str().as_encoded_bytes());
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
	let mut digest = blake3::Hasher::new();
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
