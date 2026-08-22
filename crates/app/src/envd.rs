//! Project environment-daemon assembly and production serving.

mod admission;
pub mod blobs;
pub mod browser_fetch;
mod computer;
mod direnv;
pub mod docs;
pub mod document_cache;
pub(crate) mod eval;
pub mod exec;
pub(crate) mod exec_settings;
mod github;
pub(crate) mod github_url;
pub(crate) mod host_info;
mod http_egress;
mod journal_runtime;
pub(crate) mod lsp_settings;
mod managed_skills;
pub(crate) mod mcp;
mod media_devices;
pub mod policy;
pub mod process_identity;
pub mod process_log;
pub mod process_store;
pub mod recovery;
mod resource_materializer;
mod search_backend;
mod server;
pub mod shell_profile;
pub(crate) mod site;
pub(crate) mod ssh;
mod staged_preview;
mod tool_debug;
mod tool_document;
mod tool_lsp;
mod tool_read_sources;
mod tool_search;
pub(crate) mod tool_settings;
pub(crate) mod tool_shell;
pub(crate) mod tool_url;
mod tools;
mod vault;
pub mod vcs;
#[cfg(windows)]
pub mod windows;
pub mod worker;
pub mod worker_pool;
pub mod workspace;
pub(crate) mod workspace_roots;
use std::{io, path::Path, sync::Arc};

#[doc(hidden)]
pub use eval::{EVAL_CHILD_ARG, run_eval_child_entry};
use miette::IntoDiagnostic as _;
use omp_core::{Hash32, Str, sf};
use omp_env::EnvClient;
use omp_proto::env::v1::{ClientHello, ServerHello};
use omp_tool::Registry;
pub use server::{EnvServer, EnvdError};
use tokio_util::sync::CancellationToken;
#[doc(hidden)]
pub use worker::run_py_worker_entry;

use self::{
	server::ExtensionDataBinding,
	worker::{ExtHostConfig, ExtHostSpec, HostKey, PY_EVAL_MODULE},
};
use crate::cli::EnvdArgs;

/// Copies session-local artifacts into the replacement session root.
pub(crate) fn migrate_session_artifacts(
	sessions_dir: &Path,
	source_session: &str,
	destination_session: &str,
) -> Result<(), std::io::Error> {
	tool_url::local::migrate_session_artifacts(sessions_dir, source_session, destination_session)
}

/// Starts the project environment daemon and serves until process shutdown.
#[cfg(unix)]
pub async fn run(args: EnvdArgs) -> miette::Result<()> {
	server::run(args).await.into_diagnostic()
}

/// Starts the Windows named-pipe project environment daemon.
#[cfg(windows)]
pub async fn run(args: EnvdArgs) -> miette::Result<()> {
	windows::run(args).await.into_diagnostic()
}

/// Reports that no owner-local environment transport exists on this target.
#[cfg(not(any(unix, windows)))]
pub async fn run(args: EnvdArgs) -> miette::Result<()> {
	server::run(args).await.into_diagnostic()
}

/// Client-side ownership of one project environment composition.
///
/// Dropping this value shuts down only servers and children started by this
/// composition. An existing owner environment remains untouched.
pub(crate) struct ProjectEnvironment {
	pub(crate) client:   EnvClient,
	pub(crate) registry: Arc<Registry>,
	eval_bridge:         Arc<eval::SessionBridgeHost>,
	reflection_bridge:   Arc<crate::memory::ReflectionBridgeHost>,
	eval_control:        omp_tools::eval::EvalSessionControl,
	search_bridge:       Arc<search_backend::SearchBridgeHost>,
	github_credentials:  Arc<github_url::GithubCredentialBridge>,
	goal_control:        tools::AgentGoalControl,
	lifecycle:           ProjectLifecycle,
}

struct ProjectLifecycle {
	shutdown: Option<CancellationToken>,
	tasks:    Vec<tokio::task::JoinHandle<()>>,
	server:   Arc<EnvServer>,
}

impl Drop for ProjectLifecycle {
	fn drop(&mut self) {
		if let Some(shutdown) = &self.shutdown {
			shutdown.cancel();
		}
		for task in &self.tasks {
			task.abort();
		}
	}
}

