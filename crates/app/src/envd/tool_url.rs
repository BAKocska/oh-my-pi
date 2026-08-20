//! App-owned internal URL resolver composition.

use std::sync::Arc;

use omp_agent::AgentRegistry;
use omp_core::{CowBytes, Str};
use omp_tools::read::{
	Fault,
	conflicts::{ConflictRegistry, ConflictResolver},
	resolver::{LineOffsetCache, Resolve, ResolverTable, Scheme, SchemeEntry},
	selector::ParsedSelector,
};

#[derive(Clone, Copy, Debug)]
enum RegistryResource {
	Agent,
	History,
}

pub(super) struct RegistryResolver {
	resource: RegistryResource,
	lines:    LineOffsetCache,
}

impl RegistryResolver {
	fn new(resource: RegistryResource) -> Self {
		Self { resource, lines: LineOffsetCache::default() }
	}
}

impl Resolve for RegistryResolver {
	async fn read<'a>(
		&'a self,
		resource: &'a str,
		selector: &'a ParsedSelector,
	) -> Result<CowBytes<'static>, Fault> {
		let bytes = match self.resource {
			RegistryResource::Agent => AgentRegistry::global().resolve_agent(resource),
			RegistryResource::History => AgentRegistry::global().resolve_history(resource),
		}
		.map_err(|error| Fault::Source { message: Str::from(error.to_string()) })?;
		select_bytes(&self.lines, resource, CowBytes::from(bytes), selector)
	}
}

/// Constructor-owned resolver union used by the production read registry.
pub(super) enum UrlResolver {
	/// Agent output and child artifacts.
	Agent(RegistryResolver),
	/// Read-only agent transcript index and bodies.
	History(RegistryResolver),
	/// Session-registered merge conflict regions.
	Conflict(ConflictResolver),
}

impl Resolve for UrlResolver {
	async fn read<'a>(
		&'a self,
		resource: &'a str,
		selector: &'a ParsedSelector,
	) -> Result<CowBytes<'static>, Fault> {
		match self {
			Self::Agent(resolver) | Self::History(resolver) => resolver.read(resource, selector).await,
			Self::Conflict(resolver) => resolver.read(resource, selector).await,
		}
	}
}

/// Builds the production internal URL table and shared conflict registry.
pub(super) fn production_url_resolvers(
	conflicts: Arc<ConflictRegistry>,
) -> Arc<ResolverTable<UrlResolver>> {
	let mut builder = ResolverTable::builder();
	builder
		.register(
			SchemeEntry::new(Scheme::Agent, true, false, "settled agent output and child artifacts"),
			UrlResolver::Agent(RegistryResolver::new(RegistryResource::Agent)),
		)
		.expect("agent URL resolver is unique");
	builder
		.register(
			SchemeEntry::new(Scheme::History, true, false, "read-only agent transcript index"),
			UrlResolver::History(RegistryResolver::new(RegistryResource::History)),
		)
		.expect("history URL resolver is unique");
	builder
		.register(
			SchemeEntry::new(Scheme::Conflict, true, false, "registered merge conflict regions"),
			UrlResolver::Conflict(ConflictResolver::new((*conflicts).clone())),
		)
		.expect("conflict URL resolver is unique");
	Arc::new(builder.build())
}

fn select_bytes(
	lines: &LineOffsetCache,
	resource: &str,
	bytes: CowBytes<'static>,
	selector: &ParsedSelector,
) -> Result<CowBytes<'static>, Fault> {
	let ParsedSelector::Lines { ranges, .. } = selector else {
		return Ok(bytes);
	};
	if ranges.len() == 1 {
		return lines
			.slice(resource, &bytes, ranges[0])
			.map(CowBytes::into_owned)
			.map_err(|error| Fault::Invalid { message: Str::from(error.to_string()) });
	}
	let mut output = Vec::new();
	for range in ranges {
		let piece = lines
			.slice(resource, &bytes, *range)
			.map_err(|error| Fault::Invalid { message: Str::from(error.to_string()) })?;
		output.extend_from_slice(&piece);
	}
	Ok(CowBytes::from(output))
}
