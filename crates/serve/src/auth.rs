//! Tonic authentication projection over canonical typed auth and usage
//! operations.

use std::{
	collections::BTreeMap,
	pin::Pin,
	sync::{
		Arc,
		atomic::{AtomicU64, Ordering},
	},
	time::{Duration, Instant},
};

use futures::{Stream, StreamExt as _};
use omp_catalog::ProviderId;
use omp_core::{Secret, SecretString};
use omp_inference::{
	Client, Error as InferenceError, ErrorKind, Registry,
	answer::{
		AccountState, AccountSummary, AuthAnswer, AuthEvent, AuthSession, UsageQuantity, UsageReport,
		UsageStatus, UsageUnit, UsageWindowKind,
	},
	auth::{AuthControlHandle, CredentialControlWrite, OAuthControlImport},
	call::{
		AuthInput, AuthMethod, AuthRequest, CallMeta, LoginRequest, Target, UsageRequest, UsageScope,
	},
	id::{AccountId, LoginSessionId, RequestId},
	receipt::{ExecutionBudget, UsageSource},
	router::Router,
};
use omp_proto::omp::auth::v1 as pb;
use parking_lot::Mutex;
use tonic::{Request, Response, Status};

static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);
type AuthEventStream =
	Pin<Box<dyn Stream<Item = Result<pb::CredentialEvent, Status>> + Send + 'static>>;

/// RPC server that retains interactive login channels while a flow is active.
#[derive(Clone)]
pub struct AuthRpc {
	registry: Registry,
	flows:    Arc<Mutex<BTreeMap<String, AuthSession>>>,
	control:  Option<AuthControlHandle>,
}

impl AuthRpc {
	/// Wraps one immutable comprehensive registry.
	pub fn new(registry: Registry) -> Self {
		Self { registry, flows: Arc::new(Mutex::new(BTreeMap::new())), control: None }
	}

	/// Binds the same live auth manager used by route execution to lifecycle
	/// RPC.
	pub fn with_control(registry: Registry, control: AuthControlHandle) -> Self {
		Self { registry, flows: Arc::new(Mutex::new(BTreeMap::new())), control: Some(control) }
	}

	fn control(&self) -> Result<&AuthControlHandle, Status> {
		self
			.control
			.as_ref()
			.ok_or_else(|| Status::failed_precondition("auth lifecycle owner is not bound"))
	}

	fn control_account(&self, id: u64) -> Result<AccountId, Status> {
		let matches = self
			.control()?
			.accounts(None)
			.into_iter()
			.filter(|account| wire_account_id(&account.account) == id)
			.map(|account| account.account)
			.collect::<Vec<_>>();
		match matches.as_slice() {
			[account] => Ok(account.clone()),
			[] => Err(Status::not_found("credential not found")),
			_ => Err(Status::failed_precondition("credential id collision")),
		}
	}

	fn control_meta(
		&self,
		account: omp_inference::account::AccountRecord,
	) -> Result<pb::CredentialMeta, Status> {
		let metadata = self
			.control()?
			.metadata(&account.account)
			.map_err(store_status)?
			.ok_or_else(|| Status::not_found("credential not found"))?;
		let kind = match metadata.kind.as_str() {
			"api_key" | "api-key" => pb::credential_meta::Kind::ApiKey,
			"oauth" | "oauth-renewable-v1" | "bearer" => pb::credential_meta::Kind::Oauth,
			"aws" => pb::credential_meta::Kind::Aws,
			_ => pb::credential_meta::Kind::Unspecified,
		};
		let blocks = self
			.control()?
			.blocks(&account.account)
			.into_iter()
			.map(|(scope, until_ms)| pb::Block {
				scope: scope.to_string(),
				provider_key: String::new(),
				until_ms,
			})
			.collect();
		Ok(pb::CredentialMeta {
			id:             wire_account_id(&account.account),
			provider:       account.provider.as_str().to_owned(),
			kind:           kind as i32,
			identity:       account.principal.as_str().to_owned(),
			state:          if account.enabled {
				pb::credential_meta::State::Active as i32
			} else {
				pb::credential_meta::State::Disabled as i32
			},
			blocks,
			disabled_cause: String::new(),
			expires_at_ms:  metadata.expires_at_ms.unwrap_or_default(),
			created_at_ms:  metadata.created_at_ms,
			updated_at_ms:  metadata.updated_at_ms,
		})
	}

