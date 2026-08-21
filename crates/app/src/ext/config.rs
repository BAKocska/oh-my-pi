//! Layered extension configuration and environment overrides.

use std::{
	collections::{BTreeMap, BTreeSet},
	env,
	path::PathBuf,
};

use omp_core::{Str, sf};
use serde::{Deserialize, Serialize};

use super::{ExtensionCode, ExtensionError, Layer};

/// The ordered configuration scopes used for extension precedence.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd)]
pub enum Scope {
	/// The operator's client configuration.
	#[default]
	Client,
	/// The workspace's configuration, applied after the client scope.
	Workspace,
}

impl Scope {
	/// Returns the corresponding extension layer.
	#[must_use]
	pub const fn layer(self) -> Layer {
		match self {
			Self::Client => Layer::Client,
			Self::Workspace => Layer::Workspace,
		}
	}
}

/// A source specification accepted by extension discovery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceSpec {
	/// An omp extension index distribution.
	Index {
		/// Explicit index URL, empty when configured indexes select it.
		index:        String,
		/// Distribution name resolved from that index.
		distribution: Str,
	},
	/// A `PyPI` distribution.
	Pypi {
		/// Distribution name resolved through `PyPI`.
		distribution: Str,
	},
	/// A commit-pinned Git source.
	Git {
		/// Canonical Git repository URL.
		repository: String,
		/// Immutable commit or annotated tag.
		revision:   Str,
	},
	/// A local development source.
	Path(PathBuf),
	/// A hash-addressable archive URL.
	Url(String),
}

impl SourceSpec {
	/// Parses the explicit source grammar. `link` is deliberately absent: links
	/// are local install-record overlays and can never be resolution sources.
	pub fn parse(value: &str) -> Result<Self, ExtensionError> {
		let (kind, rest) = value.split_once(':').ok_or_else(|| {
			ExtensionError::new(
				ExtensionCode::ENoManifest,
				"source must use index:, pypi:, git:, path:, or url:",
			)
		})?;
		match kind {
			"index" if !rest.is_empty() => {
				let (index, distribution) = rest.rsplit_once('/').unwrap_or(("", rest));
				Ok(Self::Index { index: index.to_owned(), distribution: Str::new(distribution) })
			},
			"pypi" if !rest.is_empty() => Ok(Self::Pypi { distribution: Str::new(rest) }),
			"git" => {
				let (repository, revision) = rest.rsplit_once('@').ok_or_else(|| {
					ExtensionError::new(
						ExtensionCode::EGitFloating,
						"git source must name a commit or annotated tag",
					)
				})?;
				if revision.is_empty() {
					return Err(ExtensionError::new(
						ExtensionCode::EGitFloating,
						"git source has an empty revision",
					));
				}
				Ok(Self::Git { repository: repository.to_owned(), revision: Str::new(revision) })
			},
			"path" if !rest.is_empty() => Ok(Self::Path(PathBuf::from(rest))),
			"url" if rest.starts_with("https://") => Ok(Self::Url(rest.to_owned())),
			"link" => Err(ExtensionError::new(
				ExtensionCode::ELockLink,
				"link is an installed.toml development overlay, not a source",
			)),
			_ => Err(ExtensionError::new(ExtensionCode::ENoManifest, "unknown extension source")),
		}
	}
}

/// The `[extensions]` table for one precedence scope.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ExtensionOverlay {
	/// Extension ids enabled by this scope.
	#[serde(default)]
	pub enabled:  BTreeSet<Str>,
	/// Extension ids disabled by this scope; this is the negative P7 input.
	#[serde(default)]
	pub disabled: BTreeSet<Str>,
	/// Workspace-only replacement declarations.
	#[serde(default)]
	pub replace:  BTreeSet<Str>,
	/// Feature selections replacing the install-record feature selection.
	#[serde(default)]
	pub features: BTreeMap<Str, Vec<Str>>,
	/// Scalar, non-secret settings delivered to extensions.
	#[serde(default)]
	pub settings: BTreeMap<Str, BTreeMap<Str, toml::Value>>,
}

