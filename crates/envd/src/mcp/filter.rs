//! Native-device coverage filtering for configured MCP mounts.

use std::collections::{BTreeMap, BTreeSet};

use omp_core::{SecretString, Str};
use omp_inference::id::PrincipalId;

use super::{
	auth_authority::{AuthAffinity, CombinedAuthAuthority},
	config::{McpServerConfig, ResolvedServer, TransportKind},
};

const EXA_HOST_SUFFIX: &str = "mcp.exa.ai";
const EXA_KEY_QUERY: &str = "exaApiKey";
const NATIVE_EXA_SEARCH: &str = "web_search_exa";

/// Exact operation coverage published by native devices.
#[derive(Clone, Debug)]
pub struct NativeCoverage {
	/// Exa MCP leaf names owned by native search.
	pub exa_tools:     BTreeSet<Str>,
	/// Browser MCP leaf names owned by the native browser device.
	pub browser_tools: BTreeSet<Str>,
}

impl Default for NativeCoverage {
	fn default() -> Self {
		Self {
			exa_tools:     BTreeSet::from([Str::new_static(NATIVE_EXA_SEARCH)]),
			browser_tools: BTreeSet::new(),
		}
	}
}

/// Generic MCP mount retained after native coverage analysis.
#[derive(Clone, Debug)]
pub struct FilteredMount {
	/// Source declaration, with extracted literal credentials removed.
	pub server:           ResolvedServer,
	/// Advertised leaves which the generic MCP device must suppress.
	pub suppressed_tools: BTreeSet<Str>,
}

/// Secret key extracted from an Exa MCP declaration.
#[derive(Clone)]
pub struct ExtractedExaKey {
	/// Source server identity, safe for opaque affinity derivation.
	pub server: Str,
	/// Extracted secret bytes.
	pub key:    SecretString,
}

impl std::fmt::Debug for ExtractedExaKey {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter
			.debug_struct("ExtractedExaKey")
			.field("server", &self.server)
			.field("key", &"[REDACTED]")
			.finish()
	}
}

/// Coverage-filter result. Unrestricted native-covered servers remain mounted
/// so post-discovery filtering can retain newly advertised, uncovered leaves.
#[derive(Clone, Debug, Default)]
pub struct FilterResult {
	/// Generic mounts still needed for uncovered leaves.
	pub mounts:   BTreeMap<Str, FilteredMount>,
	/// Secret-typed Exa keys awaiting combined-authority import.
	pub exa_keys: Vec<ExtractedExaKey>,
}

/// Detects native-covered Exa/browser mounts and records leaf suppression.
pub fn filter_native_coverage(
	servers: &BTreeMap<Str, ResolvedServer>,
	coverage: &NativeCoverage,
) -> FilterResult {
	let mut result = FilterResult::default();
	for (name, server) in servers {
		let is_exa = is_exa_server(name, &server.config);
		let is_browser = is_browser_server(name, &server.config);
		let mut sanitized = (*server.config).clone();
		if is_exa {
			if let Some(key) = extract_exa_key(&mut sanitized) {
				result
					.exa_keys
					.push(ExtractedExaKey { server: name.clone(), key });
			}
		}
		let suppressed_tools = if is_exa {
			coverage.exa_tools.clone()
		} else if is_browser {
			coverage.browser_tools.clone()
		} else {
			BTreeSet::new()
		};
		let requested = if is_exa {
			requested_exa_tools(&sanitized)
		} else {
			None
		};
		if requested
			.as_ref()
			.is_some_and(|tools| tools.iter().all(|tool| suppressed_tools.contains(tool)))
		{
			continue;
		}
		let mut retained = server.clone();
		retained.config = std::sync::Arc::new(sanitized);
		result
			.mounts
			.insert(name.clone(), FilteredMount { server: retained, suppressed_tools });
	}
	result
}

/// Imports extracted Exa keys through the one combined credential authority.
/// Returns opaque affinities for native search composition.
pub fn import_exa_keys(
	authority: &CombinedAuthAuthority,
	profile: &str,
	principal: PrincipalId,
	keys: Vec<ExtractedExaKey>,
	now_ms: u64,
) -> Result<Vec<AuthAffinity>, omp_inference::auth::StoreError> {
	let mut affinities = Vec::with_capacity(keys.len());
	for extracted in keys {
		let affinity =
			CombinedAuthAuthority::mcp_affinity(profile, extracted.server.as_str(), principal.clone());
		authority.persist_mcp_api_key(&affinity, extracted.key, now_ms, None)?;
		affinities.push(affinity);
	}
	Ok(affinities)
}

fn is_exa_server(name: &str, config: &McpServerConfig) -> bool {
	if name.eq_ignore_ascii_case("exa") || name.to_ascii_lowercase().contains("websets") {
		return true;
	}
	if config.url.as_ref().is_some_and(|value| {
		url::Url::parse(value)
			.ok()
			.and_then(|url| {
				url.host_str()
					.map(|host| host.eq_ignore_ascii_case(EXA_HOST_SUFFIX))
			})
			.unwrap_or(false)
	}) {
		return true;
	}
	config
		.args
		.iter()
		.any(|arg| arg.to_ascii_lowercase().contains(EXA_HOST_SUFFIX))
}

fn is_browser_server(name: &str, config: &McpServerConfig) -> bool {
	const NAMES: [&str; 6] =
		["puppeteer", "playwright", "browserbase", "browser-tools", "browser-use", "browser"];
	let lower = name.to_ascii_lowercase();
	if NAMES.contains(&lower.as_str()) {
		return true;
	}
	let matches = |value: &str| {
		let lower = value.to_ascii_lowercase();
		[
			"@modelcontextprotocol/server-puppeteer",
			"@playwright/mcp",
			"browserbase",
			"browser-use-mcp",
			"playwright-mcp",
			"puppeteer-mcp",
		]
		.iter()
		.any(|needle| lower.contains(needle))
	};
	config.command.as_ref().is_some_and(|value| matches(value))
		|| config.args.iter().any(|value| matches(value))
		|| config.url.as_ref().is_some_and(|value| matches(value))
}

