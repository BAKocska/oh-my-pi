//! Command parsing and production dispatch for the `omp` executable.

use std::{
	ffi::OsString,
	fmt,
	io::IsTerminal as _,
	path::{Path, PathBuf},
	str::FromStr,
	sync::Arc,
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use clap::{Args, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use futures::StreamExt as _;
use miette::{IntoDiagnostic as _, miette};
use omp_core::Str;
use omp_llm_catalog::{ModelKey, compile::compile_oracle};
#[cfg(feature = "local-applefm")]
use omp_llm_inference::local::applefm::{AppleFm, AppleFmEvent, AppleFmOptions};
use omp_llm_inference::{
	Client,
	call::{
		CallMeta, ChatRequest, ContentPart, Message, NegotiationPolicy, Role, Sampling, Setting,
		Target,
	},
	event::ChatEvent,
	id::RequestId,
	receipt::ExecutionBudget,
};
use tokio::io::AsyncWriteExt as _;

use crate::{
	daemon::{DaemonConfig, DaemonHandle},
	endpoint::LocalEndpoint,
};

/// Validated reasoning effort accepted by launch-shaped commands.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
pub enum ThinkingLevel {
	/// Disable provider reasoning.
	Off,
	/// Smallest supported effort.
	Minimal,
	/// Low effort.
	Low,
	/// Default effort.
	Medium,
	/// High effort.
	High,
	/// Extreme effort.
	Extreme,
	/// Extra-high effort.
	XHigh,
	/// Maximum effort.
	Max,
	/// Leave effort selection to the provider.
	Auto,
}

impl FromStr for ThinkingLevel {
	type Err = String;

	fn from_str(value: &str) -> Result<Self, Self::Err> {
		let value = value.to_ascii_lowercase();
		let levels = [
			("off", Self::Off),
			("minimal", Self::Minimal),
			("low", Self::Low),
			("medium", Self::Medium),
			("high", Self::High),
			("extreme", Self::Extreme),
			("xhigh", Self::XHigh),
			("max", Self::Max),
			("auto", Self::Auto),
		];
		let matches = levels
			.into_iter()
			.filter(|(name, _)| name.starts_with(&value))
			.collect::<Vec<_>>();
		match matches.as_slice() {
			[(_, level)] => Ok(*level),
			[] if value == "inherit" => Err("`inherit` is not valid for --thinking".into()),
			[] => Err(format!("unknown thinking level `{value}`")),
			_ => Err(format!("ambiguous thinking level `{value}`")),
		}
	}
}

/// Validated provider service tier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceTier {
	/// Disable provider tier routing.
	None,
	/// Let the provider choose a tier.
	Auto,
	/// Use the provider default tier.
	Default,
	/// Select the flex tier.
	Flex,
	/// Select the priority tier.
	Priority,
	/// Select the scale tier.
	Scale,
	/// Select the standard tier.
	Standard,
}

impl FromStr for ServiceTier {
	type Err = String;

	fn from_str(value: &str) -> Result<Self, Self::Err> {
		match value {
			"none" => Ok(Self::None),
			"auto" => Ok(Self::Auto),
			"default" => Ok(Self::Default),
			"flex" => Ok(Self::Flex),
			"priority" => Ok(Self::Priority),
			"scale" => Ok(Self::Scale),
			"standard" => Ok(Self::Standard),
			_ => Err(format!("unknown service tier `{value}`")),
		}
	}
}

/// Validated policy for tool approval requests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalMode {
	/// Ask before every tool action.
	AlwaysAsk,
	/// Auto-approve workspace writes only.
	Write,
	/// Auto-approve all permitted actions.
	Yolo,
}

impl FromStr for ApprovalMode {
	type Err = String;

	fn from_str(value: &str) -> Result<Self, Self::Err> {
		match value {
			"always-ask" => Ok(Self::AlwaysAsk),
			"write" => Ok(Self::Write),
			"yolo" => Ok(Self::Yolo),
			_ => Err(format!("unknown approval mode `{value}`")),
		}
	}
}

/// A strictly positive launch duration parsed from seconds or `s`, `m`, `h`
/// suffixes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CliDuration(pub Duration);

impl FromStr for CliDuration {
	type Err = String;

	fn from_str(value: &str) -> Result<Self, Self::Err> {
		let (number, multiplier) = match value.as_bytes().last() {
			Some(b's') => (&value[..value.len() - 1], 1),
			Some(b'm') => (&value[..value.len() - 1], 60),
			Some(b'h') => (&value[..value.len() - 1], 3_600),
			_ => (value, 1),
		};
		let seconds = number
			.parse::<u64>()
			.map_err(|_| "duration must be seconds or use s, m, or h".to_owned())?;
		if seconds == 0 {
			return Err("duration must be greater than zero".into());
		}
		seconds
			.checked_mul(multiplier)
			.map(Duration::from_secs)
			.map(Self)
			.ok_or_else(|| "duration is too large".into())
	}
}

impl fmt::Display for CliDuration {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(formatter, "{}s", self.0.as_secs())
	}
}

/// Logical model role used to cycle a filtered catalog list.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelRole {
	/// First matching catalog model.
	Primary,
	/// Second matching catalog model.
	Smol,
	/// Third matching catalog model.
	Slow,
	/// Fourth matching catalog model.
	Plan,
}

impl FromStr for ModelRole {
	type Err = String;

	fn from_str(value: &str) -> Result<Self, Self::Err> {
		match value {
			"primary" => Ok(Self::Primary),
			"smol" => Ok(Self::Smol),
			"slow" => Ok(Self::Slow),
			"plan" => Ok(Self::Plan),
			_ => Err(format!("unknown model role `{value}`")),
		}
	}
}

/// Normalized comma-separated tool names accepted by launch-shaped commands.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolNames(
	/// Ordered normalized tool names.
	pub Vec<Str>,
);

impl FromStr for ToolNames {
	type Err = String;

	fn from_str(value: &str) -> Result<Self, Self::Err> {
		let names = value.split(',').map(str::trim).collect::<Vec<_>>();
		if names.is_empty()
			|| names.iter().any(|name| {
				name.is_empty()
					|| !name
						.bytes()
						.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
			}) {
			return Err("tools must be a non-empty comma-separated list of tool names".into());
		}
		Ok(Self(names.into_iter().map(Str::from).collect()))
	}
}

pub mod bootstrap;
pub mod profile_bootstrap;
pub mod routing;
/// Non-empty comma-separated selector list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectorList(
	/// Ordered selectors.
	pub Vec<Str>,
);

impl FromStr for SelectorList {
	type Err = String;

	fn from_str(value: &str) -> Result<Self, Self::Err> {
		let values = value.split(',').map(str::trim).collect::<Vec<_>>();
		if values.is_empty() || values.iter().any(|value| value.is_empty()) {
			return Err("expected a non-empty comma-separated list".into());
		}
		Ok(Self(values.into_iter().map(Str::from).collect()))
	}
}

/// Top-level parser for the production `omp` executable.
#[derive(Clone, Debug, Parser)]
#[command(
	name = "omp",
	version,
	about = "OMP coding agent and inference runtime",
	after_long_help = crate::help_extra::render()
)]
pub struct OmpCli {
	/// Enable an extension specification for this invocation.
	#[arg(long, global = true, value_name = "SPEC", conflicts_with = "no_ext")]
	pub ext:               Vec<Str>,
	/// Load only this local extension path for this invocation.
	#[arg(long = "ext-only", global = true, value_name = "PATH", conflicts_with = "no_ext")]
	pub ext_only:          Vec<PathBuf>,
	/// Load exactly these absolute Python modules through the trusted
	/// supervisor.
	#[arg(
		long = "trusted-extension",
		global = true,
		value_name = "ABSOLUTE_PATH",
		value_parser = trusted_extension_path,
		conflicts_with_all = ["ext", "ext_only", "no_ext"]
	)]
	pub trusted_extension: Vec<PathBuf>,
	/// Suppress all configured extensions for this invocation.
	#[arg(
		long = "no-ext",
		global = true,
		conflicts_with_all = ["ext", "ext_only", "trusted_extension"]
	)]
	pub no_ext:            bool,
	/// Suppress the workspace extension layer for this invocation.
	#[arg(long = "no-workspace-ext", global = true)]
	pub no_workspace_ext:  bool,
	/// Export one durable session journal to a self-contained HTML file and
	/// exit.
	#[arg(long, global = true, value_name = "SESSION_JSONL")]
	pub export:            Option<PathBuf>,
	/// Operation to run. Defaults to interactive project chat.
	#[command(subcommand)]
	pub command:           Option<Command>,
	/// Change to this project directory before dispatch.
	#[arg(long, global = true, value_name = "PATH")]
	pub cwd:               Option<PathBuf>,
	/// Permit running interactively from the home directory.
	#[arg(long, global = true)]
	pub allow_home:        bool,
	/// Select a named profile before settings and extensions are loaded.
	#[arg(skip)]
	pub profile:           Option<Str>,
	/// Install a shell wrapper for the selected profile and exit.
	#[arg(skip)]
	pub alias:             Option<Str>,
	/// Run deterministic native subsystem probes before chat startup.
	#[arg(long, global = true)]
	pub smoke_test:        bool,
	/// Typed contributed values excluded from prompt positionals.
	#[arg(skip)]
	pub contributed:       Vec<bootstrap::ContributedCliValue>,
}

/// Production application commands.
/// Statistics dashboard and JSON query options.
#[derive(Clone, Debug, Args)]
pub struct StatsArgs {
	/// Override the profile state directory containing `sessions.sqlite3`.
	#[arg(long, value_name = "PATH")]
	pub state_dir: Option<PathBuf>,
	/// Statistics operation; omitted prints a concise 30-day summary.
	#[command(subcommand)]
	pub command:   Option<StatsCommand>,
}

/// Statistics service operations.
#[derive(Clone, Debug, Subcommand)]
pub enum StatsCommand {
	/// Print a concise summary from the authoritative write-time index.
	Summary {
		/// Time range: 24h, 7d, 30d, 90d, or all.
		#[arg(long)]
		range: Option<String>,
	},
	/// Serve the embedded read-only dashboard and versioned REST API.
	Serve {
		/// IP address to bind; non-loopback addresses require authentication.
		#[arg(long, default_value = "127.0.0.1")]
		host:       String,
		/// TCP port; zero requests an ephemeral port.
		#[arg(long, default_value_t = crate::stats_server::DEFAULT_PORT)]
		port:       u16,
		/// Bearer token required for non-loopback service access.
		#[arg(long)]
		auth_token: Option<String>,
		/// Do not open the dashboard in the default browser.
		#[arg(long)]
		no_open:    bool,
	},
	/// Print the API overview envelope as JSON.
	Json {
		/// Time range: 24h, 7d, 30d, 90d, or all.
		#[arg(long, default_value = "30d")]
		range: String,
	},
	/// Serialize a manual write-time index synchronization checkpoint.
	Sync,
}

/// Lock-safe storage maintenance options.
#[derive(Clone, Debug, Args)]
pub struct GcArgs {
	/// Override the profile data directory.
	#[arg(long, value_name = "PATH")]
	pub data_dir:                Option<PathBuf>,
	/// Override the session-journal directory.
	#[arg(long, value_name = "PATH")]
	pub sessions_dir:            Option<PathBuf>,
	/// Override the authoritative sessions index.
	#[arg(long, value_name = "SQLITE")]
	pub index:                   Option<PathBuf>,
	/// Apply destructive operations; omission is a dry run.
	#[arg(long)]
	pub apply:                   bool,
	/// Gzip cold journals and move their artifact directories.
	#[arg(long)]
	pub archive:                 bool,
	/// Minimum inactive age in days for cold archives.
	#[arg(long, default_value_t = 30)]
	pub cold_archive_after_days: u64,
	/// Protect this many newest sessions globally.
	#[arg(long, default_value_t = 20)]
	pub retain_newest_global:    usize,
	/// Protect this many newest sessions per working directory.
	#[arg(long, default_value_t = 3)]
	pub retain_newest_per_cwd:   usize,
	/// Blob put-before-journal grace period.
	#[arg(long, default_value_t = 300)]
	pub min_age_seconds:         u64,
	/// Truncate SQLite WAL files after maintenance.
	#[arg(long)]
	pub wal:                     bool,
	/// Emit machine-readable JSON.
	#[arg(long)]
	pub json:                    bool,
}

