//! Typed authentication and encrypted credential-store construction.

use std::{
	io::{self, IsTerminal as _, Write as _},
	path::PathBuf,
	time::Duration,
};

use miette::{IntoDiagnostic as _, Result, miette};
use nix::sys::termios::{LocalFlags, SetArg, tcgetattr, tcsetattr};
use omp_core::{EnvPath, ExposeSecret as _, SecretString, Str};
use omp_env::{EnvClient, ExecEvent};
use omp_llm_catalog::ProviderId;
use omp_llm_inference::{
	Client,
	answer::{AuthAnswer, AuthEvent, AuthPrompt, AuthPromptKind, AuthResponse},
	auth::{CommandCredentialError, CommandCredentialExecutor, CommandExecutionFuture},
	call::{AuthInput, AuthRequest, CallMeta, LoginRequest, Target},
	id::{AccountId, RequestId},
	receipt::ExecutionBudget,
};
use omp_proto::env::v1::{
	CloseSessionRequest, ExecOutcome, ExecRequest, OpenSessionRequest, OutputChannel, Script,
};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

pub(crate) use crate::envd::mcp::auth_authority::{CombinedAuthAuthority, CredentialAuthority};

/// Composes provider and MCP leasing over the one encrypted credential store.
pub(crate) fn combined_authority(
	store: std::sync::Arc<omp_llm_inference::auth::CredentialStore>,
) -> CombinedAuthAuthority {
	CombinedAuthAuthority::new(store)
}

/// Environment-backed executor for `!command` credential sources.
#[derive(Clone, Debug)]
pub(crate) struct EnvCommandCredentialExecutor {
	client:     EnvClient,
	cwd:        EnvPath,
	timeout:    Duration,
	max_stdout: usize,
}

impl EnvCommandCredentialExecutor {
	/// Creates a bounded command credential executor rooted at `cwd`.
	pub(crate) const fn new(
		client: EnvClient,
		cwd: EnvPath,
		timeout: Duration,
		max_stdout: usize,
	) -> Self {
		Self { client, cwd, timeout, max_stdout }
	}

	async fn execute_inner(
		&self,
		command: Str,
		cancellation: CancellationToken,
	) -> Result<SecretString, CommandCredentialError> {
		let opened = self
			.client
			.open_session(&self.cwd, OpenSessionRequest::default())
			.await
			.map_err(|_| CommandCredentialError::Execution)?;
		let session = opened.session.clone();
		let run = self
			.client
			.exec(ExecRequest {
				session: opened.session,
				source: Some(Script { text: command.to_string(), ..Script::default() }),
				..ExecRequest::default()
			})
			.await;
		let result = match run {
			Ok(mut run) => {
				let mut stdout = Zeroizing::new(Vec::new());
				loop {
					let event = tokio::select! {
						() = cancellation.cancelled() => {
							Err(CommandCredentialError::Cancelled)
						},
						event = tokio::time::timeout(self.timeout, run.next_event()) => {
							match event {
								Ok(event) => event.map_err(|_| CommandCredentialError::Execution),
								Err(_) => Err(CommandCredentialError::Timeout),
							}
						},
					};
					let event = match event {
						Ok(event) => event,
						Err(error) => break Err(error),
					};
					match event {
						Some(ExecEvent::Output(frame))
							if frame.channel == OutputChannel::Stdout as i32 =>
						{
							if stdout.len().saturating_add(frame.data.len()) > self.max_stdout {
								break Err(CommandCredentialError::OutputTooLarge);
							}
							stdout.extend_from_slice(&frame.data);
						},
						Some(ExecEvent::Exit(exit)) => {
							let Some(status) = exit.status else {
								break Err(CommandCredentialError::Execution);
							};
							if status.outcome == ExecOutcome::Timeout as i32 {
								break Err(CommandCredentialError::Timeout);
							}
							if status.outcome == ExecOutcome::Cancelled as i32 {
								break Err(CommandCredentialError::Cancelled);
							}
							if status.outcome != ExecOutcome::Exited as i32 || status.exit_code != Some(0)
							{
								break Err(CommandCredentialError::Execution);
							}
							let text = std::str::from_utf8(&stdout)
								.map_err(|_| CommandCredentialError::InvalidUtf8)?;
							let trimmed = text.trim();
							if trimmed.is_empty() {
								break Err(CommandCredentialError::Empty);
							}
							break Ok(SecretString::from(trimmed));
						},
						Some(ExecEvent::Started(_) | ExecEvent::Output(_)) => {},
						None => break Err(CommandCredentialError::Execution),
					}
				}
			},
			Err(_) => Err(CommandCredentialError::Execution),
		};
		let _ = self
			.client
			.close_session(CloseSessionRequest { session, ..CloseSessionRequest::default() })
			.await;
		result
	}
}

