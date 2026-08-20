//! Extension-host child spawning over a dedicated CONTROL descriptor.

use std::{
	io,
	path::PathBuf,
	process::Stdio,
	sync::{
		Arc,
		atomic::{AtomicUsize, Ordering},
	},
};

use pyo3::{
	prelude::*,
	types::{PyList, PyModule},
};
use thiserror::Error;
use tokio::{
	io::AsyncReadExt,
	net::UnixStream,
	process::{Child, Command},
};

use crate::envd::worker::HostKey;

/// Hidden argv selector for one extension-host child.
pub const EXT_HOST_ARG: &str = "__omp-ext-host";
/// Environment variable carrying the inherited CONTROL descriptor number.
pub const CONTROL_FD_ENV: &str = "OMP_EXT_CONTROL_FD";
/// Environment variable carrying the extension-scoped DATA socket path.
pub const ENV_SOCKET_ENV: &str = "OMP_EXT_ENV_SOCKET";
/// Environment variable carrying the extension-private Python site tree.
pub const PY_SITE_ENV: &str = "OMP_PY_SITE";
/// Environment variable carrying the verified package snapshot JSON.
pub const PACKAGE_SNAPSHOT_ENV: &str = "OMP_EXT_PACKAGE_SNAPSHOT";

/// One captured child output fragment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostLog {
	/// Stream which emitted the fragment.
	pub stream: HostLogStream,
	/// Raw output bytes; framing is intentionally not interpreted as CONTROL.
	pub bytes:  Vec<u8>,
}

/// Captured output source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostLogStream {
	/// Child standard output.
	Stdout,
	/// Child standard error.
	Stderr,
}

/// Spawn inputs authenticated before a child is reached.
#[derive(Clone, Debug)]
pub struct SpawnSpec {
	/// Isolated host identity `(layer, tier, unit)`; the existing `HostKey`
	/// calls the unit `extension`.
	pub key:                HostKey,
	/// Same-binary executable to re-enter.
	pub executable:         PathBuf,
	/// Per-extension Python site tree.
	pub python_site:        PathBuf,
	/// Scoped Environment DATA socket.
	pub env_socket:         PathBuf,
	/// Generation assigned to this newly spawned child.
	pub host_generation:    u64,
	/// Session generation shared with the CONTROL parent.
	pub session_generation: u64,
	/// Verified package ownership snapshot encoded for the Python bootstrap.
	///
	/// `None` identifies an anonymous or development extension and installs an
	/// explicitly empty package snapshot in the child.
	pub package_snapshot:   Option<omp_core::Str>,
}

/// Owned parent ends for an extension-host child.
pub struct SpawnedHost {
	/// Authenticated host identity.
	pub key:     HostKey,
	/// Supervised child process group leader.
	pub child:   Child,
	/// Dedicated bidirectional CONTROL transport, never stdio.
	pub control: UnixStream,
	/// Captured stdout/stderr records.
	pub logs:    flume::Receiver<HostLog>,
}

/// Host-child bound and spawn failures.
#[derive(Debug, Error)]
pub enum SpawnError {
	/// The session already reached its admitted child bound.
	#[error("omp.MAX_HOST_CHILDREN ({limit}) is exhausted")]
	ChildLimit {
		/// Configured session bound.
		limit: usize,
	},
	/// Creating or configuring the CONTROL socket failed.
	#[error("CONTROL descriptor setup failed: {0}")]
	Control(#[from] io::Error),
	/// The embedded Python extension-host runtime failed to boot.
	#[error("extension host Python runtime failed: {0}")]
	Python(String),
	/// The child process could not be spawned.
	#[error("extension host spawn failed: {0}")]
	Spawn(io::Error),
}

/// Session-local lazy child admission bound.
#[derive(Clone, Debug)]
pub struct HostChildLimit {
	limit: usize,
	live:  Arc<AtomicUsize>,
}

impl HostChildLimit {
	/// Creates a lazy-spawn admission bound.
	#[must_use]
	pub fn new(limit: usize) -> Self {
		Self { limit, live: Arc::new(AtomicUsize::new(0)) }
	}

	/// Starts a child only after its declared surface is reached.
	///
	/// The returned permit is released when [`Self::release`] is called after
	/// the process is reaped.
	pub async fn spawn_on_reach(&self, spec: SpawnSpec) -> Result<SpawnedHost, SpawnError> {
		let previous = self.live.fetch_add(1, Ordering::AcqRel);
		if previous >= self.limit {
			self.live.fetch_sub(1, Ordering::AcqRel);
			return Err(SpawnError::ChildLimit { limit: self.limit });
		}
		match spawn(spec).await {
			Ok(host) => Ok(host),
			Err(error) => {
				self.live.fetch_sub(1, Ordering::AcqRel);
				Err(error)
			},
		}
	}

