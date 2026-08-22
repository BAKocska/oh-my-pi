//! Packaged `omp://` documentation resolver.

use omp_core::{CowBytes, Str};
use omp_tools::read::{
	Fault,
	resolver::{
		DocsArchive, LineOffsetCache, Resolve, ResourceCompletion, ResourceEntry, ResourceList,
		fuzzy_score,
	},
	selector::ParsedSelector,
};

/// Constructor-owned packaged documentation resolver.
#[derive(Debug, Default)]
pub(crate) struct DocsResolver {
	docs:  DocsArchive,
	lines: LineOffsetCache,
}

impl Resolve for DocsResolver {
	async fn read<'a>(
		&'a self,
		resource: &'a str,
		selector: &'a ParsedSelector,
	) -> Result<CowBytes<'static>, Fault> {
		if resource.is_empty() {
			let mut index = String::from("# OMP documentation\n\n");
			for name in self.docs.names() {
				index.push_str("- omp://");
				index.push_str(name);
				index.push('\n');
			}
			return Ok(CowBytes::from(index.into_bytes()));
		}
		let bytes = self.docs.read(resource)?.ok_or_else(|| {
			let mut nearby = self
				.docs
				.names()
				.filter_map(|name| fuzzy_score(resource, name).map(|score| (score, name)))
				.collect::<Vec<_>>();
			nearby
				.sort_unstable_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(right.1)));
			nearby.truncate(3);
			let message = if nearby.is_empty() {
				format!(
					"Documentation not found: omp://{resource}. Use omp:// to list available paths."
				)
			} else {
				format!(
					"Documentation not found: omp://{resource}. Did you mean: {}",
					nearby
						.into_iter()
						.map(|(_, name)| format!("omp://{name}"))
						.collect::<Vec<_>>()
						.join(", ")
				)
			};
			Fault::Source { message: Str::new(message) }
		})?;
		let ParsedSelector::Lines { ranges, .. } = selector else {
			return Ok(bytes);
		};
		if ranges.len() == 1 {
			return self
				.lines
				.slice(resource, &bytes, ranges[0])
				.map(CowBytes::into_owned)
				.map_err(|error| Fault::Invalid { message: Str::new(error.to_string()) });
		}
		let mut output = Vec::new();
		for range in ranges {
			let piece = self
				.lines
				.slice(resource, &bytes, *range)
				.map_err(|error| Fault::Invalid { message: Str::new(error.to_string()) })?;
			output.extend_from_slice(&piece);
		}
		Ok(CowBytes::from(output))
	}

	async fn list(
		&self,
		resource: &str,
		max_entries: usize,
		max_bytes: usize,
	) -> Result<ResourceList, Fault> {
		let prefix = resource.trim_matches('/');
		let prefix = if prefix.is_empty() {
			String::new()
		} else {
			format!("{prefix}/")
		};
		let mut entries = Vec::new();
		let mut bytes = 0usize;
		let mut truncated = false;
		for name in self.docs.names() {
			let Some(rest) = name.strip_prefix(&prefix) else {
				continue;
			};
			if rest.is_empty() {
				continue;
			}
			let (child, directory) = rest
				.split_once('/')
				.map_or((rest, false), |(child, _)| (child, true));
			if entries
				.iter()
				.any(|entry: &ResourceEntry| entry.name == child)
			{
				continue;
			}
			let entry_bytes = child.len().saturating_add(prefix.len()).saturating_add(6);
			if entries.len() == max_entries || bytes.saturating_add(entry_bytes) > max_bytes {
				truncated = true;
				break;
			}
			bytes += entry_bytes;
			let suffix = if directory { "/" } else { "" };
			entries.push(ResourceEntry {
				uri: Str::new(format!("omp://{prefix}{child}{suffix}")),
				name: Str::new(format!("{child}{suffix}")),
				directory,
				size: 0,
			});
		}
		Ok(ResourceList { entries, truncated })
	}

	async fn complete(
		&self,
		query: &str,
		max_results: usize,
	) -> Result<Vec<ResourceCompletion>, Fault> {
		let query = query.trim_start_matches('/');
		let mut matches = self
			.docs
			.names()
			.filter_map(|name| {
				fuzzy_score(query, name).map(|score| ResourceCompletion {
					value: Str::new(format!("omp://{name}")),
					description: Str::new_static("packaged OMP documentation"),
					score,
				})
			})
			.collect::<Vec<_>>();
		matches.sort_unstable_by(|left, right| {
			right
				.score
				.cmp(&left.score)
				.then_with(|| left.value.cmp(&right.value))
		});
		matches.truncate(max_results);
		Ok(matches)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn packaged_index_and_content_share_the_same_sorted_corpus() {
		let resolver = DocsResolver::default();
		let listing = resolver.list("", 100, 64 * 1024).await.unwrap();
		assert!(!listing.entries.is_empty());
		assert!(
			listing
				.entries
				.windows(2)
				.all(|pair| pair[0].name <= pair[1].name)
		);
		let completions = resolver.complete("overview", 5).await.unwrap();
		assert!(!completions.is_empty());
		let uri = completions[0].value.strip_prefix("omp://").unwrap();
		let body = resolver.read(&uri, &ParsedSelector::None).await.unwrap();
		assert!(!body.is_empty());
	}

	#[tokio::test]
	async fn packaged_docs_reject_traversal_and_suggest_nearby_paths() {
		let resolver = DocsResolver::default();
		assert!(
			resolver
				.read("../Cargo.toml", &ParsedSelector::None)
				.await
				.is_err()
		);
		let fault = resolver
			.read("py/00-overvew.md", &ParsedSelector::None)
			.await
			.unwrap_err();
		assert!(fault.message().contains("omp://py/00-overview.md"));
	}
}