	fn provider_for(&self, requested: Option<&str>) -> Result<ProviderId, Status> {
		if let Some(provider) = requested.filter(|value| !value.is_empty()) {
			return Ok(ProviderId::from(provider));
		}
		self
			.registry
			.catalog()
			.providers()
			.iter()
			.find(|provider| {
				provider
					.management
					.supports(omp_catalog::OperationKind::Auth)
			})
			.map(|provider| provider.id.clone())
			.ok_or_else(|| Status::failed_precondition("no constructed route supports authentication"))
	}

	fn client(&self, provider: ProviderId) -> Client<omp_inference::ProviderService, Router> {
		let sequence = REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
		Client::new(
			self.registry.service(),
			Router::new(self.registry.clone(), Duration::from_secs(30)),
			CallMeta {
				id:       RequestId::from(format!("auth-rpc-{sequence}")),
				target:   Target::ProviderService(provider),
				deadline: None,
				budget:   ExecutionBudget::default(),
				session:  None,
			},
		)
	}

	async fn execute(
		&self,
		provider: ProviderId,
		request: AuthRequest,
	) -> Result<AuthAnswer, Status> {
		self
			.client(provider)
			.execute(request)
			.await
			.map_err(inference_status)
	}

	async fn account_operation(
		&self,
		account: u64,
		refresh: bool,
	) -> Result<pb::CredentialMeta, Status> {
		let account = if self.control.is_some() {
			self.control_account(account)?
		} else {
			AccountId::from(account.to_string())
		};
		let provider = self
			.control
			.as_ref()
			.and_then(|control| {
				control
					.accounts(None)
					.into_iter()
					.find(|record| record.account == account)
					.map(|record| record.provider)
			})
			.map_or_else(|| self.provider_for(None), Ok)?;
		let operation = if refresh {
			AuthRequest::Refresh { account }
		} else {
			AuthRequest::Logout { account }
		};
		match self.execute(provider, operation).await? {
			AuthAnswer::Refreshed(account) => account_meta(account),
			AuthAnswer::LoggedOut(account) => Ok(pb::CredentialMeta {
				id: parse_account_id(&account)?,
				state: pb::credential_meta::State::Disabled as i32,
				..pb::CredentialMeta::default()
			}),
			_ => Err(Status::internal("auth operation returned the wrong typed answer")),
		}
	}

	async fn probe_account(
		&self,
		account: AccountSummary,
		strict: bool,
	) -> Result<pb::CredentialHealth, Status> {
		let credential_id = parse_account_id(&account.account)?;
		let provider = account.provider.clone();
		let started = Instant::now();
		let result = self
			.client(provider.clone())
			.execute(UsageRequest {
				provider:    Some(provider.clone()),
				account:     Some(account.account),
				scope:       UsageScope::All,
				allow_stale: !strict,
			})
			.await;
		Ok(match result {
			Ok(_) => pb::CredentialHealth {
				credential_id,
				provider: provider.as_str().to_owned(),
				healthy: true,
				status_code: Some(200),
				latency_ms: elapsed_ms(started.elapsed()),
				error_class: pb::credential_health::ErrorClass::Unspecified as i32,
			},
			Err(error) => failed_health(credential_id, provider, started.elapsed(), &error),
		})
	}
}

#[tonic::async_trait]
impl pb::auth_server::Auth for AuthRpc {
	type WatchCredentialsStream = AuthEventStream;

	async fn list_credentials(
		&self,
		request: Request<pb::ListCredentialsRequest>,
	) -> Result<Response<pb::ListCredentialsResponse>, Status> {
		let request = request.into_inner();
		if let Some(control) = &self.control {
			let requested = (!request.provider.is_empty())
				.then(|| ProviderId::from(request.provider.as_str()));
			let credentials = control
				.accounts(requested.as_ref().map(|provider| &**provider))
				.into_iter()
				.map(|account| self.control_meta(account))
				.collect::<Result<Vec<_>, _>>()?;
			return Ok(Response::new(pb::ListCredentialsResponse {
				credentials,
				cursor: None,
			}));
		}
		let provider = self.provider_for(Some(&request.provider))?;
		let answer = self
			.execute(provider.clone(), AuthRequest::ListAccounts { provider: Some(provider) })
			.await?;
		let AuthAnswer::Accounts(accounts) = answer else {
			return Err(Status::internal("auth list returned the wrong typed answer"));
		};
		let credentials = accounts
			.into_iter()
			.map(account_meta)
			.collect::<Result<Vec<_>, _>>()?;
		Ok(Response::new(pb::ListCredentialsResponse { credentials, cursor: None }))
	}

