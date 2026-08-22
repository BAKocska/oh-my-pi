//! Runtime-owned model, thinking, provider, and wire settings projections.

#![allow(missing_docs, reason = "strum IntoStaticStr emits undocumented inherent methods")]

use std::{collections::BTreeMap, time::Duration};

use omp_core::Str;
use omp_settings::{
	DomainRegistration, FieldDescriptor, SettingKind, SettingScope, SettingsDomain, ValidationError,
};
use serde::{Deserialize, Serialize};
use strum::{Display, EnumString, IntoStaticStr};

use crate::{
	capability::{ProviderFamily, ServiceTier, TierAudience},
	id::WireModelId,
	thinking::ThinkingEffort,
};

const PERSISTED: &[SettingScope] = &[SettingScope::Global, SettingScope::Project];

/// Token budgets associated with portable reasoning effort levels.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct ThinkingBudgets {
	/// Minimal-effort token ceiling.
	pub minimal: u64,
	/// Low-effort token ceiling.
	pub low:     u64,
	/// Medium-effort token ceiling.
	pub medium:  u64,
	/// High-effort token ceiling.
	pub high:    u64,
	/// Extra-high-effort token ceiling.
	pub xhigh:   u64,
	/// Maximum-effort token ceiling.
	pub max:     u64,
}

impl Default for ThinkingBudgets {
	fn default() -> Self {
		Self {
			minimal: 1_024,
			low:     2_048,
			medium:  8_192,
			high:    16_384,
			xhigh:   32_768,
			max:     32_768,
		}
	}
}

impl ThinkingBudgets {
	/// Returns the configured budget for a concrete effort.
	#[must_use]
	pub const fn for_effort(self, effort: ThinkingEffort) -> Option<u64> {
		match effort {
			ThinkingEffort::Off => None,
			ThinkingEffort::Minimal => Some(self.minimal),
			ThinkingEffort::Low => Some(self.low),
			ThinkingEffort::Medium => Some(self.medium),
			ThinkingEffort::High => Some(self.high),
			ThinkingEffort::XHigh => Some(self.xhigh),
			ThinkingEffort::Max => Some(self.max),
		}
	}
}

/// Portable service-tier selection persisted without provider credentials.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TierSetting {
	/// Omit a service tier.
	#[default]
	None,
	/// Inherit the root session tier.
	Inherit,
	/// Provider standard tier.
	Standard,
	/// Provider flex tier.
	Flex,
	/// Provider priority tier.
	Priority,
}

impl TierSetting {
	fn resolve(&self, family: ProviderFamily, parent: Option<&ServiceTier>) -> Option<ServiceTier> {
		match self {
			Self::None => None,
			Self::Inherit => parent.cloned(),
			Self::Standard => Some(ServiceTier { name: Str::new_static("standard"), priority: 0 }),
			Self::Flex if family == ProviderFamily::OpenAi => {
				Some(ServiceTier { name: Str::new_static("flex"), priority: -10 })
			},
			Self::Flex => None,
			Self::Priority => {
				Some(ServiceTier { name: Str::new_static("priority"), priority: 10 })
			},
		}
	}
}

/// Default OpenRouter routing suffix.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Deserialize,
	Display,
	EnumString,
	Eq,
	IntoStaticStr,
	PartialEq,
	Serialize,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive, const_into_str)]
pub enum OpenRouterVariant {
	/// Do not append a routing suffix.
	#[default]
	Default,
	/// Prefer throughput and latency.
	Nitro,
	/// Prefer lowest price.
	Floor,
	/// Enable OpenRouter online routing.
	Online,
	/// Use OpenRouter's curated exacto route.
	Exacto,
}

/// Tri-state wire feature selection.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Deserialize,
	Display,
	EnumString,
	Eq,
	IntoStaticStr,
	PartialEq,
	Serialize,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive, const_into_str)]
pub enum WireToggle {
	/// Follow catalog policy.
	#[default]
	Auto,
	/// Disable the feature.
	Off,
	/// Require the feature.
	On,
}

/// Kimi provider API format.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Deserialize,
	Display,
	EnumString,
	Eq,
	IntoStaticStr,
	PartialEq,
	Serialize,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive, const_into_str)]
pub enum KimiApiFormat {
	/// Follow live catalog metadata.
	#[default]
	Auto,
	/// Require an OpenAI-compatible route.
	OpenAi,
	/// Require an Anthropic-compatible route.
	Anthropic,
}

