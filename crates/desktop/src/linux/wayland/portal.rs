use std::{
	fs,
	path::{Path, PathBuf},
};

/// File name of the `RemoteDesktop` restore token that pre-#7884 builds wrote
/// (world-readable) during read-only `computer` calls. Nothing reads it after
/// #7884 dropped the restore-token path.
const ORPHANED_REMOTE_DESKTOP_TOKEN: &str = "remote-desktop-token";

/// Resolves the `omp` state directory (`$XDG_STATE_HOME/omp` or
/// `~/.local/state/omp`) that holds portal tokens.
fn omp_state_dir() -> Option<PathBuf> {
	let base = std::env::var_os("XDG_STATE_HOME")
		.map(PathBuf::from)
		.or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))?;
	Some(base.join("omp"))
}

fn remove_token_in(dir: &Path) {
	let _ = fs::remove_file(dir.join(ORPHANED_REMOTE_DESKTOP_TOKEN));
}

/// Best-effort removal of the orphaned `RemoteDesktop` restore token left
/// behind by pre-#7884 builds. Runs on Wayland backend construction; a missing
/// file is success, so it is safe to call on every session.
pub(super) fn remove_orphaned_remote_desktop_token() {
	if let Some(dir) = omp_state_dir() {
		remove_token_in(&dir);
	}
}

#[cfg(feature = "wayland-pipewire")]
fn token_path(name: &str) -> Option<PathBuf> {
	Some(omp_state_dir()?.join(name))
}

#[cfg(feature = "wayland-pipewire")]
pub(super) fn read_token(name: &str) -> Option<String> {
	fs::read_to_string(token_path(name)?)
		.ok()
		.map(|token| token.trim().to_string())
		.filter(|token| !token.is_empty())
}

#[cfg(feature = "wayland-pipewire")]
pub(super) fn store_token(name: &str, token: Option<&str>) {
	let (Some(path), Some(token)) = (token_path(name), token) else {
		return;
	};
	let Some(parent) = path.parent() else {
		return;
	};
	if fs::create_dir_all(parent).is_ok() {
		let _ = fs::write(path, token);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The orphaned RemoteDesktop token written by pre-#7884 builds must be
	/// removed, and a second removal on the now-missing file must stay a no-op.
	#[test]
	fn removes_orphaned_remote_desktop_token() {
		let dir = std::env::temp_dir().join(format!("omp-token-test-{}", std::process::id()));
		fs::create_dir_all(&dir).expect("create token test dir");
		let token = dir.join(ORPHANED_REMOTE_DESKTOP_TOKEN);
		fs::write(&token, "cafef00d").expect("plant orphaned token");
		remove_token_in(&dir);
		assert!(!token.exists(), "orphaned token must be removed");
		remove_token_in(&dir);
		let _ = fs::remove_dir_all(&dir);
	}
}