	async fn watch_credentials(
		&self,
		_request: Request<pb::WatchCredentialsRequest>,
	) -> Result<Response<Self::WatchCredentialsStream>, Status> {
		let stream = futures::stream::once(async {
			Ok(pb::CredentialEvent {
				cursor: None,
				event:  Some(pb::credential_event::Event::Reset(pb::credential_event::Reset {})),
			})
		});
		Ok(Response::new(Box::pin(stream)))
	}

	async fn begin_login(
		&self,
		request: Request<pb::BeginLoginRequest>,
	) -> Result<Response<pb::BeginLoginResponse>, Status> {
		let provider = self.provider_for(Some(&request.into_inner().provider))?;
		let answer = self
			.execute(provider.clone(), AuthRequest::Login(LoginRequest { provider, method: None }))
			.await?;
		let AuthAnswer::Session(session) = answer else {
			return Err(Status::internal("auth login returned the wrong typed answer"));
		};
		let flow_id = session.id.as_str().to_owned();
		let event = session
			.events
			.recv_async()
			.await
			.map_err(|_| Status::unavailable("auth flow ended before its first step"))?
			.map_err(inference_status)?;
		let step = login_step(event)?;
		self.flows.lock().insert(flow_id.clone(), session);
		Ok(Response::new(pb::BeginLoginResponse { flow_id, step: Some(step) }))
	}

	async fn submit_code(
		&self,
		request: Request<pb::SubmitCodeRequest>,
	) -> Result<Response<pb::CredentialMeta>, Status> {
		let request = request.into_inner();
		let (responses, events) = {
			let flows = self.flows.lock();
			let session = flows
				.get(&request.flow_id)
				.ok_or_else(|| Status::not_found("auth flow not found"))?;
			(session.responses.clone(), session.events.clone())
		};
		let session = LoginSessionId::from(request.flow_id.as_str());
		responses
			.send_async(omp_inference::answer::AuthResponse {
				session,
				input: AuthInput::AuthorizationCode(SecretString::from(request.code)),
			})
			.await
			.map_err(|_| Status::unavailable("auth flow no longer accepts input"))?;
		let account = await_account(events).await?;
		self.flows.lock().remove(&request.flow_id);
		Ok(Response::new(account_meta(account)?))
	}

	async fn wait_login(
		&self,
		request: Request<pb::WaitLoginRequest>,
	) -> Result<Response<pb::CredentialMeta>, Status> {
		let flow_id = request.into_inner().flow_id;
		let events = self
			.flows
			.lock()
			.get(&flow_id)
			.ok_or_else(|| Status::not_found("auth flow not found"))?
			.events
			.clone();
		let account = await_account(events).await?;
		self.flows.lock().remove(&flow_id);
		Ok(Response::new(account_meta(account)?))
	}

	async fn put_api_key(
		&self,
		request: Request<pb::PutApiKeyRequest>,
	) -> Result<Response<pb::CredentialMeta>, Status> {
		let request = request.into_inner();
		let provider = self.provider_for(Some(&request.provider))?;
		let answer = self
			.execute(
				provider.clone(),
				AuthRequest::Login(LoginRequest { provider, method: Some(AuthMethod::ApiKey) }),
			)
			.await?;
		let AuthAnswer::Session(session) = answer else {
			return Err(Status::internal("API-key login returned the wrong typed answer"));
		};
		session
			.responses
			.send_async(omp_inference::answer::AuthResponse {
				session: session.id.clone(),
				input:   AuthInput::ApiKey(SecretString::from(request.api_key)),
			})
			.await
			.map_err(|_| Status::unavailable("API-key login no longer accepts input"))?;
		Ok(Response::new(account_meta(await_account(session.events).await?)?))
	}

	async fn refresh_credential(
		&self,
		request: Request<pb::RefreshCredentialRequest>,
	) -> Result<Response<pb::CredentialMeta>, Status> {
		if self.control.is_some() {
			let account = self.control_account(request.get_ref().id)?;
			self
				.control()?
				.refresh(account.clone())
				.await
				.map_err(inference_status)?;
			let record = self
				.control()?
				.accounts(None)
				.into_iter()
				.find(|record| record.account == account)
				.ok_or_else(|| Status::not_found("credential not found"))?;
			return Ok(Response::new(self.control_meta(record)?));
		}
		Ok(Response::new(
			self
				.account_operation(request.into_inner().id, true)
				.await?,
		))
	}

