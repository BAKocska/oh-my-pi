//! Canonical static capability declarations and structured discovery
//! provenance.
//!
//! These types are deliberately data-only. Discovery may parse manifests and
//! hand their winning declarations to runtime owners, but it never imports or
//! executes an extension module.

use std::{
	collections::BTreeMap,
	path::{Path, PathBuf},
	sync::Arc,
};

use omp_core::{Str, sf};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use strum::{Display, EnumString, IntoStaticStr};

/// Static capability families understood by native OMP discovery.
#[derive(
	Clone,
	Copy,
	Debug,
	Display,
	EnumString,
	Eq,
	Hash,
	IntoStaticStr,
	Ord,
	PartialEq,
	PartialOrd,
	Serialize,
	Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case", ascii_case_insensitive)]
pub enum CapabilityKind {
	/// Markdown skill content.
	Skills,
	/// Declarative rules and constraints.
	Rules,
	/// Native Model Context Protocol server declarations.
	Mcps,
	/// Persistent context files.
	ContextFiles,
	/// Declared pre/post tool hooks.
	Hooks,
	/// Reusable prompt templates.
	Prompts,
	/// File-targeted instructions.
	Instructions,
	/// Static native extension manifests.
	Extensions,
	/// Markdown slash-command templates.
	SlashCommands,
	/// Static custom-tool declarations.
	Tools,
	/// Native settings contribution files.
	Settings,
	/// Native SSH host declarations.
	#[serde(rename = "ssh")]
	#[strum(to_string = "ssh", serialize = "ssh-hosts")]
	Ssh,
	/// System-prompt projections.
	SystemPrompt,
	/// Markdown subagent definitions parsed by `omp-agent`.
	Agents,
}

/// Authority which assigned a source revision.
#[derive(
	Clone,
	Copy,
	Debug,
	Display,
	EnumString,
	Eq,
	Hash,
	IntoStaticStr,
	Ord,
	PartialEq,
	PartialOrd,
	Serialize,
	Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case", ascii_case_insensitive)]
pub enum RevisionAuthority {
	/// Environment-owned resource or document revision.
	Environment,
	/// In-process document-server content revision.
	DocumentServer,
	/// Bounded walker result revision.
	Walker,
	/// Repository snapshot revision.
	Repository,
	/// Installed native-package generation.
	InstalledPackage,
}

/// Opaque, authority-scoped identity of immutable discovery input.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct CapabilityRevision {
	/// Revision authority vocabulary.
	pub authority:    RevisionAuthority,
	/// Opaque authority identity; consumers must not parse these bytes.
	pub authority_id: Arc<[u8]>,
	/// Monotone sequence within the authority identity.
	pub sequence:     u64,
	/// Exact authority-provided digest.
	pub digest:       [u8; 32],
}

/// Configuration level from which a declaration originated.
#[derive(
	Clone,
	Copy,
	Debug,
	Display,
	EnumString,
	Eq,
	Hash,
	IntoStaticStr,
	Ord,
	PartialEq,
	PartialOrd,
	Serialize,
	Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case", ascii_case_insensitive)]
pub enum SourceScope {
	/// Repository or workspace-authored configuration.
	Project,
	/// Explicit user/profile configuration.
	User,
	/// Installed native OMP package content.
	Package,
	/// Native source not otherwise scoped.
	Native,
	/// Bundled, lowest-precedence content.
	BuiltIn,
}

/// Source facts supplied by a provider before registry collation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SourceProvenance {
	/// Stable source identity used by source enablement settings.
	pub source_id:            Str,
	/// Canonical source document or manifest path.
	pub path:                 PathBuf,
	/// Source configuration level.
	pub scope:                SourceScope,
	/// Whether OMP must treat the source as read-only.
	pub read_only:            bool,
	/// Exact file/document/walker revision, when available.
	pub revision:             Option<CapabilityRevision>,
	/// Repository revision governing contextual interpretation of the source.
	pub repository_revision:  Option<CapabilityRevision>,
	/// Installed-package generation governing the source, when packaged.
	pub package_revision:     Option<CapabilityRevision>,
	/// Stable installed package identity, when packaged.
	pub installed_package_id: Option<Str>,
}

impl SourceProvenance {
	/// Constructs an unrevisioned native source. Providers should replace this
	/// with authority revisions whenever their input authority supplies them.
	#[must_use]
	pub fn native(source_id: impl Into<Str>, path: PathBuf, scope: SourceScope) -> Self {
		Self {
			source_id: source_id.into(),
			path,
			scope,
			read_only: false,
			revision: None,
			repository_revision: None,
			package_revision: None,
			installed_package_id: None,
		}
	}
}

