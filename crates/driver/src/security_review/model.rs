//! Strict, minimal result model for local security review.

use omp_core::Str;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Actionable severity assigned by the local reviewer.
#[derive(
	Clone,
	Copy,
	Debug,
	Deserialize,
	Eq,
	Ord,
	PartialEq,
	PartialOrd,
	Serialize,
	strum::Display,
	strum::EnumString,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum Severity {
	/// Direct, broadly exploitable compromise or destructive impact.
	Critical,
	/// Serious exploitable confidentiality, integrity, or availability impact.
	High,
	/// Exploitable impact requiring meaningful preconditions.
	Medium,
	/// Narrow but concrete exploitable impact.
	Low,
}

/// Inclusive one-based source line range.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SourceRange {
	/// First affected source line.
	pub start_line: u32,
	/// Last affected source line.
	pub end_line:   u32,
}

/// One evidence-backed exploitable defect.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
	/// Reviewer-assigned severity.
	pub severity:    Severity,
	/// Concise defect title.
	pub title:       Str,
	/// Workspace-relative source path.
	pub path:        Str,
	/// Inclusive source range.
	pub range:       SourceRange,
	/// Concrete source-to-sink or broken-control evidence.
	pub evidence:    Str,
	/// Credible attacker impact.
	pub impact:      Str,
	/// Short corrective guidance, without automated remediation.
	pub remediation: Str,
}

/// Complete findings-first child result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewOutput {
	/// Actionable findings, ordered highest severity first.
	pub findings: Vec<Finding>,
	/// Concise coverage summary rendered only after findings.
	pub summary:  Str,
}

/// Returns the strict JSON Schema supplied to the ordinary child-agent runtime.
pub fn strict_result_schema() -> Value {
	json!({
		"type": "object",
		"additionalProperties": false,
		"required": ["findings", "summary"],
		"properties": {
			"findings": {
				"type": "array",
				"items": {
					"type": "object",
					"additionalProperties": false,
					"required": [
						"severity", "title", "path", "range", "evidence", "impact",
						"remediation"
					],
					"properties": {
						"severity": { "enum": ["critical", "high", "medium", "low"] },
						"title": { "type": "string", "minLength": 1 },
						"path": { "type": "string", "minLength": 1 },
						"range": {
							"type": "object",
							"additionalProperties": false,
							"required": ["startLine", "endLine"],
							"properties": {
								"startLine": { "type": "integer", "minimum": 1 },
								"endLine": { "type": "integer", "minimum": 1 }
							}
						},
						"evidence": { "type": "string", "minLength": 1 },
						"impact": { "type": "string", "minLength": 1 },
						"remediation": { "type": "string", "minLength": 1 }
					}
				}
			},
			"summary": { "type": "string", "minLength": 1 }
		}
	})
}
