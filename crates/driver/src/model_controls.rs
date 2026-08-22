//! Durable model preferences and journal-restored session overrides.

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use omp_catalog::{ModelKey, ThinkingEffort};
use omp_core::{InvocationPhase, LifecyclePhase, Str, sf};
use omp_envd::exthost::control::{
	ControlAuthority, ControlAuthorityFactory, ControlCompositionError, ControlConnectionIdentity,
	ControlEffect, ControlProtocolError, ControlRequestContext,
};
use serde::Serialize;
use serde_json::{Map, Value, json};
use thiserror::Error;

/// Direction for model and role cycling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CycleDirection {
	/// Advance and wrap at the end.
	Forward,
	/// Move backward and wrap at the beginning.
	Backward,
}

/// One enabled model in a temporary cycle scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScopedModel {
	/// Catalog model key.
	pub model:    ModelKey,
	/// Optional role-specified thinking selection.
	pub thinking: Option<ThinkingEffort>,
}

/// Journal payload for a session-only model override.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournaledModelOverride {
	/// Role whose configured model was temporarily replaced.
	pub role:     Str,
	/// Effective session model.
	pub model:    ModelKey,
	/// Optional temporary thinking selection.
	pub thinking: Option<ThinkingEffort>,
}

/// Result of bidirectional role cycling.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoleCycleSelection {
	/// Selected configured role.
	pub role:  Str,
	/// Model assigned to the role.
	pub model: ModelKey,
}

/// Split authority for durable preferences and journaled effective overrides.
#[derive(Clone, Debug, Default)]
pub struct ModelControls {
	durable_roles: BTreeMap<Str, ModelKey>,
	override_:     Option<JournaledModelOverride>,
	scoped:        Arc<[ScopedModel]>,
	active_role:   Option<Str>,
}

impl ModelControls {
	/// Restores durable settings without creating a journal event.
	pub fn from_durable(durable_roles: BTreeMap<Str, ModelKey>) -> Self {
		Self { durable_roles, ..Self::default() }
	}

	/// Replaces one durable `/model` preference.
	///
	/// The caller persists this through settings authority. This operation never
	/// creates or changes a session override.
	pub fn set_durable(&mut self, role: impl Into<Str>, model: ModelKey) {
		self.durable_roles.insert(role.into(), model);
	}

	/// Applies a temporary Ctrl-P or `/switch` selection and returns its journal
	/// payload.
	pub fn switch_session(
		&mut self,
		role: impl Into<Str>,
		model: ModelKey,
		thinking: Option<ThinkingEffort>,
	) -> JournaledModelOverride {
		let override_ = JournaledModelOverride { role: role.into(), model, thinking };
		self.active_role = Some(override_.role.clone());
		self.override_ = Some(override_.clone());
		override_
	}

	/// Restores the latest live override from the journal without rewriting
	/// settings.
	pub fn restore_override(&mut self, override_: Option<JournaledModelOverride>) {
		self.active_role = override_.as_ref().map(|selection| selection.role.clone());
		self.override_ = override_;
	}

	/// Clears the effective override while retaining durable preferences.
	pub fn clear_override(&mut self) {
		self.override_ = None;
		self.active_role = None;
	}

	/// Returns the effective model for a role.
	pub fn effective(&self, role: &str) -> Option<&ModelKey> {
		self
			.override_
			.as_ref()
			.filter(|selection| selection.role.as_str() == role)
			.map(|selection| &selection.model)
			.or_else(|| self.durable_roles.get(role))
	}

	/// Returns the active journaled override.
	pub const fn session_override(&self) -> Option<&JournaledModelOverride> {
		self.override_.as_ref()
	}

	/// Replaces the already-enabled temporary cycle scope.
	///
	/// Enabled-model filtering happens before this call, so disabled models
	/// cannot re-enter through cycling.
	pub fn set_scoped_models(&mut self, scoped: Arc<[ScopedModel]>) {
		self.scoped = scoped;
	}

	/// Cycles the filtered scope in either direction and journals the result.
	pub fn cycle_scoped(
		&mut self,
		current: &ModelKey<str>,
		direction: CycleDirection,
	) -> Option<JournaledModelOverride> {
		if self.scoped.len() <= 1 {
			return None;
		}
		let current_index = self
			.scoped
			.iter()
			.position(|entry| &entry.model == current)
			.unwrap_or(0);
		let index = cycle_index(current_index, self.scoped.len(), direction);
		let next = self.scoped[index].clone();
		Some(self.switch_session("temporary", next.model, next.thinking))
	}