/// Durable quota-history options.
#[derive(Clone, Debug, Args)]
pub struct UsageArgs {
	/// Override the profile data directory containing `credentials.db`.
	#[arg(long, value_name = "PATH")]
	pub data_dir:   Option<PathBuf>,
	/// Restrict snapshots to one provider.
	#[arg(long)]
	pub provider:   Option<Str>,
	/// Restrict snapshots to one opaque account identifier.
	#[arg(long)]
	pub account:    Option<Str>,
	/// Explicitly invalidate matching durable usage observations.
	#[arg(long)]
	pub invalidate: bool,
	/// Emit machine-readable JSON.
	#[arg(long)]
	pub json:       bool,
}

/// Normal inference benchmark options.
#[derive(Clone, Debug, Args)]
pub struct BenchArgs {
	/// Model key routed through the production inference registry.
	pub model:      Str,
	/// Override the profile data directory containing credentials.
	#[arg(long, value_name = "PATH")]
	pub data_dir:   Option<PathBuf>,
	/// Number of measured requests.
	#[arg(long, default_value_t = 10)]
	pub runs:       u32,
	/// Maximum output tokens per request.
	#[arg(long, default_value_t = 512)]
	pub max_tokens: u64,
	/// Benchmark prompt.
	#[arg(long, default_value = "Reply with one concise sentence about deterministic systems.")]
	pub prompt:     Str,
	/// Maximum concurrent requests.
	#[arg(long, default_value_t = 4)]
	pub par:        usize,
	/// Emit machine-readable JSON.
	#[arg(long)]
	pub json:       bool,
}

/// Deterministic OAuth account-pool simulation options.
#[derive(Clone, Debug, Args)]
pub struct DryBalanceArgs {
	/// Optional model selector; defaults to the first catalog model.
	pub model:       Option<Str>,
	/// Override the profile data directory containing credentials.
	#[arg(long, value_name = "PATH")]
	pub data_dir:    Option<PathBuf>,
	/// Number of selection samples.
	#[arg(long, default_value_t = 100)]
	pub count:       u32,
	/// Maximum live benchmark concurrency.
	#[arg(long, default_value_t = 32)]
	pub concurrency: usize,
	/// Send live completion requests after the simulation.
	#[arg(long)]
	pub bench:       bool,
	/// Emit machine-readable JSON.
	#[arg(long)]
	pub json:        bool,
}

/// Verified local tiny-model operator options.
#[derive(Clone, Debug, Args)]
pub struct TinyModelsArgs {
	/// Override the verified local-model cache root.
	#[arg(long, value_name = "PATH")]
	pub cache_dir: Option<PathBuf>,
	/// Tiny-model operation; omitted lists the catalog.
	#[command(subcommand)]
	pub command:   Option<TinyModelsCommand>,
}

/// Tiny-model catalog operations.
#[derive(Clone, Debug, Subcommand)]
pub enum TinyModelsCommand {
	/// List declared title and Mnemopi-only assets.
	List {
		/// Emit machine-readable JSON.
		#[arg(long)]
		json: bool,
	},
	/// Verify one model or every declared model.
	Verify {
		/// Stable model identifier.
		model: Option<String>,
		/// Emit machine-readable JSON.
		#[arg(long)]
		json:  bool,
	},
	/// Download one model or `all`, verify it, and atomically install it.
	Download {
		/// Stable model identifier or `all`.
		#[arg(default_value = "all")]
		model: String,
		/// Emit machine-readable JSON.
		#[arg(long)]
		json:  bool,
		/// Suppress transient progress.
		#[arg(long)]
		quiet: bool,
	},
}

/// Standalone onboarding and local-runtime setup options.
#[derive(Clone, Debug, Args)]
pub struct SetupArgs {
	/// Override the profile data directory.
	#[arg(long, value_name = "PATH")]
	pub data_dir: Option<PathBuf>,
	/// Setup operation; omitted runs onboarding.
	#[command(subcommand)]
	pub command:  Option<SetupCommand>,
}

/// Standalone setup operations.
#[derive(Clone, Debug, Subcommand)]
pub enum SetupCommand {
	/// Run provider/model onboarding.
	Wizard,
	/// Validate the supervised embedded Python runtime.
	Python {
		/// Emit machine-readable JSON.
		#[arg(long)]
		json: bool,
	},
	/// Inspect or download local STT/TTS assets.
	Speech {
		/// STT preset (`fast`, `balanced`, `turbo`, `parakeet`) or `kokoro`.
		model: Option<String>,
		/// Check every speech artifact without downloading.
		#[arg(long, short = 'c')]
		check: bool,
		/// Emit machine-readable JSON.
		#[arg(long)]
		json:  bool,
		/// Suppress transient progress.
		#[arg(long)]
		quiet: bool,
	},
}

/// Standalone Kokoro synthesis options.
#[derive(Clone, Debug, Args)]
pub struct SayArgs {
	/// Text to synthesize.
	pub text:            Option<Str>,
	/// Read text from a UTF-8 file instead of the positional argument.
	#[arg(long, value_name = "PATH", conflicts_with = "text")]
	pub file:            Option<PathBuf>,
	/// Override the profile data directory containing model assets.
	#[arg(long, value_name = "PATH")]
	pub data_dir:        Option<PathBuf>,
	/// Kokoro voice identifier.
	#[arg(long)]
	pub voice:           Option<String>,
	/// Stable local TTS model identifier.
	#[arg(long)]
	pub model:           Option<String>,
	/// Speaking-rate multiplier.
	#[arg(long, default_value_t = 1.0)]
	pub speed:           f32,
	/// Maximum approximate characters per synthesis pass.
	#[arg(long, default_value_t = 400)]
	pub max_chunk_chars: usize,
	/// Remove decoder noise for repeatable output.
	#[arg(long)]
	pub deterministic:   bool,
	/// Atomically write PCM16 WAV instead of playing through the default
	/// speaker.
	#[arg(long = "out", visible_alias = "output", short = 'o', value_name = "WAV")]
	pub output:          Option<PathBuf>,
}

/// Standalone native grep options.
#[derive(Clone, Debug, Args)]
pub struct GrepArgs {
	/// Rust/PCRE2 regular expression.
	pub pattern:      Str,
	/// File or directory to search.
	#[arg(default_value = ".")]
	pub path:         PathBuf,
	/// Recursive file glob.
	#[arg(short = 'g', long)]
	pub glob:         Option<Str>,
	/// Maximum returned matches.
	#[arg(short = 'l', long, default_value_t = 20)]
	pub limit:        u32,
	/// Context lines before and after each match.
	#[arg(short = 'C', long, default_value_t = 2)]
	pub context:      u32,
	/// Return matching file names only.
	#[arg(short = 'f', long, conflicts_with = "count")]
	pub files:        bool,
	/// Return match counts per file.
	#[arg(short = 'c', long)]
	pub count:        bool,
	/// Match without regard to ASCII case.
	#[arg(short = 'i', long)]
	pub ignore_case:  bool,
	/// Enable multiline matching.
	#[arg(long)]
	pub multiline:    bool,
	/// Include dot-prefixed paths.
	#[arg(long, default_value_t = true)]
	pub hidden:       bool,
	/// Ignore repository ignore files.
	#[arg(long)]
	pub no_gitignore: bool,
	/// Operation deadline in milliseconds.
	#[arg(long)]
	pub timeout_ms:   Option<u32>,
	/// Emit machine-readable JSON.
	#[arg(long)]
	pub json:         bool,
}

/// Stream category used by standalone TTSR matching.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum TtsrSourceArg {
	/// Assistant visible text.
	#[default]
	Text,
	/// Assistant reasoning text.
	Thinking,
	/// Tool snapshot text.
	Tool,
}

/// Standalone TTSR options.
#[derive(Clone, Debug, Args)]
pub struct TtsrArgs {
	/// Workspace root used for rule discovery.
	#[arg(long, value_name = "PATH")]
	pub root:    Option<PathBuf>,
	/// TTSR operation; omitted lists active rules.
	#[command(subcommand)]
	pub command: Option<TtsrCommand>,
}

/// TTSR inspection and matching operations.
#[derive(Clone, Debug, Subcommand)]
pub enum TtsrCommand {
	/// List active rules.
	List {
		/// Emit machine-readable JSON.
		#[arg(long)]
		json: bool,
	},
	/// Test a snippet, file, or standard input.
	Test {
		/// Inline snippet; omit with `--file -` to read standard input.
		snippet: Option<String>,
		/// File to inspect, or `-` for standard input.
		#[arg(long, short = 'f')]
		file:    Option<PathBuf>,
		/// Restrict reported matches to one rule name.
		#[arg(long, short = 'r')]
		rule:    Option<String>,
		/// Stream category.
		#[arg(long, value_enum, default_value_t)]
		source:  TtsrSourceArg,
		/// Tool name for tool-stream matching.
		#[arg(long, default_value = "edit")]
		tool:    String,
		/// Candidate path used by glob and AST-language matching.
		#[arg(long, short = 'p')]
		path:    Option<String>,
		/// Include matched reminder content.
		#[arg(long, short = 'v')]
		verbose: bool,
		/// Emit machine-readable JSON.
		#[arg(long)]
		json:    bool,
	},
	/// Scan a directory with native walker ignore semantics.
	Scan {
		/// Directory to scan.
		#[arg(default_value = ".")]
		directory:    PathBuf,
		/// Restrict reported matches to one rule name.
		#[arg(long, short = 'r')]
		rule:         Option<String>,
		/// Ignore repository ignore files.
		#[arg(long)]
		no_gitignore: bool,
		/// Maximum bytes read from any candidate.
		#[arg(long, default_value_t = 4 * 1024 * 1024)]
		max_bytes:    u64,
		/// Emit machine-readable JSON.
		#[arg(long)]
		json:         bool,
	},
}

/// Core updater options.
#[derive(Clone, Debug, Args)]
pub struct UpdateArgs {
	/// Only report whether a newer signed release is available.
	#[arg(long)]
	pub check:     bool,
	/// Reinstall even when the selected release matches this binary.
	#[arg(long)]
	pub force:     bool,
	/// Upgrade extensions instead; equivalent to `omp ext upgrade`.
	#[arg(long)]
	pub plugins:   bool,
	/// Signed package-index snapshot. Defaults to `OMP_RELEASE_INDEX`.
	#[arg(long, value_name = "JSON")]
	pub index:     Option<PathBuf>,
	/// Ed25519 index authority key file. Defaults to `OMP_RELEASE_INDEX_KEY`.
	#[arg(long, value_name = "KEY")]
	pub index_key: Option<PathBuf>,
}

/// Read-only signed package registry options.
#[derive(Clone, Debug, Args)]
pub struct RegistryArgs {
	/// Signed package-index snapshot. Defaults to `OMP_RELEASE_INDEX`.
	#[arg(long, value_name = "JSON")]
	pub index:     Option<PathBuf>,
	/// Ed25519 index authority key file. Defaults to `OMP_RELEASE_INDEX_KEY`.
	#[arg(long, value_name = "KEY")]
	pub index_key: Option<PathBuf>,
	/// Package identity to inspect.
	#[arg(long, default_value = "omp-cli")]
	pub package:   Str,
	/// Emit machine-readable JSON.
	#[arg(long)]
	pub json:      bool,
}

