//! Durable quota-history CLI over the inference-owned account state store.

use std::time::{SystemTime, UNIX_EPOCH};

use miette::{IntoDiagnostic as _, miette};
use omp_llm_catalog::ProviderId;
use omp_llm_inference::{account::AccountStateStore, id::AccountId};
use serde_json::{Value, json};

use crate::cli::UsageArgs;

/// Renders durable quota snapshots or explicitly invalidates them.
pub fn run(args: UsageArgs) -> miette::Result<()> {
	if args.account.is_some() && args.provider.is_some() {
		return Err(miette!("--account and --provider are mutually exclusive"));
	}
	let data_dir = crate::cli::data_dir(args.data_dir)?;
	std::fs::create_dir_all(&data_dir).into_diagnostic()?;
	let store = AccountStateStore::open(data_dir.join("credentials.db")).into_diagnostic()?;
	let provider = args.provider.map(ProviderId::from);
	let account = args.account.map(AccountId::from);
	if args.invalidate {
		let removed = store
			.invalidate_usage(provider.as_deref(), account.as_deref())
			.into_diagnostic()?;
		if args.json {
			println!("{}", json!({ "invalidatedReceipts": removed }));
		} else {
			println!("invalidated {removed} durable usage receipt(s)");
		}
		return Ok(());
	}

	let mut rows = Vec::new();
	for record in store.load_accounts().into_diagnostic()? {
		if provider
			.as_ref()
			.is_some_and(|value| value != &record.provider)
			|| account
				.as_ref()
				.is_some_and(|value| value != &record.account)
		{
			continue;
		}
		let state = store.load_account(&record.account).into_diagnostic()?;
		for (window_id, window) in state.quota.windows() {
			rows.push(json!({
				"provider": record.provider.as_str(),
				"account": mask(record.account.as_str()),
				"window": window_id.as_str(),
				"consumed": window.consumed.map(|sample| sample.value),
				"remaining": window.remaining.map(|sample| sample.value),
				"limit": window.limit.map(|sample| sample.value),
				"resetAtMs": window.reset_at.and_then(|sample| unix_millis(sample.value)),
				"observedAtMs": window.receipts.last().and_then(|receipt| unix_millis(receipt.observed_at)),
				"historySamples": window.receipts.len(),
			}));
		}
	}
	if args.json {
		println!("{}", serde_json::to_string_pretty(&rows).into_diagnostic()?);
		return Ok(());
	}
	if rows.is_empty() {
		println!("no durable quota observations");
		return Ok(());
	}
	for row in rows {
		print_row(&row);
	}
	Ok(())
}

fn mask(value: &str) -> String {
	if value.len() <= 8 {
		return "********".to_owned();
	}
	format!("{}…{}", &value[..4], &value[value.len() - 4..])
}

fn unix_millis(time: SystemTime) -> Option<u64> {
	time
		.duration_since(UNIX_EPOCH)
		.ok()
		.and_then(|duration| u64::try_from(duration.as_millis()).ok())
}

fn print_row(row: &Value) {
	let consumed = row["consumed"]
		.as_u64()
		.map_or("?".to_owned(), |value| value.to_string());
	let limit = row["limit"]
		.as_u64()
		.map_or("?".to_owned(), |value| value.to_string());
	println!(
		"{:<18} {:<12} {:<20} {consumed}/{limit} ({} sample(s))",
		row["provider"].as_str().unwrap_or("unknown"),
		row["account"].as_str().unwrap_or("********"),
		row["window"].as_str().unwrap_or("unknown"),
		row["historySamples"].as_u64().unwrap_or_default(),
	);
}
