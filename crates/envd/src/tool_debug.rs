//! Production bridge from `debug@1` to the Environment DAP wire.

use std::{collections::BTreeMap, future::Future, sync::Arc, time::Duration};

use bytes::Bytes;
use omp_core::{Str, encoding::hex};
use omp_proto::document::v1 as pb;
use omp_tools::debug::{Action, DebugControl, Fault, Params, Payload, render};
use parking_lot::RwLock;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::docs::{DapRegistryEvent, DocumentError, DocumentHost};

/// Environment-owned implementation of the revisioned debugger tool.
#[derive(Clone)]
pub struct DocumentDebugControl {
	documents: DocumentHost,
	sessions:  Arc<RwLock<BTreeMap<Str, pb::DapSessionRef>>>,
}

impl DocumentDebugControl {
	/// Binds the project document authority.
	pub fn new(documents: DocumentHost) -> Self {
		Self { documents, sessions: Arc::new(RwLock::new(BTreeMap::new())) }
	}

	fn session(&self, requested: Option<&Str>) -> Result<(Str, pb::DapSessionRef), Fault> {
		let sessions = self.sessions.read();
		if let Some(id) = requested {
			return sessions
				.get(id)
				.cloned()
				.map(|session| (id.clone(), session))
				.ok_or(Fault::Unavailable);
		}
		sessions
			.iter()
			.next()
			.map(|(id, session)| (id.clone(), session.clone()))
			.ok_or(Fault::Unavailable)
	}
}

impl DebugControl for DocumentDebugControl {
	fn execute(
		&self,
		params: Params,
		_timeout: Duration,
		cancel: CancellationToken,
	) -> impl Future<Output = Result<Payload, Fault>> + Send + '_ {
		async move {
			if matches!(params.action, Action::Launch | Action::Attach) {
				return self.start(params, &cancel).await;
			}
			let (session_id, session) = self.session(params.session.as_ref())?;
			let arguments = action_arguments(&params);
			let required_capability = if params.action.read_only() {
				pb::DapCapability::Read
			} else {
				pb::DapCapability::Execute
			};
			let (response, events) = self
				.documents
				.dap_action(
					pb::DapActionRequest {
						session:             Some(session.clone()),
						expected_revision:   session.revision,
						required_capability: required_capability as i32,
						command:             params.action.to_string(),
						arguments_json:      Bytes::from(
							serde_json::to_vec(&arguments).map_err(|_| Fault::InvalidArguments)?,
						),
						max_response_bytes:  256 * 1024,
					},
					&cancel,
				)
				.await
				.map_err(map_document_error)?;
			let next = response.session.ok_or(Fault::Adapter)?;
			self
				.sessions
				.write()
				.insert(session_id.clone(), next.clone());
			let mut data = if response.body_json.is_empty() {
				json!({})
			} else {
				serde_json::from_slice(&response.body_json).map_err(|_| Fault::Adapter)?
			};
			merge_events(&mut data, events);
			let output = render(params.action, &data);
			if params.action == Action::Terminate {
				self.sessions.write().remove(&session_id);
			}
			Ok(Payload {
				action: params.action,
				session: Some(session_id),
				revision: Some(next.revision),
				output,
				data,
			})
		}
	}
}

impl DocumentDebugControl {
	async fn start(&self, params: Params, cancel: &CancellationToken) -> Result<Payload, Fault> {
		let adapter = params.adapter.as_deref().ok_or(Fault::InvalidArguments)?;
		let configuration = start_arguments(&params);
		let capabilities = vec![
			omp_proto::document::v1::DapCapability::Read as i32,
			omp_proto::document::v1::DapCapability::Execute as i32,
		];
		let workspace_uri = self.documents.hello().root_uri.to_string();
		let encoded =
			Bytes::from(serde_json::to_vec(&configuration).map_err(|_| Fault::InvalidArguments)?);
		let (response, events) = match params.action {
			Action::Launch => {
				self
					.documents
					.dap_launch(
						pb::DapLaunchRequest {
							adapter: adapter.to_owned(),
							workspace_uri,
							configuration_json: encoded,
							capabilities,
							max_event_bytes: 64 * 1024,
						},
						cancel,
					)
					.await
			},
			Action::Attach => {
				self
					.documents
					.dap_attach(
						pb::DapAttachRequest {
							adapter: adapter.to_owned(),
							workspace_uri,
							configuration_json: encoded,
							capabilities,
							max_event_bytes: 64 * 1024,
						},
						cancel,
					)
					.await
			},
			_ => unreachable!("start handles launch and attach only"),
		}
		.map_err(map_document_error)?;
		let session = response.session.ok_or(Fault::Adapter)?;
		let id = Str::from(hex::encode(&session.session_id).into_string());
		self.sessions.write().insert(id.clone(), session.clone());
		let mut data = json!({
			"session": id,
			"revision": session.revision,
			"capabilities": serde_json::from_slice::<Value>(&response.adapter_capabilities_json).unwrap_or(Value::Null),
		});
		merge_events(&mut data, events);
		Ok(Payload {
			action: params.action,
			session: Some(id),
			revision: Some(session.revision),
			output: render(params.action, &data),
			data,
		})
	}
}

