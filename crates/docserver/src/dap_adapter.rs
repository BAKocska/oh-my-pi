//! Built-in Debug Adapter Protocol declarations and deterministic selection.

use std::{
	collections::{BTreeMap, HashSet},
	path::{Path, PathBuf},
};

use omp_core::{Str, sf};
use parking_lot::RwLock;
use serde_json::{Map, Value};
use thiserror::Error;

/// How a debug adapter exchanges DAP frames with the document authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DapTransport {
	/// The adapter reads and writes DAP on standard streams.
	Stdio,
	/// The adapter listens on a TCP port substituted for `${port}`.
	Tcp {
		/// Argument token replaced with the allocated port.
		port_argument: Str,
	},
	/// The adapter listens on a Unix-domain socket substituted for `${socket}`.
	Unix {
		/// Argument token replaced with the socket path.
		socket_argument: Str,
	},
}

/// One immutable debug adapter declaration.
#[derive(Clone, Debug)]
pub struct DapAdapterSpec {
	/// Unique configured name.
	pub name: Str,
	/// Executable name or path.
	pub command: Str,
	/// Arguments before launch/attach-specific payloads.
	pub args: Vec<Str>,
	/// Byte transport used by the adapter.
	pub transport: DapTransport,
	/// Program extensions accepted without a leading dot.
	pub extensions: Vec<Str>,
	/// Project-root markers accepted by this adapter.
	pub root_markers: Vec<Str>,
	/// Whether a directory may be supplied as the launch program.
	pub accepts_directory_program: bool,
	/// Defaults merged below caller launch arguments.
	pub launch_defaults: Map<String, Value>,
	/// Defaults merged below caller attach arguments.
	pub attach_defaults: Map<String, Value>,
	/// Lower values win the deterministic preference tie-break.
	pub preference: u16,
}

impl DapAdapterSpec {
	/// Creates a validated adapter declaration.
	pub fn new(name: impl AsRef<str>, command: impl AsRef<str>) -> Result<Self, DapAdapterError> {
		let name = name.as_ref();
		let command = command.as_ref();
		if name.is_empty() || command.is_empty() {
			return Err(DapAdapterError::InvalidSpec(sf!(
				"adapter name and command must be non-empty",
			)));
		}
		Ok(Self {
			name: Str::new(name),
			command: Str::new(command),
			args: Vec::new(),
			transport: DapTransport::Stdio,
			extensions: Vec::new(),
			root_markers: Vec::new(),
			accepts_directory_program: false,
			launch_defaults: Map::new(),
			attach_defaults: Map::new(),
			preference: u16::MAX,
		})
	}

	/// Applies launch or attach defaults without replacing caller values.
	#[must_use]
	pub fn merged_arguments(
		&self,
		attach: bool,
		supplied: &Map<String, Value>,
	) -> Map<String, Value> {
		let mut merged = if attach {
			self.attach_defaults.clone()
		} else {
			self.launch_defaults.clone()
		};
		merged.extend(supplied.clone());
		merged
	}
}

/// Stable process-local identity of an installed DAP adapter.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DapAdapterId(u64);

impl DapAdapterId {
	/// Returns the registry-local integer.
	#[must_use]
	pub const fn get(self) -> u64 {
		self.0
	}
}

/// Public installed adapter row.
#[derive(Clone, Debug)]
pub struct DapAdapterInfo {
	/// Stable registry identity.
	pub id:   DapAdapterId,
	/// Installed declaration.
	pub spec: DapAdapterSpec,
}

/// Result of launch adapter selection.
#[derive(Clone, Debug)]
pub enum LaunchAdapterSelection {
	/// The selected adapter command exists.
	Available(DapAdapterInfo),
	/// Selection succeeded but the configured executable is absent.
	Unavailable {
		/// Selected adapter.
		adapter: DapAdapterInfo,
		/// Missing command.
		command: Str,
	},
	/// No configured adapter accepts the target.
	NoMatch,
}

/// Registry mutation or selection failure.
#[derive(Clone, Debug, Error)]
pub enum DapAdapterError {
	/// A declaration is incomplete or inconsistent.
	#[error("invalid DAP adapter: {0}")]
	InvalidSpec(Str),
	/// Another declaration already owns the name.
	#[error("DAP adapter {0:?} is already installed")]
	Duplicate(Str),
}

#[derive(Default)]
struct RegistryState {
	next_id: u64,
	by_name: BTreeMap<Str, DapAdapterInfo>,
}

