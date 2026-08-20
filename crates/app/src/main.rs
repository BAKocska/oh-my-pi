//! OMP command-line entry point.

use std::process::ExitCode;

fn install_panic_hook() {
	std::panic::set_hook(Box::new(|info| {
		eprintln!("\x1b[31momp internal error:\x1b[0m {info}");
	}));
}

#[tokio::main]
async fn main() -> ExitCode {
	install_panic_hook();
	if std::env::args_os()
		.nth(1)
		.is_some_and(|arg| arg == omp_app::envd::EVAL_CHILD_ARG)
	{
		return match omp_app::envd::run_eval_child_entry().await {
			Ok(()) => ExitCode::SUCCESS,
			Err(error) => {
				eprintln!("omp eval child: {error}");
				ExitCode::FAILURE
			},
		};
	}
	if std::env::args_os()
		.nth(1)
		.is_some_and(|arg| arg == omp_app::exthost::EXT_HOST_ARG)
	{
		return match omp_app::exthost::run_ext_host_entry() {
			Ok(()) => ExitCode::SUCCESS,
			Err(error) => {
				eprintln!("omp extension host: {error}");
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
				eprintln!("omp Python worker: {error}");
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
			// Usage diagnostics are intentionally stack-free and follow the
			// conventional exit status 2; other execution failures remain 1.
			eprintln!("{error:?}");
			if error
				.downcast_ref::<omp_app::usage_error::CliUsageError>()
				.is_some()
			{
				ExitCode::from(2)
			} else {
				ExitCode::FAILURE
			}
		},
	}
}