	async fn delete_credential(
		&self,
		request: Request<pb::DeleteCredentialRequest>,
	) -> Result<Response<pb::DeleteCredentialResponse>, Status> {
		if self.control.is_some() {
			let account = self.control_account(request.get_ref().id)?;
			self
				.control()?
				.delete(account)
				.await
				.map_err(inference_status)?;
			return Ok(Response::new(pb::DeleteCredentialResponse {}));
		}
		self
			.account_operation(request.into_inner().id, false)
			.await?;
		Ok(Response::new(pb::DeleteCredentialResponse {}))
	}
	async fn reveal_credential(
		&self,
		request: Request<pb::RevealCredentialRequest>,
	) -> Result<Response<pb::RevealCredentialResponse>, Status> {
		let request = request.into_inner();
		let account = self.control_account(request.id)?;
		let record = self
			.control()?
			.accounts(None)
			.into_iter()
			.find(|record| record.account == account)
			.ok_or_else(|| Status::not_found("credential not found"))?;
		if record.provider.as_str() != request.provider {
			return Err(Status::permission_denied(
				"credential does not belong to the authorized provider",
			));
		}
		let audit = reveal_audit(request);
		let secret = self
			.control()?
			.reveal(&account, &audit, |secret| secret.expose(|bytes| bytes.to_vec()))
			.map_err(store_status)?;
		Ok(Response::new(pb::RevealCredentialResponse { secret: secret.into() }))
	}

	async fn get_usage(
		&self,
		request: Request<pb::GetUsageRequest>,
	) -> Result<Response<pb::GetUsageResponse>, Status> {
		let request = request.into_inner();
		let provider = self.provider_for(Some(&request.provider))?;
		let account =
			(request.credential_id != 0).then(|| AccountId::from(request.credential_id.to_string()));
		let mut client = self.client(provider.clone());
		let report = client
			.execute(UsageRequest {
				provider: Some(provider),
				account,
				scope: UsageScope::All,
				allow_stale: !request.refresh,
			})
			.await
			.map_err(inference_status)?;
		Ok(Response::new(pb::GetUsageResponse { reports: vec![usage_report(*report)] }))
	}

	async fn put_aws_credential(
		&self,
		request: Request<pb::PutAwsCredentialRequest>,
	) -> Result<Response<pb::CredentialMeta>, Status> {
		let request = request.into_inner();
		let provider = ProviderId::from(request.provider.as_str());
		let principal = omp_inference::PrincipalId::from(request.identity.as_str());
		let mut material = Vec::with_capacity(
			request.access_key_id.len()
				+ request.secret_access_key.len()
				+ request.session_token.len()
				+ 16,
		);
		for field in [request.access_key_id, request.secret_access_key, request.session_token] {
			material.extend_from_slice(&(field.len() as u64).to_le_bytes());
			material.extend_from_slice(&field);
		}
		let (_, account) = self
			.control()?
			.store(CredentialControlWrite {
				provider,
				principal,
				identity: Some(request.identity.into()),
				kind: "aws".into(),
				secret: Secret::new(material),
				expires_at_ms: None,
			})
			.map_err(store_status)?;
		Ok(Response::new(self.control_meta(account)?))
	}

	async fn import_o_auth(
		&self,
		request: Request<pb::ImportOAuthRequest>,
	) -> Result<Response<pb::CredentialMeta>, Status> {
		let request = request.into_inner();
		let provider = ProviderId::from(request.provider.as_str());
		let identity = (!request.identity.is_empty()).then(|| request.identity.into());
		let principal = omp_inference::PrincipalId::from(
			identity
				.as_ref()
				.map_or(provider.as_str(), omp_core::Str::as_str),
		);
		let (_, account) = self
			.control()?
			.import_oauth(OAuthControlImport {
				provider,
				principal,
				identity,
				access_token: (!request.access_token.is_empty())
					.then(|| SecretString::from(request.access_token)),
				refresh_token: SecretString::from(request.refresh_token),
				expires_at_ms: (request.expires_at_ms != 0).then_some(request.expires_at_ms),
			})
			.map_err(store_status)?;
		Ok(Response::new(self.control_meta(account)?))
	}

	async fn disable_credential(
		&self,
		request: Request<pb::DisableCredentialRequest>,
	) -> Result<Response<pb::CredentialMeta>, Status> {
		let request = request.into_inner();
		let account = self.control_account(request.id)?;
		let record = self
			.control()?
			.set_enabled(&account, false)
			.map_err(store_status)?;
		let mut metadata = self.control_meta(record)?;
		metadata.disabled_cause = request.cause;
		Ok(Response::new(metadata))
	}

