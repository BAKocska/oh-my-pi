//! Deterministic projection of one MCP server beneath a dynamic device
//! namespace.

use std::{collections::BTreeMap, sync::Arc};

use bytes::Bytes;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use omp_core::{Hash32, Str};
use omp_secrets::replacement::bun_wyhash;
use omp_tool::Rev;
use serde::Serialize;
use serde_json::Value;

use super::{
	McpLeafDefinition,
	prompts::PromptDefinition,
	resources::{ResourceDefinition, ResourceTemplate},
};

/// Immutable endpoint projection and Python device policy for one mount.
#[derive(Clone)]
pub struct McpDeviceProjection {
	include:        GlobSet,
	exclude:        GlobSet,
	/// Exact upstream endpoint-to-device renames.
	pub rename:     BTreeMap<Str, Str>,
	/// Exact upstream endpoint documentation overrides.
	pub docs:       BTreeMap<Str, Str>,
	/// Python device precedence.
	pub precedence: i32,
	/// Default approval tier.
	pub tier:       Str,
}

impl McpDeviceProjection {
	/// Compiles an immutable endpoint projection.
	pub fn new(
		include: &[Str],
		exclude: &[Str],
		rename: BTreeMap<Str, Str>,
		docs: BTreeMap<Str, Str>,
		precedence: i32,
		tier: Str,
	) -> Result<Self, DeviceError> {
		if include.is_empty() {
			return Err(DeviceError::InvalidProjection);
		}
		Ok(Self {
			include: compile_globs(include)?,
			exclude: compile_globs(exclude)?,
			rename,
			docs,
			precedence,
			tier,
		})
	}

	/// Native mounts expose every endpoint with default device policy.
	pub fn all() -> Arc<Self> {
		Arc::new(
			Self::new(
				&[Str::new_static("*")],
				&[],
				BTreeMap::new(),
				BTreeMap::new(),
				0,
				Str::new_static("write"),
			)
			.expect("the default MCP projection is valid"),
		)
	}

	fn includes(&self, endpoint: &str) -> bool {
		self.include.is_match(endpoint) && !self.exclude.is_match(endpoint)
	}
}

fn compile_globs(patterns: &[Str]) -> Result<GlobSet, DeviceError> {
	let mut builder = GlobSetBuilder::new();
	for pattern in patterns {
		let glob = GlobBuilder::new(pattern)
			.literal_separator(true)
			.build()
			.map_err(|_| DeviceError::InvalidProjection)?;
		builder.add(glob);
	}
	builder.build().map_err(|_| DeviceError::InvalidProjection)
}

/// One server's live definitions ready for revisioned registry publication.
pub struct McpDeviceDefinitions {
	/// Server name used as the dynamic device namespace.
	pub server:       Str,
	/// Advertised tools in server order.
	pub tools:        Vec<Value>,
	/// Advertised concrete resources.
	pub resources:    Vec<ResourceDefinition>,
	/// Advertised resource templates.
	pub templates:    Vec<ResourceTemplate>,
	/// Advertised prompts.
	pub prompts:      Vec<PromptDefinition>,
	/// Bounded server instructions used as device documentation.
	pub instructions: Option<Str>,
	/// Extension-declared endpoint projection and device policy.
	pub projection:   Arc<McpDeviceProjection>,
}

impl McpDeviceDefinitions {
	/// Materializes deterministic leaf definitions. Names are scoped beneath one
	/// MCP device and never create model slots or slash commands.
	pub fn into_leaves(self, protocol_version: &str) -> Result<Vec<McpLeafDefinition>, DeviceError> {
		let revision = match protocol_version {
			"2025-11-25" => Rev { family: Str::new_static("mcp"), n: 4 },
			"2025-06-18" => Rev { family: Str::new_static("mcp"), n: 3 },
			"2025-03-26" => Rev { family: Str::new_static("mcp"), n: 2 },
			"2024-11-05" => Rev { family: Str::new_static("mcp"), n: 1 },
			_ => return Err(DeviceError::InvalidRevision),
		};
		let mut leaves = Vec::with_capacity(
			self.tools.len() + self.resources.len() + self.templates.len() + self.prompts.len(),
		);
		let instructions = self.instructions;
		for tool in self.tools {
			let name = tool
				.get("name")
				.and_then(Value::as_str)
				.ok_or(DeviceError::MalformedDefinition)?;
			if !self.projection.includes(name) {
				continue;
			}
			let device_name = self
				.projection
				.rename
				.get(name)
				.cloned()
				.unwrap_or_else(|| Str::from(minted_name(&self.server, "tool", name)));
			let documentation = self
				.projection
				.docs
				.get(name)
				.cloned()
				.or_else(|| instructions.clone());
			leaves.push(leaf(
				&self.server,
				"tool",
				name,
				device_name,
				&revision,
				&tool,
				&documentation,
				&instructions,
				self.projection.precedence,
				self.projection.tier.clone(),
			)?);
		}
		for resource in self.resources {
			leaves.push(leaf(
				&self.server,
				"resource",
				resource.name.as_str(),
				Str::from(minted_name(&self.server, "resource", resource.name.as_str())),
				&revision,
				&resource,
				&instructions,
				&instructions,
				self.projection.precedence,
				self.projection.tier.clone(),
			)?);
		}
		for template in self.templates {
			leaves.push(leaf(
				&self.server,
				"resource-template",
				template.name.as_str(),
				Str::from(minted_name(&self.server, "resource-template", template.name.as_str())),
				&revision,
				&template,
				&instructions,
				&instructions,
				self.projection.precedence,
				self.projection.tier.clone(),
			)?);
		}
		for prompt in self.prompts {
			leaves.push(leaf(
				&self.server,
				"prompt",
				prompt.name.as_str(),
				Str::from(minted_name(&self.server, "prompt", prompt.name.as_str())),
				&revision,
				&prompt,
				&instructions,
				&instructions,
				self.projection.precedence,
				self.projection.tier.clone(),
			)?);
		}
		leaves.sort_unstable_by(|left, right| left.name.cmp(&right.name));
		Ok(leaves)
	}
}

