//! Responsive live multi-worker wall for vibe-mode waves.
//!
//! The wall consumes immutable worker snapshots. It owns no worker lifecycle,
//! messaging, or cancellation authority; the application refreshes it from the
//! sole agent-tree projection.

use omp_core::{Str, sf};
use omp_tui::{Dim, Layer, OverlayAnchor, OverlayOptions, Size, Ui, UiContext, dom};

use crate::overlays::OverlayPanel;

const MIN_CARD_WIDTH: u16 = 28;
const MAX_CARD_WIDTH: u16 = 48;

/// One live worker card projected into the vibe wall.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VibeWorkerRow {
	/// Stable worker identity.
	pub id:     Str,
	/// Human-facing worker name.
	pub name:   Str,
	/// Current lifecycle status.
	pub status: Str,
	/// Assigned model or tier.
	pub model:  Str,
	/// Concise assigned task.
	pub task:   Str,
	/// Latest bounded output preview.
	pub output: Str,
	/// Tokens consumed when known.
	pub tokens: Option<u64>,
}

/// Retained responsive TV-wall projection for a live vibe wave.
pub struct VibeWall {
	ui:      Ui,
	rows:    Vec<VibeWorkerRow>,
	ctx:     UiContext,
	options: OverlayOptions,
	width:   u16,
}

impl VibeWall {
	/// Opens a wall from one immutable worker snapshot.
	#[must_use]
	pub fn open(rows: &[VibeWorkerRow], ctx: &UiContext) -> Self {
		let width = 100;
		Self {
			ui: build(rows, width, ctx),
			rows: rows.to_vec(),
			ctx: ctx.clone(),
			options: OverlayOptions::default()
				.anchor(OverlayAnchor::Bottom)
				.width(Dim::Pct(100))
				.z(9),
			width,
		}
	}

	/// Replaces the complete live projection without retaining stale workers.
	pub fn update_rows(&mut self, rows: &[VibeWorkerRow]) {
		if self.rows == rows {
			return;
		}
		self.rows.clear();
		self.rows.extend_from_slice(rows);
		self.ui = build(&self.rows, self.width, &self.ctx);
	}

	/// Returns the full-width responsive wall layer.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		if self.width != viewport.width {
			self.width = viewport.width;
			self.ui = build(&self.rows, self.width, &self.ctx);
		}
		Layer { frame: self.ui.frame(), options: &self.options, active: true }
	}
}

fn build(rows: &[VibeWorkerRow], width: u16, ctx: &UiContext) -> Ui {
	let columns = usize::from((width / MIN_CARD_WIDTH).max(1));
	let gaps = u16::try_from(columns.saturating_sub(1)).unwrap_or(u16::MAX);
	let card_width = width
		.saturating_sub(gaps)
		.checked_div(u16::try_from(columns).unwrap_or(u16::MAX).max(1))
		.unwrap_or(MIN_CARD_WIDTH)
		.clamp(MIN_CARD_WIDTH.min(width.max(1)), MAX_CARD_WIDTH);
	let cards = rows.iter().map(|row| {
		let tokens = row
			.tokens
			.map_or_else(|| sf!("—"), |tokens| sf!("{tokens} tok"));
		(row, tokens)
	});
	let title = sf!("Vibe Wall · {} worker{}", rows.len(), if rows.len() == 1 { "" } else { "s" });
	let root = OverlayPanel::new(title).child(dom! {
		<col>
			if rows.is_empty() {
				<text fg=muted>{"No vibe workers are active."}</text>
			} else {
				<row gap=1 wrap>
					for (worker, tokens) in cards {
						<box border=round pad="0 1" w={card_width}>
							<col>
								<row gap=1>
									<text bold fg=accent truncate grow>{worker.name.clone()}</text>
									<text fg=muted truncate>{worker.status.clone()}</text>
								</row>
								<row gap=1>
									<text fg=muted>{worker.model.clone()}</text>
									<text fg=muted>{tokens}</text>
								</row>
								<text truncate>{worker.task.clone()}</text>
								<text dim wrap h=3>{worker.output.clone()}</text>
							</col>
						</box>
					}
				</row>
			}
		</col>
	});
	Ui::from_root(root, width.max(1), ctx.clone())
}