/// Encrypted transcript sharing options.
#[derive(Clone, Debug, Args)]
pub struct ShareArgs {
	/// Durable session journal. Omit to choose from the native session index.
	#[arg(value_name = "SESSION_JSONL")]
	pub journal:   Option<PathBuf>,
	/// HTTPS blob-store endpoint accepting the sealed envelope.
	#[arg(long, value_name = "URL")]
	pub server:    Option<Str>,
	/// Browser viewer base URL.
	#[arg(long, value_name = "URL", default_value = "https://omp.dev/share")]
	pub viewer:    Str,
	/// Disable irreversible secret redaction.
	#[arg(long)]
	pub no_redact: bool,
}

/// Production application commands.
#[derive(Clone, Debug, Subcommand)]
pub enum Command {
	/// Start the inference gateway on a platform-native local endpoint.
	Serve(ServeArgs),
	/// Start the project environment daemon.
	Envd(EnvdArgs),
	/// Start an interactive project agent session.
	#[command(alias = "i", alias = "launch")]
	Chat(ChatArgs),
	/// Run a single prompt and stream its response to standard output.
	#[command(alias = "p")]
	Print(PrintArgs),
	/// Run the stateful Content-Length framed RPC server on standard I/O.
	Rpc(RpcArgs),
	/// Run RPC with retained UI frame support.
	#[command(name = "rpc-ui")]
	RpcUi(RpcArgs),
	/// Run the Agent Client Protocol server over newline-delimited JSON.
	Acp(AcpArgs),
	/// Run one typed operation in process.
	Infer(InferArgs),
	/// Manage provider credentials.
	Auth(AuthArgs),
	/// Manage generated model-catalog data.
	Catalog(CatalogArgs),
	/// Run hardware-accelerated local inference.
	Local(LocalArgs),
	/// Manage Python extension resolution, trust, and site trees.
	Ext(crate::ext_cli::ExtArgs),
	/// Inspect or update the schema-validated application configuration.
	Config(ConfigArgs),
	/// Check or install a signed native OMP release.
	Update(UpdateArgs),
	/// Inspect the signed native package registry and platform assets.
	Registry(RegistryArgs),
	/// Redact, encrypt, and upload a durable transcript projection.
	Share(ShareArgs),
	/// Inspect models from the validated embedded catalog.
	#[command(alias = "model")]
	Models(ModelsArgs),
	/// Inspect or clear Environment-owned worktrees.
	Worktree(WorktreeArgs),
	/// Inspect usage statistics or serve the embedded dashboard.
	Stats(StatsArgs),
	/// Inspect or apply lock-safe session and blob maintenance.
	Gc(GcArgs),
	/// Inspect or invalidate durable provider quota observations.
	Usage(UsageArgs),
	/// Benchmark model TTFT, throughput, concurrency, and cold/warm cache pairs.
	Bench(BenchArgs),
	/// Simulate account selection and optionally run a live balance benchmark.
	#[command(name = "dry-balance")]
	DryBalance(DryBalanceArgs),
	/// Manage verified local title and Mnemopi assets.
	#[command(name = "tiny-models")]
	TinyModels(TinyModelsArgs),
	/// Run onboarding, Python checks, or speech asset setup.
	Setup(SetupArgs),
	/// Synthesize text with local Kokoro and play or export it.
	Say(SayArgs),
	/// Run the native grep engine as a standalone operator.
	Grep(GrepArgs),
	/// Manage scoped native SSH hosts and run bounded client operations.
	Ssh(crate::ssh_cmd::SshArgs),
	/// Inspect and test active Time-Traveling Stream Rules.
	Ttsr(TtsrArgs),
	/// Generate a static shell completion script.
	Completions {
		/// Target shell.
		#[arg(value_enum)]
		shell: CompletionShell,
	},
	/// Emit dynamic model or session completion candidates.
	#[command(name = "__complete", hide = true)]
	Complete {
		/// Candidate class.
		#[arg(value_enum)]
		kind:   crate::complete_cmd::CompletionKind,
		/// Optional fuzzy prefix after `--`.
		#[arg(last = true, default_value = "")]
		prefix: Str,
	},
	/// Auth-broker verbs are retained as structured errors until a broker
	/// backend lands.
	#[command(name = "auth-broker")]
	AuthBroker(AuthBrokerArgs),
	/// Operate the credential-injecting inference gateway.
	#[command(name = "auth-gateway")]
	AuthGateway(AuthGatewayArgs),
}

/// Shell completion target.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum CompletionShell {
	/// Bash.
	Bash,
	/// Z shell.
	Zsh,
	/// Fish.
	Fish,
}

impl From<CompletionShell> for Shell {
	fn from(value: CompletionShell) -> Self {
		match value {
			CompletionShell::Bash => Self::Bash,
			CompletionShell::Zsh => Self::Zsh,
			CompletionShell::Fish => Self::Fish,
		}
	}
}

/// Inspect and prune Environment-owned isolated worktrees.
#[derive(Clone, Debug, Args)]
pub struct WorktreeArgs {
	/// Worktree inventory or cleanup operation.
	#[command(subcommand)]
	pub command: WorktreeCommand,
}

/// Worktree inventory and cleanup verbs.
#[derive(Clone, Debug, Subcommand)]
pub enum WorktreeCommand {
	/// List classified worktrees.
	List {
		/// Emit machine-readable JSON.
		#[arg(long)]
		json: bool,
		/// Include unregistered stray directories.
		#[arg(long)]
		all:  bool,
	},
	/// Remove orphaned worktrees, or every worktree with `--all`.
	Clear {
		/// Remove live worktrees as well as orphans.
		#[arg(long)]
		all:     bool,
		/// Report without deleting.
		#[arg(long)]
		dry_run: bool,
		/// Emit machine-readable JSON.
		#[arg(long)]
		json:    bool,
	},
}

/// Declarative root-command metadata shared by help and command normalization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandSpec {
	/// Canonical root verb.
	pub name:    &'static str,
	/// Accepted aliases for the root verb.
	pub aliases: &'static [&'static str],
}

/// Complete registry for the commands implemented by this binary.
pub const COMMAND_REGISTRY: &[CommandSpec] = &[
	CommandSpec { name: "serve", aliases: &[] },
	CommandSpec { name: "envd", aliases: &[] },
	CommandSpec { name: "chat", aliases: &["i", "launch"] },
	CommandSpec { name: "print", aliases: &["p"] },
	CommandSpec { name: "infer", aliases: &[] },
	CommandSpec { name: "rpc", aliases: &[] },
	CommandSpec { name: "rpc-ui", aliases: &[] },
	CommandSpec { name: "acp", aliases: &[] },
	CommandSpec { name: "auth", aliases: &[] },
	CommandSpec { name: "auth-broker", aliases: &[] },
	CommandSpec { name: "auth-gateway", aliases: &[] },
	CommandSpec { name: "catalog", aliases: &[] },
	CommandSpec { name: "local", aliases: &[] },
	CommandSpec { name: "ext", aliases: &[] },
	CommandSpec { name: "config", aliases: &[] },
	CommandSpec { name: "update", aliases: &[] },
	CommandSpec { name: "registry", aliases: &[] },
	CommandSpec { name: "share", aliases: &[] },
	CommandSpec { name: "models", aliases: &["model"] },
	CommandSpec { name: "worktree", aliases: &[] },
	CommandSpec { name: "stats", aliases: &[] },
	CommandSpec { name: "gc", aliases: &[] },
	CommandSpec { name: "usage", aliases: &[] },
	CommandSpec { name: "bench", aliases: &[] },
	CommandSpec { name: "dry-balance", aliases: &[] },
	CommandSpec { name: "tiny-models", aliases: &[] },
	CommandSpec { name: "setup", aliases: &[] },
	CommandSpec { name: "say", aliases: &[] },
	CommandSpec { name: "grep", aliases: &[] },
	CommandSpec { name: "ssh", aliases: &[] },
	CommandSpec { name: "ttsr", aliases: &[] },
	CommandSpec { name: "completions", aliases: &[] },
	CommandSpec { name: "__complete", aliases: &[] },
];

/// Returns whether a root command shares the launch option surface.
fn is_launch_command(argument: &OsString) -> bool {
	matches!(
		argument.to_string_lossy().as_ref(),
		"chat" | "i" | "launch" | "print" | "p" | "rpc" | "rpc-ui" | "acp"
	)
}

/// Classifies options accepted by launch-shaped invocations.
///
/// The boolean indicates whether the bare option consumes its successor.
fn launch_option(argument: &OsString) -> Option<bool> {
	let argument = argument.to_string_lossy();
	let (name, inline) = argument
		.split_once('=')
		.map_or((argument.as_ref(), false), |(name, _)| (name, true));
	let consumes_value = matches!(
		name,
		"--cwd"
			| "--export"
			| "--ext"
			| "--ext-only"
			| "--trusted-extension"
			| "--profile"
			| "--alias"
			| "--model"
			| "--project"
			| "--gateway"
			| "--resume"
			| "--continue"
			| "-c" | "--fork"
			| "--session-dir"
			| "--thinking"
			| "--service-tier"
			| "--approval-mode"
			| "--max-time"
			| "--tools"
			| "--mode"
			| "--follow-up"
			| "--provider"
			| "--provider-session-id"
			| "--prompt-cache-key"
			| "--config"
			| "--add-dir"
			| "--smol"
			| "--slow"
			| "--plan"
			| "--models"
			| "--prewalk-into"
			| "--skills"
			| "--api-key"
			| "--system-prompt"
			| "--append-system-prompt"
			| "--follow-up"
	);
	if consumes_value {
		return Some(!inline);
	}
	matches!(
		name,
		"--help"
			| "--version"
			| "--no-ext"
			| "--no-workspace-ext"
			| "--allow-home"
			| "--no-session"
			| "--py-eval"
			| "--print-thoughts"
			| "--shape-transcript"
			| "--acp-terminal-auth"
			| "--smoke-test"
			| "--plan-mode"
			| "--prewalk"
			| "--no-prewalk"
			| "--no-tools"
			| "--no-lsp"
			| "--no-pty"
			| "--no-skills"
			| "--no-rules"
			| "--no-title"
			| "--acp-terminal-auth"
	)
	.then_some(false)
}

/// Gateway serving options.
#[derive(Clone, Debug, Args)]
pub struct ServeArgs {
	/// Platform-local endpoint: a Unix socket path or Windows named-pipe name.
	#[arg(long = "endpoint", visible_aliases = ["uds", "pipe"], value_name = "LOCAL_ENDPOINT")]
	pub endpoint: LocalEndpoint,
	/// Override the directory containing daemon state.
	#[arg(long, value_name = "PATH")]
	pub data_dir: Option<PathBuf>,
}
/// Project environment-daemon options.
#[derive(Clone, Debug, Args)]
pub struct EnvdArgs {
	/// Workspace root exposed by the environment.
	#[arg(long, value_name = "PATH", default_value = ".")]
	pub root:             PathBuf,
	/// Owner-only environment socket. Defaults to `<state-dir>/env.sock`.
	#[arg(long, value_name = "PATH")]
	pub socket:           Option<PathBuf>,
	/// Document-server socket. An explicit live socket is attached; the default
	/// `<state-dir>/docserver.sock` must be unowned.
	#[arg(long, value_name = "PATH")]
	pub docserver_socket: Option<PathBuf>,
	/// Environment state directory. Defaults to a project-keyed directory under
	/// `OMP_DATA_DIR`.
	#[arg(long, value_name = "PATH")]
	pub state_dir:        Option<PathBuf>,
	/// Enable the built-in Python expression-evaluation tool.
	///
	/// This executes Python inside the environment owner's process sandbox and
	/// is disabled unless explicitly requested.
	#[arg(long)]
	pub py_eval:          bool,
	/// Seconds without connected apps before the daemon exits (0 disables).
	#[arg(long, value_name = "SECONDS", default_value_t = 900)]
	pub idle_timeout:     u64,
}
/// Typed prompt overrides shared by launch-shaped commands.
#[derive(Clone, Debug, Default, Args)]
pub struct PromptArgs {
	/// Select the prompt personality preset.
	#[arg(long, value_name = "PRESET")]
	pub personality:             Option<omp_agent::Personality>,
	/// Surface the active model identifier in workstation facts.
	#[arg(long, value_name = "BOOL", num_args = 0..=1, default_missing_value = "true")]
	pub include_model_in_prompt: Option<bool>,
	/// Include Environment-owned workstation facts.
	#[arg(long, value_name = "BOOL", num_args = 0..=1, default_missing_value = "true")]
	pub include_workstation:     Option<bool>,
	/// Include a bounded workspace tree.
	#[arg(long, value_name = "BOOL", num_args = 0..=1, default_missing_value = "true")]
	pub include_workspace_tree:  Option<bool>,
	/// Permit Mermaid diagram rendering guidance.
	#[arg(long, value_name = "BOOL", num_args = 0..=1, default_missing_value = "true")]
	pub render_mermaid:          Option<bool>,
	/// Include enabled skills in prompt assembly.
	#[arg(
		long = "skills-enabled",
		value_name = "BOOL",
		num_args = 0..=1,
		default_missing_value = "true"
	)]
	pub skills_enabled:          Option<bool>,
	/// Replace customizable prompt slots from a file path or literal string.
	#[arg(long = "system-prompt", visible_alias = "system", value_name = "PATH_OR_TEXT")]
	pub custom_prompt:           Option<Str>,
	/// Append guidance from a file path or literal string.
	#[arg(
		long = "append-system-prompt",
		visible_aliases = ["append-prompt", "append-system"],
		value_name = "PATH_OR_TEXT"
	)]
	pub append_prompt:           Option<Str>,
	/// Explicitly bypass provider prompt items for developer and test use.
	#[arg(long)]
	pub null_prompt:             bool,
}

