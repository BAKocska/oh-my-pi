//! Environment-scoped MCP lifecycle supervisor and definition catalog.

use std::{
	collections::{BTreeMap, BTreeSet, VecDeque},
	ffi::OsString,
	path::{Path, PathBuf},
	pin::Pin,
	sync::{Arc, Weak},
	time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use futures::{
	Future, FutureExt as _, StreamExt as _, future::BoxFuture, stream::FuturesUnordered,
};
use http::{HeaderMap, HeaderName, HeaderValue};
use omp_core::Str;
use omp_proto::env::v1 as pb;
use omp_tool::{LeafOwner, LeafVersion};
use parking_lot::{Mutex, RwLock};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use url::Url;

use super::{
	McpServerBackend, McpService, McpServiceError,
	client::{ClientError, InitializedServer, McpClient},
	config::{McpServerConfig, RequestIdFormat as ConfigRequestIdFormat, TransportKind},
	config_values::{ResolvedConfigValue, ResolvedTransportValues},
	device::{DeviceError, McpDeviceDefinitions},
	http::{RefreshableHeaders, StreamableHttpConfig, StreamableHttpTransport, WreqExchange},
	invoke,
	json_rpc::RequestIdFormat,
	legacy_sse::{LegacySseConfig, LegacySseTransport},
	prompts::{PromptContent, PromptDefinition, PromptError, PromptsClient},
	resources::{ResourceDefinition, ResourceError, ResourceTemplate, ResourcesClient},
	stdio::{StdioConfig, StdioTransport},
	transport::{McpTransport, TransportError},
};

const STARTUP_RACE: Duration = Duration::from_millis(250);
const RECONNECT_WINDOW: Duration = Duration::from_secs(30);
const RECONNECT_BURST_LIMIT: usize = 5;
const RECONNECT_DELAYS: [Duration; 4] = [
	Duration::from_millis(500),
	Duration::from_secs(1),
	Duration::from_secs(2),
	Duration::from_secs(4),
];
const MAX_INSTRUCTIONS_BYTES: usize = 10_000;
const MAX_TOOL_PAGES: usize = 1_024;

/// Fully resolved declaration mounted into one Environment supervisor.
#[derive(Clone)]
pub struct MountSpec {
	/// Stable server name.
	pub name:         Str,
	/// Validated declaration used as the persistent cache identity.
	pub config:       Arc<McpServerConfig>,
	/// Canonical original declaration JSON, without resolved credential bytes.
	pub config_json:  Bytes,
	/// Secret-typed dynamic values exposed only during transport construction.
	pub values:       ResolvedTransportValues,
	/// Optional live combined-authority header lease for HTTP-like transports.
	pub auth_headers: Option<Arc<dyn RefreshableHeaders>>,
}

/// One sanitized startup-race observation.
#[derive(Clone, Debug)]
pub struct StartupSnapshot {
	/// Deterministically ordered server status after the startup race.
	pub status:    pb::McpStatusResult,
	/// Whether every initial connection completed before 250 ms.
	pub completed: bool,
}

/// Connected initialized client returned by a transport connector.
pub struct ConnectedClient {
	pub(crate) client:      Arc<McpClient>,
	pub(crate) initialized: InitializedServer,
}

/// Cold transport-construction boundary used by the supervisor.
pub trait McpConnector: Send + Sync {
	/// Connects and initializes one server.
	fn connect<'a>(
		&'a self,
		spec: &'a MountSpec,
		roots: Arc<[Str]>,
		cancel: CancellationToken,
	) -> Pin<Box<dyn Future<Output = Result<ConnectedClient, ManagerError>> + Send + 'a>>;
}

/// Combined-authority hook for tool-level `mcp/www_authenticate` challenges.
pub trait McpAuthChallengeHandler: Send + Sync {
	/// Refreshes the credential lease named by one server response.
	fn refresh<'a>(
		&'a self,
		server: &'a str,
		challenges: &'a [Str],
		cancel: CancellationToken,
	) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>>;
}

/// Production connector for stdio, Streamable HTTP, and legacy HTTP+SSE.
pub struct ProductionConnector {
	workspace_root: PathBuf,
	http:           Arc<WreqExchange>,
}

impl ProductionConnector {
	/// Creates a connector whose relative stdio paths belong to this
	/// Environment.
	pub fn new(workspace_root: PathBuf) -> Self {
		Self { workspace_root, http: Arc::new(WreqExchange::new()) }
	}
}

impl McpConnector for ProductionConnector {
	fn connect<'a>(
		&'a self,
		spec: &'a MountSpec,
		roots: Arc<[Str]>,
		cancel: CancellationToken,
	) -> Pin<Box<dyn Future<Output = Result<ConnectedClient, ManagerError>> + Send + 'a>> {
		Box::pin(async move {
			let request_id_format = match spec.config.request_id_format.unwrap_or_default() {
				ConfigRequestIdFormat::Number => RequestIdFormat::Number,
				ConfigRequestIdFormat::String => RequestIdFormat::String,
			};
			let timeout = spec.config.timeout.map(Duration::from_millis);
			let transport: Arc<dyn McpTransport> = match spec.config.resolved_transport() {
				TransportKind::Stdio => {
					let command = spec
						.config
						.command
						.as_ref()
						.ok_or(ManagerError::InvalidConfig)?;
					let cwd = spec.config.cwd.as_ref().map_or_else(
						|| self.workspace_root.clone(),
						|cwd| {
							if cwd.is_absolute() {
								cwd.clone()
							} else {
								self.workspace_root.join(cwd)
							}
						},
					);
					let env = expose_env(&spec.values.env);
					Arc::new(
						StdioTransport::spawn(StdioConfig {
							command: PathBuf::from(command.as_str()),
							args: spec.config.args.clone(),
							env,
							cwd,
							timeout,
							request_id_format,
						})
						.await?,
					)
				},
				TransportKind::Http => {
					let url = parse_url(&spec.config)?;
					let headers = expose_headers(&spec.values.headers)?;
					Arc::new(StreamableHttpTransport::new(
						StreamableHttpConfig {
							url,
							headers,
							origin_locked: spec.config.header_policy.is_some(),
							timeout,
							request_id_format,
							auth: spec.auth_headers.clone(),
						},
						self.http.clone(),
					)?)
				},
				TransportKind::Sse => {
					let url = parse_url(&spec.config)?;
					let headers = expose_headers(&spec.values.headers)?;
					Arc::new(
						LegacySseTransport::connect(
							LegacySseConfig {
								url,
								headers,
								origin_locked: spec.config.header_policy.is_some(),
								timeout,
								request_id_format,
								auth: spec.auth_headers.clone(),
							},
							self.http.clone(),
							cancel.child_token(),
						)
						.await?,
					)
				},
			};
			let client = Arc::new(McpClient::new(transport, roots));
			let initialized = client.initialize(cancel).await?;
			Ok(ConnectedClient { client, initialized })
		})
	}
}