/// Provider facts attached by the registry, never trusted from provider data.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderProvenance {
	/// Stable provider ID.
	pub id:           Str,
	/// Human-readable provider label.
	pub display_name: Str,
	/// Deterministic collation priority.
	pub priority:     i32,
}

/// Complete structured origin of a discovered declaration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CapabilityProvenance {
	/// Registry-owned provider facts.
	pub provider: ProviderProvenance,
	/// Provider-supplied source facts.
	pub source:   SourceProvenance,
}

/// Parsed skill frontmatter retained without re-reading the source file.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SkillFrontmatter {
	/// Optional declared description.
	pub description:              Option<Str>,
	/// Optional file applicability globs.
	#[serde(default)]
	pub globs:                    Vec<Str>,
	/// Whether the skill is always active.
	#[serde(default)]
	pub always_apply:             bool,
	/// Whether inventory rendering hides the skill.
	#[serde(default)]
	pub hidden:                   bool,
	/// Whether the model may select the skill itself.
	#[serde(default)]
	pub disable_model_invocation: bool,
}

/// Canonical parsed skill declaration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SkillPayload {
	/// Stable skill name.
	pub name:         Str,
	/// Canonical skill document path.
	pub path:         PathBuf,
	/// Markdown body.
	pub content:      Str,
	/// Typed frontmatter.
	#[serde(default)]
	pub frontmatter:  SkillFrontmatter,
	/// Optional containment root for packaged resources.
	pub contain_root: Option<PathBuf>,
}

/// TTSR interruption mode declared by a rule.
#[derive(
	Clone, Copy, Debug, Display, EnumString, Eq, IntoStaticStr, PartialEq, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case", ascii_case_insensitive)]
pub enum RuleInterruptMode {
	/// Never interrupt.
	Never,
	/// Interrupt only prose streams.
	ProseOnly,
	/// Interrupt only tool streams.
	ToolOnly,
	/// Interrupt every eligible stream.
	Always,
}

/// Canonical declarative rule.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RulePayload {
	/// Stable rule name.
	pub name:           Str,
	/// Canonical rule document path.
	pub path:           PathBuf,
	/// Markdown body.
	pub content:        Str,
	/// File applicability globs.
	#[serde(default)]
	pub globs:          Vec<Str>,
	/// Whether the rule is sticky.
	#[serde(default)]
	pub always_apply:   bool,
	/// Optional inventory description.
	pub description:    Option<Str>,
	/// Regex TTSR conditions.
	#[serde(default)]
	pub conditions:     Vec<Str>,
	/// ast-grep TTSR conditions.
	#[serde(default)]
	pub ast_conditions: Vec<Str>,
	/// Stream scope expressions.
	#[serde(default)]
	pub scopes:         Vec<Str>,
	/// Optional interruption override.
	pub interrupt_mode: Option<RuleInterruptMode>,
}

/// MCP request-ID encoding.
#[derive(
	Clone, Copy, Debug, Display, EnumString, Eq, IntoStaticStr, PartialEq, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case", ascii_case_insensitive)]
pub enum McpRequestIdFormat {
	/// Numeric JSON-RPC IDs.
	Number,
	/// String JSON-RPC IDs.
	String,
}

/// MCP transport declaration.
#[derive(
	Clone, Copy, Debug, Display, EnumString, Eq, IntoStaticStr, PartialEq, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case", ascii_case_insensitive)]
pub enum McpTransport {
	/// Child-process standard I/O.
	Stdio,
	/// Legacy HTTP plus server-sent events.
	Sse,
	/// Streamable HTTP.
	Http,
}

/// MCP environment-value interpretation policy.
#[derive(
	Clone, Copy, Debug, Display, EnumString, Eq, IntoStaticStr, PartialEq, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case", ascii_case_insensitive)]
pub enum McpEnvironmentPolicy {
	/// Values are opaque native-package data and are never command-expanded.
	Literal,
}

/// MCP configured-header forwarding policy.
#[derive(
	Clone, Copy, Debug, Display, EnumString, Eq, IntoStaticStr, PartialEq, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case", ascii_case_insensitive)]
pub enum McpHeaderPolicy {
	/// Headers remain literal and may only reach the configured URL origin.
	OriginLocked,
}

