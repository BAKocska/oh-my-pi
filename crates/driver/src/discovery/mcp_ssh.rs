//! Static native MCP/SSH declaration parsing and validation.

use std::{
	collections::BTreeMap,
	env, fs, io,
	path::{Path, PathBuf},
	sync::Arc,
};

use omp_core::Str;
use serde_json::Value;

use super::{
	containment::{contained_existing, rebase_executable},
	manifest::{
		McpAuth, McpConnection, McpEnvironmentPolicy, McpHeaderPolicy, McpOauth, McpPayload,
		McpRequestIdFormat, McpTransport, SshPayload,
	},
};

/// Native MCP/SSH declaration failure.
#[derive(Debug, thiserror::Error)]
pub enum DeclarationError {
	/// Source could not be read.
	#[error("failed to read declaration {path}")]
	Read {
		/// Source path.
		path:   PathBuf,
		/// Filesystem error.
		#[source]
		source: io::Error,
	},
	/// JSON syntax was invalid.
	#[error("failed to parse declaration {path}")]
	Json {
		/// Source path.
		path:   PathBuf,
		/// JSON parser error.
		#[source]
		source: serde_json::Error,
	},
	/// Declaration shape or security constraints were rejected.
	#[error("invalid native declaration in {path}: {reason}")]
	Invalid {
		/// Source path.
		path:   PathBuf,
		/// Static rejection reason.
		reason: &'static str,
	},
}

/// Parses native `mcp.json`/`.mcp.json`, expands `${VAR}` and
/// `${VAR:-default}` recursively, coerces enabled/timeout scalar forms, and
/// validates endpoints, auth references, literal environment values, and
/// package containment.
pub fn parse_mcp_file(
	path: &Path,
	package_root: Option<&Path>,
) -> Result<Vec<McpPayload>, DeclarationError> {
	let text = fs::read_to_string(path)
		.map_err(|source| DeclarationError::Read { path: path.to_path_buf(), source })?;
	let mut value: Value = serde_json::from_str(&text)
		.map_err(|source| DeclarationError::Json { path: path.to_path_buf(), source })?;
	expand_env_deep(&mut value);
	let servers = value
		.get("mcpServers")
		.or_else(|| value.get("servers"))
		.unwrap_or(&value)
		.as_object()
		.ok_or_else(|| DeclarationError::Invalid {
			path:   path.to_path_buf(),
			reason: "expected an MCP server object",
		})?;
	let base = package_root
		.or_else(|| path.parent())
		.unwrap_or(Path::new("."));
	let mut output = Vec::with_capacity(servers.len());
	for (name, raw) in servers {
		let object = raw.as_object().ok_or_else(|| DeclarationError::Invalid {
			path:   path.to_path_buf(),
			reason: "MCP server must be an object",
		})?;
		let enabled = object.get("enabled").and_then(coerce_bool).unwrap_or(true);
		let timeout_ms = object
			.get("timeoutMs")
			.or_else(|| object.get("timeout"))
			.and_then(coerce_u64);
		let command = object
			.get("command")
			.and_then(Value::as_str)
			.map(PathBuf::from)
			.map(|declared| rebase_executable(base, &declared))
			.transpose()
			.map_err(|_| DeclarationError::Invalid {
				path:   path.to_path_buf(),
				reason: "MCP command escapes its package root",
			})?;
		let args = string_array(object.get("args"));
		let env = string_map(object.get("env"))?;
		let headers = string_map(object.get("headers"))?;
		if headers.iter().any(|(name, value)| {
			!valid_header_name(name.as_str())
				|| value
					.as_bytes()
					.iter()
					.any(|byte| matches!(byte, b'\r' | b'\n'))
		}) {
			return Err(DeclarationError::Invalid {
				path:   path.to_path_buf(),
				reason: "MCP header name or value is invalid",
			});
		}
		let url = object.get("url").and_then(Value::as_str).map(Str::from);
		if let Some(endpoint) = &url {
			let parsed = url::Url::parse(endpoint.as_str()).map_err(|_| {
				DeclarationError::Invalid { path: path.to_path_buf(), reason: "MCP URL is invalid" }
			})?;
			if !matches!(parsed.scheme(), "http" | "https") {
				return Err(DeclarationError::Invalid {
					path:   path.to_path_buf(),
					reason: "MCP URL must use HTTP or HTTPS",
				});
			}
			let loopback = parsed.host_str().is_some_and(|host| {
				host.eq_ignore_ascii_case("localhost")
					|| host
						.parse::<std::net::IpAddr>()
						.is_ok_and(|address| address.is_loopback())
			});
			if parsed.scheme() != "https" && !loopback {
				return Err(DeclarationError::Invalid {
					path:   path.to_path_buf(),
					reason: "remote MCP URLs must use HTTPS",
				});
			}
		}
		if command.is_none() == url.is_none() {
			return Err(DeclarationError::Invalid {
				path:   path.to_path_buf(),
				reason: "MCP server must declare exactly one command or URL",
			});
		}
		let transport = object
			.get("transport")
			.and_then(Value::as_str)
			.map(|value| match value {
				"stdio" => Ok(McpTransport::Stdio),
				"sse" => Ok(McpTransport::Sse),
				"http" | "streamable-http" => Ok(McpTransport::Http),
				_ => Err(()),
			})
			.transpose()
			.map_err(|()| DeclarationError::Invalid {
				path:   path.to_path_buf(),
				reason: "unsupported MCP transport",
			})?;
		let request_id_format = object
			.get("requestIdFormat")
			.and_then(Value::as_str)
			.map(|value| match value {
				"number" | "numeric" => Ok(McpRequestIdFormat::Number),
				"string" => Ok(McpRequestIdFormat::String),
				_ => Err(()),
			})
			.transpose()
			.map_err(|()| DeclarationError::Invalid {
				path:   path.to_path_buf(),
				reason: "unsupported MCP request ID format",
			})?;
		let auth = parse_auth(object.get("auth"), path)?;
		let oauth = parse_oauth(object.get("oauth"), path)?;
		output.push(McpPayload {
			name: Str::from(name.as_str()),
			enabled,
			timeout_ms,
			connection: Arc::new(McpConnection {
				request_id_format,
				command,
				args,
				env,
				env_policy: Some(McpEnvironmentPolicy::Literal),
				cwd: None,
				url,
				headers,
				header_policy: Some(McpHeaderPolicy::OriginLocked),
				auth,
				oauth,
				transport,
			}),
		});
	}
	output.sort_by(|left, right| left.name.cmp(&right.name));
	Ok(output)
}