fn expose_env(values: &BTreeMap<Str, ResolvedConfigValue>) -> BTreeMap<Str, OsString> {
	values
		.iter()
		.map(|(name, value)| {
			let exposed = value.with_exposed(|text| OsString::from(text));
			(name.clone(), exposed)
		})
		.collect()
}

fn expose_headers(values: &BTreeMap<Str, ResolvedConfigValue>) -> Result<HeaderMap, ManagerError> {
	let mut headers = HeaderMap::new();
	for (name, value) in values {
		let name =
			HeaderName::from_bytes(name.as_bytes()).map_err(|_| ManagerError::InvalidConfig)?;
		let value = value
			.with_exposed(HeaderValue::from_str)
			.map_err(|_| ManagerError::InvalidConfig)?;
		headers.insert(name, value);
	}
	Ok(headers)
}

fn parse_url(config: &McpServerConfig) -> Result<Url, ManagerError> {
	let raw = config.url.as_deref().ok_or(ManagerError::InvalidConfig)?;
	Url::parse(raw).map_err(|_| ManagerError::InvalidConfig)
}

pub(crate) struct LiveConnection {
	pub(crate) client:      Arc<McpClient>,
	pub(crate) initialized: InitializedServer,
	tools:                  RwLock<Arc<[Value]>>,
	resources:              RwLock<Arc<[ResourceDefinition]>>,
	templates:              RwLock<Arc<[ResourceTemplate]>>,
	prompts:                RwLock<Arc<[PromptDefinition]>>,
}

struct MountState {
	spec:               MountSpec,
	generation:         u64,
	definition_version: u64,
	connection:         Option<Arc<LiveConnection>>,
	connecting:         bool,
	reconnecting:       bool,
	terminal_failure:   bool,
	reconnects:         VecDeque<Instant>,
	tools:              Arc<[Value]>,
}

struct ManagerState {
	mounts: BTreeMap<Str, MountState>,
}

struct SubscriptionState {
	enabled: bool,
	epoch:   u64,
	active:  BTreeMap<Str, BTreeSet<Str>>,
}

/// Environment-owned multiprocess MCP supervisor.
pub struct McpManager {
	service:       Arc<McpService>,
	connector:     Arc<dyn McpConnector>,
	workspace:     Arc<[Str]>,
	local_root:    PathBuf,
	state:         Mutex<ManagerState>,
	subscriptions: Mutex<SubscriptionState>,
	auth:          RwLock<Option<Arc<dyn McpAuthChallengeHandler>>>,
	changed:       tokio::sync::Notify,
	shutdown:      CancellationToken,
	generation:    std::sync::atomic::AtomicU64,
	sequence:      std::sync::atomic::AtomicU64,
}

impl McpManager {
	/// Creates an Environment-scoped supervisor. Call [`Self::start`] to mount a
	/// complete resolved declaration set.
	pub fn new(
		service: Arc<McpService>,
		connector: Arc<dyn McpConnector>,
		workspace: Arc<[Str]>,
		local_root: PathBuf,
	) -> Arc<Self> {
		Arc::new(Self {
			service,
			connector,
			workspace,
			local_root,
			state: Mutex::new(ManagerState { mounts: BTreeMap::new() }),
			subscriptions: Mutex::new(SubscriptionState {
				enabled: false,
				epoch:   0,
				active:  BTreeMap::new(),
			}),
			auth: RwLock::new(None),
			changed: tokio::sync::Notify::new(),
			shutdown: CancellationToken::new(),
			generation: std::sync::atomic::AtomicU64::new(1),
			sequence: std::sync::atomic::AtomicU64::new(1),
		})
	}

	/// Binds the combined credential authority's reactive challenge hook.
	pub fn bind_auth_handler(&self, handler: Arc<dyn McpAuthChallengeHandler>) {
		*self.auth.write() = Some(handler);
	}

	/// Starts all declarations in parallel, waits at most 250 ms, and leaves
	/// unfinished cache/connect work running in the background.
	pub async fn start(self: &Arc<Self>, specs: Vec<MountSpec>) -> StartupSnapshot {
		let requested = specs
			.iter()
			.map(|spec| spec.name.clone())
			.collect::<BTreeSet<_>>();
		let mounted = self
			.state
			.lock()
			.mounts
			.keys()
			.filter(|name| !requested.contains(*name))
			.cloned()
			.collect::<Vec<_>>();
		for name in mounted {
			let _ = self.unmount(&name).await;
		}
		let mut completions = FuturesUnordered::new();
		for spec in specs {
			if self.state.lock().mounts.contains_key(&spec.name) {
				let _ = self.unmount(&spec.name).await;
			}
			let name = spec.name.clone();
			let generation = self.next_generation();
			self.install_mount(spec, generation);
			let cache_manager = Arc::clone(self);
			let cache_name = name.clone();
			tokio::spawn(async move {
				cache_manager.publish_cached(cache_name, generation).await;
			});
			let manager = Arc::clone(self);
			completions.push(tokio::spawn(async move {
				let _ = manager.connect_initial(name, generation).await;
			}));
		}

		let mut completed = true;
		let deadline = tokio::time::sleep(STARTUP_RACE);
		tokio::pin!(deadline);
		while !completions.is_empty() {
			tokio::select! {
				biased;
				_ = &mut deadline => {
					completed = false;
					break;
				},
				Some(_) = completions.next() => {},
			}
		}
		StartupSnapshot { status: self.service.status(None), completed }
	}

