//! Native `models.toml` decoding for configured catalog overlays.

use std::{collections::BTreeMap, fs, path::Path};

use omp_core::Str;
use omp_llm_catalog::{
	CatalogOverlay, CatalogOverlayBuilder, EvidenceConfidence, ModelKey, ModelLimits, ModelOverlay,
	ModelPatch, OverlaySource, OverlayStore, ProvenanceKind, ProvenanceSource,
};
use serde::Deserialize;
/// Native model configuration. The field names deliberately mirror the pi
/// `models.yml` contract while TOML is OMP's native serialization.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ModelsConfig {
	/// Provider definitions keyed by stable provider id.
	#[serde(default)]
	pub providers: BTreeMap<Str, ProviderConfig>,
}

/// Provider-level configuration facts.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
	/// Route base URL override.
	pub base_url:             Option<Str>,
	/// Static request headers.
	#[serde(default)]
	pub headers:              BTreeMap<Str, Str>,
	/// Authentication mode.
	pub auth:                 Option<Str>,
	/// Provider model-discovery configuration.
	pub discovery:            Option<toml::Value>,
	/// Wire compatibility configuration.
	pub compat:               Option<toml::Value>,
	/// Whether strict tool schemas are disabled.
	pub disable_strict_tools: Option<bool>,
	/// Per-model replacement facts keyed by model id.
	#[serde(default)]
	pub model_overrides:      BTreeMap<Str, ModelConfig>,
	/// Provider model definitions keyed by model id.
	#[serde(default)]
	pub models:               BTreeMap<Str, ModelConfig>,
}

/// Model-level configuration facts mapped one-for-one onto the catalog model
/// fields. Typed overlay lowering is intentionally done by the catalog owner.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelConfig {
	/// Explicit normalized model id.
	pub id: Option<Str>,
	/// Picker display name.
	pub name: Option<Str>,
	/// Wire API selector.
	pub api: Option<Str>,
	/// Total context-window limit.
	pub context_window: Option<u64>,
	/// Maximum generated-token limit.
	pub max_tokens: Option<u64>,
	/// Tool-use support flag.
	pub supports_tools: Option<bool>,
	/// Streaming support flag.
	pub supports_streaming: Option<bool>,
	/// Reasoning-policy declaration.
	pub reasoning: Option<toml::Value>,
	/// Accepted input modalities.
	pub input: Option<toml::Value>,
	/// Price schedule.
	pub cost: Option<toml::Value>,
	/// Model-specific compatibility configuration.
	pub compat: Option<toml::Value>,
	/// Remote compaction contract.
	pub remote_compaction: Option<toml::Value>,
	/// Premium quota multiplier.
	pub premium_multiplier: Option<Str>,
	/// Compaction model selector.
	pub compaction_model: Option<Str>,
	/// Context-promotion target selector.
	pub context_promotion_target: Option<Str>,
}

/// Decodes a native configured-model file.
pub fn load_models_config(path: &Path) -> Result<ModelsConfig, ModelsConfigError> {
	let source = fs::read_to_string(path)?;
	Ok(toml::from_str(&source)?)
}

/// Lowers configured model limits and promotion targets into the catalog's
/// immutable user-config overlay. Route/provider wire facts remain catalog
/// declarations and are never synthesized from untyped configuration.
pub fn lower_user_overlay(config: &ModelsConfig) -> CatalogOverlay {
	let source = ProvenanceSource {
		kind:           ProvenanceKind::Configured,
		origin:         "models.toml".into(),
		revision:       None,
		confidence:     EvidenceConfidence::Declared,
		observed_at_ms: None,
	};
	let mut builder = CatalogOverlayBuilder::new(source);
	for (provider, definition) in &config.providers {
		for (name, model) in definition
			.models
			.iter()
			.chain(definition.model_overrides.iter())
		{
			let key = model.id.as_deref().unwrap_or(name.as_str());
			let limits =
				(model.context_window.is_some() || model.max_tokens.is_some()).then_some(ModelLimits {
					context_window:        model.context_window,
					maximum_input_tokens:  None,
					maximum_output_tokens: model.max_tokens,
					maximum_batch:         None,
				});
			builder = builder.with_model(ModelOverlay {
				selector: omp_llm_catalog::ExactSelector::new(provider.clone(), ModelKey::from(key)),
				added:    None,
				patch:    ModelPatch {
					display_name: model.name.clone(),
					limits,
					context_promotion_target: model
						.context_promotion_target
						.clone()
						.map(|value| Some(ModelKey::from(value))),
					..ModelPatch::default()
				},
			});
		}
	}
	builder.build()
}

/// Publishes the complete native user-config generation atomically.
pub fn publish_user_overlay(store: &OverlayStore, config: &ModelsConfig) {
	store.replace(OverlaySource::UserConfig, lower_user_overlay(config));
}
/// Native model-config decoding failures.
#[derive(Debug, thiserror::Error)]
pub enum ModelsConfigError {
	/// Reading the configured source failed.
	#[error(transparent)]
	Io(#[from] std::io::Error),
	/// The TOML source was malformed.
	#[error(transparent)]
	Toml(#[from] toml::de::Error),
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn representative_models_toml_decodes_and_publishes() {
		let value: ModelsConfig = toml::from_str(
			"[providers.demo]\nbaseUrl='https://example.test/v1'\nauth='apiKey'\ndisableStrictTools=true\n[providers.demo.models.fast]\ncontextWindow=128000\nmaxTokens=8192\npremiumMultiplier='0.25'\ncontextPromotionTarget='large'\n",
		).expect("decode");
		let provider = &value.providers["demo"];
		assert_eq!(provider.base_url.as_deref(), Some("https://example.test/v1"));
		let model = &provider.models["fast"];
		assert_eq!(model.context_window, Some(128000));
		assert_eq!(model.max_tokens, Some(8192));
		let store = OverlayStore::default();
		publish_user_overlay(&store, &value);
		assert_eq!(store.load().sources(), &[OverlaySource::UserConfig]);
	}
}
