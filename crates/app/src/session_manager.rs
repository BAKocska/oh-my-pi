//! Owner-local session UI state that is deliberately excluded from journals and
//! replication.

use std::{
	collections::BTreeSet,
	io,
	path::{Path, PathBuf},
};

use omp_core::{Str, encoding::hex};
use omp_storage::{atomic, transcript::SessionId};
use thiserror::Error;

/// Draft persistence failure.
#[derive(Debug, Error)]
pub enum DraftError {
	/// Draft directory or file access failed.
	#[error("session draft I/O failed")]
	Io(#[from] io::Error),
	/// Atomic draft publication failed.
	#[error("failed to publish session draft")]
	Atomic(#[from] atomic::Error),
}
/// Durable owner-local session pin persistence failure.
#[derive(Debug, Error)]
pub enum PinError {
	/// Pin file access failed.
	#[error("session pin I/O failed")]
	Io(#[from] io::Error),
	/// Pin metadata encoding failed.
	#[error("failed to encode session pin metadata")]
	Json(#[from] serde_json::Error),
	/// Atomic pin publication failed.
	#[error("failed to publish session pins")]
	Atomic(#[from] atomic::Error),
}

/// Project-local pinned session identities stored beside the session journals.
pub struct PinStore {
	path: PathBuf,
}

impl PinStore {
	/// Opens the pin file belonging to `sessions_dir`.
	pub fn new(sessions_dir: &Path) -> Self {
		Self { path: sessions_dir.join("session-pins.json") }
	}

	/// Loads the complete deterministic pin set.
	///
	/// Missing or corrupt metadata degrades to an empty set so a stale UI file
	/// cannot prevent session discovery.
	pub fn load(&self) -> Result<BTreeSet<Str>, PinError> {
		let bytes = match std::fs::read(&self.path) {
			Ok(bytes) => bytes,
			Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
			Err(error) => return Err(error.into()),
		};
		let pins: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap_or_else(|error| {
			tracing::warn!(
				path = %self.path.display(),
				%error,
				"ignoring corrupt session pin metadata"
			);
			Vec::new()
		});
		Ok(pins.into_iter().filter_map(|pin| match pin {
			serde_json::Value::String(id) => Some(Str::from(id)),
			_ => None,
		})
		.collect())
	}

	/// Toggles one session and atomically persists the complete set.
	///
	/// Returns `true` when the session is pinned after the mutation.
	pub fn toggle(&self, session: &SessionId) -> Result<bool, PinError> {
		let mut pins = self.load()?;
		let pinned = if pins.remove(session.0.as_str()) {
			false
		} else {
			pins.insert(session.0.clone());
			true
		};
		let bytes = serde_json::to_vec_pretty(&pins)?;
		atomic::commit(&self.path, &bytes, || true)?;
		Ok(pinned)
	}
}

/// Private, owner-local unsent composer buffers keyed by session identity.
pub struct DraftStore {
	directory: PathBuf,
}

impl DraftStore {
	/// Opens the private draft directory below the owner's application data
	/// root.
	pub fn new(data_dir: &Path) -> Result<Self, DraftError> {
		let directory = data_dir.join("drafts");
		std::fs::create_dir_all(&directory)?;
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt as _;
			std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
		}
		Ok(Self { directory })
	}

	fn path(&self, session: &SessionId) -> PathBuf {
		let digest = omp_core::Hash32::sum(session.0.as_bytes());
		let short: &[u8; 16] = digest.as_bytes()[..16]
			.try_into()
			.expect("a Blake3 digest contains 16 prefix bytes");
		self.directory.join(hex::encode_n(short).as_str())
	}

	/// Atomically saves the current unsent composer text, or removes an empty
	/// draft.
	pub fn save(&self, session: &SessionId, draft: &str) -> Result<(), DraftError> {
		let path = self.path(session);
		if draft.is_empty() {
			match std::fs::remove_file(path) {
				Ok(()) => {},
				Err(error) if error.kind() == io::ErrorKind::NotFound => {},
				Err(error) => return Err(error.into()),
			}
			return Ok(());
		}
		atomic::commit(&path, draft.as_bytes(), || true)?;
		Ok(())
	}

	/// Takes a saved draft exactly once after restart or session switch.
	pub fn consume(&self, session: &SessionId) -> Result<Option<String>, DraftError> {
		let path = self.path(session);
		let claimed = path.with_extension(format!("claimed-{}", omp_core::Ulid::generate()));
		match std::fs::rename(&path, &claimed) {
			Ok(()) => {},
			Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
			Err(error) => return Err(error.into()),
		}
		let result = std::fs::read_to_string(&claimed);
		let removal = std::fs::remove_file(&claimed);
		match (result, removal) {
			(Ok(draft), Ok(())) => Ok(Some(draft)),
			(Err(error), _) | (Ok(_), Err(error)) => Err(error.into()),
		}
	}
}

#[cfg(test)]
mod tests {
	use omp_core::Str;
	use tempfile::tempdir;

	use super::*;

	#[test]
	fn draft_is_private_and_consumed_once() {
		let temp = tempdir().expect("tempdir");
		let store = DraftStore::new(temp.path()).expect("draft store");
		let session = SessionId(Str::from("session-one"));
		store
			.save(&session, "unfinished prompt")
			.expect("save draft");
		assert_eq!(store.consume(&session).expect("consume"), Some("unfinished prompt".to_owned()));
		assert_eq!(store.consume(&session).expect("consume again"), None);
	}
	#[test]
	fn pins_toggle_and_persist_across_reopen() {
		let temp = tempdir().expect("tempdir");
		let first = SessionId(Str::from("session-one"));
		let second = SessionId(Str::from("session-two"));
		let store = PinStore::new(temp.path());

		assert!(store.toggle(&first).expect("pin first"));
		assert!(store.toggle(&second).expect("pin second"));
		let reopened = PinStore::new(temp.path());
		assert_eq!(
			reopened.load().expect("reload pins"),
			BTreeSet::from([first.0.clone(), second.0.clone()])
		);
		assert!(!reopened.toggle(&first).expect("unpin first"));
		assert_eq!(reopened.load().expect("reload unpin"), BTreeSet::from([second.0]));
	}
	#[test]
	fn corrupt_pin_metadata_does_not_break_session_listing() {
		let temp = tempdir().expect("tempdir");
		std::fs::write(temp.path().join("session-pins.json"), b"{broken")
			.expect("corrupt fixture");
		assert!(PinStore::new(temp.path()).load().expect("recover pins").is_empty());
	}
}