	/// Atomically refreshes live mounts from the Environment configuration
	/// authority after a successful mutation.
	pub async fn replace_config_entries(self: &Arc<Self>, entries: Vec<pb::McpConfigEntry>) {
		let mut declarations = BTreeMap::new();
		for entry in entries {
			if declarations.contains_key(entry.name.as_str()) {
				continue;
			}
			let Ok(config) = serde_json::from_slice::<McpServerConfig>(&entry.server_json) else {
				tracing::warn!(
					server = %entry.name,
					"MCP config refresh skipped an invalid declaration"
				);
				continue;
			};
			if !config.enabled {
				continue;
			}
			let values = ResolvedTransportValues {
				env:     config
					.env
					.iter()
					.map(|(name, value)| (name.clone(), ResolvedConfigValue::Public(value.clone())))
					.collect(),
				headers: config
					.headers
					.iter()
					.map(|(name, value)| (name.clone(), ResolvedConfigValue::Public(value.clone())))
					.collect(),
			};
			declarations.insert(Str::from(entry.name.as_str()), MountSpec {
				name: Str::from(entry.name),
				config: Arc::new(config),
				config_json: entry.server_json,
				values,
				auth_headers: None,
			});
		}
		let _ = self.start(declarations.into_values().collect()).await;
	}

	/// Enables or disables exact resource subscriptions across every live
	/// server. Reconnects replay the desired set and stale completions roll
	/// themselves back.
	pub fn set_notifications_enabled(self: &Arc<Self>, enabled: bool) {
		let connections = {
			let mut subscriptions = self.subscriptions.lock();
			if subscriptions.enabled == enabled {
				return;
			}
			subscriptions.enabled = enabled;
			subscriptions.epoch = subscriptions.epoch.saturating_add(1);
			self
				.state
				.lock()
				.mounts
				.iter()
				.filter_map(|(name, mount)| {
					mount
						.connection
						.as_ref()
						.map(|connection| (name.clone(), Arc::clone(connection)))
				})
				.collect::<Vec<_>>()
		};
		for (name, connection) in connections {
			let manager = Arc::clone(self);
			tokio::spawn(async move {
				manager.sync_subscriptions(name, connection).await;
			});
		}
	}

	/// Mounts one declaration without replacing unrelated servers.
	pub async fn mount(self: &Arc<Self>, spec: MountSpec) -> pb::McpServerStatus {
		let name = spec.name.clone();
		if self.state.lock().mounts.contains_key(&name) {
			let _ = self.unmount(&name).await;
		}
		let generation = self.next_generation();
		self.install_mount(spec, generation);
		let cache_manager = Arc::clone(self);
		let cache_name = name.clone();
		tokio::spawn(async move {
			cache_manager.publish_cached(cache_name, generation).await;
		});
		let manager = Arc::clone(self);
		let connect_name = name.clone();
		let completion = tokio::spawn(async move {
			let _ = manager.connect_initial(connect_name, generation).await;
		});
		tokio::select! {
			biased;
			_ = completion => {},
			() = tokio::time::sleep(STARTUP_RACE) => {},
		}
		self.status_for(&name)
	}

	/// Unmounts one server, closes its live transport, and removes its current
	/// leaves in one owner-fenced publication.
	pub async fn unmount(&self, name: &str) -> Result<bool, ManagerError> {
		let removed = self.state.lock().mounts.remove(name);
		let Some(removed) = removed else {
			return Ok(false);
		};
		self.subscriptions.lock().active.remove(name);
		if let Some(connection) = removed.connection {
			let _ = connection.client.transport().close().await;
		}
		let owner = leaf_owner(name);
		self.service.replace_leaves(
			owner,
			LeafVersion {
				manager_generation: removed.generation,
				definition_epoch:   removed.definition_version.saturating_add(1),
			},
			Vec::new(),
		)?;
		let server = pb::McpServerRef {
			name:             name.to_owned(),
			definition_epoch: self.service.definition_epoch(),
		};
		let _ = self.service.remove(&server);
		self.changed.notify_waiters();
		Ok(true)
	}

	/// Returns deterministic server inventory from the shared Environment owner.
	pub fn servers(&self) -> pb::McpStatusResult {
		self.service.status(None)
	}

	/// Returns the immutable current MCP definition catalog for CONTROL and dyn
	/// registry epoch consumers.
	pub fn catalog_snapshot(&self) -> omp_tool::LeafCatalogSnapshot<super::McpLeaf> {
		self.service.leaf_snapshot()
	}

	/// Invokes a CONTROL-originated MCP tool through the same receipt-bearing
	/// bridge used by Environment RPC.
	pub async fn control_invoke(
		self: &Arc<Self>,
		server: &str,
		tool: &str,
		arguments: Value,
		cancel: CancellationToken,
	) -> Result<pb::McpInvokeResult, McpServiceError> {
		let arguments_json =
			serde_json::to_vec(&arguments).map_err(|_| McpServiceError::InvalidRequest)?;
		invoke::invoke(
			Arc::clone(self),
			pb::McpInvokeRequest {
				server:         Some(pb::McpServerRef {
					name:             server.to_owned(),
					definition_epoch: self.service.definition_epoch(),
				}),
				tool:           tool.to_owned(),
				arguments_json: arguments_json.into(),
				timeout_ms:     0,
				max_bytes:      8 * 1024 * 1024,
				wire_revision:  omp_proto::SCHEMA_REV,
			},
			cancel,
		)
		.await
	}

	/// Performs a manual reconnect, clearing the burst circuit breaker.
	pub async fn reset(self: &Arc<Self>, name: &str) -> Result<(), ManagerError> {
		self.reconnect(name, true).await.map(|_| ())
	}

