//! Parallel `/v1beta/extract` request and response projection.

use omp_core::Str;
use serde::{Deserialize, Serialize};

/// Maximum URLs accepted by one extraction request.
pub const MAX_URLS: usize = 20;
/// Parallel extract beta resource.
pub const EXTRACT_PATH: &str = "/v1beta/extract";

/// Bounded extraction request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ParallelExtractRequest {
	/// Absolute URLs to extract.
	pub urls:           Box<[Str]>,
	/// Optional extraction objective.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub objective:      Option<Str>,
	/// Queries used to focus excerpts.
	#[serde(skip_serializing_if = "<[omp_core::Str]>::is_empty")]
	pub search_queries: Box<[Str]>,
	/// Request relevant excerpts.
	pub excerpts:       bool,
	/// Request complete page content.
	pub full_content:   bool,
}

impl ParallelExtractRequest {
	/// Validates API hard bounds and URL syntax.
	pub fn validate(&self) -> Result<(), ParallelExtractError> {
		if self.urls.is_empty() || self.urls.len() > MAX_URLS {
			return Err(ParallelExtractError::InvalidUrlCount);
		}
		fn slice_is_empty<T>(values: &[T]) -> bool {
			values.is_empty()
		}
		if self
			.urls
			.iter()
			.any(|url| url::Url::parse(url.as_str()).is_err())
		{
			return Err(ParallelExtractError::InvalidUrl);
		}
		Ok(())
	}
}

/// One successfully extracted document.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
pub struct ParallelExtractDocument {
	/// Canonical document URL.
	pub url:            Str,
	/// Provider title.
	#[serde(default)]
	pub title:          Option<Str>,
	/// Provider publication date.
	#[serde(default, rename = "publish_date")]
	pub published_date: Option<Str>,
	/// Focused excerpts.
	#[serde(default)]
	pub excerpts:       Box<[Str]>,
	/// Complete extracted content when requested.
	#[serde(default)]
	pub full_content:   Option<Str>,
}

impl ParallelExtractDocument {
	/// Returns excerpts joined with blank lines, then full content as fallback.
	#[must_use]
	pub fn content(&self) -> Str {
		let nonempty = self
			.excerpts
			.iter()
			.filter(|excerpt| !excerpt.trim().is_empty())
			.map(Str::as_str)
			.collect::<Vec<_>>();
		if nonempty.is_empty() {
			self.full_content.clone().unwrap_or_default()
		} else {
			Str::new(nonempty.join("\n\n"))
		}
	}
}

/// Per-URL extraction failure retained beside successful documents.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
pub struct ParallelExtractFailure {
	/// Failed URL.
	pub url:              Str,
	/// Provider error classification.
	#[serde(default)]
	pub error_type:       Option<Str>,
	/// Origin HTTP status, when known.
	#[serde(default)]
	pub http_status_code: Option<u16>,
	/// Bounded provider detail.
	#[serde(default)]
	pub content:          Option<Str>,
}

/// One provider usage counter.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
pub struct ParallelExtractUsage {
	/// Counter name.
	pub name:  Str,
	/// Counter value.
	pub count: u64,
}

/// Lossless Parallel extract result.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
pub struct ParallelExtractResult {
	/// Provider request identity.
	#[serde(default, rename = "extract_id")]
	pub request_id: Str,
	/// Successful documents.
	#[serde(default)]
	pub results:    Box<[ParallelExtractDocument]>,
	/// URL-specific errors.
	#[serde(default)]
	pub errors:     Box<[ParallelExtractFailure]>,
	/// Provider warnings.
	#[serde(default)]
	pub warnings:   Box<[Str]>,
	/// Provider usage counters.
	#[serde(default)]
	pub usage:      Box<[ParallelExtractUsage]>,
}

/// Invalid extraction request or response.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ParallelExtractError {
	/// URL count is outside the API contract.
	#[error("Parallel extract requires between one and twenty URLs")]
	InvalidUrlCount,
	/// A request URL is not absolute and valid.
	#[error("Parallel extract URL is invalid")]
	InvalidUrl,
	/// Provider response is not valid extract JSON.
	#[error("Parallel extract response is malformed")]
	MalformedResponse,
}

/// Parses the bounded JSON response while retaining partial failures.
pub fn decode_parallel_extract(
	bytes: &[u8],
) -> Result<ParallelExtractResult, ParallelExtractError> {
	serde_json::from_slice(bytes).map_err(|_| ParallelExtractError::MalformedResponse)
}
