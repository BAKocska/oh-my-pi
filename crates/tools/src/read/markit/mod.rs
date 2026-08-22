//! In-memory document-to-Markdown conversion.

use std::{fmt, path::Path};

use bytes::Bytes;
use omp_core::{Hash32, IntoStr, Str};
use serde::{Deserialize, Serialize};
use strum::{EnumString, IntoStaticStr};

use super::web::types::{CachedDocument, DocumentCacheLocation, DocumentCacheRequest, HttpClient};

const CONVERSION_CACHE_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+markit.1");

mod doc;
mod docx;
mod epub;
mod odf;
mod odp;
mod ods;
mod odt;
mod ooxml;
mod pdf;
mod ppt;
mod pptx;
mod rtf;
mod xls;
mod xlsx;

#[derive(Clone, Copy, EnumString, IntoStaticStr)]
#[strum(ascii_case_insensitive, serialize_all = "lowercase")]
enum Format {
	Pdf,
	Doc,
	#[strum(serialize = "docx", serialize = "docm")]
	Docx,
	Xls,
	#[strum(serialize = "xlsx", serialize = "xlsm")]
	Xlsx,
	Odt,
	Ods,
	Odp,
	Ppt,
	Pptx,
	Rtf,
	Epub,
	Html,
	Xml,
}

/// Markdown produced from a supported document.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Conversion {
	/// Converted document text.
	pub text:  Str,
	/// Optional model-facing qualification of the converted text.
	pub note:  Option<Str>,
	/// Optional title supplied by document metadata.
	///
	/// Metadata stays separate from `text`, preserving the converter's source
	/// order and model-facing Markdown.
	pub title: Option<Str>,
}

impl Conversion {
	const fn plain(text: Str) -> Self {
		Self { text, note: None, title: None }
	}
}

/// Whether a successful conversion came from persistent cache.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConversionCacheStatus {
	/// Serialized conversion was decoded from a cache hit.
	Hit,
	/// Conversion ran and was offered for atomic cache publication.
	Miss,
}

/// One typed conversion with its cache outcome and durable location.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CachedConversion {
	/// Typed converter output, identical on cache hits and misses.
	pub conversion: Conversion,
	/// Cache lookup outcome.
	pub status:     ConversionCacheStatus,
	/// Durable cache location when lookup or publication succeeded.
	pub location:   Option<DocumentCacheLocation>,
}

/// A typed document conversion failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MarkitError {
	/// A converter accepted the document but could not produce Markdown.
	Conversion {
		/// Stable converter name.
		format:  &'static str,
		/// Converter-specific failure detail.
		message: Str,
	},
}

impl MarkitError {
	/// Build a failure reported by a specific document converter.
	pub fn conversion(format: &'static str, message: impl IntoStr) -> Self {
		Self::Conversion { format, message: message.into_str() }
	}

	/// Stable name of the converter that failed.
	pub const fn format(&self) -> &'static str {
		match self {
			Self::Conversion { format, .. } => format,
		}
	}

	/// Converter-specific failure detail.
	pub fn message(&self) -> &str {
		match self {
			Self::Conversion { message, .. } => message.as_ref(),
		}
	}
}

impl fmt::Display for MarkitError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(formatter, "{} conversion failed: {}", self.format(), self.message())
	}
}

impl std::error::Error for MarkitError {}

fn convert_with_anydoc(
	bytes: &[u8],
	format: anydoc::Format,
	format_name: &'static str,
) -> Result<Str, MarkitError> {
	anydoc::to_markdown_bytes(bytes, format)
		.map(Str::new)
		.map_err(|error| MarkitError::conversion(format_name, error.to_string()))
}

fn format_from_extension(extension: &str) -> Option<Format> {
	extension.trim_start_matches('.').parse().ok()
}

/// Whether a path names a supported in-memory document format.
pub(crate) fn supports_path(path: &Path) -> bool {
	path
		.extension()
		.and_then(|extension| extension.to_str())
		.and_then(format_from_extension)
		.is_some()
}

/// Whether an extension names a supported in-memory document format.
///
/// Both `docx` and `.docx` forms are accepted.
pub(crate) fn supports_extension(extension: &str) -> bool {
	format_from_extension(extension).is_some()
}