/// Interactive project-chat options.
#[derive(Clone, Debug, Args)]
pub struct ChatArgs {
	/// Catalog model key, alias, or role.
	#[arg(long)]
	pub model:            Option<Str>,
	/// Provider preference for the selected model.
	#[arg(long)]
	pub provider:         Option<Str>,
	/// Fast/low-cost model-role selector.
	#[arg(long)]
	pub smol:             Option<Str>,
	/// Deep-reasoning model-role selector.
	#[arg(long)]
	pub slow:             Option<Str>,
	/// Planning model-role selector.
	#[arg(long)]
	pub plan:             Option<Str>,
	/// Ordered model selectors available for interactive cycling.
	#[arg(long)]
	pub models:           Option<SelectorList>,
	/// Provider session selector, never inferred from prompt text.
	#[arg(long = "provider-session-id")]
	pub provider_session: Option<Str>,
	/// Project root whose environment and durable sessions are used.
	#[arg(long, value_name = "PATH", default_value = ".")]
	pub project:          PathBuf,
	/// Existing inference gateway endpoint. Omit to run inference in process.
	#[arg(long, value_name = "LOCAL_ENDPOINT")]
	pub gateway:          Option<LocalEndpoint>,
	/// Existing ULID session to reopen strictly.
	#[arg(long, value_name = "ULID")]
	pub resume:           Option<Str>,
	/// Continue a UUID session.
	#[arg(
		long = "continue",
		short = 'c',
		value_name = "SESSION",
		num_args = 0..=1,
		default_missing_value = "@terminal",
		conflicts_with = "fork"
	)]
	pub continue_session: Option<Str>,
	/// Fork an existing session before opening the chat.
	#[arg(long, value_name = "SESSION", conflicts_with_all = ["resume", "continue_session", "no_session"])]
	pub fork:             Option<Str>,
	/// Do not persist a durable session for this chat.
	#[arg(long, conflicts_with_all = ["resume", "continue_session", "fork"])]
	pub no_session:       bool,
	/// Override the native session storage directory.
	#[arg(long, value_name = "PATH")]
	pub session_dir:      Option<PathBuf>,
	/// Select provider reasoning effort with unambiguous prefix abbreviations.
	#[arg(long)]
	pub thinking:         Option<ThinkingLevel>,
	/// Select the provider's service tier.
	#[arg(long)]
	pub service_tier:     Option<ServiceTier>,
	/// Tool approval policy.
	#[arg(long)]
	pub approval_mode:    Option<ApprovalMode>,
	/// Stop after this strictly positive duration.
	#[arg(long)]
	pub max_time:         Option<CliDuration>,
	/// Restrict enabled tools to these normalized names.
	#[arg(long, conflicts_with = "no_tools")]
	pub tools:            Option<ToolNames>,
	/// Disable every built-in tool.
	#[arg(long)]
	pub no_tools:         bool,
	/// Disable LSP tools, formatting, and diagnostics.
	#[arg(long)]
	pub no_lsp:           bool,
	/// Disable PTY-backed shell execution.
	#[arg(long)]
	pub no_pty:           bool,
	/// Enter read-only planning mode at startup.
	#[arg(long = "plan-mode")]
	pub plan_mode:        bool,
	/// Enter prewalk automation.
	#[arg(long, conflicts_with = "no_prewalk")]
	pub prewalk:          bool,
	/// Disable configured prewalk automation.
	#[arg(long, conflicts_with = "prewalk")]
	pub no_prewalk:       bool,
	/// Model selector used when prewalk begins.
	#[arg(long)]
	pub prewalk_into:     Option<Str>,
	/// Read-only native TOML settings overlays in precedence order.
	#[arg(long = "config", value_name = "TOML")]
	pub config:           Vec<PathBuf>,
	/// Additional authorized workspace roots.
	#[arg(long = "add-dir", value_name = "PATH")]
	pub add_dir:          Vec<PathBuf>,
	/// Comma-separated skill glob filters.
	#[arg(long)]
	pub skills:           Option<SelectorList>,
	/// Disable skill discovery.
	#[arg(long, conflicts_with = "skills")]
	pub no_skills:        bool,
	/// Disable rule discovery.
	#[arg(long)]
	pub no_rules:         bool,
	/// Disable generated terminal titles.
	#[arg(long)]
	pub no_title:         bool,
	/// Ephemeral provider API key; never journaled.
	#[arg(long)]
	pub api_key:          Option<Str>,
	/// Ephemeral provider prompt-cache affinity.
	#[arg(long = "prompt-cache-key")]
	pub prompt_cache_key: Option<Str>,
	#[arg(long)]
	pub py_eval:          bool,
	/// Typed prompt settings and invocation overrides.
	#[command(flatten)]
	pub prompt_settings:  PromptArgs,
}

impl ChatArgs {
	/// Returns the default options for an interactive project chat.
	pub fn default_interactive() -> Self {
		Self {
			model:            None,
			provider:         None,
			smol:             None,
			slow:             None,
			plan:             None,
			models:           None,
			provider_session: None,
			project:          ".".into(),
			gateway:          None,
			resume:           None,
			continue_session: None,
			fork:             None,
			no_session:       false,
			session_dir:      None,
			thinking:         None,
			service_tier:     None,
			approval_mode:    None,
			max_time:         None,
			tools:            None,
			no_tools:         false,
			no_lsp:           false,
			no_pty:           false,
			plan_mode:        false,
			prewalk:          false,
			no_prewalk:       false,
			prewalk_into:     None,
			config:           Vec::new(),
			add_dir:          Vec::new(),
			skills:           None,
			no_skills:        false,
			no_rules:         false,
			no_title:         false,
			api_key:          None,
			prompt_cache_key: None,
			py_eval:          false,
			prompt_settings:  PromptArgs::default(),
		}
	}
}
/// Non-interactive inference output options.
#[derive(Clone, Debug, Args)]
pub struct PrintArgs {
	/// Catalog model key. Falls back to `config.default_model`.
	#[arg(long)]
	pub model:            Option<Str>,
	/// Read-only native TOML settings overlays in precedence order.
	#[arg(long = "config", value_name = "TOML")]
	pub config:           Vec<PathBuf>,
	/// Additional authorized roots used by Environment-backed print tools.
	#[arg(long = "add-dir", value_name = "PATH")]
	pub add_dir:          Vec<PathBuf>,
	/// Fast/low-cost model-role selector.
	#[arg(long)]
	pub smol:             Option<Str>,
	/// Deep-reasoning model-role selector.
	#[arg(long)]
	pub slow:             Option<Str>,
	/// Planning model-role selector.
	#[arg(long)]
	pub plan:             Option<Str>,
	/// Model cycling list shared with interactive launch metadata.
	#[arg(long)]
	pub models:           Option<SelectorList>,
	/// Emit newline-delimited JSON events rather than final text.
	#[arg(long, value_parser = ["text", "json"], default_value = "text")]
	pub mode:             String,
	/// Include streamed reasoning in text output.
	#[arg(long)]
	pub print_thoughts:   bool,
	/// Select provider reasoning effort with unambiguous prefix abbreviations.
	#[arg(long)]
	pub thinking:         Option<ThinkingLevel>,
	/// Select the provider's service tier.
	#[arg(long)]
	pub service_tier:     Option<ServiceTier>,
	/// Tool approval policy for launch-shaped invocations.
	#[arg(long)]
	pub approval_mode:    Option<ApprovalMode>,
	/// Stop after this strictly positive duration.
	#[arg(long)]
	pub max_time:         Option<CliDuration>,
	/// Restrict enabled tools to these normalized names.
	#[arg(long, conflicts_with = "no_tools")]
	pub tools:            Option<ToolNames>,
	/// Disable every tool for this invocation.
	#[arg(long)]
	pub no_tools:         bool,
	/// Disable LSP-backed tools.
	#[arg(long)]
	pub no_lsp:           bool,
	/// Disable PTY-backed tools.
	#[arg(long)]
	pub no_pty:           bool,
	/// Additional user messages applied in order after the initial prompt.
	#[arg(long = "follow-up", value_name = "TEXT")]
	pub follow_ups:       Vec<Str>,
	/// Enter plan mode with one explicitly authorized mutation transition.
	#[arg(long)]
	pub plan_yolo:        bool,
	/// Drop provider payloads and partial transcript snapshots from NDJSON.
	#[arg(long)]
	pub shape_transcript: bool,
	/// Typed prompt settings and invocation overrides.
	#[command(flatten)]
	pub prompt_settings:  PromptArgs,
	/// Prompt words; `@path` includes a typed attachment.
	#[arg(num_args = 0..)]
	pub prompt:           Vec<Str>,
}

/// Stateful headless RPC server options.
#[derive(Clone, Debug, Args)]
pub struct RpcArgs {
	/// Catalog model key. Falls back to `config.default_model`.
	#[arg(long)]
	pub model:       Option<Str>,
	/// Prefer routes owned by this provider when the selected model permits it.
	#[arg(long)]
	pub provider:    Option<Str>,
	/// Project root used for session metadata and orchestration context.
	#[arg(long, value_name = "PATH", default_value = ".")]
	pub project:     PathBuf,
	/// Optional directory used to discover subagent transcript files.
	#[arg(long, value_name = "PATH")]
	pub session_dir: Option<PathBuf>,
}

/// Agent Client Protocol stdio options.
#[derive(Clone, Debug, Args)]
pub struct AcpArgs {
	/// Catalog model key. Falls back to `config.default_model`.
	#[arg(long)]
	pub model:             Option<Str>,
	/// Project root whose durable sessions ACP exposes.
	#[arg(long, value_name = "PATH", default_value = ".")]
	pub project:           PathBuf,
	/// Advertise and permit terminal-spawned provider authentication.
	#[arg(long)]
	pub acp_terminal_auth: bool,
}

/// Direct typed inference options.
#[derive(Clone, Debug, Args)]
pub struct InferArgs {
	/// Catalog model key.
	#[arg(long)]
	pub model:  Str,
	/// User prompt.
	#[arg(long)]
	pub prompt: Str,
}

/// Authentication command options.
#[derive(Clone, Debug, Args)]
pub struct AuthArgs {
	/// OMP data directory containing `credentials.db`.
	#[arg(long, value_name = "PATH")]
	pub data_dir: Option<PathBuf>,
	/// Authentication operation.
	#[command(subcommand)]
	pub command:  AuthCommand,
}