/// Static MCP authentication reference.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct McpAuth {
	/// Authentication mechanism.
	pub kind:          Str,
	/// Opaque credential authority ID.
	pub credential_id: Option<Str>,
	/// OAuth token endpoint, if explicitly declared.
	pub token_url:     Option<Str>,
	/// OAuth client ID.
	pub client_id:     Option<Str>,
	/// Secret reference, never resolved secret bytes.
	pub secret_ref:    Option<Str>,
	/// OAuth protected-resource identity.
	pub resource:      Option<Str>,
}

/// Explicit OAuth client and callback declaration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct McpOauth {
	/// OAuth client ID.
	pub client_id:     Option<Str>,
	/// Secret authority reference, never resolved secret bytes.
	pub secret_ref:    Option<Str>,
	/// Explicit redirect URI.
	pub redirect_uri:  Option<Str>,
	/// Loopback callback port.
	pub callback_port: Option<u16>,
	/// Loopback callback path.
	pub callback_path: Option<Str>,
	/// Optional authorization prompt behavior.
	pub prompt:        Option<Str>,
}

/// Canonical MCP connection identity. The payload holds this behind `Arc`:
/// one shared cold declaration instead of embedding several cloned maps and
/// credential-reference strings in every registry claim.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct McpConnection {
	/// JSON-RPC request ID encoding.
	pub request_id_format: Option<McpRequestIdFormat>,
	/// Optional stdio executable declaration.
	pub command:           Option<PathBuf>,
	/// Stdio arguments.
	#[serde(default)]
	pub args:              Vec<Str>,
	/// Literal or authority-resolved environment declarations.
	#[serde(default)]
	pub env:               BTreeMap<Str, Str>,
	/// Optional package-literal environment policy.
	pub env_policy:        Option<McpEnvironmentPolicy>,
	/// Optional stdio working directory.
	pub cwd:               Option<PathBuf>,
	/// HTTP/SSE endpoint.
	pub url:               Option<Str>,
	/// HTTP headers containing literals or credential references.
	#[serde(default)]
	pub headers:           BTreeMap<Str, Str>,
	/// Optional configured-header forwarding policy.
	pub header_policy:     Option<McpHeaderPolicy>,
	/// Typed authentication declaration.
	pub auth:              Option<McpAuth>,
	/// Explicit OAuth client and callback declaration.
	pub oauth:             Option<McpOauth>,
	/// Explicit transport, otherwise inferred from endpoint shape.
	pub transport:         Option<McpTransport>,
}

impl McpConnection {
	fn resolved_transport(&self) -> McpTransport {
		self.transport.unwrap_or(if self.command.is_some() {
			McpTransport::Stdio
		} else {
			McpTransport::Http
		})
	}

	fn semantically_equivalent(&self, other: &Self) -> bool {
		if self.auth != other.auth
			|| self.oauth != other.oauth
			|| self.request_id_format.unwrap_or(McpRequestIdFormat::Number)
				!= other
					.request_id_format
					.unwrap_or(McpRequestIdFormat::Number)
			|| self.resolved_transport() != other.resolved_transport()
		{
			return false;
		}
		match self.resolved_transport() {
			McpTransport::Stdio => {
				self.command == other.command
					&& self.args == other.args
					&& self.env == other.env
					&& self.cwd == other.cwd
			},
			McpTransport::Sse | McpTransport::Http => {
				self.url == other.url && self.headers == other.headers
			},
		}
	}
}

/// Canonical native MCP server declaration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct McpPayload {
	/// Server name and deduplication key.
	pub name:       Str,
	/// Whether the server is enabled.
	#[serde(default = "default_true")]
	pub enabled:    bool,
	/// Optional connection deadline in milliseconds.
	pub timeout_ms: Option<u64>,
	/// Shared connection identity.
	pub connection: Arc<McpConnection>,
}

impl McpPayload {
	fn same_connection(&self, other: &Self) -> bool {
		self.connection.semantically_equivalent(&other.connection)
	}
}

const fn default_true() -> bool {
	true
}

/// Canonical persistent context document.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextPayload {
	/// Canonical source path.
	pub path:    PathBuf,
	/// Markdown body.
	pub content: Str,
	/// Ancestor distance for project sources.
	pub depth:   Option<u16>,
}

/// Hook execution phase.
#[derive(
	Clone, Copy, Debug, Display, EnumString, Eq, IntoStaticStr, PartialEq, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case", ascii_case_insensitive)]
