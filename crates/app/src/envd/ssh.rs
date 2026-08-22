//! Native Rust SSH/SFTP sessions and configured-host authority.

use std::{
	collections::BTreeMap,
	net::SocketAddr,
	path::{Path, PathBuf},
	sync::Arc,
	time::Duration,
};

use omp_core::{CowBytes, Str};
use parking_lot::RwLock;
use russh::{
	client,
	keys::{HashAlg, load_secret_key},
};
use russh_sftp::{client::SftpSession, protocol::OpenFlags};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::{
	net::TcpListener,
	task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;

const DEFAULT_READ_LIMIT: usize = 8 * 1024 * 1024;
const DEFAULT_WRITE_LIMIT: usize = 8 * 1024 * 1024;
const DEFAULT_LIST_LIMIT: usize = 1_000;
const DEFAULT_EXEC_LIMIT: usize = 1024 * 1024;
const MAX_TIMEOUT_SECS: u64 = 120;
const INTERACTIVE_MESSAGE_LIMIT: usize = 64 * 1024;
const INTERACTIVE_CHANNEL_CAPACITY: usize = 16;
const FORWARD_ERROR_CAPACITY: usize = 8;
const MAX_FORWARD_CONNECTIONS: usize = 16;

/// A configured native SSH host.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
	/// DNS name or numeric address.
	pub address:      Str,
	/// SSH port.
	#[serde(default = "default_port")]
	pub port:         u16,
	/// Remote account name.
	pub user:         Str,
	/// SHA-256 host-key fingerprint, including the `SHA256:` prefix.
	pub host_key:     Str,
	/// Authentication policy.
	pub auth:         AuthPolicy,
	/// Per-operation timeout in seconds.
	#[serde(default = "default_timeout")]
	pub timeout_secs: u64,
}

const fn default_port() -> u16 {
	22
}
const fn default_timeout() -> u64 {
	30
}

/// Explicit SSH authentication policy. Passwords are intentionally unsupported.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthPolicy {
	/// Use identities from the native SSH agent protocol.
	Agent,
	/// Load one unencrypted private key after checking its filesystem
	/// permissions.
	Key { path: PathBuf },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HostFile {
	#[serde(default)]
	hosts: BTreeMap<Str, HostConfig>,
}

/// Immutable configured-host store, reloadable by its owner.
#[derive(Clone, Debug, Default)]
pub struct HostStore {
	hosts: Arc<RwLock<BTreeMap<Str, HostConfig>>>,
}

impl HostStore {
	/// Loads `hosts.toml`. A missing file produces an empty store.
	pub fn load(path: &Path) -> Result<Self, SshError> {
		let body = match std::fs::read_to_string(path) {
			Ok(body) => body,
			Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
			Err(source) => return Err(SshError::ConfigIo { path: path.to_path_buf(), source }),
		};
		let parsed: HostFile = toml::from_str(&body)
			.map_err(|source| SshError::ConfigParse { path: path.to_path_buf(), source })?;
		for (alias, host) in &parsed.hosts {
			validate_alias(alias)?;
			validate_host(host)?;
		}
		Ok(Self { hosts: Arc::new(RwLock::new(parsed.hosts)) })
	}

	/// Returns a configured host without permitting URI-provided connection
	/// overrides.
	pub fn get(&self, alias: &str) -> Result<HostConfig, SshError> {
		self
			.hosts
			.read()
			.get(alias)
			.cloned()
			.ok_or_else(|| SshError::UnknownHost { alias: Str::new(alias) })
	}

	/// Returns configured aliases in deterministic order.
	pub fn aliases(&self) -> Vec<Str> {
		self.hosts.read().keys().cloned().collect()
	}

	/// Atomically inserts or replaces one validated host in this scoped store.
	pub fn upsert(&self, path: &Path, alias: Str, host: HostConfig) -> Result<(), SshError> {
		validate_alias(alias.as_str())?;
		validate_host(&host)?;
		let mut hosts = self.hosts.write();
		hosts.insert(alias, host);
		persist_hosts(path, &hosts)
	}

	/// Atomically removes one host from this scoped store.
	pub fn remove(&self, path: &Path, alias: &str) -> Result<bool, SshError> {
		validate_alias(alias)?;
		let mut hosts = self.hosts.write();
		let removed = hosts.remove(alias).is_some();
		if removed {
			persist_hosts(path, &hosts)?;
		}
		Ok(removed)
	}
}