impl ProjectEnvironment {
	/// Connects an existing owner environment or starts one for this process.
	#[cfg(unix)]
	pub(crate) async fn connect_or_start(
		root: &Path,
		state_dir: &Path,
		socket: &Path,
		docserver_socket: &Path,
		py_eval: bool,
		interrupt_grace: omp_core::Duration,
	) -> Result<Self, EnvdError> {
		match EnvServer::connect_owner_uds(socket).await {
			Ok((owner_probe, bridge)) => {
				match hello(&owner_probe).await {
					Ok(owner_hello)
						if crate::build_id::is_stale(
							crate::build_id::current(),
							&owner_hello.server_build,
						) =>
					{
						// Stale-build owners can only appear on explicitly
						// configured socket paths; the automatic path is keyed
						// by executable generation. Ask the owner to retire, then wait
						// briefly for the endpoint to be released.
						let _ = owner_probe.retire().await;
						bridge.abort();
						let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
						loop {
							match tokio::net::UnixStream::connect(socket).await {
								Err(error)
									if matches!(
										error.kind(),
										io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
									) =>
								{
									return Self::start(
										root,
										state_dir,
										socket,
										docserver_socket,
										py_eval,
										interrupt_grace,
									)
									.await;
								},
								_ if tokio::time::Instant::now() >= deadline => break,
								_ => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
							}
						}
						tracing::warn!(
							socket = %socket.display(),
							"stale-build environment daemon kept its socket; using an in-process environment"
						);
					},
					Ok(_) => bridge.abort(),
					Err(EnvdError::Client(omp_env::ClientError::Protocol(error))) => {
						// Owners from before the current schema revision reject
						// the hello outright; their endpoint drains with its
						// owner while this process stays in-process.
						bridge.abort();
						tracing::warn!(
							socket = %socket.display(),
							code = error.code,
							message = %error.message,
							"environment owner rejected the handshake; using an in-process environment"
						);
					},
					Err(error) => return Err(error),
				}
				Self::connect_peer(root, state_dir, docserver_socket, py_eval, interrupt_grace).await
			},
			Err(EnvdError::Io(error))
				if matches!(
					error.kind(),
					io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
				) =>
			{
				// No owner: autostart a detached project daemon so the shared
				// authorities outlive this process, then join it as a peer.
				match spawn_project_daemon(root, state_dir, socket, docserver_socket).await {
					Ok(()) => {
						Self::connect_peer(root, state_dir, docserver_socket, py_eval, interrupt_grace)
							.await
					},
					Err(error) => {
						tracing::warn!(
							socket = %socket.display(),
							%error,
							"could not autostart the project daemon; running an embedded environment"
						);
						Self::start(root, state_dir, socket, docserver_socket, py_eval, interrupt_grace)
							.await
					},
				}
			},
			Err(error) => Err(error),
		}
	}

	/// Connects to or starts the owner-scoped Windows project environment.
	#[cfg(windows)]
	pub(crate) async fn connect_or_start(
		root: &Path,
		state_dir: &Path,
		socket: &Path,
		docserver_socket: &Path,
		py_eval: bool,
		interrupt_grace: omp_core::Duration,
	) -> Result<Self, EnvdError> {
		match omp_env::windows::connect_owner_pipe(socket) {
			Ok((owner_probe, bridge)) => {
				match hello(&owner_probe).await {
					Ok(owner_hello)
						if crate::build_id::is_stale(
							crate::build_id::current(),
							&owner_hello.server_build,
						) =>
					{
						let _ = owner_probe.retire().await;
						bridge.abort();
						let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
						loop {
							match omp_env::windows::open_owner_pipe(socket) {
								Err(error) if error.kind() == io::ErrorKind::NotFound => {
									return Self::start(
										root,
										state_dir,
										socket,
										docserver_socket,
										py_eval,
										interrupt_grace,
									)
									.await;
								},
								_ if tokio::time::Instant::now() >= deadline => break,
								_ => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
							}
						}
					},
					Ok(_) => bridge.abort(),
					Err(omp_env::ClientError::Protocol(error)) => {
						bridge.abort();
						tracing::warn!(
							socket = %socket.display(),
							code = error.code,
							message = %error.message,
							"environment owner rejected the handshake; joining document authority"
						);
					},
					Err(error) => return Err(EnvdError::Client(error)),
				}
				Self::connect_peer(root, state_dir, docserver_socket, py_eval, interrupt_grace).await
			},
			Err(error)
				if matches!(
					error.kind(),
					io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
				) =>
			{
				match spawn_project_daemon(root, state_dir, socket, docserver_socket).await {
					Ok(()) => {
						Self::connect_peer(root, state_dir, docserver_socket, py_eval, interrupt_grace)
							.await
					},
					Err(error) => {
						tracing::warn!(
							socket = %socket.display(),
							%error,
							"could not autostart the project daemon; running an embedded environment"
						);
						Self::start(root, state_dir, socket, docserver_socket, py_eval, interrupt_grace)
							.await
					},
				}
			},
			Err(error) => Err(error.into()),
		}
	}