pub enum HookPhase {
	/// Before an admitted tool invocation.
	Pre,
	/// After a completed tool invocation.
	Post,
}

/// Static hook declaration. The hook path is data until the hook owner admits
/// it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HookPayload {
	/// Stable hook name.
	pub name:  Str,
	/// Canonical declared hook path.
	pub path:  PathBuf,
	/// Execution phase.
	pub phase: HookPhase,
	/// Tool name or `*` wildcard.
	pub tool:  Str,
}

/// Reusable prompt template.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PromptPayload {
	/// Stable prompt name.
	pub name:    Str,
	/// Canonical source path.
	pub path:    PathBuf,
	/// Markdown body.
	pub content: Str,
}

/// File-targeted instruction document.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstructionPayload {
	/// Stable instruction name.
	pub name:     Str,
	/// Canonical source path.
	pub path:     PathBuf,
	/// Markdown body.
	pub content:  Str,
	/// Optional applicability glob.
	pub apply_to: Option<Str>,
}

/// Supervised native Python worker declaration retained as inert manifest data.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PythonWorkerDeclaration {
	/// Package-contained Python module.
	pub module: Str,
	/// Optional named manifest entry point.
	pub entry:  Option<Str>,
}

/// Static native OMP extension manifest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExtensionPayload {
	/// Stable extension name.
	pub name:        Str,
	/// Canonical package root.
	pub root:        PathBuf,
	/// Human-readable description.
	pub description: Option<Str>,
	/// Supervised worker declaration, not imported by discovery.
	pub worker:      Option<PythonWorkerDeclaration>,
	/// Forward-compatible static manifest properties.
	/// Static CLI flag contributions discovered before final argument parsing.
	#[serde(default)]
	pub cli:         Vec<crate::ext::config::CliContribution>,
	#[serde(default)]
	pub manifest:    BTreeMap<Str, serde_json::Value>,
}

/// Markdown slash-command template.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CommandPayload {
	/// Name without the leading slash.
	pub name:        Str,
	/// Canonical source path.
	pub path:        PathBuf,
	/// Frontmatter description or first non-empty body line.
	#[serde(default)]
	pub description: Str,
	/// Markdown template body with frontmatter removed.
	pub content:     Str,
}

/// Static custom-tool execution declaration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ToolHandlerDeclaration {
	/// Supervised Python worker call target.
	Python {
		/// Package-contained module.
		module:   Str,
		/// Declared callable name.
		callable: Str,
	},
	/// Environment-authorized executable declaration.
	Process {
		/// Declared executable path.
		program: PathBuf,
		/// Static arguments.
		args:    Vec<Str>,
	},
}

/// Static custom-tool declaration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolPayload {
	/// Tool name.
	pub name:         Str,
	/// Canonical manifest path.
	pub path:         PathBuf,
	/// Tool description.
	pub description:  Str,
	/// Frozen JSON Schema input contract.
	pub input_schema: serde_json::Value,
	/// Declared handler, not activated by discovery.
	pub handler:      ToolHandlerDeclaration,
}

/// Parsed native settings contribution.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SettingsPayload {
	/// Canonical settings path.
	pub path: PathBuf,
	/// Parsed TOML table.
	pub data: toml::Table,
}

/// Native SSH host declaration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SshPayload {
	/// Stable host name.
	pub name:        Str,
	/// Address or DNS name.
	pub host:        Str,
	/// Optional username.
	pub username:    Option<Str>,
	/// Optional TCP port.
	pub port:        Option<u16>,
	/// Optional identity-key path declaration.
	pub key_path:    Option<PathBuf>,
	/// Human-readable description.
	pub description: Option<Str>,
	/// Compatibility mode requested by the native declaration.
	#[serde(default)]
	pub compat:      bool,
}

/// System-prompt projection document.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SystemPromptPayload {
	/// Canonical source path.
	pub path:    PathBuf,
	/// Markdown body.
	pub content: Str,
}

/// Static subagent definition source.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentPayload {
	/// Stable agent name.
	pub name:    Str,
	/// Canonical definition path.
	pub path:    PathBuf,
	/// Markdown declaration body.
	pub content: Str,
}

