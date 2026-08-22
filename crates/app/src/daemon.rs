//! Production typed inference registry construction and daemon lifecycle.

use std::{
	collections::BTreeMap,
	io::{BufRead as _, IsTerminal as _},
	path::{Path, PathBuf},
	sync::Arc,
	time::Duration,
};

use omp_core::{Hash32, Str, sf};
#[cfg(target_os = "macos")]
use omp_llm_inference::auth::FallbackKeySource;
use omp_llm_inference::{
	Client, ProviderService, Registry,
	account::{
		AccountPool, AccountStateStore, AccountStateStoreError, RefreshCoordinator, RefreshPolicy,
	},
	auth::{
		AlibabaTokenPlanLoginEngine, AlibabaTokenPlanShaper, AuthLoginEngine, AuthManager,
		AuthManagerBuildError, CredentialAcquisitionLoginEngine,
		CredentialAcquisitionLoginEngineError, CredentialBroker, CredentialBrokerEngines,
		CredentialShaperRegistry, CredentialStore, FileCredentialKeySource, FileKeyError,
		GithubCopilotShaper, KeyError, KeySource, OAuthCustomDispatcher, OAuthLoginEngine,
		OAuthLoginEngineError, OsCredentialKeySource, ProviderShaper, SecretLoginEngine,
		SecretLoginEngineError, StoreError, StoredOAuthRefreshEngine, SystemOAuthClock,
		SystemOAuthHttpClient, UnavailableKeySource,
	},
	call::AuthMethod,
	codec::google_cca::{
		AntigravityFingerprint, AntigravityPolicy, CcaHeaders, DEFAULT_ANTIGRAVITY_ARCH,
		DEFAULT_ANTIGRAVITY_CL, DEFAULT_ANTIGRAVITY_OS, DEFAULT_ANTIGRAVITY_VERSION,
	},
	layer::{
		admission::AdmissionController,
		observe::{ExecutionFinished, ExecutionStarted, Observer},
		stack::BuiltinConfig,
	},
	operation::usage::{
		ConsoleUsageFetcher, ConsoleUsageManager, UsageFetcherRegistry,
		alibaba_token_plan::AlibabaTokenPlanUsageFetcher, claude::ClaudeUsageFetcher,
		cursor::CursorUsageFetcher, gemini::GeminiUsageFetcher,
		github_copilot::GithubCopilotUsageFetcher, google_antigravity::GoogleAntigravityUsageFetcher,
		kimi::KimiUsageFetcher, minimax_code::MiniMaxCodeUsageFetcher, ollama::OllamaUsageFetcher,
		openai_codex::OpenAiCodexUsageFetcher, opencode_go::OpenCodeGoUsageFetcher,
		synthetic::SyntheticUsageFetcher, umans::UmansUsageFetcher, xai_oauth::XaiOauthUsageFetcher,
		zai::ZaiUsageFetcher,
	},
	provider::builtin::{
		AuthApplicationConfig, GoogleCcaConfig, ProductionDependencies, discover_antigravity_version,
	},
	router::Router,
	session::{ConversationError, ConversationSessionPlanner},
	transport::{http::HttpTransport, websocket_transport::WebSocketTransport},
};
#[cfg(feature = "local-applefm")]
use omp_llm_inference::{ReasonId, provider::builtin::LocalRouteBackend};
use omp_proto::{
	auth::v1::auth_server::AuthServer, blob::v1::blob_server::BlobServer, control::v1 as control_pb,
	inference::v1::inference_server::InferenceServer, thread::v1::Item,
};
use omp_storage::{
	blob::BlobStore,
	transcript::{Event, Header, ItemRecord, Kind, SessionId, Writer, writer::JournalError},
};
use parking_lot::Mutex;
use tokio::{sync::watch, task::JoinHandle};
use tonic::transport::Server;

use crate::{
	auth_rpc::AuthRpc, blob_rpc::BlobRpc, endpoint::LocalEndpoint, rpc_adapter::InferenceRpc,
};