	/// Joins the project as a peer of an already-running owner environment.
	///
	/// The composition serves tools in-process and holds only client
	/// connections to shared authorities, so dropping it never affects other
	/// connected apps.
	async fn connect_peer(
		root: &Path,
		state_dir: &Path,
		docserver_socket: &Path,
		py_eval: bool,
		interrupt_grace: omp_core::Duration,
	) -> Result<Self, EnvdError> {
		let (worker_config, data_bindings) = worker_config(state_dir, py_eval, interrupt_grace)?;
		let server = EnvServer::open_project(
			root,
			state_dir,
			docserver_socket,
			Registry::new(),
			worker_config,
			None,
		)
		.await?;
		let server = Arc::new(server);
		let registry = server.registry();
		let eval_bridge = server.eval_bridge();
		let reflection_bridge = server.reflection_bridge();
		let eval_control = server.eval_control();
		let search_bridge = server.search_bridge();
		let github_credentials = server.github_credentials();
		let goal_control = server.goal_control();
		let (client, transport) = EnvClient::in_process(64);
		let in_process_server = Arc::clone(&server);
		let in_process =
			tokio::spawn(async move { in_process_server.serve_in_process(transport).await });
		let shutdown = CancellationToken::new();
		let mut tasks = vec![in_process];
		spawn_extension_data_servers(&server, data_bindings, &shutdown, &mut tasks);
		hello(&client).await?;
		let lifecycle = ProjectLifecycle { shutdown: Some(shutdown), tasks, server };
		Ok(Self {
			client,
			registry,
			eval_bridge,
			reflection_bridge,
			eval_control,
			search_bridge,
			github_credentials,
			goal_control,
			lifecycle,
		})
	}

	#[cfg(unix)]
	async fn start(
		root: &Path,
		state_dir: &Path,
		socket: &Path,
		docserver_socket: &Path,
		py_eval: bool,
		interrupt_grace: omp_core::Duration,
	) -> Result<Self, EnvdError> {
		let (worker_config, data_bindings) = worker_config(state_dir, py_eval, interrupt_grace)?;
		let server = EnvServer::open_project(
			root,
			state_dir,
			docserver_socket,
			Registry::new(),
			worker_config,
			None,
		)
		.await?;
		let server = Arc::new(server);
		let registry = server.registry();
		let eval_bridge = server.eval_bridge();
		let reflection_bridge = server.reflection_bridge();
		let eval_control = server.eval_control();
		let search_bridge = server.search_bridge();
		let github_credentials = server.github_credentials();
		let goal_control = server.goal_control();
		let (client, transport) = EnvClient::in_process(64);
		let in_process_server = Arc::clone(&server);
		let in_process = tokio::spawn(async move {
			in_process_server.serve_in_process(transport).await;
		});
		hello(&client).await?;
		let shutdown = CancellationToken::new();
		let uds_server = Arc::clone(&server);
		let uds_shutdown = shutdown.clone();
		let socket = socket.to_path_buf();
		let uds = tokio::spawn(async move {
			if let Err(error) = uds_server.serve_uds(&socket, uds_shutdown, None).await {
				// A lost same-build bind race is benign: the winner serves the
				// endpoint while this composition stays fully in-process.
				tracing::debug!(
					socket = %socket.display(),
					%error,
					"environment socket is served by another process"
				);
			}
		});
		let mut tasks = vec![in_process, uds];
		spawn_extension_data_servers(&server, data_bindings, &shutdown, &mut tasks);
		let lifecycle = ProjectLifecycle { shutdown: Some(shutdown), tasks, server };
		Ok(Self {
			client,
			registry,
			eval_bridge,
			reflection_bridge,
			eval_control,
			search_bridge,
			github_credentials,
			goal_control,
			lifecycle,
		})
	}

