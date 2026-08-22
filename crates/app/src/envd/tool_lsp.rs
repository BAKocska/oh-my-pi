//! Production bridge from `lsp@1` to the project document authority.

use std::{
	future::Future,
	path::{Component, Path},
	time::Duration,
};

use bytes::Bytes;
use omp_core::Str;
use omp_docserver::position::{PositionEncoding, TextEdit, apply_text_edits};
use omp_proto::document::v1 as pb;
use omp_tools::lsp::{
	Action, Fault, LspControl, Params, Payload, actions, navigation, refactor, render,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use url::Url;

use super::{
	docs::{DocumentHost, lease_target},
	tool_document::{read_whole, transaction_id},
};

/// Environment-owned implementation of the revisioned LSP tool.
#[derive(Clone)]
pub struct DocumentLspControl {
	documents: DocumentHost,
}

impl DocumentLspControl {
	/// Binds the project document authority.
	#[must_use]
	pub const fn new(documents: DocumentHost) -> Self {
		Self { documents }
	}

	fn file_uri(&self, file: &str) -> Result<Url, Fault> {
		let mut root = Url::parse(self.documents.hello().root_uri.as_str())
			.map_err(|_| Fault::InvalidArguments)?;
		if file == "*" {
			return Ok(root);
		}
		let path = Path::new(file);
		if path.is_absolute()
			|| path.components().any(|component| {
				matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_))
			}) {
			return Err(Fault::InvalidArguments);
		}
		root
			.path_segments_mut()
			.map_err(|_| Fault::InvalidArguments)?
			.pop_if_empty()
			.extend(path.components().filter_map(|component| match component {
				Component::Normal(value) => value.to_str(),
				Component::CurDir => None,
				_ => None,
			}));
		Ok(root)
	}

	async fn apply_workspace_edit(
		&self,
		edit: &Value,
		encoding: PositionEncoding,
		cancel: &CancellationToken,
	) -> Result<usize, Fault> {
		let changes = edit
			.get("changes")
			.and_then(Value::as_object)
			.ok_or(Fault::WorkspaceEdit)?;
		let mut operations = Vec::with_capacity(changes.len());
		for (uri, edits) in changes {
			let lease = self
				.documents
				.open(Str::from(uri.as_str()), None, cancel)
				.await
				.map_err(|_| Fault::WorkspaceEdit)?;
			let content = read_whole(&self.documents, &lease)
				.await
				.map_err(|_| Fault::WorkspaceEdit)?;
			let text = std::str::from_utf8(&content).map_err(|_| Fault::WorkspaceEdit)?;
			let edits = serde_json::from_value::<Vec<TextEdit>>(edits.clone())
				.map_err(|_| Fault::WorkspaceEdit)?;
			let content =
				apply_text_edits(text, &edits, encoding).map_err(|_| Fault::WorkspaceEdit)?;
			operations.push(pb::DocumentMutation {
				document:  Some(lease_target(&lease)),
				operation: Some(pb::document_mutation::Operation::Text(pb::TextMutation {
					base_revision: lease.head().revision.clone(),
					change:        Some(pb::text_mutation::Change::ProposedContent(content)),
					stale_policy:  pb::StalePolicy::Fail as i32,
					format_policy: pb::FormatPolicy::BestEffort as i32,
				})),
			});
		}
		let response = self
			.documents
			.commit_transaction(
				transaction_id(self.documents.hello().server_epoch.as_ref()),
				operations,
				cancel,
			)
			.await
			.map_err(|_| Fault::WorkspaceEdit)?;
		match response.outcome {
			Some(pb::commit_transaction_response::Outcome::Committed(committed)) => {
				Ok(committed.operations.len())
			},
			_ => Err(Fault::WorkspaceEdit),
		}
	}
}