/// Typed capability declaration payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "declaration", rename_all = "kebab-case")]
pub enum CapabilityPayload {
	/// Skill declaration.
	Skills(SkillPayload),
	/// Rule declaration.
	Rules(RulePayload),
	/// MCP declaration.
	Mcps(McpPayload),
	/// Context-file declaration.
	ContextFiles(ContextPayload),
	/// Hook declaration.
	Hooks(HookPayload),
	/// Prompt declaration.
	Prompts(PromptPayload),
	/// Instruction declaration.
	Instructions(InstructionPayload),
	/// Native extension declaration.
	Extensions(ExtensionPayload),
	/// Slash-command declaration.
	SlashCommands(CommandPayload),
	/// Custom-tool declaration.
	Tools(ToolPayload),
	/// Settings contribution.
	Settings(SettingsPayload),
	/// SSH host declaration.
	#[serde(rename = "ssh")]
	Ssh(SshPayload),
	/// System-prompt declaration.
	SystemPrompt(SystemPromptPayload),
	/// Subagent definition declaration.
	Agents(AgentPayload),
}

const _: () =
	assert!(std::mem::size_of::<CapabilityPayload>() <= 256, "CapabilityPayload must stay compact");

impl CapabilityPayload {
	/// Returns this payload's static capability family.
	#[must_use]
	pub const fn kind(&self) -> CapabilityKind {
		match self {
			Self::Skills(_) => CapabilityKind::Skills,
			Self::Rules(_) => CapabilityKind::Rules,
			Self::Mcps(_) => CapabilityKind::Mcps,
			Self::ContextFiles(_) => CapabilityKind::ContextFiles,
			Self::Hooks(_) => CapabilityKind::Hooks,
			Self::Prompts(_) => CapabilityKind::Prompts,
			Self::Instructions(_) => CapabilityKind::Instructions,
			Self::Extensions(_) => CapabilityKind::Extensions,
			Self::SlashCommands(_) => CapabilityKind::SlashCommands,
			Self::Tools(_) => CapabilityKind::Tools,
			Self::Settings(_) => CapabilityKind::Settings,
			Self::Ssh(_) => CapabilityKind::Ssh,
			Self::SystemPrompt(_) => CapabilityKind::SystemPrompt,
			Self::Agents(_) => CapabilityKind::Agents,
		}
	}

	/// Returns payload-local enablement before source and provider policy.
	#[must_use]
	pub const fn declared_enabled(&self) -> bool {
		match self {
			Self::Mcps(value) => value.enabled,
			_ => true,
		}
	}

	/// Tests provider-independent semantic equivalence. At present MCP endpoint
	/// aliases are the canonical capability family with equivalence semantics.
	#[must_use]
	pub fn semantically_equivalent(&self, other: &Self) -> bool {
		matches!((self, other), (Self::Mcps(left), Self::Mcps(right)) if left.same_connection(right))
	}

	/// Validates required canonical fields without executing or resolving data.
	#[must_use]
	pub fn validation_issue(&self) -> Option<ValidationIssue> {
		let missing_name = |name: &Str| name.is_empty().then_some(ValidationIssue::MissingName);
		let missing_path = |path: &Path| {
			path
				.as_os_str()
				.is_empty()
				.then_some(ValidationIssue::MissingPath)
		};
		match self {
			Self::Skills(value) => missing_name(&value.name).or_else(|| missing_path(&value.path)),
			Self::Rules(value) => missing_name(&value.name).or_else(|| missing_path(&value.path)),
			Self::Mcps(value) => missing_name(&value.name)
				.or_else(|| {
					(value.connection.command.is_none() && value.connection.url.is_none())
						.then_some(ValidationIssue::MissingEndpoint)
				})
				.or_else(|| match value.connection.resolved_transport() {
					McpTransport::Stdio if value.connection.command.is_none() => {
						Some(ValidationIssue::TransportMismatch)
					},
					McpTransport::Sse | McpTransport::Http if value.connection.url.is_none() => {
						Some(ValidationIssue::TransportMismatch)
					},
					_ => None,
				}),
			Self::ContextFiles(value) => missing_path(&value.path),
			Self::Hooks(value) => missing_name(&value.name)
				.or_else(|| missing_path(&value.path))
				.or_else(|| {
					value
						.tool
						.is_empty()
						.then_some(ValidationIssue::MissingTool)
				}),
			Self::Prompts(value) => missing_name(&value.name).or_else(|| missing_path(&value.path)),
			Self::Instructions(value) => {
				missing_name(&value.name).or_else(|| missing_path(&value.path))
			},
			Self::Extensions(value) => missing_name(&value.name).or_else(|| missing_path(&value.root)),
			Self::SlashCommands(value) => {
				missing_name(&value.name).or_else(|| missing_path(&value.path))
			},
			Self::Tools(value) => missing_name(&value.name).or_else(|| missing_path(&value.path)),
			Self::Settings(value) => missing_path(&value.path),
			Self::Ssh(value) => missing_name(&value.name).or_else(|| {
				value
					.host
					.is_empty()
					.then_some(ValidationIssue::MissingHost)
			}),
			Self::SystemPrompt(value) => missing_path(&value.path),
			Self::Agents(value) => missing_name(&value.name).or_else(|| missing_path(&value.path)),
		}
	}
}