	/// Cycles configured role models in fixed role order and either direction.
	///
	/// Missing roles and roles filtered out of `enabled` are skipped before the
	/// current position is selected.
	pub fn cycle_roles(
		&mut self,
		role_order: &[Str],
		enabled: impl Fn(&ModelKey<str>) -> bool,
		direction: CycleDirection,
	) -> Option<RoleCycleSelection> {
		let available: Vec<_> = role_order
			.iter()
			.filter_map(|role| {
				self
					.durable_roles
					.get(role)
					.filter(|model| enabled(model))
					.map(|model| (role.clone(), model.clone()))
			})
			.collect();
		if available.len() <= 1 {
			return None;
		}
		let current_index = self
			.active_role
			.as_ref()
			.and_then(|role| {
				available
					.iter()
					.position(|(candidate, _)| candidate == role)
			})
			.or_else(|| {
				self.override_.as_ref().and_then(|active| {
					available
						.iter()
						.position(|(_, model)| model == &active.model)
				})
			})
			.unwrap_or(0);
		let index = cycle_index(current_index, available.len(), direction);
		let (role, model) = available[index].clone();
		self.switch_session(role.clone(), model.clone(), None);
		Some(RoleCycleSelection { role, model })
	}
}

fn cycle_index(current: usize, len: usize, direction: CycleDirection) -> usize {
	match direction {
		CycleDirection::Forward => (current + 1) % len,
		CycleDirection::Backward => (current + len - 1) % len,
	}
}

/// Stable catalog projection consumed by `omp.provider.models`.
#[derive(Clone, Debug, Serialize)]
pub struct ProviderModelCard {
	pub id:                Str,
	pub provider:          Str,
	pub model:             Str,
	pub name:              Str,
	pub family:            Option<Str>,
	pub facets:            Box<[Str]>,
	pub inputs:            Box<[Str]>,
	pub outputs:           Box<[Str]>,
	pub reasoning:         bool,
	pub efforts:           Box<[Str]>,
	pub context_window:    Option<u64>,
	pub max_output_tokens: Option<u64>,
	pub pricing:           Box<[ProviderPrice]>,
	pub availability:      Str,
	pub source:            u8,
	pub blocked_until_ms:  Option<u64>,
	pub deprecated:        bool,
	pub updated_at_ms:     Option<u64>,
	pub supports_tools:    Option<bool>,
	pub props:             Map<String, Value>,
}

/// One settled catalog price component.
#[derive(Clone, Debug, Serialize)]
pub struct ProviderPrice {
	pub unit:      Str,
	pub nanos_usd: u64,
}

/// Resumable catalog position.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderCatalogCursor {
	pub epoch:      Box<[u8]>,
	pub generation: u64,
}

/// One ordered model-catalog delta.
#[derive(Clone, Debug)]
pub enum ProviderModelEvent {
	Upsert { cursor: ProviderCatalogCursor, card: ProviderModelCard },
	Remove { cursor: ProviderCatalogCursor, id: Str },
	Reset { cursor: ProviderCatalogCursor },
}

/// Complete non-secret frozen provider declaration.
#[derive(Clone, Debug)]
pub struct ProviderDeclarationDocument {
	pub provider: Str,
	pub document: Value,
}

/// Closed provider request vocabulary admitted by Python.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderRequestKind {
	GenerateImage,
	Speak,
	Transcribe,
	Realtime,
}

/// Exact provider request passed to the application inference facade.
#[derive(Clone, Debug)]
pub struct ProviderControlRequest {
	pub provider:  Str,
	pub operation: ProviderRequestKind,
	pub payload:   Map<String, Value>,
}

/// Blob reference whose bytes remain in the application blob owner.
#[derive(Clone, Debug, Serialize)]
pub struct ProviderBlobRef {
	pub hash: Str,
	pub size: u64,
}

/// Typed provider request settlement.
#[derive(Clone, Debug)]
pub enum ProviderControlResult {
	Image {
		images:         Box<[ProviderBlobRef]>,
		cost_nanos_usd: u64,
	},
	Speech {
		audio:          ProviderBlobRef,
		format:         Str,
		cost_nanos_usd: u64,
	},
	Transcription {
		text:           Str,
		language:       Option<Str>,
		cost_nanos_usd: u64,
	},
	Realtime {
		id:            Str,
		endpoint:      Str,
		credential:    Str,
		expires_at_ms: u64,
		transport:     Str,
	},
}

