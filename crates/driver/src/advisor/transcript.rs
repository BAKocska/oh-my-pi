//! Per-advisor JSONL persistence and session-statistics attribution.

use std::{
	collections::BTreeMap,
	fs::{self, File, OpenOptions},
	io::{self, Write},
	path::{Path, PathBuf},
};

use omp_core::Str;
use serde::{Deserialize, Serialize};

/// Usage and integer cost attributed only to one advisor transcript.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AdvisorUsageTotals {
	/// Fresh input tokens.
	pub input_tokens:       u64,
	/// Reused prompt-cache input.
	pub cache_read_tokens:  u64,
	/// Prompt-cache writes.
	pub cache_write_tokens: u64,
	/// Generated output tokens.
	pub output_tokens:      u64,
	/// Integer micro-US dollars charged to advisor inference.
	pub cost_micro_usd:     i128,
}

impl AdvisorUsageTotals {
	fn accumulate(&mut self, other: Self) {
		self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
		self.cache_read_tokens = self
			.cache_read_tokens
			.saturating_add(other.cache_read_tokens);
		self.cache_write_tokens = self
			.cache_write_tokens
			.saturating_add(other.cache_write_tokens);
		self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
		self.cost_micro_usd = self.cost_micro_usd.saturating_add(other.cost_micro_usd);
	}
}

/// One append-only advisor transcript record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AdvisorTranscriptRecord {
	/// Epoch-millisecond observation time.
	pub timestamp_ms: u64,
	/// Stable advisor child id.
	pub advisor_id:   Str,
	/// Record kind (`prompt`, `assistant`, `tool`, or `error`).
	pub kind:         Str,
	/// Secret-obfuscated model-visible body.
	pub content:      Str,
	/// Usage and cost contributed by this record.
	pub usage:        AdvisorUsageTotals,
}

/// Sink used by the app's session-statistics authority.
pub trait AdvisorStatisticsSink: Clone + Send + Sync + 'static {
	/// Attributes one advisor usage delta to the owning primary session.
	fn record_advisor_usage(
		&self,
		primary_session: &str,
		advisor_id: &str,
		usage: AdvisorUsageTotals,
	);
}

/// A no-op statistics sink for hosts without a statistics authority.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopAdvisorStatistics;

impl AdvisorStatisticsSink for NoopAdvisorStatistics {
	fn record_advisor_usage(&self, _: &str, _: &str, _: AdvisorUsageTotals) {}
}

/// Advisor transcript persistence failure.
#[derive(Debug, thiserror::Error)]
pub enum AdvisorTranscriptError {
	/// Transcript directory or file I/O failed.
	#[error("advisor transcript I/O failed")]
	Io(#[from] io::Error),
	/// A typed record could not be encoded as JSON.
	#[error("advisor transcript serialization failed")]
	Json(#[from] serde_json::Error),
}

/// Per-primary-session transcript writer with per-advisor usage totals.
pub struct AdvisorTranscriptStore<S = NoopAdvisorStatistics> {
	root:            PathBuf,
	primary_session: Str,
	statistics:      S,
	totals:          BTreeMap<Str, AdvisorUsageTotals>,
}

impl<S: AdvisorStatisticsSink> AdvisorTranscriptStore<S> {
	/// Opens `.omp/advisors/<primary-session>/` under the project root.
	pub fn open(
		project_root: &Path,
		primary_session: impl Into<Str>,
		statistics: S,
	) -> Result<Self, AdvisorTranscriptError> {
		let primary_session = primary_session.into();
		let root = project_root
			.join(".omp")
			.join("advisors")
			.join(safe_component(primary_session.as_str()));
		fs::create_dir_all(&root)?;
		Ok(Self { root, primary_session, statistics, totals: Default::default() })
	}

	/// Appends and flushes one JSONL record before updating in-memory totals.
	pub fn append(
		&mut self,
		record: &AdvisorTranscriptRecord,
	) -> Result<(), AdvisorTranscriptError> {
		let path = self.path_for(record.advisor_id.as_str());
		let mut file = append_file(&path)?;
		serde_json::to_writer(&mut file, record)?;
		file.write_all(b"\n")?;
		file.flush()?;
		let total = self.totals.entry(record.advisor_id.clone()).or_default();
		total.accumulate(record.usage);
		self.statistics.record_advisor_usage(
			self.primary_session.as_str(),
			record.advisor_id.as_str(),
			record.usage,
		);
		Ok(())
	}

	/// Returns historical totals observed by this store instance.
	pub fn totals(&self, advisor_id: &str) -> AdvisorUsageTotals {
		self.totals.get(advisor_id).copied().unwrap_or_default()
	}

	/// Returns the stable JSONL path for one advisor id.
	pub fn path_for(&self, advisor_id: &str) -> PathBuf {
		self
			.root
			.join(format!("{}.jsonl", safe_component(advisor_id)))
	}
}

fn append_file(path: &Path) -> io::Result<File> {
	OpenOptions::new().create(true).append(true).open(path)
}

fn safe_component(value: &str) -> String {
	let mut safe = String::with_capacity(value.len().max(1));
	for character in value.chars() {
		if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
			safe.push(character);
		} else {
			safe.push('-');
		}
	}
	if safe.is_empty() {
		safe.push_str("advisor");
	}
	safe
}