fn persist_hosts(path: &Path, hosts: &BTreeMap<Str, HostConfig>) -> Result<(), SshError> {
	let body = toml::to_string_pretty(&HostFile { hosts: hosts.clone() })
		.map_err(|source| SshError::ConfigEncode { path: path.to_path_buf(), source })?;
	crate::settings::io::atomic_replace(path, &body)
		.map_err(|source| SshError::ConfigWrite { path: path.to_path_buf(), source })
}

fn validate_alias(alias: &str) -> Result<(), SshError> {
	if alias.is_empty()
		|| alias.len() > 128
		|| !alias
			.bytes()
			.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
	{
		return Err(SshError::InvalidAlias { alias: Str::new(alias) });
	}
	Ok(())
}

fn validate_host(host: &HostConfig) -> Result<(), SshError> {
	if host.address.is_empty()
		|| host.user.is_empty()
		|| host.port == 0
		|| !host.host_key.starts_with("SHA256:")
	{
		return Err(SshError::InvalidHostConfig);
	}
	Ok(())
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HostCapabilities {
	pub sftp: bool,
	pub exec: bool,
}

/// A bounded directory entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteEntry {
	pub name:      Str,
	pub directory: bool,
	pub size:      u64,
}

/// Bounded remote metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RemoteMetadata {
	pub directory: bool,
	pub size:      u64,
}

/// Bounded command outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecOutput {
	pub stdout:      CowBytes<'static>,
	pub stderr:      CowBytes<'static>,
	pub exit_status: Option<u32>,
}
#[derive(Debug)]
enum InteractiveInput {
	Data(CowBytes<'static>),
	Eof,
}

/// One bounded event emitted by an interactive SSH command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InteractiveEvent {
	/// Bytes written by the remote command to stdout.
	Stdout(CowBytes<'static>),
	/// Bytes written by the remote command to stderr.
	Stderr(CowBytes<'static>),
	/// Exit status reported by the remote command.
	ExitStatus(u32),
}

/// Bounded bidirectional channel for one interactive SSH command.
#[derive(Debug)]
pub struct InteractiveChannel {
	input:  flume::Sender<InteractiveInput>,
	events: flume::Receiver<Result<InteractiveEvent, SshError>>,
}

impl InteractiveChannel {
	/// Sends one bounded stdin chunk to the remote command.
	pub async fn write(&self, bytes: &[u8]) -> Result<(), SshError> {
		if bytes.len() > INTERACTIVE_MESSAGE_LIMIT {
			return Err(SshError::Limit { limit: INTERACTIVE_MESSAGE_LIMIT });
		}
		self
			.input
			.send_async(InteractiveInput::Data(CowBytes::from(bytes.to_vec())))
			.await
			.map_err(|_| SshError::InteractiveClosed)
	}

	/// Closes the remote command's stdin while retaining its output stream.
	pub async fn eof(&self) -> Result<(), SshError> {
		self
			.input
			.send_async(InteractiveInput::Eof)
			.await
			.map_err(|_| SshError::InteractiveClosed)
	}

	/// Receives the next stdout, stderr, or exit-status event.
	pub async fn next_event(&self) -> Result<Option<InteractiveEvent>, SshError> {
		match self.events.recv_async().await {
			Ok(event) => event.map(Some),
			Err(_) => Ok(None),
		}
	}
}

/// Active loopback listener forwarding accepted connections through SSH.
#[derive(Debug)]
pub struct LocalForward {
	local_addr: SocketAddr,
	errors:     flume::Receiver<SshError>,
	shutdown:   CancellationToken,
	task:       Option<JoinHandle<()>>,
}

impl LocalForward {
	/// Returns the bound loopback address.
	pub const fn local_addr(&self) -> SocketAddr {
		self.local_addr
	}

	/// Receives the next forwarding failure, if the listener is still active.
	pub async fn next_error(&self) -> Option<SshError> {
		self.errors.recv_async().await.ok()
	}

	/// Stops the listener and every active forwarded connection.
	pub async fn close(mut self) -> Result<(), SshError> {
		self.shutdown.cancel();
		if let Some(task) = self.task.take() {
			task.await?;
		}
		Ok(())
	}
}

impl Drop for LocalForward {
	fn drop(&mut self) {
		self.shutdown.cancel();
	}
}

#[derive(Clone, Debug)]
struct ClientHandler {
	expected: Str,
}

impl client::Handler for ClientHandler {
	type Error = russh::Error;

	async fn check_server_key(
		&mut self,
		key: &russh::keys::PublicKeyOrCertificate,
	) -> Result<bool, Self::Error> {
		let fingerprint = key.public_key().fingerprint(HashAlg::Sha256).to_string();
		Ok(fingerprint == self.expected.as_str())
	}
}

/// Native SSH/SFTP service with a configured-host authority and capability
/// cache.
#[derive(Clone, Debug)]
pub struct SshService {
	hosts:        HostStore,
	capabilities: Arc<RwLock<BTreeMap<Str, HostCapabilities>>>,
}

impl SshService {
	pub fn new(hosts: HostStore) -> Self {
		Self { hosts, capabilities: Arc::new(RwLock::new(BTreeMap::new())) }
	}

	pub fn aliases(&self) -> Vec<Str> {
		self.hosts.aliases()
	}

	pub fn cached_capabilities(&self, alias: &str) -> Option<HostCapabilities> {
		self.capabilities.read().get(alias).copied()
	}

	async fn connect(&self, alias: &str) -> Result<client::Handle<ClientHandler>, SshError> {
		let host = self.hosts.get(alias)?;
		let timeout = Duration::from_secs(host.timeout_secs.clamp(1, MAX_TIMEOUT_SECS));
		let connect = client::connect(
			Arc::new(client::Config::default()),
			(host.address.as_str(), host.port),
			ClientHandler { expected: host.host_key.clone() },
		);
		let mut session = tokio::time::timeout(timeout, connect)
			.await
			.map_err(|_| SshError::Timeout)??;
		let authenticated = match &host.auth {
			AuthPolicy::Key { path } => {
				check_key_permissions(path)?;
				let key = load_secret_key(path, None)
					.map_err(|source| SshError::Key { path: path.clone(), source })?;
				let hash = session.best_supported_rsa_hash().await?.flatten();
				session
					.authenticate_publickey(
						host.user.as_str(),
						russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), hash),
					)
					.await?
					.success()
			},
			AuthPolicy::Agent => authenticate_agent(&mut session, host.user.as_str()).await?,
		};
		if !authenticated {
			return Err(SshError::Authentication { alias: Str::new(alias) });
		}
		Ok(session)
	}