/// Typed authentication commands.
#[derive(Clone, Debug, Subcommand)]
pub enum AuthCommand {
	/// Begin an interactive provider login.
	Login {
		/// Target provider identifier.
		provider: Str,
	},
	/// List non-secret account summaries.
	List {
		/// Optional provider filter.
		#[arg(long)]
		provider: Option<Str>,
	},
	/// Refresh one account.
	Refresh {
		/// Target account identifier.
		account: Str,
	},
	/// Remove one account.
	Logout {
		/// Target account identifier.
		account: Str,
	},
}

/// Application settings command tree.
#[derive(Clone, Debug, Args)]
pub struct ConfigArgs {
	/// Settings operation.
	#[command(subcommand)]
	pub command: ConfigCommand,
}

/// Writable native settings scope.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum ConfigScope {
	/// User/profile settings.
	#[default]
	Global,
	/// Nearest project `.omp/config.toml`.
	Project,
}

/// Writable native MCP configuration scope.
#[derive(
	Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum, strum::Display, strum::IntoStaticStr,
)]
#[strum(serialize_all = "lowercase")]
pub enum McpConfigScope {
	/// User-level `~/.omp/mcp.json`.
	Global,
	/// Project-owned `.omp/mcp.json`.
	#[default]
	Project,
	/// Project-root `.mcp.json`.
	Root,
}

/// Native MCP configuration operations.
#[derive(Clone, Debug, Subcommand)]
pub enum McpConfigCommand {
	/// List configured MCP servers.
	List {
		/// Restrict the listing to one native scope.
		#[arg(long, value_enum)]
		scope: Option<McpConfigScope>,
		/// Emit structured JSON.
		#[arg(long)]
		json:  bool,
	},
	/// Read one configured MCP server.
	Get {
		/// Server name.
		name: Str,
	},
	/// Add a validated MCP server from a JSON object.
	Add {
		/// Server name.
		name:   Str,
		/// MCP server JSON object.
		config: Str,
		/// Writable native scope.
		#[arg(long, value_enum, default_value_t)]
		scope:  McpConfigScope,
	},
	/// Insert or replace a validated MCP server from a JSON object.
	Update {
		/// Server name.
		name:   Str,
		/// MCP server JSON object.
		config: Str,
		/// Writable native scope.
		#[arg(long, value_enum, default_value_t)]
		scope:  McpConfigScope,
	},
	/// Remove an MCP server from one native scope.
	Remove {
		/// Server name.
		name:  Str,
		/// Writable native scope.
		#[arg(long, value_enum, default_value_t)]
		scope: McpConfigScope,
	},
	/// Enable a server, using a native override for read-only manifest sources.
	Enable {
		/// Server name.
		name: Str,
	},
	/// Disable a server, using a native override for read-only manifest sources.
	Disable {
		/// Server name.
		name: Str,
	},
}

/// Schema-validated settings operations.
#[derive(Clone, Debug, Subcommand)]
pub enum ConfigCommand {
	/// Initialize canonical XDG roots and migrate recognized legacy storage
	/// without replacing existing destinations.
	#[command(name = "init-xdg")]
	InitXdg {
		/// Emit a machine-readable migration report.
		#[arg(long)]
		json: bool,
	},
	/// List schema keys, types, and effective values.
	List {
		/// Emit structured JSON.
		#[arg(long)]
		json: bool,
	},
	/// Read one schema key.
	Get {
		/// Schema key.
		key: Str,
	},
	/// Set one schema key after validating its typed value.
	Set {
		/// Schema key.
		key:   Str,
		/// Typed value.
		value: Str,
		/// Writable native scope.
		#[arg(long, value_enum, default_value_t)]
		scope: ConfigScope,
	},
	/// Remove one schema key from a writable layer.
	Unset {
		/// Schema key.
		key:   Str,
		/// Writable native scope.
		#[arg(long, value_enum, default_value_t)]
		scope: ConfigScope,
	},
	/// Print a native settings file path.
	Path {
		/// Writable native scope.
		#[arg(long, value_enum, default_value_t)]
		scope: ConfigScope,
	},
	/// Manage native MCP server configuration.
	Mcp {
		/// MCP operation.
		#[command(subcommand)]
		command: McpConfigCommand,
	},
}

/// Model catalog command tree.
#[derive(Clone, Debug, Args)]
pub struct ModelsArgs {
	/// Catalog operation; omitted means list.
	#[command(subcommand)]
	pub command: Option<ModelsCommand>,
	/// Optional provider/model/display-name filter for the default list
	/// operation.
	#[arg(value_name = "FILTER")]
	pub filter:  Option<Str>,
	/// Emit structured JSON for the default list operation.
	#[arg(long)]
	pub json:    bool,
	/// Pick one deterministic cycling role from matching rows.
	#[arg(long)]
	pub role:    Option<ModelRole>,
}

/// Model catalog operations.
#[derive(Clone, Debug, Subcommand)]
pub enum ModelsCommand {
	/// List catalog models, optionally narrowed by a fuzzy filter.
	#[command(alias = "ls")]
	List {
		/// Optional provider/model/display-name filter.
		filter: Option<Str>,
		/// Emit structured JSON.
		#[arg(long)]
		json:   bool,
		/// Pick one deterministic cycling role from matching rows.
		#[arg(long)]
		role:   Option<ModelRole>,
	},
	/// Search provider IDs, model keys, and display names case-insensitively.
	Find {
		/// Search text.
		pattern: Str,
		/// Emit structured JSON.
		#[arg(long)]
		json:    bool,
	},
	/// Force provider discovery refresh when a discovery backend is available.
	Refresh,
}

/// Combined provider/MCP credential-broker command tree.
#[derive(Clone, Debug, Args)]
pub struct AuthBrokerArgs {
	/// Override the profile data directory containing broker state.
	#[arg(long, value_name = "PATH")]
	pub data_dir: Option<PathBuf>,
	/// Broker operation.
	#[command(subcommand)]
	pub command:  AuthBrokerCommand,
}

/// Combined provider/MCP credential-broker operations.
#[derive(Clone, Debug, Subcommand)]
pub enum AuthBrokerCommand {
	/// Start the owner-local broker service.
	Serve {
		/// Platform-local socket or named-pipe endpoint.
		#[arg(long, value_name = "LOCAL_ENDPOINT")]
		endpoint: LocalEndpoint,
	},
	/// Print or rotate the owner-only broker token.
	Token {
		/// Replace the current bearer token.
		#[arg(long)]
		regenerate: bool,
	},
	/// Begin OAuth login for one provider.
	Login {
		/// Provider identifier.
		provider: Str,
	},
	/// Remove stored OAuth credentials for one provider.
	Logout {
		/// Provider identifier.
		provider: Str,
	},
	/// List available broker providers.
	List,
	/// Import credential material from a file.
	Import {
		/// Credential export path.
		path: PathBuf,
	},
	/// Apply store migrations and rotate every credential under the active key.
	Migrate {
		/// Report the number of credentials that would be re-encrypted.
		#[arg(long)]
		dry_run: bool,
	},
	/// Inspect broker health.
	Status,
}

/// Credential-injecting inference gateway options.
#[derive(Clone, Debug, Args)]
pub struct AuthGatewayArgs {
	/// Override the profile data directory containing gateway state.
	#[arg(long, value_name = "PATH")]
	pub data_dir: Option<PathBuf>,
	/// Gateway operation.
	#[command(subcommand)]
	pub command:  AuthGatewayCommand,
}

/// Credential-injecting gateway operations.
#[derive(Clone, Debug, Subcommand)]
pub enum AuthGatewayCommand {
	/// Start the gateway over an owner-local socket or named pipe.
	Serve {
		/// Platform-local socket or named-pipe endpoint.
		#[arg(long, value_name = "LOCAL_ENDPOINT")]
		endpoint: LocalEndpoint,
	},
	/// Print or rotate the gateway bearer token.
	Token {
		/// Replace the current bearer token.
		#[arg(long)]
		regenerate: bool,
	},
	/// Query the versioned gateway health handshake.
	Status {
		/// Platform-local socket or named-pipe endpoint.
		#[arg(long, value_name = "LOCAL_ENDPOINT")]
		endpoint: LocalEndpoint,
	},
	/// Check gateway health, optionally failing on an unavailable endpoint.
	Check {
		/// Platform-local socket or named-pipe endpoint.
		#[arg(long, value_name = "LOCAL_ENDPOINT")]
		endpoint: LocalEndpoint,
		/// Return an error when the gateway is unhealthy.
		#[arg(long)]
		strict:   bool,
	},
}

/// Model-catalog command tree.
#[derive(Clone, Debug, Args)]
pub struct CatalogArgs {
	/// Catalog operation.
	#[command(subcommand)]
	pub command: CatalogCommand,
}

/// Model-catalog operations.
#[derive(Clone, Debug, Subcommand)]
pub enum CatalogCommand {
	/// Import catalog sources into normalized JSON.
	Import(CatalogImportArgs),
}

/// Catalog compiler inputs and normalized output.
#[derive(Clone, Debug, Args)]
pub struct CatalogImportArgs {
	/// Provider manifest TOML.
	#[arg(long, value_name = "TOML")]
	pub providers:   PathBuf,
	/// Secret-free OAuth flow manifest TOML.
	#[arg(long, value_name = "TOML")]
	pub oauth:       PathBuf,
	/// Compressed oracle model rows.
	#[arg(long, value_name = "ZST")]
	pub models:      PathBuf,
	/// Destination normalized JSON.
	#[arg(long, value_name = "JSON")]
	pub destination: PathBuf,
}

/// In-process local inference command tree.
#[derive(Clone, Debug, Args)]
pub struct LocalArgs {
	/// Local inference operation.
	#[command(subcommand)]
	pub command: LocalCommand,
}

/// Local inference operations.
#[derive(Clone, Debug, Subcommand)]
pub enum LocalCommand {
	/// Run local in-process inference.
	Infer(LocalInferArgs),
}

