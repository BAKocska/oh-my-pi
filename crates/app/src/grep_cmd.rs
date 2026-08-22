//! Standalone projection of the same native grep engine used by tools.

use miette::IntoDiagnostic as _;
use omp_core::Str;
use omp_grep::{GrepOptions, GrepOutputMode};
use serde_json::json;

use crate::cli::GrepArgs;

/// Executes the shared regex engine and emits its model-visible projection.
pub fn run(args: GrepArgs) -> miette::Result<()> {
	let mode = if args.files {
		GrepOutputMode::FilesWithMatches
	} else if args.count {
		GrepOutputMode::Count
	} else {
		GrepOutputMode::Content
	};
	let result = omp_grep::grep(&GrepOptions {
		pattern: args.pattern,
		path: Str::new(args.path.to_string_lossy().into_owned()),
		glob: args.glob,
		ignore_case: args.ignore_case,
		multiline: args.multiline,
		hidden: args.hidden,
		gitignore: !args.no_gitignore,
		max_count: Some(args.limit),
		max_count_per_file: None,
		context_before: args.context,
		context_after: args.context,
		max_columns: None,
		mode,
		timeout_ms: args.timeout_ms,
	})
	.into_diagnostic()?;
	if args.json {
		let rows = result
			.matches
			.iter()
			.map(|matched| {
				json!({
					"path": matched.path,
					"line": matched.line_number,
					"text": matched.line,
					"truncated": matched.truncated,
				})
			})
			.collect::<Vec<_>>();
		println!(
			"{}",
			serde_json::to_string_pretty(&json!({
				"matches": rows,
				"totalMatches": result.total_matches,
				"filesWithMatches": result.files_with_matches,
				"filesSearched": result.files_searched,
				"limitReached": result.limit_reached,
			}))
			.into_diagnostic()?
		);
		return Ok(());
	}
	for matched in result.matches {
		match mode {
			GrepOutputMode::Content => {
				for context in matched.context_before {
					println!("{}:{}-{}", matched.path, context.line_number, context.line);
				}
				println!("{}:{}:{}", matched.path, matched.line_number, matched.line);
				for context in matched.context_after {
					println!("{}:{}-{}", matched.path, context.line_number, context.line);
				}
			},
			GrepOutputMode::Count => println!("{}:{}", matched.path, matched.line_number),
			GrepOutputMode::FilesWithMatches => println!("{}", matched.path),
		}
	}
	Ok(())
}