	async fn sftp(&self, alias: &str) -> Result<SftpSession, SshError> {
		let session = self.connect(alias).await?;
		let channel = session.channel_open_session().await?;
		channel.request_subsystem(true, "sftp").await?;
		let sftp = SftpSession::new(channel.into_stream()).await?;
		self
			.capabilities
			.write()
			.entry(Str::new(alias))
			.or_default()
			.sftp = true;
		Ok(sftp)
	}

	pub async fn probe(&self, alias: &str) -> Result<HostCapabilities, SshError> {
		let _ = self.sftp(alias).await?;
		let mut caps = self.cached_capabilities(alias).unwrap_or_default();
		caps.exec = true;
		self.capabilities.write().insert(Str::new(alias), caps);
		Ok(caps)
	}

	pub async fn read(
		&self,
		alias: &str,
		path: &str,
		max_bytes: usize,
	) -> Result<CowBytes<'static>, SshError> {
		let limit = max_bytes.min(DEFAULT_READ_LIMIT);
		let sftp = self.sftp(alias).await?;
		let metadata = sftp.metadata(path).await?;
		if metadata.file_type().is_dir() {
			return Err(SshError::IsDirectory);
		}
		if metadata.size.unwrap_or(0) > limit as u64 {
			return Err(SshError::Limit { limit });
		}
		let file = sftp.open(path).await?;
		let mut bytes = Vec::with_capacity(metadata.size.unwrap_or(0).min(limit as u64) as usize);
		file
			.take((limit + 1) as u64)
			.read_to_end(&mut bytes)
			.await?;
		if bytes.len() > limit {
			return Err(SshError::Limit { limit });
		}
		Ok(CowBytes::from(bytes))
	}

	pub async fn write(&self, alias: &str, path: &str, bytes: &[u8]) -> Result<(), SshError> {
		if bytes.len() > DEFAULT_WRITE_LIMIT {
			return Err(SshError::Limit { limit: DEFAULT_WRITE_LIMIT });
		}
		let sftp = self.sftp(alias).await?;
		let mut file = sftp
			.open_with_flags(path, OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE)
			.await?;
		file.write_all(bytes).await?;
		file.sync_all().await?;
		file.shutdown().await?;
		Ok(())
	}

	pub async fn stat(&self, alias: &str, path: &str) -> Result<RemoteMetadata, SshError> {
		let metadata = self.sftp(alias).await?.metadata(path).await?;
		Ok(RemoteMetadata {
			directory: metadata.file_type().is_dir(),
			size:      metadata.size.unwrap_or(0),
		})
	}

	pub async fn list(
		&self,
		alias: &str,
		path: &str,
		max_entries: usize,
	) -> Result<(Vec<RemoteEntry>, bool), SshError> {
		let limit = max_entries.min(DEFAULT_LIST_LIMIT);
		let mut entries = self
			.sftp(alias)
			.await?
			.read_dir(path)
			.await?
			.collect::<Vec<_>>();
		entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
		let truncated = entries.len() > limit;
		entries.truncate(limit);
		Ok((
			entries
				.into_iter()
				.map(|entry| RemoteEntry {
					name:      Str::new(entry.file_name()),
					directory: entry.metadata().file_type().is_dir(),
					size:      entry.metadata().size.unwrap_or(0),
				})
				.collect(),
			truncated,
		))
	}

	/// Opens a bounded bidirectional channel to one remote command.
	pub async fn open_interactive(
		&self,
		alias: &str,
		command: &str,
	) -> Result<InteractiveChannel, SshError> {
		if command.as_bytes().contains(&0) {
			return Err(SshError::InvalidCommand);
		}
		let session = self.connect(alias).await?;
		let channel = session.channel_open_session().await?;
		channel.exec(true, command.as_bytes()).await?;
		let (interactive, inputs, events) = interactive_channel_pair();
		tokio::spawn(run_interactive_channel(channel, inputs, events));
		Ok(interactive)
	}

	/// Binds a loopback listener and forwards accepted TCP connections through
	/// the configured SSH host.
	pub async fn local_forward(
		&self,
		alias: &str,
		local_port: u16,
		remote_host: &str,
		remote_port: u16,
	) -> Result<LocalForward, SshError> {
		if remote_host.is_empty() || remote_port == 0 {
			return Err(SshError::InvalidForwardTarget);
		}
		let session = Arc::new(self.connect(alias).await?);
		let listener = TcpListener::bind(("127.0.0.1", local_port)).await?;
		let local_addr = listener.local_addr()?;
		let shutdown = CancellationToken::new();
		let (error_tx, errors) = flume::bounded(FORWARD_ERROR_CAPACITY);
		let task = tokio::spawn(run_local_forward(
			listener,
			session,
			Str::new(remote_host),
			remote_port,
			shutdown.clone(),
			error_tx,
		));
		Ok(LocalForward { local_addr, errors, shutdown, task: Some(task) })
	}

	pub async fn exec(
		&self,
		alias: &str,
		command: &str,
		max_bytes: usize,
	) -> Result<ExecOutput, SshError> {
		if command.as_bytes().contains(&0) {
			return Err(SshError::InvalidCommand);
		}
		let limit = max_bytes.min(DEFAULT_EXEC_LIMIT);
		let session = self.connect(alias).await?;
		let mut channel = session.channel_open_session().await?;
		channel.exec(true, command.as_bytes()).await?;
		let mut stdout = Vec::new();
		let mut stderr = Vec::new();
		let mut status = None;
		while let Some(message) = channel.wait().await {
			match message {
				russh::ChannelMsg::Data { data } => append_bounded(&mut stdout, &data, limit)?,
				russh::ChannelMsg::ExtendedData { data, .. } => {
					append_bounded(&mut stderr, &data, limit)?
				},
				russh::ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
				_ => {},
			}
		}
		self
			.capabilities
			.write()
			.entry(Str::new(alias))
			.or_default()
			.exec = true;
		Ok(ExecOutput {
			stdout:      CowBytes::from(stdout),
			stderr:      CowBytes::from(stderr),
			exit_status: status,
		})
	}
}

