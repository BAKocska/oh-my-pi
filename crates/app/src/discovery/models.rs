//! Native `models.toml` decoding for configured catalog overlays.

use std::{collections::BTreeMap, fs, path::Path};

use omp_core::Str;
use omp_llm_catalog::{
	Availability, CatalogOverlay, CatalogOverlayBuilder, EvidenceConfidence, ModalityBits, ModelKey,
	ModelLimits, ModelOverlay, ModelPatch, OverlaySource, OverlayStore, PremiumMultiplier, Pricing,
	ProvenanceKind, ProvenanceSource, RouteOverlay, RoutePatch, ThinkingPolicy,
};
use serde::{Deserialize, Serialize};
/// Native model configuration. The field names deliberately mirror the pi
/// `models.yml` contract while TOML is OMP's native serialization.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ModelsConfig {
	/// Provider definitions keyed by stable provider id.
	#[serde(default)]
	pub providers: BTreeMap<Str, ProviderConfig>,
}

/// Provider-level configuration facts.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
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

/// Declarative configured header value source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HeaderValueSource {
	/// Safe static public value.
	Public(Str),
	/// Environment-owned secret name.
	Environment(Str),
	/// Environment-executed secret command.
	Command(Str),
}

impl ProviderConfig {
	/// Classifies configured headers without resolving or copying secret
	/// material into the catalog.
	#[must_use]
	pub fn header_sources(&self) -> Vec<(Str, HeaderValueSource)> {
		self
			.headers
			.iter()
			.map(|(name, value)| {
				let source = if let Some(command) = value.strip_prefix("!") {
					HeaderValueSource::Command(Str::new(command.trim()))
				} else if let Some(environment) = value.strip_prefix("$") {
					HeaderValueSource::Environment(Str::new(environment))
				} else {
					HeaderValueSource::Public(value.clone())
				};
				(name.clone(), source)
			})
			.collect()
	}
}

/// Model-level configuration facts mapped one-for-one onto the catalog model
/// fields. Typed overlay lowering is intentionally done by the catalog owner.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
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
	/// Preferred edit-tool contract revision.
	pub edit_revision: Option<Str>,
	/// Context-promotion target selector.
	pub context_promotion_target: Option<Str>,
}

/// Decodes a native configured-model file.
pub fn load_models_config(path: &Path) -> Result<ModelsConfig, ModelsConfigError> {
	let source = fs::read_to_string(path)?;
	Ok(toml::from_str(&source)?)
}

/// Source label for a native model configuration or one-time legacy import.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelsConfigSource {
	/// Canonical native TOML.
	NativeToml(std::path::PathBuf),
	/// Imported legacy JSON.
	LegacyJson(std::path::PathBuf),
	/// Imported legacy YAML.
	LegacyYaml(std::path::PathBuf),
}

/// Typed model config paired with its explicit provenance label.
#[derive(Clone, Debug)]
pub struct LoadedModelsConfig {
	/// Typed configuration.
	pub config: ModelsConfig,
	/// Decoder/import source.
	pub source: ModelsConfigSource,
}

/// Loads canonical TOML, or performs a one-time legacy JSON/YAML import when
/// no canonical file exists. Legacy formats are never live fallback decoders.
pub fn load_or_import_legacy(
	directory: &Path,
) -> Result<Option<LoadedModelsConfig>, ModelsConfigError> {
	let native = directory.join("models.toml");
	if native.exists() {
		return Ok(Some(LoadedModelsConfig {
			config: load_models_config(&native)?,
			source: ModelsConfigSource::NativeToml(native),
		}));
	}
	let marker = directory.join(".models-migration-v1");
	if marker.exists() {
		return Ok(None);
	}
	let candidates = [("models.json", false), ("models.yml", true), ("models.yaml", true)];
	let Some((path, yaml)) = candidates
		.into_iter()
		.map(|(name, yaml)| (directory.join(name), yaml))
		.find(|(path, _)| path.exists())
	else {
		crate::settings::io::atomic_replace(&marker, "revision = 1\n")?;
		return Ok(None);
	};
	let text = fs::read_to_string(&path)?;
	let config = if yaml {
		serde_yaml::from_str(&text)?
	} else {
		omp_slopjson::from_str(&text)?
	};
	crate::settings::io::atomic_replace(&native, &toml::to_string_pretty(&config)?)?;
	let backup = path.with_file_name(format!(
		"{}.pre-omp-migration.bak",
		path
			.file_name()
			.and_then(|name| name.to_str())
			.unwrap_or("models")
	));
	fs::copy(&path, &backup).map_err(|source| ModelsConfigError::Backup {
		path: path.clone(),
		backup,
		source,
	})?;
	crate::settings::io::atomic_replace(
		&marker,
		if yaml {
			"revision = 1\nsource = \"legacy-yaml\"\n"
		} else {
			"revision = 1\nsource = \"legacy-json\"\n"
		},
	)?;
	Ok(Some(LoadedModelsConfig {
		config,
		source: if yaml {
			ModelsConfigSource::LegacyYaml(path)
		} else {
			ModelsConfigSource::LegacyJson(path)
		},
	}))
}

