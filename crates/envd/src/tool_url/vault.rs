//! Bounded configured `vault://` resolver.

use omp_core::{CowBytes, Str};
use omp_tools::read::{
	Fault,
	resolver::{
		LineOffsetCache, Resolve, ResourceCompletion, ResourceEntry, ResourceList, fuzzy_score,
	},
	selector::ParsedSelector,
};

use crate::vault::{VaultError, VaultService};

pub(crate) struct VaultResolver {
	service: VaultService,
	lines:   LineOffsetCache,
}
impl VaultResolver {
	pub(super) fn new(service: VaultService) -> Self {
		Self { service, lines: LineOffsetCache::default() }
	}
}

impl Resolve for VaultResolver {
	async fn read<'a>(
		&'a self,
		resource: &'a str,
		selector: &'a ParsedSelector,
	) -> Result<CowBytes<'static>, Fault> {
		use super::select_bytes;
		if resource.is_empty() {
			let mut body = String::new();
			for name in self.service.names() {
				body.push_str("- ");
				body.push_str(&name);
				body.push('\n');
			}
			return select_bytes(&self.lines, resource, CowBytes::from(body.into_bytes()), selector);
		}
		let (vault, path) = parse_resource(resource)?;
		let bytes = self
			.service
			.read(&vault, &path, 8 * 1024 * 1024)
			.map_err(vault_fault)?;
		select_bytes(&self.lines, resource, bytes, selector)
	}

	async fn read_query<'a>(
		&'a self,
		resource: &'a str,
		query: Option<&'a str>,
		selector: &'a ParsedSelector,
	) -> Result<CowBytes<'static>, Fault> {
		if query.is_none() {
			return self.read(resource, selector).await;
		}
		Err(Fault::Invalid {
			message: Str::new_static(
				"vault:// query operations are unavailable for configured filesystem vaults.",
			),
		})
	}

	async fn list(
		&self,
		resource: &str,
		max_entries: usize,
		max_bytes: usize,
	) -> Result<ResourceList, Fault> {
		if resource.is_empty() {
			let names = self.service.names();
			let truncated = names.len() > max_entries;
			return Ok(ResourceList {
				entries: names
					.into_iter()
					.take(max_entries)
					.map(|name| ResourceEntry {
						uri: Str::new(format!("vault://{name}/")),
						name,
						directory: true,
						size: 0,
					})
					.collect(),
				truncated,
			});
		}
		let (vault, path) = parse_resource(resource)?;
		let (values, mut truncated) = self
			.service
			.list(&vault, &path, max_entries)
			.map_err(vault_fault)?;
		let mut used = 0;
		let mut entries = Vec::new();
		for (name, directory, size) in values {
			used += name.len();
			if used > max_bytes {
				truncated = true;
				break;
			}
			let child = if path.is_empty() {
				name.to_string()
			} else {
				format!("{path}/{name}")
			};
			entries.push(ResourceEntry {
				uri: Str::new(format!("vault://{vault}/{child}")),
				name,
				directory,
				size,
			});
		}
		Ok(ResourceList { entries, truncated })
	}

	async fn complete(
		&self,
		query: &str,
		max_results: usize,
	) -> Result<Vec<ResourceCompletion>, Fault> {
		let mut values = self
			.service
			.names()
			.into_iter()
			.filter_map(|name| {
				Some(ResourceCompletion {
					score:       fuzzy_score(query, &name)?,
					value:       Str::new(format!("vault://{name}/")),
					description: Str::new_static("configured vault"),
				})
			})
			.collect::<Vec<_>>();
		values.sort_unstable_by(|a, b| b.score.cmp(&a.score).then_with(|| a.value.cmp(&b.value)));
		values.truncate(max_results);
		Ok(values)
	}
}

pub(crate) fn parse_resource(resource: &str) -> Result<(Str, Str), Fault> {
	let (vault, path) = resource.split_once('/').unwrap_or((resource, ""));
	if vault.is_empty()
		|| vault.contains(['@', ':', '\\'])
		|| path
			.split('/')
			.any(|p| matches!(p, "." | "..") || p.contains('\\'))
	{
		return Err(Fault::Invalid {
			message: Str::new_static("invalid or escaping vault:// resource"),
		});
	}
	Ok((Str::new(vault), Str::new(path)))
}
fn vault_fault(error: VaultError) -> Fault {
	Fault::Source { message: Str::new(error.to_string()) }
}
