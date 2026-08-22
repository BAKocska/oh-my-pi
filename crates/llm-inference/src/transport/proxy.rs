//! URL-based standard proxy environment resolution.

use std::{env, ffi::OsString, net::IpAddr};

use url::Url;

/// Resolves the standard proxy environment for one destination URL.
pub(crate) fn for_url(url: &Url) -> Option<Url> {
	for_url_with(url, |name| env::var_os(name))
}

fn for_url_with(url: &Url, env: impl Fn(&str) -> Option<OsString>) -> Option<Url> {
	if bypasses_proxy(url, &env) {
		return None;
	}
	let protocol_names: &[&str] = match url.scheme() {
		"https" | "wss" => &["HTTPS_PROXY", "https_proxy"],
		"http" | "ws" => &["HTTP_PROXY", "http_proxy"],
		_ => return None,
	};
	protocol_names
		.iter()
		.chain(["ALL_PROXY", "all_proxy"].iter())
		.find_map(|name| env(name).and_then(parse_proxy))
}

fn parse_proxy(value: OsString) -> Option<Url> {
	let value = value.to_str()?.trim();
	if value.is_empty() {
		return None;
	}
	Url::parse(value)
		.or_else(|_| Url::parse(&format!("http://{value}")))
		.ok()
		.filter(|url| url.host_str().is_some())
}

fn bypasses_proxy(url: &Url, env: &impl Fn(&str) -> Option<OsString>) -> bool {
	let host = url
		.host_str()
		.unwrap_or_default()
		.trim_matches(|character| matches!(character, '[' | ']'))
		.to_ascii_lowercase();
	if is_local_or_metadata(&host) {
		return true;
	}
	let Some(rules) = env("NO_PROXY").or_else(|| env("no_proxy")) else {
		return false;
	};
	let Some(rules) = rules.to_str() else {
		return false;
	};
	let port = url.port_or_known_default();
	rules
		.split(|character: char| character == ',' || character.is_ascii_whitespace())
		.filter(|rule| !rule.is_empty())
		.any(|rule| no_proxy_matches(rule, &host, port))
}

fn no_proxy_matches(rule: &str, host: &str, port: Option<u16>) -> bool {
	if rule == "*" {
		return true;
	}
	let rule = rule.to_ascii_lowercase();
	let (rule_host, rule_port) = split_rule_host_port(&rule);
	if rule_port.is_some() && rule_port != port {
		return false;
	}
	let rule_host = rule_host
		.trim_matches(|character| matches!(character, '[' | ']'))
		.trim_start_matches('.');
	!rule_host.is_empty()
		&& (host == rule_host
			|| host
				.strip_suffix(rule_host)
				.is_some_and(|prefix| prefix.ends_with('.')))
}

fn split_rule_host_port(rule: &str) -> (&str, Option<u16>) {
	if let Some(bracket) = rule.strip_prefix('[')
		&& let Some(end) = bracket.find(']')
	{
		let host_end = end + 2;
		let port = rule
			.get(host_end..)
			.and_then(|tail| tail.strip_prefix(':'))
			.and_then(|port| port.parse().ok());
		return (&rule[..host_end], port);
	}
	let Some((host, port)) = rule.rsplit_once(':') else {
		return (rule, None);
	};
	if host.contains(':') {
		return (rule, None);
	}
	port.parse().map_or((rule, None), |port| (host, Some(port)))
}

fn is_local_or_metadata(host: &str) -> bool {
	if host == "localhost"
		|| host.ends_with(".localhost")
		|| host == "metadata.google.internal"
	{
		return true;
	}
	let Ok(ip) = host.parse::<IpAddr>() else {
		return false;
	};
	match ip {
		IpAddr::V4(ip) => {
			let [first, second, ..] = ip.octets();
			first == 0
				|| first == 10
				|| first == 127
				|| (first == 169 && second == 254)
				|| (first == 172 && (16..=31).contains(&second))
				|| (first == 192 && second == 168)
		},
		IpAddr::V6(ip) => ip.is_loopback() || ip.is_unspecified() || {
			let first = ip.segments()[0];
			(first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
		},
	}
}

#[cfg(test)]
mod tests {
	use std::collections::BTreeMap;

	use super::*;

	fn resolve(url: &str, values: &[(&str, &str)]) -> Option<Url> {
		let values = values
			.iter()
			.map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
			.collect::<BTreeMap<_, _>>();
		for_url_with(&Url::parse(url).unwrap(), |name| values.get(name).cloned())
	}

	#[test]
	fn resolves_protocol_proxy_then_all_proxy() {
		assert_eq!(
			resolve(
				"https://api2.cursor.sh/agent.v1.AgentService/Run",
				&[("HTTPS_PROXY", "http://secure-proxy:8080"), ("ALL_PROXY", "http://all:8080")],
			)
			.unwrap()
			.as_str(),
			"http://secure-proxy:8080/"
		);
		assert_eq!(
			resolve("https://api2.cursor.sh", &[("ALL_PROXY", "all-proxy:8080")])
				.unwrap()
				.as_str(),
			"http://all-proxy:8080/"
		);
		assert_eq!(
			resolve("http://api2.cursor.sh", &[("HTTP_PROXY", "http://plain:8080")])
				.unwrap()
				.as_str(),
			"http://plain:8080/"
		);
	}

	#[test]
	fn no_proxy_and_local_destinations_bypass() {
		assert!(
			resolve(
				"https://api2.cursor.sh:8443",
				&[("HTTPS_PROXY", "http://proxy:8080"), ("NO_PROXY", ".cursor.sh:8443")],
			)
			.is_none()
		);
		assert!(
			resolve("http://127.0.0.1", &[("HTTP_PROXY", "http://proxy:8080")]).is_none()
		);
	}
}