/// Prompt-cache retention selection.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Deserialize,
	Display,
	EnumString,
	Eq,
	IntoStaticStr,
	PartialEq,
	Serialize,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive, const_into_str)]
pub enum CacheRetentionSetting {
	/// Preserve request intent and catalog defaults.
	#[default]
	Auto,
	/// Disable prompt caching.
	None,
	/// Request short retention.
	Short,
	/// Request long retention.
	Long,
}

/// Catalog-owned model and provider policy projection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct ModelSettings {
	/// Default thinking effort used when a caller leaves effort unset.
	pub default_thinking:         ThinkingEffort,
	/// Universal configured reasoning ceiling.
	pub thinking_ceiling:         ThinkingEffort,
	/// Per-effort reasoning token budgets.
	pub thinking_budgets:         ThinkingBudgets,
	/// Provider ids in preferred routing order.
	pub provider_order:           ArcStrList,
	/// OpenAI-family service tier.
	pub tier_openai:              TierSetting,
	/// Anthropic-family service tier.
	pub tier_anthropic:           TierSetting,
	/// Google-family service tier.
	pub tier_google:              TierSetting,
	/// Fireworks serving tier.
	pub tier_fireworks:           TierSetting,
	/// Spawned-agent tier override.
	pub tier_subagent:            TierSetting,
	/// Advisor tier override.
	pub tier_advisor:             TierSetting,
	/// Prompt-cache retention policy.
	pub cache_retention:          CacheRetentionSetting,
	/// OpenAI Codex websocket preference.
	pub openai_websockets:        WireToggle,
	/// Default OpenRouter routing suffix.
	pub openrouter_variant:       OpenRouterVariant,
	/// Kimi wire format preference.
	pub kimi_api_format:          KimiApiFormat,
	/// Model selector for tiny/title work.
	pub tiny_selector:            Str,
	/// Model selector for memory inference.
	pub memory_selector:          Str,
	/// Model selector for automatic-thinking classification.
	pub auto_thinking_selector:   Str,
	/// Model selector for unexpected-stop classification.
	pub unexpected_stop_selector: Str,
}

/// Clone-cheap provider priority sequence.
pub type ArcStrList = std::sync::Arc<[Str]>;

impl Default for ModelSettings {
	fn default() -> Self {
		Self {
			default_thinking:         ThinkingEffort::Medium,
			thinking_ceiling:         ThinkingEffort::Max,
			thinking_budgets:         ThinkingBudgets::default(),
			provider_order:           std::sync::Arc::from([]),
			tier_openai:              TierSetting::None,
			tier_anthropic:           TierSetting::None,
			tier_google:              TierSetting::None,
			tier_fireworks:           TierSetting::None,
			tier_subagent:            TierSetting::Inherit,
			tier_advisor:             TierSetting::None,
			cache_retention:          CacheRetentionSetting::Auto,
			openai_websockets:        WireToggle::Auto,
			openrouter_variant:       OpenRouterVariant::Default,
			kimi_api_format:          KimiApiFormat::Auto,
			tiny_selector:            Str::new_static("@tiny"),
			memory_selector:          Str::new_static("@tiny"),
			auto_thinking_selector:   Str::new_static("@tiny"),
			unexpected_stop_selector: Str::new_static("@tiny"),
		}
	}
}

impl ModelSettings {
	/// Applies configured effort budgets and the configured default to one model
	/// policy.
	pub fn apply_thinking_policy(&self, policy: &mut crate::thinking::ThinkingPolicy) {
		policy.default_level = Some(self.default_thinking)
			.filter(|effort| *effort != ThinkingEffort::Off && policy.efforts.contains(effort));
		for effort in policy.efforts.iter().copied() {
			if let Some(budget) = self.thinking_budgets.for_effort(effort) {
				policy.effort_budgets.insert(effort, budget);
			}
		}
	}

	/// Returns a stable provider preference rank; unlisted providers follow
	/// listed ones.
	#[must_use]
	pub fn provider_rank(&self, provider: &str) -> usize {
		self
			.provider_order
			.iter()
			.position(|item| item == provider)
			.unwrap_or(usize::MAX)
	}