/// Project-scoped DAP adapter registry, intentionally separate from LSP
/// bindings.
#[derive(Default)]
pub struct DapAdapterRegistry {
	state: RwLock<RegistryState>,
}

impl DapAdapterRegistry {
	/// Creates a registry populated with OMP's built-in adapters.
	pub fn with_builtins() -> Self {
		let registry = Self::default();
		for spec in builtin_adapters() {
			registry
				.install(spec)
				.expect("built-in DAP declarations are unique");
		}
		registry
	}

	/// Installs one unique named adapter.
	pub fn install(&self, spec: DapAdapterSpec) -> Result<DapAdapterId, DapAdapterError> {
		let mut state = self.state.write();
		if state.by_name.contains_key(&spec.name) {
			return Err(DapAdapterError::Duplicate(spec.name));
		}
		state.next_id = state
			.next_id
			.checked_add(1)
			.expect("DAP adapter id space exhausted");
		let id = DapAdapterId(state.next_id);
		state
			.by_name
			.insert(spec.name.clone(), DapAdapterInfo { id, spec });
		Ok(id)
	}

	/// Replaces a declaration while preserving its stable registry identity.
	pub fn replace(&self, spec: DapAdapterSpec) -> Result<DapAdapterId, DapAdapterError> {
		let mut state = self.state.write();
		if let Some(current) = state.by_name.get_mut(&spec.name) {
			current.spec = spec;
			return Ok(current.id);
		}
		drop(state);
		self.install(spec)
	}

	/// Returns installed adapters in deterministic name order.
	pub fn list(&self) -> Vec<DapAdapterInfo> {
		self.state.read().by_name.values().cloned().collect()
	}

	/// Selects a launch adapter by extension, root marker, preference, then
	/// name.
	pub fn select_launch(&self, program: &Path, project_root: &Path) -> LaunchAdapterSelection {
		let is_directory = program.is_dir();
		let extension = program
			.extension()
			.and_then(|value| value.to_str())
			.unwrap_or_default();
		let mut candidates = self
			.list()
			.into_iter()
			.filter_map(|adapter| {
				if is_directory && !adapter.spec.accepts_directory_program {
					return None;
				}
				let extension_rank = adapter
					.spec
					.extensions
					.iter()
					.any(|candidate| candidate.trim_start_matches('.') == extension);
				let marker_rank = adapter
					.spec
					.root_markers
					.iter()
					.any(|marker| project_root.join(marker.as_str()).exists());
				if !extension_rank && !marker_rank && !extension.is_empty() {
					return None;
				}
				if extension.is_empty()
					&& !marker_rank
					&& !matches!(adapter.spec.name.as_str(), "gdb" | "lldb-dap")
				{
					return None;
				}
				Some((
					!extension_rank,
					!marker_rank,
					adapter.spec.preference,
					adapter.spec.name.clone(),
					adapter,
				))
			})
			.collect::<Vec<_>>();
		candidates.sort_by(|left, right| {
			left
				.0
				.cmp(&right.0)
				.then(left.1.cmp(&right.1))
				.then(left.2.cmp(&right.2))
				.then(left.3.cmp(&right.3))
		});
		let Some((_, _, _, _, adapter)) = candidates.into_iter().next() else {
			return LaunchAdapterSelection::NoMatch;
		};
		if command_available(adapter.spec.command.as_str()) {
			LaunchAdapterSelection::Available(adapter)
		} else {
			LaunchAdapterSelection::Unavailable { command: adapter.spec.command.clone(), adapter }
		}
	}

	/// Selects attach by explicit name, port-capable adapter, or preference.
	pub fn select_attach(
		&self,
		preferred: Option<&str>,
		port: Option<u16>,
	) -> Option<DapAdapterInfo> {
		let mut adapters = self.list();
		if let Some(preferred) = preferred {
			return adapters
				.into_iter()
				.find(|adapter| adapter.spec.name.as_str() == preferred);
		}
		if port.is_some() {
			adapters.retain(|adapter| matches!(adapter.spec.transport, DapTransport::Tcp { .. }));
		}
		adapters.sort_by_key(|adapter| (adapter.spec.preference, adapter.spec.name.clone()));
		adapters.into_iter().next()
	}
}