	pub(crate) fn local_root(&self) -> &Path {
		&self.local_root
	}

	pub(crate) fn mount_timeout(&self, name: &str) -> Option<u64> {
		self
			.state
			.lock()
			.mounts
			.get(name)
			.and_then(|mount| mount.spec.config.timeout)
	}

	pub(crate) fn tool_definition(&self, name: &str, tool: &str) -> Option<Value> {
		self
			.state
			.lock()
			.mounts
			.get(name)?
			.tools
			.iter()
			.find(|definition| definition.get("name").and_then(Value::as_str) == Some(tool))
			.cloned()
	}

	pub(crate) async fn connection(
		&self,
		name: &str,
		cancel: &CancellationToken,
	) -> Result<Arc<LiveConnection>, ManagerError> {
		loop {
			let notified = self.changed.notified();
			{
				let state = self.state.lock();
				let mount = state.mounts.get(name).ok_or(ManagerError::ServerNotFound)?;
				if let Some(connection) = &mount.connection {
					return Ok(Arc::clone(connection));
				}
				if mount.terminal_failure && !mount.connecting && !mount.reconnecting {
					return Err(ManagerError::ConnectionUnavailable);
				}
			}
			tokio::select! {
				biased;
				() = cancel.cancelled() => return Err(ManagerError::Cancelled),
				() = notified => {},
			}
		}
	}

	pub(crate) async fn reconnect_for_invoke(
		self: &Arc<Self>,
		name: &str,
	) -> Result<Arc<LiveConnection>, ManagerError> {
		self.reconnect(name, false).await
	}

	pub(crate) async fn refresh_auth(
		&self,
		name: &str,
		challenges: &[Str],
		cancel: CancellationToken,
	) -> bool {
		let handler = self.auth.read().clone();
		match handler {
			Some(handler) => handler.refresh(name, challenges, cancel).await,
			None => false,
		}
	}

	fn install_mount(self: &Arc<Self>, spec: MountSpec, generation: u64) {
		let name = spec.name.clone();
		let backend: Arc<dyn McpServerBackend> =
			Arc::new(ManagedBackend { manager: Arc::downgrade(self), name: name.clone() });
		self.state.lock().mounts.insert(name.clone(), MountState {
			spec,
			generation,
			definition_version: 0,
			connection: None,
			connecting: true,
			reconnecting: false,
			terminal_failure: false,
			reconnects: VecDeque::new(),
			tools: Arc::from([]),
		});
		let _ = self.service.install(
			status(
				&name,
				pb::McpLifecycleState::Starting,
				generation,
				self.service.definition_epoch(),
				"",
			),
			backend,
		);
	}

	async fn publish_cached(self: Arc<Self>, name: Str, generation: u64) {
		let (cache, spec) = {
			let state = self.state.lock();
			let Some(mount) = state.mounts.get(&name) else {
				return;
			};
			(Arc::clone(self.service.cache()), mount.spec.clone())
		};
		let cache_name = name.clone();
		let config_json = spec.config_json.clone();
		let loaded =
			tokio::task::spawn_blocking(move || cache.get(&cache_name, &config_json, now_ms())).await;
		let Ok(Ok(Some(cached))) = loaded else {
			return;
		};
		let Ok(tools) = serde_json::from_slice::<Vec<Value>>(&cached.definitions_json) else {
			return;
		};
		{
			let mut state = self.state.lock();
			let Some(mount) = state.mounts.get_mut(&name) else {
				return;
			};
			if mount.generation != generation || mount.connection.is_some() {
				return;
			}
			mount.tools = Arc::from(tools.clone());
		}
		if self
			.publish_definitions(&name, generation, tools, Vec::new(), Vec::new(), Vec::new(), None)
			.is_ok()
		{
			self.publish_status(
				&name,
				generation,
				pb::McpLifecycleState::Degraded,
				"cached definitions; connection pending",
			);
		}
	}

	async fn connect_initial(
		self: Arc<Self>,
		name: Str,
		generation: u64,
	) -> Result<Arc<LiveConnection>, ManagerError> {
		let result = self.connect_once(&name, generation).await;
		{
			let mut state = self.state.lock();
			if let Some(mount) = state.mounts.get_mut(&name)
				&& mount.generation == generation
			{
				mount.connecting = false;
				mount.terminal_failure = result.is_err();
			}
		}
		if result.is_err() {
			self.publish_status(&name, generation, pb::McpLifecycleState::Failed, "connection failed");
		}
		self.changed.notify_waiters();
		result
	}

