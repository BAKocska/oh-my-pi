//! Typed JSON CONTROL projection over one Environment MCP manager.

use std::{future::Future, pin::Pin, sync::Arc};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::manager::{McpManager, MountSpec};

/// Cold declaration resolver which applies Environment config, secret, and auth
/// policy before a Python mount reaches the lifecycle supervisor.
pub trait ControlMountResolver: Send + Sync {
	/// Resolves one validated Python declaration without exposing credential
	/// bytes.
	fn resolve<'a>(
		&'a self,
		declaration: Value,
	) -> Pin<Box<dyn Future<Output = Result<MountSpec, McpControlError>> + Send + 'a>>;
}

/// Environment-owned implementation of the declared Python MCP CONTROL calls.
pub struct McpControl {
	manager:  Arc<McpManager>,
	resolver: Arc<dyn ControlMountResolver>,
	identity: Arc<super::super::exthost::control::ControlConnectionIdentity>,
}

impl McpControl {
	/// Binds CONTROL dispatch to the same manager used by RPC, dyn, and
	/// `mcp://` reads.
	pub fn new(
		manager: Arc<McpManager>,
		resolver: Arc<dyn ControlMountResolver>,
		identity: Arc<super::super::exthost::control::ControlConnectionIdentity>,
	) -> Self {
		Self { manager, resolver, identity }
	}

	/// Dispatches one MCP CONTROL operation and returns JSON-safe typed facts.
	pub async fn dispatch(
		&self,
		operation: &str,
		arguments: Value,
	) -> Result<Value, McpControlError> {
		self
			.dispatch_with_cancel(operation, arguments, CancellationToken::new())
			.await
	}

	/// Dispatches one cancellable MCP CONTROL operation.
	pub async fn dispatch_with_cancel(
		&self,
		operation: &str,
		arguments: Value,
		cancellation: CancellationToken,
	) -> Result<Value, McpControlError> {
		match operation {
			"omp.mcp.mount" => {
				let declaration = arguments
					.get("spec")
					.cloned()
					.ok_or(McpControlError::InvalidArguments)?;
				let spec = tokio::select! {
					biased;
					() = cancellation.cancelled() => {
						return Err(McpControlError::Manager(
							super::manager::ManagerError::Cancelled,
						));
					},
					spec = self.resolver.resolve(declaration) => spec?,
				};
				let server = spec.name.clone();
				self
					.manager
					.control_mount(&self.identity, spec, &cancellation)
					.await?;
				self.mount_result(&server)
			},
			"omp.mcp.unmount" => {
				let server = arguments
					.get("server")
					.and_then(Value::as_str)
					.filter(|server| !server.is_empty())
					.ok_or(McpControlError::InvalidArguments)?;
				Ok(json!({
					"removed": self.manager.control_unmount(&self.identity, server).await?
				}))
			},
			"omp.mcp.servers" => self.servers_result(),
			"omp.mcp.invoke" => {
				let server = arguments
					.get("server")
					.and_then(Value::as_str)
					.filter(|server| !server.is_empty())
					.ok_or(McpControlError::InvalidArguments)?;
				let tool = arguments
					.get("tool")
					.and_then(Value::as_str)
					.filter(|tool| !tool.is_empty())
					.ok_or(McpControlError::InvalidArguments)?;
				let params = arguments
					.get("arguments")
					.cloned()
					.unwrap_or_else(|| json!({}));
				if !params.is_object() {
					return Err(McpControlError::InvalidArguments);
				}
				let result = self
					.manager
					.control_invoke_scoped(&self.identity, server, tool, params, cancellation)
					.await?;
				let content = decode_json(&result.content_json)?;
				let structured_content = decode_optional_json(&result.structured_content_json)?;
				let meta = decode_optional_json(&result.meta_json)?;
				Ok(json!({
					"content": content,
					"structured_content": structured_content,
					"meta": meta,
					"is_error": result.is_error,
					"truncated": result.truncated,
					"dispatch_certainty": result.dispatch_certainty,
					"retry_count": result.retry_count,
					"auth_retried": result.auth_retried,
					"effects_unknown": result.effects_unknown,
				}))
			},
			_ => Err(McpControlError::UnknownOperation),
		}
	}

	fn mount_result(&self, server: &str) -> Result<Value, McpControlError> {
		if !self.manager.control_owns(&self.identity, server) {
			return Err(McpControlError::Manager(super::manager::ManagerError::OwnershipDenied));
		}
		let snapshot = self.manager.catalog_snapshot();
		let devices = snapshot
			.leaves
			.iter()
			.filter(|leaf| leaf.owner.root == server && leaf.value.kind.as_str() == "tool")
			.map(|leaf| {
				let definition = decode_json(&leaf.value.definition_json)?;
				Ok(json!({
					"name": leaf.name,
					"family": leaf.rev.family,
					"rev": leaf.rev.n,
					"server": leaf.value.server,
					"kind": leaf.value.kind,
					"definition": definition,
					"documentation": leaf.value.documentation,
					"precedence": leaf.value.precedence,
					"tier": leaf.value.tier,
					"catalog_epoch": snapshot.epoch,
				}))
			})
			.collect::<Result<Vec<_>, McpControlError>>()?;
		Ok(json!({ "devices": devices, "catalog_epoch": snapshot.epoch }))
	}

