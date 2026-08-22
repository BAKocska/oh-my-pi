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
}

impl McpControl {
	/// Binds CONTROL dispatch to the same manager used by RPC, dyn, and
	/// `mcp://` reads.
	#[must_use]
	pub fn new(manager: Arc<McpManager>, resolver: Arc<dyn ControlMountResolver>) -> Self {
		Self { manager, resolver }
	}

	/// Dispatches one MCP CONTROL operation and returns JSON-safe typed facts.
	pub async fn dispatch(
		&self,
		operation: &str,
		arguments: Value,
	) -> Result<Value, McpControlError> {
		match operation {
			"omp.mcp.mount" => {
				let spec = self.resolver.resolve(arguments).await?;
				let server = spec.name.clone();
				self.manager.mount(spec).await;
				Ok(self.mount_result(&server))
			},
			"omp.mcp.unmount" => {
				let server = arguments
					.get("server")
					.and_then(Value::as_str)
					.ok_or(McpControlError::InvalidArguments)?;
				Ok(json!({ "removed": self.manager.unmount(server).await? }))
			},
			"omp.mcp.servers" => Ok(self.servers_result()),
			"omp.mcp.invoke" => {
				let server = arguments
					.get("server")
					.and_then(Value::as_str)
					.ok_or(McpControlError::InvalidArguments)?;
				let tool = arguments
					.get("tool")
					.and_then(Value::as_str)
					.ok_or(McpControlError::InvalidArguments)?;
				let params = arguments
					.get("arguments")
					.cloned()
					.unwrap_or_else(|| json!({}));
				let result = self
					.manager
					.control_invoke(server, tool, params, CancellationToken::new())
					.await?;
				let content =
					serde_json::from_slice::<Value>(&result.content_json).unwrap_or(Value::Null);
				let structured_content =
					serde_json::from_slice::<Value>(&result.structured_content_json)
						.unwrap_or(Value::Null);
				let meta = serde_json::from_slice::<Value>(&result.meta_json).unwrap_or(Value::Null);
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

	fn mount_result(&self, server: &str) -> Value {
		let snapshot = self.manager.catalog_snapshot();
		let devices = snapshot
			.leaves
			.iter()
			.filter(|leaf| leaf.owner.root == server && leaf.value.kind.as_str() == "tool")
			.map(|leaf| {
				let definition =
					serde_json::from_slice::<Value>(&leaf.value.definition_json).unwrap_or(Value::Null);
				json!({
					"name": leaf.name,
					"family": leaf.rev.family,
					"rev": leaf.rev.n,
					"server": leaf.value.server,
					"kind": leaf.value.kind,
					"definition": definition,
					"documentation": leaf.value.documentation,
					"catalog_epoch": snapshot.epoch,
				})
			})
			.collect::<Vec<_>>();
		json!({ "devices": devices, "catalog_epoch": snapshot.epoch })
	}

	fn servers_result(&self) -> Value {
		let snapshot = self.manager.catalog_snapshot();
		let status = self.manager.servers();
		let servers = status
			.servers
			.into_iter()
			.filter_map(|status| {
				let server = status.server?;
				let endpoints = snapshot
					.leaves
					.iter()
					.filter(|leaf| leaf.owner.root.as_str() == server.name)
					.map(|leaf| leaf.name.clone())
					.collect::<Vec<_>>();
				Some(json!({
					"name": server.name,
					"state": status.state,
					"generation": status.generation,
					"definition_epoch": status.definition_epoch,
					"endpoints": endpoints,
					"last_error": (!status.detail.is_empty()).then_some(status.detail),
				}))
			})
			.collect::<Vec<_>>();
		json!({ "servers": servers, "definition_epoch": status.definition_epoch })
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
	/// Lifecycle manager rejected the operation.
	#[error(transparent)]
	Manager(#[from] super::manager::ManagerError),
	/// Shared MCP service rejected the operation.
	#[error(transparent)]
	Service(#[from] super::McpServiceError),
}
