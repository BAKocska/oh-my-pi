//! Model benchmark command over the normal inference registry and receipts.

use std::time::{Duration, Instant};

use futures::{StreamExt as _, stream};
use miette::{IntoDiagnostic as _, miette};
use omp_catalog::ModelKey;
use omp_core::Str;
use omp_inference::{
	Client,
	call::{CallMeta, Target},
	event::ChatEvent,
	id::RequestId,
	receipt::ExecutionBudget,
};
use serde::Serialize;

use crate::cli::BenchArgs;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Sample {
	run:               u32,
	cache_phase:       &'static str,
	ttft_ms:           u64,
	elapsed_ms:        u64,
	output_tokens:     u64,
	cache_read_tokens: u64,
	tokens_per_second: f64,
}

/// Executes bounded cold/warm pairs through production credentials, routing,
/// retries, and final accounting receipts.
pub async fn run(args: BenchArgs) -> miette::Result<()> {
	if args.runs == 0 || args.par == 0 || args.max_tokens == 0 {
		return Err(miette!("--runs, --par, and --max-tokens must be greater than zero"));
	}
	let data_dir = omp_core::dirs::data_dir(args.data_dir).into_diagnostic()?;
	let store = omp_driver::registry::open_credential_store(data_dir.join("credentials.db"))
		.into_diagnostic()?;
	let registry = omp_driver::registry::production_registry(&data_dir, store)
		.await
		.into_diagnostic()?;
	let model = ModelKey::from(args.model);
	let prompt = args.prompt;
	let max_tokens = args.max_tokens;
	let jobs = (0..args.runs).map(|run| {
		let registry = registry.clone();
		let model = model.clone();
		let prompt = prompt.clone();
		async move { sample(registry, model, prompt, max_tokens, run).await }
	});
	let mut samples = stream::iter(jobs)
		.buffer_unordered(args.par)
		.collect::<Vec<_>>()
		.await
		.into_iter()
		.collect::<miette::Result<Vec<_>>>()?;
	samples.sort_by_key(|sample| sample.run);
	if args.json {
		println!("{}", serde_json::to_string_pretty(&samples).into_diagnostic()?);
		return Ok(());
	}
	for sample in &samples {
		println!(
			"run {:>3} {:>4}: ttft {:>6} ms, {:>8.2} tok/s, {} output token(s), {} cache-read \
			 token(s)",
			sample.run + 1,
			sample.cache_phase,
			sample.ttft_ms,
			sample.tokens_per_second,
			sample.output_tokens,
			sample.cache_read_tokens,
		);
	}
	Ok(())
}

async fn sample(
	registry: omp_inference::Registry,
	model: ModelKey,
	prompt: Str,
	max_tokens: u64,
	run: u32,
) -> miette::Result<Sample> {
	let planner = omp_inference::router::Router::new(registry.clone(), Duration::from_secs(30));
	let meta = CallMeta {
		id:       RequestId::from(format!("omp-bench-{run}")),
		target:   Target::Model(model),
		deadline: None,
		budget:   ExecutionBudget::default(),
		session:  None,
	};
	let mut request = crate::cli::chat_request(prompt);
	request.max_output_tokens = Some(max_tokens);
	let mut events = Client::new(registry.service(), planner, meta)
		.execute(request)
		.await
		.into_diagnostic()?;
	let started = Instant::now();
	let mut first = None;
	let mut completion = None;
	while let Some(event) = events.next().await {
		match event.into_diagnostic()? {
			ChatEvent::TextDelta { .. } => {
				first.get_or_insert_with(Instant::now);
			},
			ChatEvent::Completed(done) => completion = Some(done),
			_ => {},
		}
	}
	let ended = Instant::now();
	let completion =
		completion.ok_or_else(|| miette!("benchmark stream ended without completion"))?;
	let first = first.unwrap_or(ended);
	let generation_seconds = ended.duration_since(first).as_secs_f64();
	let output_tokens = completion.usage.output_tokens;
	Ok(Sample {
		run,
		cache_phase: if run % 2 == 0 { "cold" } else { "warm" },
		ttft_ms: millis(first.duration_since(started)),
		elapsed_ms: millis(ended.duration_since(started)),
		output_tokens,
		cache_read_tokens: completion.usage.cache_read_tokens,
		tokens_per_second: if generation_seconds > 0.0 {
			output_tokens as f64 / generation_seconds
		} else {
			0.0
		},
	})
}

fn millis(duration: Duration) -> u64 {
	u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
