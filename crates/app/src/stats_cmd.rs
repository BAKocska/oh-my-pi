//! `omp stats` command composition.

use std::{
	fs, io,
	io::IsTerminal as _,
	net::{IpAddr, Ipv4Addr, SocketAddr},
	path::Path,
	sync::Arc,
};

use miette::{IntoDiagnostic as _, miette};
use omp_core::{Str, sf};
use omp_driver::{stats_api::StatsApi, stats_server};
use omp_storage::index::SessionIndex;
use tokio::{signal, sync::Mutex};

use crate::cli::{StatsArgs, StatsCommand};
static ACTIVE_STATS_SERVER: Mutex<Option<stats_server::RunningServer>> = Mutex::const_new(None);

/// Result of launching or locating the process-local dashboard.
pub struct DashboardLaunch {
	/// Browser-safe loopback URL for the dashboard.
	pub url:     Str,
	/// Human-facing launch status.
	pub message: Str,
}

/// Runs a statistics CLI operation against the authoritative write-time index.
pub async fn run(args: StatsArgs) -> miette::Result<()> {
	let state_dir = args
		.state_dir
		.unwrap_or(omp_core::dirs::data_dir(None).into_diagnostic()?);
	fs::create_dir_all(&state_dir).into_diagnostic()?;
	let (index, api) = open_stats(&state_dir)?;
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
			if io::stderr().is_terminal() {
				eprint!("Synchronizing write-time statistics... ");
			}
			let document = api.sync_document().map_err(|message| miette!(message))?;
			if io::stderr().is_terminal() {
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
			signal::ctrl_c().await.into_diagnostic()?;
			server.shutdown().await;
			Ok(())
		},
	}
}

fn open_stats(state_dir: &Path) -> miette::Result<(Arc<SessionIndex>, StatsApi)> {
	let index = Arc::new(
		SessionIndex::open_authoritative_reader(state_dir.join("sessions.sqlite3"))
			.into_diagnostic()?,
	);
	let api = StatsApi::new(Arc::clone(&index), state_dir.join("stats-sync.lock"));
	Ok((index, api))
}

/// Starts the local dashboard once per process and opens it in the browser.
pub async fn launch_dashboard(
	state_dir: &Path,
	flags: &[(Str, Option<Str>)],
) -> miette::Result<DashboardLaunch> {
	fs::create_dir_all(state_dir).into_diagnostic()?;
	let mut host = IpAddr::V4(Ipv4Addr::LOCALHOST);
	let mut port = stats_server::DEFAULT_PORT;
	for (flag, value) in flags {
		match flag.as_str() {
			"--host" => {
				let value = value
					.as_ref()
					.ok_or_else(|| miette!("Missing host. Usage: /stats [--host HOST] [--port PORT]"))?;
				host = value
					.parse::<IpAddr>()
					.map_err(|_| miette!("--host must be an IP address"))?;
			},
			"--port" => {
				let value = value
					.as_ref()
					.ok_or_else(|| miette!("Missing port. Usage: /stats [--host HOST] [--port PORT]"))?;
				port = value
					.parse::<u16>()
					.map_err(|_| miette!("Invalid port: {value}"))?;
			},
			unknown => {
				return Err(miette!(
					"Unknown option: {unknown}. Usage: /stats [--host HOST] [--port PORT]"
				));
			},
		}
	}

	let requested = SocketAddr::new(host, port);
	let mut active = ACTIVE_STATS_SERVER.lock().await;
	if let Some(server) = active.as_ref() {
		let address = server.address();
		let url = dashboard_url(address);
		omp_core::open::open_path(url.as_str());
		let message = if requested == address {
			sf!("Dashboard available at: {url}")
		} else {
			sf!("Dashboard already running at: {url} (requested {requested} ignored)")
		};
		return Ok(DashboardLaunch { url, message });
	}

	let (index, api) = open_stats(state_dir)?;
	api.sync_document().map_err(|message| miette!(message))?;
	let server = match stats_server::start(
		stats_server::Config {
			address:    requested,
			auth_token: None,
			state_dir:  state_dir.to_path_buf(),
		},
		index,
	)
	.await
	{
		Ok(server) => server,
		Err(stats_server::Error::AlreadyRunning { address }) => {
			let url = dashboard_url(address);
			omp_core::open::open_path(url.as_str());
			return Ok(DashboardLaunch { message: sf!("Dashboard already running at: {url}"), url });
		},
		Err(error) => return Err(error).into_diagnostic(),
	};
	let url = dashboard_url(server.address());
	omp_core::open::open_path(url.as_str());
	let message = sf!("Dashboard available at: {url}");
	*active = Some(server);
	Ok(DashboardLaunch { url, message })
}

fn dashboard_url(address: SocketAddr) -> Str {
	let host = if address.ip().is_unspecified() {
		IpAddr::V4(Ipv4Addr::LOCALHOST)
	} else {
		address.ip()
	};
	sf!("http://{host}:{}", address.port())
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