	#[cfg(windows)]
	async fn start(
		root: &Path,
		state_dir: &Path,
		socket: &Path,
		docserver_socket: &Path,
		py_eval: bool,
		interrupt_grace: omp_core::Duration,
	) -> Result<Self, EnvdError> {
		let owner_listener = windows::OwnerPipeListener::bind(socket)?;
		let (worker_config, data_bindings) = worker_config(state_dir, py_eval, interrupt_grace)?;
		let server = EnvServer::open_project(
			root,
			state_dir,
			docserver_socket,
			Registry::new(),
			worker_config,
			None,
		)
		.await?;
		let server = Arc::new(server);
		let registry = server.registry();
		let eval_bridge = server.eval_bridge();
		let reflection_bridge = server.reflection_bridge();
		let eval_control = server.eval_control();
		let search_bridge = server.search_bridge();
		let github_credentials = server.github_credentials();
		let goal_control = server.goal_control();
		let (client, transport) = EnvClient::in_process(64);
		let in_process_server = Arc::clone(&server);
		let in_process = tokio::spawn(async move {
			in_process_server.serve_in_process(transport).await;
		});
		hello(&client).await?;
		let shutdown = CancellationToken::new();
		let owner_server = Arc::clone(&server);
		let owner_shutdown = shutdown.clone();
		let owner = tokio::spawn(async move {
			if let Err(error) =
				windows::serve_owner_pipe(owner_server, owner_listener, owner_shutdown, None).await
			{
				tracing::warn!(%error, "environment owner pipe stopped");
			}
		});
		let mut tasks = vec![in_process, owner];
		spawn_extension_data_servers(&server, data_bindings, &shutdown, &mut tasks);
		let lifecycle = ProjectLifecycle { shutdown: Some(shutdown), tasks, server };
		Ok(Self {
			client,
			registry,
			eval_bridge,
			reflection_bridge,
			eval_control,
			search_bridge,
			github_credentials,
			goal_control,
			lifecycle,
		})
	}

	/// Starts an embedded Environment rooted at one isolated worktree.
	pub(crate) async fn isolated(root: &Path, state_dir: &Path) -> Result<Self, EnvdError> {
		let (worker_config, data_bindings) =
			worker_config(state_dir, true, omp_tool::DEFAULT_INTERRUPT_GRACE)?;
		let server =
			Arc::new(EnvServer::open_local(root, state_dir, Registry::new(), worker_config).await?);
		let registry = server.registry();
		let eval_bridge = server.eval_bridge();
		let reflection_bridge = server.reflection_bridge();
		let eval_control = server.eval_control();
		let search_bridge = server.search_bridge();
		let github_credentials = server.github_credentials();
		let goal_control = server.goal_control();
		let (client, transport) = EnvClient::in_process(64);
		let in_process_server = Arc::clone(&server);
		let in_process =
			tokio::spawn(async move { in_process_server.serve_in_process(transport).await });
		let shutdown = CancellationToken::new();
		let mut tasks = vec![in_process];
		spawn_extension_data_servers(&server, data_bindings, &shutdown, &mut tasks);
		hello(&client).await?;
		let lifecycle = ProjectLifecycle { shutdown: Some(shutdown), tasks, server };
		Ok(Self {
			client,
			registry,
			eval_bridge,
			reflection_bridge,
			eval_control,
			search_bridge,
			github_credentials,
			goal_control,
			lifecycle,
		})
	}

	pub(crate) const fn client(&self) -> &EnvClient {
		&self.client
	}

	pub(crate) fn registry(&self) -> Arc<Registry> {
		Arc::clone(&self.registry)
	}

	/// Binds or clears the editor-owned terminal backend for this environment
	/// composition.
	pub(crate) fn bind_acp_exec(&self, backend: Option<Arc<dyn tool_shell::AcpExecBackend>>) {
		self.lifecycle.server.bind_acp_exec(backend);
	}

	/// Binds or clears the editor-owned document backend for this environment
	/// composition.
	pub(crate) fn bind_acp_documents(&self, backend: Option<Arc<dyn docs::AcpDocumentBackend>>) {
		self.lifecycle.server.bind_acp_documents(backend);
	}