/// Structured failure from the real provider owner.
#[derive(Clone, Debug, Error)]
pub enum ProviderControlError {
	#[error("provider operation is not authorized")]
	Authorization,
	#[error("provider resource is not found")]
	NotFound,
	#[error("provider is not authenticated")]
	Unauthenticated,
	#[error("provider catalog generation is stale")]
	StaleGeneration,
	#[error("{0}")]
	Request(Str),
}

/// Application-owned provider catalog, authentication, and inference seam.
#[async_trait]
pub trait ProviderControlBackend: Send + Sync + 'static {
	async fn models(
		&self,
		provider: Option<&str>,
	) -> Result<Vec<ProviderModelCard>, ProviderControlError>;
	async fn watch_models(
		&self,
		since: Option<ProviderCatalogCursor>,
	) -> Result<Vec<ProviderModelEvent>, ProviderControlError>;
	async fn is_authenticated(&self, provider: &str) -> Result<bool, ProviderControlError>;
	async fn replace(
		&self,
		identity: &ControlConnectionIdentity,
		declaration: ProviderDeclarationDocument,
	) -> Result<(), ProviderControlError>;
	async fn retract(
		&self,
		identity: &ControlConnectionIdentity,
		provider: &str,
	) -> Result<(), ProviderControlError>;
	async fn request(
		&self,
		identity: &ControlConnectionIdentity,
		request: ProviderControlRequest,
	) -> Result<ProviderControlResult, ProviderControlError>;
}

/// Factory for connection-scoped `omp.provider.*` ownership.
pub struct ProviderControlAuthorityFactory {
	backend: Arc<dyn ProviderControlBackend>,
}

impl ProviderControlAuthorityFactory {
	/// Binds the application provider owner.
	pub fn new(backend: Arc<dyn ProviderControlBackend>) -> Self {
		Self { backend }
	}
}

struct ProviderControlAuthority {
	identity: Arc<ControlConnectionIdentity>,
	backend:  Arc<dyn ProviderControlBackend>,
}

impl ControlAuthorityFactory for ProviderControlAuthorityFactory {
	fn bind(
		&self,
		identity: Arc<ControlConnectionIdentity>,
	) -> Result<Arc<dyn ControlAuthority>, ControlCompositionError> {
		Ok(Arc::new(ProviderControlAuthority { identity, backend: Arc::clone(&self.backend) }))
	}
}

impl ProviderControlAuthority {
	fn validate(&self, context: &ControlRequestContext) -> Result<(), ControlProtocolError> {
		if Arc::ptr_eq(&context.connection, &self.identity) {
			Ok(())
		} else {
			Err(ControlProtocolError::new(
				"StaleGeneration",
				"provider CONTROL authority belongs to a replaced connection",
			))
		}
	}

	fn error(error: ProviderControlError) -> ControlProtocolError {
		match error {
			ProviderControlError::Authorization => {
				ControlProtocolError::new("AuthorizationError", "provider operation is not authorized")
			},
			ProviderControlError::NotFound => {
				ControlProtocolError::new("TargetNotFound", "provider resource is not found")
			},
			ProviderControlError::Unauthenticated => {
				ControlProtocolError::new("AuthenticationError", "provider is not authenticated")
			},
			ProviderControlError::StaleGeneration => {
				ControlProtocolError::new("StaleGeneration", "provider catalog generation is stale")
					.retryable(true)
			},
			ProviderControlError::Request(message) => {
				ControlProtocolError::new("ProviderRequestError", message)
			},
		}
	}

	fn provider(arguments: &Map<String, Value>) -> Result<&str, ControlProtocolError> {
		arguments
			.get("provider")
			.and_then(Value::as_str)
			.filter(|provider| !provider.is_empty())
			.ok_or_else(|| ControlProtocolError::new("InvalidProvider", "provider is required"))
	}

	fn cursor(value: Option<&Value>) -> Result<Option<ProviderCatalogCursor>, ControlProtocolError> {
		let Some(value) = value else { return Ok(None) };
		if value.is_null() {
			return Ok(None);
		}
		let value = value.as_object().ok_or_else(|| {
			ControlProtocolError::new("InvalidCursor", "model cursor must be an object")
		})?;
		let epoch = value
			.get("epoch")
			.and_then(Value::as_object)
			.and_then(|epoch| epoch.get("$bytes"))
			.and_then(Value::as_str)
			.and_then(|epoch| omp_core::base64::decode(epoch).into_vec().ok())
			.filter(|epoch| !epoch.is_empty())
			.ok_or_else(|| {
				ControlProtocolError::new("InvalidCursor", "model cursor epoch is malformed")
			})?;
		let generation = value
			.get("generation")
			.and_then(Value::as_u64)
			.ok_or_else(|| {
				ControlProtocolError::new("InvalidCursor", "model cursor generation is missing")
			})?;
		Ok(Some(ProviderCatalogCursor { epoch: epoch.into_boxed_slice(), generation }))
	}

