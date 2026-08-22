//! Explicit `mcp://server/resource-uri` reads.

use std::sync::Arc;

use omp_core::{CowBytes, Str};
use omp_proto::env::v1::{McpResourceRequest, McpServerRef};
use omp_tools::read::{
	Fault,
	resolver::{Resolve, ResourceCompletion, fuzzy_score},
	selector::ParsedSelector,
};
use tokio_util::sync::CancellationToken;

use crate::envd::mcp::McpService;

/// Environment-scoped MCP resource resolver.
pub(super) struct McpUrlResolver {
	service: Arc<McpService>,
}

impl McpUrlResolver {
	pub(super) fn new(service: Arc<McpService>) -> Self {
		Self { service }
	}

	fn parse<'a>(&self, resource: &'a str) -> Result<(&'a str, &'a str), Fault> {
		let (server, uri) = resource.split_once('/').ok_or_else(|| Fault::Invalid {
			message: Str::new_static("mcp:// reads require mcp://server/resource-uri."),
		})?;
		if server.is_empty() || uri.is_empty() {
			return Err(Fault::Invalid {
				message: Str::new_static("mcp:// reads require a nonempty server and resource URI."),
			});
		}
		Ok((server, uri))
	}
}

impl Resolve for McpUrlResolver {
	async fn read<'a>(
		&'a self,
		resource: &'a str,
		_selector: &'a ParsedSelector,
	) -> Result<CowBytes<'static>, Fault> {
		let (server, uri) = self.parse(resource)?;
		let status = self.service.status(Some(server));
		let current = status
			.servers
			.first()
			.and_then(|status| status.server.as_ref())
			.ok_or_else(|| Fault::Source {
				message: Str::new(format!("MCP server '{server}' is not mounted.")),
			})?;
		let result = self
			.service
			.resource(
				McpResourceRequest {
					server:        Some(McpServerRef {
						name:             server.to_owned(),
						definition_epoch: current.definition_epoch,
					}),
					uri:           uri.to_owned(),
					max_bytes:     8 * 1024 * 1024,
					wire_revision: 1,
				},
				CancellationToken::new(),
			)
			.await
			.map_err(|error| Fault::Source { message: Str::new(error.to_string()) })?;
		if result.truncated {
			return Err(Fault::Source {
				message: Str::new_static("MCP resource exceeded the read size limit."),
			});
		}
		Ok(CowBytes::from(result.content))
	}

	async fn complete(
		&self,
		query: &str,
		max_results: usize,
	) -> Result<Vec<ResourceCompletion>, Fault> {
		let mut matches = self
			.service
			.status(None)
			.servers
			.into_iter()
			.filter_map(|status| {
				let server = status.server?;
				let score = fuzzy_score(query, &server.name)?;
				Some(ResourceCompletion {
					value: Str::new(format!("mcp://{}/", server.name)),
					description: Str::new_static("mounted MCP server"),
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