	/// Binds or clears the durable approval authority for Environment
	/// fallbacks.
	pub(crate) fn bind_approval_authority(
		&self,
		book: Option<Arc<omp_agent::ApprovalBook>>,
		route: Option<omp_agent::ApprovalRoute>,
	) {
		self.lifecycle.server.bind_approval_authority(book, route);
	}

	/// Returns the session's sole Off/Mnemopi runtime.
	pub(crate) fn memory_runtime(&self) -> Arc<omp_memory::MemoryRuntime> {
		self.lifecycle.server.memory_runtime()
	}

	pub(crate) fn eval_bridge(&self) -> Arc<eval::SessionBridgeHost> {
		Arc::clone(&self.eval_bridge)
	}

	/// Returns the late-bound memory reflection bridge.
	pub(crate) fn reflection_bridge(&self) -> Arc<crate::memory::ReflectionBridgeHost> {
		Arc::clone(&self.reflection_bridge)
	}

	pub(crate) fn eval_control(&self) -> omp_tools::eval::EvalSessionControl {
		self.eval_control.clone()
	}

	pub(crate) fn search_bridge(&self) -> Arc<search_backend::SearchBridgeHost> {
		Arc::clone(&self.search_bridge)
	}

	pub(crate) fn github_credentials(&self) -> Arc<github_url::GithubCredentialBridge> {
		Arc::clone(&self.github_credentials)
	}

	pub(crate) fn goal_control(&self) -> tools::AgentGoalControl {
		self.goal_control.clone()
	}

	/// Returns the Environment-owned authoritative sessions index.
	pub(crate) fn sessions_index(&self) -> Arc<omp_storage::index::SessionIndex> {
		self.lifecycle.server.sessions_index()
	}

	/// Binds authenticated extension CONTROL to the active Agent Journal until
	/// the returned sole-owner lease is dropped.
	///
	/// # Errors
	///
	/// Fails if a journal runtime is concurrently owned or an initial binding
	/// is attempted after child activation began.
	pub(crate) fn bind_agent_control(
		&self,
		sender: omp_agent::control::ControlSender,
	) -> Result<server::AgentControlBinding, EnvdError> {
		self.lifecycle.server.bind_agent_control(sender)
	}

	/// Binds extension device availability notifications to the active turn.
	pub(crate) fn bind_device_availability(&self, mailbox: omp_agent::MailboxSender) {
		self.lifecycle.server.bind_device_availability(mailbox);
	}
}

fn worker_config(
	state_dir: &Path,
	py_eval: bool,
	interrupt_grace: omp_core::Duration,
) -> Result<(ExtHostConfig, Vec<ExtensionDataBinding>), EnvdError> {
	let (authority, session_id, session_generation) = authenticated_runtime_identity()?;
	let mut config = ExtHostConfig::current(
		authority.principal().clone(),
		session_id.clone(),
		session_generation,
	)?;
	config.interrupt_grace = interrupt_grace;
	let mut bindings = Vec::new();
	if py_eval {
		let key = HostKey::new("workspace", "trusted", PY_EVAL_MODULE);
		let binding = ExtensionDataBinding::built_in(
			state_dir,
			key.clone(),
			session_id.as_str(),
			session_generation,
		);
		let mut digest = Hash32::hasher();
		digest.update(crate::build_id::current().as_bytes());
		digest.update(env!("CARGO_PKG_VERSION").as_bytes());
		digest.update(PY_EVAL_MODULE.as_bytes());
		let provenance = omp_core::Provenance::new(
			sf!("omp-first-party"),
			sf!(PY_EVAL_MODULE),
			sf!(env!("CARGO_PKG_VERSION")),
			omp_core::ArtifactDigest::new(digest.finalize().into_bytes()),
			sf!("workspace"),
			sf!("trusted"),
			1,
		);
		let manifest = crate::exthost::ExtensionManifest::py_eval(provenance, []);
		let mut extension = ExtHostSpec::new(key, manifest);
		extension.data_grants = binding.grants().clone();
		extension.data_socket = Some(extension_data_endpoint(&binding));
		config.extensions.push(extension);
		bindings.push(binding);
	}
	Ok((config, bindings))
}

