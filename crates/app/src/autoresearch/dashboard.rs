//! Retained dashboard model shared by sticky and fullscreen renderers.

use omp_core::Str;

use super::types::{DashboardMode, ExperimentStatus, MetricDirection};

/// One row rendered by the autoresearch dashboard.
#[derive(Clone, Debug, PartialEq)]
pub struct RunRow {
	/// Stable run id.
	pub id:          i64,
	/// Segment number.
	pub segment:     u32,
	/// Terminal status.
	pub status:      ExperimentStatus,
	/// Primary metric.
	pub metric:      f64,
	/// Human-readable description.
	pub description: Str,
	/// Whether the result is excluded from control-state math.
	pub flagged:     bool,
}

/// Reconstructed dashboard state with bounded navigation.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Dashboard {
	/// Current presentation.
	pub mode:       DashboardMode,
	/// Experiment name.
	pub name:       Option<Str>,
	/// Metric label.
	pub metric:     Option<Str>,
	/// Unit suffix.
	pub unit:       Str,
	/// Improvement direction.
	pub direction:  MetricDirection,
	/// Current segment.
	pub segment:    u32,
	/// Current best kept value.
	pub best:       Option<f64>,
	/// MAD confidence ratio.
	pub confidence: Option<f64>,
	/// Visible run ledger.
	pub rows:       Vec<RunRow>,
	/// Selected row for fullscreen navigation.
	pub selected:   usize,
}

impl Dashboard {
	/// Toggles collapsed and expanded sticky presentation.
	pub fn toggle(&mut self) {
		self.mode = match self.mode {
			DashboardMode::Collapsed => DashboardMode::Expanded,
			DashboardMode::Expanded | DashboardMode::Fullscreen => DashboardMode::Collapsed,
		};
	}

	/// Opens fullscreen presentation and clamps the selection.
	pub fn open(&mut self) {
		self.mode = DashboardMode::Fullscreen;
		self.selected = self.selected.min(self.rows.len().saturating_sub(1));
	}

	/// Closes fullscreen presentation to the expanded sticky card.
	pub fn close(&mut self) {
		self.mode = DashboardMode::Expanded;
	}

	/// Selects the previous run without wrapping.
	pub fn previous(&mut self) {
		self.selected = self.selected.saturating_sub(1);
	}

	/// Selects the next run without escaping the ledger.
	pub fn next(&mut self) {
		self.selected = self
			.selected
			.saturating_add(1)
			.min(self.rows.len().saturating_sub(1));
	}

	/// Reconstructs best-value control state from unflagged kept rows.
	pub fn reconstruct(&mut self) {
		self.best = self
			.rows
			.iter()
			.filter(|row| {
				row.segment == self.segment && !row.flagged && row.status == ExperimentStatus::Keep
			})
			.map(|row| row.metric)
			.reduce(|best, metric| match self.direction {
				MetricDirection::Lower if metric < best => metric,
				MetricDirection::Higher if metric > best => metric,
				_ => best,
			});
		self.selected = self.selected.min(self.rows.len().saturating_sub(1));
	}

	/// Produces a compact, renderer-neutral sticky summary.
	#[must_use]
	pub fn sticky_text(&self) -> Str {
		let name = self.name.as_deref().unwrap_or("Autoresearch");
		let best = self.best.map_or_else(
			|| "baseline pending".to_owned(),
			|value| format!("{}={value}{}", self.metric.as_deref().unwrap_or("metric"), self.unit),
		);
		let confidence = self
			.confidence
			.map_or_else(String::new, |value| format!(" · {value:.1}x noise"));
		Str::from(format!(
			"{name} · segment {} · {best}{confidence} · {} runs",
			self.segment,
			self.rows.len()
		))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn dashboard_navigation_and_best_are_bounded() {
		let mut dashboard = Dashboard {
			direction: MetricDirection::Lower,
			rows: vec![
				RunRow {
					id:          1,
					segment:     0,
					status:      ExperimentStatus::Keep,
					metric:      10.0,
					description: "base".into(),
					flagged:     false,
				},
				RunRow {
					id:          2,
					segment:     0,
					status:      ExperimentStatus::Keep,
					metric:      8.0,
					description: "win".into(),
					flagged:     false,
				},
			],
			..Dashboard::default()
		};
		dashboard.reconstruct();
		assert_eq!(dashboard.best, Some(8.0));
		dashboard.next();
		dashboard.next();
		assert_eq!(dashboard.selected, 1);
	}
}
