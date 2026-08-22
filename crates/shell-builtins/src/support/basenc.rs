//! Binary-to-text codecs used by the `base32` and `base64` builtins.

use std::collections::VecDeque;

use thiserror::Error;

/// Errors returned by a binary-to-text codec.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub(crate) enum EncodingError {
	#[error("error: invalid input")]
	InvalidInput,
}

/// Encoding selected by the shared base32/base64 frontend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Codec {
	/// RFC 4648 base32 with padding.
	Base32,
	/// RFC 4648 base64 with padding.
	Base64,
}

impl Codec {
	/// Returns all bytes accepted while decoding.
	pub(crate) const fn alphabet(self) -> &'static [u8] {
		match self {
			Self::Base32 => b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567=",
			Self::Base64 => b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789+/=",
		}
	}

	/// Encodes `input` and appends it to `output`.
	pub(crate) fn encode_into(self, input: &[u8], output: &mut VecDeque<u8>) {
		match self {
			Self::Base32 => output.extend(omp_core::encoding::base32::encode(input).into_vec()),
			Self::Base64 => output.extend(omp_core::encoding::base64::encode(input).into_vec()),
		}
	}

	/// Decodes `input` and appends it to `output` atomically.
	pub(crate) fn decode_into(
		self,
		input: &[u8],
		output: &mut Vec<u8>,
	) -> Result<(), EncodingError> {
		let original_len = output.len();
		let result = match self {
			Self::Base32 => omp_core::encoding::base32::decode(input)
				.extend_into(output)
				.map(|_| ())
				.map_err(|_| EncodingError::InvalidInput),
			Self::Base64 => decode_concatenated_base64(input, output),
		};
		if result.is_err() {
			output.truncate(original_len);
		}
		result
	}

	/// Input byte multiple that produces an unpadded encoding.
	pub(crate) const fn unpadded_multiple(self) -> usize {
		match self {
			Self::Base32 => 5,
			Self::Base64 => 3,
		}
	}

	/// Encoded character multiple required for decoding.
	pub(crate) const fn valid_decoding_multiple(self) -> usize {
		match self {
			Self::Base32 => 8,
			Self::Base64 => 4,
		}
	}

	/// Reports whether complete quanta may be decoded before EOF.
	pub(crate) const fn supports_partial_decode(self) -> bool {
		matches!(self, Self::Base32)
	}

	/// Pads a final unpadded base32 quantum.
	///
	/// The boolean reports whether invalid trailing bytes had to be discarded.
	pub(crate) fn pad_remainder(self, remainder: &[u8]) -> Option<(Vec<u8>, bool)> {
		if self != Self::Base32 || remainder.is_empty() || remainder.contains(&b'=') {
			return None;
		}
		const VALID_LENGTHS: [usize; 4] = [2, 4, 5, 7];
		let mut length = remainder.len();
		while length > 0 && !VALID_LENGTHS.contains(&length) {
			length -= 1;
		}
		if length == 0 {
			return None;
		}
		let mut chunk = remainder[..length].to_vec();
		chunk.resize(self.valid_decoding_multiple(), b'=');
		Some((chunk, length != remainder.len()))
	}
}

fn decode_concatenated_base64(input: &[u8], output: &mut Vec<u8>) -> Result<(), EncodingError> {
	let mut start = 0;
	while start < input.len() {
		let remaining = &input[start..];
		if let Some(equal) = remaining.iter().position(|&byte| byte == b'=') {
			let segment_len = (equal / 4 + 1) * 4;
			if segment_len > remaining.len() {
				return Err(EncodingError::InvalidInput);
			}
			omp_core::encoding::base64::decode(&remaining[..segment_len])
				.extend_into(output)
				.map_err(|_| EncodingError::InvalidInput)?;
			start += segment_len;
		} else {
			let decoder = if remaining.len().is_multiple_of(4) {
				omp_core::encoding::base64::decode(remaining)
			} else {
				omp_core::encoding::base64::decode_raw(remaining)
			};
			decoder
				.extend_into(output)
				.map_err(|_| EncodingError::InvalidInput)?;
			break;
		}
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn codecs_round_trip() {
		for codec in [Codec::Base32, Codec::Base64] {
			let mut encoded = VecDeque::new();
			codec.encode_into(b"hello world", &mut encoded);
			let mut decoded = Vec::new();
			codec
				.decode_into(encoded.make_contiguous(), &mut decoded)
				.unwrap();
			assert_eq!(decoded, b"hello world", "{codec:?}");
		}
	}

	#[test]
	fn documented_vectors() {
		let mut base32 = VecDeque::new();
		Codec::Base32.encode_into(b"foobar", &mut base32);
		assert_eq!(base32.make_contiguous(), b"MZXW6YTBOI======");

		let mut base64 = VecDeque::new();
		Codec::Base64.encode_into(b"foobar", &mut base64);
		assert_eq!(base64.make_contiguous(), b"Zm9vYmFy");
	}

	#[test]
	fn concatenated_base64_decodes() {
		let mut decoded = Vec::new();
		Codec::Base64
			.decode_into(b"MTIzNA==MTIzNA==", &mut decoded)
			.unwrap();
		assert_eq!(decoded, b"12341234");
	}
}