impl ExtensionOverlay {
	/// Validates scope-only and secret-handling invariants before the overlay is
	/// used.
	pub fn validate(&self, scope: Scope) -> Result<(), ExtensionError> {
		if scope == Scope::Client && !self.replace.is_empty() {
			return Err(ExtensionError::new(
				ExtensionCode::EReplaceScope,
				"[extensions].replace is workspace-only",
			));
		}
		for (extension, settings) in &self.settings {
			for (key, value) in settings {
				if !value.is_str() && !value.is_integer() && !value.is_float() && !value.is_bool() {
					return Err(ExtensionError::new(
						ExtensionCode::ESettingSecret,
						format!("{extension}.{key} is not a scalar setting"),
					));
				}
				if matches!(key.as_str(), "secret" | "password" | "token" | "api_key" | "key") {
					return Err(ExtensionError::new(
						ExtensionCode::ESettingSecret,
						format!("{extension}.{key} belongs in omp.creds"),
					));
				}
			}
		}
		Ok(())
	}
}

/// A parsed configuration scope and its P1/P2 position.
#[derive(Clone, Debug, Default)]
pub struct ScopedOverlay {
	/// Scope identity.
	pub scope:   Scope,
	/// Parsed overlay.
	pub overlay: ExtensionOverlay,
}

/// The result of applying P1–P7 to a specific extension id.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EffectiveExtensionConfig {
	/// Whether P7 disabled the extension in any scope.
	pub disabled:         bool,
	/// Whether the latest non-negative scope enabled the extension.
	pub enabled:          bool,
	/// Latest feature selection, replacing rather than merging.
	pub features:         Vec<Str>,
	/// Later scalar settings override earlier settings.
	pub settings:         BTreeMap<Str, toml::Value>,
	/// Workspace replacement was explicitly declared.
	pub replace_declared: bool,
}

/// Folds ordered client then workspace overlays. P7 is represented directly as
/// the `disabled` accumulator so no caller can accidentally implement a
/// first-wins exception.
#[must_use]
pub fn fold_extension(scopes: &[ScopedOverlay], id: &Str) -> EffectiveExtensionConfig {
	let mut result = EffectiveExtensionConfig::default();
	for scope in scopes {
		let overlay = &scope.overlay;
		result.disabled |= overlay.disabled.contains(id);
		if overlay.enabled.contains(id) {
			result.enabled = true;
		}
		if let Some(features) = overlay.features.get(id) {
			result.features.clone_from(features);
		}
		if let Some(settings) = overlay.settings.get(id) {
			for (key, value) in settings {
				result.settings.insert(key.clone(), value.clone());
			}
		}
		result.replace_declared |= scope.scope == Scope::Workspace && overlay.replace.contains(id);
	}
	if result.disabled {
		result.enabled = false;
	}
	result
}

/// Parses supported extension environment variables before CLI flag wiring.
#[derive(Clone, Debug, Default)]
pub struct ExtensionEnvironment {
	/// Content-addressed store root.
	pub store:         Option<PathBuf>,
	/// Artifact cache root.
	pub cache:         Option<PathBuf>,
	/// Ordered configured indexes.
	pub indexes:       Vec<String>,
	/// Index public-key path.
	pub index_keys:    Option<PathBuf>,
	/// Offline mode; `strict` also fails closed on stale revocations.
	pub offline:       OfflineMode,
	/// Lock mutation refusal.
	pub locked:        bool,
	/// R9 resolution clamp.
	pub exclude_newer: Option<Str>,
	/// Emergency negative admission set.
	pub disabled:      BTreeSet<Str>,
	/// Suppresses the workspace layer entirely.
	pub no_workspace:  bool,
	/// Noninteractive grants.
	pub grant:         Option<String>,
	/// Build allowance for path/git only.
	pub allow_build:   bool,
	/// Publisher signing key.
	pub sign_key:      Option<PathBuf>,
	/// `uv` executable.
	pub uv:            Option<PathBuf>,
	/// Target triples.
	pub targets:       Vec<Str>,
	/// Diagnostic resolution trace.
	pub trace:         bool,
	/// Ambient one-entry Python site override, reported as `W-SITE-OVERRIDE`.
	pub site_override: Option<PathBuf>,
	/// Per-host environment socket.
	pub env_socket:    Option<PathBuf>,
}