const DATA_DIR_ENV: &str = "OMP_DATA_DIR";
const KEYCHAIN_OPT_IN_ENV: &str = "OMP_LLM_KEYCHAIN";
const KEYCHAIN_SERVICE: &str = "dev.omp.llm";
const KEYCHAIN_ACCOUNT: &str = "credential-store-master";
const ANTIGRAVITY_VERSION_ENV: &str = "OMP_ANTIGRAVITY_VERSION";
const ANTIGRAVITY_CL_ENV: &str = "OMP_ANTIGRAVITY_CL";
const ANTIGRAVITY_OS_ENV: &str = "OMP_ANTIGRAVITY_OS";
const ANTIGRAVITY_ARCH_ENV: &str = "OMP_ANTIGRAVITY_ARCH";
const ANTIGRAVITY_VERSION_CACHE_FILE: &str = "antigravity-version";
const ANTIGRAVITY_VERSION_FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Daemon-owned session journal replication failure.
#[derive(Debug, thiserror::Error)]
pub enum SessionAuthorityError {
	/// Journal filesystem operation failed.
	#[error("session journal I/O failed")]
	Io(#[from] std::io::Error),
	/// Transcript append failed with a proven or indeterminate outcome.
	#[error(transparent)]
	Journal(#[from] JournalError),
	/// Transcript header, codec, or recovery validation failed.
	#[error(transparent)]
	Transcript(#[from] omp_storage::transcript::Error),
	/// An RPC addressed a different session authority.
	#[error("session RPC addressed an unknown session")]
	SessionMismatch,
	/// Structured ingestion omitted its thread item.
	#[error("session ingestion omitted its structured item")]
	MissingItem,
}

struct SessionAuthorityState {
	writer:   Writer,
	revision: u64,
}

/// Single-daemon owner for fenced session snapshots, deltas, and structured
/// ingestion.
///
/// Clients receive canonical journal bytes but can submit only typed thread
/// items. Revision checks occur while holding the same lock as the append, so
/// stale clients can never write or truncate history.
pub struct SessionJournalAuthority {
	id:    SessionId,
	path:  PathBuf,
	state: Mutex<SessionAuthorityState>,
}

impl SessionJournalAuthority {
	/// Creates a fileless authority; the first accepted ingest atomically
	/// publishes header plus event.
	pub fn create(path: impl AsRef<Path>, header: &Header) -> Result<Self, SessionAuthorityError> {
		let path = path.as_ref().to_owned();
		if let Some(parent) = path.parent() {
			std::fs::create_dir_all(parent)?;
		}
		Ok(Self {
			id:    header.id.clone(),
			path:  path.clone(),
			state: Mutex::new(SessionAuthorityState {
				writer:   Writer::create_lazy(&path, header)?,
				revision: 0,
			}),
		})
	}

	/// Opens an existing journal and restores its monotonic revision.
	pub fn open(path: impl AsRef<Path>) -> Result<Self, SessionAuthorityError> {
		let path = path.as_ref().to_owned();
		let reader = omp_storage::transcript::Reader::open(&path)?;
		let id = reader.log().header().id.clone();
		let revision = reader.next_index();
		drop(reader);
		Ok(Self {
			id,
			path: path.clone(),
			state: Mutex::new(SessionAuthorityState { writer: Writer::open_append(&path)?, revision }),
		})
	}

	/// Returns a consistent exact-byte snapshot fenced by the current revision.
	pub fn snapshot(
		&self,
		request: &control_pb::SessionSnapshotRequest,
	) -> Result<control_pb::SessionSnapshotMsg, SessionAuthorityError> {
		if request.session_id != self.id.0 {
			return Err(SessionAuthorityError::SessionMismatch);
		}
		let state = self.state.lock();
		let journal = match std::fs::read(&self.path) {
			Ok(bytes) => bytes,
			Err(source) if source.kind() == std::io::ErrorKind::NotFound && state.revision == 0 => {
				Vec::new()
			},
			Err(source) => return Err(source.into()),
		};
		let integrity = Hash32::sum(&journal).into_bytes().to_vec();
		Ok(control_pb::SessionSnapshotMsg {
			session_id: self.id.0.as_str().to_owned(),
			revision:   state.revision,
			journal:    journal.into(),
			integrity:  integrity.into(),
			props:      None,
		})
	}

	/// Returns bounded exact event lines after a client revision.
	pub fn delta(
		&self,
		request: &control_pb::SessionDeltaRequest,
	) -> Result<control_pb::SessionDeltaMsg, SessionAuthorityError> {
		if request.session_id != self.id.0 {
			return Err(SessionAuthorityError::SessionMismatch);
		}
		let state = self.state.lock();
		let head_revision = state.revision;
		if request.after_revision >= head_revision {
			return Ok(control_pb::SessionDeltaMsg {
				session_id: self.id.0.as_str().to_owned(),
				base_revision: request.after_revision.min(head_revision),
				head_revision,
				entries: Vec::new(),
				has_more: false,
				props: None,
			});
		}
		let maximum = if request.maximum_entries == 0 {
			256
		} else {
			request.maximum_entries.min(4_096)
		};
		let file = std::fs::File::open(&self.path)?;
		let mut reader = std::io::BufReader::new(file);
		let mut line = Vec::new();
		reader.read_until(b'\n', &mut line)?;
		let mut revision = 0_u64;
		let mut entries = Vec::new();
		loop {
			line.clear();
			let read = reader.read_until(b'\n', &mut line)?;
			if read == 0 {
				break;
			}
			revision = revision.saturating_add(1);
			if revision <= request.after_revision {
				continue;
			}
			if line.last() == Some(&b'\n') {
				line.pop();
			}
			entries
				.push(control_pb::SessionJournalEntryMsg { revision, event_json: line.clone().into() });
			if entries.len() == usize::try_from(maximum).expect("u32 fits in usize") {
				break;
			}
		}
		let returned = u64::try_from(entries.len()).expect("delta count fits in u64");
		Ok(control_pb::SessionDeltaMsg {
			session_id: self.id.0.as_str().to_owned(),
			base_revision: request.after_revision,
			head_revision,
			entries,
			has_more: request.after_revision.saturating_add(returned) < head_revision,
			props: None,
		})
	}

	/// Fenced structured ingestion encoded and appended only by the daemon.
	pub fn ingest(
		&self,
		request: control_pb::SessionIngestRequest,
	) -> Result<control_pb::SessionIngestResultMsg, SessionAuthorityError> {
		if request.session_id != self.id.0 {
			return Ok(control_pb::SessionIngestResultMsg {
				session_id: request.session_id,
				revision:   0,
				refusal:    Some(control_pb::SessionIngestRefusal::UnknownSession.into()),
				props:      None,
			});
		}
		let mut state = self.state.lock();
		if request.expected_revision != state.revision {
			return Ok(control_pb::SessionIngestResultMsg {
				session_id: self.id.0.as_str().to_owned(),
				revision:   state.revision,
				refusal:    Some(control_pb::SessionIngestRefusal::Conflict.into()),
				props:      None,
			});
		}
		let mut item: Item = request.item.ok_or(SessionAuthorityError::MissingItem)?;
		omp_agent::truncate_item_for_persistence(&mut item);
		let event = Event {
			ts:   item.created_at_ms,
			kind: Kind::Item(ItemRecord { item, turn_id: None, prompt_hash: None }),
		};
		match state.writer.append_atomic(std::slice::from_ref(&event)) {
			Ok(indexes) => state.revision = indexes[0].saturating_add(1),
			Err(JournalError::Indeterminate(_)) => {
				return Ok(control_pb::SessionIngestResultMsg {
					session_id: self.id.0.as_str().to_owned(),
					revision:   state.revision,
					refusal:    Some(control_pb::SessionIngestRefusal::WriterHalted.into()),
					props:      None,
				});
			},
			Err(error) => return Err(error.into()),
		}
		Ok(control_pb::SessionIngestResultMsg {
			session_id: self.id.0.as_str().to_owned(),
			revision:   state.revision,
			refusal:    None,
			props:      None,
		})
	}
}

#[cfg(test)]
mod session_authority_tests {
	use omp_proto::thread::v1::{Message, Part, Role, item, part};
	use tempfile::tempdir;

	use super::*;

	#[test]
	fn structured_ingest_is_revision_fenced_and_daemon_encoded() {
		let directory = tempdir().expect("temporary directory");
		let path = directory.path().join("session.jsonl");
		let header = Header {
			v:       4,
			id:      SessionId(sf!("rpc-session")),
			created: 1,
			cwd:     directory.path().to_owned(),
		};
		let authority = SessionJournalAuthority::create(&path, &header).expect("create authority");
		let empty = authority
			.snapshot(&control_pb::SessionSnapshotRequest {
				session_id:  header.id.0.as_str().to_owned(),
				if_revision: None,
				props:       None,
			})
			.expect("empty snapshot");
		assert_eq!(empty.revision, 0);
		assert!(empty.journal.is_empty());
		assert!(!path.exists());

		let item = Item {
			created_at_ms: 2,
			kind: Some(item::Kind::Message(Message {
				role:  Role::User.into(),
				parts: vec![Part { kind: Some(part::Kind::Text("hello".to_owned())) }],
			})),
			..Item::default()
		};
		let accepted = authority
			.ingest(control_pb::SessionIngestRequest {
				request_id:         1,
				idempotency_key:    "one".to_owned(),
				host_generation:    1,
				session_generation: 1,
				session_id:         header.id.0.as_str().to_owned(),
				expected_revision:  0,
				item:               Some(item.clone()),
				props:              None,
			})
			.expect("ingest");
		assert_eq!(accepted.revision, 1);
		assert!(accepted.refusal.is_none());
		let conflict = authority
			.ingest(control_pb::SessionIngestRequest {
				request_id:         2,
				idempotency_key:    "stale".to_owned(),
				host_generation:    1,
				session_generation: 1,
				session_id:         header.id.0.as_str().to_owned(),
				expected_revision:  0,
				item:               Some(item),
				props:              None,
			})
			.expect("conflict result");
		assert_eq!(conflict.refusal, Some(control_pb::SessionIngestRefusal::Conflict.into()));
		let delta = authority
			.delta(&control_pb::SessionDeltaRequest {
				session_id:      header.id.0.as_str().to_owned(),
				after_revision:  0,
				maximum_entries: 8,
				props:           None,
			})
			.expect("delta");
		assert_eq!(delta.entries.len(), 1);
		assert_eq!(delta.head_revision, 1);
	}
}

/// Selection of the credential encryption-key source.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CredentialKeyMode {
	/// Fail closed without accessing persistent encryption-key material.
	#[default]
	Unavailable,
	/// Use an owner-only key file beside the credential database.
	///
	/// This deliberately matches pi's filesystem security boundary for
	/// interactive local use. It avoids macOS Keychain ACLs tied to a rebuilt
	/// executable, but does not protect against an attacker who can read the
	/// user's data directory.
	LocalFile,
	/// Use the operating-system credential service after explicit opt-in.
	OsKeychain,
}

impl CredentialKeyMode {
	/// Selects the OS keychain only for exact `OMP_LLM_KEYCHAIN=1`. When the
	/// variable is unset, an interactive macOS process uses an owner-only local
	/// key file; unattended processes and any other set value fail closed.
	pub fn from_environment() -> Self {
		let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
		Self::from_value(std::env::var_os(KEYCHAIN_OPT_IN_ENV).as_deref(), interactive)
	}

	fn from_value(value: Option<&std::ffi::OsStr>, interactive: bool) -> Self {
		match value {
			Some(value) if value == "1" => Self::OsKeychain,
			None if interactive && cfg!(target_os = "macos") => Self::LocalFile,
			_ => Self::Unavailable,
		}
	}
}

/// Production daemon construction options.
pub struct DaemonConfig {
	data_dir: Option<PathBuf>,
	endpoint: LocalEndpoint,
}

impl DaemonConfig {
	/// Creates the standard owner-local daemon configuration.
	pub fn local(endpoint: impl Into<LocalEndpoint>) -> Self {
		let data_dir = std::env::var_os(DATA_DIR_ENV)
			.map(PathBuf::from)
			.or_else(|| {
				std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share/omp"))
			});
		Self { data_dir, endpoint: endpoint.into() }
	}

	/// Overrides the directory containing encrypted credentials and session
	/// state.
	pub fn with_data_dir(mut self, data_dir: PathBuf) -> Self {
		self.data_dir = Some(data_dir);
		self
	}
}

/// Runtime facts available once registry construction succeeds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DaemonReadiness {
	/// Requested owner-local endpoint.
	pub endpoint: LocalEndpoint,
	/// Number of catalog routes backed by constructed services.
	pub routes:   usize,
}

/// A production daemon startup or lifecycle failure.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
	/// Neither an explicit data directory nor `OMP_DATA_DIR`/`HOME` was
	/// available.
	#[error("daemon data directory is unavailable; set OMP_DATA_DIR or HOME")]
	MissingDataDirectory,
	/// Durable state directory could not be prepared.
	#[error("could not prepare daemon state directory")]
	PrepareState(#[source] std::io::Error),
	/// The checked-in catalog snapshot is invalid.
	#[error("embedded catalog snapshot is invalid")]
	Catalog(#[source] &'static omp_llm_catalog::snapshot::SnapshotError),
	/// Registry construction or route service failed.
	#[error(transparent)]
	Inference(#[from] Box<omp_llm_inference::Error>),
	/// Encrypted credential state could not be opened.
	#[error(transparent)]
	CredentialStore(#[from] StoreError),
	/// Credential encryption key provisioning failed.
	#[error(transparent)]
	CredentialKey(#[from] KeyError),
	/// Owner-only credential key file provisioning failed.
	#[error(transparent)]
	CredentialKeyFile(#[from] FileKeyError),
	/// Native settings authority could not be opened.
	#[error(transparent)]
	SettingsManager(#[from] crate::settings::manager::SettingsManagerError),
	/// Web-search settings could not be projected.
	#[error(transparent)]
	SettingsSnapshot(#[from] omp_settings::SnapshotError),
	/// Durable account state could not be opened.
	#[error(transparent)]
	AccountState(#[from] AccountStateStoreError),
	/// A static secret login engine was configured with an unsupported method.
	#[error(transparent)]
	SecretLogin(#[from] SecretLoginEngineError),
	/// A credential acquisition engine was configured with an unsupported
	/// method.
	#[error(transparent)]
	CredentialAcquisitionLogin(#[from] CredentialAcquisitionLoginEngineError),
	/// An OAuth login engine was configured with an unsupported method.
	#[error(transparent)]
	OAuthLogin(#[from] OAuthLoginEngineError),
	/// A built-in custom OAuth exchange handler could not be registered.
	#[error(transparent)]
	OAuthCustom(#[from] omp_llm_inference::auth::oauth::OAuthCustomDispatchError),
	/// Refresh coordination policy was invalid.
	#[error(transparent)]
	RefreshPolicy(#[from] omp_llm_inference::account::RefreshPolicyError),
	/// The catalog advertised an authentication method without a concrete
	/// engine.
	#[error(transparent)]
	AuthManager(#[from] AuthManagerBuildError),
	/// Durable conversation state could not be opened.
	#[error(transparent)]
	Conversation(#[from] ConversationError),
	/// Content-addressed blob state could not be opened.
	#[error(transparent)]
	BlobStore(#[from] omp_storage::blob::Error),
	/// Owner-local RPC listener could not bind.
	#[error("could not bind owner-local RPC endpoint")]
	RpcListen(#[source] omp_rpc::Error),
	/// Tonic RPC serving failed.
	#[error("owner-local inference RPC server failed")]
	RpcServe(#[source] tonic::transport::Error),
	/// The daemon RPC task failed to join.
	#[error("owner-local inference RPC task failed")]
	RpcTask(#[source] tokio::task::JoinError),
	/// The RPC server exited before a shutdown request.
	#[error("owner-local inference RPC server stopped unexpectedly")]
	RpcStopped,
	/// Signal handling failed.
	#[error("shutdown signal handling failed")]
	Signal(#[source] std::io::Error),
}

impl From<omp_llm_inference::Error> for DaemonError {
	fn from(error: omp_llm_inference::Error) -> Self {
		Self::Inference(Box::new(error))
	}
}

/// Opens encrypted production credential state.
///
/// Interactive macOS processes default to an owner-only adjacent key file.
/// `OMP_LLM_KEYCHAIN=1` deliberately opts into the stronger Keychain boundary,
/// whose application ACL can prompt again when an unsigned development binary
/// is rebuilt.
pub fn open_credential_store(
	database: impl AsRef<Path>,
) -> Result<Arc<CredentialStore>, DaemonError> {
	match CredentialKeyMode::from_environment() {
		CredentialKeyMode::Unavailable => {
			open_credential_store_with_key_source(database, Arc::new(UnavailableKeySource))
		},
		CredentialKeyMode::LocalFile => open_local_file_credential_store(database.as_ref()),
		CredentialKeyMode::OsKeychain => {
			let key_source = OsCredentialKeySource::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT);
			if key_source.active_key().is_err() {
				key_source.rotate()?;
			}
			open_credential_store_with_key_source(database, Arc::new(key_source))
		},
	}
}

fn open_local_file_credential_store(database: &Path) -> Result<Arc<CredentialStore>, DaemonError> {
	let file = FileCredentialKeySource::open(database.with_extension("key"))?;
	#[cfg(target_os = "macos")]
	{
		// One-time clean cutover for credentials written by the old interactive
		// default. The fallback is consulted only for legacy key identifiers;
		// after this transaction all rows use the file key and later rebuilds
		// never contact Keychain. Denial aborts the transaction without losing
		// the existing encrypted records.
		let source = FallbackKeySource::new(
			file,
			OsCredentialKeySource::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT),
		);
		let store = open_credential_store_with_key_source(database, Arc::new(source))?;
		store.rotate_keys()?;
		Ok(store)
	}
	#[cfg(not(target_os = "macos"))]
	{
		open_credential_store_with_key_source(database, Arc::new(file))
	}
}

/// Opens encrypted credential state with an explicitly supplied non-secret key
/// source.
pub fn open_credential_store_with_key_source(
	database: impl AsRef<Path>,
	key_source: Arc<dyn KeySource>,
) -> Result<Arc<CredentialStore>, DaemonError> {
	Ok(Arc::new(CredentialStore::open(database.as_ref(), key_source)?))
}

/// Builds the production inference registry over durable daemon state.
pub async fn production_registry(
	data_dir: &Path,
	credential_store: Arc<CredentialStore>,
) -> Result<Registry, DaemonError> {
	production_assembly(data_dir, credential_store)
		.await
		.map(|(registry, ..)| registry)
}

/// Builds the production inference registry and exposes a clone of its one
/// authentication manager to the stdio RPC host.
pub(crate) async fn production_rpc_registry(
	data_dir: &Path,
	credential_store: Arc<CredentialStore>,
) -> Result<(Registry, AuthManager), DaemonError> {
	production_assembly(data_dir, credential_store)
		.await
		.map(|(registry, _, _, auth)| (registry, auth))
}
/// Builds the production inference RPC authority used by both the standalone
/// gateway and in-process chat turns.
///
/// Keeping this seam crate-private ensures credentials, provider routing, and
/// provider-session state are assembled exactly once without becoming a public
/// application API.
pub(crate) async fn production_inference(
	data_dir: &Path,
	tool_registry: Arc<omp_tool::Registry>,
	project_root: Option<&Path>,
) -> Result<(Registry, InferenceRpc, Arc<dyn crate::auth_backend::CredentialAuthority>), DaemonError>
{
	let credential_store = open_credential_store(data_dir.join("credentials.db"))?;
	let (registry, sessions, authority, _) = production_assembly(data_dir, credential_store).await?;
	let settings = crate::settings::manager::SettingsManager::open(
		crate::settings::manager::SettingsPaths::discover(data_dir, project_root),
	)?;
	let search_settings = settings
		.snapshot()
		.project::<omp_llm_inference::search_settings::WebSearchSettings>()?
		.get()
		.clone();
	let inference = InferenceRpc::new(registry.clone(), sessions, tool_registry)
		.with_search_settings(search_settings);
	Ok((registry, inference, authority))
}

async fn production_assembly(
	data_dir: &Path,
	credential_store: Arc<CredentialStore>,
) -> Result<
	(
		Registry,
		ConversationSessionPlanner,
		Arc<dyn crate::auth_backend::CredentialAuthority>,
		AuthManager,
	),
	DaemonError,
> {
	std::fs::create_dir_all(data_dir).map_err(DaemonError::PrepareState)?;
	let catalog = Arc::new(
		omp_llm_catalog::snapshot::Catalog::try_embedded()
			.map_err(DaemonError::Catalog)?
			.clone(),
	);
	#[cfg(feature = "local-applefm")]
	let apple_routes = catalog
		.routes()
		.iter()
		.filter(|route| {
			route.codec_profile == omp_llm_catalog::CodecProfile::AppleFm
				&& route.transport == omp_llm_catalog::TransportKind::Local
		})
		.map(|route| route.id.clone())
		.collect::<Vec<_>>();
	let stored = Arc::new(crate::auth_backend::combined_authority(credential_store.clone()));
	let credentials = CredentialBroker::system(&catalog, CredentialBrokerEngines {
		stored: Some(stored.clone()),
		..CredentialBrokerEngines::default()
	})
	.map_err(|_| {
		DaemonError::Inference(Box::new(omp_llm_inference::Error::planning(
			omp_llm_inference::ErrorKind::InvalidRequest,
			omp_llm_inference::ErrorDetail::target(sf!("catalog-credential-broker-invalid",)),
			Default::default(),
		)))
	})?;
	let database = data_dir.join("credentials.db");
	let accounts = AccountPool::with_store(Arc::new(AccountStateStore::open(&database)?))?;
	let oauth_http = Arc::new(SystemOAuthHttpClient::new());
	// Resolve the Antigravity client version concurrently with the remaining
	// assembly: route codecs freeze their headers at construction, so the
	// bounded manifest probe must settle before `GoogleCcaConfig` is built.
	let antigravity_version = antigravity_version_task(data_dir, oauth_http.clone());
	let oauth_clock = Arc::new(SystemOAuthClock);
	let oauth_custom =
		Arc::new(OAuthCustomDispatcher::builtin(oauth_http.clone(), oauth_clock.clone())?);
	let refresh_coordinator =
		Arc::new(RefreshCoordinator::new("omp-auth-refresh", RefreshPolicy::default())?);
	let login_engines: Vec<Arc<dyn AuthLoginEngine>> = vec![
		// Provider-scoped engines must precede generic engines for the same method.
		Arc::new(AlibabaTokenPlanLoginEngine::new(
			catalog.clone(),
			credential_store.clone(),
			accounts.clone(),
			oauth_http.clone(),
		)),
		Arc::new(SecretLoginEngine::new(
			AuthMethod::ApiKey,
			sf!("api-key"),
			catalog.clone(),
			credential_store.clone(),
			accounts.clone(),
		)?),
		Arc::new(SecretLoginEngine::new(
			AuthMethod::SessionToken,
			sf!("session-token"),
			catalog.clone(),
			credential_store.clone(),
			accounts.clone(),
		)?),
		Arc::new(CredentialAcquisitionLoginEngine::new(
			AuthMethod::ApplicationDefault,
			sf!("application-default"),
			catalog.clone(),
			credentials.clone(),
			accounts.clone(),
		)?),
		Arc::new(CredentialAcquisitionLoginEngine::new(
			AuthMethod::AwsCredentialChain,
			sf!("aws-credential-chain"),
			catalog.clone(),
			credentials.clone(),
			accounts.clone(),
		)?),
		Arc::new(OAuthLoginEngine::new(
			AuthMethod::OAuthPkce,
			catalog.clone(),
			credential_store.clone(),
			accounts.clone(),
			oauth_http.clone(),
			oauth_clock.clone(),
			oauth_custom.clone(),
		)?),
		Arc::new(OAuthLoginEngine::new(
			AuthMethod::OAuthDevice,
			catalog.clone(),
			credential_store.clone(),
			accounts.clone(),
			oauth_http.clone(),
			oauth_clock.clone(),
			oauth_custom.clone(),
		)?),
	];
	let refresh = Arc::new(StoredOAuthRefreshEngine::new(
		catalog.clone(),
		credential_store.clone(),
		accounts.clone(),
		oauth_http.clone(),
		oauth_clock,
		oauth_custom,
		refresh_coordinator,
	));
	let auth_manager = AuthManager::new(
		catalog.clone(),
		credential_store,
		credentials.clone(),
		accounts.clone(),
		login_engines,
		refresh,
	)?;
	let exposed_auth_manager = auth_manager.clone();
	let usage_fetchers = UsageFetcherRegistry::new([
		Arc::new(AlibabaTokenPlanUsageFetcher::new(oauth_http.clone()))
			as Arc<dyn ConsoleUsageFetcher>,
		Arc::new(ClaudeUsageFetcher::new(oauth_http.clone())),
		Arc::new(OpenAiCodexUsageFetcher::new(oauth_http.clone())),
		Arc::new(GithubCopilotUsageFetcher::new(oauth_http.clone())),
		Arc::new(CursorUsageFetcher::new(oauth_http.clone())),
		Arc::new(XaiOauthUsageFetcher::new(oauth_http.clone())),
		Arc::new(GoogleAntigravityUsageFetcher::new(oauth_http.clone())),
		Arc::new(GeminiUsageFetcher::new(oauth_http.clone())),
		Arc::new(KimiUsageFetcher::new(oauth_http.clone())),
		Arc::new(ZaiUsageFetcher::new(oauth_http.clone())),
		Arc::new(MiniMaxCodeUsageFetcher::new(oauth_http.clone())),
		Arc::new(MiniMaxCodeUsageFetcher::china(oauth_http.clone())),
		Arc::new(UmansUsageFetcher::new(oauth_http.clone())),
		Arc::new(SyntheticUsageFetcher::new(oauth_http.clone())),
		Arc::new(OpenCodeGoUsageFetcher::new(oauth_http.clone())),
		Arc::new(OllamaUsageFetcher::new()),
		Arc::new(OllamaUsageFetcher::cloud()),
	]);
	let usage_manager = ConsoleUsageManager::new(
		catalog.clone(),
		credentials.clone(),
		accounts.clone(),
		usage_fetchers,
	);
	let mut credential_shapers = CredentialShaperRegistry::new();
	credential_shapers
		.register(ProviderShaper::AlibabaTokenPlan(AlibabaTokenPlanShaper::new()))
		.expect("Alibaba Token Plan credential shaper registered once");
	credential_shapers
		.register(ProviderShaper::GithubCopilot(GithubCopilotShaper::new(oauth_http)))
		.expect("GitHub Copilot credential shaper registered once");
	let sessions = ConversationSessionPlanner::open(&database, catalog.clone())?;
	let auth_application = AuthApplicationConfig { signing_regions: Arc::new(BTreeMap::new()) };
	let antigravity_fingerprint = AntigravityFingerprint {
		version: antigravity_version.await,
		cl:      env_override(ANTIGRAVITY_CL_ENV).unwrap_or_else(|| sf!(DEFAULT_ANTIGRAVITY_CL)),
		os:      env_override(ANTIGRAVITY_OS_ENV).unwrap_or_else(|| sf!(DEFAULT_ANTIGRAVITY_OS)),
		arch:    env_override(ANTIGRAVITY_ARCH_ENV).unwrap_or_else(|| sf!(DEFAULT_ANTIGRAVITY_ARCH)),
	};
	let google_cca = GoogleCcaConfig {
		gemini_cli_platform: Str::from(std::env::consts::OS),
		gemini_cli_arch:     Str::from(std::env::consts::ARCH),
		antigravity_headers: CcaHeaders::antigravity(&antigravity_fingerprint, false, None),
		antigravity_policy:  AntigravityPolicy::default(),
	};
	let dependencies = ProductionDependencies::new(
		credentials,
		auth_manager,
		accounts,
		sessions.clone(),
		WebSocketTransport::new(),
		google_cca,
		HttpTransport::new(),
		auth_application,
		AdmissionController::new(32, 128),
		Duration::from_secs(60),
		Arc::new(BTreeMap::new()),
		Arc::new(credential_shapers),
	);
	let dependencies = dependencies.with_usage_manager(usage_manager);
	#[cfg(feature = "local-applefm")]
	let dependencies = {
		use omp_llm_inference::local::applefm::{AppleFmCodec, AppleFmTransport, FRAMEWORK_TIMEOUT};
		match AppleFmTransport::new() {
			Ok(transport) => {
				let backend =
					LocalRouteBackend::new(Arc::new(AppleFmCodec), transport, FRAMEWORK_TIMEOUT);
				dependencies.with_local_routes(
					apple_routes
						.into_iter()
						.map(|route| (route, backend.clone())),
				)
			},
			Err(evidence) => {
				let reason = ReasonId(Str::from(evidence.state.code()));
				dependencies.with_local_unavailable(
					apple_routes
						.into_iter()
						.map(|route| (route, reason.clone())),
				)
			},
		}
	};
	let registry = Registry::builder(catalog)
		.with_builtins(BuiltinConfig::production(dependencies))?
		.build()?;
	let authority: Arc<dyn crate::auth_backend::CredentialAuthority> = stored;
	Ok((registry, sessions, authority, exposed_auth_manager))
}

/// Resolves the Antigravity client version without blocking assembly work:
/// explicit `OMP_ANTIGRAVITY_VERSION` override → bounded update-manifest
/// discovery → last discovered release persisted in the data directory →
/// pinned reference fallback.
fn antigravity_version_task(
	data_dir: &Path,
	client: Arc<SystemOAuthHttpClient>,
) -> impl Future<Output = Str> {
	let override_version = env_override(ANTIGRAVITY_VERSION_ENV);
	let cache_path = data_dir.join(ANTIGRAVITY_VERSION_CACHE_FILE);
	let fetch = override_version.is_none().then(|| {
		tokio::spawn(async move {
			tokio::time::timeout(
				ANTIGRAVITY_VERSION_FETCH_TIMEOUT,
				discover_antigravity_version(client.as_ref()),
			)
			.await
			.ok()
			.flatten()
		})
	});
	async move {
		if let Some(version) = override_version {
			return version;
		}
		if let Some(fetch) = fetch
			&& let Ok(Some(version)) = fetch.await
		{
			// Best-effort persistence so offline boots keep the discovered release.
			let _ = std::fs::write(&cache_path, version.as_str());
			return version;
		}
		// Discovery failed: prefer the persisted release over the pinned default
		// only when it is actually newer (a stale cache must not undo a shipped
		// fallback bump).
		let cached = std::fs::read_to_string(&cache_path).ok().and_then(|raw| {
			let raw = raw.trim();
			release_ordinal(raw).map(|ordinal| (Str::from(raw), ordinal))
		});
		let pinned = release_ordinal(DEFAULT_ANTIGRAVITY_VERSION).unwrap_or_default();
		match cached {
			Some((version, ordinal)) if ordinal > pinned => version,
			_ => sf!(DEFAULT_ANTIGRAVITY_VERSION),
		}
	}
}

/// Parses a `major.minor.patch` release into an orderable key; any other
/// shape is rejected.
fn release_ordinal(version: &str) -> Option<[u64; 3]> {
	let mut ordinal = [0_u64; 3];
	let mut parts = version.split('.');
	for slot in &mut ordinal {
		let part = parts.next()?;
		if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
			return None;
		}
		*slot = part.parse().ok()?;
	}
	parts.next().is_none().then_some(ordinal)
}

/// Reads a non-empty trimmed environment override.
fn env_override(name: &str) -> Option<Str> {
	std::env::var(name).ok().and_then(|value| {
		let value = value.trim();
		(!value.is_empty()).then(|| Str::from(value))
	})
}

#[derive(Clone, Copy)]
struct TracingObservation;

impl Observer for TracingObservation {
	fn started(&self, event: ExecutionStarted) {
		tracing::debug!(execution = ?event, "inference execution started");
	}

	fn finished(&self, event: ExecutionFinished) {
		tracing::debug!(execution = ?event, "inference execution finished");
	}
}

/// Running comprehensive inference registry.
pub struct DaemonHandle {
	readiness: DaemonReadiness,
	registry:  Registry,
	shutdown:  watch::Sender<bool>,
	rpc_task:  JoinHandle<Result<(), tonic::transport::Error>>,
}

impl DaemonHandle {
	/// Loads the immutable catalog and constructs every built-in route service
	/// with an empty shared tool registry.
	pub async fn start(config: DaemonConfig) -> Result<Self, DaemonError> {
		Self::start_with_tool_registry(config, Arc::new(omp_tool::Registry::new())).await
	}

	/// Starts inference with the same revision registry used by environment
	/// dispatch in a composed application.
	pub async fn start_with_tool_registry(
		config: DaemonConfig,
		tool_registry: Arc<omp_tool::Registry>,
	) -> Result<Self, DaemonError> {
		let data_dir = config
			.data_dir
			.clone()
			.ok_or(DaemonError::MissingDataDirectory)?;
		std::fs::create_dir_all(&data_dir).map_err(DaemonError::PrepareState)?;
		let (registry, inference, _authority) =
			production_inference(&data_dir, tool_registry, None).await?;
		Self::start_rpc(config, data_dir, registry, inference).await
	}

	/// Starts the production RPC service set around a deterministic test
	/// registry while retaining the gateway's real context and replay authority.
	#[doc(hidden)]
	pub async fn start_for_test(
		config: DaemonConfig,
		registry: Registry,
		sessions: ConversationSessionPlanner,
		tool_registry: Arc<omp_tool::Registry>,
		live_responses: flume::Sender<omp_llm_inference::event::WorkflowResponse>,
	) -> Result<Self, DaemonError> {
		let data_dir = config
			.data_dir
			.clone()
			.ok_or(DaemonError::MissingDataDirectory)?;
		std::fs::create_dir_all(&data_dir).map_err(DaemonError::PrepareState)?;
		let inference =
			InferenceRpc::new_for_test(registry.clone(), sessions, tool_registry, live_responses);
		Self::start_rpc(config, data_dir, registry, inference).await
	}

	async fn start_rpc(
		config: DaemonConfig,
		data_dir: PathBuf,
		registry: Registry,
		inference: InferenceRpc,
	) -> Result<Self, DaemonError> {
		let routes = registry
			.catalog()
			.routes()
			.iter()
			.filter(|route| registry.contains_service(&route.id))
			.count();
		let incoming = omp_rpc::uds::listen(config.endpoint.as_path())
			.await
			.map_err(DaemonError::RpcListen)?;
		let (shutdown, mut rpc_shutdown) = watch::channel(false);
		let blobs = Arc::new(BlobStore::open(&data_dir)?);
		let inference = InferenceServer::new(inference);
		let auth = AuthServer::new(AuthRpc::new(registry.clone()));
		let blobs = BlobServer::new(BlobRpc::new(blobs));
		let rpc_task = tokio::spawn(async move {
			Server::builder()
				.add_service(inference)
				.add_service(blobs)
				.add_service(auth)
				.serve_with_incoming_shutdown(incoming, async move {
					while !*rpc_shutdown.borrow() && rpc_shutdown.changed().await.is_ok() {}
				})
				.await
		});
		Ok(Self {
			readiness: DaemonReadiness { endpoint: config.endpoint, routes },
			registry,
			shutdown,
			rpc_task,
		})
	}

	/// Returns registry readiness facts.
	pub const fn readiness(&self) -> &DaemonReadiness {
		&self.readiness
	}

	/// Returns a clone-cheap comprehensive operation service.
	pub fn service(&self) -> ProviderService {
		self.registry.service_with_observer(TracingObservation)
	}

	/// Creates a typed client using caller-provided call metadata.
	pub fn client(&self, meta: omp_llm_inference::CallMeta) -> Client<ProviderService, Router> {
		Client::new(self.service(), Router::new(self.registry.clone(), Duration::from_secs(30)), meta)
	}

	/// Waits for process shutdown and then signals daemon-owned tasks.
	pub async fn wait(mut self) -> Result<(), DaemonError> {
		tokio::select! {
			signal = shutdown_signal() => signal.map_err(DaemonError::Signal)?,
			result = &mut self.rpc_task => {
				result.map_err(DaemonError::RpcTask)?.map_err(DaemonError::RpcServe)?;
				return Err(DaemonError::RpcStopped);
			},
		}
		self.finish_shutdown().await
	}

	/// Initiates graceful shutdown.
	pub async fn shutdown(self) -> Result<(), DaemonError> {
		self.finish_shutdown().await
	}

	async fn finish_shutdown(mut self) -> Result<(), DaemonError> {
		let _ = self.shutdown.send(true);
		(&mut self.rpc_task)
			.await
			.map_err(DaemonError::RpcTask)?
			.map_err(DaemonError::RpcServe)?;
		#[cfg(unix)]
		match tokio::fs::remove_file(self.readiness.endpoint.as_path()).await {
			Ok(()) => {},
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
			Err(error) => return Err(DaemonError::PrepareState(error)),
		}

		Ok(())
	}
}
#[cfg(test)]
mod credential_key_mode_tests {
	use std::ffi::OsStr;

	use super::CredentialKeyMode;

	#[test]
	fn environment_and_interactivity_select_keychain_mode() {
		assert_eq!(
			CredentialKeyMode::from_value(Some(OsStr::new("1")), false),
			CredentialKeyMode::OsKeychain
		);
		assert_eq!(
			CredentialKeyMode::from_value(Some(OsStr::new("1")), true),
			CredentialKeyMode::OsKeychain
		);
		assert_eq!(CredentialKeyMode::from_value(None, false), CredentialKeyMode::Unavailable);
		assert_eq!(
			CredentialKeyMode::from_value(None, true),
			if cfg!(target_os = "macos") {
				CredentialKeyMode::LocalFile
			} else {
				CredentialKeyMode::Unavailable
			}
		);
		for interactive in [false, true] {
			for value in [OsStr::new(""), OsStr::new("true"), OsStr::new("0")] {
				assert_eq!(
					CredentialKeyMode::from_value(Some(value), interactive),
					CredentialKeyMode::Unavailable
				);
			}
		}
	}
}

#[cfg(test)]
mod antigravity_version_tests {
	use super::release_ordinal;

	#[test]
	fn only_strict_release_triples_are_orderable() {
		assert_eq!(release_ordinal("2.8.0"), Some([2, 8, 0]));
		assert_eq!(release_ordinal("10.0.3"), Some([10, 0, 3]));
		assert!(release_ordinal("2.8").is_none());
		assert!(release_ordinal("2.8.0.1").is_none());
		assert!(release_ordinal("2.8.0-beta").is_none());
		assert!(release_ordinal("+1.2.3").is_none());
		assert!(release_ordinal("").is_none());
	}

	#[test]
	fn cached_release_only_beats_a_newer_pinned_fallback_by_ordering() {
		// The downgrade guard in `antigravity_version_task` compares ordinals.
		assert!(release_ordinal("2.9.0") > release_ordinal("2.8.0"));
		assert!(release_ordinal("2.8.0") > release_ordinal("2.7.9"));
		assert!(release_ordinal("3.0.0") > release_ordinal("2.99.99"));
	}
}

#[cfg(all(test, feature = "local-applefm"))]
mod tests {
	use super::*;

	#[tokio::test]
	async fn every_catalog_apple_route_has_backend_or_unavailability_evidence() {
		let state = tempfile::tempdir().expect("temporary daemon state");
		let store = open_credential_store_with_key_source(
			state.path().join("credentials.db"),
			Arc::new(omp_llm_inference::auth::HeadlessKeySource::new(
				omp_llm_inference::auth::KeyId::new("apple-route-test"),
				[0x34; 32],
			)),
		)
		.expect("credential store");
		let registry = production_registry(state.path(), store)
			.await
			.expect("production registry");
		for route in registry.catalog().routes().iter().filter(|route| {
			route.codec_profile == omp_llm_catalog::CodecProfile::AppleFm
				&& route.transport == omp_llm_catalog::TransportKind::Local
		}) {
			assert!(
				registry.contains_service(&route.id) || registry.unavailability(&route.id).is_some(),
				"Apple route {} lacks a backend and typed unavailability",
				route.id
			);
		}
	}
}

#[cfg(unix)]
async fn shutdown_signal() -> Result<(), std::io::Error> {
	use tokio::signal::unix::{SignalKind, signal};
	let mut terminate = signal(SignalKind::terminate())?;
	tokio::select! { result = tokio::signal::ctrl_c() => result, _ = terminate.recv() => Ok(()) }
}

#[cfg(windows)]
async fn shutdown_signal() -> Result<(), std::io::Error> {
	tokio::signal::ctrl_c().await
}
