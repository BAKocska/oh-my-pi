//! Interactive retained plan-review overlay with TOC and section feedback.

use std::collections::BTreeMap;

use omp_core::{Str, sf};
use omp_tui::{
	Dim, Key, Layer, Mouse, OverlayAnchor, OverlayOptions, Size, Ui, UiContext, UiEvent, dom,
};

use crate::{OverlayPanel, panel_divider};

const REVIEW_HINT: &str =
	"↑/↓ section · Ctrl+A annotate · Ctrl+S submit · Ctrl+Q save and quit · Esc close";

/// One parsed plan segment shown in the table of contents.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanReviewSection {
	/// Markdown heading label, or `Overview` for pre-heading text.
	pub title:   Str,
	/// Heading depth (`0` for overview).
	pub level:   u8,
	/// Exact markdown segment.
	pub content: Str,
}

/// Serializable per-section feedback state.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PlanReviewAnnotations {
	/// Feedback keyed by stable section index.
	pub sections: BTreeMap<usize, Str>,
	/// Whole-plan feedback editor value.
	pub overall:  Str,
}

impl PlanReviewAnnotations {
	/// Renders feedback into one deterministic model-visible refinement prompt.
	pub fn prompt(&self, sections: &[PlanReviewSection]) -> Str {
		let mut output = String::from("Revise the approved plan using this review feedback:");
		if !self.overall.trim().is_empty() {
			output.push_str("\n\nOverall:\n");
			output.push_str(self.overall.as_str().trim());
		}
		for (index, feedback) in &self.sections {
			if feedback.trim().is_empty() {
				continue;
			}
			output.push_str("\n\nSection ");
			output.push_str(
				sections
					.get(*index)
					.map_or("Overview", |section| section.title.as_str()),
			);
			output.push_str(":\n");
			output.push_str(feedback.as_str().trim());
		}
		Str::new(output)
	}
}

/// Result of routing input through the review overlay.
pub enum PlanReviewEvent {
	/// Event consumed while review remains open.
	Consumed,
	/// Current section changed.
	SectionChanged(usize),
	/// Annotation state changed.
	AnnotationsChanged(PlanReviewAnnotations),
	/// Submit all feedback to the live host.
	Submit(PlanReviewAnnotations),
	/// Save the exact reviewed Markdown and start a fresh session.
	SaveAndQuit(Str),
	/// Close without submitting.
	Cancel,
}

/// Full-height interactive plan review surface.
pub struct PlanReviewOverlay {
	ui:          Ui,
	ctx:         UiContext,
	options:     OverlayOptions,
	sections:    Vec<PlanReviewSection>,
	content:     Str,
	current:     usize,
	annotations: PlanReviewAnnotations,
	width:       u16,
	height:      u16,
}

impl PlanReviewOverlay {
	/// Parses markdown and opens a full-height review overlay.
	pub fn open(plan: &str, annotations: PlanReviewAnnotations, ctx: &UiContext) -> Self {
		let sections = split_plan_sections(plan);
		let width = 96;
		let height = 28;
		let mut ui = build_review(&sections, 0, &annotations, width, height, ctx);
		ui.focus_first();
		Self {
			ui,
			ctx: ctx.clone(),
			options: OverlayOptions::default()
				.anchor(OverlayAnchor::Center)
				.width(Dim::Pct(100))
				.max_height(Dim::Pct(100))
				.fill_height()
				.z(35),
			sections,
			content: Str::new(plan),
			current: 0,
			annotations,
			width,
			height,
		}
	}

	/// Routes keyboard navigation and feedback actions.
	pub fn handle_key(&mut self, key: Key) -> PlanReviewEvent {
		match key {
			Key::Esc => PlanReviewEvent::Cancel,
			Key::Ctrl('a') => self.capture_section_annotation(),
			Key::Ctrl('s') => {
				self.capture_overall();
				PlanReviewEvent::Submit(self.annotations.clone())
			},
			Key::Ctrl('q') => PlanReviewEvent::SaveAndQuit(self.content.clone()),
			_ => {
				let event = self.ui.handle_key(key);
				self.route(event)
			},
		}
	}

	/// Routes pasted feedback.
	pub fn handle_paste(&mut self, text: &str) -> PlanReviewEvent {
		let event = self.ui.handle_paste(text);
		self.route(event)
	}

	/// Routes pointer input; clicking outside cancels review.
	pub fn handle_mouse(
		&mut self,
		col: u16,
		row: u16,
		kind: Mouse,
		viewport: Size,
	) -> PlanReviewEvent {
		match self
			.ui
			.handle_mouse_as_layer(&self.options, viewport, col, row, kind)
		{
			Some(event) => self.route(event),
			None if kind == Mouse::Click => PlanReviewEvent::Cancel,
			None => PlanReviewEvent::Consumed,
		}
	}

