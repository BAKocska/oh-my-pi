//! Severity-weighted whole-file LPT scheduling.

use std::collections::BTreeMap;

use omp_core::Str;

use super::types::{Assignment, Diagnostic, FileIssues, Severity};

/// Groups diagnostics by file and calculates pi's severity/detail weight.
#[must_use]
pub fn group_by_file(diagnostics: &[Diagnostic]) -> Vec<FileIssues> {
	let mut grouped = BTreeMap::<Option<Str>, Vec<Diagnostic>>::new();
	for diagnostic in diagnostics {
		grouped
			.entry(diagnostic.file.clone())
			.or_default()
			.push(diagnostic.clone());
	}
	let mut groups = grouped
		.into_iter()
		.map(|(file, diagnostics)| {
			let weight = diagnostics.iter().map(weight).sum();
			FileIssues { file, diagnostics, weight }
		})
		.collect::<Vec<_>>();
	groups.sort_by(|left, right| {
		right
			.weight
			.cmp(&left.weight)
			.then(left.file.cmp(&right.file))
	});
	groups
}

fn weight(diagnostic: &Diagnostic) -> u64 {
	let severity = match diagnostic.severity {
		Severity::Error => 8,
		Severity::Warning => 4,
		Severity::Info => 2,
	};
	let missing_location = u64::from(diagnostic.file.is_none()) * 5
		+ u64::from(diagnostic.line.is_none()) * 3
		+ u64::from(diagnostic.column.is_none());
	let missing_detail =
		u64::from(diagnostic.code.is_none()) + u64::from(diagnostic.suggestion.is_none());
	severity + missing_location + missing_detail
}

/// LPT-packs file-disjoint groups into at most `agents` assignments.
#[must_use]
pub fn pack(groups: Vec<FileIssues>, agents: usize) -> Vec<Assignment> {
	if groups.is_empty() || agents == 0 {
		return Vec::new();
	}
	let count = agents.min(groups.len());
	let mut assignments = (0..count)
		.map(|index| Assignment { index, groups: Vec::new(), weight: 0 })
		.collect::<Vec<_>>();
	for group in groups {
		let target = assignments
			.iter_mut()
			.min_by_key(|assignment| (assignment.weight, assignment.index))
			.expect("non-empty LPT bins");
		target.weight = target.weight.saturating_add(group.weight);
		target.groups.push(group);
	}
	assignments
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn lpt_never_splits_a_file_and_balances_heavy_first() {
		let groups = [9, 8, 7, 6]
			.into_iter()
			.enumerate()
			.map(|(index, weight)| FileIssues {
				file: Some(Str::from(format!("{index}.rs"))),
				diagnostics: Vec::new(),
				weight,
			})
			.collect();
		let assignments = pack(groups, 2);
		assert_eq!(assignments[0].weight, 15);
		assert_eq!(assignments[1].weight, 15);
		assert_eq!(
			assignments
				.iter()
				.map(|assignment| assignment.groups.len())
				.sum::<usize>(),
			4
		);
	}
}