	fn servers_result(&self) -> Result<Value, McpControlError> {
		let snapshot = self.manager.catalog_snapshot();
		let status = self.manager.servers();
		let owned = self.manager.control_server_names(&self.identity);
		let servers = status
			.servers
			.into_iter()
			.filter(|status| {
				status
					.server
					.as_ref()
					.is_some_and(|server| owned.contains(server.name.as_str()))
			})
			.filter_map(|status| {
				Some((
					status.server?,
					status.state,
					status.generation,
					status.definition_epoch,
					status.detail,
				))
			})
			.map(|(server, state, generation, definition_epoch, detail)| {
				let leaves = snapshot
					.leaves
					.iter()
					.filter(|leaf| leaf.owner.root.as_str() == server.name)
					.collect::<Vec<_>>();
				let mut endpoints = Vec::new();
				let mut resources = Vec::new();
				let mut prompts = Vec::new();
				let mut instructions = None;
				let mut protocol_version = None;
				for leaf in leaves {
					let definition = decode_json(&leaf.value.definition_json)?;
					instructions = instructions.or_else(|| leaf.value.server_instructions.clone());
					protocol_version = protocol_version.or_else(|| protocol_version_for_rev(leaf.rev.n));
					match leaf.value.kind.as_str() {
						"tool" => {
							let name = definition
								.get("name")
								.and_then(Value::as_str)
								.ok_or(McpControlError::InvalidResult)?;
							endpoints.push(name.to_owned());
						},
						"resource" | "resource-template" => {
							let template = leaf.value.kind.as_str() == "resource-template";
							let uri_key = if template { "uriTemplate" } else { "uri" };
							let uri = definition
								.get(uri_key)
								.and_then(Value::as_str)
								.ok_or(McpControlError::InvalidResult)?;
							let name = definition
								.get("name")
								.and_then(Value::as_str)
								.ok_or(McpControlError::InvalidResult)?;
							resources.push(json!({
								"uri": uri,
								"name": name,
								"media_type": definition.get("mimeType"),
								"template": template,
							}));
						},
						"prompt" => {
							let name = definition
								.get("name")
								.and_then(Value::as_str)
								.ok_or(McpControlError::InvalidResult)?;
							prompts.push(name.to_owned());
						},
						_ => return Err(McpControlError::InvalidResult),
					}
				}
				Ok(json!({
					"name": server.name,
					"state": state,
					"generation": generation,
					"definition_epoch": definition_epoch,
					"protocol_version": protocol_version,
					"instructions": instructions,
					"endpoints": endpoints,
					"resources": resources,
					"prompts": prompts,
					"last_error": (!detail.is_empty()).then_some(detail),
				}))
			})
			.collect::<Result<Vec<_>, McpControlError>>()?;
		Ok(json!({ "servers": servers, "definition_epoch": status.definition_epoch }))
	}
}

fn decode_json(bytes: &[u8]) -> Result<Value, McpControlError> {
	serde_json::from_slice(bytes).map_err(|_| McpControlError::InvalidResult)
}

fn decode_optional_json(bytes: &[u8]) -> Result<Value, McpControlError> {
	if bytes.is_empty() {
		Ok(Value::Null)
	} else {
		decode_json(bytes)
	}
}

fn protocol_version_for_rev(revision: u16) -> Option<&'static str> {
	match revision {
		1 => Some("2024-11-05"),
		2 => Some("2025-03-26"),
		3 => Some("2025-06-18"),
		4 => Some("2025-11-25"),
		_ => None,
	}
}

/// MCP CONTROL dispatch failure.
#[derive(Debug, thiserror::Error)]
pub enum McpControlError {
	/// Operation is outside the declared MCP vocabulary.
	#[error("unknown MCP CONTROL operation")]
	UnknownOperation,
	/// Arguments do not match the selected operation.
	#[error("invalid MCP CONTROL arguments")]
	InvalidArguments,
	/// Declaration failed Environment config, secret, or authorization policy.
	#[error("MCP mount declaration was rejected")]
	DeclarationRejected,
	/// The shared MCP authority produced malformed catalog or result JSON.
	#[error("MCP authority returned an invalid result")]
	InvalidResult,
	/// Lifecycle manager rejected the operation.
	#[error(transparent)]
	Manager(#[from] super::manager::ManagerError),
	/// Shared MCP service rejected the operation.
	#[error(transparent)]
	Service(#[from] super::McpServiceError),
}