/// Offline policy derived from `OMP_EXT_OFFLINE`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OfflineMode {
	/// Network access is permitted.
	#[default]
	Online,
	/// Network is prohibited but stale revocation lists warn and proceed.
	Offline,
	/// Network is prohibited and stale revocation lists are refused.
	Strict,
}

impl ExtensionEnvironment {
	/// Reads the `OMP_EXT_*` configuration surface. Flag equivalence is wired by
	/// `ExtCli`; this type deliberately has no CLI dependency.
	#[must_use]
	pub fn from_environment() -> Self {
		let value = |name| env::var(name).ok().filter(|value| !value.is_empty());
		let comma = |name| {
			value(name).map_or_else(Vec::new, |value| {
				value
					.split(',')
					.filter(|entry| !entry.is_empty())
					.map(Str::new)
					.collect()
			})
		};
		let bool_value = |name| matches!(value(name).as_deref(), Some("1" | "true"));
		Self {
			store:         value("OMP_EXT_STORE").map(PathBuf::from),
			cache:         value("OMP_EXT_CACHE").map(PathBuf::from),
			indexes:       value("OMP_EXT_INDEX").map_or_else(Vec::new, |value| {
				value
					.split(',')
					.filter(|entry| !entry.is_empty())
					.map(str::to_owned)
					.collect()
			}),
			index_keys:    value("OMP_EXT_INDEX_KEYS").map(PathBuf::from),
			offline:       match value("OMP_EXT_OFFLINE").as_deref() {
				Some("strict") => OfflineMode::Strict,
				Some(_) => OfflineMode::Offline,
				None => OfflineMode::Online,
			},
			locked:        bool_value("OMP_EXT_LOCKED"),
			exclude_newer: value("OMP_EXT_EXCLUDE_NEWER").map(Str::new),
			disabled:      comma("OMP_EXT_DISABLE").into_iter().collect(),
			no_workspace:  bool_value("OMP_EXT_NO_WORKSPACE"),
			grant:         value("OMP_EXT_GRANT"),
			allow_build:   bool_value("OMP_EXT_ALLOW_BUILD"),
			sign_key:      value("OMP_EXT_SIGN_KEY").map(PathBuf::from),
			uv:            value("OMP_EXT_UV").map(PathBuf::from),
			targets:       comma("OMP_EXT_TARGETS"),
			trace:         bool_value("OMP_EXT_TRACE"),
			env_socket:    value("OMP_EXT_ENV_SOCKET").map(PathBuf::from),
			site_override: value("OMP_PY_SITE").map(PathBuf::from),
		}
	}

	/// Returns the diagnostic emitted when an ambient site override bypasses
	/// managed per-host site-tree selection.
	#[must_use]
	pub const fn site_override_warning(&self) -> Option<ExtensionCode> {
		if self.site_override.is_some() {
			Some(ExtensionCode::WSiteOverride)
		} else {
			None
		}
	}
}
/// Static discovery locations for one layer, ordered per P2.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AmbientPaths {
	/// Directories containing manifests, in discovery order.
	pub manifest_roots:  Vec<PathBuf>,
	/// Config overlays, in discovery order.
	pub config_files:    Vec<PathBuf>,
	/// Local install records, in discovery order.
	pub install_records: Vec<PathBuf>,
	/// Compatibility roots that are reported but never loaded.
	pub foreign_roots:   Vec<PathBuf>,
}