fn interactive_channel_pair() -> (
	InteractiveChannel,
	flume::Receiver<InteractiveInput>,
	flume::Sender<Result<InteractiveEvent, SshError>>,
) {
	let (input, inputs) = flume::bounded(INTERACTIVE_CHANNEL_CAPACITY);
	let (events, output) = flume::bounded(INTERACTIVE_CHANNEL_CAPACITY);
	(InteractiveChannel { input, events: output }, inputs, events)
}

async fn run_interactive_channel(
	mut channel: russh::Channel<client::Msg>,
	inputs: flume::Receiver<InteractiveInput>,
	events: flume::Sender<Result<InteractiveEvent, SshError>>,
) {
	let result: Result<(), russh::Error> = async {
		loop {
			tokio::select! {
				input = inputs.recv_async() => match input {
					Ok(InteractiveInput::Data(data)) => channel.data_bytes(data).await?,
					Ok(InteractiveInput::Eof) => channel.eof().await?,
					Err(_) => return Ok(()),
				},
				message = channel.wait() => {
					let Some(message) = message else {
						return Ok(());
					};
					let event = match message {
						russh::ChannelMsg::Data { data } => {
							Some(InteractiveEvent::Stdout(CowBytes::from(data.to_vec())))
						},
						russh::ChannelMsg::ExtendedData { data, .. } => {
							Some(InteractiveEvent::Stderr(CowBytes::from(data.to_vec())))
						},
						russh::ChannelMsg::ExitStatus { exit_status } => {
							Some(InteractiveEvent::ExitStatus(exit_status))
						},
						_ => None,
					};
					if let Some(event) = event
						&& events.send_async(Ok(event)).await.is_err()
					{
						return Ok(());
					}
				},
			}
		}
	}
	.await;
	if let Err(error) = result {
		let _ = events.send_async(Err(SshError::Ssh(error))).await;
	}
}