/// Validates and lowers configured model facts into a secret-free immutable
/// overlay.
///
/// Omitted fields inherit bundled facts. Header and credential values remain
/// declarative in the configuration authority and are never copied into model
/// records.
pub fn lower_user_overlay(config: &ModelsConfig) -> Result<CatalogOverlay, ModelsConfigError> {
	let source = ProvenanceSource {
		kind:           ProvenanceKind::Configured,
		origin:         "models.toml".into(),
		revision:       None,
		confidence:     EvidenceConfidence::Declared,
		observed_at_ms: None,
	};
	let mut builder = CatalogOverlayBuilder::new(source);
	let catalog = omp_llm_catalog::Catalog::embedded();
	for (provider, definition) in &config.providers {
		if let Some(base_provider) =
			catalog.provider(omp_llm_catalog::ProviderId::from_ref(provider.as_str()))
		{
			for route_id in &base_provider.routes {
				let Some(route) = catalog.route(route_id) else {
					continue;
				};
				let endpoint =
					definition
						.base_url
						.as_ref()
						.map(|base_url| omp_llm_catalog::EndpointSpec {
							base_url: base_url.clone(),
							region:   route.endpoint.region.clone(),
						});
				let discovery = definition.discovery.as_ref().and_then(|value| {
					value
						.get("id")
						.and_then(toml::Value::as_str)
						.map(omp_llm_catalog::DiscoverySpecId::from)
				});
				builder = builder.with_route(RouteOverlay {
					route: route_id.clone(),
					added: None,
					patch: RoutePatch {
						endpoint,
						auth: definition
							.auth
							.as_deref()
							.map(omp_llm_catalog::AuthSpecId::from),
						discovery: definition.discovery.as_ref().map(|_| discovery),
						disable_strict_tools: definition.disable_strict_tools,
						..RoutePatch::default()
					},
				});
			}
		}
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
			let mut capabilities = None;
			if model.supports_tools.is_some()
				|| model.supports_streaming.is_some()
				|| model.input.is_some()
			{
				let inherited = omp_llm_catalog::Catalog::embedded()
					.models()
					.iter()
					.find(|candidate| {
						candidate.key.as_str() == key
							|| candidate
								.key
								.as_str()
								.split_once('/')
								.is_some_and(|(_, id)| id == key)
					})
					.map(|candidate| candidate.capabilities.clone())
					.unwrap_or_else(omp_llm_catalog::unknown_capabilities);
				let mut updated = inherited;
				if let Some(chat) = updated.chat.as_mut() {
					if let Some(supports) = model.supports_tools {
						chat.tools = if supports {
							Availability::Native(omp_llm_catalog::ToolCapabilities {
								features:      omp_llm_catalog::ToolFeatureBits::empty(),
								maximum_tools: None,
							})
						} else {
							Availability::Unsupported
						};
					}
					if let Some(input) = &model.input {
						chat.input_modalities =
							Availability::Native(parse_modalities(input, provider, key)?);
					}
				}
				capabilities = Some(updated);
			}
			let thinking = match &model.reasoning {
				None => None,
				Some(toml::Value::Boolean(false)) => Some(None),
				Some(toml::Value::Boolean(true)) => None,
				Some(value) => {
					let policy = value.clone().try_into::<ThinkingPolicy>().map_err(|_| {
						ModelsConfigError::InvalidFact {
							provider: provider.clone(),
							model:    Str::new(key),
							field:    "reasoning",
						}
					})?;
					policy
						.validate()
						.map_err(|_| ModelsConfigError::InvalidFact {
							provider: provider.clone(),
							model:    Str::new(key),
							field:    "reasoning",
						})?;
					Some(Some(policy.content_id()))
				},
			};
			let pricing = model
				.cost
				.as_ref()
				.map(|value| {
					value
						.clone()
						.try_into::<Pricing>()
						.map_err(|_| ModelsConfigError::InvalidFact {
							provider: provider.clone(),
							model:    Str::new(key),
							field:    "cost",
						})
				})
				.transpose()?;
			if let Some(pricing) = &pricing {
				pricing
					.validate()
					.map_err(|_| ModelsConfigError::InvalidFact {
						provider: provider.clone(),
						model:    Str::new(key),
						field:    "cost",
					})?;
			}
			let premium_multiplier_millionths = model
				.premium_multiplier
				.as_deref()
				.map(|value| {
					parse_multiplier(value).ok_or_else(|| ModelsConfigError::InvalidFact {
						provider: provider.clone(),
						model:    Str::new(key),
						field:    "premiumMultiplier",
					})
				})
				.transpose()?
				.map(|value| Some(PremiumMultiplier::from_millionths(value)));
			builder = builder.with_model(ModelOverlay {
				selector: omp_llm_catalog::ExactSelector::new(provider.clone(), ModelKey::from(key)),
				added:    None,
				patch:    ModelPatch {
					display_name: model.name.clone(),
					capabilities,
					limits,
					thinking,
					pricing,
					premium_multiplier_millionths,
					compaction_model: model
						.compaction_model
						.clone()
						.map(|value| Some(ModelKey::from(value))),
					edit_revision: model.edit_revision.clone().map(Some),
					context_promotion_target: model
						.context_promotion_target
						.clone()
						.map(|value| Some(ModelKey::from(value))),
					..ModelPatch::default()
				},
			});
		}
	}
	Ok(builder.build())
}