	async fn enable_credential(
		&self,
		request: Request<pb::EnableCredentialRequest>,
	) -> Result<Response<pb::CredentialMeta>, Status> {
		let account = self.control_account(request.into_inner().id)?;
		let record = self
			.control()?
			.set_enabled(&account, true)
			.map_err(store_status)?;
		Ok(Response::new(self.control_meta(record)?))
	}

	async fn report_block(
		&self,
		request: Request<pb::ReportBlockRequest>,
	) -> Result<Response<pb::CredentialMeta>, Status> {
		let request = request.into_inner();
		let account = self.control_account(request.id)?;
		let block = request
			.block
			.ok_or_else(|| Status::invalid_argument("credential block is missing"))?;
		let until = std::time::UNIX_EPOCH
			.checked_add(Duration::from_millis(block.until_ms))
			.ok_or_else(|| Status::invalid_argument("credential block time is invalid"))?;
		self
			.control()?
			.report_block(&account, block.scope, until)
			.map_err(store_status)?;
		let record = self
			.control()?
			.accounts(None)
			.into_iter()
			.find(|record| record.account == account)
			.ok_or_else(|| Status::not_found("credential not found"))?;
		Ok(Response::new(self.control_meta(record)?))
	}

	async fn clear_blocks(
		&self,
		_request: Request<pb::ClearBlocksRequest>,
	) -> Result<Response<pb::CredentialMeta>, Status> {
		Err(not_available("operator block clearing"))
	}

	async fn mark_usage_stale(
		&self,
		_request: Request<pb::MarkUsageStaleRequest>,
	) -> Result<Response<pb::MarkUsageStaleResponse>, Status> {
		Err(not_available("explicit usage cache invalidation"))
	}

	async fn get_usage_history(
		&self,
		_request: Request<pb::GetUsageHistoryRequest>,
	) -> Result<Response<pb::GetUsageHistoryResponse>, Status> {
		Err(not_available("durable usage history"))
	}

	async fn get_client_usage(
		&self,
		_request: Request<pb::GetClientUsageRequest>,
	) -> Result<Response<pb::GetClientUsageResponse>, Status> {
		Err(not_available("per-client usage accounting"))
	}

	async fn probe_credentials(
		&self,
		request: Request<pb::ProbeCredentialsRequest>,
	) -> Result<Response<pb::ProbeCredentialsResponse>, Status> {
		let request = request.into_inner();
		let requested =
			(!request.provider.is_empty()).then(|| ProviderId::from(request.provider.as_str()));
		let provider = self.provider_for(requested.as_ref().map(ProviderId::as_str))?;
		let answer = self
			.execute(provider, AuthRequest::ListAccounts { provider: requested })
			.await?;
		let AuthAnswer::Accounts(accounts) = answer else {
			return Err(Status::internal("auth probe list returned the wrong typed answer"));
		};
		let strict = request.strict;
		let credentials = futures::stream::iter(accounts.into_iter().map(|account| {
			let rpc = self.clone();
			async move { rpc.probe_account(account, strict).await }
		}))
		.buffered(4)
		.collect::<Vec<_>>()
		.await
		.into_iter()
		.collect::<Result<Vec<_>, _>>()?;
		Ok(Response::new(pb::ProbeCredentialsResponse { credentials }))
	}

	async fn mint_scoped_token(
		&self,
		_request: Request<pb::MintScopedTokenRequest>,
	) -> Result<Response<pb::ScopedToken>, Status> {
		Err(not_available("scoped client-direct token minting"))
	}
}

async fn await_account(
	events: flume::Receiver<Result<AuthEvent, omp_inference::Error>>,
) -> Result<AccountSummary, Status> {
	while let Ok(event) = events.recv_async().await {
		if let AuthEvent::Complete(account) = event.map_err(inference_status)? {
			return Ok(account);
		}
	}
	Err(Status::unavailable("auth flow ended without account completion"))
}

fn login_step(event: AuthEvent) -> Result<pb::begin_login_response::Step, Status> {
	match event {
		AuthEvent::OpenUrl(url) => {
			Ok(pb::begin_login_response::Step::Browse(pb::begin_login_response::Browse {
				url: url.as_str().to_owned(),
			}))
		},
		AuthEvent::ShowDeviceCode { code, verification_url } => {
			Ok(pb::begin_login_response::Step::Device(pb::begin_login_response::DeviceCode {
				user_code:  omp_core::ExposeSecret::expose_secret(&code).to_owned(),
				verify_url: verification_url.as_str().to_owned(),
			}))
		},
		AuthEvent::Prompt(prompt) => Err(Status::failed_precondition(format!(
			"auth flow requires {} input via the typed prompt channel",
			prompt.message
		))),
		AuthEvent::Waiting => Err(Status::failed_precondition(
			"auth flow is waiting without a client-visible login step",
		)),
		AuthEvent::Complete(_) => {
			Err(Status::failed_precondition("auth flow completed before returning a login step"))
		},
	}
}

