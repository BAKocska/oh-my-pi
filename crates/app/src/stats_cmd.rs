//! `omp stats` command composition.

use std::{
	io::IsTerminal as _,
	net::{IpAddr, SocketAddr},
	sync::Arc,
};

use miette::{IntoDiagnostic as _, miette};
use omp_driver::{stats_api::StatsApi, stats_server};
use omp_storage::index::SessionIndex;

use crate::cli::{StatsArgs, StatsCommand};

/// Runs a statistics CLI operation against the authoritative write-time index.
pub async fn run(args: StatsArgs) -> miette::Result<()> {
	let state_dir = args.state_dir.unwrap_or(omp_core::dirs::data_dir(None).into_diagnostic()?);
	std::fs::create_dir_all(&state_dir).into_diagnostic()?;
	let index = Arc::new(
		SessionIndex::open_authoritative_reader(state_dir.join("sessions.sqlite3"))
			.into_diagnostic()?,
	);
	let api = StatsApi::new(Arc::clone(&index), state_dir.join("stats-sync.lock"));
	match args.command {
		None | Some(StatsCommand::Summary { range: None }) => summary(&api, "30d"),
		Some(StatsCommand::Summary { range: Some(range) }) => summary(&api, &range),
		Some(StatsCommand::Json { range }) => {
			let document = api
				.overview_document(&range)
				.map_err(|message| miette!(message))?;
			println!("{}", serde_json::to_string_pretty(&document).into_diagnostic()?);
			Ok(())
		},
		Some(StatsCommand::Sync) => {
			if std::io::stderr().is_terminal() {
				eprint!("Synchronizing write-time statistics... ");
			}
			let document = api.sync_document().map_err(|message| miette!(message))?;
			if std::io::stderr().is_terminal() {
				eprintln!("done");
			}
			println!("{}", serde_json::to_string_pretty(&document).into_diagnostic()?);
			Ok(())
		},
		Some(StatsCommand::Serve { host, port, auth_token, no_open }) => {
			let ip = host
				.parse::<IpAddr>()
				.map_err(|_| miette!("--host must be an IP address"))?;
			let server = stats_server::start(
				stats_server::Config { address: SocketAddr::new(ip, port), auth_token, state_dir },
				index,
			)
			.await
			.into_diagnostic()?;
			let address = server.address();
			let display_host = if address.ip().is_unspecified() {
				"127.0.0.1".to_owned()
			} else {
				address.ip().to_string()
			};
			let url = format!("http://{display_host}:{}", address.port());
			println!("Dashboard available at: {url}");
			if !no_open {
				omp_core::open::open_path(&url);
			}
			tokio::signal::ctrl_c().await.into_diagnostic()?;
			server.shutdown().await;
			Ok(())
		},
	}
}

fn summary(api: &StatsApi, range: &str) -> miette::Result<()> {
	let document = api
		.overview_document(range)
		.map_err(|message| miette!(message))?;
	let overall = &document["data"]["overall"];
	let requests = overall["requests"].as_u64().unwrap_or_default();
	let errors = overall["errors"].as_u64().unwrap_or_default();
	let error_rate = if requests == 0 {
		0.0
	} else {
		errors as f64 * 100.0 / requests as f64
	};
	println!("Range: {range}");
	println!("Sessions: {}", overall["sessions"].as_u64().unwrap_or_default());
	println!("Requests: {requests} ({error_rate:.2}% errors)");
	println!(
		"Tokens: {} input, {} output, {} cache read",
		overall["input_tokens"].as_u64().unwrap_or_default(),
		overall["output_tokens"].as_u64().unwrap_or_default(),
		overall["cache_read_tokens"].as_u64().unwrap_or_default(),
	);
	println!("Cost: ${:.6}", overall["cost_usd"].as_f64().unwrap_or_default());
	Ok(())
}