fn start_arguments(params: &Params) -> Value {
	let mut arguments = params
		.arguments
		.as_ref()
		.and_then(Value::as_object)
		.cloned()
		.unwrap_or_default();
	if let Some(path) = &params.path {
		arguments.insert("program".to_owned(), json!(path));
	}
	if let Some(pid) = params.pid {
		arguments.insert("pid".to_owned(), json!(pid));
		arguments.insert("processId".to_owned(), json!(pid));
	}
	if let Some(port) = params.port {
		arguments.insert("port".to_owned(), json!(port));
	}
	if let Some(host) = &params.host {
		arguments.insert("host".to_owned(), json!(host));
	}
	Value::Object(arguments)
}

fn action_arguments(params: &Params) -> Value {
	let mut arguments = params
		.arguments
		.as_ref()
		.and_then(Value::as_object)
		.cloned()
		.unwrap_or_default();
	insert(&mut arguments, "threadId", params.thread_id);
	insert(&mut arguments, "frameId", params.frame_id);
	insert(&mut arguments, "variablesReference", params.variables_reference);
	insert(&mut arguments, "start", params.start);
	insert(&mut arguments, "count", params.count);
	insert(&mut arguments, "offset", params.offset);
	insert_str(&mut arguments, "expression", params.expression.as_ref());
	insert_str(&mut arguments, "context", params.context.as_ref());
	insert_str(&mut arguments, "memoryReference", params.memory_reference.as_ref());
	insert_str(&mut arguments, "data", params.data.as_ref());
	insert_str(&mut arguments, "granularity", params.granularity.as_ref());
	match params.action {
		Action::SetBreakpoint | Action::RemoveBreakpoint => {
			arguments.insert("source".to_owned(), json!({"path": params.path}));
			arguments.insert(
				"breakpoint".to_owned(),
				json!({"line": params.line, "column": params.column, "condition": params.condition, "hitCondition": params.hit_condition}),
			);
		},
		Action::SetFunctionBreakpoint | Action::RemoveFunctionBreakpoint => {
			arguments.insert(
				"breakpoint".to_owned(),
				json!({"name": params.function, "condition": params.condition, "hitCondition": params.hit_condition}),
			);
		},
		Action::SetInstructionBreakpoint | Action::RemoveInstructionBreakpoint => {
			arguments.insert(
				"breakpoint".to_owned(),
				json!({"instructionReference": params.instruction_reference, "offset": params.offset, "condition": params.condition, "hitCondition": params.hit_condition}),
			);
		},
		Action::SetDataBreakpoint | Action::RemoveDataBreakpoint => {
			arguments.insert(
				"breakpoint".to_owned(),
				json!({"dataId": params.data_id, "accessType": params.access_type, "condition": params.condition, "hitCondition": params.hit_condition}),
			);
		},
		Action::DataBreakpointInfo => {
			insert_str(&mut arguments, "name", params.expression.as_ref());
		},
		Action::Disassemble => {
			insert(&mut arguments, "instructionOffset", params.offset);
			insert(&mut arguments, "instructionCount", params.count);
		},
		Action::ReadMemory => {
			insert(&mut arguments, "count", params.count);
		},
		Action::CustomRequest => {
			insert_str(&mut arguments, "command", params.command.as_ref());
			arguments
				.insert("arguments".to_owned(), params.arguments.clone().unwrap_or_else(|| json!({})));
		},
		_ => {},
	}
	Value::Object(arguments)
}

fn insert<T: serde::Serialize>(map: &mut Map<String, Value>, key: &str, value: Option<T>) {
	if let Some(value) = value {
		map.insert(key.to_owned(), json!(value));
	}
}

fn insert_str(map: &mut Map<String, Value>, key: &str, value: Option<&Str>) {
	if let Some(value) = value {
		map.insert(key.to_owned(), json!(value));
	}
}

fn merge_events(data: &mut Value, events: Vec<DapRegistryEvent>) {
	let mut lifecycle = Vec::new();
	let mut output = Vec::new();
	for event in events {
		match event {
			DapRegistryEvent::Output(event) => output.extend_from_slice(&event.output),
			DapRegistryEvent::Event(event) => lifecycle.push(json!({
				"sequence": event.sequence,
				"event": event.event,
				"body": serde_json::from_slice::<Value>(&event.body_json).unwrap_or(Value::Null),
			})),
		}
	}
	if !lifecycle.is_empty() {
		data["events"] = Value::Array(lifecycle);
	}
	if !output.is_empty() {
		data["output"] = Value::String(String::from_utf8_lossy(&output).into_owned());
	}
}

fn map_document_error(error: DocumentError) -> Fault {
	match error {
		DocumentError::Cancelled => Fault::Cancelled,
		DocumentError::Disconnected => Fault::Unavailable,
		DocumentError::Protocol { code, .. } => match pb::ProtocolErrorCode::try_from(code).ok() {
			Some(pb::ProtocolErrorCode::PermissionDenied) => Fault::Unauthorized,
			Some(
				pb::ProtocolErrorCode::RevisionExpired
				| pb::ProtocolErrorCode::PreconditionFailed
				| pb::ProtocolErrorCode::ContentModified,
			) => Fault::Stale,
			Some(pb::ProtocolErrorCode::NotFound) => Fault::Unavailable,
			Some(pb::ProtocolErrorCode::Cancelled) => Fault::Cancelled,
			_ => Fault::Adapter,
		},
		DocumentError::Wire(_) | DocumentError::MalformedResponse(_) => Fault::Adapter,
	}
}