/// Derives the authenticated OS principal and a fresh project-runtime fence.
///
/// The generation is the runtime's creation timestamp, not a placeholder
/// ordinal, and the ULID distinguishes simultaneous runtimes.
pub(crate) fn authenticated_runtime_identity()
-> Result<(crate::exthost::PrincipalAuthority, Str, u64), EnvdError> {
	let user = authenticated_os_user()?;
	let principal = omp_core::Principal::new(Str::from(format!("os:{user}")), user);
	let authority = crate::exthost::PrincipalAuthority::new(principal);
	let session_id = Str::from(omp_core::Ulid::generate().to_string());
	let session_generation = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map_err(io::Error::other)?
		.as_millis()
		.try_into()
		.map_err(io::Error::other)?;
	Ok((authority, session_id, session_generation))
}

#[cfg(unix)]
fn authenticated_os_user() -> Result<Str, EnvdError> {
	let uid = nix::unistd::geteuid();
	let user = nix::unistd::User::from_uid(uid)
		.map_err(io::Error::from)?
		.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "current OS user has no account"))?;
	Ok(Str::from(user.name))
}

#[cfg(windows)]
fn authenticated_os_user() -> Result<Str, EnvdError> {
	Ok(omp_env::windows::current_user_name()?)
}

#[cfg(not(windows))]
fn extension_data_endpoint(binding: &ExtensionDataBinding) -> std::path::PathBuf {
	binding.path().to_path_buf()
}

#[cfg(windows)]
fn extension_data_endpoint(binding: &ExtensionDataBinding) -> std::path::PathBuf {
	windows::extension_pipe_endpoint(binding)
}

#[cfg(unix)]
fn spawn_extension_data_servers(
	server: &Arc<EnvServer>,
	bindings: Vec<ExtensionDataBinding>,
	shutdown: &CancellationToken,
	tasks: &mut Vec<tokio::task::JoinHandle<()>>,
) {
	for binding in bindings {
		let server = Arc::clone(server);
		let shutdown = shutdown.clone();
		tasks.push(tokio::spawn(async move {
			if let Err(error) = server.serve_extension_uds(binding, shutdown).await {
				tracing::warn!(%error, "extension DATA socket stopped");
			}
		}));
	}
}

#[cfg(windows)]
fn spawn_extension_data_servers(
	server: &Arc<EnvServer>,
	bindings: Vec<ExtensionDataBinding>,
	shutdown: &CancellationToken,
	tasks: &mut Vec<tokio::task::JoinHandle<()>>,
) {
	for binding in bindings {
		let server = Arc::clone(server);
		let shutdown = shutdown.clone();
		tasks.push(tokio::spawn(async move {
			if let Err(error) = windows::serve_extension_pipe(server, binding, shutdown).await {
				tracing::warn!(%error, "extension DATA pipe stopped");
			}
		}));
	}
}

#[cfg(not(any(unix, windows)))]
fn spawn_extension_data_servers(
	_server: &Arc<EnvServer>,
	_bindings: Vec<ExtensionDataBinding>,
	_shutdown: &CancellationToken,
	_tasks: &mut Vec<tokio::task::JoinHandle<()>>,
) {
}

async fn hello(client: &EnvClient) -> Result<ServerHello, EnvdError> {
	Ok(client
		.hello(ClientHello {
			client: "omp-chat".into(),
			schema_rev: omp_proto::SCHEMA_REV,
			..ClientHello::default()
		})
		.await?)
}

/// Launches a detached `omp envd` for this project and waits until its
/// environment socket answers a hello.
async fn spawn_project_daemon(
	root: &Path,
	state_dir: &Path,
	socket: &Path,
	docserver_socket: &Path,
) -> Result<(), EnvdError> {
	let executable = std::env::current_exe()?;
	spawn_project_daemon_with(
		&executable,
		root,
		state_dir,
		socket,
		docserver_socket,
		std::time::Duration::from_secs(10),
	)
	.await
}