/// Provider-independent validation failures.
#[derive(
	Clone, Copy, Debug, Display, EnumString, Eq, IntoStaticStr, PartialEq, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case", ascii_case_insensitive)]
pub enum ValidationIssue {
	/// A keyed declaration omitted its name.
	MissingName,
	/// A file-backed declaration omitted its source path.
	MissingPath,
	/// An MCP declaration omitted every endpoint.
	MissingEndpoint,
	/// An MCP transport does not match its endpoint fields.
	TransportMismatch,
	/// A hook declaration omitted its tool selector.
	MissingTool,
	/// An SSH declaration omitted its host address.
	MissingHost,
}

/// Provider-returned declaration before registry-owned provenance is attached.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiscoveredCapability {
	/// Optional deduplication key; unkeyed settings contributions all survive.
	pub key:          Option<Str>,
	/// Optional provider-defined semantic identity for aliases.
	pub semantic_key: Option<Str>,
	/// Source-local enabled state. Disabled declarations still claim their key.
	pub enabled:      bool,
	/// Typed static payload.
	pub payload:      CapabilityPayload,
	/// Structured source provenance.
	pub source:       SourceProvenance,
}

impl DiscoveredCapability {
	/// Constructs a keyed, enabled declaration.
	#[must_use]
	pub fn keyed(key: impl Into<Str>, payload: CapabilityPayload, source: SourceProvenance) -> Self {
		Self { key: Some(key.into()), semantic_key: None, enabled: true, payload, source }
	}

	/// Constructs an unkeyed, enabled declaration.
	#[must_use]
	pub const fn unkeyed(payload: CapabilityPayload, source: SourceProvenance) -> Self {
		Self { key: None, semantic_key: None, enabled: true, payload, source }
	}
}

/// Registry-normalized declaration with registry-owned provider provenance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CapabilityRecord {
	/// Optional stable deduplication key.
	pub key:          Option<Str>,
	/// Optional provider-defined semantic identity.
	pub semantic_key: Option<Str>,
	/// Effective enabled state after source settings.
	pub enabled:      bool,
	/// Typed static payload.
	pub payload:      CapabilityPayload,
	/// Complete provider and source provenance.
	pub provenance:   CapabilityProvenance,
}

impl CapabilityRecord {
	/// Returns the static capability family.
	#[must_use]
	pub const fn kind(&self) -> CapabilityKind {
		self.payload.kind()
	}
}

/// A manifest-declared capability root, available without importing code.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CapabilityDeclaration {
	/// Stable declaration identity within its manifest.
	pub id:       Str,
	/// Content type exposed by this root.
	pub kind:     CapabilityKind,
	/// Filesystem root or static manifest payload location.
	pub root:     PathBuf,
	/// Larger values override lower-priority sources for the same item key.
	pub priority: i32,
}

/// One malformed or unreadable manifest-discovered agent definition.
#[derive(Debug)]
pub struct AgentDiscoveryWarning {
	/// Source file skipped during discovery.
	pub path: PathBuf,
	/// Typed failure retained without stringifying an inner error.
	pub kind: AgentDiscoveryWarningKind,
}