	async fn connect_once(
		self: &Arc<Self>,
		name: &str,
		generation: u64,
	) -> Result<Arc<LiveConnection>, ManagerError> {
		let spec = {
			let state = self.state.lock();
			let mount = state.mounts.get(name).ok_or(ManagerError::ServerNotFound)?;
			if mount.generation != generation {
				return Err(ManagerError::StaleGeneration);
			}
			mount.spec.clone()
		};
		let connected = self
			.connector
			.connect(&spec, Arc::clone(&self.workspace), self.shutdown.child_token())
			.await?;
		let tools = list_tools(connected.client.transport(), self.shutdown.child_token()).await?;
		let supports_resources = connected
			.initialized
			.capabilities
			.get("resources")
			.is_some();
		let supports_prompts = connected.initialized.capabilities.get("prompts").is_some();
		let (resources, templates) = if supports_resources {
			let client = ResourcesClient::new(Arc::clone(connected.client.transport()));
			let resources = client
				.list(self.shutdown.child_token())
				.await
				.unwrap_or_default();
			let templates = client
				.templates(self.shutdown.child_token())
				.await
				.unwrap_or_default();
			(resources, templates)
		} else {
			(Vec::new(), Vec::new())
		};
		let prompts = if supports_prompts {
			PromptsClient::new(Arc::clone(connected.client.transport()))
				.list(self.shutdown.child_token())
				.await
				.unwrap_or_default()
		} else {
			Vec::new()
		};
		let instructions = bounded_instructions(connected.initialized.instructions.as_ref());
		let connection = Arc::new(LiveConnection {
			client:      connected.client,
			initialized: connected.initialized,
			tools:       RwLock::new(Arc::from(tools.clone())),
			resources:   RwLock::new(Arc::from(resources.clone())),
			templates:   RwLock::new(Arc::from(templates.clone())),
			prompts:     RwLock::new(Arc::from(prompts.clone())),
		});
		let stale = {
			let mut state = self.state.lock();
			let mount = state
				.mounts
				.get_mut(name)
				.ok_or(ManagerError::ServerNotFound)?;
			if mount.generation != generation {
				true
			} else {
				mount.connection = Some(Arc::clone(&connection));
				mount.tools = Arc::from(tools.clone());
				false
			}
		};
		if stale {
			let _ = connection.client.transport().close().await;
			return Err(ManagerError::StaleGeneration);
		}
		self.publish_definitions(
			name,
			generation,
			tools.clone(),
			resources,
			templates,
			prompts,
			instructions,
		)?;
		let cache = Arc::clone(self.service.cache());
		let cache_name = Str::from(name);
		let config_json = spec.config_json;
		if let Ok(definitions_json) = serde_json::to_vec(&tools) {
			tokio::task::spawn_blocking(move || {
				let _ = cache.put(&cache_name, &config_json, &definitions_json, now_ms());
			});
		}
		self.publish_status(name, generation, pb::McpLifecycleState::Ready, "");
		self.changed.notify_waiters();
		self.spawn_message_loop(Str::from(name), generation, Arc::clone(&connection));
		let subscriptions = Arc::clone(self);
		let subscription_name = Str::from(name);
		let subscription_connection = Arc::clone(&connection);
		tokio::spawn(async move {
			subscriptions
				.sync_subscriptions(subscription_name, subscription_connection)
				.await;
		});
		Ok(connection)
	}

	fn publish_definitions(
		&self,
		name: &str,
		generation: u64,
		tools: Vec<Value>,
		resources: Vec<ResourceDefinition>,
		templates: Vec<ResourceTemplate>,
		prompts: Vec<PromptDefinition>,
		instructions: Option<Str>,
	) -> Result<u64, ManagerError> {
		let (definition_version, protocol_version) = {
			let mut state = self.state.lock();
			let mount = state
				.mounts
				.get_mut(name)
				.ok_or(ManagerError::ServerNotFound)?;
			if mount.generation != generation {
				return Err(ManagerError::StaleGeneration);
			}
			mount.definition_version = mount.definition_version.saturating_add(1);
			let protocol_version = mount
				.connection
				.as_ref()
				.map_or("2025-11-25", |connection| connection.initialized.protocol_version.as_str());
			(mount.definition_version, Str::from(protocol_version))
		};
		let leaves = McpDeviceDefinitions {
			server: Str::from(name),
			tools,
			resources,
			templates,
			prompts,
			instructions,
		}
		.into_leaves(&protocol_version)?;
		Ok(self.service.replace_leaves(
			leaf_owner(name),
			LeafVersion { manager_generation: generation, definition_epoch: definition_version },
			leaves,
		)?)
	}

	fn spawn_message_loop(
		self: &Arc<Self>,
		name: Str,
		generation: u64,
		connection: Arc<LiveConnection>,
	) {
		let manager = Arc::downgrade(self);
		let shutdown = self.shutdown.clone();
		tokio::spawn(async move {
			loop {
				let message = connection.client.next(shutdown.child_token()).await;
				let Some(manager) = manager.upgrade() else {
					return;
				};
				match message {
					Ok(Some((method, params))) => {
						manager
							.handle_notification(&name, generation, &connection, &method, params)
							.await;
					},
					Ok(None) | Err(_) => break,
				}
				drop(manager);
			}
			let Some(manager) = manager.upgrade() else {
				return;
			};
			if manager.is_current_connection(&name, generation, &connection) {
				let _ = manager.reconnect(&name, false).await;
			}
		});
	}

	async fn handle_notification(
		&self,
		name: &str,
		generation: u64,
		connection: &Arc<LiveConnection>,
		method: &str,
		params: Value,
	) {
		if !self.is_current_connection(name, generation, connection) {
			return;
		}
		let refresh = match method {
			"notifications/tools/list_changed" => Some(RefreshKind::Tools),
			"notifications/resources/list_changed" => Some(RefreshKind::Resources),
			"notifications/prompts/list_changed" => Some(RefreshKind::Prompts),
			_ => None,
		};
		if let Some(refresh) = refresh {
			let _ = self.refresh_definitions(name, generation, refresh).await;
		}
		let sequence = self
			.sequence
			.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
		let params_json = serde_json::to_vec(&params).unwrap_or_else(|_| b"null".to_vec());
		let _ = self.service.notify(pb::McpNotification {
			server: Some(pb::McpServerRef {
				name:             name.to_owned(),
				definition_epoch: self.service.definition_epoch(),
			}),
			sequence,
			method: method.to_owned(),
			params_json: params_json.into(),
		});
	}

