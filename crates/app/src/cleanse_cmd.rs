//! Terminal presentation adapter for the driver-owned cleanse workflow.

use std::{env, future::Future, pin::Pin, sync::Arc};

use miette::IntoDiagnostic as _;
use omp_core::Str;
use omp_driver::cleanse::{
	Checker, CleanseArgs, CleanseStatus, TargetChoice,
	production::{CleansePresentation, PresentationError, ProductionCleanseHost},
};
use tokio_util::sync::CancellationToken;

use crate::pickers;

struct TerminalCleansePresentation;

impl CleansePresentation for TerminalCleansePresentation {
	fn pick_target<'a>(
		&'a self,
		checkers: &'a [Checker],
		cancel: &'a CancellationToken,
	) -> Pin<Box<dyn Future<Output = Result<TargetChoice, PresentationError>> + 'a>> {
		Box::pin(async move {
			tokio::select! {
				() = cancel.cancelled() => Ok(TargetChoice::Cancel),
				choice = crate::pickers::pick_cleanse_target(checkers) => {
					choice.map_err(|error| Box::new(error) as PresentationError)
				},
			}
		})
	}

	fn prompt_request<'a>(
		&'a self,
		cancel: &'a CancellationToken,
	) -> Pin<Box<dyn Future<Output = Result<Option<Str>, PresentationError>> + 'a>> {
		Box::pin(async move {
			if cancel.is_cancelled() {
				return Ok(None);
			}
			pickers::prompt_cleanse_request().map_err(|error| Box::new(error) as PresentationError)
		})
	}
}

/// Runs the driver-owned cleanse workflow through terminal pickers.
pub async fn run(args: CleanseArgs) -> miette::Result<()> {
	let root = env::current_dir().into_diagnostic()?;
	let data_dir = omp_core::dirs::data_dir(None).into_diagnostic()?;
	let host = ProductionCleanseHost::open(root, data_dir, Arc::new(TerminalCleansePresentation))
		.into_diagnostic()?;
	let cancel = CancellationToken::new();
	let exit = omp_driver::cleanse::run(&args, &host, &cancel)
		.await
		.into_diagnostic()?;
	match exit.status {
		CleanseStatus::Clean => println!("Cleanse completed with no remaining diagnostics."),
		CleanseStatus::Unresolved => println!(
			"Cleanse left {} file group(s) unresolved{}.",
			exit.remainder.len(),
			if exit.omitted_files == 0 {
				String::new()
			} else {
				format!(" ({} more omitted)", exit.omitted_files)
			}
		),
		CleanseStatus::Unsupported => println!("No supported cleanse checker was discovered."),
		CleanseStatus::Cancelled => println!("Cleanse cancelled."),
	}
	if exit.code == 0 {
		Ok(())
	} else {
		Err(miette::miette!("cleanse exited with status {}", exit.code))
	}
}
