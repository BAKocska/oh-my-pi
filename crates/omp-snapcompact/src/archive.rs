//! Provider-aware frame geometry and bounded text chunking.

use crate::{
	Result as RenderResult, SnapcompactError, SnapcompactRenderOptions, cell_units,
	render_snapcompact_png,
};

/// Maximum archive frames carried by one compaction.
pub const MAX_FRAMES_DEFAULT: usize = 80;
/// Conservative encoded size used before rendering a frame.
pub const FRAME_DATA_BYTES_ESTIMATE: usize = 170_000;
/// Maximum PNG bytes carried in rebuilt requests.
pub const FRAME_DATA_BYTES_BUDGET: usize = 3_000_000;
/// Minimum source-to-image savings required to accept an archive.
pub const SAVINGS_MARGIN: f64 = 0.9;
/// Safe image-count floor for an unknown provider.
pub const DEFAULT_PROVIDER_IMAGE_BUDGET: usize = 5;

/// Provider billing family for image inputs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BillingFamily {
	/// Anthropic patch billing.
	Anthropic,
	/// Google fixed media-resolution billing.
	Google,
	/// OpenAI patch billing.
	OpenAi,
	/// Conservative unknown-provider billing.
	Unknown,
}

/// One eval-validated rendering geometry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Shape {
	/// Bundled renderer font.
	pub font:                 &'static str,
	/// Horizontal cell advance.
	pub cell_width:           u32,
	/// Vertical cell pitch.
	pub cell_height:          u32,
	/// Whether the renderer may stretch bitmap glyphs.
	pub stretch:              Option<bool>,
	/// Ink variant.
	pub variant:              &'static str,
	/// Number of repeated copies of each line.
	pub line_repeat:          u32,
	/// Newspaper columns, either one or two.
	pub columns:              u32,
	/// Square frame edge in pixels.
	pub frame_size:           u32,
	/// Conservative provider input tokens billed per frame.
	pub frame_token_estimate: u64,
}

/// Model and transport identity used to select a shape.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ShapeTarget<'a> {
	/// Wire API name.
	pub api:      Option<&'a str>,
	/// Catalog model identifier.
	pub model_id: Option<&'a str>,
}

/// A rendered PNG and its exact reading geometry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Frame {
	/// PNG bytes.
	pub png:   Vec<u8>,
	/// Characters per row.
	pub cols:  u32,
	/// Available text rows.
	pub rows:  u32,
	/// Unicode scalar values printed in this frame.
	pub chars: usize,
	/// Geometry used to render the frame.
	pub shape: Shape,
}

/// Measured compaction accounting persisted beside an archive.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SavingsRecord {
	/// Estimated source text tokens before imaging.
	pub source_tokens: u64,
	/// Conservative provider tokens after imaging.
	pub image_tokens:  u64,
	/// Exact PNG bytes retained.
	pub png_bytes:     usize,
	/// Frames retained.
	pub frames:        usize,
	/// `image_tokens / source_tokens`.
	pub ratio:         f64,
}

/// Completed bounded archive.
#[derive(Clone, Debug, PartialEq)]
pub struct Archive {
	/// Oldest-to-newest rendered frames.
	pub frames:          Vec<Frame>,
	/// Characters dropped from the oldest side after hitting a hard budget.
	pub truncated_chars: usize,
	/// Measured savings used for admission.
	pub savings:         SavingsRecord,
}

/// Failure to construct an admissible archive.
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
	/// The bitmap renderer rejected a frame.
	#[error(transparent)]
	Renderer(#[from] SnapcompactError),
	/// Image input would not save the required ten-percent margin.
	#[error(
		"snapcompact image cost {image_tokens} tokens exceeds the {maximum_tokens}-token admission \
		 ceiling"
	)]
	InsufficientSavings {
		/// Conservative image token estimate.
		image_tokens:   u64,
		/// Largest admitted image token estimate.
		maximum_tokens: u64,
	},
}

/// Archive construction result.
pub type ArchiveResult<T, E = ArchiveError> = std::result::Result<T, E>;

/// Resolves a wire API name to its image billing family.
pub fn billing_family(api: Option<&str>) -> BillingFamily {
	match api {
		Some("anthropic-messages" | "bedrock-converse-stream") => BillingFamily::Anthropic,
		Some("google-generative-ai" | "google-gemini-cli" | "google-vertex") => BillingFamily::Google,
		Some(
			"openai-completions"
			| "openai-responses"
			| "openai-codex-responses"
			| "azure-openai-responses",
		) => BillingFamily::OpenAi,
		_ => BillingFamily::Unknown,
	}
}

/// Returns the conservative request image budget for a provider.
pub fn provider_image_budget(provider: Option<&str>) -> usize {
	match provider {
		Some("anthropic" | "amazon-bedrock" | "openrouter") => 90,
		Some("openai" | "openai-codex" | "google" | "google-vertex" | "google-gemini-cli") => 200,
		Some("umans") => 10,
		_ => DEFAULT_PROVIDER_IMAGE_BUDGET,
	}
}

/// Returns the bounded number of archive frames available to a provider.
pub fn provider_frame_budget(provider: Option<&str>, existing_images: usize) -> usize {
	provider_image_budget(provider)
		.saturating_sub(existing_images)
		.min(MAX_FRAMES_DEFAULT)
		.min((FRAME_DATA_BYTES_BUDGET / FRAME_DATA_BYTES_ESTIMATE).max(1))
}