	async fn refresh_definitions(
		&self,
		name: &str,
		generation: u64,
		kind: RefreshKind,
	) -> Result<(), ManagerError> {
		let connection = {
			let state = self.state.lock();
			let mount = state.mounts.get(name).ok_or(ManagerError::ServerNotFound)?;
			if mount.generation != generation {
				return Err(ManagerError::StaleGeneration);
			}
			mount
				.connection
				.clone()
				.ok_or(ManagerError::ConnectionUnavailable)?
		};
		let mut tools = connection.tools.read().to_vec();
		let mut resources = connection.resources.read().to_vec();
		let mut templates = connection.templates.read().to_vec();
		let mut prompts = connection.prompts.read().to_vec();
		match kind {
			RefreshKind::Tools => {
				tools = list_tools(connection.client.transport(), self.shutdown.child_token()).await?;
			},
			RefreshKind::Resources => {
				let client = ResourcesClient::new(Arc::clone(connection.client.transport()));
				resources = client.list(self.shutdown.child_token()).await?;
				templates = client.templates(self.shutdown.child_token()).await?;
			},
			RefreshKind::Prompts => {
				prompts = PromptsClient::new(Arc::clone(connection.client.transport()))
					.list(self.shutdown.child_token())
					.await?;
			},
		}
		self.publish_definitions(
			name,
			generation,
			tools.clone(),
			resources.clone(),
			templates.clone(),
			prompts.clone(),
			bounded_instructions(connection.initialized.instructions.as_ref()),
		)?;
		*connection.tools.write() = Arc::from(tools.clone());
		*connection.resources.write() = Arc::from(resources);
		*connection.templates.write() = Arc::from(templates);
		*connection.prompts.write() = Arc::from(prompts);
		if matches!(kind, RefreshKind::Tools) {
			if let Some(mount) = self.state.lock().mounts.get_mut(name)
				&& mount.generation == generation
			{
				mount.tools = Arc::from(tools);
			}
		} else if matches!(kind, RefreshKind::Resources) {
			self.sync_subscriptions(Str::from(name), connection).await;
		}
		Ok(())
	}

	async fn sync_subscriptions(&self, name: Str, connection: Arc<LiveConnection>) {
		let supports = connection
			.initialized
			.capabilities
			.get("resources")
			.and_then(Value::as_object)
			.and_then(|resources| resources.get("subscribe"))
			.and_then(Value::as_bool)
			.unwrap_or(false);
		let (globally_enabled, epoch, current) = {
			let subscriptions = self.subscriptions.lock();
			(
				subscriptions.enabled,
				subscriptions.epoch,
				subscriptions.active.get(&name).cloned().unwrap_or_default(),
			)
		};
		let enabled = globally_enabled && supports;
		let desired = if enabled {
			connection
				.resources
				.read()
				.iter()
				.map(|resource| resource.uri.clone())
				.collect::<BTreeSet<_>>()
		} else {
			BTreeSet::new()
		};
		let client = ResourcesClient::new(Arc::clone(connection.client.transport()));
		for uri in current.difference(&desired) {
			if client
				.unsubscribe(uri, self.shutdown.child_token())
				.await
				.is_err()
			{
				return;
			}
		}
		let mut added: Vec<Str> = Vec::new();
		for uri in desired.difference(&current) {
			if client
				.subscribe(uri, self.shutdown.child_token())
				.await
				.is_err()
			{
				for rollback in added {
					let _ = client
						.unsubscribe(&rollback, self.shutdown.child_token())
						.await;
				}
				return;
			}
			added.push(uri.clone());
		}
		let stale = {
			let mut subscriptions = self.subscriptions.lock();
			if subscriptions.epoch != epoch || subscriptions.enabled != globally_enabled {
				true
			} else {
				if desired.is_empty() {
					subscriptions.active.remove(&name);
				} else {
					subscriptions.active.insert(name.clone(), desired);
				}
				false
			}
		};
		if stale {
			for uri in added {
				let _ = client.unsubscribe(&uri, self.shutdown.child_token()).await;
			}
		}
	}

	async fn reconnect(
		self: &Arc<Self>,
		name: &str,
		manual: bool,
	) -> Result<Arc<LiveConnection>, ManagerError> {
		loop {
			let notified = self.changed.notified();
			let decision = {
				let mut state = self.state.lock();
				let mount = state
					.mounts
					.get_mut(name)
					.ok_or(ManagerError::ServerNotFound)?;
				if manual {
					mount.reconnects.clear();
				}
				if mount.reconnecting {
					ReconnectStart::Wait
				} else {
					let now = Instant::now();
					while mount
						.reconnects
						.front()
						.is_some_and(|attempt| now.duration_since(*attempt) >= RECONNECT_WINDOW)
					{
						mount.reconnects.pop_front();
					}
					mount.reconnects.push_back(now);
					if mount.reconnects.len() > RECONNECT_BURST_LIMIT {
						mount.connection = None;
						mount.terminal_failure = true;
						ReconnectStart::CircuitOpen(mount.generation)
					} else {
						mount.reconnecting = true;
						mount.terminal_failure = false;
						ReconnectStart::Begin(mount.generation, mount.connection.take())
					}
				}
			};
			let (generation, stale) = match decision {
				ReconnectStart::Wait => {
					notified.await;
					continue;
				},
				ReconnectStart::CircuitOpen(generation) => {
					self.publish_status(
						name,
						generation,
						pb::McpLifecycleState::Failed,
						"automatic reconnect suspended",
					);
					return Err(ManagerError::CircuitOpen);
				},
				ReconnectStart::Begin(generation, stale) => (generation, stale),
			};
			if let Some(stale) = stale {
				let _ = stale.client.transport().close().await;
			}
			self.publish_status(name, generation, pb::McpLifecycleState::Starting, "reconnecting");
			let mut result = self.connect_once(name, generation).await;
			for delay in RECONNECT_DELAYS {
				if result.is_ok() {
					break;
				}
				tokio::select! {
					biased;
					() = self.shutdown.cancelled() => {
						result = Err(ManagerError::Cancelled);
						break;
					},
					() = tokio::time::sleep(delay) => {},
				}
				result = self.connect_once(name, generation).await;
			}
			{
				let mut state = self.state.lock();
				if let Some(mount) = state.mounts.get_mut(name)
					&& mount.generation == generation
				{
					mount.reconnecting = false;
					mount.terminal_failure = result.is_err();
				}
			}
			if result.is_err() {
				self.publish_status(
					name,
					generation,
					pb::McpLifecycleState::Failed,
					"reconnect failed",
				);
			}
			self.changed.notify_waiters();
			return result;
		}
	}

	fn is_current_connection(
		&self,
		name: &str,
		generation: u64,
		connection: &Arc<LiveConnection>,
	) -> bool {
		self.state.lock().mounts.get(name).is_some_and(|mount| {
			mount.generation == generation
				&& mount
					.connection
					.as_ref()
					.is_some_and(|current| Arc::ptr_eq(current, connection))
		})
	}