fn account_meta(account: AccountSummary) -> Result<pb::CredentialMeta, Status> {
	Ok(pb::CredentialMeta {
		id:             parse_account_id(&account.account)?,
		provider:       account.provider.as_str().to_owned(),
		kind:           pb::credential_meta::Kind::Unspecified as i32,
		identity:       account
			.principal
			.map_or_else(String::new, |value| value.as_str().to_owned()),
		state:          match account.state {
			AccountState::Active => 1,
			AccountState::RefreshRequired => 2,
			AccountState::Disabled | AccountState::LoggedOut => 4,
		},
		blocks:         Vec::new(),
		disabled_cause: String::new(),
		expires_at_ms:  0,
		created_at_ms:  0,
		updated_at_ms:  0,
	})
}

fn parse_account_id(account: &AccountId<str>) -> Result<u64, Status> {
	Ok(wire_account_id(account))
}
fn reveal_audit(request: pb::RevealCredentialRequest) -> omp_inference::auth::AuditedCredentialReveal {
	omp_inference::auth::AuditedCredentialReveal {
		extension:          request.extension.into(),
		caller_principal:   request.caller_principal.into(),
		provider:           request.provider.into(),
		host_generation:    request.host_generation,
		session_generation: request.session_generation,
		request_id:         request.request_id,
		reason:             request.reason.into(),
	}
}

fn store_status(error: omp_inference::auth::StoreError) -> Status {
	match error {
		omp_inference::auth::StoreError::NotFound => Status::not_found(error.to_string()),
		omp_inference::auth::StoreError::GenerationConflict
		| omp_inference::auth::StoreError::RevealAuditConflict => Status::aborted(error.to_string()),
		omp_inference::auth::StoreError::InvalidRevealAudit => {
			Status::permission_denied(error.to_string())
		},
		_ => Status::internal(error.to_string()),
	}
}

fn wire_account_id(account: &AccountId<str>) -> u64 {
	if let Ok(id) = account.as_str().parse() {
		return id;
	}
	let mut hash = 0xcbf2_9ce4_8422_2325_u64;
	for byte in b"omp/auth/control-id/v1"
		.iter()
		.chain(account.as_str().as_bytes())
	{
		hash ^= u64::from(*byte);
		hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
	}
	hash
}

