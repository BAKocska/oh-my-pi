//! Bounded pure-Rust PDF page rasterization.

use bytes::Bytes;
use hayro::{
	RenderCache, RenderSettings, hayro_interpret::InterpreterSettings, hayro_syntax::Pdf, render,
};
use omp_core::Str;

/// Maximum encoded PDF accepted by the rasterizer.
pub const MAX_PDF_INPUT_BYTES: usize = 20 * 1024 * 1024;
/// Maximum page count accepted before selecting a page.
pub const MAX_PDF_PAGES: usize = 2_000;
/// Maximum rendered pixel count.
pub const MAX_PDF_PAGE_PIXELS: usize = 1_500_000;
/// Maximum PNG result size.
pub const MAX_PDF_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
/// Maximum edge of a rendered page.
pub const MAX_PDF_EDGE: usize = 1_568;

/// One bounded raster result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RasterizedPage {
	/// PNG-encoded page bytes.
	pub data:        Bytes,
	/// One-based page number.
	pub page:        usize,
	/// Total pages in the document.
	pub total_pages: usize,
	/// Rendered width.
	pub width:       usize,
	/// Rendered height.
	pub height:      usize,
	/// Media type of `data`.
	pub media_type:  Str,
}

/// PDF page rasterization failure.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PdfRasterError {
	/// Encoded source crossed the pre-decode ceiling.
	#[error("PDF input exceeds the {max_bytes}-byte raster limit")]
	InputTooLarge {
		/// Configured ceiling.
		max_bytes: usize,
	},
	/// The source was not a supported PDF.
	#[error("PDF could not be parsed for page rasterization")]
	Invalid,
	/// Excessive page trees are rejected before selecting content.
	#[error("PDF has {pages} pages, above the {max_pages}-page raster limit")]
	TooManyPages {
		/// Actual page count.
		pages:     usize,
		/// Configured ceiling.
		max_pages: usize,
	},
	/// The selected page does not exist.
	#[error("PDF page {page} is out of range; document has {total_pages} pages")]
	PageOutOfRange {
		/// Requested one-based page.
		page:        usize,
		/// Document page count.
		total_pages: usize,
	},
	/// Invalid dimensions were reported by the page.
	#[error("PDF page dimensions are invalid")]
	InvalidDimensions,
	/// The encoded output crossed its bounded ceiling.
	#[error("rendered PDF page exceeds the {max_bytes}-byte output limit")]
	OutputTooLarge {
		/// Configured ceiling.
		max_bytes: usize,
	},
	/// PNG encoding failed.
	#[error("rendered PDF page could not be encoded as PNG")]
	Encode,
}

/// Renders one one-based page to PNG after enforcing input, page-count,
/// geometry, raw-output, and edge bounds.
pub fn rasterize_page(input: Bytes, page: usize) -> Result<RasterizedPage, PdfRasterError> {
	if input.len() > MAX_PDF_INPUT_BYTES {
		return Err(PdfRasterError::InputTooLarge { max_bytes: MAX_PDF_INPUT_BYTES });
	}
	let pdf = Pdf::new(input.to_vec()).map_err(|_| PdfRasterError::Invalid)?;
	let pages = pdf.pages();
	let total_pages = pages.len();
	if total_pages > MAX_PDF_PAGES {
		return Err(PdfRasterError::TooManyPages {
			pages:     total_pages,
			max_pages: MAX_PDF_PAGES,
		});
	}
	let selected = page
		.checked_sub(1)
		.and_then(|index| pages.get(index))
		.ok_or(PdfRasterError::PageOutOfRange { page, total_pages })?;
	let (width, height) = selected.render_dimensions();
	if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
		return Err(PdfRasterError::InvalidDimensions);
	}
	let edge_scale = (MAX_PDF_EDGE as f32 / width)
		.min(MAX_PDF_EDGE as f32 / height)
		.min(2.0);
	let pixel_scale = (MAX_PDF_PAGE_PIXELS as f32 / (width * height)).sqrt();
	let scale = edge_scale.min(pixel_scale).min(1.5);
	let render_width = (width * scale).ceil() as usize;
	let render_height = (height * scale).ceil() as usize;
	let pixels = render_width
		.checked_mul(render_height)
		.ok_or(PdfRasterError::InvalidDimensions)?;
	let raw_bytes = pixels
		.checked_mul(4)
		.ok_or(PdfRasterError::InvalidDimensions)?;
	if pixels > MAX_PDF_PAGE_PIXELS || raw_bytes > MAX_PDF_OUTPUT_BYTES {
		return Err(PdfRasterError::OutputTooLarge { max_bytes: MAX_PDF_OUTPUT_BYTES });
	}
	let settings = RenderSettings { x_scale: scale, y_scale: scale, ..RenderSettings::default() };
	let pixmap = render(selected, &RenderCache::new(), &InterpreterSettings::default(), &settings);
	let data = pixmap.into_png().map_err(|_| PdfRasterError::Encode)?;
	if data.len() > MAX_PDF_OUTPUT_BYTES {
		return Err(PdfRasterError::OutputTooLarge { max_bytes: MAX_PDF_OUTPUT_BYTES });
	}
	Ok(RasterizedPage {
		data: Bytes::from(data),
		page,
		total_pages,
		width: render_width,
		height: render_height,
		media_type: Str::new_static("image/png"),
	})
}