impl CommandCredentialExecutor for EnvCommandCredentialExecutor {
	fn execute(&self, command: Str, cancellation: CancellationToken) -> CommandExecutionFuture {
		let executor = self.clone();
		Box::pin(async move { executor.execute_inner(command, cancellation).await })
	}
}

use crate::cli::AuthCommand;

/// Opens encrypted credential state and executes one typed authentication
/// operation.
pub async fn run(database: PathBuf, command: AuthCommand) -> Result<()> {
	let data_dir = database
		.parent()
		.filter(|path| !path.as_os_str().is_empty())
		.ok_or_else(|| miette!("HOME or OMP_DATA_DIR must be set"))?;
	std::fs::create_dir_all(data_dir).into_diagnostic()?;
	let store = crate::daemon::open_credential_store(&database).into_diagnostic()?;
	let registry = crate::daemon::production_registry(data_dir, store)
		.await
		.into_diagnostic()?;
	let default_provider = registry
		.catalog()
		.providers()
		.first()
		.map(|provider| provider.id.clone())
		.ok_or_else(|| miette!("embedded catalog is unavailable"))?;
	let (provider, operation) = match command {
		AuthCommand::Login { provider } => {
			let provider = ProviderId::from(provider);
			(provider.clone(), AuthRequest::Login(LoginRequest { provider, method: None }))
		},
		AuthCommand::List { provider } => {
			let provider = provider.map_or(default_provider, ProviderId::from);
			(provider.clone(), AuthRequest::ListAccounts { provider: Some(provider) })
		},
		AuthCommand::Refresh { account } => {
			(default_provider.clone(), AuthRequest::Refresh { account: AccountId::from(account) })
		},
		AuthCommand::Logout { account } => {
			(default_provider.clone(), AuthRequest::Logout { account: AccountId::from(account) })
		},
	};
	let meta = CallMeta {
		id:       RequestId::from("omp-auth-cli"),
		target:   Target::ProviderService(provider),
		deadline: None,
		budget:   ExecutionBudget::default(),
		session:  None,
	};
	let planner =
		omp_llm_inference::router::Router::new(registry.clone(), std::time::Duration::from_secs(30));
	let mut client = Client::new(registry.service(), planner, meta);
	print_auth(client.execute(operation).await.into_diagnostic()?).await
}

async fn print_auth(answer: AuthAnswer) -> Result<()> {
	match answer {
		AuthAnswer::Session(session) => {
			let session_id = session.id.clone();
			while let Ok(event) = session.events.recv_async().await {
				match event.into_diagnostic()? {
					AuthEvent::OpenUrl(url) => {
						crate::open::open_path(&url);
						println!("open {url}");
					},
					AuthEvent::ShowDeviceCode { code, verification_url } => {
						crate::open::open_path(&verification_url);
						println!(
							"complete device authorization at {verification_url} using code {}",
							code.expose_secret()
						);
					},
					AuthEvent::Prompt(prompt) => {
						let input = tokio::task::spawn_blocking(move || read_prompt(&prompt))
							.await
							.into_diagnostic()??;
						session
							.responses
							.send_async(AuthResponse { session: session_id.clone(), input })
							.await
							.into_diagnostic()?;
					},
					AuthEvent::Waiting => println!("waiting for provider authorization"),
					AuthEvent::Complete(account) => {
						println!("{} {}", account.account, account.provider);
						break;
					},
				}
			}
		},
		AuthAnswer::Accounts(accounts) => {
			for account in accounts {
				println!("{} {}", account.account, account.provider);
			}
		},
		AuthAnswer::Refreshed(account) => println!("{} {}", account.account, account.provider),
		AuthAnswer::LoggedOut(account) => println!("{account}"),
		AuthAnswer::Submitted(session) => println!("{session}"),
	}
	Ok(())
}

