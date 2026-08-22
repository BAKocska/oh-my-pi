//! Environment-routed remote embedding contract.

use omp_core::Str;

use crate::{
	Error, Result,
	config::CredentialRef,
	embedding::protocol::{MAX_TEXT_BYTES, MAX_TEXTS, MAX_VECTOR_DIMENSIONS},
};

/// Bounded OpenAI-compatible embedding request. Credential contents are never
/// present.
pub struct RemoteEmbeddingRequest {
	/// Endpoint base URL.
	pub base_url:   Str,
	/// Model selector.
	pub model:      Str,
	/// Opaque Environment credential reference.
	pub credential: CredentialRef,
	/// Ordered input texts.
	pub texts:      Vec<String>,
}

impl RemoteEmbeddingRequest {
	/// Validates request count and aggregate UTF-8 bounds before egress.
	pub fn validate(&self) -> Result<()> {
		let bytes = self
			.texts
			.iter()
			.try_fold(0usize, |total, text| total.checked_add(text.len()))
			.ok_or(Error::InputTooLarge)?;
		if self.base_url.is_empty()
			|| self.model.is_empty()
			|| self.texts.is_empty()
			|| self.texts.len() > MAX_TEXTS
			|| bytes > MAX_TEXT_BYTES
		{
			return Err(Error::InputTooLarge);
		}
		Ok(())
	}
}

/// Environment-owned credential and HTTP egress boundary.
pub trait RemoteEmbeddingEgress: Send + Sync {
	/// Sends one bounded request and returns vectors in input order.
	fn embed(
		&self,
		request: RemoteEmbeddingRequest,
	) -> impl Future<Output = Result<Vec<Vec<f32>>>> + Send;
}

/// Executes and validates a remote embedding request without exposing
/// credentials to memory storage.
pub async fn embed_remote<E: RemoteEmbeddingEgress>(
	egress: &E,
	request: RemoteEmbeddingRequest,
) -> Result<Vec<Vec<f32>>> {
	request.validate()?;
	let expected = request.texts.len();
	let mut vectors = egress.embed(request).await?;
	if vectors.len() != expected
		|| vectors.iter().any(|vector| {
			vector.is_empty()
				|| vector.len() > MAX_VECTOR_DIMENSIONS
				|| vector.iter().any(|value| !value.is_finite())
		}) {
		return Err(Error::EmbeddingWorker);
	}
	for vector in &mut vectors {
		let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
		if norm == 0.0 {
			return Err(Error::EmbeddingWorker);
		}
		for value in vector {
			*value /= norm;
		}
	}
	Ok(vectors)
}