	fn publish_status(
		&self,
		name: &str,
		generation: u64,
		state: pb::McpLifecycleState,
		detail: &str,
	) {
		let backend = self
			.service
			.backend_for_manager(name)
			.unwrap_or_else(|| Arc::new(UnavailableBackend));
		let _ = self.service.install(
			status(name, state, generation, self.service.definition_epoch(), detail),
			backend,
		);
	}

	fn status_for(&self, name: &str) -> pb::McpServerStatus {
		self
			.service
			.status(Some(name))
			.servers
			.into_iter()
			.next()
			.unwrap_or_else(|| {
				status(name, pb::McpLifecycleState::Stopped, 0, self.service.definition_epoch(), "")
			})
	}

	fn next_generation(&self) -> u64 {
		self
			.generation
			.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
	}
}

impl Drop for McpManager {
	fn drop(&mut self) {
		self.shutdown.cancel();
	}
}

#[derive(Clone, Copy)]
enum RefreshKind {
	Tools,
	Resources,
	Prompts,
}

enum ReconnectStart {
	Wait,
	CircuitOpen(u64),
	Begin(u64, Option<Arc<LiveConnection>>),
}

fn leaf_owner(name: &str) -> LeafOwner {
	LeafOwner { root: Str::from(name), claimant: Str::new_static("mcp") }
}

fn status(
	name: &str,
	state: pb::McpLifecycleState,
	generation: u64,
	definition_epoch: u64,
	detail: &str,
) -> pb::McpServerStatus {
	pb::McpServerStatus {
		server: Some(pb::McpServerRef { name: name.to_owned(), definition_epoch }),
		state: state.into(),
		detail: detail.to_owned(),
		generation,
		definition_epoch,
	}
}

async fn list_tools(
	transport: &Arc<dyn McpTransport>,
	cancel: CancellationToken,
) -> Result<Vec<Value>, ManagerError> {
	let mut output = Vec::new();
	let mut cursor: Option<Str> = None;
	let mut seen = std::collections::BTreeSet::new();
	for _ in 0..MAX_TOOL_PAGES {
		let params = cursor
			.as_ref()
			.map_or_else(|| json!({}), |cursor| json!({ "cursor": cursor }));
		let response = transport
			.request("tools/list", params, cancel.child_token())
			.await?;
		let mut object = response
			.result
			.as_object()
			.cloned()
			.ok_or(ManagerError::MalformedDefinitions)?;
		let tools = object
			.remove("tools")
			.ok_or(ManagerError::MalformedDefinitions)?;
		output.extend(
			serde_json::from_value::<Vec<Value>>(tools)
				.map_err(|_| ManagerError::MalformedDefinitions)?,
		);
		cursor = object.remove("nextCursor").and_then(|value| {
			value
				.as_str()
				.filter(|value| !value.is_empty())
				.map(Str::from)
		});
		let Some(next) = cursor.as_ref() else {
			output.sort_unstable_by(|left, right| {
				left
					.get("name")
					.and_then(Value::as_str)
					.cmp(&right.get("name").and_then(Value::as_str))
			});
			return Ok(output);
		};
		if !seen.insert(next.clone()) {
			return Err(ManagerError::MalformedDefinitions);
		}
	}
	Err(ManagerError::MalformedDefinitions)
}

fn bounded_instructions(instructions: Option<&Str>) -> Option<Str> {
	instructions.map(|instructions| {
		let end = floor_char_boundary(instructions, MAX_INSTRUCTIONS_BYTES);
		instructions.slice(..end)
	})
}

fn floor_char_boundary(value: &str, limit: usize) -> usize {
	let mut end = value.len().min(limit);
	while !value.is_char_boundary(end) {
		end -= 1;
	}
	end
}

fn now_ms() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map_or(0, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
}

struct ManagedBackend {
	manager: Weak<McpManager>,
	name:    Str,
}

