//! Credential-injecting gateway administration over the canonical daemon.

use miette::{IntoDiagnostic as _, WrapErr as _, miette};
use omp_proto::omp::gateway::v1::{HelloRequest, gateway_client::GatewayClient};

use crate::{
	cli::{AuthGatewayArgs, AuthGatewayCommand},
	daemon::{DaemonConfig, DaemonHandle},
	endpoint::LocalEndpoint,
};

/// Starts, rotates, and health-checks the gateway without owning credentials.
pub async fn run(args: AuthGatewayArgs) -> miette::Result<()> {
	let data_dir = crate::cli::data_dir(args.data_dir)?;
	std::fs::create_dir_all(&data_dir).into_diagnostic()?;
	match args.command {
		AuthGatewayCommand::Serve { endpoint } => {
			let handle = DaemonHandle::start(DaemonConfig::local(endpoint).with_data_dir(data_dir))
				.await
				.into_diagnostic()?;
			handle.wait().await.into_diagnostic()
		},
		AuthGatewayCommand::Token { regenerate } => {
			crate::auth_broker_cmd::token(&data_dir, regenerate)
		},
		AuthGatewayCommand::Status { endpoint } => health(&endpoint, true).await,
		AuthGatewayCommand::Check { endpoint, strict } => health(&endpoint, strict).await,
	}
}

async fn health(endpoint: &LocalEndpoint, strict: bool) -> miette::Result<()> {
	let channel = match omp_rpc::uds::connect(endpoint.as_path()).await {
		Ok(channel) => channel,
		Err(error) if !strict => {
			println!("unhealthy: {error}");
			return Ok(());
		},
		Err(error) => {
			return Err(error)
				.into_diagnostic()
				.wrap_err_with(|| format!("could not connect to {endpoint}"));
		},
	};
	let request = HelloRequest {
		client:       "omp-auth-gateway-cli".to_owned(),
		schema_rev:   omp_proto::SCHEMA_REV,
		capabilities: vec!["auth".to_owned(), "inference.turn".to_owned()],
	};
	let response = GatewayClient::new(channel).hello(request).await;
	let response = match response {
		Ok(response) => response.into_inner(),
		Err(error) if !strict => {
			println!("unhealthy: {error}");
			return Ok(());
		},
		Err(error) => return Err(miette!("gateway health check failed: {error}")),
	};
	if response.schema_rev < omp_proto::SCHEMA_REV {
		if strict {
			return Err(miette!(
				"gateway schema {} is older than required {}",
				response.schema_rev,
				omp_proto::SCHEMA_REV,
			));
		}
		println!("unhealthy: obsolete schema {}", response.schema_rev);
		return Ok(());
	}
	println!(
		"healthy: {} schema {} [{}]",
		response.server_version,
		response.schema_rev,
		response.capabilities.join(", "),
	);
	Ok(())
}