fn leaf(
	server: &str,
	kind: &str,
	name: &str,
	device_name: Str,
	revision: &Rev,
	definition: &impl Serialize,
	documentation: &Option<Str>,
	server_instructions: &Option<Str>,
	precedence: i32,
	tier: Str,
) -> Result<McpLeafDefinition, DeviceError> {
	if name.is_empty() || name.chars().any(char::is_control) {
		return Err(DeviceError::MalformedDefinition);
	}
	let definition_json =
		serde_json::to_vec(definition).map_err(|_| DeviceError::MalformedDefinition)?;
	let mut hasher = Hash32::hasher();
	hasher.update(b"omp-mcp-device-leaf/v1\0");
	for field in [
		server.as_bytes(),
		kind.as_bytes(),
		name.as_bytes(),
		device_name.as_bytes(),
		tier.as_bytes(),
		&definition_json,
	] {
		hasher.update(&(field.len() as u64).to_le_bytes());
		hasher.update(field);
	}
	hasher.update(&precedence.to_le_bytes());
	if let Some(documentation) = documentation {
		hasher.update(&(documentation.len() as u64).to_le_bytes());
		hasher.update(documentation.as_bytes());
	}
	if let Some(instructions) = server_instructions {
		hasher.update(&(instructions.len() as u64).to_le_bytes());
		hasher.update(instructions.as_bytes());
	}
	Ok(McpLeafDefinition {
		name: device_name,
		kind: Str::from(kind),
		rev: revision.clone(),
		code: hasher.finalize(),
		definition_json: Bytes::from(definition_json),
		documentation: documentation.clone(),
		server_instructions: server_instructions.clone(),
		precedence,
		tier,
	})
}

fn push_safe_name(output: &mut String, value: &str) {
	output.extend(value.chars().map(|character| {
		if character.is_ascii_alphanumeric() || character == '_' {
			character
		} else {
			'_'
		}
	}));
}

const MAX_MCP_TOOL_NAME_LENGTH: usize = 64;
const MCP_TOOL_NAME_HASH_LENGTH: usize = 8;

fn minted_name(server: &str, kind: &str, name: &str) -> String {
	let mut full = String::with_capacity(9 + server.len() + kind.len() + name.len());
	full.push_str("mcp__");
	push_safe_name(&mut full, server);
	full.push_str("__");
	full.push_str(kind);
	full.push_str("__");
	push_safe_name(&mut full, name);
	if full.len() <= MAX_MCP_TOOL_NAME_LENGTH {
		return full;
	}
	let hash = bun_wyhash(full.as_bytes());
	let mut encoded = [0_u8; 13];
	let hash = base36(hash, &mut encoded);
	let hash = &hash[..MCP_TOOL_NAME_HASH_LENGTH.min(hash.len())];
	let mut capped = full;
	capped.truncate(MAX_MCP_TOOL_NAME_LENGTH - hash.len() - 1);
	capped.push('_');
	capped.push_str(hash);
	capped
}

fn base36(mut value: u64, encoded: &mut [u8; 13]) -> &str {
	const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
	let mut cursor = encoded.len();
	loop {
		cursor -= 1;
		encoded[cursor] = DIGITS[(value % 36) as usize];
		value /= 36;
		if value == 0 {
			break;
		}
	}
	std::str::from_utf8(&encoded[cursor..]).expect("base-36 alphabet is UTF-8")
}

/// Dynamic device projection failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum DeviceError {
	/// Protocol revision cannot be represented as a tool revision.
	#[error("MCP protocol revision is not a valid tool revision")]
	InvalidRevision,
	/// Advertised definition is missing a required stable name.
	#[error("MCP advertised definition is malformed")]
	MalformedDefinition,
	/// Endpoint globs or projected device policy are invalid.
	#[error("MCP endpoint projection is invalid")]
	InvalidProjection,
}
#[cfg(test)]
mod tests {
	use super::minted_name;

	#[test]
	fn minted_names_cap_with_bun_hash_suffix() {
		let first =
			minted_name("chrome-devtools-mcp", "tool", "chrome_devtools_performance_analyze_insight");
		let repeated =
			minted_name("chrome-devtools-mcp", "tool", "chrome_devtools_performance_analyze_insight");
		let distinct = minted_name(
			"chrome-devtools-mcp",
			"tool",
			"chrome_devtools_performance_analyze_something_else_entirely",
		);

		assert_eq!(first, "mcp__chrome_devtools_mcp__tool__chrome_devtools_perform_wnr94qdc");
		assert_eq!(first.len(), 64);
		assert_eq!(first, repeated);
		assert_ne!(first, distinct);
		assert_eq!(distinct.len(), 64);
		assert!(
			first
				.bytes()
				.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
		);
	}

	#[test]
	fn minted_names_within_limit_are_unchanged() {
		assert_eq!(
			minted_name("puppeteer", "tool", "screenshot"),
			"mcp__puppeteer__tool__screenshot"
		);
	}
}
