//! Ordered discovery of native MCP configuration sources.
//!
//! This module intentionally has no foreign-tool roots. Claude/plugin MCP files
//! are not product inputs; native OMP manifests arrive as already admitted,
//! data-only mounts.

use std::path::{Path, PathBuf};

use omp_core::Str;

use crate::{
	discovery::manifest::{
		McpEnvironmentPolicy, McpHeaderPolicy, McpPayload, McpRequestIdFormat, McpTransport,
	},
	envd::mcp::config::{
		self, AuthConfig, AuthKind, ConfigSource, ConfigSourceKind, EnvironmentPolicy, HeaderPolicy,
		McpConfigFile, McpServerConfig, OauthConfig, RequestIdFormat, TransportKind,
	},
};

/// Native OMP manifest MCP mount admitted by extension discovery.
#[derive(Clone, Debug)]
pub struct ManifestMcpMount {
	/// Stable manifest identity or path used for diagnostics and ownership.
	pub identity: PathBuf,
	/// Data-only MCP declarations from the admitted native manifest.
	pub file:     McpConfigFile,
}

/// Converts admitted native manifest payloads into the runtime file schema.
pub fn manifest_mount(
	identity: PathBuf,
	payloads: &[McpPayload],
) -> Result<ManifestMcpMount, ManifestMountError> {
	let mut file = McpConfigFile::default();
	for payload in payloads {
		let connection = &payload.connection;
		let command = connection
			.command
			.as_ref()
			.map(|path| {
				path
					.to_str()
					.map(Str::from)
					.ok_or_else(|| ManifestMountError::NonUtf8Path { path: path.clone() })
			})
			.transpose()?;
		let auth = connection
			.auth
			.as_ref()
			.map(|auth| {
				let kind = match auth.kind.as_str() {
					"oauth" => AuthKind::Oauth,
					"apikey" => AuthKind::Apikey,
					_ => {
						return Err(ManifestMountError::AuthKind { kind: auth.kind.clone() });
					},
				};
				Ok(AuthConfig {
					kind,
					credential_id: auth.credential_id.clone(),
					token_url: auth.token_url.clone(),
					client_id: auth.client_id.clone(),
					secret_ref: auth.secret_ref.clone(),
					resource: auth.resource.clone(),
				})
			})
			.transpose()?;
		file
			.mcp_servers
			.insert(payload.name.clone(), McpServerConfig {
				transport: connection.transport.map(|transport| match transport {
					McpTransport::Stdio => TransportKind::Stdio,
					McpTransport::Http => TransportKind::Http,
					McpTransport::Sse => TransportKind::Sse,
				}),
				enabled: payload.enabled,
				command,
				args: connection.args.clone(),
				env: connection.env.clone(),
				env_policy: connection.env_policy.map(|policy| match policy {
					McpEnvironmentPolicy::Literal => EnvironmentPolicy::Literal,
				}),
				cwd: connection.cwd.clone(),
				url: connection.url.clone(),
				headers: connection.headers.clone(),
				header_policy: connection.header_policy.map(|policy| match policy {
					McpHeaderPolicy::OriginLocked => HeaderPolicy::OriginLocked,
				}),
				timeout: payload.timeout_ms,
				request_id_format: connection.request_id_format.map(|format| match format {
					McpRequestIdFormat::Number => RequestIdFormat::Number,
					McpRequestIdFormat::String => RequestIdFormat::String,
				}),
				auth,
				oauth: connection.oauth.as_ref().map(|oauth| OauthConfig {
					client_id:     oauth.client_id.clone(),
					secret_ref:    oauth.secret_ref.clone(),
					redirect_uri:  oauth.redirect_uri.clone(),
					callback_port: oauth.callback_port,
					callback_path: oauth.callback_path.clone(),
					prompt:        oauth.prompt.clone(),
				}),
				protocol_versions: Vec::new(),
			});
	}
	Ok(ManifestMcpMount { identity, file })
}

/// Native manifest conversion failure.
#[derive(Debug, thiserror::Error)]
pub enum ManifestMountError {
	/// Stdio executable path cannot be represented in JSON.
	#[error("native MCP manifest command path `{path}` is not UTF-8")]
	NonUtf8Path {
		/// Invalid path.
		path: PathBuf,
	},
	/// Manifest auth kind is not part of the MCP schema.
	#[error("native MCP manifest uses unsupported auth kind `{kind}`")]
	AuthKind {
		/// Invalid auth kind.
		kind: Str,
	},
}

/// Result of native MCP discovery in stable scan order.
#[derive(Debug, Default)]
pub struct McpDiscovery {
	/// Parsed sources: user, project, root fallback, then native manifests.
	pub sources:     Vec<ConfigSource>,
	/// Non-fatal malformed or unreadable sources.
	pub diagnostics: Vec<McpDiscoveryError>,
}