async fn run_local_forward(
	listener: TcpListener,
	session: Arc<client::Handle<ClientHandler>>,
	remote_host: Str,
	remote_port: u16,
	shutdown: CancellationToken,
	errors: flume::Sender<SshError>,
) {
	let mut connections = JoinSet::new();
	loop {
		tokio::select! {
			() = shutdown.cancelled() => break,
			completed = connections.join_next(), if !connections.is_empty() => {
				match completed {
					Some(Ok(Err(error))) => report_forward_error(&errors, error),
					Some(Err(error)) => report_forward_error(&errors, SshError::Join(error)),
					Some(Ok(Ok(()))) | None => {},
				}
			},
			accepted = listener.accept() => {
				let (mut socket, peer) = match accepted {
					Ok(accepted) => accepted,
					Err(error) => {
						report_forward_error(&errors, SshError::Io(error));
						break;
					},
				};
				if connections.len() >= MAX_FORWARD_CONNECTIONS {
					report_forward_error(
						&errors,
						SshError::ForwardCapacity { limit: MAX_FORWARD_CONNECTIONS },
					);
					continue;
				}
				let channel = match session
					.channel_open_direct_tcpip(
						remote_host.as_str(),
						u32::from(remote_port),
						peer.ip().to_string(),
						u32::from(peer.port()),
					)
					.await
				{
					Ok(channel) => channel,
					Err(error) => {
						report_forward_error(&errors, SshError::Ssh(error));
						continue;
					},
				};
				connections.spawn(async move {
					let mut stream = channel.into_stream();
					tokio::io::copy_bidirectional(&mut socket, &mut stream).await?;
					Ok::<_, SshError>(())
				});
			},
		}
	}
	connections.abort_all();
	while connections.join_next().await.is_some() {}
}

fn report_forward_error(errors: &flume::Sender<SshError>, error: SshError) {
	let _ = errors.try_send(error);
}

fn append_bounded(target: &mut Vec<u8>, bytes: &[u8], limit: usize) -> Result<(), SshError> {
	if target.len().saturating_add(bytes.len()) > limit {
		return Err(SshError::Limit { limit });
	}
	target.extend_from_slice(bytes);
	Ok(())
}

#[cfg(unix)]
fn check_key_permissions(path: &Path) -> Result<(), SshError> {
	use std::os::unix::fs::MetadataExt as _;
	let metadata = std::fs::metadata(path)
		.map_err(|source| SshError::ConfigIo { path: path.to_path_buf(), source })?;
	if metadata.mode() & 0o077 != 0 {
		return Err(SshError::UnsafeKeyPermissions { path: path.to_path_buf() });
	}
	Ok(())
}
#[cfg(not(unix))]
fn check_key_permissions(path: &Path) -> Result<(), SshError> {
	if !path.is_file() {
		return Err(SshError::UnsafeKeyPermissions { path: path.to_path_buf() });
	}
	Ok(())
}

