//! Production memory runtime composition from settings and Environment
//! repository facts.

use std::{
	path::{Path, PathBuf},
	sync::Arc,
};

use omp_core::Str;
use omp_memory::{MemoryRuntime, RuntimeRegistry, runtime::RuntimeStart, session::SessionMemory};

use crate::{envd::vcs::RepositorySnapshot, settings::Settings};

/// Registered top-level memory runtime. Dropping it removes only the
/// contextless URL lookup; existing parent/subagent handles keep their shared
/// banks alive.
pub struct RegisteredMemoryRuntime {
	session_id: Str,
	runtime:    Arc<MemoryRuntime>,
}

impl RegisteredMemoryRuntime {
	/// Borrows the live Off/Mnemopi runtime.
	#[must_use]
	pub const fn runtime(&self) -> &Arc<MemoryRuntime> {
		&self.runtime
	}

	/// Creates the top-level lifecycle handle shared with subagents.
	#[must_use]
	pub fn session(&self) -> SessionMemory {
		SessionMemory::top_level(Arc::clone(&self.runtime))
	}
}

impl Drop for RegisteredMemoryRuntime {
	fn drop(&mut self) {
		RuntimeRegistry::unregister(self.session_id.as_str());
	}
}

/// Constructs and registers one runtime from native settings and the
/// Environment's immutable VCS snapshot. Memory never probes Git:
/// `snapshot.primary_root` is the sole project-bank identity,
/// with the canonical workspace root used only when the snapshot says no
/// repository exists. `None` is accepted only for the effect-free Off backend.
pub fn start(
	settings: &Settings,
	data_dir: &Path,
	session_id: impl Into<Str>,
	workspace_root: impl Into<PathBuf>,
	snapshot: Option<&RepositorySnapshot>,
) -> omp_memory::Result<RegisteredMemoryRuntime> {
	let session_id = session_id.into();
	let runtime = MemoryRuntime::start(RuntimeStart {
		session_id:             session_id.clone(),
		data_dir:               data_dir.join("memory"),
		workspace_root:         workspace_root.into(),
		canonical_primary_root: snapshot.and_then(|snapshot| snapshot.primary_root.clone()),
		backend:                settings.memory.backend,
		mnemopi:                settings.mnemopi.clone(),
	})?;
	RuntimeRegistry::register(session_id.clone(), &runtime);
	Ok(RegisteredMemoryRuntime { session_id, runtime })
}
