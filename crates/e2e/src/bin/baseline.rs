//! Executable P8 performance recorder entry point.
//!
//! Measurement and artifact logic lives in [`omp_e2e::baseline`]; this bin
//! only parses the artifact path and records a run at default sizes.

use std::{env, path::PathBuf};

use omp_e2e::{
	Context as _, Result,
	baseline::{measure, write_metrics},
	error,
};

const DEFAULT_FRAME_TOKENS: usize = 2_048;
const DEFAULT_LOOP_TOKENS: usize = 8_192;
const DEFAULT_SAMPLES: usize = 5;

fn artifact_argument() -> Result<PathBuf> {
	let mut args = env::args_os().skip(1);
	let Some(flag) = args.next() else {
		return Err(error("usage: baseline --artifact <path>"));
	};
	if flag != "--artifact" {
		return Err(error("expected --artifact <path>"));
	}
	let path = args.next().context("--artifact requires a path")?;
	if args.next().is_some() {
		return Err(error("unexpected arguments after artifact path"));
	}
	Ok(path.into())
}

#[tokio::main]
async fn main() -> Result<()> {
	let artifact = artifact_argument()?;
	let metrics =
		Box::pin(measure(DEFAULT_FRAME_TOKENS, DEFAULT_LOOP_TOKENS, DEFAULT_SAMPLES)).await?;
	write_metrics(&artifact, &metrics)?;
	println!("{}", serde_json::to_string(&metrics)?);
	if metrics.r#loop.gross_regression {
		return Err(error(format!(
			"full-loop throughput regressed {:.2}x versus raw scripted TurnClient (limit {:.2}x)",
			metrics.r#loop.slowdown_ratio, metrics.r#loop.regression_limit
		)));
	}
	Ok(())
}