fn extract_exa_key(config: &mut McpServerConfig) -> Option<SecretString> {
	if let Some(value) = config.env.remove("EXA_API_KEY") {
		if !value.is_empty() && !value.starts_with('!') {
			return Some(SecretString::from(value.as_str()));
		}
		config.env.insert(Str::new_static("EXA_API_KEY"), value);
	}
	if let Some(raw) = config.url.as_ref()
		&& let Ok(mut url) = url::Url::parse(raw)
		&& let Some(value) = url
			.query_pairs()
			.find(|(key, _)| key.eq_ignore_ascii_case(EXA_KEY_QUERY))
			.map(|(_, value)| value.into_owned())
	{
		let retained: Vec<(String, String)> = url
			.query_pairs()
			.filter(|(key, _)| !key.eq_ignore_ascii_case(EXA_KEY_QUERY))
			.map(|(key, value)| (key.into_owned(), value.into_owned()))
			.collect();
		url.query_pairs_mut().clear().extend_pairs(retained);
		config.url = Some(Str::from(url.as_str()));
		return Some(SecretString::from(value));
	}
	for arg in &mut config.args {
		if let Some(value) = query_value(arg, EXA_KEY_QUERY) {
			let secret = SecretString::from(value.as_str());
			*arg = Str::from(redact_query_value(arg, EXA_KEY_QUERY));
			return Some(secret);
		}
	}
	None
}

fn query_value(value: &str, key: &str) -> Option<String> {
	value.split(['?', '&', ' ']).find_map(|part| {
		part
			.split_once('=')
			.filter(|(name, value)| name.eq_ignore_ascii_case(key) && !value.is_empty())
			.map(|(_, value)| value.to_owned())
	})
}

fn redact_query_value(value: &str, key: &str) -> String {
	value
		.split('&')
		.filter(|part| {
			!part
				.trim_start_matches(|character| character == '?' || character == ' ')
				.split_once('=')
				.is_some_and(|(name, _)| name.eq_ignore_ascii_case(key))
		})
		.collect::<Vec<_>>()
		.join("&")
}

fn requested_exa_tools(config: &McpServerConfig) -> Option<BTreeSet<Str>> {
	let raw = match config.resolved_transport() {
		TransportKind::Http | TransportKind::Sse => config
			.url
			.as_ref()
			.and_then(|raw| url::Url::parse(raw).ok())
			.and_then(|url| {
				url.query_pairs()
					.find(|(key, _)| key.eq_ignore_ascii_case("tools"))
					.map(|(_, value)| value.into_owned())
			}),
		TransportKind::Stdio => config.args.iter().enumerate().find_map(|(index, arg)| {
			if matches!(arg.as_str(), "--tools" | "-tools") {
				config.args.get(index + 1).map(ToString::to_string)
			} else {
				query_value(arg, "tools")
			}
		}),
	};
	raw.map(|raw| {
		raw.split(',')
			.map(str::trim)
			.filter(|value| !value.is_empty())
			.map(Str::from)
			.collect()
	})
	.filter(|tools: &BTreeSet<Str>| !tools.is_empty())
}

#[cfg(test)]
mod tests {
	use std::{path::PathBuf, sync::Arc};

	use omp_core::ExposeSecret as _;

	use super::*;
	use crate::mcp::config::{ConfigSourceKind, McpServerConfig};

	fn remote(url: &str) -> ResolvedServer {
		ResolvedServer {
			name:        Str::from("exa"),
			source:      PathBuf::from("config"),
			source_kind: ConfigSourceKind::User,
			writable:    true,
			config:      Arc::new(McpServerConfig {
				transport:         Some(TransportKind::Http),
				enabled:           true,
				command:           None,
				args:              Vec::new(),
				env:               BTreeMap::new(),
				env_policy:        None,
				cwd:               None,
				url:               Some(Str::from(url)),
				headers:           BTreeMap::new(),
				header_policy:     None,
				timeout:           None,
				request_id_format: None,
				auth:              None,
				oauth:             None,
				protocol_versions: Vec::new(),
			}),
		}
	}

	#[test]
	fn extracts_key_and_retains_uncovered_exa_tools() {
		let servers = BTreeMap::from([(
			Str::from("exa"),
			remote("https://mcp.exa.ai/mcp?exaApiKey=top-secret&tools=web_search_exa,web_fetch_exa"),
		)]);
		let filtered = filter_native_coverage(&servers, &NativeCoverage::default());
		assert!(filtered.mounts.contains_key("exa"));
		assert!(
			filtered.mounts["exa"]
				.suppressed_tools
				.contains("web_search_exa")
		);
		assert_eq!(filtered.exa_keys[0].key.expose_secret(), "top-secret");
		assert!(
			!filtered.mounts["exa"]
				.server
				.config
				.url
				.as_ref()
				.expect("url")
				.contains("top-secret")
		);
		assert!(!format!("{:?}", filtered.exa_keys).contains("top-secret"));
	}

	#[test]
	fn drops_exactly_native_only_restricted_mount() {
		let servers = BTreeMap::from([(
			Str::from("exa"),
			remote("https://mcp.exa.ai/mcp?tools=web_search_exa"),
		)]);
		assert!(
			filter_native_coverage(&servers, &NativeCoverage::default())
				.mounts
				.is_empty()
		);
	}
}