/// Typed agent discovery warning reason.
#[derive(Debug, thiserror::Error)]
pub enum AgentDiscoveryWarningKind {
	/// The definition file could not be read.
	#[error("agent definition could not be read")]
	Io(#[source] std::io::Error),
	/// The definition frontmatter was malformed.
	#[error("agent definition could not be parsed")]
	Parse(#[source] omp_agent::AgentDefinitionError),
}

/// First-wins agent definitions plus non-fatal source warnings.
#[derive(Debug, Default)]
pub struct AgentDiscovery {
	/// Definitions in manifest precedence order.
	pub definitions: Vec<(Str, omp_agent::AgentDefinition)>,
	/// Malformed definitions skipped without aborting discovery.
	pub warnings:    Vec<AgentDiscoveryWarning>,
}

/// Loads manifest-declared agent directories through the common static
/// capability table. File stems are stable definition keys and malformed
/// extension/project content is skipped with structured warning evidence.
pub fn discover_agents(declarations: &[CapabilityDeclaration]) -> AgentDiscovery {
	let ordered = priority_ordered(declarations.to_vec());
	let warnings = std::cell::RefCell::new(Vec::new());
	let definitions = dispatch_first(&ordered, CapabilityKind::Agents, |declaration| {
		agent_files(&declaration.root)
			.into_iter()
			.filter_map(|path| {
				let key = path
					.file_stem()
					.and_then(std::ffi::OsStr::to_str)
					.map(Str::from)?;
				let markdown = match std::fs::read_to_string(&path) {
					Ok(markdown) => markdown,
					Err(source) => {
						warnings.borrow_mut().push(AgentDiscoveryWarning {
							path,
							kind: AgentDiscoveryWarningKind::Io(source),
						});
						return None;
					},
				};
				match omp_agent::AgentDefinition::parse_markdown(key.clone(), &markdown) {
					Ok(definition) => Some((key, definition)),
					Err(source) => {
						warnings.borrow_mut().push(AgentDiscoveryWarning {
							path,
							kind: AgentDiscoveryWarningKind::Parse(source),
						});
						None
					},
				}
			})
			.collect()
	});
	AgentDiscovery { definitions, warnings: warnings.into_inner() }
}

/// Project agent definitions always outrank every other native tier.
pub const PROJECT_AGENT_PRIORITY: i32 = 400;
/// User agent definitions shadow native extensions and bundled roles.
pub const USER_AGENT_PRIORITY: i32 = 300;
/// Native OMP extension definitions shadow only the bundled catalog.
pub const EXTENSION_AGENT_PRIORITY: i32 = 200;
/// Bundled definitions are inserted after filesystem discovery.
pub const BUNDLED_AGENT_PRIORITY: i32 = 100;

/// Builds the native-only agent declaration tiers.
///
/// Foreign product roots are intentionally not accepted by this API. Extension
/// roots are already-authorized native OMP package roots and contribute only
/// their `agents` sibling.
#[must_use]
pub fn agent_declarations(
	project_root: &Path,
	user_home: Option<&Path>,
	extension_roots: &[PathBuf],
) -> Vec<CapabilityDeclaration> {
	let mut declarations = vec![CapabilityDeclaration {
		id:       sf!("project-agents"),
		kind:     CapabilityKind::Agents,
		root:     project_root.join(".omp/agents"),
		priority: PROJECT_AGENT_PRIORITY,
	}];
	if let Some(home) = user_home {
		declarations.push(CapabilityDeclaration {
			id:       sf!("user-agents"),
			kind:     CapabilityKind::Agents,
			root:     home.join(".omp/agents"),
			priority: USER_AGENT_PRIORITY,
		});
	}
	declarations.extend(extension_roots.iter().enumerate().map(|(index, root)| {
		CapabilityDeclaration {
			id:       sf!("extension-agents-{}", index),
			kind:     CapabilityKind::Agents,
			root:     root.join("agents"),
			priority: EXTENSION_AGENT_PRIORITY,
		}
	}));
	declarations
}

/// Canonical key for one immutable agent discovery generation.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct AgentDiscoveryCacheKey {
	/// Canonical working directory.
	pub cwd:                  PathBuf,
	/// Settings generation controlling disablement and overrides.
	pub settings_generation:  u64,
	/// Native extension installation generation.
	pub extension_generation: u64,
}

impl AgentDiscoveryCacheKey {
	/// Canonicalizes a working directory without making a missing root fatal.
	#[must_use]
	pub fn new(cwd: &Path, settings_generation: u64, extension_generation: u64) -> Self {
		Self {
			cwd: std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf()),
			settings_generation,
			extension_generation,
		}
	}
}

/// Per-working-directory immutable agent discovery cache.
#[derive(Default)]
pub struct AgentDiscoveryCache {
	entries: RwLock<BTreeMap<AgentDiscoveryCacheKey, Arc<AgentDiscovery>>>,
}

impl AgentDiscoveryCache {
	/// Returns a cached native discovery or inserts one exact generation.
	pub fn discover(
		&self,
		key: AgentDiscoveryCacheKey,
		declarations: &[CapabilityDeclaration],
	) -> Arc<AgentDiscovery> {
		if let Some(discovery) = self.entries.read().get(&key) {
			return Arc::clone(discovery);
		}
		let discovery = Arc::new(discover_agents(declarations));
		Arc::clone(self.entries.write().entry(key).or_insert(discovery))
	}