impl LspControl for DocumentLspControl {
	fn execute(
		&self,
		params: Params,
		_timeout: Duration,
		cancel: CancellationToken,
	) -> impl Future<Output = Result<Payload, Fault>> + Send + '_ {
		async move {
			let file = params.file.as_deref().unwrap_or("*");
			let uri = self.file_uri(file)?;
			if file == "*" && !matches!(params.action, Action::Status) {
				return Err(Fault::InvalidArguments);
			}
			if params.action == Action::Status && file == "*" {
				return Ok(Payload {
					action:  params.action,
					servers: Vec::new(),
					output:  Str::from(
						"Native project document daemon is connected; provide a file for binding status",
					),
					data:    json!({ "daemon": "connected", "workspace": self.documents.hello().root_uri }),
				});
			}
			let lease = self
				.documents
				.open(Str::from(uri.as_str()), None, &cancel)
				.await
				.map_err(|_| Fault::Unavailable)?;
			let bindings = self
				.documents
				.get_lsp_bindings(
					pb::GetLspBindingsRequest { document: Some(lease_target(&lease)) },
					&cancel,
				)
				.await
				.map_err(|_| Fault::Server)?
				.bindings;
			let selected = bindings
				.into_iter()
				.filter(|binding| {
					params
						.server
						.as_ref()
						.is_none_or(|name| name.as_str() == binding.name)
				})
				.collect::<Vec<_>>();
			if selected.is_empty() {
				return Err(Fault::Unavailable);
			}
			let servers = selected
				.iter()
				.map(|binding| Str::from(binding.name.as_str()))
				.collect::<Vec<_>>();
			if params.action == Action::Status {
				let data = Value::Array(selected.iter().map(|binding| json!({ "name": binding.name, "state": "ready", "position_encoding": binding.sync_policy.as_ref().map(|policy| policy.position_encoding.as_str()) })).collect());
				return Ok(Payload {
					action: params.action,
					servers,
					output: render::structured(&data, 50),
					data,
				});
			}
			if params.action == Action::Capabilities {
				let data = Value::Array(
					selected
						.iter()
						.map(|binding| {
							serde_json::from_slice(&binding.capabilities_json).unwrap_or(Value::Null)
						})
						.collect(),
				);
				return Ok(Payload {
					action: params.action,
					servers,
					output: Str::from(serde_json::to_string_pretty(&data).unwrap_or_default()),
					data,
				});
			}
			let content = read_whole(&self.documents, &lease)
				.await
				.map_err(|_| Fault::Server)?;
			let line = params.line.unwrap_or(1);
			let character = if let Some(symbol) = &params.symbol {
				let target =
					navigation::parse_symbol_target(symbol).map_err(|_| Fault::InvalidArguments)?;
				let text = std::str::from_utf8(&content).map_err(|_| Fault::InvalidArguments)?;
				let source_line = text
					.lines()
					.nth(line.saturating_sub(1) as usize)
					.ok_or(Fault::InvalidArguments)?;
				navigation::resolve_symbol_column(source_line, &target)
					.ok_or(Fault::InvalidArguments)?
			} else {
				0
			};
			let method = if params.action == Action::Request {
				params.method.as_deref().ok_or(Fault::InvalidArguments)?
			} else if params.action == Action::Reload {
				"rust-analyzer/reloadWorkspace"
			} else {
				actions::method(params.action).ok_or(Fault::InvalidArguments)?
			};
			let mut request_params = actions::auto_parameters(
				params.params.clone(),
				Some(uri.as_str()),
				params.line,
				Some(character),
			);
			if params.action == Action::References {
				request_params["context"] = json!({ "includeDeclaration": true });
			}
			if params.action == Action::Rename {
				request_params["newName"] = json!(params.new_name);
			}
			if params.action == Action::Symbols {
				request_params = json!({ "textDocument": { "uri": uri.as_str() } });
			}
			if params.action == Action::CodeActions {
				request_params["range"] = json!({ "start": { "line": line - 1, "character": character }, "end": { "line": line - 1, "character": character } });
				request_params["context"] = json!({ "diagnostics": [], "triggerKind": 1 });
			}
			let mut results = Vec::new();
			for binding in &selected {
				let response = self
					.documents
					.lsp_request(
						pb::LspRequest {
							server_id:    binding.server_id.clone(),
							method:       method.into(),
							params_json:  Bytes::from(
								serde_json::to_vec(&request_params).map_err(|_| Fault::InvalidArguments)?,
							),
							document:     Some(lease_target(&lease)),
							revision:     lease.head().revision.clone(),
							stale_policy: pb::LspStalePolicy::Fail as i32,
						},
						&cancel,
					)
					.await
					.map_err(|_| Fault::Server)?;
				match response.outcome {
					Some(pb::lsp_response::Outcome::ResultJson(bytes)) => {
						results.push(serde_json::from_slice(&bytes).map_err(|_| Fault::Server)?)
					},
					Some(pb::lsp_response::Outcome::Error(_)) | None => return Err(Fault::Server),
				}
			}
			let mut data = if results.len() == 1 {
				results.remove(0)
			} else {
				Value::Array(results)
			};
			if matches!(
				params.action,
				Action::Definition
					| Action::TypeDefinition
					| Action::Implementation
					| Action::References
			) {
				data = Value::Array(navigation::normalize_locations(
					&data,
					if params.action == Action::References {
						200
					} else {
						50
					},
				));
			}
			if params.action == Action::Rename {
				refactor::validate_workspace_edit(&data).map_err(|_| Fault::WorkspaceEdit)?;
				if params.apply.unwrap_or(true) {
					let encoding = selected
						.first()
						.and_then(|binding| binding.sync_policy.as_ref())
						.map(|policy| PositionEncoding::from_lsp_name(Some(&policy.position_encoding)))
						.unwrap_or_default();
					let applied = self.apply_workspace_edit(&data, encoding, &cancel).await?;
					let output =
						Str::from(format!("Applied rename transaction across {applied} document(s)"));
					return Ok(Payload { action: params.action, servers, output, data });
				}
			}
			let output = match params.action {
				Action::Hover => data
					.get("contents")
					.map_or_else(|| Str::from("No hover information"), navigation::hover_text),
				Action::Rename => refactor::preview(&data),
				_ => render::structured(&data, 200),
			};
			Ok(Payload { action: params.action, servers, output, data })
		}
	}
}