fn command_available(command: &str) -> bool {
	let path = Path::new(command);
	if path.components().count() > 1 {
		return path.is_file();
	}
	std::env::var_os("PATH").is_some_and(|paths| {
		std::env::split_paths(&paths).any(|directory| {
			executable_candidates(&directory, command)
				.iter()
				.any(|path| path.is_file())
		})
	})
}

#[cfg(windows)]
fn executable_candidates(directory: &Path, command: &str) -> Vec<PathBuf> {
	let mut candidates = vec![directory.join(command)];
	if Path::new(command).extension().is_none() {
		for extension in std::env::var_os("PATHEXT")
			.unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into())
			.to_string_lossy()
			.split(';')
		{
			candidates.push(directory.join(format!("{command}{extension}")));
		}
	}
	candidates
}

#[cfg(not(windows))]
fn executable_candidates(directory: &Path, command: &str) -> Vec<PathBuf> {
	vec![directory.join(command)]
}

fn builtin_adapters() -> Vec<DapAdapterSpec> {
	let declarations: &[(&str, &str, &[&str], &[&str])] = &[
		("gdb", "gdb", &["-i", "dap"], &["c", "cc", "cpp", "cxx"]),
		("lldb-dap", "lldb-dap", &[], &["c", "cc", "cpp", "cxx", "m", "mm", "swift"]),
		("codelldb", "codelldb", &["--port", "${port}"], &["rs", "c", "cc", "cpp"]),
		("debugpy", "python", &["-m", "debugpy.adapter"], &["py", "pyw"]),
		("dlv", "dlv", &["dap", "--listen=127.0.0.1:${port}"], &["go"]),
		("js-debug", "js-debug-adapter", &["${port}"], &["js", "jsx", "mjs", "cjs", "ts", "tsx"]),
		("netcoredbg", "netcoredbg", &["--interpreter=vscode"], &["cs", "dll"]),
		("kotlin", "kotlin-debug-adapter", &[], &["kt", "kts"]),
		("rdbg", "rdbg", &["--open", "--command", "--"], &["rb"]),
		("php", "php-debug-adapter", &[], &["php"]),
		("bash", "bash-debug-adapter", &[], &["sh", "bash"]),
		("dart", "dart-debug-adapter", &[], &["dart"]),
		("flutter", "flutter-debug-adapter", &[], &["dart"]),
		("elixir", "elixir-ls-debugger", &[], &["ex", "exs"]),
	];
	let tcp = HashSet::from(["codelldb", "dlv", "js-debug"]);
	declarations
		.iter()
		.enumerate()
		.map(|(preference, (name, command, args, extensions))| {
			let mut spec = DapAdapterSpec::new(name, command).expect("static adapter declaration");
			spec.args = args.iter().copied().map(Str::new_static).collect();
			spec.extensions = extensions.iter().copied().map(Str::new_static).collect();
			spec.preference = u16::try_from(preference).expect("small built-in adapter set");
			if tcp.contains(*name) {
				spec.transport = DapTransport::Tcp { port_argument: Str::new_static("${port}") };
			}
			if *name == "dlv" {
				spec.accepts_directory_program = true;
				spec.root_markers = vec![sf!("go.mod"), sf!("go.work")];
			}
			if *name == "debugpy" {
				spec.root_markers = vec![sf!("pyproject.toml")];
			}
			spec
		})
		.collect()
}

#[cfg(test)]
mod tests {
	use std::fs;

	use super::*;

	#[test]
	fn extensionless_order_is_gdb_then_lldb() {
		let registry = DapAdapterRegistry::with_builtins();
		let root = tempfile::tempdir().unwrap();
		let selection = registry.select_launch(&root.path().join("program"), root.path());
		match selection {
			LaunchAdapterSelection::Available(adapter)
			| LaunchAdapterSelection::Unavailable { adapter, .. } => assert_eq!(adapter.spec.name, "gdb"),
			LaunchAdapterSelection::NoMatch => panic!("extensionless debugger"),
		}
	}

	#[test]
	fn directory_launch_restricts_to_capable_adapter() {
		let registry = DapAdapterRegistry::with_builtins();
		let root = tempfile::tempdir().unwrap();
		fs::write(root.path().join("go.mod"), b"module example").unwrap();
		let selection = registry.select_launch(root.path(), root.path());
		match selection {
			LaunchAdapterSelection::Available(adapter)
			| LaunchAdapterSelection::Unavailable { adapter, .. } => assert_eq!(adapter.spec.name, "dlv"),
			LaunchAdapterSelection::NoMatch => panic!("directory debugger"),
		}
	}
}
