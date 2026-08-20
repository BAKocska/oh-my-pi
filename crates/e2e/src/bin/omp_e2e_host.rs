//! Worker-capable OMP executable used by cross-crate acceptance proofs.

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
	if std::env::args_os()
		.nth(1)
		.is_some_and(|arg| arg == omp_app::exthost::EXT_HOST_ARG)
	{
		return match omp_app::exthost::run_ext_host_entry() {
			Ok(()) => ExitCode::SUCCESS,
			Err(error) => {
				eprintln!("omp e2e extension host: {error}");
				ExitCode::FAILURE
			},
		};
	}
	if std::env::args_os()
		.nth(1)
		.is_some_and(|arg| arg == omp_app::envd::worker::WORKER_ARG)
	{
		return match omp_app::envd::run_py_worker_entry() {
			Ok(()) => ExitCode::SUCCESS,
			Err(error) => {
				eprintln!("omp e2e Python worker: {error}");
				ExitCode::FAILURE
			},
		};
	}

	omp_telemetry::export::init();
	let result = omp_app::run().await;
	omp_telemetry::export::shutdown();
	match result {
		Ok(()) => ExitCode::SUCCESS,
		Err(error) => {
			eprintln!("{error:?}");
			ExitCode::FAILURE
		},
	}
}
