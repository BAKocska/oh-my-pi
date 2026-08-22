//! Bounded consent-only AutoQA delivery worker.

use std::{sync::Arc, time::Duration};

use http::{HeaderMap, HeaderValue, header::USER_AGENT};
use omp_llm_inference::auth::HeaderPlacement;
use omp_storage::telemetry_index::{PendingIssue, TelemetryIndex};
use serde_json::{Value, json};

use crate::envd::github_url::GithubCredentialBridge;

const ENDPOINT: &str = "https://qa.omp.sh/v1/issues";
const BATCH: usize = 4;

/// Applies an explicit UI decision against the exact displayed target
/// revision. Model-facing report arguments never reach this authority.
pub(crate) fn apply_consent(
	store: &TelemetryIndex,
	intent: omp_chat_ui::autoqa::ConsentIntent,
) -> Result<bool, omp_storage::telemetry_index::QueryError> {
	match intent.decision {
		omp_chat_ui::autoqa::Decision::Upload => {
			store.consent_upload(&intent.issue_id, &intent.revision, now_ms())
		},
		omp_chat_ui::autoqa::Decision::LocalOnly => {
			store.reject_upload(&intent.issue_id)?;
			Ok(true)
		},
	}
}

/// Starts a nonblocking worker when registry assembly runs inside Tokio.
pub(crate) fn start(store: Arc<TelemetryIndex>, credentials: Arc<GithubCredentialBridge>) {
	let Ok(runtime) = tokio::runtime::Handle::try_current() else {
		return;
	};
	runtime.spawn(async move {
		let client = wreq::Client::builder()
			.redirect(wreq::redirect::Policy::none())
			.build()
			.expect("AutoQA client");
		loop {
			let now = now_ms();
			if let Ok(pending) = store.pending_uploads(now, BATCH) {
				for issue in pending {
					deliver(&client, &store, &credentials, issue, now).await;
				}
			}
			tokio::time::sleep(Duration::from_secs(15)).await;
		}
	});
}

async fn deliver(
	client: &wreq::Client,
	store: &TelemetryIndex,
	credentials: &GithubCredentialBridge,
	pending: PendingIssue,
	now: u64,
) {
	let Some(revision) = pending.issue.consent_revision.as_deref() else {
		let _ = store.reject_upload(&pending.issue.id);
		return;
	};
	if pending.issue.rev.as_deref() != Some(revision) {
		let _ = store.reject_upload(&pending.issue.id);
		return;
	}
	let payload = String::from_utf8_lossy(&pending.payload);
	let redacted = omp_telemetry::redact::redact_sensitive_credentials(&payload);
	let body = json!({
		"issue_id": pending.issue.id,
		"session_id": pending.issue.session_id,
		"device": pending.issue.device,
		"revision": revision,
		"payload": serde_json::from_str::<Value>(&redacted).unwrap_or(Value::String(redacted)),
	});
	let mut headers = HeaderMap::new();
	headers.insert(USER_AGENT, HeaderValue::from_static("omp-autoqa/1"));
	let Ok(idempotency) = HeaderValue::from_str(&pending.issue.id) else {
		let _ = store.reject_upload(&pending.issue.id);
		return;
	};
	headers.insert("idempotency-key", idempotency);
	let Ok(Some(lease)) = credentials.lease_for("autoqa").await else {
		let _ = retry(store, &pending, now);
		return;
	};
	if lease
		.apply_header(&HeaderPlacement::bearer(), &mut headers)
		.is_err()
	{
		let _ = retry(store, &pending, now);
		return;
	}
	let response = client
		.post(ENDPOINT)
		.headers(headers)
		.json(&body)
		.send()
		.await;
	let Ok(response) = response else {
		let _ = retry(store, &pending, now);
		return;
	};
	let status = response.status().as_u16();
	if (200..300).contains(&status) {
		let acknowledgement = response
			.json::<Value>()
			.await
			.ok()
			.and_then(|value| {
				value
					.get("acknowledgement")
					.and_then(Value::as_str)
					.map(str::to_owned)
			})
			.unwrap_or_else(|| pending.issue.id.to_string());
		let _ = store.acknowledge_upload(&pending.issue.id, &acknowledgement);
	} else if status == 408 || status == 429 || status >= 500 {
		let _ = retry(store, &pending, now);
	} else {
		let _ = store.reject_upload(&pending.issue.id);
	}
}

fn retry(
	store: &TelemetryIndex,
	pending: &PendingIssue,
	now: u64,
) -> Result<(), omp_storage::telemetry_index::QueryError> {
	let exponent = pending.issue.attempt_count.min(10);
	let delay = 1_000u64
		.checked_shl(exponent)
		.unwrap_or(3_600_000)
		.min(3_600_000);
	store.record_upload_failure(&pending.issue.id, now.saturating_add(delay))
}

fn now_ms() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.unwrap_or_default()
		.as_millis()
		.try_into()
		.unwrap_or(u64::MAX)
}