	fn cursor_json(cursor: &ProviderCatalogCursor) -> Value {
		json!({
			"epoch": {"$bytes": omp_core::base64::encode(&cursor.epoch).into_string()},
			"generation": cursor.generation,
		})
	}
}

#[async_trait]
impl ControlAuthority for ProviderControlAuthority {
	fn handles(&self, operation: &str) -> bool {
		matches!(
			operation,
			"omp.provider.models"
				| "omp.provider.watch_models"
				| "omp.provider.is_authenticated"
				| "omp.provider.replace"
				| "omp.provider.retract"
				| "omp.provider.request"
		)
	}

	fn authorize(
		&self,
		context: &ControlRequestContext,
		operation: &str,
		_arguments: &Map<String, Value>,
	) -> Result<(), ControlProtocolError> {
		self.validate(context)?;
		if context
			.invocation
			.as_ref()
			.is_some_and(|invocation| invocation.lifecycle != LifecyclePhase::Active)
		{
			return Err(ControlProtocolError::new(
				"PhaseError",
				"provider operations require an active extension lifecycle",
			));
		}
		if operation == "omp.provider.request"
			&& !context.invocation.as_ref().is_some_and(|invocation| {
				invocation
					.phase
					.allows_operation(InvocationPhase::EffectsAuthorized)
			}) {
			return Err(ControlProtocolError::new(
				"PhaseError",
				"provider requests require invocation-scoped effect authority",
			));
		}
		Ok(())
	}

	async fn request(
		&self,
		context: ControlRequestContext,
		operation: Str,
		arguments: Map<String, Value>,
	) -> Result<Value, ControlProtocolError> {
		self.validate(&context)?;
		match operation.as_str() {
			"omp.provider.models" => {
				let provider = arguments.get("provider").and_then(Value::as_str);
				let cards = self.backend.models(provider).await.map_err(Self::error)?;
				serde_json::to_value(cards)
					.map_err(|error| ControlProtocolError::new("CatalogCodecError", sf!("{error}")))
			},
			"omp.provider.watch_models" => {
				let since = Self::cursor(arguments.get("since"))?;
				let events = self
					.backend
					.watch_models(since)
					.await
					.map_err(Self::error)?;
				Ok(Value::Array(
					events
						.into_iter()
						.map(|event| match event {
							ProviderModelEvent::Upsert { cursor, card } => json!({
								"cursor": Self::cursor_json(&cursor),
								"upserted": card,
							}),
							ProviderModelEvent::Remove { cursor, id } => json!({
								"cursor": Self::cursor_json(&cursor),
								"removed_id": id.as_str(),
							}),
							ProviderModelEvent::Reset { cursor } => json!({
								"cursor": Self::cursor_json(&cursor),
								"reset": true,
							}),
						})
						.collect(),
				))
			},
			"omp.provider.is_authenticated" => Ok(Value::Bool(
				self
					.backend
					.is_authenticated(Self::provider(&arguments)?)
					.await
					.map_err(Self::error)?,
			)),
			"omp.provider.replace" => {
				let provider = Self::provider(&arguments)?;
				let document = arguments
					.get("spec")
					.filter(|spec| spec.is_object())
					.cloned()
					.ok_or_else(|| {
						ControlProtocolError::new(
							"InvalidProvider",
							"replacement provider declaration must be an object",
						)
					})?;
				if document.get("id").and_then(Value::as_str) != Some(provider) {
					return Err(ControlProtocolError::new(
						"InvalidProvider",
						"replacement declaration identity does not match provider",
					));
				}
				self
					.backend
					.replace(&self.identity, ProviderDeclarationDocument {
						provider: Str::from(provider),
						document,
					})
					.await
					.map_err(Self::error)?;
				Ok(Value::Null)
			},
			"omp.provider.retract" => {
				self
					.backend
					.retract(&self.identity, Self::provider(&arguments)?)
					.await
					.map_err(Self::error)?;
				Ok(Value::Null)
			},
			"omp.provider.request" => {
				let provider = Str::from(Self::provider(&arguments)?);
				let kind = match arguments.get("operation").and_then(Value::as_str) {
					Some("generate_image") => ProviderRequestKind::GenerateImage,
					Some("speak") => ProviderRequestKind::Speak,
					Some("transcribe") => ProviderRequestKind::Transcribe,
					Some("realtime") => ProviderRequestKind::Realtime,
					_ => {
						return Err(ControlProtocolError::new(
							"InvalidProviderOperation",
							"provider request operation is unsupported",
						));
					},
				};
				let payload = arguments
					.get("request")
					.and_then(Value::as_object)
					.cloned()
					.ok_or_else(|| {
						ControlProtocolError::new(
							"InvalidProviderRequest",
							"provider request payload must be an object",
						)
					})?;
				let result = self
					.backend
					.request(&self.identity, ProviderControlRequest {
						provider,
						operation: kind,
						payload,
					})
					.await
					.map_err(Self::error)?;
				Ok(match result {
					ProviderControlResult::Image { images, cost_nanos_usd } => {
						json!({"images": images, "cost_nanos_usd": cost_nanos_usd})
					},
					ProviderControlResult::Speech { audio, format, cost_nanos_usd } => json!({
						"audio": audio,
						"format": format.as_str(),
						"cost_nanos_usd": cost_nanos_usd,
					}),
					ProviderControlResult::Transcription { text, language, cost_nanos_usd } => json!({
						"text": text.as_str(),
						"language": language.as_deref(),
						"cost_nanos_usd": cost_nanos_usd,
					}),
					ProviderControlResult::Realtime {
						id,
						endpoint,
						credential,
						expires_at_ms,
						transport,
					} => json!({
						"id": id.as_str(),
						"endpoint": {"id": endpoint.as_str()},
						"credential": {"id": credential.as_str()},
						"expires_at_ms": expires_at_ms,
						"transport": transport.as_str(),
					}),
				})
			},
			_ => Err(ControlProtocolError::new(
				"UnknownOperation",
				"provider authority does not own this operation",
			)),
		}
	}

