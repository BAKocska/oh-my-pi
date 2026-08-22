//! Deterministic provider/model header merging and custom OAuth application.

use std::{collections::BTreeMap, time::SystemTime};

use bytes::Bytes;
use http::{HeaderName, HeaderValue, Request};
use omp_core::{ExposeSecret as _, SecretString, Str};

use super::{AuthSpec, CredentialApplyError, CredentialLease};

/// One dynamic secret header resolved by the Environment credential authority.
#[derive(Clone, Debug)]
pub struct SecretHeader {
	/// Header name.
	pub name:  Str,
	/// Secret-only value.
	pub value: SecretString,
}

/// Merges safe provider headers, model overrides, dynamic secret headers, then
/// catalog auth.
///
/// Model headers replace provider headers by case-insensitive HTTP identity.
/// Public layers may not contain credential-bearing header names; those must
/// use `secret_headers` or the credential lease. OAuth/API-key application runs
/// last and is therefore authoritative.
pub fn apply_custom_auth(
	request: &mut Request<Bytes>,
	provider_headers: &BTreeMap<Str, Str>,
	model_headers: &BTreeMap<Str, Str>,
	secret_headers: &[SecretHeader],
	lease: Option<(&CredentialLease, &AuthSpec)>,
	now: SystemTime,
) -> Result<(), CustomAuthApplyError> {
	for (name, value) in provider_headers.iter().chain(model_headers) {
		let name = parse_public_name(name)?;
		let value = HeaderValue::from_str(value).map_err(|_| CustomAuthApplyError::InvalidHeader)?;
		request.headers_mut().insert(name, value);
	}
	for header in secret_headers {
		let name = HeaderName::from_bytes(header.name.as_bytes())
			.map_err(|_| CustomAuthApplyError::InvalidHeader)?;
		let mut value = HeaderValue::from_bytes(header.value.expose_secret().as_bytes())
			.map_err(|_| CustomAuthApplyError::InvalidHeader)?;
		value.set_sensitive(true);
		request.headers_mut().insert(name, value);
	}
	if let Some((lease, spec)) = lease {
		lease.apply(spec, now, request)?;
	}
	Ok(())
}

fn parse_public_name(name: &str) -> Result<HeaderName, CustomAuthApplyError> {
	let parsed =
		HeaderName::from_bytes(name.as_bytes()).map_err(|_| CustomAuthApplyError::InvalidHeader)?;
	if matches!(parsed.as_str(), "authorization" | "proxy-authorization" | "cookie" | "set-cookie") {
		return Err(CustomAuthApplyError::SecretInPublicHeaders);
	}
	Ok(parsed)
}

/// Custom endpoint auth/header failure.
#[derive(Debug, thiserror::Error)]
pub enum CustomAuthApplyError {
	/// Public or secret header syntax is invalid.
	#[error("custom endpoint header is invalid")]
	InvalidHeader,
	/// A credential-bearing header was placed in the public catalog layer.
	#[error("credential-bearing custom headers must use a secret source")]
	SecretInPublicHeaders,
	/// Catalog credential application failed.
	#[error(transparent)]
	Credential(#[from] CredentialApplyError),
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn model_headers_override_provider_and_secret_values_are_sensitive() {
		let mut provider = BTreeMap::new();
		provider.insert(Str::new_static("x-route"), Str::new_static("provider"));
		let mut model = BTreeMap::new();
		model.insert(Str::new_static("x-route"), Str::new_static("model"));
		let mut request = Request::new(Bytes::new());
		apply_custom_auth(
			&mut request,
			&provider,
			&model,
			&[SecretHeader {
				name:  Str::new_static("x-api-key"),
				value: SecretString::from("secret-marker"),
			}],
			None,
			SystemTime::UNIX_EPOCH,
		)
		.expect("apply");
		assert_eq!(request.headers()["x-route"], "model");
		assert!(request.headers()["x-api-key"].is_sensitive());
		assert!(!format!("{:?}", request.headers()).contains("secret-marker"));
	}

	#[test]
	fn authorization_cannot_enter_public_catalog_headers() {
		let mut provider = BTreeMap::new();
		provider.insert(Str::new_static("authorization"), Str::new_static("secret"));
		assert!(matches!(
			apply_custom_auth(
				&mut Request::new(Bytes::new()),
				&provider,
				&BTreeMap::new(),
				&[],
				None,
				SystemTime::UNIX_EPOCH,
			),
			Err(CustomAuthApplyError::SecretInPublicHeaders)
		));
	}
}