fn billed_tokens(family: BillingFamily, frame_size: u32) -> u64 {
	match family {
		BillingFamily::Google => 1_120,
		BillingFamily::OpenAi => {
			let patches = u64::from(frame_size.div_ceil(32)).pow(2).min(10_000);
			(patches as f64 * 1.2).ceil() as u64
		},
		BillingFamily::Anthropic | BillingFamily::Unknown => {
			let patches = u64::from(frame_size.div_ceil(28)).pow(2).min(4_784);
			(patches as f64 * 1.05).ceil() as u64
		},
	}
}

/// Selects Pi's eval-winning geometry for a model and carrying API.
pub fn resolve_shape(target: ShapeTarget<'_>) -> Shape {
	let family = billing_family(target.api);
	let id = target.model_id.unwrap_or_default().to_ascii_lowercase();
	let (font, cell_width, cell_height, stretch, frame_size) = if id.contains("claude") {
		let high_resolution = id.contains("fable")
			|| id.contains("mythos")
			|| id.contains("opus-4-7")
			|| id.contains("opus-4.7")
			|| id.contains("opus-4-8")
			|| id.contains("opus-4.8");
		("8x13", 11, 16, Some(false), if high_resolution { 1_932 } else { 1_568 })
	} else if id.contains("gemini") {
		("8x13", 8, 22, Some(false), 2_048)
	} else if id.contains("glm") {
		("8x13", 8, 16, Some(false), 1_568)
	} else {
		match family {
			BillingFamily::Anthropic => ("8x13", 11, 16, Some(false), 1_568),
			BillingFamily::Google | BillingFamily::OpenAi | BillingFamily::Unknown => {
				("8x13", 8, 22, Some(false), 1_568)
			},
		}
	};
	Shape {
		font,
		cell_width,
		cell_height,
		stretch,
		variant: "bw",
		line_repeat: 1,
		columns: 1,
		frame_size,
		frame_token_estimate: billed_tokens(family, frame_size),
	}
}

fn shape_options(shape: Shape) -> SnapcompactRenderOptions {
	SnapcompactRenderOptions {
		size:        shape.frame_size,
		font:        Some(shape.font.to_owned()),
		cell_width:  Some(shape.cell_width),
		cell_height: Some(shape.cell_height),
		variant:     Some(shape.variant.to_owned()),
		line_repeat: Some(shape.line_repeat),
		stretch:     shape.stretch,
		columns:     Some(shape.columns),
	}
}

fn frame_capacity(shape: Shape) -> (usize, u32, u32) {
	let cols = shape.frame_size / shape.cell_width;
	let rows = shape.frame_size / shape.cell_height / shape.line_repeat;
	((cols as usize).saturating_mul(rows as usize), cols, rows)
}

fn take_frame_end(
	text: &str,
	start: usize,
	capacity: usize,
	cols: usize,
	wide_cells: bool,
) -> (usize, usize) {
	let mut cells = 0usize;
	let mut chars = 0usize;
	let mut end = start;
	for (offset, ch) in text[start..].char_indices() {
		let units = cell_units(ch as u32, wide_cells);
		let row_offset = cells % cols;
		let pad = usize::from(units == 2 && row_offset + units > capacity);
		if chars != 0 && cells.saturating_add(pad).saturating_add(units) > capacity {
			break;
		}
		cells = cells.saturating_add(pad).saturating_add(units);
		chars += 1;
		end = start + offset + ch.len_utf8();
	}
	(end, chars)
}

/// Renders text into provider-bounded PNG frames and enforces the 0.9 savings
/// margin.
///
/// `source_tokens` must be measured by the active model tokenizer. Frames are
/// admitted only when their conservative image bill remains at least ten
/// percent below that source measurement.
pub fn render_archive(
	text: &str,
	source_tokens: u64,
	target: ShapeTarget<'_>,
	provider: Option<&str>,
	existing_images: usize,
) -> ArchiveResult<Archive> {
	let shape = resolve_shape(target);
	let max_frames = provider_frame_budget(provider, existing_images);
	let (capacity, cols, rows) = frame_capacity(shape);
	let mut frames = Vec::with_capacity(max_frames.min(16));
	let mut cursor = 0usize;
	let mut png_bytes = 0usize;
	while cursor < text.len() && frames.len() < max_frames {
		let (end, chars) =
			take_frame_end(text, cursor, capacity, cols as usize, shape.font != "silver");
		if end == cursor {
			break;
		}
		let png = render_snapcompact_png(&text[cursor..end], &shape_options(shape))?;
		if png_bytes.saturating_add(png.len()) > FRAME_DATA_BYTES_BUDGET {
			break;
		}
		png_bytes += png.len();
		frames.push(Frame { png, cols, rows, chars, shape });
		cursor = end;
	}
	let image_tokens = shape
		.frame_token_estimate
		.saturating_mul(frames.len() as u64);
	let maximum_tokens = (source_tokens as f64 * SAVINGS_MARGIN).floor() as u64;
	if image_tokens > maximum_tokens {
		return Err(ArchiveError::InsufficientSavings { image_tokens, maximum_tokens });
	}
	let truncated_chars = text[cursor..].chars().count();
	let ratio = if source_tokens == 0 {
		0.0
	} else {
		image_tokens as f64 / source_tokens as f64
	};
	let frame_count = frames.len();
	Ok(Archive {
		frames,
		truncated_chars,
		savings: SavingsRecord { source_tokens, image_tokens, png_bytes, frames: frame_count, ratio },
	})
}

/// Calls the renderer directly for callers that already performed framing.
pub fn render_frame(text: &str, shape: Shape) -> RenderResult<Vec<u8>> {
	render_snapcompact_png(text, &shape_options(shape))
}