fn parse_modalities(
	value: &toml::Value,
	provider: &str,
	model: &str,
) -> Result<ModalityBits, ModelsConfigError> {
	let values = value
		.as_array()
		.ok_or_else(|| ModelsConfigError::InvalidFact {
			provider: Str::new(provider),
			model:    Str::new(model),
			field:    "input",
		})?;
	let mut modalities = ModalityBits::empty();
	for value in values {
		let modality = value
			.as_str()
			.ok_or_else(|| ModelsConfigError::InvalidFact {
				provider: Str::new(provider),
				model:    Str::new(model),
				field:    "input",
			})?;
		match modality {
			"text" => modalities.insert(ModalityBits::TEXT),
			"image" => modalities.insert(ModalityBits::IMAGE),
			"audio" => modalities.insert(ModalityBits::AUDIO),
			"video" => modalities.insert(ModalityBits::VIDEO),
			"document" => modalities.insert(ModalityBits::DOCUMENT),
			_ => {
				return Err(ModelsConfigError::InvalidFact {
					provider: Str::new(provider),
					model:    Str::new(model),
					field:    "input",
				});
			},
		}
	}
	Ok(modalities)
}

fn parse_multiplier(value: &str) -> Option<u64> {
	let value = value.trim();
	let (whole, fractional) = value.split_once('.').unwrap_or((value, ""));
	if whole.is_empty()
		|| fractional.len() > 6
		|| !whole.bytes().all(|byte| byte.is_ascii_digit())
		|| !fractional.bytes().all(|byte| byte.is_ascii_digit())
	{
		return None;
	}
	let whole = whole.parse::<u64>().ok()?;
	let fractional = if fractional.is_empty() {
		0
	} else {
		fractional
			.parse::<u64>()
			.ok()?
			.checked_mul(10_u64.pow(u32::try_from(6_usize.saturating_sub(fractional.len())).ok()?))?
	};
	whole
		.checked_mul(PremiumMultiplier::SCALE)?
		.checked_add(fractional)
}

/// Publishes the complete native user-config generation atomically.
pub fn publish_user_overlay(
	store: &OverlayStore,
	config: &ModelsConfig,
) -> Result<(), ModelsConfigError> {
	store.replace(OverlaySource::UserConfig, lower_user_overlay(config)?);
	Ok(())
}
/// Native model-config decoding failures.
#[derive(Debug, thiserror::Error)]
pub enum ModelsConfigError {
	/// A configured provider/model fact is malformed or internally inconsistent.
	#[error("invalid `{field}` for configured model {provider}/{model}")]
	InvalidFact {
		/// Provider containing the invalid fact.
		provider: Str,
		/// Model containing the invalid fact.
		model:    Str,
		/// Stable field name.
		field:    &'static str,
	},
	/// Reading the configured source failed.
	#[error(transparent)]
	Io(#[from] std::io::Error),
	/// The TOML source was malformed.
	#[error(transparent)]
	Toml(#[from] toml::de::Error),
	/// A legacy YAML source was malformed.
	#[error(transparent)]
	Yaml(#[from] serde_yaml::Error),
	/// A legacy JSON/JSONC source was malformed.
	#[error(transparent)]
	Json(#[from] omp_slopjson::ParseError),
	/// Native TOML encoding failed.
	#[error(transparent)]
	Encode(#[from] toml::ser::Error),
	/// Atomic persistence failed.
	#[error(transparent)]
	Persist(#[from] crate::settings::io::SettingsIoError),
	/// A legacy source backup failed.
	#[error("failed to back up model config {path} to {backup}")]
	Backup {
		/// Legacy source path.
		path:   std::path::PathBuf,
		/// Backup path.
		backup: std::path::PathBuf,
		/// Filesystem failure.
		#[source]
		source: std::io::Error,
	},
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn legacy_yaml_is_imported_once_then_native_toml_is_live() {
		let directory = tempfile::tempdir().expect("directory");
		let legacy = directory.path().join("models.yml");
		fs::write(
			&legacy,
			"providers:\n  demo:\n    models:\n      fast:\n        contextWindow: 4096\n",
		)
		.expect("legacy");
		let imported = load_or_import_legacy(directory.path())
			.expect("import")
			.expect("config");
		assert!(matches!(imported.source, ModelsConfigSource::LegacyYaml(_)));
		assert_eq!(imported.config.providers["demo"].models["fast"].context_window, Some(4096));
		assert!(
			directory
				.path()
				.join("models.yml.pre-omp-migration.bak")
				.exists()
		);
		let native = load_or_import_legacy(directory.path())
			.expect("native")
			.expect("config");
		assert!(matches!(native.source, ModelsConfigSource::NativeToml(_)));
	}

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
		publish_user_overlay(&store, &value).expect("publish");
		assert_eq!(store.load().sources(), &[OverlaySource::UserConfig]);
	}
}