fn cache_request(format: Format, bytes: &[u8]) -> DocumentCacheRequest {
	DocumentCacheRequest {
		source_digest:     Hash32::sum(bytes),
		converter:         format.into(),
		converter_version: CONVERSION_CACHE_VERSION,
	}
}

fn decode_cached(cached: CachedDocument) -> Option<CachedConversion> {
	let conversion = serde_json::from_slice(&cached.content).ok()?;
	Some(CachedConversion {
		conversion,
		status: ConversionCacheStatus::Hit,
		location: Some(cached.location),
	})
}

/// Converts through the application-owned persistent cache.
///
/// Only successfully typed conversions are published. Corrupt cache payloads
/// are treated as misses and replaced by the fresh successful conversion.
pub async fn convert_cached<C: HttpClient + Sync>(
	cache: &C,
	path: &Path,
	bytes: &[u8],
) -> Result<Option<CachedConversion>, MarkitError> {
	let Some(format) = path
		.extension()
		.and_then(|extension| extension.to_str())
		.and_then(format_from_extension)
	else {
		return Ok(None);
	};
	let request = cache_request(format, bytes);
	if let Some(cached) = cache.document_cache_get(request).await
		&& let Some(converted) = decode_cached(cached)
	{
		return Ok(Some(converted));
	}

	let Some(conversion) = convert_format(format, bytes)? else {
		return Ok(None);
	};
	let location = if let Ok(serialized) = serde_json::to_vec(&conversion) {
		cache
			.document_cache_put(request, Bytes::from(serialized))
			.await
			.map(|cached| cached.location)
	} else {
		None
	};
	Ok(Some(CachedConversion { conversion, status: ConversionCacheStatus::Miss, location }))
}

/// Convert one of the approved document formats to Markdown.
///
/// Unsupported extensions return `Ok(None)`. Once an extension is recognized,
/// converter failures remain typed so the caller can truthfully render the
/// original binary size rather than treating the bytes as text.
pub fn convert(path: &Path, bytes: &[u8]) -> Result<Option<Conversion>, MarkitError> {
	let Some(format) = path
		.extension()
		.and_then(|extension| extension.to_str())
		.and_then(format_from_extension)
	else {
		return Ok(None);
	};

	convert_format(format, bytes)
}

fn convert_format(format: Format, bytes: &[u8]) -> Result<Option<Conversion>, MarkitError> {
	let conversion = match format {
		Format::Pdf => pdf::convert(bytes)?,
		Format::Doc => Conversion::plain(doc::convert(bytes)?),
		Format::Docx => Conversion::plain(docx::convert(bytes)?),
		Format::Xls => Conversion::plain(xls::convert(bytes)?),
		Format::Xlsx => Conversion::plain(xlsx::convert(bytes)?),
		Format::Odt => Conversion::plain(odt::convert(bytes)?),
		Format::Ods => Conversion::plain(ods::convert(bytes)?),
		Format::Odp => Conversion::plain(odp::convert(bytes)?),
		Format::Ppt => Conversion::plain(ppt::convert(bytes)?),
		Format::Pptx => Conversion::plain(pptx::convert(bytes)?),
		Format::Rtf => Conversion::plain(rtf::convert(bytes)?),
		Format::Epub => {
			let (text, title) = epub::convert(bytes)?;
			Conversion { text, note: None, title }
		},
		Format::Html | Format::Xml => {
			let source = std::str::from_utf8(bytes)
				.map_err(|error| MarkitError::conversion("html/xml", error.to_string()))?;
			let converted = html_to_markdown_rs::convert(source, None)
				.map_err(|error| MarkitError::conversion("html/xml", error.to_string()))?;
			let text = Str::new(converted.content.unwrap_or_default());
			Conversion::plain(text)
		},
	};
	Ok(Some(conversion))
}

#[cfg(test)]
mod tests {
	use std::path::Path;

	use super::{convert, supports_path};

	#[test]
	fn converts_local_html_and_xml_as_documents() {
		for (path, source, needle) in [
			("page.html", "<h1>Title</h1><p>Body</p>", "# Title"),
			("feed.xml", "<article><h2>Entry</h2><p>Text</p></article>", "## Entry"),
		] {
			assert!(supports_path(Path::new(path)));
			let converted = convert(Path::new(path), source.as_bytes())
				.unwrap()
				.expect("recognized document");
			assert!(converted.text.contains(needle), "{path}: {}", converted.text);
		}
	}
}
