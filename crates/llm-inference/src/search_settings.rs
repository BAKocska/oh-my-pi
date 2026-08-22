//! Typed web-search routing and provider settings.

use omp_core::Str;
use omp_settings::{
	DomainRegistration, FieldDescriptor, SettingKind, SettingScope, SettingsDomain, ValidationError,
};
use serde::{Deserialize, Serialize};

const PERSISTED: &[SettingScope] = &[SettingScope::Global, SettingScope::Project];

/// Search provider routing and endpoint policy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct WebSearchSettings {
	/// Automatic provider preference order.
	pub order:                Vec<Str>,
	/// Providers omitted from automatic search.
	pub exclusions:           Vec<Str>,
	/// Per-provider attempt timeout in seconds.
	pub timeout_seconds:      u32,
	/// Optional self-hosted SearXNG endpoint.
	pub searxng_endpoint:     Option<Str>,
	/// Optional Gemini grounding model.
	pub gemini_model:         Option<Str>,
	/// Antigravity endpoint selection (`auto`, `production`, or `sandbox`).
	pub antigravity_mode:     Str,
	/// Whether Perplexity uses its Responses endpoint.
	pub perplexity_responses: bool,
}

impl Default for WebSearchSettings {
	fn default() -> Self {
		Self {
			order:                [
				"perplexity",
				"gemini",
				"anthropic",
				"codex",
				"xai",
				"zai",
				"exa",
				"tinyfish",
				"jina",
				"kagi",
				"tavily",
				"firecrawl",
				"brave",
				"kimi",
				"parallel",
				"synthetic",
				"searxng",
				"startpage",
				"duckduckgo",
				"ecosia",
				"google",
				"mojeek",
				"public",
			]
			.into_iter()
			.map(Str::new_static)
			.collect(),
			exclusions:           Vec::new(),
			timeout_seconds:      60,
			searxng_endpoint:     None,
			gemini_model:         None,
			antigravity_mode:     Str::new_static("auto"),
			perplexity_responses: false,
		}
	}
}

impl SettingsDomain for WebSearchSettings {
	const DOMAIN: &'static str = "web_search";
	const FIELDS: &'static [FieldDescriptor] = &[
		FieldDescriptor {
			path:        "web_search.order",
			label:       "Provider Order",
			description: "Automatic web-search provider order.",
			kind:        SettingKind::Array,
			scopes:      PERSISTED,
			order:       10,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "web_search.exclusions",
			label:       "Provider Exclusions",
			description: "Providers excluded from automatic web search.",
			kind:        SettingKind::Array,
			scopes:      PERSISTED,
			order:       20,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "web_search.timeout_seconds",
			label:       "Attempt Timeout",
			description: "Per-provider search timeout in seconds (1-300).",
			kind:        SettingKind::Integer,
			scopes:      PERSISTED,
			order:       30,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "web_search.searxng_endpoint",
			label:       "SearXNG Endpoint",
			description: "Optional HTTPS SearXNG endpoint.",
			kind:        SettingKind::String,
			scopes:      PERSISTED,
			order:       40,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "web_search.gemini_model",
			label:       "Gemini Model",
			description: "Optional Gemini grounding model.",
			kind:        SettingKind::String,
			scopes:      PERSISTED,
			order:       50,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "web_search.antigravity_mode",
			label:       "Antigravity Mode",
			description: "Antigravity endpoint mode.",
			kind:        SettingKind::Enum(&["auto", "production", "sandbox"]),
			scopes:      PERSISTED,
			order:       60,
			options:     None,
			condition:   None,
			secret:      false,
		},
		FieldDescriptor {
			path:        "web_search.perplexity_responses",
			label:       "Perplexity Responses",
			description: "Use the Perplexity Responses endpoint.",
			kind:        SettingKind::Boolean,
			scopes:      PERSISTED,
			order:       70,
			options:     None,
			condition:   None,
			secret:      false,
		},
	];

	fn validate(&self) -> Result<(), ValidationError> {
		let unique = |values: &[Str]| {
			values.iter().all(|value| !value.is_empty())
				&& values
					.iter()
					.enumerate()
					.all(|(index, value)| values[..index].iter().all(|prior| prior != value))
		};
		let endpoint_valid = self.searxng_endpoint.as_deref().is_none_or(|endpoint| {
			url::Url::parse(endpoint)
				.is_ok_and(|url| url.scheme() == "https" && url.host_str().is_some())
		});
		if unique(&self.order)
			&& unique(&self.exclusions)
			&& (1..=300).contains(&self.timeout_seconds)
			&& matches!(self.antigravity_mode.as_str(), "auto" | "production" | "sandbox")
			&& endpoint_valid
		{
			Ok(())
		} else {
			Err(ValidationError::DomainInvariant { domain: Self::DOMAIN })
		}
	}
}

omp_settings::inventory::submit! { DomainRegistration::of::<WebSearchSettings>() }

#[cfg(test)]
mod tests {
	use omp_settings::{SettingsSnapshot, registered_domains};

	use super::*;

	#[test]
	fn projection_is_registered_and_rejects_invalid_timeout() {
		let expected = WebSearchSettings { timeout_seconds: 42, ..Default::default() };
		let snapshot = SettingsSnapshot::isolated(expected.clone()).expect("snapshot");
		assert_eq!(snapshot.project::<WebSearchSettings>().unwrap().get(), &expected);
		assert!(
			registered_domains()
				.iter()
				.any(|domain| domain.name == WebSearchSettings::DOMAIN)
		);
		assert!(
			WebSearchSettings { timeout_seconds: 301, ..Default::default() }
				.validate()
				.is_err()
		);
	}
}
