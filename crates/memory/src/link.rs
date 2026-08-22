//! Proactive associative-link construction and generation-fenced
//! reconciliation.

use std::collections::{HashMap, HashSet};

use omp_core::Str;

use crate::{
	Result,
	store::{BankStore, GraphTriple, MemoryLink},
};

const MAX_RECONCILE_ROWS: usize = 1000;
const MAX_LINKS_PER_MEMORY: usize = 8;

struct OwnedTriple {
	subject:    Str,
	predicate:  Str,
	object:     Str,
	source:     Str,
	confidence: f64,
}

struct OwnedLink {
	source: Str,
	target: Str,
	weight: f64,
}

/// Rebuilds graph terms and associative links against one stable durable
/// generation.
///
/// Rows are bounded, deterministic, and derived exclusively from authoritative
/// memory content. A concurrent durable write rejects the commit rather than
/// publishing a mixed generation.
pub fn reconcile(store: &BankStore) -> Result<usize> {
	let expected = store.generations()?.durable;
	let records = store.list(MAX_RECONCILE_ROWS)?;
	let mut terms_by_id = HashMap::<Str, HashSet<Str>>::new();
	let mut ids_by_term = HashMap::<Str, Vec<Str>>::new();
	let mut triples = Vec::<OwnedTriple>::new();
	for record in &records {
		let terms = content_terms(record.content.as_str());
		for term in &terms {
			ids_by_term
				.entry(term.clone())
				.or_default()
				.push(record.id.clone());
			triples.push(OwnedTriple {
				subject:    term.clone(),
				predicate:  Str::new_static("mentioned_in"),
				object:     record.id.clone(),
				source:     record.id.clone(),
				confidence: record.importance,
			});
		}
		terms_by_id.insert(record.id.clone(), terms);
	}
	let mut overlap = HashMap::<(Str, Str), usize>::new();
	for ids in ids_by_term.values_mut() {
		ids.sort_unstable();
		ids.dedup();
		for (index, left) in ids.iter().enumerate() {
			for right in ids.iter().skip(index + 1) {
				*overlap.entry((left.clone(), right.clone())).or_default() += 1;
			}
		}
	}
	let mut candidates = HashMap::<Str, Vec<(Str, f64)>>::new();
	for ((left, right), intersection) in overlap {
		let left_count = terms_by_id.get(&left).map_or(0, HashSet::len);
		let right_count = terms_by_id.get(&right).map_or(0, HashSet::len);
		let union = left_count + right_count - intersection;
		if union == 0 {
			continue;
		}
		let weight = intersection as f64 / union as f64;
		if weight < 0.2 {
			continue;
		}
		candidates
			.entry(left.clone())
			.or_default()
			.push((right.clone(), weight));
		candidates.entry(right).or_default().push((left, weight));
	}
	let mut links = Vec::<OwnedLink>::new();
	for (source, targets) in &mut candidates {
		targets.sort_by(|left, right| {
			right
				.1
				.total_cmp(&left.1)
				.then_with(|| left.0.cmp(&right.0))
		});
		targets.truncate(MAX_LINKS_PER_MEMORY);
		for (target, weight) in targets {
			links.push(OwnedLink { source: source.clone(), target: target.clone(), weight: *weight });
		}
	}
	let graph = triples
		.iter()
		.map(|triple| GraphTriple {
			subject:          triple.subject.as_str(),
			predicate:        triple.predicate.as_str(),
			object:           triple.object.as_str(),
			source_memory_id: triple.source.as_str(),
			confidence:       triple.confidence,
		})
		.collect::<Vec<_>>();
	let associations = links
		.iter()
		.map(|link| MemoryLink {
			source_memory_id: link.source.as_str(),
			target_memory_id: link.target.as_str(),
			relation:         "context",
			weight:           link.weight,
		})
		.collect::<Vec<_>>();
	store.replace_graph(expected, &graph, &associations)?;
	Ok(associations.len())
}

fn content_terms(content: &str) -> HashSet<Str> {
	content
		.split(|character: char| !character.is_alphanumeric() && character != '_')
		.filter(|term| term.chars().count() >= 3)
		.map(|term| Str::new(term.to_lowercase()))
		.take(128)
		.collect()
}