#[cfg(unix)]
async fn authenticate_agent(
	session: &mut client::Handle<ClientHandler>,
	user: &str,
) -> Result<bool, SshError> {
	let mut agent = russh::keys::agent::client::AgentClient::connect_env().await?;
	for identity in agent.request_identities().await? {
		let key = identity.public_key().into_owned();
		if session
			.authenticate_publickey_with(user, key, None, &mut agent)
			.await?
			.success()
		{
			return Ok(true);
		}
	}
	Ok(false)
}
#[cfg(not(unix))]
async fn authenticate_agent(
	_session: &mut client::Handle<ClientHandler>,
	_user: &str,
) -> Result<bool, SshError> {
	Err(SshError::AgentUnavailable)
}

/// Native SSH operation failure.
#[derive(Debug, thiserror::Error)]
pub enum SshError {
	#[error("cannot read SSH host configuration {path}")]
	ConfigIo {
		path:   PathBuf,
		#[source]
		source: std::io::Error,
	},
	#[error("invalid SSH host configuration {path}")]
	ConfigParse {
		path:   PathBuf,
		#[source]
		source: toml::de::Error,
	},
	#[error("cannot encode SSH host configuration {path}")]
	ConfigEncode {
		path:   PathBuf,
		#[source]
		source: toml::ser::Error,
	},
	#[error("cannot atomically write SSH host configuration {path}")]
	ConfigWrite {
		path:   PathBuf,
		#[source]
		source: crate::settings::io::SettingsIoError,
	},
	#[error("invalid configured SSH alias {alias}")]
	InvalidAlias { alias: Str },
	#[error("configured SSH host is missing an address, user, port, or SHA-256 host key")]
	InvalidHostConfig,
	#[error("SSH host {alias} is not configured")]
	UnknownHost { alias: Str },
	#[error("private key {path} has unsafe permissions")]
	UnsafeKeyPermissions { path: PathBuf },
	#[error("cannot load private key {path}")]
	Key {
		path:   PathBuf,
		#[source]
		source: russh::keys::Error,
	},
	#[error("SSH authentication failed for configured host {alias}")]
	Authentication { alias: Str },
	#[error("SSH operation timed out")]
	Timeout,
	#[error("remote path is a directory")]
	IsDirectory,
	#[error("remote operation exceeded its {limit}-byte/item bound")]
	Limit { limit: usize },
	#[error("remote command contains a NUL byte")]
	InvalidCommand,
	#[error("interactive SSH command channel is closed")]
	InteractiveClosed,
	#[error("SSH local-forward target is invalid")]
	InvalidForwardTarget,
	#[error("SSH local-forward connection limit {limit} was reached")]
	ForwardCapacity { limit: usize },
	#[error("native SSH agent authentication is unavailable on this platform")]
	AgentUnavailable,
	#[error(transparent)]
	Ssh(#[from] russh::Error),
	#[error(transparent)]
	Sftp(#[from] russh_sftp::client::error::Error),
	#[error(transparent)]
	Io(#[from] std::io::Error),
	#[error(transparent)]
	Agent(#[from] russh::keys::Error),
	#[error(transparent)]
	AgentAuth(#[from] russh::AgentAuthError),
	#[error(transparent)]
	Join(#[from] tokio::task::JoinError),
}
#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn interactive_channel_carries_bounded_input_and_output() {
		let (channel, inputs, events) = interactive_channel_pair();
		channel.write(b"pasted-code\n").await.expect("send interactive input");
		let InteractiveInput::Data(input) = inputs.recv_async().await.expect("receive input") else {
			panic!("expected interactive data");
		};
		assert_eq!(input.as_ref(), b"pasted-code\n");

		events
			.send_async(Ok(InteractiveEvent::Stdout(CowBytes::from_static(
				b"Credentials saved\n",
			))))
			.await
			.expect("send interactive output");
		assert_eq!(
			channel.next_event().await.expect("receive output"),
			Some(InteractiveEvent::Stdout(CowBytes::from_static(
				b"Credentials saved\n"
			)))
		);

		let oversized = vec![0_u8; INTERACTIVE_MESSAGE_LIMIT + 1];
		assert!(matches!(
			channel.write(&oversized).await,
			Err(SshError::Limit { limit: INTERACTIVE_MESSAGE_LIMIT })
		));
	}

	#[tokio::test]
	async fn local_forward_rejects_invalid_target_without_connecting() {
		let service = SshService::new(HostStore::default());
		assert!(matches!(
			service.local_forward("missing", 0, "", 0).await,
			Err(SshError::InvalidForwardTarget)
		));
	}
}
