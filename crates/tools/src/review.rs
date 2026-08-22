//! Review-finding parsing and stable priority normalization.

use omp_core::{Str, sf};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use strum::{Display, EnumString, IntoStaticStr};

/// Normalized review finding priority, ordered from release-blocking to minor.
#[derive(
	Clone,
	Copy,
	Debug,
	Display,
	EnumString,
	Eq,
	IntoStaticStr,
	Ord,
	PartialEq,
	PartialOrd,
	Serialize,
	Deserialize,
)]
#[serde(rename_all = "UPPERCASE")]
#[strum(ascii_case_insensitive)]
pub enum FindingPriority {
	/// Release-blocking defect.
	P0,
	/// High-priority defect.
	P1,
	/// Normal-priority defect.
	P2,
	/// Minor defect.
	P3,
}

impl FindingPriority {
	/// Stable zero-based severity order.
	#[must_use]
	pub const fn ordinal(self) -> u8 {
		match self {
			Self::P0 => 0,
			Self::P1 => 1,
			Self::P2 => 2,
			Self::P3 => 3,
		}
	}

	/// Stable semantic status symbol used by presentation layers.
	#[must_use]
	pub const fn status_symbol(self) -> &'static str {
		match self {
			Self::P0 => "status.error",
			Self::P1 | Self::P2 => "status.warning",
			Self::P3 => "status.info",
		}
	}
}

/// One fully validated reviewer finding.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FindingDetails {
	/// Concise imperative finding title.
	pub title:      Str,
	/// Finding explanation and impact.
	pub body:       Str,
	/// Normalized P0-P3 priority.
	pub priority:   FindingPriority,
	/// Reviewer confidence in the inclusive range 0..=1.
	pub confidence: f64,
	/// Repository-relative source path.
	pub file_path:  Str,
	/// Inclusive one-based starting line.
	pub line_start: u64,
	/// Inclusive one-based ending line.
	pub line_end:   u64,
}

/// Final correctness verdict attached to a review.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReviewVerdict {
	/// Whether the reviewed change is free of blocking findings.
	pub overall_correctness: OverallCorrectness,
	/// Short plain-text verdict summary.
	pub explanation:         Str,
	/// Reviewer confidence in the inclusive range 0..=1.
	pub confidence:          f64,
}

/// Normalized overall review correctness.
#[derive(
	Clone, Copy, Debug, Display, EnumString, Eq, IntoStaticStr, PartialEq, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
pub enum OverallCorrectness {
	/// No blocking defect was found.
	Correct,
	/// At least one blocking defect was found.
	Incorrect,
}

/// Converts canonical strings or numeric ordinals into P0-P3 priority.
#[must_use]
pub fn normalize_finding_priority(value: &Value) -> Option<FindingPriority> {
	match value {
		Value::String(value) => value.parse().ok(),
		Value::Number(value) => match value.as_u64()? {
			0 => Some(FindingPriority::P0),
			1 => Some(FindingPriority::P1),
			2 => Some(FindingPriority::P2),
			3 => Some(FindingPriority::P3),
			_ => None,
		},
		_ => None,
	}
}

/// Parses a reviewer finding, rejecting incomplete or out-of-range payloads.
#[must_use]
pub fn parse_finding_details(value: &Value) -> Option<FindingDetails> {
	let object = value.as_object()?;
	let confidence = finite_confidence(object.get("confidence")?)?;
	let line_start = positive_integer(object.get("line_start")?)?;
	let line_end = positive_integer(object.get("line_end")?)?;
	if line_end < line_start {
		return None;
	}
	Some(FindingDetails {
		title: Str::new(object.get("title")?.as_str()?),
		body: Str::new(object.get("body")?.as_str()?),
		priority: normalize_finding_priority(object.get("priority")?)?,
		confidence,
		file_path: nonempty_string(object.get("file_path")?)?,
		line_start,
		line_end,
	})
}

/// Parses the final reviewer verdict and validates its confidence bound.
#[must_use]
pub fn parse_review_verdict(value: &Value) -> Option<ReviewVerdict> {
	let object = value.as_object()?;
	Some(ReviewVerdict {
		overall_correctness: object.get("overall_correctness")?.as_str()?.parse().ok()?,
		explanation:         nonempty_string(object.get("explanation")?)?,
		confidence:          finite_confidence(object.get("confidence")?)?,
	})
}

/// Produces a deterministic one-line verdict summary for logs and compact UI.
#[must_use]
pub fn verdict_summary(verdict: &ReviewVerdict, findings: &[FindingDetails]) -> Str {
	let highest = findings.iter().map(|finding| finding.priority).min();
	match highest {
		Some(priority) => sf!(
			"{} ({:.0}% confidence): {} finding(s), highest {} — {}",
			verdict.overall_correctness,
			verdict.confidence * 100.0,
			findings.len(),
			priority,
			verdict.explanation
		),
		None => sf!(
			"{} ({:.0}% confidence): no findings — {}",
			verdict.overall_correctness,
			verdict.confidence * 100.0,
			verdict.explanation
		),
	}
}

fn finite_confidence(value: &Value) -> Option<f64> {
	let value = value.as_f64()?;
	value
		.is_finite()
		.then_some(value)
		.filter(|value| (0.0..=1.0).contains(value))
}

fn positive_integer(value: &Value) -> Option<u64> {
	value.as_u64().filter(|value| *value > 0)
}

fn nonempty_string(value: &Value) -> Option<Str> {
	let value = value.as_str()?;
	(!value.is_empty()).then(|| Str::new(value))
}
