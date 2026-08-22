//! Deterministic projection of one MCP server beneath a dynamic device
//! namespace.

use bytes::Bytes;
use omp_core::{Hash32, Str};
use omp_tool::Rev;
use serde::Serialize;
use serde_json::Value;

use super::{
	McpLeafDefinition,
	prompts::PromptDefinition,
	resources::{ResourceDefinition, ResourceTemplate},
};

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
		let documentation = self.instructions;
		for tool in self.tools {
			let name = tool
				.get("name")
				.and_then(Value::as_str)
				.ok_or(DeviceError::MalformedDefinition)?;
			leaves.push(leaf(&self.server, "tool", name, &revision, &tool, &documentation)?);
		}
		for resource in self.resources {
			leaves.push(leaf(
				&self.server,
				"resource",
				resource.name.as_str(),
				&revision,
				&resource,
				&documentation,
			)?);
		}
		for template in self.templates {
			leaves.push(leaf(
				&self.server,
				"resource-template",
				template.name.as_str(),
				&revision,
				&template,
				&documentation,
			)?);
		}
		for prompt in self.prompts {
			leaves.push(leaf(
				&self.server,
				"prompt",
				prompt.name.as_str(),
				&revision,
				&prompt,
				&documentation,
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
	revision: &Rev,
	definition: &impl Serialize,
	documentation: &Option<Str>,
) -> Result<McpLeafDefinition, DeviceError> {
	if name.is_empty() || name.chars().any(char::is_control) {
		return Err(DeviceError::MalformedDefinition);
	}
	let definition_json =
		serde_json::to_vec(definition).map_err(|_| DeviceError::MalformedDefinition)?;
	let mut hasher = Hash32::hasher();
	hasher.update(b"omp-mcp-device-leaf/v1\0");
	for field in [server.as_bytes(), kind.as_bytes(), name.as_bytes(), &definition_json] {
		hasher.update(&(field.len() as u64).to_le_bytes());
		hasher.update(field);
	}
	if let Some(documentation) = documentation {
		hasher.update(&(documentation.len() as u64).to_le_bytes());
		hasher.update(documentation.as_bytes());
	}
	Ok(McpLeafDefinition {
		name:            Str::new(format!(
			"mcp__{}__{}__{}",
			safe_name(server),
			kind,
			safe_name(name)
		)),
		kind:            Str::from(kind),
		rev:             revision.clone(),
		code:            hasher.finalize(),
		definition_json: Bytes::from(definition_json),
		documentation:   documentation.clone(),
	})
}

fn safe_name(value: &str) -> String {
	value
		.chars()
		.map(|character| {
			if character.is_ascii_alphanumeric() || character == '_' {
				character
			} else {
				'_'
			}
		})
		.collect()
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
}