	/// Returns a viewport-responsive full-height layer.
	pub fn layer(&mut self, viewport: Size) -> Layer<'_> {
		let width = viewport.width.saturating_sub(2).max(1);
		let height = viewport.height.saturating_sub(2).max(8);
		if width != self.width || height != self.height {
			self.capture_overall();
			self.ui =
				build_review(&self.sections, self.current, &self.annotations, width, height, &self.ctx);
			self.ui.focus_first();
			self.width = width;
			self.height = height;
		}
		Layer { frame: self.ui.frame(), options: &self.options, active: true }
	}

	/// Borrows parsed sections in TOC order.
	pub fn sections(&self) -> &[PlanReviewSection] {
		&self.sections
	}

	fn route(&mut self, event: UiEvent) -> PlanReviewEvent {
		match event {
			UiEvent::Changed { id, value } if id == "plan-toc" => {
				let Ok(next) = value.as_str().parse::<usize>() else {
					return PlanReviewEvent::Consumed;
				};
				if next >= self.sections.len() || next == self.current {
					return PlanReviewEvent::Consumed;
				}
				self.capture_overall();
				self.current = next;
				self.ui = build_review(
					&self.sections,
					self.current,
					&self.annotations,
					self.width,
					self.height,
					&self.ctx,
				);
				PlanReviewEvent::SectionChanged(next)
			},
			UiEvent::Cancel => PlanReviewEvent::Cancel,
			UiEvent::None
			| UiEvent::Submit
			| UiEvent::Changed { .. }
			| UiEvent::Highlighted { .. }
			| UiEvent::Filtered { .. }
			| UiEvent::Pressed(_)
			| UiEvent::Copied(_)
			| UiEvent::DiffAction { .. } => PlanReviewEvent::Consumed,
		}
	}

	fn capture_section_annotation(&mut self) -> PlanReviewEvent {
		self.capture_overall();
		let feedback = self.annotations.overall.trim();
		if feedback.is_empty() {
			self.annotations.sections.remove(&self.current);
		} else {
			self
				.annotations
				.sections
				.insert(self.current, Str::new(feedback));
		}
		PlanReviewEvent::AnnotationsChanged(self.annotations.clone())
	}

	fn capture_overall(&mut self) {
		self.annotations.overall = Str::new(
			self.ui.values()["plan-feedback"]
				.as_str()
				.unwrap_or_default(),
		);
	}
}

fn build_review(
	sections: &[PlanReviewSection],
	current: usize,
	annotations: &PlanReviewAnnotations,
	width: u16,
	height: u16,
	ctx: &UiContext,
) -> Ui {
	let toc_width = (width / 4).clamp(18, 32);
	let body_height = height.saturating_sub(7).max(3);
	let feedback_height = 3_u16;
	let rows = sections
		.iter()
		.enumerate()
		.map(|(index, section)| {
			let indent = "  ".repeat(usize::from(section.level.saturating_sub(1)));
			(
				sf!("{index}"),
				sf!("{indent}{}", section.title),
				index == current,
				annotations.sections.contains_key(&index),
			)
		})
		.collect::<Vec<_>>();
	let content = sections
		.get(current)
		.map_or_else(|| sf!("No plan content."), |section| section.content.clone());
	let feedback = annotations.overall.clone();
	Ui::from_root(
		OverlayPanel::new("Plan review").child(dom! {
			<col h={height}>
				<row grow>
					<select id="plan-toc" w={toc_width} h={body_height}>
						for (value, label, selected, annotated) in rows {
							<option value={value} label={label.clone()} recommended={selected}>
								<text truncate bold={annotated}>{label}</text>
							</option>
						}
					</select>
					<col grow h={body_height}>
						<md grow>{content}</md>
					</col>
				</row>
				{panel_divider()}
				<editor id="plan-feedback" value={feedback} h={feedback_height}/>
				<text dim truncate>{REVIEW_HINT}</text>
			</col>
		}),
		width,
		ctx.clone(),
	)
}

/// Splits a Markdown plan into reviewable heading segments.
pub fn split_plan_sections(plan: &str) -> Vec<PlanReviewSection> {
	let mut starts = Vec::<(usize, u8, Str)>::new();
	for (offset, line) in line_offsets(plan) {
		let hashes = line.bytes().take_while(|byte| *byte == b'#').count();
		if !(1..=6).contains(&hashes) || line.as_bytes().get(hashes) != Some(&b' ') {
			continue;
		}
		let title = line[hashes + 1..].trim();
		if !title.is_empty() {
			starts.push((offset, hashes as u8, Str::new(title)));
		}
	}
	let mut sections = Vec::new();
	if starts.first().is_none_or(|(offset, ..)| *offset != 0) {
		let end = starts.first().map_or(plan.len(), |(offset, ..)| *offset);
		let overview = plan[..end].trim();
		if !overview.is_empty() {
			sections.push(PlanReviewSection {
				title:   sf!("Overview"),
				level:   0,
				content: Str::new(overview),
			});
		}
	}
	for (index, (start, level, title)) in starts.iter().enumerate() {
		let end = starts
			.get(index + 1)
			.map_or(plan.len(), |(offset, ..)| *offset);
		sections.push(PlanReviewSection {
			title:   title.clone(),
			level:   *level,
			content: Str::new(plan[*start..end].trim()),
		});
	}
	if sections.is_empty() {
		sections.push(PlanReviewSection {
			title:   sf!("Overview"),
			level:   0,
			content: Str::new(plan.trim()),
		});
	}
	sections
}

fn line_offsets(text: &str) -> impl Iterator<Item = (usize, &str)> {
	let mut offset = 0;
	text.split_inclusive('\n').map(move |line| {
		let start = offset;
		offset += line.len();
		(start, line.trim_end_matches(['\r', '\n']))
	})
}
#[cfg(test)]
mod tests {
	use omp_tui::{Key, UiContext};

	use super::{PlanReviewEvent, PlanReviewOverlay};

	#[test]
	fn save_and_quit_returns_exact_reviewed_markdown() {
		let plan = "# Topic\n\nApproved body.\n";
		let mut review = PlanReviewOverlay::open(plan, Default::default(), &UiContext::default());
		let PlanReviewEvent::SaveAndQuit(content) = review.handle_key(Key::Ctrl('q')) else {
			panic!("Ctrl+Q must request plan save");
		};
		assert_eq!(content, plan);
	}
}