/// Builds ambient discovery paths. Workspace paths are included on the
/// workspace side; callers do not invoke this for a remote workspace on the
/// client. Compatibility roots are diagnostic-only (`W-FOREIGN-ROOT`).
#[must_use]
pub fn ambient_paths(
	data_dir: &std::path::Path,
	workspace: Option<&std::path::Path>,
) -> AmbientPaths {
	let mut paths = AmbientPaths {
		manifest_roots:  Vec::new(),
		config_files:    vec![data_dir.join("config.toml")],
		install_records: vec![data_dir.join("ext/installed.toml")],
		foreign_roots:   Vec::new(),
	};
	if let Some(workspace) = workspace {
		let root = workspace.join(".omp");
		paths.manifest_roots.push(root.join("extensions"));
		paths.config_files.push(root.join("config.toml"));
		paths.install_records.push(root.join("installed.toml"));
		for name in [".claude", ".codex", ".gemini"] {
			paths.foreign_roots.push(workspace.join(name));
		}
	}
	paths
}

/// Outcome of the P4 workspace replacement gate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplacementDecision {
	/// The workspace instance is the sole active instance for this id.
	Replace,
	/// The client instance remains active and the workspace instance is omitted.
	Denied(ExtensionCode),
	/// No workspace replacement was requested.
	NotRequested,
}

/// Applies P4's declaration, publisher-match, and policy gates. A denial is
/// deterministic: callers retain or re-admit the client instance rather than
/// allowing both instances to coexist.
#[must_use]
pub fn workspace_replacement(
	replace_declared: bool,
	client_publisher: &Str,
	workspace_publisher: &Str,
	policy_permits: bool,
) -> ReplacementDecision {
	if !replace_declared {
		return ReplacementDecision::NotRequested;
	}
	if client_publisher != workspace_publisher || !policy_permits {
		return ReplacementDecision::Denied(ExtensionCode::WReplaceDenied);
	}
	ReplacementDecision::Replace
}

/// The authoring intent of one `[[tools]]` entry.
#[derive(
	Clone,
	Copy,
	Debug,
	Default,
	Eq,
	PartialEq,
	Deserialize,
	Serialize,
	strum::Display,
	strum::EnumString,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum ToolIntent {
	/// Catalog-routed tool declaration.
	#[default]
	Soft,
	/// Model-slot-claiming tool declaration gated by `tools.hard`.
	Hard,
}

/// One source `[[tools]]` manifest entry.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct ToolManifestEntry {
	/// Stable declaration id.
	pub id:     Str,
	/// Tool intent; defaults to soft.
	#[serde(default, rename = "kind")]
	pub intent: ToolIntent,
	/// Module imported when the tool activates.
	pub module: Str,
	/// Static route key.
	pub key:    Str,
	/// Required API level.
	pub api:    u32,
}

/// Uniform declaration consumed by static catalogs and lazy activation.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct Declaration {
	/// Stable declaration id.
	pub id:      Str,
	/// Closed declaration kind (`soft` or `hard` for this lowering).
	pub kind:    ToolIntent,
	/// Module imported on activation.
	pub module:  Str,
	/// Static route key.
	pub key:     Str,
	/// Tools always activate lazily from their static declarations.
	pub trigger: Str,
	/// Required OMP API level.
	pub api:     u32,
}

/// Lowers authoring `[[tools]]` entries into the static declaration table.
#[must_use]
pub fn lower_tools(tools: impl IntoIterator<Item = ToolManifestEntry>) -> Vec<Declaration> {
	tools
		.into_iter()
		.map(|tool| Declaration {
			id:      tool.id,
			kind:    tool.intent,
			module:  tool.module,
			key:     tool.key,
			trigger: sf!("lazy"),
			api:     tool.api,
		})
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn p7_negative_dominates_later_positive() {
		let id = sf!("acme.reviewer");
		let client = ScopedOverlay {
			scope:   Scope::Client,
			overlay: ExtensionOverlay {
				disabled: [id.clone()].into_iter().collect(),
				..ExtensionOverlay::default()
			},
		};
		let workspace = ScopedOverlay {
			scope:   Scope::Workspace,
			overlay: ExtensionOverlay {
				enabled: [id.clone()].into_iter().collect(),
				..ExtensionOverlay::default()
			},
		};
		let effective = fold_extension(&[client, workspace], &id);
		assert!(effective.disabled);
		assert!(!effective.enabled);
	}
}
