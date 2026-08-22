//! Token-throughput measurement shared by live and finalized status facts.

use std::time::{Duration, Instant};

const MIN_SAMPLE_DURATION: Duration = Duration::from_millis(100);
const APPROXIMATE_UTF8_BYTES_PER_TOKEN: u64 = 4;

/// Streaming token-rate accumulator.
///
/// Provider receipts remain authoritative at finalization. While a provider has
/// not reported usage, output bytes provide a bounded live estimate so the
/// status line does not remain blank for the entire generation.
#[derive(Clone, Copy, Debug)]
pub struct TokenRateMeter {
	started:        Instant,
	streamed_bytes: u64,
	final_tokens:   Option<u64>,
}

impl TokenRateMeter {
	/// Starts a fresh generation sample.
	pub fn start(now: Instant) -> Self {
		Self { started: now, streamed_bytes: 0, final_tokens: None }
	}

	/// Adds one visible provider text fragment to the live estimate.
	pub fn observe_fragment(&mut self, fragment: &str) {
		self.streamed_bytes = self
			.streamed_bytes
			.saturating_add(u64::try_from(fragment.len()).unwrap_or(u64::MAX));
	}

	/// Replaces the estimate with authoritative provider output usage.
	pub const fn finalize(&mut self, output_tokens: u64) {
		self.final_tokens = Some(output_tokens);
	}

	/// Calculates rounded tokens per second at `now`.
	pub fn rate(&self, now: Instant) -> Option<u64> {
		let elapsed = now.saturating_duration_since(self.started);
		if elapsed < MIN_SAMPLE_DURATION {
			return None;
		}
		let tokens = self.final_tokens.unwrap_or_else(|| {
			self
				.streamed_bytes
				.saturating_add(APPROXIMATE_UTF8_BYTES_PER_TOKEN - 1)
				/ APPROXIMATE_UTF8_BYTES_PER_TOKEN
		});
		if tokens == 0 {
			return None;
		}
		let rate = tokens as f64 / elapsed.as_secs_f64();
		(rate.is_finite() && rate > 0.0).then(|| rate.round() as u64)
	}
}