/// Parses a native `ssh.json` host table and validates identity paths against
/// an optional package root. Discovery only hands declarations to the SSH
/// registry; it never connects.
pub fn parse_ssh_file(
	path: &Path,
	package_root: Option<&Path>,
) -> Result<Vec<SshPayload>, DeclarationError> {
	let text = fs::read_to_string(path)
		.map_err(|source| DeclarationError::Read { path: path.to_path_buf(), source })?;
	let mut value: Value = serde_json::from_str(&text)
		.map_err(|source| DeclarationError::Json { path: path.to_path_buf(), source })?;
	expand_env_deep(&mut value);
	let hosts = value
		.get("hosts")
		.unwrap_or(&value)
		.as_object()
		.ok_or_else(|| DeclarationError::Invalid {
			path:   path.to_path_buf(),
			reason: "expected an SSH hosts object",
		})?;
	let mut output = Vec::with_capacity(hosts.len());
	for (name, raw) in hosts {
		let object = raw.as_object().ok_or_else(|| DeclarationError::Invalid {
			path:   path.to_path_buf(),
			reason: "SSH host must be an object",
		})?;
		let host = object
			.get("host")
			.or_else(|| object.get("hostname"))
			.and_then(Value::as_str)
			.filter(|host| !host.is_empty())
			.ok_or_else(|| DeclarationError::Invalid {
				path:   path.to_path_buf(),
				reason: "SSH host is missing hostname",
			})?;
		let key_path = object
			.get("keyPath")
			.or_else(|| object.get("identityFile"))
			.and_then(Value::as_str)
			.map(PathBuf::from)
			.map(|declared| {
				if let Some(root) = package_root {
					contained_existing(root, &declared)
				} else {
					Ok(declared)
				}
			})
			.transpose()
			.map_err(|_| DeclarationError::Invalid {
				path:   path.to_path_buf(),
				reason: "SSH key path escapes its package root",
			})?;
		let port = object
			.get("port")
			.and_then(coerce_u64)
			.and_then(|port| u16::try_from(port).ok());
		output.push(SshPayload {
			name: Str::from(name.as_str()),
			host: Str::from(host),
			username: object
				.get("username")
				.or_else(|| object.get("user"))
				.and_then(Value::as_str)
				.map(Str::from),
			port,
			key_path,
			description: object
				.get("description")
				.and_then(Value::as_str)
				.map(Str::from),
			compat: object.get("compat").and_then(coerce_bool).unwrap_or(false),
		});
	}
	output.sort_by(|left, right| left.name.cmp(&right.name));
	Ok(output)
}

fn coerce_bool(value: &Value) -> Option<bool> {
	match value {
		Value::Bool(value) => Some(*value),
		Value::Number(value) => value.as_i64().map(|value| value != 0),
		Value::String(value) => match value.trim().to_ascii_lowercase().as_str() {
			"true" | "yes" | "on" | "1" => Some(true),
			"false" | "no" | "off" | "0" => Some(false),
			_ => None,
		},
		_ => None,
	}
}

fn coerce_u64(value: &Value) -> Option<u64> {
	match value {
		Value::Number(value) => value.as_u64(),
		Value::String(value) => value.trim().parse().ok(),
		_ => None,
	}
}

fn valid_header_name(name: &str) -> bool {
	!name.is_empty()
		&& name.bytes().all(|byte| {
			byte.is_ascii_alphanumeric()
				|| matches!(
					byte,
					b'!'
						| b'#' | b'$'
						| b'%' | b'&'
						| b'\'' | b'*'
						| b'+' | b'-'
						| b'.' | b'^'
						| b'_' | b'`'
						| b'|' | b'~'
				)
		})
}

