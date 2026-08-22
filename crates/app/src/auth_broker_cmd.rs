//! Combined provider/MCP credential-vault operator.

use std::{
	collections::BTreeSet,
	path::Path,
	sync::Arc,
	time::{SystemTime, UNIX_EPOCH},
};

use miette::{IntoDiagnostic as _, miette};
use omp_core::SecretBox;
use omp_llm_catalog::ProviderId;
use omp_llm_inference::{
	account::{AccountRecord, AccountStateStore},
	auth::{CredentialOrigin, CredentialWrite},
	call::AccountRoutingContext,
	id::{AccountId, PrincipalId},
};
use ring::rand::{SecureRandom as _, SystemRandom};
use serde::Deserialize;
use zeroize::Zeroizing;

use crate::{
	cli::{AuthBrokerArgs, AuthBrokerCommand, AuthCommand},
	daemon::{DaemonConfig, DaemonHandle},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ImportRecord {
	provider:      String,
	account:       String,
	principal:     String,
	kind:          String,
	secret:        String,
	#[serde(default)]
	expires_at_ms: Option<u64>,
}

/// Executes one combined credential-authority operation.
pub async fn run(args: AuthBrokerArgs) -> miette::Result<()> {
	let data_dir = crate::cli::data_dir(args.data_dir)?;
	std::fs::create_dir_all(&data_dir).into_diagnostic()?;
	match args.command {
		AuthBrokerCommand::Serve { endpoint } => {
			let handle = DaemonHandle::start(DaemonConfig::local(endpoint).with_data_dir(data_dir))
				.await
				.into_diagnostic()?;
			handle.wait().await.into_diagnostic()
		},
		AuthBrokerCommand::Token { regenerate } => token(&data_dir, regenerate),
		AuthBrokerCommand::Login { provider } => {
			crate::auth_backend::run(data_dir.join("credentials.db"), AuthCommand::Login { provider })
				.await
		},
		AuthBrokerCommand::Logout { provider } => logout(&data_dir, provider.as_str()).await,
		AuthBrokerCommand::List => {
			crate::auth_backend::run(data_dir.join("credentials.db"), AuthCommand::List {
				provider: None,
			})
			.await
		},
		AuthBrokerCommand::Import { path } => import(&data_dir, &path),
		AuthBrokerCommand::Migrate { dry_run } => migrate(&data_dir, dry_run),
		AuthBrokerCommand::Status => status(&data_dir),
	}
}

pub(crate) fn token(data_dir: &Path, regenerate: bool) -> miette::Result<()> {
	let path = data_dir.join("auth-broker.token");
	if !regenerate && path.is_file() {
		let value = std::fs::read_to_string(&path).into_diagnostic()?;
		println!("{}", value.trim());
		return Ok(());
	}
	let mut bytes = Zeroizing::new([0_u8; 32]);
	SystemRandom::new()
		.fill(bytes.as_mut())
		.map_err(|_| miette!("system random source failed"))?;
	let value = Zeroizing::new(hex(&*bytes));
	write_owner_only(&path, value.as_bytes())?;
	println!("{}", value.as_str());
	Ok(())
}

async fn logout(data_dir: &Path, provider: &str) -> miette::Result<()> {
	let state = AccountStateStore::open(data_dir.join("credentials.db")).into_diagnostic()?;
	let accounts = state
		.load_accounts()
		.into_diagnostic()?
		.into_iter()
		.filter(|record| record.provider.as_str() == provider)
		.map(|record| record.account)
		.collect::<Vec<_>>();
	if accounts.is_empty() {
		return Err(miette!("provider `{provider}` has no stored accounts"));
	}
	for account in accounts {
		crate::auth_backend::run(data_dir.join("credentials.db"), AuthCommand::Logout {
			account: account.into_inner(),
		})
		.await?;
	}
	Ok(())
}

fn import(data_dir: &Path, path: &Path) -> miette::Result<()> {
	let input = std::fs::read(path).into_diagnostic()?;
	let records: Vec<ImportRecord> = serde_json::from_slice(&input).into_diagnostic()?;
	if records.is_empty() {
		return Err(miette!("credential import is empty"));
	}
	let credentials =
		crate::daemon::open_credential_store(data_dir.join("credentials.db")).into_diagnostic()?;
	let state = AccountStateStore::open(data_dir.join("credentials.db")).into_diagnostic()?;
	let catalog = omp_llm_catalog::snapshot::Catalog::try_embedded()
		.map_err(|error| miette!(error.to_string()))?;
	let now_ms = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.into_diagnostic()?
		.as_millis()
		.try_into()
		.map_err(|_| miette!("system clock exceeds credential timestamp range"))?;
	for record in records {
		let provider = ProviderId::from(record.provider.as_str());
		let account = AccountId::from(record.account.as_str());
		let principal = PrincipalId::from(record.principal.as_str());
		let secret = Zeroizing::new(record.secret);
		let secret = SecretBox::new(secret.as_bytes().to_vec().into_boxed_slice());
		let metadata = credentials
			.put(CredentialWrite {
				account_id: &account,
				principal_id: &principal,
				kind: &record.kind,
				secret: &secret,
				expires_at_ms: record.expires_at_ms,
				origin: CredentialOrigin::Persistent,
				now_ms,
				expected_generation: None,
			})
			.into_diagnostic()?;
		let routes = catalog
			.routes()
			.iter()
			.filter(|route| route.provider == provider)
			.map(|route| route.id.clone())
			.collect::<BTreeSet<_>>();
		if routes.is_empty() {
			return Err(miette!("unknown credential provider `{}`", provider.as_str()));
		}
		state
			.upsert_account(&AccountRecord {
				account,
				principal,
				provider,
				routes,
				enabled: true,
				credential_generation: metadata.generation,
				routing: AccountRoutingContext::default(),
			})
			.into_diagnostic()?;
	}
	println!("imported credential records");
	Ok(())
}

fn migrate(data_dir: &Path, dry_run: bool) -> miette::Result<()> {
	let store =
		crate::daemon::open_credential_store(data_dir.join("credentials.db")).into_diagnostic()?;
	let count = if dry_run {
		store.list_metadata().into_diagnostic()?.len()
	} else {
		store.rotate_keys().into_diagnostic()?
	};
	println!("{} {count} credential record(s)", if dry_run { "would migrate" } else { "migrated" });
	Ok(())
}

fn status(data_dir: &Path) -> miette::Result<()> {
	let store =
		crate::daemon::open_credential_store(data_dir.join("credentials.db")).into_diagnostic()?;
	let accounts = store.list_metadata().into_diagnostic()?.len();
	let token = data_dir.join("auth-broker.token").is_file();
	println!(
		"healthy: {accounts} credential(s), bearer token {}",
		if token { "ready" } else { "not generated" }
	);
	Ok(())
}

fn hex(bytes: &[u8]) -> String {
	const DIGITS: &[u8; 16] = b"0123456789abcdef";
	let mut output = String::with_capacity(bytes.len() * 2);
	for byte in bytes {
		output.push(char::from(DIGITS[usize::from(byte >> 4)]));
		output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
	}
	output
}

fn write_owner_only(path: &Path, bytes: &[u8]) -> miette::Result<()> {
	let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
	let mut options = std::fs::OpenOptions::new();
	options.write(true).create_new(true);
	#[cfg(unix)]
	{
		use std::os::unix::fs::OpenOptionsExt as _;
		options.mode(0o600);
	}
	let mut file = options.open(&temporary).into_diagnostic()?;
	std::io::Write::write_all(&mut file, bytes).into_diagnostic()?;
	file.sync_all().into_diagnostic()?;
	std::fs::rename(&temporary, path).into_diagnostic()?;
	Ok(())
}
