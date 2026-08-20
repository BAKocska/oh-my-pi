//! Read-only model catalog commands over the validated embedded registry.

use miette::miette;
use omp_llm_catalog::{ModelSpec, snapshot::Catalog};

use crate::usage_error::CliUsageError;

/// Runs a model catalog operation. Remote refresh is intentionally explicit:
/// this binary currently ships only the verified immutable snapshot and has no
/// provider-discovery refresh backend to invoke.
pub fn run(args: &crate::cli::ModelsArgs) -> miette::Result<()> {
	let catalog = Catalog::try_embedded().map_err(|error| miette!(error.to_string()))?;
	match args.command.as_ref() {
		None => print_rows(&select(catalog, args.filter.as_deref(), args.role), args.json),
		Some(crate::cli::ModelsCommand::List { filter, json, role }) => {
			print_rows(&select(catalog, filter.as_deref(), *role), *json)
		},
		Some(crate::cli::ModelsCommand::Find { pattern, json }) => {
			print_rows(&select(catalog, Some(pattern), None), *json)
		},
		Some(crate::cli::ModelsCommand::Refresh) => Err(
			CliUsageError::new(
				"model refresh is unavailable: no provider discovery backend is configured",
			)
			.into(),
		),
	}
}

fn select<'a>(
	catalog: &'a Catalog,
	filter: Option<&str>,
	role: Option<crate::cli::ModelRole>,
) -> Vec<&'a ModelSpec> {
	let needle = filter.unwrap_or_default().to_ascii_lowercase();
	let mut rows = catalog
		.models()
		.iter()
		.filter(|model| {
			needle.is_empty()
				|| model.key.as_str().to_ascii_lowercase().contains(&needle)
				|| model
					.display_name
					.as_str()
					.to_ascii_lowercase()
					.contains(&needle)
				|| model
					.routes
					.iter()
					.filter_map(|id| catalog.routes().iter().find(|route| route.id == *id))
					.any(|route| {
						route
							.provider
							.as_str()
							.to_ascii_lowercase()
							.contains(&needle)
					})
		})
		.collect::<Vec<_>>();
	if let Some(role) = role {
		let index = match role {
			crate::cli::ModelRole::Primary => 0,
			crate::cli::ModelRole::Smol => 1,
			crate::cli::ModelRole::Slow => 2,
			crate::cli::ModelRole::Plan => 3,
		};
		rows = rows
			.get(index % rows.len().max(1))
			.into_iter()
			.copied()
			.collect();
	}
	rows
}

fn print_rows(rows: &[&ModelSpec], json: bool) -> miette::Result<()> {
	if json {
		println!(
			"{}",
			serde_json::to_string_pretty(rows).map_err(|error| miette!(error.to_string()))?
		);
		return Ok(());
	}
	for model in rows {
		println!(
			"{}\t{}\tcontext={}\tmax_output={}\tthinking={}",
			model.key,
			model.display_name,
			model
				.limits
				.context_window
				.map_or_else(|| "?".into(), |value| value.to_string()),
			model
				.limits
				.maximum_output_tokens
				.map_or_else(|| "?".into(), |value| value.to_string()),
			model.thinking.as_ref().map_or("no", |_| "yes")
		);
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn finds_a_model_by_case_insensitive_key_or_display_name() {
		let catalog = Catalog::embedded();
		let first = catalog.models().first().expect("embedded model");
		let prefix = &first.key.as_str()[..3.min(first.key.as_str().len())];
		assert!(select(catalog, Some(&prefix.to_ascii_uppercase()), None).contains(&first));
	}
}
