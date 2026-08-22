//! Irreversible share-snapshot projection boundary.
//!
//! Share transports and persistence accept [`ShareProjection`], never a secret
//! session transform. This keeps placeholder keys and restoration mappings out
//! of payloads, receipts, URLs, and transport diagnostics.

use omp_secrets::redact::SecretRedactor;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::{secrets::session::SecretSessionSnapshot, settings::ExportSettings};

/// A materialized share snapshot after the configured leakage policy ran.
///
/// The inner value is intentionally private so callers cannot accidentally
/// mutate the projection with unredacted material before serialization.
pub struct ShareProjection(Value);

impl std::fmt::Debug for ShareProjection {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter
			.debug_struct("ShareProjection")
			.finish_non_exhaustive()
	}
}

impl Serialize for ShareProjection {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		self.0.serialize(serializer)
	}
}

impl ShareProjection {
	/// Applies the authoritative export policy to a fully materialized snapshot.
	///
	/// Redaction is independent of reversible provider obfuscation. Only
	/// `export.shareRedactSecrets = false` bypasses this walk.
	#[must_use]
	pub fn materialize(
		mut snapshot: Value,
		policy: ExportSettings,
		secrets: &SecretSessionSnapshot,
	) -> Self {
		if policy.share_redact_secrets {
			let mut redactor = SecretRedactor::new(secrets.rules().iter().cloned());
			redact_value(&mut snapshot, &mut redactor);
		}
		Self(snapshot)
	}
}

fn redact_value(value: &mut Value, redactor: &mut SecretRedactor) {
	match value {
		Value::String(text) => *text = redactor.redact(text),
		Value::Array(values) => {
			for value in values {
				redact_value(value, redactor);
			}
		},
		Value::Object(object) => {
			let mut redacted = Map::with_capacity(object.len());
			for (key, mut value) in std::mem::take(object) {
				redact_value(&mut value, redactor);
				redacted.insert(redactor.redact(&key), value);
			}
			*object = redacted;
		},
		Value::Null | Value::Bool(_) | Value::Number(_) => {},
	}
}