/// In-process Apple Foundation Models options.
#[derive(Clone, Debug, Args)]
pub struct LocalInferArgs {
	/// User prompt.
	#[arg(long)]
	pub prompt: Str,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DispatchTarget {
	Serve,
	Envd,
	Chat,
	Print,
	Rpc,
	RpcUi,
	Acp,
	Infer,
	Auth,
	CatalogImport,
	LocalInfer,
	Ext,
	Config,
	Update,
	Registry,
	Share,
	Models,
	AuthBroker,
	AuthGateway,
	Worktree,
	Stats,
	Gc,
	Usage,
	Bench,
	DryBalance,
	TinyModels,
	Setup,
	Say,
	Grep,
	Ssh,
	Ttsr,
	Completions,
	Complete,
}

#[cfg(test)]
const fn dispatch_target(command: Option<&Command>) -> DispatchTarget {
	match command {
		None | Some(Command::Chat(_)) => DispatchTarget::Chat,
		Some(Command::Print(_)) => DispatchTarget::Print,
		Some(Command::Rpc(_)) => DispatchTarget::Rpc,
		Some(Command::RpcUi(_)) => DispatchTarget::RpcUi,
		Some(Command::Acp(_)) => DispatchTarget::Acp,
		Some(Command::Serve(_)) => DispatchTarget::Serve,
		Some(Command::Envd(_)) => DispatchTarget::Envd,
		Some(Command::Infer(_)) => DispatchTarget::Infer,
		Some(Command::Auth(_)) => DispatchTarget::Auth,
		Some(Command::Catalog(CatalogArgs { command: CatalogCommand::Import(_) })) => {
			DispatchTarget::CatalogImport
		},
		Some(Command::Local(LocalArgs { command: LocalCommand::Infer(_) })) => {
			DispatchTarget::LocalInfer
		},
		Some(Command::Ext(_)) => DispatchTarget::Ext,
		Some(Command::Config(_)) => DispatchTarget::Config,
		Some(Command::Update(_)) => DispatchTarget::Update,
		Some(Command::Registry(_)) => DispatchTarget::Registry,
		Some(Command::Share(_)) => DispatchTarget::Share,
		Some(Command::Models(_)) => DispatchTarget::Models,
		Some(Command::Worktree(_)) => DispatchTarget::Worktree,
		Some(Command::Stats(_)) => DispatchTarget::Stats,
		Some(Command::Gc(_)) => DispatchTarget::Gc,
		Some(Command::Usage(_)) => DispatchTarget::Usage,
		Some(Command::Bench(_)) => DispatchTarget::Bench,
		Some(Command::DryBalance(_)) => DispatchTarget::DryBalance,
		Some(Command::TinyModels(_)) => DispatchTarget::TinyModels,
		Some(Command::Setup(_)) => DispatchTarget::Setup,
		Some(Command::Say(_)) => DispatchTarget::Say,
		Some(Command::Grep(_)) => DispatchTarget::Grep,
		Some(Command::Ssh(_)) => DispatchTarget::Ssh,
		Some(Command::Ttsr(_)) => DispatchTarget::Ttsr,
		Some(Command::Completions { .. }) => DispatchTarget::Completions,
		Some(Command::Complete { .. }) => DispatchTarget::Complete,
		Some(Command::AuthBroker(_)) => DispatchTarget::AuthBroker,
		Some(Command::AuthGateway(_)) => DispatchTarget::AuthGateway,
	}
}

fn chat_start(args: &mut ChatArgs) -> crate::chat::ChatStart {
	if args.resume.as_deref() == Some("__omp_picker__") {
		args.resume = None;
		crate::chat::ChatStart::SessionIndex
	} else {
		crate::chat::ChatStart::Session
	}
}

/// Dispatches one parsed command to its production implementation.
#[expect(
	clippy::future_not_send,
	reason = "chat dispatch preserves the thread-confined omp_tui::App future"
)]
pub async fn dispatch(cli: OmpCli) -> miette::Result<()> {
	if let Some(journal) = cli.export.as_deref() {
		let output = journal.with_extension("html");
		let exported = crate::export::export_session(journal, &output).into_diagnostic()?;
		println!("Exported to: {}", exported.display());
		return Ok(());
	}
	if cli.smoke_test {
		return crate::smoke_test::run().await;
	}
	if let Some(alias) = cli.alias.as_deref() {
		let profile = cli.profile.as_deref().ok_or_else(|| {
			crate::usage_error::CliUsageError::new("--alias requires --profile or OMP_PROFILE")
		})?;
		let installed = crate::profile_alias::install(alias, profile, None).into_diagnostic()?;
		println!(
			"installed {} profile wrapper `{}` in {}",
			installed.shell,
			installed.name,
			installed.path.display()
		);
		return Ok(());
	}
	if let Some(cwd) = cli.cwd.as_deref() {
		std::env::set_current_dir(cwd).into_diagnostic()?;
	}
	if !cli.allow_home && cli.command.is_none() && is_home_dir()? {
		return Err(miette!(
			"refusing to start an interactive session in HOME; pass --allow-home or --cwd"
		));
	}
	match cli
		.command
		.unwrap_or_else(|| Command::Chat(ChatArgs::default_interactive()))
	{
		Command::Serve(args) => serve(args).await,
		Command::Envd(args) => crate::envd::run(args).await,
		Command::Chat(mut args) => {
			let start = chat_start(&mut args);
			crate::startup_notice::show_once(
				&data_dir(None)?,
				args.model.as_ref(),
				args.thinking.map(<&'static str>::from),
				crate::startup_notice::Eligibility {
					resume: args.resume.is_some()
						|| args.continue_session.is_some()
						|| args.fork.is_some(),
					quiet:  false,
					timing: std::env::var_os("OMP_TIMING").is_some(),
				},
			)
			.into_diagnostic()?;
			Box::pin(crate::chat::run(args, start)).await
		},
		Command::Print(args) => crate::print_mode::run(args).await,
		Command::Rpc(args) | Command::RpcUi(args) => crate::rpc_mode::run(args).await,
		Command::Acp(args) => crate::acp_mode::run(args).await,
		Command::Infer(args) => infer(args).await,
		Command::Auth(args) => auth(args).await,
		Command::Catalog(CatalogArgs { command: CatalogCommand::Import(args) }) => {
			catalog_import(&args)
		},
		Command::Local(LocalArgs { command: LocalCommand::Infer(args) }) => local_infer(args).await,
		Command::Ext(args) => crate::ext_cli::run(args).await,
		Command::Config(args) => crate::config_cmd::run(&data_dir(None)?, &args.command),
		Command::Update(args) => crate::update_cmd::run(args).await,
		Command::Registry(args) => crate::update_cmd::registry(args),
		Command::Share(args) => crate::share_cmd::run(args).await,
		Command::Models(args) => crate::models_cmd::run(&args).await,
		Command::Worktree(args) => crate::worktree_cmd::run(&data_dir(None)?, &args),
		Command::Stats(args) => crate::stats_cmd::run(args).await,
		Command::Gc(args) => crate::gc_cmd::run(args),
		Command::Usage(args) => crate::usage_cmd::run(args),
		Command::Bench(args) => crate::bench_cmd::run(args).await,
		Command::DryBalance(args) => crate::dry_balance_cmd::run(args).await,
		Command::TinyModels(args) => crate::tiny_models_cmd::run(args).await,
		Command::Setup(args) => crate::setup_cmd::run(args).await,
		Command::Say(args) => crate::say_cmd::run(args).await,
		Command::Grep(args) => crate::grep_cmd::run(args),
		Command::Ssh(args) => crate::ssh_cmd::run(args).await,
		Command::Ttsr(args) => crate::ttsr_cmd::run(args),
		Command::Completions { shell } => {
			let bytes = crate::completions::script(shell.into());
			std::io::Write::write_all(&mut std::io::stdout(), &bytes).into_diagnostic()
		},
		Command::Complete { kind, prefix } => crate::complete_cmd::run(kind, &prefix),
		Command::AuthBroker(args) => crate::auth_broker_cmd::run(args).await,
		Command::AuthGateway(args) => crate::auth_gateway_cmd::run(args).await,
	}
}

/// Parses process arguments after routing commands hidden behind launch
/// options, normalizing bare prompts, and selecting print mode for a
/// non-interactive empty invocation.
pub fn parse_from_os(arguments: impl IntoIterator<Item = OsString>) -> Result<OmpCli, clap::Error> {
	use clap::error::ErrorKind;
	let profile = profile_bootstrap::extract(arguments)
		.map_err(|error| clap::Error::raw(ErrorKind::InvalidValue, error.to_string()))?;
	profile_bootstrap::select(profile.profile.clone());
	if let Some(message) = routing::redirect(&profile.arguments) {
		return Err(clap::Error::raw(ErrorKind::InvalidSubcommand, message.to_string()));
	}
	let mut bootstrap = bootstrap::run(profile.arguments, builtin_contribution_names())
		.map_err(|error| clap::Error::raw(ErrorKind::InvalidValue, error.to_string()))?;
	profile_bootstrap::remove_boundaries(&mut bootstrap.arguments);
	let mut arguments = bootstrap.arguments;
	normalize_hidden_command(&mut arguments);
	if !std::io::stdin().is_terminal()
		&& first_positional(&arguments).is_none()
		&& !arguments.iter().skip(1).any(|argument| {
			matches!(argument.to_string_lossy().as_ref(), "--help" | "-h" | "--version" | "-V")
		}) {
		arguments.push(OsString::from("print"));
	}
	if let Some(index) = first_positional(&arguments) {
		if arguments[index] == "resume" {
			arguments[index] = OsString::from("chat");
			arguments.insert(index + 1, OsString::from("--resume=__omp_picker__"));
		} else if !is_command(&arguments[index])
			&& !arguments[index].to_string_lossy().starts_with('-')
		{
			arguments.insert(index, OsString::from("print"));
		}
	}
	normalize_hidden_command(&mut arguments);
	normalize_bare_resume(&mut arguments);
	let mut cli = OmpCli::try_parse_from(arguments)?;
	cli.profile = profile.profile;
	cli.alias = profile.alias;
	cli.contributed = bootstrap.values;
	Ok(cli)
}

fn builtin_contribution_names() -> impl Iterator<Item = Str> {
	[
		"add-dir",
		"alias",
		"allow-home",
		"api-key",
		"config",
		"cwd",
		"ext",
		"ext-only",
		"model",
		"models",
		"no-ext",
		"no-lsp",
		"no-pty",
		"no-rules",
		"no-skills",
		"no-title",
		"no-tools",
		"plan",
		"prewalk",
		"prewalk-into",
		"profile",
		"provider",
		"provider-session-id",
		"plan-mode",
		"skills",
		"slow",
		"smol",
		"system-prompt",
		"append-system-prompt",
		"tools",
		"trusted-extension",
	]
	.into_iter()
	.map(Str::new_static)
}

fn normalize_hidden_command(arguments: &mut Vec<OsString>) {
	if arguments.get(1).is_some_and(|argument| {
		matches!(argument.to_string_lossy().as_ref(), "--help" | "-h" | "--version" | "-V")
	}) {
		return;
	}
	let Some(command_index) = leading_command_index(arguments) else {
		return;
	};
	if arguments[command_index] == "-p" {
		arguments[command_index] = OsString::from("print");
	}
	if command_index == 1 {
		return;
	}
	let leading: Vec<OsString> = arguments.drain(1..command_index).collect();
	if is_launch_command(&arguments[1]) {
		arguments.splice(2..2, leading);
		return;
	}
	let mut kept = Vec::with_capacity(leading.len());
	let mut leading = leading.into_iter();
	while let Some(argument) = leading.next() {
		if let Some(consumes_value) = launch_option(&argument) {
			if consumes_value {
				leading.next();
			}
		} else {
			kept.push(argument);
		}
	}
	arguments.splice(2..2, kept);
}

fn leading_command_index(arguments: &[OsString]) -> Option<usize> {
	let mut index = 1;
	while index < arguments.len() {
		let argument = &arguments[index];
		if is_command(argument) || argument == "-p" {
			return Some(index);
		}
		if argument == "--" || !argument.to_string_lossy().starts_with('-') {
			return None;
		}
		index += 1 + usize::from(launch_option(argument) == Some(true));
	}
	None
}

fn first_positional(arguments: &[OsString]) -> Option<usize> {
	let mut index = 1;
	while index < arguments.len() {
		let argument = arguments[index].to_string_lossy();
		if argument == "--" {
			return Some(index);
		}
		if launch_option(&arguments[index]) == Some(true) {
			index += 2;
			continue;
		}
		if argument.starts_with('-') {
			index += 1;
			continue;
		}
		return Some(index);
	}
	None
}

fn normalize_bare_resume(arguments: &mut Vec<OsString>) {
	let mut index = 1;
	while index < arguments.len() {
		if arguments[index] == "--resume"
			&& arguments
				.get(index + 1)
				.is_none_or(|next| next.to_string_lossy().starts_with('-'))
		{
			arguments.insert(index + 1, OsString::from("__omp_picker__"));
			index += 1;
		}
		index += 1;
	}
}

fn is_command(argument: &OsString) -> bool {
	let argument = argument.to_string_lossy();
	COMMAND_REGISTRY
		.iter()
		.any(|entry| entry.name == argument || entry.aliases.contains(&argument.as_ref()))
}

fn trusted_extension_path(value: &str) -> Result<PathBuf, String> {
	crate::envd::site::validate_trusted_module(Path::new(value))
		.map(|module| module.path)
		.map_err(|error| error.to_string())
}

fn is_home_dir() -> miette::Result<bool> {
	let home = std::env::var_os("HOME").ok_or_else(|| miette!("HOME must be set"))?;
	Ok(std::env::current_dir().into_diagnostic()? == home)
}

async fn serve(args: ServeArgs) -> miette::Result<()> {
	let config = args.data_dir.map_or_else(
		|| DaemonConfig::local(args.endpoint.clone()),
		|dir| DaemonConfig::local(args.endpoint.clone()).with_data_dir(dir),
	);
	let handle = DaemonHandle::start(config).await.into_diagnostic()?;
	handle.wait().await.into_diagnostic()?;
	Ok(())
}

async fn infer(args: InferArgs) -> miette::Result<()> {
	let data_dir = data_dir(None)?;
	let store =
		crate::daemon::open_credential_store(data_dir.join("credentials.db")).into_diagnostic()?;
	let registry = crate::daemon::production_registry(&data_dir, store)
		.await
		.into_diagnostic()?;
	let planner =
		omp_llm_inference::router::Router::new(registry.clone(), std::time::Duration::from_secs(30));
	let meta = CallMeta {
		id:       RequestId::from(turn_id()),
		target:   Target::Model(ModelKey::from(args.model)),
		deadline: None,
		budget:   ExecutionBudget::default(),
		session:  None,
	};
	let mut client = Client::new(registry.service(), planner, meta);
	let mut events = client
		.execute(chat_request(args.prompt))
		.await
		.into_diagnostic()?;
	let mut completed = false;
	let mut stdout = tokio::io::stdout();
	while let Some(event) = events.next().await {
		match event.into_diagnostic()? {
			ChatEvent::TextDelta { text, .. } => {
				stdout.write_all(text.as_bytes()).await.into_diagnostic()?;
			},
			ChatEvent::Completed(_) => completed = true,
			_ => {},
		}
	}
	if !completed {
		return Err(miette!("inference stream ended without completion"));
	}
	stdout.write_all(b"\n").await.into_diagnostic()?;
	stdout.flush().await.into_diagnostic()?;
	Ok(())
}

pub(crate) fn chat_request(prompt: Str) -> ChatRequest {
	chat_request_with_messages(
		vec![ContentPart::Text { text: prompt, proof: None }],
		Vec::new(),
		None,
	)
}

/// Builds a canonical request from typed initial attachments and ordered
/// follow-up messages, optionally prepending discovered system instructions.
pub(crate) fn chat_request_with_messages(
	initial: Vec<ContentPart>,
	follow_ups: Vec<Str>,
	system: Option<Str>,
) -> ChatRequest {
	let mut messages = Vec::with_capacity(usize::from(system.is_some()) + 1 + follow_ups.len());
	if let Some(text) = system {
		messages.push(Message {
			role:    Role::System,
			content: Arc::from([ContentPart::Text { text, proof: None }]),
			name:    None,
		});
	}
	messages.push(Message { role: Role::User, content: Arc::from(initial), name: None });
	messages.extend(follow_ups.into_iter().map(|text| Message {
		role:    Role::User,
		content: Arc::from([ContentPart::Text { text, proof: None }]),
		name:    None,
	}));
	ChatRequest {
		messages:          Arc::from(messages),
		tools:             Arc::from([]),
		hosted_tools:      Arc::from([]),
		tool_choice:       Setting::Unset,
		output:            Setting::Unset,
		reasoning:         Setting::Unset,
		verbosity:         Setting::Unset,
		cache_retention:   Setting::Unset,
		service_tier:      Setting::Unset,
		sampling:          Sampling::default(),
		max_output_tokens: None,
		top_logprobs:      None,
		safety:            Arc::from([]),
		negotiation:       NegotiationPolicy::default(),
	}
}

pub(crate) fn turn_id() -> String {
	let now = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or_default()
		.as_nanos();
	format!("omp-cli-{}-{now}", std::process::id())
}

async fn auth(args: AuthArgs) -> miette::Result<()> {
	let data = data_dir(args.data_dir)?;
	crate::auth_backend::run(data.join("credentials.db"), args.command).await
}

pub(crate) fn data_dir(explicit: Option<PathBuf>) -> miette::Result<PathBuf> {
	if let Some(path) = explicit {
		return Ok(path);
	}
	let base = if let Some(path) = std::env::var_os("OMP_DATA_DIR").filter(|value| !value.is_empty())
	{
		PathBuf::from(path)
	} else {
		let home =
			std::env::var_os("HOME").ok_or_else(|| miette!("HOME or OMP_DATA_DIR must be set"))?;
		crate::discovery::native::native_directories(&PathBuf::from(home)).data
	};
	Ok(crate::cli::profile_bootstrap::selected()
		.map_or(base.clone(), |profile| base.join("profiles").join(profile)))
}

fn catalog_import(args: &CatalogImportArgs) -> miette::Result<()> {
	if same_path(&args.providers, &args.destination)
		|| same_path(&args.oauth, &args.destination)
		|| same_path(&args.models, &args.destination)
	{
		return Err(miette!("catalog inputs and destination must be different files"));
	}
	let providers = std::fs::read_to_string(&args.providers).into_diagnostic()?;
	let oauth = std::fs::read_to_string(&args.oauth).into_diagnostic()?;
	let models = std::fs::read(&args.models).into_diagnostic()?;
	let payload = compile_oracle(&providers, &models, &oauth)
		.into_diagnostic()?
		.normalized_json()
		.into_diagnostic()?;
	if let Some(parent) = args
		.destination
		.parent()
		.filter(|path| !path.as_os_str().is_empty())
	{
		std::fs::create_dir_all(parent).into_diagnostic()?;
	}
	std::fs::write(&args.destination, payload).into_diagnostic()?;
	Ok(())
}

fn same_path(left: &Path, right: &Path) -> bool {
	left == right
		|| left
			.canonicalize()
			.ok()
			.zip(right.canonicalize().ok())
			.is_some_and(|(left, right)| left == right)
}

#[cfg(feature = "local-applefm")]
async fn local_infer(args: LocalInferArgs) -> miette::Result<()> {
	let model = AppleFm::load().await.into_diagnostic()?;
	let mut events = model
		.stream(AppleFmOptions::new(args.prompt))
		.into_diagnostic()?;
	let mut completed = false;
	let mut stdout = tokio::io::stdout();
	while let Some(event) = events.next().await {
		match event.into_diagnostic()? {
			AppleFmEvent::Delta(text) => stdout.write_all(text.as_bytes()).await.into_diagnostic()?,
			AppleFmEvent::Finished(_) => completed = true,
		}
	}
	if !completed {
		return Err(miette!("local inference stream ended without completion"));
	}
	stdout.write_all(b"\n").await.into_diagnostic()?;
	stdout.flush().await.into_diagnostic()?;
	Ok(())
}

#[cfg(not(feature = "local-applefm"))]
fn local_infer(_args: LocalInferArgs) -> std::future::Ready<miette::Result<()>> {
	std::future::ready(Err(miette!("local inference requires the `local-applefm` feature")))
}

#[cfg(test)]
mod tests {
	use clap::error::ErrorKind;
	use omp_core::sf;

	use super::*;

	fn parse(arguments: &[&str]) -> OmpCli {
		OmpCli::try_parse_from(arguments).expect("valid command")
	}

	#[cfg(unix)]
	const TEST_ENDPOINT: &str = "/tmp/omp.sock";
	#[cfg(windows)]
	const TEST_ENDPOINT: &str = r"\\.\pipe\omp-cli-test";

	#[test]
	fn bare_command_defaults_to_interactive_chat() {
		let cli = parse(&["omp"]);
		assert!(cli.command.is_none());
		assert_eq!(dispatch_target(cli.command.as_ref()), DispatchTarget::Chat);

		let args = ChatArgs::default_interactive();
		assert!(args.model.is_none());
		assert_eq!(args.project, PathBuf::from("."));
		assert!(args.gateway.is_none());
		assert!(args.resume.is_none());
		assert!(!args.py_eval);
	}

	#[test]
	fn parses_chat_without_model() {
		let Some(Command::Chat(args)) = parse(&["omp", "chat"]).command else {
			panic!("chat command");
		};
		assert!(args.model.is_none());
	}

	#[test]
	fn parses_every_dispatch_branch() {
		let cases = [
			(&["omp", "serve", "--endpoint", TEST_ENDPOINT][..], DispatchTarget::Serve),
			(&["omp", "envd"][..], DispatchTarget::Envd),
			(
				&["omp", "chat", "--model", "provider/model", "--project", "."][..],
				DispatchTarget::Chat,
			),
			(&["omp", "rpc"][..], DispatchTarget::Rpc),
			(&["omp", "acp"][..], DispatchTarget::Acp),
			(
				&["omp", "infer", "--model", "provider/model", "--prompt", "hello"][..],
				DispatchTarget::Infer,
			),
			(&["omp", "auth", "list"][..], DispatchTarget::Auth),
			(
				&[
					"omp",
					"catalog",
					"import",
					"--providers",
					"providers.toml",
					"--oauth",
					"oauth.toml",
					"--models",
					"models.json.zst",
					"--destination",
					"catalog.json",
				][..],
				DispatchTarget::CatalogImport,
			),
			(&["omp", "local", "infer", "--prompt", "hello"][..], DispatchTarget::LocalInfer),
			(&["omp", "ext", "list"][..], DispatchTarget::Ext),
		];
		for (arguments, expected) in cases {
			assert_eq!(dispatch_target(parse(arguments).command.as_ref()), expected);
		}
	}
	#[test]
	fn parses_chat_composition_options() {
		let Some(Command::Chat(args)) = parse(&[
			"omp",
			"chat",
			"--model",
			"provider/model",
			"--project",
			"workspace",
			"--gateway",
			TEST_ENDPOINT,
			"--resume",
			"01ARZ3NDEKTSV4RRFFQ69G5FAV",
			"--py-eval",
		])
		.command
		else {
			panic!("chat command");
		};
		assert_eq!(args.model, Some(sf!("provider/model")));
		assert_eq!(args.project, PathBuf::from("workspace"));
		assert_eq!(args.gateway.as_ref().map(LocalEndpoint::as_path), Some(Path::new(TEST_ENDPOINT)));
		assert_eq!(args.resume, Some(sf!("01ARZ3NDEKTSV4RRFFQ69G5FAV")));
		assert!(args.py_eval);
	}

	#[test]
	fn parses_ext_group_flags_and_subcommands() {
		let cli = parse(&[
			"omp",
			"--ext=publisher/example",
			"--ext-only",
			"local-ext",
			"--no-workspace-ext",
			"ext",
			"install",
			"--pool=shared",
			"--tier",
			"trusted",
			"--grant",
			"network",
			"publisher/example",
			"--",
			"literal-spec",
		]);
		assert_eq!(cli.ext, vec![sf!("publisher/example")]);
		assert_eq!(cli.ext_only, vec![PathBuf::from("local-ext")]);
		assert!(cli.no_workspace_ext);
		let Some(Command::Ext(args)) = cli.command else {
			panic!("ext command");
		};
		assert_eq!(args.project, PathBuf::from("."));
		let crate::ext_cli::ExtCommand::Install(install) = args.command else {
			panic!("ext install command");
		};
		assert_eq!(install.pool, Some(sf!("shared")));
		assert_eq!(install.specs, vec![sf!("publisher/example"), sf!("literal-spec")]);

		for arguments in [
			&["omp", "ext", "list"][..],
			&["omp", "ext", "info", "example"][..],
			&["omp", "ext", "install", "example"][..],
			&["omp", "ext", "uninstall", "example"][..],
			&["omp", "ext", "link", "example-dir"][..],
			&["omp", "ext", "unlink", "example"][..],
			&["omp", "ext", "enable", "example"][..],
			&["omp", "ext", "disable", "example"][..],
			&["omp", "ext", "features", "example", "--list"][..],
			&["omp", "ext", "lock"][..],
			&["omp", "ext", "resolve", "example"][..],
			&["omp", "ext", "sync"][..],
			&["omp", "ext", "upgrade"][..],
			&["omp", "ext", "pin", "example", "1.0.0"][..],
			&["omp", "ext", "unpin", "example"][..],
			&["omp", "ext", "gc"][..],
			&["omp", "ext", "doctor"][..],
			&["omp", "ext", "trust", "example"][..],
			&["omp", "ext", "verify"][..],
			&["omp", "ext", "bundle", "extensions.ompb"][..],
			&["omp", "ext", "publish"][..],
			&["omp", "ext", "search", "example"][..],
			&["omp", "ext", "index", "list"][..],
			&["omp", "ext", "where"][..],
			&["omp", "ext", "index", "add", "primary", "https://index.example"][..],
			&["omp", "ext", "index", "remove", "primary"][..],
		] {
			assert!(matches!(parse(arguments).command, Some(Command::Ext(_))), "{arguments:?}");
		}
	}

	#[test]
	fn rejects_unknown_ext_flags_as_usage_errors() {
		let error = OmpCli::try_parse_from(["omp", "ext", "list", "--unrecognized"])
			.expect_err("unknown extension flag must be rejected");
		assert_eq!(error.kind(), ErrorKind::UnknownArgument);
		assert_eq!(error.exit_code(), 2);
		assert!(error.to_string().contains("Usage:"));
	}

	#[test]
	fn parses_prompt_override_surface() {
		let cli = parse(&[
			"omp",
			"chat",
			"--personality=pragmatic",
			"--include-model-in-prompt=false",
			"--include-workstation",
			"--include-workspace-tree",
			"--render-mermaid=false",
			"--skills-enabled=false",
			"--system-prompt=SYSTEM.md",
			"--append-prompt=extra",
			"--null-prompt",
		]);
		let Some(Command::Chat(args)) = cli.command else {
			panic!("chat command");
		};
		assert_eq!(args.prompt_settings.personality, Some(omp_agent::Personality::Pragmatic));
		assert_eq!(args.prompt_settings.include_model_in_prompt, Some(false));
		assert_eq!(args.prompt_settings.include_workstation, Some(true));
		assert_eq!(args.prompt_settings.include_workspace_tree, Some(true));
		assert_eq!(args.prompt_settings.render_mermaid, Some(false));
		assert_eq!(args.prompt_settings.skills_enabled, Some(false));
		assert_eq!(args.prompt_settings.custom_prompt.as_deref(), Some("SYSTEM.md"));
		assert_eq!(args.prompt_settings.append_prompt.as_deref(), Some("extra"));
		assert!(args.prompt_settings.null_prompt);
	}

	#[test]
	fn parses_every_auth_branch() {
		assert!(matches!(
			parse(&["omp", "auth", "login", "provider"]).command,
			Some(Command::Auth(AuthArgs { command: AuthCommand::Login { .. }, .. }))
		));
		assert!(matches!(
			parse(&["omp", "auth", "list", "--provider", "provider"]).command,
			Some(Command::Auth(AuthArgs { command: AuthCommand::List { provider: Some(_) }, .. }))
		));
		assert!(matches!(
			parse(&["omp", "auth", "refresh", "account"]).command,
			Some(Command::Auth(AuthArgs { command: AuthCommand::Refresh { .. }, .. }))
		));
		assert!(matches!(
			parse(&["omp", "auth", "logout", "account"]).command,
			Some(Command::Auth(AuthArgs { command: AuthCommand::Logout { .. }, .. }))
		));
	}

	#[test]
	fn normalizes_bare_prompts_and_short_print_alias() {
		for arguments in [
			[OsString::from("omp"), OsString::from("explain"), OsString::from("this")],
			[OsString::from("omp"), OsString::from("-p"), OsString::from("explain")],
		] {
			let Some(Command::Print(args)) =
				parse_from_os(arguments).expect("print invocation").command
			else {
				panic!("print command");
			};
			assert_eq!(args.prompt[0], sf!("explain"));
		}
	}

	#[test]
	fn parses_print_inline_flags_and_posix_delimiter() {
		let Some(Command::Print(args)) = parse(&[
			"omp",
			"print",
			"--model=provider/model",
			"--mode=json",
			"--print-thoughts",
			"--",
			"--literal",
		])
		.command
		else {
			panic!("print command");
		};
		assert_eq!(args.model, Some(sf!("provider/model")));
		assert_eq!(args.mode, "json");
		assert!(args.print_thoughts);
		assert_eq!(args.prompt, vec![sf!("--literal")]);
	}

	#[test]
	fn print_rejects_invalid_mode_and_unknown_flags_as_usage_errors() {
		for arguments in [
			&["omp", "print", "--mode=xml", "hello"][..],
			&["omp", "print", "--mdoe", "text", "hello"][..],
		] {
			let error = OmpCli::try_parse_from(arguments).expect_err("invalid print usage");
			assert_eq!(error.exit_code(), 2);
			assert!(error.to_string().contains("error:"));
		}
	}

	#[test]
	fn hoists_global_flags_after_the_subcommand() {
		let cli = parse(&["omp", "print", "hello", "--no-ext", "--cwd=workspace"]);
		assert!(cli.no_ext);
		assert_eq!(cli.cwd, Some(PathBuf::from("workspace")));
	}

	#[test]
	fn routes_launch_options_around_launch_commands() {
		for arguments in [["omp", "--cwd", "workspace", "--model", "provider/model", "chat"], [
			"omp",
			"chat",
			"--cwd",
			"workspace",
			"--model",
			"provider/model",
		]] {
			let cli = parse_from_os(arguments.map(OsString::from)).expect("launch options");
			assert_eq!(cli.cwd, Some(PathBuf::from("workspace")));
			let Some(Command::Chat(args)) = cli.command else {
				panic!("chat command");
			};
			assert_eq!(args.model, Some(sf!("provider/model")));
		}
	}

	#[test]
	fn strips_leading_launch_options_from_non_launch_commands_only() {
		let cli = parse_from_os(
			["omp", "--cwd=workspace", "--model", "provider/model", "config", "list", "--json"]
				.map(OsString::from),
		)
		.expect("leading launch options are inapplicable to config");
		assert!(cli.cwd.is_none());
		assert!(matches!(
			cli.command,
			Some(Command::Config(ConfigArgs { command: ConfigCommand::List { json: true } }))
		));

		let error =
			parse_from_os(["omp", "config", "list", "--model", "provider/model"].map(OsString::from))
				.expect_err("a trailing launch option still belongs to config's strict parser");
		assert_eq!(error.kind(), ErrorKind::UnknownArgument);

		let cli = parse_from_os(["omp", "--json", "models"].map(OsString::from))
			.expect("a non-launch flag before its command is retained");
		assert!(matches!(cli.command, Some(Command::Models(ModelsArgs { json: true, .. }))));
	}

	#[test]
	fn parses_continue_selector_and_session_modes() {
		let Some(Command::Chat(args)) = parse(&[
			"omp",
			"chat",
			"--continue",
			"550e8400-e29b-41d4-a716-446655440000",
			"--session-dir",
			"sessions",
		])
		.command
		else {
			panic!("chat command");
		};
		assert_eq!(args.continue_session, Some(sf!("550e8400-e29b-41d4-a716-446655440000")));
		assert_eq!(args.session_dir, Some(PathBuf::from("sessions")));
		assert!(matches!(
			parse(&["omp", "chat", "--no-session"]).command,
			Some(Command::Chat(ChatArgs { no_session: true, .. }))
		));
	}
	#[test]
	fn validates_launch_levels_tiers_and_durations() {
		let Some(Command::Print(args)) = parse(&[
			"omp",
			"print",
			"--thinking=min",
			"--service-tier=priority",
			"--approval-mode=write",
			"--max-time=2m",
			"--tools=read,write",
			"--follow-up",
			"then summarize",
			"prompt",
		])
		.command
		else {
			panic!("print command");
		};
		assert_eq!(args.thinking, Some(ThinkingLevel::Minimal));
		assert_eq!(args.service_tier, Some(ServiceTier::Priority));
		assert_eq!(args.approval_mode, Some(ApprovalMode::Write));
		assert_eq!(args.max_time, Some(CliDuration(Duration::from_secs(120))));
		assert_eq!(args.follow_ups, vec![sf!("then summarize")]);
		assert_eq!(args.tools, Some(ToolNames(vec![sf!("read"), sf!("write")])));
		for arguments in [
			["omp", "print", "--thinking=inherit", "prompt"],
			["omp", "print", "--thinking=m", "prompt"],
			["omp", "print", "--max-time=0", "prompt"],
			["omp", "print", "--service-tier=fast", "prompt"],
			["omp", "print", "--tools=read,,write", "prompt"],
		] {
			assert_eq!(
				OmpCli::try_parse_from(arguments)
					.expect_err("invalid value")
					.exit_code(),
				2
			);
		}
	}

	#[test]
	fn session_index_is_explicit_while_chat_starts_inline() {
		let mut chat = ChatArgs::default_interactive();
		assert_eq!(chat_start(&mut chat), crate::chat::ChatStart::Session);
		let mut picker =
			ChatArgs { resume: Some(sf!("__omp_picker__")), ..ChatArgs::default_interactive() };
		assert_eq!(chat_start(&mut picker), crate::chat::ChatStart::SessionIndex);
		assert!(picker.resume.is_none());
	}

	#[test]
	fn normalizes_global_prefixed_bare_prompts_and_resume_picker() {
		let Some(Command::Print(args)) = parse_from_os([
			OsString::from("omp"),
			OsString::from("--cwd"),
			OsString::from("workspace"),
			OsString::from("explain"),
		])
		.expect("print")
		.command
		else {
			panic!("print command");
		};
		assert_eq!(args.prompt, vec![sf!("explain")]);
		let Some(Command::Chat(args)) =
			parse_from_os([OsString::from("omp"), OsString::from("resume")])
				.expect("resume")
				.command
		else {
			panic!("chat command");
		};
		assert_eq!(args.resume, Some(sf!("__omp_picker__")));
	}

	#[test]
	fn parses_config_models_and_broker_registry_entries() {
		assert!(matches!(
			parse(&["omp", "config", "init-xdg", "--json"]).command,
			Some(Command::Config(ConfigArgs { command: ConfigCommand::InitXdg { json: true } }))
		));
		assert!(matches!(
			parse(&["omp", "config", "set", "default_model", "provider/model"]).command,
			Some(Command::Config(_))
		));
		assert!(matches!(
			parse(&["omp", "models", "find", "model"]).command,
			Some(Command::Models(_))
		));
		assert!(matches!(
			parse(&["omp", "update", "--check"]).command,
			Some(Command::Update(UpdateArgs { check: true, .. }))
		));
		assert!(matches!(
			parse(&["omp", "registry", "--json"]).command,
			Some(Command::Registry(RegistryArgs { json: true, .. }))
		));
		assert!(matches!(
			parse(&["omp", "share", "--server", "https://share.example"]).command,
			Some(Command::Share(_))
		));
		assert!(matches!(
			parse(&["omp", "auth-broker", "status"]).command,
			Some(Command::AuthBroker(_))
		));
	}

	#[test]
	fn parses_worktree_inventory_and_pruning_flags() {
		assert!(matches!(
			parse(&["omp", "worktree", "list", "--json", "--all"]).command,
			Some(Command::Worktree(WorktreeArgs {
				command: WorktreeCommand::List { json: true, all: true },
			}))
		));
		assert!(matches!(
			parse(&["omp", "worktree", "clear", "--dry-run", "--all", "--json"]).command,
			Some(Command::Worktree(WorktreeArgs {
				command: WorktreeCommand::Clear { all: true, dry_run: true, json: true },
			}))
		));
	}

	#[test]
	fn rejects_incomplete_commands() {
		for arguments in [
			&["omp", "serve"][..],
			&["omp", "infer", "--model", "provider/model"][..],
			&["omp", "local", "infer"][..],
			&["omp", "catalog", "import", "--providers", "providers.toml", "--oauth", "oauth.toml"][..],
			&["omp", "auth", "login"][..],
		] {
			assert_eq!(
				OmpCli::try_parse_from(arguments)
					.expect_err("command must be rejected")
					.kind(),
				ErrorKind::MissingRequiredArgument
			);
		}
		// `--model` is optional now; a dangling `--gateway` fails on its
		// missing value instead of a missing required argument.
		assert_eq!(
			OmpCli::try_parse_from(["omp", "chat", "--gateway"])
				.expect_err("dangling gateway endpoint must be rejected")
				.kind(),
			ErrorKind::InvalidValue
		);
	}
}