	/// Resolves route family and provider-specific tier policy.
	#[must_use]
	pub fn service_tier_for_route(
		&self,
		provider: &str,
		model: Option<&str>,
		audience: TierAudience,
		parent: Option<&ServiceTier>,
	) -> Option<ServiceTier> {
		if provider.contains("fireworks") {
			return self.tier_fireworks.resolve(ProviderFamily::Other, parent);
		}
		self.service_tier(provider_family(provider, model), audience, parent)
	}

	/// Resolves a family/audience service tier into the concrete wire value.
	#[must_use]
	pub fn service_tier(
		&self,
		family: ProviderFamily,
		audience: TierAudience,
		parent: Option<&ServiceTier>,
	) -> Option<ServiceTier> {
		let audience_setting = match audience {
			TierAudience::Session => None,
			TierAudience::Subagent => Some(&self.tier_subagent),
			TierAudience::Advisor => Some(&self.tier_advisor),
		};
		if let Some(setting) = audience_setting
			&& !matches!(setting, TierSetting::Inherit)
		{
			return setting.resolve(family, parent);
		}
		let family_setting = match family {
			ProviderFamily::OpenAi => &self.tier_openai,
			ProviderFamily::Anthropic => &self.tier_anthropic,
			ProviderFamily::Google => &self.tier_google,
			ProviderFamily::Other => return None,
		};
		family_setting.resolve(family, parent)
	}

	/// Reports whether a concrete route satisfies configured wire preferences.
	#[must_use]
	pub fn wire_route_allowed(
		&self,
		provider: &str,
		codec: &str,
		transport: crate::provider::TransportKind,
	) -> bool {
		let openai_route = provider.contains("openai") || provider.contains("codex");
		let websocket_allowed = !openai_route
			|| match self.openai_websockets {
				WireToggle::Auto => true,
				WireToggle::Off => transport != crate::provider::TransportKind::Websocket,
				WireToggle::On => transport == crate::provider::TransportKind::Websocket,
			};
		let kimi_route = provider.contains("kimi") || provider.contains("moonshot");
		let kimi_allowed = !kimi_route
			|| match self.kimi_api_format {
				KimiApiFormat::Auto => true,
				KimiApiFormat::OpenAi => codec.starts_with("openai-"),
				KimiApiFormat::Anthropic => codec == "anthropic",
			};
		websocket_allowed && kimi_allowed
	}

	/// Applies the configured OpenRouter suffix only when the model has no
	/// explicit variant.
	#[must_use]
	pub fn openrouter_wire_model(&self, provider: &str, model: &WireModelId<str>) -> WireModelId {
		if provider != "openrouter"
			|| self.openrouter_variant == OpenRouterVariant::Default
			|| model
				.rsplit('/')
				.next()
				.is_some_and(|tail| tail.contains(':'))
		{
			return model.to_owned();
		}
		Str::from(format!("{}:{}", model, <&'static str>::from(self.openrouter_variant))).into()
	}

	/// Selects the configured model for one harness-owned auxiliary purpose.
	#[must_use]
	pub const fn special_selector(&self, purpose: SpecialModelPurpose) -> &Str {
		match purpose {
			SpecialModelPurpose::Tiny => &self.tiny_selector,
			SpecialModelPurpose::Memory => &self.memory_selector,
			SpecialModelPurpose::AutoThinking => &self.auto_thinking_selector,
			SpecialModelPurpose::UnexpectedStop => &self.unexpected_stop_selector,
		}
	}

	/// Returns a bounded first-event timeout derived from provider settings.
	#[must_use]
	pub const fn plan_ttl(&self) -> Duration {
		Duration::from_secs(30)
	}
}

/// Harness-owned auxiliary model use.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpecialModelPurpose {
	/// Session titles and cheap transforms.
	Tiny,
	/// Memory extraction and consolidation.
	Memory,
	/// Automatic thinking classifier.
	AutoThinking,
	/// Unexpected-stop classifier.
	UnexpectedStop,
}

impl SettingsDomain for ModelSettings {
	const DOMAIN: &'static str = "model";
	const FIELDS: &'static [FieldDescriptor] = &[
		field(
			"model.default_thinking",
			"Default Thinking",
			SettingKind::Enum(&["off", "minimal", "low", "medium", "high", "xhigh", "max"]),
			10,
		),
		field(
			"model.thinking_ceiling",
			"Thinking Ceiling",
			SettingKind::Enum(&["off", "minimal", "low", "medium", "high", "xhigh", "max"]),
			20,
		),
		field("model.thinking_budgets", "Thinking Budgets", SettingKind::Table, 30),
		field("model.provider_order", "Provider Priority", SettingKind::Array, 40),
		field(
			"model.tier_openai",
			"OpenAI Tier",
			SettingKind::Enum(&["none", "standard", "flex", "priority"]),
			50,
		),
		field(
			"model.tier_anthropic",
			"Anthropic Tier",
			SettingKind::Enum(&["none", "standard", "priority"]),
			60,
		),
		field(
			"model.tier_google",
			"Google Tier",
			SettingKind::Enum(&["none", "standard", "priority"]),
			70,
		),
		field(
			"model.tier_fireworks",
			"Fireworks Tier",
			SettingKind::Enum(&["none", "standard", "priority"]),
			80,
		),
		field(
			"model.tier_subagent",
			"Subagent Tier",
			SettingKind::Enum(&["none", "inherit", "standard", "flex", "priority"]),
			90,
		),
		field(
			"model.tier_advisor",
			"Advisor Tier",
			SettingKind::Enum(&["none", "inherit", "standard", "flex", "priority"]),
			100,
		),
		field(
			"model.cache_retention",
			"Cache Retention",
			SettingKind::Enum(&["auto", "none", "short", "long"]),
			100,
		),
		field(
			"model.openai_websockets",
			"OpenAI WebSockets",
			SettingKind::Enum(&["auto", "off", "on"]),
			110,
		),
		field(
			"model.openrouter_variant",
			"OpenRouter Variant",
			SettingKind::Enum(&["default", "nitro", "floor", "online", "exacto"]),
			120,
		),
		field(
			"model.kimi_api_format",
			"Kimi API Format",
			SettingKind::Enum(&["auto", "openai", "anthropic"]),
			130,
		),
		field("model.tiny_selector", "Tiny Model", SettingKind::String, 140),
		field("model.memory_selector", "Memory Model", SettingKind::String, 150),
		field("model.auto_thinking_selector", "Auto-Thinking Model", SettingKind::String, 160),
		field("model.unexpected_stop_selector", "Unexpected-Stop Model", SettingKind::String, 170),
	];

