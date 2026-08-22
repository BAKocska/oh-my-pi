//! OMP-v1 collaboration endpoint validation shared by relay and future link
//! surfaces.

use thiserror::Error;
use url::Url;

/// A query-free native OMP collaboration room endpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RelayEndpoint(Url);

impl RelayEndpoint {
	/// Validates a ws/wss endpoint without query credentials or fragments.
	pub fn parse(input: &str) -> Result<Self, EndpointError> {
		let url = Url::parse(input).map_err(EndpointError::Parse)?;
		if !matches!(url.scheme(), "ws" | "wss") {
			return Err(EndpointError::Scheme);
		}
		if url.query().is_some() || url.fragment().is_some() {
			return Err(EndpointError::SecretBearingEndpoint);
		}
		Ok(Self(url))
	}

	/// Returns the validated URL.
	#[must_use]
	pub const fn as_url(&self) -> &Url {
		&self.0
	}

	/// Transfers the validated URL to a transport owner.
	#[must_use]
	pub fn into_url(self) -> Url {
		self.0
	}
}

/// Invalid collaboration relay endpoint.
#[derive(Debug, Error)]
pub enum EndpointError {
	/// URL syntax was invalid.
	#[error("invalid collaboration relay URL")]
	Parse(#[source] url::ParseError),
	/// Only WebSocket transports are accepted.
	#[error("collaboration relay URL must use ws or wss")]
	Scheme,
	/// Query and fragment data are not accepted at the transport endpoint layer.
	#[error("collaboration relay endpoint must not contain a query or fragment")]
	SecretBearingEndpoint,
}