/// Spawns `executable envd …` detached from this process and waits for
/// readiness on `socket` within `deadline`.
///
/// The daemon runs in its own process group with output appended to
/// `envd.log` in the state directory. A daemon that fails to become ready is
/// killed so it cannot linger half-initialized while the caller falls back
/// to an embedded environment.
async fn spawn_project_daemon_with(
	executable: &Path,
	root: &Path,
	state_dir: &Path,
	socket: &Path,
	docserver_socket: &Path,
	deadline: std::time::Duration,
) -> Result<(), EnvdError> {
	std::fs::create_dir_all(state_dir)?;
	let log = std::fs::OpenOptions::new()
		.create(true)
		.append(true)
		.open(state_dir.join("envd.log"))?;
	let errors = log.try_clone()?;
	let mut command = tokio::process::Command::new(executable);
	command
		.arg("envd")
		.arg("--root")
		.arg(root)
		.arg("--state-dir")
		.arg(state_dir)
		.arg("--socket")
		.arg(socket)
		.arg("--docserver-socket")
		.arg(docserver_socket)
		.stdin(std::process::Stdio::null())
		.stdout(log)
		.stderr(errors)
		.kill_on_drop(false);
	#[cfg(unix)]
	{
		use std::os::unix::process::CommandExt as _;
		command.as_std_mut().process_group(0);
	}
	let mut child = command.spawn()?;
	let deadline = tokio::time::Instant::now() + deadline;
	loop {
		if let Some(status) = child.try_wait()? {
			return Err(
				io::Error::other(format!("project daemon exited during startup: {status}")).into(),
			);
		}
		if owner_endpoint_ready(socket).await {
			// Reap in the background; the daemon's lifetime is its own.
			tokio::spawn(async move {
				let _ = child.wait().await;
			});
			return Ok(());
		}
		if tokio::time::Instant::now() >= deadline {
			let _ = child.start_kill();
			tokio::spawn(async move {
				let _ = child.wait().await;
			});
			return Err(
				io::Error::new(io::ErrorKind::TimedOut, "project daemon did not become ready").into(),
			);
		}
		tokio::time::sleep(std::time::Duration::from_millis(50)).await;
	}
}

#[cfg(unix)]
async fn owner_endpoint_ready(socket: &Path) -> bool {
	let Ok((probe, bridge)) = EnvServer::connect_owner_uds(socket).await else {
		return false;
	};
	let ready = hello(&probe).await.is_ok();
	bridge.abort();
	ready
}

#[cfg(windows)]
async fn owner_endpoint_ready(socket: &Path) -> bool {
	let Ok((probe, bridge)) = omp_env::windows::connect_owner_pipe(socket) else {
		return false;
	};
	let ready = hello(&probe).await.is_ok();
	bridge.abort();
	ready
}

#[cfg(all(test, unix))]
mod tests {
	use super::*;

	async fn spawn_with(executable: &Path, deadline_ms: u64) -> Result<(), EnvdError> {
		let scratch = tempfile::tempdir().expect("scratch state directory");
		spawn_project_daemon_with(
			executable,
			scratch.path(),
			scratch.path(),
			&scratch.path().join("env.sock"),
			&scratch.path().join("doc.sock"),
			std::time::Duration::from_millis(deadline_ms),
		)
		.await
	}

	#[tokio::test]
	async fn spawn_reports_missing_daemon_executable() {
		let error = spawn_with(Path::new("/nonexistent/omp"), 1_000)
			.await
			.expect_err("missing executable must fail");
		assert!(matches!(error, EnvdError::Io(_)));
	}

	#[tokio::test]
	async fn spawn_reports_a_daemon_that_exits_during_startup() {
		let error = spawn_with(Path::new("/usr/bin/true"), 5_000)
			.await
			.expect_err("exiting daemon must fail");
		assert!(error.to_string().contains("exited during startup"), "unexpected error: {error}");
	}

	#[tokio::test]
	async fn spawn_kills_a_daemon_that_never_becomes_ready() {
		use std::os::unix::fs::PermissionsExt as _;

		let scratch = tempfile::tempdir().expect("scratch script directory");
		let script = scratch.path().join("hang.sh");
		std::fs::write(&script, "#!/bin/sh\nexec sleep 30\n").expect("write hang script");
		std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
			.expect("mark script executable");

		let error = spawn_with(&script, 300)
			.await
			.expect_err("unready daemon must time out");
		let EnvdError::Io(error) = &error else {
			panic!("unexpected error: {error}");
		};
		assert_eq!(error.kind(), io::ErrorKind::TimedOut);
	}
}