fn read_prompt(prompt: &AuthPrompt) -> Result<AuthInput> {
	let mut stdout = io::stdout().lock();
	write!(stdout, "{}: ", prompt.message).into_diagnostic()?;
	stdout.flush().into_diagnostic()?;
	drop(stdout);

	let stdin = io::stdin();
	let hide_input =
		!matches!(prompt.input, AuthPromptKind::Confirmation | AuthPromptKind::PlainText)
			&& stdin.is_terminal();
	let original = if hide_input {
		let original = tcgetattr(&stdin).into_diagnostic()?;
		let mut hidden = original.clone();
		hidden.local_flags.remove(LocalFlags::ECHO);
		tcsetattr(&stdin, SetArg::TCSANOW, &hidden).into_diagnostic()?;
		Some(original)
	} else {
		None
	};
	let mut value = Zeroizing::new(String::new());
	let read = stdin.read_line(&mut value);
	if let Some(original) = original {
		tcsetattr(&stdin, SetArg::TCSANOW, &original).into_diagnostic()?;
		println!();
	}
	if read.into_diagnostic()? == 0 {
		return Err(miette!("authentication input closed"));
	}
	auth_input(prompt, value.trim())
}

fn auth_input(prompt: &AuthPrompt, value: &str) -> Result<AuthInput> {
	if value.is_empty()
		&& matches!(
			prompt.input,
			AuthPromptKind::AuthorizationCode | AuthPromptKind::ApiKey | AuthPromptKind::SessionToken
		) {
		return Err(miette!("authentication input must not be empty"));
	}
	let kind = match prompt.input {
		AuthPromptKind::AuthorizationCode => crate::chat_ui::AuthPromptKind::AuthorizationCode,
		AuthPromptKind::ApiKey => crate::chat_ui::AuthPromptKind::ApiKey,
		AuthPromptKind::SessionToken => crate::chat_ui::AuthPromptKind::SessionToken,
		AuthPromptKind::PlainText => crate::chat_ui::AuthPromptKind::PlainText,
		AuthPromptKind::OptionalSecret => crate::chat_ui::AuthPromptKind::OptionalSecret,
		AuthPromptKind::Confirmation => crate::chat_ui::AuthPromptKind::Confirmation,
	};
	Ok(crate::chat_ui::auth_input(kind, value.to_owned()))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn callback_prompt_submits_the_complete_url() {
		let prompt = AuthPrompt {
			id:      "oauth-callback-url".into(),
			message: "callback".into(),
			input:   AuthPromptKind::AuthorizationCode,
		};
		let AuthInput::CallbackUrl(value) =
			auth_input(&prompt, "http://localhost/callback?code=abc&state=xyz").unwrap()
		else {
			panic!("callback prompt must preserve the complete URL");
		};
		assert_eq!(value.expose_secret(), "http://localhost/callback?code=abc&state=xyz");
	}

	#[test]
	fn confirmation_accepts_an_empty_line() {
		let prompt = AuthPrompt {
			id:      "confirm".into(),
			message: "continue".into(),
			input:   AuthPromptKind::Confirmation,
		};
		assert!(matches!(auth_input(&prompt, "").unwrap(), AuthInput::DeviceConfirmed));
	}

	#[test]
	fn plain_text_is_visible_and_may_select_the_empty_default() {
		let prompt = AuthPrompt {
			id:      "region".into(),
			message: "region".into(),
			input:   AuthPromptKind::PlainText,
		};
		assert!(
			matches!(auth_input(&prompt, "").unwrap(), AuthInput::PlainText(value) if value.is_empty())
		);
	}

	#[test]
	fn optional_secret_accepts_empty_as_skip() {
		let prompt = AuthPrompt {
			id:      "cookie".into(),
			message: "cookie".into(),
			input:   AuthPromptKind::OptionalSecret,
		};
		assert!(matches!(
			auth_input(&prompt, "").unwrap(),
			AuthInput::OptionalSecret(value) if value.expose_secret().is_empty()
		));
	}
}