	/// Explicitly invalidates every generation for on-demand refresh.
	pub fn refresh(&self) {
		self.entries.write().clear();
	}
}

/// Filters disabled definitions and explicit provider selectors without
/// granting or rewriting any definition.
pub fn enabled_agents<'a>(
	discovery: &'a AgentDiscovery,
	disabled_agents: &'a [Str],
	disabled_providers: &'a [Str],
) -> impl Iterator<Item = &'a omp_agent::AgentDefinition> + 'a {
	discovery
		.definitions
		.iter()
		.filter_map(move |(_, definition)| {
			let disabled_agent = disabled_agents
				.iter()
				.any(|name| name.as_str().eq_ignore_ascii_case(definition.name.as_str()));
			let disabled_provider = definition.model.as_ref().is_some_and(|model| {
				let selector = model.as_str().trim_start_matches('@');
				let provider = selector.split_once('/').map(|(provider, _)| provider);
				provider.is_some_and(|provider| {
					disabled_providers
						.iter()
						.any(|disabled| disabled.as_str().eq_ignore_ascii_case(provider))
				})
			});
			(!disabled_agent && !disabled_provider).then_some(definition)
		})
}

fn agent_files(root: &Path) -> Vec<PathBuf> {
	if root.is_file() {
		return vec![root.to_path_buf()];
	}
	let mut paths = super::native::scan_capability_dir(root)
		.into_iter()
		.filter(|path| path.extension().and_then(std::ffi::OsStr::to_str) == Some("md"))
		.collect::<Vec<_>>();
	paths.sort();
	paths
}

/// Sorts declarations so dispatch is deterministic without registration order.
pub fn priority_ordered(
	mut declarations: Vec<CapabilityDeclaration>,
) -> Vec<CapabilityDeclaration> {
	declarations.sort_by(|left, right| {
		right
			.priority
			.cmp(&left.priority)
			.then_with(|| left.id.cmp(&right.id))
	});
	declarations
}

/// First-wins dispatch over static declarations of one kind. The caller owns
/// file parsing; its `key` must be the stable capability identity.
pub fn dispatch_first<'a, T>(
	declarations: &'a [CapabilityDeclaration],
	kind: CapabilityKind,
	mut load: impl FnMut(&'a CapabilityDeclaration) -> Vec<(Str, T)>,
) -> Vec<(Str, T)> {
	let mut claimed = std::collections::BTreeSet::new();
	let mut output = Vec::new();
	for declaration in declarations
		.iter()
		.filter(|declaration| declaration.kind == kind)
	{
		for (key, item) in load(declaration) {
			if claimed.insert(key.clone()) {
				output.push((key, item));
			}
		}
	}
	output
}

#[cfg(test)]
mod tests {
	use omp_core::sf;

	use super::*;

	#[test]
	fn static_priority_and_first_wins_dispatch() {
		let declarations = priority_ordered(vec![
			CapabilityDeclaration {
				id:       "low".into(),
				kind:     CapabilityKind::Skills,
				root:     PathBuf::new(),
				priority: 1,
			},
			CapabilityDeclaration {
				id:       "high".into(),
				kind:     CapabilityKind::Skills,
				root:     PathBuf::new(),
				priority: 2,
			},
		]);
		let entries = dispatch_first(&declarations, CapabilityKind::Skills, |declaration| {
			vec![("same".into(), declaration.id.clone())]
		});
		assert_eq!(entries, vec![("same".into(), sf!("high"))]);
	}

	#[test]
	fn every_canonical_kind_round_trips() {
		for kind in [
			CapabilityKind::Skills,
			CapabilityKind::Rules,
			CapabilityKind::Mcps,
			CapabilityKind::ContextFiles,
			CapabilityKind::Hooks,
			CapabilityKind::Prompts,
			CapabilityKind::Instructions,
			CapabilityKind::Extensions,
			CapabilityKind::SlashCommands,
			CapabilityKind::Tools,
			CapabilityKind::Settings,
			CapabilityKind::Ssh,
			CapabilityKind::SystemPrompt,
			CapabilityKind::Agents,
		] {
			let encoded = serde_json::to_string(&kind).expect("serialize kind");
			assert_eq!(serde_json::from_str::<CapabilityKind>(&encoded).expect("parse kind"), kind);
		}
	}
}