	fn validate(&self) -> Result<(), ValidationError> {
		let budgets = self.thinking_budgets;
		let ordered =
			[budgets.minimal, budgets.low, budgets.medium, budgets.high, budgets.xhigh, budgets.max];
		let selectors_valid = [
			&self.tiny_selector,
			&self.memory_selector,
			&self.auto_thinking_selector,
			&self.unexpected_stop_selector,
		]
		.into_iter()
		.all(|value| !value.trim().is_empty());
		let priority_valid = self
			.provider_order
			.iter()
			.enumerate()
			.all(|(index, value)| {
				!value.is_empty()
					&& self.provider_order[..index]
						.iter()
						.all(|prior| prior != value)
			});
		if ordered.iter().all(|value| *value > 0)
			&& ordered.windows(2).all(|pair| pair[0] <= pair[1])
			&& selectors_valid
			&& priority_valid
		{
			Ok(())
		} else {
			Err(ValidationError::DomainInvariant { domain: Self::DOMAIN })
		}
	}
}

const fn field(
	path: &'static str,
	label: &'static str,
	kind: SettingKind,
	order: u16,
) -> FieldDescriptor {
	FieldDescriptor {
		path,
		label,
		description: "Runtime-owned model and provider policy.",
		kind,
		scopes: PERSISTED,
		order,
		options: None,
		condition: None,
		secret: false,
	}
}

omp_settings::inventory::submit! { DomainRegistration::of::<ModelSettings>() }

/// Resolves provider family from canonical route and model identities.
#[must_use]
pub fn provider_family(provider: &str, model: Option<&str>) -> ProviderFamily {
	let model = model.unwrap_or_default();
	if provider.contains("anthropic")
		|| provider.contains("claude")
		|| model.contains("anthropic/")
		|| model.contains("claude")
	{
		ProviderFamily::Anthropic
	} else if provider.contains("google")
		|| provider.contains("gemini")
		|| model.contains("google/")
		|| model.contains("gemini")
	{
		ProviderFamily::Google
	} else if provider.contains("openai")
		|| provider == "openrouter"
		|| provider == "azure"
		|| model.contains("openai/")
	{
		ProviderFamily::OpenAi
	} else {
		ProviderFamily::Other
	}
}

/// Exact configured model fallback chains keyed by model id or `provider/*`.
pub type FallbackChains = BTreeMap<Str, Vec<Str>>;