fn usage_report(report: UsageReport) -> pb::UsageReport {
	let fetched_at_ms = report
		.windows
		.iter()
		.map(|window| usage_time_ms(window.observed_at))
		.max()
		.unwrap_or_default();
	pb::UsageReport {
		credential_id: report.account.as_str().parse().unwrap_or(0),
		provider: report.provider.as_str().to_owned(),
		plan: report
			.plan
			.map_or_else(String::new, |value| value.as_str().to_owned()),
		account: report.account.as_str().to_owned(),
		principal: report
			.principal
			.map_or_else(String::new, |value| value.as_str().to_owned()),
		account_metadata: Some(pb::usage_report::AccountMetadata {
			provider_account_id: report
				.account_meta
				.provider_account_id
				.map(|value| value.as_str().to_owned()),
			email:               report
				.account_meta
				.email
				.map(|value| value.as_str().to_owned()),
			project_id:          report
				.account_meta
				.project_id
				.map(|value| value.as_str().to_owned()),
			organization_id:     report
				.account_meta
				.organization_id
				.map(|value| value.as_str().to_owned()),
			organization_name:   report
				.account_meta
				.organization_name
				.map(|value| value.as_str().to_owned()),
		}),
		source_label: report.source_label.map(|value| value.as_str().to_owned()),
		notes: report
			.notes
			.into_vec()
			.into_iter()
			.map(|value| value.as_str().to_owned())
			.collect(),
		reset_credits: report
			.reset_credits
			.map(|reset| pb::usage_report::ResetCredits {
				available: reset.available,
				credits:   reset
					.credits
					.into_vec()
					.into_iter()
					.map(|credit| pb::usage_report::reset_credits::Credit {
						granted_at_ms: credit.granted_at.map(usage_time_ms),
						expires_at_ms: credit.expires_at.map(usage_time_ms),
						status:        credit.status.map(|value| value.as_str().to_owned()),
					})
					.collect(),
			}),
		windows: report
			.windows
			.into_iter()
			.map(|window| {
				let used_percent = match (window.amount.consumed, window.amount.limit) {
					(Some(used), Some(limit)) if limit.units != 0 => {
						(usage_quantity_f64(used) / usage_quantity_f64(limit)) * 100.0
					},
					(Some(used), None) if window.amount.unit == UsageUnit::Percent => {
						usage_quantity_f64(used)
					},
					_ => 0.0,
				};
				pb::UsageWindow {
					label: window
						.label
						.as_ref()
						.unwrap_or(&window.dimension)
						.as_str()
						.to_owned(),
					used_percent,
					resets_at_ms: window.resets_at.map_or(0, usage_time_ms),
					id: window.id.as_str().to_owned(),
					kind: match window.kind {
						UsageWindowKind::RateLimit => pb::usage_window::Kind::RateLimit,
						UsageWindowKind::Quota => pb::usage_window::Kind::Quota,
						UsageWindowKind::Billing => pb::usage_window::Kind::Billing,
						UsageWindowKind::Balance => pb::usage_window::Kind::Balance,
					} as i32,
					dimension: window.dimension.as_str().to_owned(),
					consumed: window.amount.consumed.map(|value| value.units),
					remaining: window.amount.remaining.map(|value| value.units),
					limit: window.amount.limit.map(|value| value.units),
					unit: match window.amount.unit {
						UsageUnit::Percent => pb::usage_window::Unit::Percent,
						UsageUnit::Tokens => pb::usage_window::Unit::Tokens,
						UsageUnit::Requests => pb::usage_window::Unit::Requests,
						UsageUnit::Usd => pb::usage_window::Unit::Usd,
						UsageUnit::Minutes => pb::usage_window::Unit::Minutes,
						UsageUnit::Bytes => pb::usage_window::Unit::Bytes,
						UsageUnit::Unknown => pb::usage_window::Unit::Unknown,
					} as i32,
					consumed_decimal_exponent: window
						.amount
						.consumed
						.map_or(0, |value| u32::from(value.decimal_exponent)),
					remaining_decimal_exponent: window
						.amount
						.remaining
						.map_or(0, |value| u32::from(value.decimal_exponent)),
					limit_decimal_exponent: window
						.amount
						.limit
						.map_or(0, |value| u32::from(value.decimal_exponent)),
					scope: window.scope.map(|value| value.as_str().to_owned()),
					duration_ms: window
						.duration
						.map(|value| value.as_millis().try_into().unwrap_or(u64::MAX)),
					reset_label: window.reset_label.map(|value| value.as_str().to_owned()),
					status: window.status.map(|status| match status {
						UsageStatus::Ok => pb::usage_window::Status::Ok,
						UsageStatus::Warning => pb::usage_window::Status::Warning,
						UsageStatus::Exhausted => pb::usage_window::Status::Exhausted,
						UsageStatus::Unknown => pb::usage_window::Status::Unknown,
					} as i32),
					notes: window
						.notes
						.into_vec()
						.into_iter()
						.map(|value| value.as_str().to_owned())
						.collect(),
					observed_at_ms: usage_time_ms(window.observed_at),
					accuracy: match window.source {
						UsageSource::Provider | UsageSource::Measured => {
							omp_proto::omp::inference::v1::usage::Accuracy::Exact
						},
						UsageSource::Estimated => {
							omp_proto::omp::inference::v1::usage::Accuracy::Estimated
						},
						UsageSource::Mixed => omp_proto::omp::inference::v1::usage::Accuracy::Mixed,
						UsageSource::Unknown => {
							omp_proto::omp::inference::v1::usage::Accuracy::Unspecified
						},
					} as i32,
				}
			})
			.collect(),
		fetched_at_ms,
		detail: None,
	}
}

fn usage_time_ms(time: std::time::SystemTime) -> u64 {
	time
		.duration_since(std::time::UNIX_EPOCH)
		.map_or(0, |duration| duration.as_millis().try_into().unwrap_or(u64::MAX))
}

fn usage_quantity_f64(quantity: UsageQuantity) -> f64 {
	quantity.units as f64 / 10_f64.powi(i32::from(quantity.decimal_exponent))
}
fn failed_health(
	credential_id: u64,
	provider: ProviderId,
	elapsed: Duration,
	error: &InferenceError,
) -> pb::CredentialHealth {
	pb::CredentialHealth {
		credential_id,
		provider: provider.as_str().to_owned(),
		healthy: false,
		status_code: error.status.map(u32::from),
		latency_ms: elapsed_ms(elapsed),
		error_class: error_class(error) as i32,
	}
}

