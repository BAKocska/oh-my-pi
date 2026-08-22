//! Native Rust SSH/SFTP sessions and configured-host authority.

use std::{
	collections::BTreeMap,
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
use serde::Deserialize;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

const DEFAULT_READ_LIMIT: usize = 8 * 1024 * 1024;
const DEFAULT_WRITE_LIMIT: usize = 8 * 1024 * 1024;
const DEFAULT_LIST_LIMIT: usize = 1_000;
const DEFAULT_EXEC_LIMIT: usize = 1024 * 1024;
const MAX_TIMEOUT_SECS: u64 = 120;

/// A configured native SSH host.
#[derive(Clone, Debug, Deserialize)]
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
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthPolicy {
	/// Use identities from the native SSH agent protocol.
	Agent,
	/// Load one unencrypted private key after checking its filesystem
	/// permissions.
	Key { path: PathBuf },
}

#[derive(Debug, Deserialize)]
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
		let mut file = sftp.open(path).await?;
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
}