	/// Releases one reaped child slot.
	pub fn release(&self) {
		self.live.fetch_sub(1, Ordering::AcqRel);
	}
}

/// Spawns one isolated extension host with CONTROL on descriptor three.
pub async fn spawn(spec: SpawnSpec) -> Result<SpawnedHost, SpawnError> {
	let (parent, child_control) = UnixStream::pair()?;
	let fd = std::os::fd::AsRawFd::as_raw_fd(&child_control);
	let mut command = Command::new(&spec.executable);
	command
		.arg(EXT_HOST_ARG)
		.env(CONTROL_FD_ENV, "3")
		.env(PY_SITE_ENV, &spec.python_site)
		.env(ENV_SOCKET_ENV, &spec.env_socket)
		.env("OMP_EXT_LAYER", spec.key.layer().as_str())
		.env("OMP_EXT_TIER", spec.key.tier().as_str())
		.env("OMP_EXT_HOST_GENERATION", spec.host_generation.to_string())
		.env("OMP_EXT_SESSION_GENERATION", spec.session_generation.to_string())
		.stdin(Stdio::null())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped());
	if let Some(snapshot) = &spec.package_snapshot {
		command.env(PACKAGE_SNAPSHOT_ENV, snapshot.as_str());
	} else {
		command.env_remove(PACKAGE_SNAPSHOT_ENV);
	}
	#[cfg(unix)]
	{
		// The child owns a fresh process group. Its CONTROL peer is duplicated
		// onto a stable descriptor; stdio remains ordinary captured logging.
		unsafe {
			command.pre_exec(move || {
				if nix::libc::setpgid(0, 0) == -1 {
					return Err(io::Error::last_os_error());
				}
				if nix::libc::dup2(fd, 3) == -1 {
					return Err(io::Error::last_os_error());
				}
				let flags = nix::libc::fcntl(3, nix::libc::F_GETFD);
				if flags == -1
					|| nix::libc::fcntl(3, nix::libc::F_SETFD, flags & !nix::libc::FD_CLOEXEC) == -1
				{
					return Err(io::Error::last_os_error());
				}
				Ok(())
			});
		}
	}
	let mut child = command.spawn().map_err(SpawnError::Spawn)?;
	drop(child_control);
	let (logs_tx, logs) = flume::unbounded();
	if let Some(stdout) = child.stdout.take() {
		capture(stdout, HostLogStream::Stdout, logs_tx.clone());
	}
	if let Some(stderr) = child.stderr.take() {
		capture(stderr, HostLogStream::Stderr, logs_tx);
	}
	Ok(SpawnedHost { key: spec.key, child, control: parent, logs })
}

fn capture<R>(stream: R, source: HostLogStream, logs: flume::Sender<HostLog>)
where
	R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
	tokio::spawn(async move {
		let mut stream = stream;
		let mut bytes = [0_u8; 4096];
		loop {
			let Ok(read) = stream.read(&mut bytes).await else {
				return;
			};
			if read == 0 {
				return;
			}
			if logs
				.send_async(HostLog { stream: source, bytes: bytes[..read].to_vec() })
				.await
				.is_err()
			{
				return;
			}
		}
	});
}

/// Runs the hidden extension-host child entry.
///
/// The Python runtime owns the protocol loop after this function establishes
/// that CONTROL is an inherited descriptor rather than standard input.
pub fn run_ext_host_entry() -> Result<(), SpawnError> {
	let fd = std::env::var(CONTROL_FD_ENV)
		.ok()
		.and_then(|value| value.parse::<i32>().ok())
		.filter(|fd| *fd >= 0)
		.ok_or_else(|| {
			SpawnError::Control(io::Error::new(
				io::ErrorKind::InvalidInput,
				"missing OMP_EXT_CONTROL_FD",
			))
		})?;
	#[cfg(unix)]
	unsafe {
		if nix::libc::fcntl(fd, nix::libc::F_GETFD) == -1 {
			return Err(SpawnError::Control(io::Error::last_os_error()));
		}
	}
	let engine = omp_py::Engine::builder()
		.init()
		.map_err(|error| SpawnError::Python(error.to_string()))?;
	install_package_snapshot(&engine)?;
	omp_py::bootstrap_extension_host(&engine).map_err(|error| SpawnError::Python(error.to_string()))
}

/// Installs the private site tree and parent-verified snapshot before any
/// extension module imports.
fn install_package_snapshot(engine: &omp_py::Engine) -> Result<(), SpawnError> {
	let snapshot = std::env::var(PACKAGE_SNAPSHOT_ENV).unwrap_or_else(|_| {
		String::from(r#"{"distributions":[],"modules":{},"own":null,"tree":null}"#)
	});
	engine
		.attach(|py| -> PyResult<()> {
			if let Ok(site) = std::env::var(PY_SITE_ENV) {
				let sys = PyModule::import(py, "sys")?;
				let value = sys.getattr("path")?;
				let path = value.cast::<PyList>()?;
				path.insert(0, site)?;
			}
			let packages = PyModule::import(py, "omp.packages")?;
			packages.call_method1("_install_snapshot_json", (snapshot,))?;
			Ok(())
		})
		.map_err(|error| SpawnError::Python(error.to_string()))
}