fn elapsed_ms(elapsed: Duration) -> u64 {
	elapsed.as_millis().try_into().unwrap_or(u64::MAX)
}

fn error_class(error: &InferenceError) -> pb::credential_health::ErrorClass {
	use pb::credential_health::ErrorClass;

	match error.status {
		Some(401) => return ErrorClass::Authentication,
		Some(403) => return ErrorClass::Authorization,
		Some(408) => return ErrorClass::Timeout,
		Some(429) => return ErrorClass::RateLimited,
		Some(500..=599) => return ErrorClass::Upstream,
		_ => {},
	}
	match error.kind {
		ErrorKind::Authentication
		| ErrorKind::CredentialStorageUnavailable
		| ErrorKind::AccountDisabled => ErrorClass::Authentication,
		ErrorKind::Authorization | ErrorKind::PaymentRequired => ErrorClass::Authorization,
		ErrorKind::RateLimited => ErrorClass::RateLimited,
		ErrorKind::QuotaExhausted | ErrorKind::BudgetExhausted => ErrorClass::Quota,
		ErrorKind::Dns
		| ErrorKind::Tls
		| ErrorKind::Connectivity
		| ErrorKind::Protocol
		| ErrorKind::StreamCorruption => ErrorClass::Connectivity,
		ErrorKind::Cancelled | ErrorKind::DeadlineExceeded => ErrorClass::Timeout,
		ErrorKind::InvalidRequest
		| ErrorKind::TargetNotFound
		| ErrorKind::CapabilityUnknown
		| ErrorKind::CodecMismatch
		| ErrorKind::CapabilityMismatch
		| ErrorKind::NativeRequestRejected => ErrorClass::InvalidRequest,
		ErrorKind::RouteUnavailable
		| ErrorKind::StalePlan
		| ErrorKind::ReplayRequired
		| ErrorKind::StagingRequired
		| ErrorKind::ProviderContractMismatch
		| ErrorKind::ContextOverflow
		| ErrorKind::ContentFilter
		| ErrorKind::SafetyRefusal
		| ErrorKind::MalformedModelOutput
		| ErrorKind::StructuredOutputFailure
		| ErrorKind::ToolNonCompliance
		| ErrorKind::RepeatedReasoning
		| ErrorKind::RepeatedToolCall
		| ErrorKind::EmptyCompletion
		| ErrorKind::EmptyOutput
		| ErrorKind::SessionExpired
		| ErrorKind::SessionConflict
		| ErrorKind::LocalModelUnavailable
		| ErrorKind::ResourceExhausted => ErrorClass::Upstream,
		ErrorKind::PolicyBufferExceeded | ErrorKind::InternalInvariant => ErrorClass::Internal,
	}
}
fn not_available(capability: &str) -> Status {
	Status::failed_precondition(format!(
		"{capability} is not exposed by any constructed canonical auth operation"
	))
}
fn inference_status(error: omp_inference::Error) -> Status {
	Status::failed_precondition(error.to_string())
}
#[cfg(test)]
mod tests {
	use omp_inference::{
		Error, ErrorKind,
		error::{ErrorPhase, RetryAction},
		receipt::ExecutionReceipt,
	};

	use super::{error_class, pb};
	use super::reveal_audit;

	#[test]
	fn reveal_rpc_preserves_authenticated_audit_evidence() {
		let audit = reveal_audit(pb::RevealCredentialRequest {
			id: 7,
			provider: "openai".to_owned(),
			extension: "fixture.extension".to_owned(),
			caller_principal: "principal".to_owned(),
			host_generation: 11,
			session_generation: 13,
			request_id: 17,
			reason: "extension_control_reveal".to_owned(),
		});
		assert_eq!(audit.extension.as_str(), "fixture.extension");
		assert_eq!(audit.caller_principal.as_str(), "principal");
		assert_eq!(audit.provider.as_str(), "openai");
		assert_eq!(audit.host_generation, 11);
		assert_eq!(audit.session_generation, 13);
		assert_eq!(audit.request_id, 17);
		assert_eq!(audit.reason.as_str(), "extension_control_reveal");
	}

	#[test]
	fn credential_probe_failures_keep_typed_http_health() {
		let error = Error::new(
			ErrorKind::Connectivity,
			ErrorPhase::Connecting,
			RetryAction::Never,
			ExecutionReceipt::default(),
		)
		.status(Some(401));
		assert_eq!(error_class(&error), pb::credential_health::ErrorClass::Authentication,);
	}
}
