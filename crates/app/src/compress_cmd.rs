//! Terminal presentation adapter for the driver-owned compression workflow.

use std::{path::Path, sync::Arc};

use miette::IntoDiagnostic as _;
use omp_driver::compress::{
	CompressArgs, Status,
	production::{CompressProgress, ProductionCompressHost},
};
use tokio_util::sync::CancellationToken;

struct TerminalCompressProgress;

impl CompressProgress for TerminalCompressProgress {
	fn update(&self, completed: usize, total: usize, path: &Path, status: Status) {
		eprintln!("[{completed}/{total}] {}: {status:?}", path.display());
	}
}

/// Runs the driver-owned compression workflow with terminal progress.
pub async fn run(args: CompressArgs) -> miette::Result<()> {
	let root = std::env::current_dir().into_diagnostic()?;
	let data_dir = omp_core::dirs::data_dir(None).into_diagnostic()?;
	let host =
		ProductionCompressHost::open(root.clone(), data_dir, Arc::new(TerminalCompressProgress))
			.await
			.into_diagnostic()?;
	let cancel = CancellationToken::new();
	let exit = omp_driver::compress::run(&args, &root, &host, &cancel)
		.await
		.into_diagnostic()?;
	println!(
		"Compressed {} file(s): {} → {} tokens.",
		exit.files.len(),
		exit.source_tokens,
		exit.draft_tokens
	);
	Ok(())
}