fn string_array(value: Option<&Value>) -> Vec<Str> {
	value
		.and_then(Value::as_array)
		.into_iter()
		.flatten()
		.filter_map(Value::as_str)
		.map(Str::from)
		.collect()
}

fn string_map(value: Option<&Value>) -> Result<BTreeMap<Str, Str>, DeclarationError> {
	let Some(value) = value else {
		return Ok(BTreeMap::new());
	};
	let Some(object) = value.as_object() else {
		return Err(DeclarationError::Invalid {
			path:   PathBuf::new(),
			reason: "environment/header declarations must be string maps",
		});
	};
	object
		.iter()
		.map(|(key, value)| {
			value
				.as_str()
				.map(|value| (Str::from(key.as_str()), Str::from(value)))
				.ok_or_else(|| DeclarationError::Invalid {
					path:   PathBuf::new(),
					reason: "environment/header values must be literal strings",
				})
		})
		.collect()
}

fn parse_auth(value: Option<&Value>, path: &Path) -> Result<Option<McpAuth>, DeclarationError> {
	let Some(object) = value.and_then(Value::as_object) else {
		return Ok(None);
	};
	let kind = object
		.get("kind")
		.or_else(|| object.get("type"))
		.and_then(Value::as_str)
		.filter(|value| !value.is_empty())
		.ok_or_else(|| DeclarationError::Invalid {
			path:   path.to_path_buf(),
			reason: "MCP auth kind is required",
		})?;
	let auth = McpAuth {
		kind:          Str::from(kind),
		credential_id: field(object, "credentialId"),
		token_url:     field(object, "tokenUrl"),
		client_id:     field(object, "clientId"),
		secret_ref:    field(object, "secretRef"),
		resource:      field(object, "resource"),
	};
	if auth.credential_id.as_ref().is_some_and(Str::is_empty)
		|| auth.secret_ref.as_ref().is_some_and(Str::is_empty)
	{
		return Err(DeclarationError::Invalid {
			path:   path.to_path_buf(),
			reason: "MCP auth references must not be empty",
		});
	}
	Ok(Some(auth))
}

fn parse_oauth(value: Option<&Value>, path: &Path) -> Result<Option<McpOauth>, DeclarationError> {
	let Some(object) = value.and_then(Value::as_object) else {
		return Ok(None);
	};
	let callback_port = object
		.get("callbackPort")
		.and_then(coerce_u64)
		.map(u16::try_from)
		.transpose()
		.map_err(|_| DeclarationError::Invalid {
			path:   path.to_path_buf(),
			reason: "OAuth callback port is invalid",
		})?;
	Ok(Some(McpOauth {
		client_id: field(object, "clientId"),
		secret_ref: field(object, "secretRef"),
		redirect_uri: field(object, "redirectUri"),
		callback_port,
		callback_path: field(object, "callbackPath"),
		prompt: field(object, "prompt"),
	}))
}

fn field(object: &serde_json::Map<String, Value>, key: &str) -> Option<Str> {
	object.get(key).and_then(Value::as_str).map(Str::from)
}

/// Recursively expands `${VAR}` and `${VAR:-default}` in static JSON string
/// values. Missing variables without defaults remain visible and unresolved.
pub fn expand_env_deep(value: &mut Value) {
	match value {
		Value::String(value) => *value = expand_env(value),
		Value::Array(values) => values.iter_mut().for_each(expand_env_deep),
		Value::Object(values) => values.values_mut().for_each(expand_env_deep),
		_ => {},
	}
}

fn expand_env(value: &str) -> String {
	let mut output = String::with_capacity(value.len());
	let mut rest = value;
	while let Some(start) = rest.find("${") {
		output.push_str(&rest[..start]);
		let tail = &rest[start + 2..];
		let Some(end) = tail.find('}') else {
			output.push_str(&rest[start..]);
			return output;
		};
		let expression = &tail[..end];
		let (name, fallback) = expression
			.split_once(":-")
			.map_or((expression, None), |(name, fallback)| (name, Some(fallback)));
		if let Ok(replacement) = env::var(name) {
			output.push_str(&replacement);
		} else if let Some(fallback) = fallback {
			output.push_str(fallback);
		} else {
			output.push_str(&rest[start..start + end + 3]);
		}
		rest = &tail[end + 1..];
	}
	output.push_str(rest);
	output
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn expands_defaults_and_coerces_mcp_scalars() {
		let tree = tempfile::tempdir().unwrap();
		let file = tree.path().join("mcp.json");
		fs::write(&file, r#"{"mcpServers":{"demo":{"command":"echo","args":["${OMP_TEST_MISSING:-ok}"],"enabled":"false","timeout":"250"}}}"#).unwrap();
		let servers = parse_mcp_file(&file, None).unwrap();
		assert!(!servers[0].enabled);
		assert_eq!(servers[0].timeout_ms, Some(250));
		assert_eq!(servers[0].connection.args, vec![Str::from("ok")]);
	}
}
