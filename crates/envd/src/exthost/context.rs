//! Generation-fenced JSON CONTROL adapter for the live Agent context owner.

use std::sync::Arc;

use async_trait::async_trait;
use omp_agent::AgentHostControl;
use omp_core::{InvocationPhase, Str};
use serde_json::{Map, Value, json};

use super::control::{
	ControlAuthority, ControlConnectionIdentity, ControlEffect, ControlProtocolError,
	ControlRequestContext,
};

/// Connection-scoped authority over the active Agent's durable context
/// projection.
pub struct LiveContextControlOwner {
	identity: Arc<ControlConnectionIdentity>,
	session:  Str,
	host:     AgentHostControl,
}

impl LiveContextControlOwner {
	/// Binds one authenticated connection to the active session owner.
	pub fn new(
		identity: Arc<ControlConnectionIdentity>,
		session: Str,
		host: AgentHostControl,
	) -> Self {
		Self { identity, session, host }
	}

	fn validate(&self, context: &ControlRequestContext) -> Result<(), ControlProtocolError> {
		let actual = &context.connection;
		if self.identity.extension != actual.extension
			|| self.identity.principal != actual.principal
			|| self.identity.artifact_digest != actual.artifact_digest
			|| self.identity.layer != actual.layer
			|| self.identity.tier != actual.tier
			|| self.identity.host_generation != actual.host_generation
			|| self.identity.session_generation != actual.session_generation
		{
			return Err(ControlProtocolError::new(
				"StaleGeneration",
				"context authority belongs to a replaced CONTROL connection",
			));
		}
		let invocation = context.invocation.as_ref().ok_or_else(|| {
			ControlProtocolError::new(
				"InvalidPhase",
				"live context CONTROL requires host-issued invocation authority",
			)
		})?;
		if invocation.session != self.session {
			return Err(ControlProtocolError::new(
				"ContextGone",
				"invocation belongs to a different session generation",
			));
		}
		if !invocation.phase.allows_operation(InvocationPhase::Open) {
			return Err(ControlProtocolError::new(
				"InvalidPhase",
				"settled invocations cannot access live context",
			));
		}
		Ok(())
	}
}

#[async_trait]
impl ControlAuthority for LiveContextControlOwner {
	fn handles(&self, operation: &str) -> bool {
		matches!(
			operation,
			"omp.context.view"
				| "omp.context.usage"
				| "omp.context.message.parts"
				| "omp.context.message.verdict"
				| "omp.context.message.raw_args"
				| "omp.context.pin"
				| "omp.context.unpin"
				| "omp.context.compact"
				| "omp.context.epoch"
		)
	}

	fn authorize(
		&self,
		context: &ControlRequestContext,
		_operation: &str,
		_arguments: &Map<String, Value>,
	) -> Result<(), ControlProtocolError> {
		self.validate(context)
	}

	async fn request(
		&self,
		context: ControlRequestContext,
		operation: Str,
		arguments: Map<String, Value>,
	) -> Result<Value, ControlProtocolError> {
		self.validate(&context)?;
		let result = self
			.host
			.request(operation.clone(), arguments)
			.await
			.map_err(|message| {
				let code = message
					.as_str()
					.split_once(':')
					.map_or("ContextError", |(code, _)| code);
				ControlProtocolError::new(Str::from(code), message)
			})?;
		let schema = match operation.as_str() {
			"omp.context.view" => "omp.context.view.v1",
			"omp.context.usage" => "omp.context.usage.v1",
			"omp.context.message.parts" => "omp.context.message.parts.v1",
			"omp.context.message.verdict" => "omp.context.message.verdict.v1",
			"omp.context.message.raw_args" => "omp.context.message.raw_args.v1",
			"omp.context.pin" => "omp.context.pin.v1",
			"omp.context.unpin" => "omp.context.unpin.v1",
			"omp.context.compact" => "omp.context.compact.v1",
			"omp.context.epoch" => "omp.context.epoch.v1",
			_ => unreachable!("handles is exhaustive"),
		};
		Ok(json!({"schema": schema, "result": result}))
	}

	async fn effect(
		&self,
		context: ControlRequestContext,
		_effect: ControlEffect,
	) -> Result<(), ControlProtocolError> {
		self.validate(&context)?;
		Err(ControlProtocolError::new(
			"InvalidEffect",
			"context authority does not own child observations",
		))
	}
}