/// Discovers only ratified native MCP sources.
///
/// Missing files are omitted. A malformed source produces a diagnostic without
/// preventing independent sources from loading.
pub async fn discover_mcp(
	home: Option<&Path>,
	project_root: &Path,
	manifests: impl IntoIterator<Item = ManifestMcpMount>,
) -> McpDiscovery {
	let mut discovered = McpDiscovery::default();
	let mut candidates = Vec::with_capacity(3);
	if let Some(home) = home {
		candidates.push((home.join(".omp/mcp.json"), ConfigSourceKind::User));
	}
	candidates.push((project_root.join(".omp/mcp.json"), ConfigSourceKind::Project));
	candidates.push((project_root.join(".mcp.json"), ConfigSourceKind::Root));

	for (path, kind) in candidates {
		match read_source(path, kind).await {
			Ok(Some(source)) => discovered.sources.push(source),
			Ok(None) => {},
			Err(error) => discovered.diagnostics.push(error),
		}
	}
	for manifest in manifests {
		let validation = config::validate_file(&manifest.file);
		if validation.is_empty() {
			discovered.sources.push(ConfigSource {
				path: manifest.identity,
				kind: ConfigSourceKind::Manifest,
				file: manifest.file,
			});
		} else {
			discovered.diagnostics.push(McpDiscoveryError::Validation {
				path:   manifest.identity,
				issues: validation.into_boxed_slice(),
			});
		}
	}
	discovered
}

async fn read_source(
	path: PathBuf,
	kind: ConfigSourceKind,
) -> Result<Option<ConfigSource>, McpDiscoveryError> {
	let bytes = match tokio::fs::read(&path).await {
		Ok(bytes) => bytes,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
		Err(source) => return Err(McpDiscoveryError::Read { path, source }),
	};
	let file: McpConfigFile = serde_json::from_slice(&bytes)
		.map_err(|source| McpDiscoveryError::Parse { path: path.clone(), source })?;
	let issues = config::validate_file(&file);
	if !issues.is_empty() {
		return Err(McpDiscoveryError::Validation { path, issues: issues.into_boxed_slice() });
	}
	Ok(Some(ConfigSource { path, kind, file }))
}

/// Native MCP discovery failure.
#[derive(Debug, thiserror::Error)]
pub enum McpDiscoveryError {
	/// Source could not be read.
	#[error("failed to read MCP configuration `{path}`")]
	Read {
		/// Source path.
		path:   PathBuf,
		/// Filesystem failure.
		#[source]
		source: std::io::Error,
	},
	/// Source JSON is malformed.
	#[error("failed to parse MCP configuration `{path}`")]
	Parse {
		/// Source path.
		path:   PathBuf,
		/// JSON parse failure.
		#[source]
		source: serde_json::Error,
	},
	/// Source schema is invalid.
	#[error("MCP configuration `{path}` failed schema validation")]
	Validation {
		/// Source path or manifest identity.
		path:   PathBuf,
		/// Every independently actionable schema issue.
		issues: Box<[config::ConfigValidationError]>,
	},
}

#[cfg(test)]
mod tests {
	use std::collections::BTreeMap;

	use omp_core::Str;

	use super::*;
	use crate::envd::mcp::config::{McpServerConfig, TransportKind};

	fn file(command: &str) -> McpConfigFile {
		let mut file = McpConfigFile::default();
		file
			.mcp_servers
			.insert(Str::from(command), McpServerConfig {
				transport:         Some(TransportKind::Stdio),
				enabled:           true,
				command:           Some(Str::from(command)),
				args:              Vec::new(),
				env:               BTreeMap::new(),
				env_policy:        None,
				cwd:               None,
				url:               None,
				headers:           BTreeMap::new(),
				header_policy:     None,
				timeout:           None,
				request_id_format: None,
				auth:              None,
				oauth:             None,
				protocol_versions: Vec::new(),
			});
		file
	}

	#[tokio::test]
	async fn scans_only_native_sources_in_ratified_order() {
		let scratch = tempfile::tempdir().expect("scratch");
		let home = scratch.path().join("home");
		let project = scratch.path().join("project");
		tokio::fs::create_dir_all(home.join(".omp"))
			.await
			.expect("home dir");
		tokio::fs::create_dir_all(project.join(".omp"))
			.await
			.expect("project dir");
		tokio::fs::create_dir_all(project.join(".claude"))
			.await
			.expect("foreign dir");
		for (path, value) in [
			(home.join(".omp/mcp.json"), file("user")),
			(project.join(".omp/mcp.json"), file("project")),
			(project.join(".mcp.json"), file("root")),
			(project.join(".claude/mcp.json"), file("foreign")),
		] {
			tokio::fs::write(path, serde_json::to_vec(&value).expect("serialize"))
				.await
				.expect("write");
		}
		let result = discover_mcp(Some(&home), &project, [ManifestMcpMount {
			identity: PathBuf::from("native-manifest"),
			file:     file("manifest"),
		}])
		.await;
		assert!(result.diagnostics.is_empty());
		assert_eq!(
			result
				.sources
				.iter()
				.map(|source| source.kind)
				.collect::<Vec<_>>(),
			[
				ConfigSourceKind::User,
				ConfigSourceKind::Project,
				ConfigSourceKind::Root,
				ConfigSourceKind::Manifest,
			]
		);
		assert!(
			result
				.sources
				.iter()
				.all(|source| !source.path.to_string_lossy().contains(".claude"))
		);
	}
}