	async fn effect(
		&self,
		context: ControlRequestContext,
		_effect: ControlEffect,
	) -> Result<(), ControlProtocolError> {
		self.validate(&context)?;
		Err(ControlProtocolError::new(
			"UnsupportedEffect",
			"provider requests are correlated CONTROL operations",
		))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn model(value: &str) -> ModelKey {
		ModelKey::from(value)
	}

	#[test]
	fn durable_preference_and_override_are_independent_across_restore() {
		let mut controls = ModelControls::from_durable(BTreeMap::from([(
			"default".into(),
			model("provider/preferred"),
		)]));
		let journaled = controls.switch_session("default", model("provider/temporary"), None);
		assert_eq!(controls.effective("default"), Some(&model("provider/temporary")));

		let mut resumed = ModelControls::from_durable(BTreeMap::from([(
			"default".into(),
			model("provider/preferred"),
		)]));
		resumed.restore_override(Some(journaled));
		assert_eq!(resumed.effective("default"), Some(&model("provider/temporary")));
		resumed.clear_override();
		assert_eq!(resumed.effective("default"), Some(&model("provider/preferred")));
	}

	#[test]
	fn scoped_and_role_cycles_wrap_in_both_directions() {
		let mut controls = ModelControls::default();
		controls.set_scoped_models(Arc::from([
			ScopedModel { model: model("p/a"), thinking: None },
			ScopedModel { model: model("p/b"), thinking: Some(ThinkingEffort::High) },
		]));
		let backward = controls
			.cycle_scoped(ModelKey::from_ref("p/a"), CycleDirection::Backward)
			.unwrap();
		assert_eq!(backward.model, model("p/b"));
		assert_eq!(backward.thinking, Some(ThinkingEffort::High));

		controls.set_durable("slow", model("p/slow"));
		controls.set_durable("default", model("p/default"));
		controls.set_durable("smol", model("p/smol"));
		controls.active_role = Some("default".into());
		let roles: Vec<Str> = ["slow", "default", "smol"]
			.into_iter()
			.map(Str::new)
			.collect();
		let previous = controls
			.cycle_roles(
				&roles,
				|model| model != ModelKey::from_ref("p/slow"),
				CycleDirection::Backward,
			)
			.unwrap();
		assert_eq!(previous.role, "smol");
	}
}