impl McpServerBackend for ManagedBackend {
	fn reset(&self, cancel: CancellationToken) -> BoxFuture<'_, Result<(), McpServiceError>> {
		Box::pin(async move {
			let manager = self.manager.upgrade().ok_or(McpServiceError::Backend)?;
			tokio::select! {
				biased;
				() = cancel.cancelled() => Err(McpServiceError::Cancelled),
				result = manager.reset(&self.name) => result.map_err(|_| McpServiceError::Backend),
			}
		})
	}

	fn live_header(
		&self,
		request: pb::McpLiveHeaderRequest,
		_cancel: CancellationToken,
	) -> BoxFuture<'_, Result<pb::McpLiveHeader, McpServiceError>> {
		Box::pin(async move {
			Ok(pb::McpLiveHeader {
				server:        request.server,
				headers:       Vec::new(),
				expires_at_ms: 0,
			})
		})
	}

	fn resource(
		&self,
		request: pb::McpResourceRequest,
		cancel: CancellationToken,
	) -> BoxFuture<'_, Result<pb::McpResourceResult, McpServiceError>> {
		Box::pin(async move {
			let manager = self.manager.upgrade().ok_or(McpServiceError::Backend)?;
			let connection = manager
				.connection(&self.name, &cancel)
				.await
				.map_err(manager_service_error)?;
			let contents = ResourcesClient::new(Arc::clone(connection.client.transport()))
				.read(&request.uri, cancel)
				.await
				.map_err(|_| McpServiceError::Backend)?;
			let max = usize::try_from(request.max_bytes).unwrap_or(usize::MAX);
			let mut bytes = Vec::new();
			let mut mime_type = None;
			let mut truncated = false;
			for content in contents {
				mime_type = mime_type.or(content.mime_type);
				let remaining = max.saturating_sub(bytes.len());
				if content.bytes.len() > remaining {
					bytes.extend_from_slice(&content.bytes[..remaining]);
					truncated = true;
					break;
				}
				bytes.extend_from_slice(&content.bytes);
			}
			Ok(pb::McpResourceResult {
				server: request.server,
				uri: request.uri,
				mime_type: mime_type.map_or_else(String::new, |mime| mime.to_string()),
				content: bytes.into(),
				truncated,
			})
		})
	}

	fn prompt(
		&self,
		request: pb::McpPromptRequest,
		cancel: CancellationToken,
	) -> BoxFuture<'_, Result<pb::McpPromptResult, McpServiceError>> {
		Box::pin(async move {
			let manager = self.manager.upgrade().ok_or(McpServiceError::Backend)?;
			let connection = manager
				.connection(&self.name, &cancel)
				.await
				.map_err(manager_service_error)?;
			let arguments = serde_json::from_slice::<Map<String, Value>>(&request.arguments_json)
				.map_err(|_| McpServiceError::InvalidRequest)?;
			let messages = PromptsClient::new(Arc::clone(connection.client.transport()))
				.get(&request.name, arguments, cancel)
				.await
				.map_err(|_| McpServiceError::Backend)?;
			let encoded = messages
				.into_iter()
				.map(|message| {
					let content = match message.content {
						PromptContent::Text(text) => json!({ "type": "text", "text": text }),
						PromptContent::Image { mime_type, bytes } => json!({
							"type": "image",
							"mimeType": mime_type,
							"data": omp_core::base64::encode(&bytes),
						}),
						PromptContent::Audio { mime_type, bytes } => json!({
							"type": "audio",
							"mimeType": mime_type,
							"data": omp_core::base64::encode(&bytes),
						}),
						PromptContent::Resource(resource) => json!({
							"type": "resource",
							"resource": {
								"uri": resource.uri,
								"mimeType": resource.mime_type,
								"blob": omp_core::base64::encode(&resource.bytes),
							}
						}),
					};
					json!({ "role": message.role, "content": content })
				})
				.collect::<Vec<_>>();
			let mut messages_json =
				serde_json::to_vec(&encoded).map_err(|_| McpServiceError::Backend)?;
			let max = usize::try_from(request.max_bytes).unwrap_or(usize::MAX);
			let truncated = messages_json.len() > max;
			if truncated {
				messages_json = br#"[{"role":"assistant","content":{"type":"text","text":"MCP prompt exceeded the configured size limit."}}]"#.to_vec();
			}
			Ok(pb::McpPromptResult {
				server: request.server,
				name: request.name,
				messages_json: messages_json.into(),
				truncated,
			})
		})
	}

	fn invoke(
		&self,
		request: pb::McpInvokeRequest,
		cancel: CancellationToken,
	) -> BoxFuture<'_, Result<pb::McpInvokeResult, McpServiceError>> {
		Box::pin(async move {
			let manager = self.manager.upgrade().ok_or(McpServiceError::Backend)?;
			invoke::invoke(manager, request, cancel).await
		})
	}
}

struct UnavailableBackend;
impl McpServerBackend for UnavailableBackend {
	fn reset(&self, _: CancellationToken) -> BoxFuture<'_, Result<(), McpServiceError>> {
		async { Err(McpServiceError::Backend) }.boxed()
	}

	fn live_header(
		&self,
		_: pb::McpLiveHeaderRequest,
		_: CancellationToken,
	) -> BoxFuture<'_, Result<pb::McpLiveHeader, McpServiceError>> {
		async { Err(McpServiceError::Backend) }.boxed()
	}

	fn resource(
		&self,
		_: pb::McpResourceRequest,
		_: CancellationToken,
	) -> BoxFuture<'_, Result<pb::McpResourceResult, McpServiceError>> {
		async { Err(McpServiceError::Backend) }.boxed()
	}

	fn prompt(
		&self,
		_: pb::McpPromptRequest,
		_: CancellationToken,
	) -> BoxFuture<'_, Result<pb::McpPromptResult, McpServiceError>> {
		async { Err(McpServiceError::Backend) }.boxed()
	}

	fn invoke(
		&self,
		_: pb::McpInvokeRequest,
		_: CancellationToken,
	) -> BoxFuture<'_, Result<pb::McpInvokeResult, McpServiceError>> {
		async { Err(McpServiceError::Backend) }.boxed()
	}
}

fn manager_service_error(error: ManagerError) -> McpServiceError {
	match error {
		ManagerError::Cancelled => McpServiceError::Cancelled,
		_ => McpServiceError::Backend,
	}
}

/// Lifecycle, transport, or definition publication failure.
#[derive(Debug, thiserror::Error)]
pub enum ManagerError {
	/// Declaration cannot construct the selected transport.
	#[error("MCP declaration is invalid")]
	InvalidConfig,
	/// Server is no longer mounted.
	#[error("MCP server is not mounted")]
	ServerNotFound,
	/// Requested supervisor generation was superseded.
	#[error("MCP manager generation is stale")]
	StaleGeneration,
	/// Server has no usable live connection.
	#[error("MCP server connection is unavailable")]
	ConnectionUnavailable,
	/// Automatic reconnect burst circuit is open.
	#[error("MCP automatic reconnect circuit is open")]
	CircuitOpen,
	/// Caller cancelled the operation.
	#[error("MCP manager operation was cancelled")]
	Cancelled,
	/// Tool list was malformed or exceeded pagination limits.
	#[error("MCP tool definitions are malformed")]
	MalformedDefinitions,
	/// Transport failed with dispatch evidence.
	#[error(transparent)]
	Transport(#[from] TransportError),
	/// Protocol initialization failed.
	#[error(transparent)]
	Client(#[from] ClientError),
	/// Dynamic device projection failed.
	#[error(transparent)]
	Device(#[from] DeviceError),
	/// Resource discovery failed.
	#[error(transparent)]
	Resource(#[from] ResourceError),
	/// Prompt discovery failed.
	#[error(transparent)]
	Prompt(#[from] PromptError),
	/// Revisioned leaf publication failed.
	#[error(transparent)]
	Service(#[from] McpServiceError),
}
