//! `omp ext` command parsing and extension-backend dispatch.

use std::path::PathBuf;

use clap::{Args, Subcommand, ValueEnum};
use miette::{IntoDiagnostic as _, miette};
use omp_core::Str;
use omp_env::{BundleFile, EnvClient, frame::MaterializeSite, pack_bundle, unpack_bundle};

use crate::{
	ext::{
		Layer as BackendLayer,
		config::{ExtensionEnvironment, SourceSpec},
		lock::{InstalledRecord, LockFile},
		trust::GrantsFile,
	},
	settings::Settings,
};

/// Shared options accepted by every `omp ext` operation.
#[derive(Clone, Debug, Args)]
pub struct ExtArgs {
	/// Workspace root whose extension layer and lock are selected.
	#[arg(long, global = true, value_name = "PATH", default_value = ".")]
	pub project:       PathBuf,
	/// Client-scope extension state root.
	#[arg(long, global = true, value_name = "PATH")]
	pub data_dir:      Option<PathBuf>,
	/// Extension store root, equivalent to `OMP_EXT_STORE`.
	#[arg(long, global = true, value_name = "PATH")]
	pub store:         Option<PathBuf>,
	/// Download cache root, equivalent to `OMP_EXT_CACHE`.
	#[arg(long, global = true, value_name = "PATH")]
	pub cache:         Option<PathBuf>,
	/// Resolution index URL, equivalent to `OMP_EXT_INDEX`.
	#[arg(long, global = true, value_name = "URL")]
	pub index:         Vec<Str>,
	/// Index public-key file, equivalent to `OMP_EXT_INDEX_KEYS`.
	#[arg(long, global = true, value_name = "PATH")]
	pub index_keys:    Option<PathBuf>,
	/// Forbid network access, equivalent to `OMP_EXT_OFFLINE`.
	#[arg(long, global = true)]
	pub offline:       bool,
	/// Refuse to modify a lock, equivalent to `OMP_EXT_LOCKED`.
	#[arg(long, global = true)]
	pub locked:        bool,
	/// Default reproducibility cutoff, equivalent to `OMP_EXT_EXCLUDE_NEWER`.
	#[arg(long, global = true, value_name = "DATE")]
	pub exclude_newer: Option<Str>,
	/// Disable extension identities, equivalent to `OMP_EXT_DISABLE`.
	#[arg(long, global = true, value_delimiter = ',', value_name = "ID")]
	pub disable:       Vec<Str>,
	/// Non-interactive capability grants, equivalent to `OMP_EXT_GRANT`.
	#[arg(long, global = true, value_name = "GRANT")]
	pub grant:         Option<Str>,
	/// Permit local source builds, equivalent to `OMP_EXT_ALLOW_BUILD`.
	#[arg(long, global = true)]
	pub allow_build:   bool,
	/// Publisher signing key, equivalent to `OMP_EXT_SIGN_KEY`.
	#[arg(long, global = true, value_name = "PATH")]
	pub sign_key:      Option<PathBuf>,
	/// `uv` executable path, equivalent to `OMP_EXT_UV`.
	#[arg(long, global = true, value_name = "PATH")]
	pub uv:            Option<PathBuf>,
	/// Default target triples, equivalent to `OMP_EXT_TARGETS`.
	#[arg(long, global = true, value_delimiter = ',', value_name = "TRIPLE")]
	pub targets:       Vec<Str>,
	/// Trace resolution and verification, equivalent to `OMP_EXT_TRACE`.
	#[arg(long, global = true)]
	pub trace:         bool,
	/// Environment socket passed to host children, equivalent to
	/// `OMP_EXT_ENV_SOCKET`.
	#[arg(long, global = true, value_name = "PATH")]
	pub env_socket:    Option<PathBuf>,
	/// Which extension layer to inspect or change.
	#[arg(long, global = true, value_enum)]
	pub layer:         Option<Layer>,
	/// Install-record scope for mutations.
	#[arg(long, global = true, value_enum, default_value_t = Scope::User)]
	pub scope:         Scope,
	/// Emit machine-readable output on stdout.
	#[arg(long, global = true)]
	pub json:          bool,
	/// Include resolver and verification detail.
	#[arg(short, long, global = true)]
	pub verbose:       bool,
	/// Extension operation.
	#[command(subcommand)]
	pub command:       ExtCommand,
}

/// The layer selected by an extension operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Layer {
	/// Select the client layer.
	Client,
	/// Select the workspace layer.
	Workspace,
	/// Select both layers.
	All,
}

/// The scope containing an extension installation record.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Scope {
	/// Select the user-level install record.
	User,
	/// Select the project-level install record.
	Project,
}

/// The containment tier granted to an extension.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Tier {
	/// Permit trusted in-process-adjacent code shipping.
	Trusted,
	/// Require sandboxed execution.
	Sandboxed,
}

/// Code shipping level for a trusted extension.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Ship {
	/// Ship the installed artifact.
	Installed,
	/// Ship source code.
	Source,
	/// Ship serialized code; requires the trusted tier.
	Pickle,
}

/// `omp ext` operations.
#[derive(Clone, Debug, Subcommand)]
pub enum ExtCommand {
	/// List installed and admitted extensions with provenance and signature
	/// state.
	List(ExtListArgs),
	/// Display an extension record and declared-versus-registered capabilities.
	Info(ExtInfoArgs),
	/// Resolve, verify, consent to, and install extension specifications.
	Install(ExtInstallArgs),
	/// Remove extension installation records.
	Uninstall(ExtUninstallArgs),
	/// Register a local extension directory.
	Link(ExtLinkArgs),
	/// Remove a local extension link.
	Unlink {
		/// Extension identity to unlink.
		id: Str,
	},
	/// Admit an installed extension and notify resident host digests.
	Enable {
		/// Extension identity to enable.
		id: Str,
	},
	/// Withdraw declarations from an extension and notify resident host digests.
	Disable {
		/// Extension identity to disable.
		id: Str,
	},
	/// Inspect or modify enabled extension features.
	Features(ExtFeaturesArgs),
	/// Write or verify the extension lock.
	Lock(ExtLockArgs),
	/// Resolve extension specifications without writing state.
	Resolve(ExtResolveArgs),
	/// Materialize managed site trees from locks.
	Sync(ExtSyncArgs),
	/// Upgrade installed extension identities.
	Upgrade(ExtUpgradeArgs),
	/// Pin an extension version.
	Pin {
		/// Extension identity to pin.
		id:      Str,
		/// Version to pin.
		version: Str,
	},
	/// Remove an extension version pin.
	Unpin {
		/// Extension identity to unpin.
		id: Str,
	},
	/// Report or collect unreachable extension artifacts.
	Gc(ExtGcArgs),
	/// Check extension lock, site tree, integrity, environment, and API health.
	Doctor(ExtDoctorArgs),
	/// Inspect or change an extension trust grant.
	Trust(ExtTrustArgs),
	/// Recheck artifact integrity, signatures, and revocations.
	Verify(ExtVerifyArgs),
	/// Build an air-gap extension bundle.
	Bundle(ExtBundleArgs),
	/// Validate or upload an extension distribution.
	Publish(ExtPublishArgs),
	/// Query the extension catalog.
	#[command(visible_alias = "discover")]
	Search(ExtSearchArgs),
	/// Manage the ordered extension index list.
	Index(ExtIndexArgs),
	/// Print resolved extension paths.
	Where(ExtWhereArgs),
}

/// Filters for `omp ext list`.
#[derive(Clone, Debug, Args)]
pub struct ExtListArgs {
	/// Show only enabled extensions.
	#[arg(long, conflicts_with = "disabled")]
	pub enabled:  bool,
	/// Show only disabled extensions.
	#[arg(long, conflicts_with = "enabled")]
	pub disabled: bool,
	/// Filter by containment tier.
	#[arg(long, value_enum)]
	pub tier:     Option<Tier>,
	/// Filter by sharing group; an empty value selects isolated extensions.
	#[arg(long, value_name = "NAME")]
	pub pool:     Option<Str>,
	/// Show only extensions with a newer available version.
	#[arg(long)]
	pub outdated: bool,
	/// Show only unsigned extensions.
	#[arg(long)]
	pub unsigned: bool,
	/// Include dependency closure and extension edges.
	#[arg(long)]
	pub tree:     bool,
}

/// Selectors for `omp ext info`.
#[derive(Clone, Debug, Args)]
pub struct ExtInfoArgs {
	/// Extension identity.
	pub id:           Str,
	/// Print only declared and registered capabilities with their digest.
	#[arg(long)]
	pub capabilities: bool,
	/// Print only the lock entry.
	#[arg(long)]
	pub lock:         bool,
	/// Print only store, site-tree, and binary paths.
	#[arg(long)]
	pub paths:        bool,
}

/// Options for `omp ext install`.
#[derive(Clone, Debug, Args)]
pub struct ExtInstallArgs {
	/// Extension specifications to install.
	#[arg(required = true, value_name = "SPEC")]
	pub specs:          Vec<Str>,
	/// Requested containment tier.
	#[arg(long, value_enum, default_value_t = Tier::Sandboxed)]
	pub tier:           Tier,
	/// Sharing group; omitted means isolated.
	#[arg(long, value_name = "NAME")]
	pub pool:           Option<Str>,
	/// Replace manifest-default enabled features.
	#[arg(long, value_name = "FEATURES")]
	pub features:       Option<Str>,
	/// Grant exactly these declared capabilities.
	#[arg(long, value_name = "CAPS")]
	pub capabilities:   Option<Str>,
	/// Grant all manifest-declared capabilities after showing the diff.
	#[arg(long)]
	pub yes:            bool,
	/// Resolve and verify but do not write state.
	#[arg(long)]
	pub dry_run:        bool,
	/// Ignore index pre-resolved closures.
	#[arg(long)]
	pub no_preresolved: bool,
	/// Resolve for these targets.
	#[arg(long, value_delimiter = ',', value_name = "TRIPLE")]
	pub target:         Vec<Str>,
	/// Do not write a lock.
	#[arg(long)]
	pub no_lock:        bool,
	/// Reinstall and re-verify already satisfied specifications.
	#[arg(long)]
	pub force:          bool,
}

/// Options for `omp ext uninstall`.
#[derive(Clone, Debug, Args)]
pub struct ExtUninstallArgs {
	/// Extension identities to remove.
	#[arg(required = true, value_name = "ID")]
	pub ids:        Vec<Str>,
	/// Retain the grant record.
	#[arg(long)]
	pub keep_grant: bool,
	/// Retain the lock entry.
	#[arg(long)]
	pub keep_lock:  bool,
	/// Remove extension state and fetched binaries.
	#[arg(long)]
	pub purge:      bool,
	/// Print removals without changing state.
	#[arg(long)]
	pub dry_run:    bool,
}

/// Options for `omp ext link`.
#[derive(Clone, Debug, Args)]
pub struct ExtLinkArgs {
	/// Local extension directory.
	pub path:       PathBuf,
	/// Requested containment tier.
	#[arg(long, value_enum, default_value_t = Tier::Sandboxed)]
	pub tier:       Tier,
	/// Override the manifest identity.
	#[arg(long, value_name = "ID")]
	pub name:       Option<Str>,
	/// Replace manifest-default enabled features.
	#[arg(long, value_name = "FEATURES")]
	pub features:   Option<Str>,
	/// Record the link without resolving requirements.
	#[arg(long)]
	pub no_resolve: bool,
}

/// Options for `omp ext features`.
#[derive(Clone, Debug, Args)]
pub struct ExtFeaturesArgs {
	/// Extension identity.
	pub id:      Str,
	/// Add enabled features.
	#[arg(long, value_name = "FEATURES", conflicts_with = "set")]
	pub enable:  Option<Str>,
	/// Remove enabled features.
	#[arg(long, value_name = "FEATURES", conflicts_with = "set")]
	pub disable: Option<Str>,
	/// Replace enabled features.
	#[arg(long, value_name = "FEATURES", conflicts_with_all = ["enable", "disable"])]
	pub set:     Option<Str>,
	/// List available features and requirements.
	#[arg(long)]
	pub list:    bool,
}

/// Options for `omp ext lock`.
#[derive(Clone, Debug, Args)]
pub struct ExtLockArgs {
	/// Target triples to write into the lock.
	#[arg(long, value_delimiter = ',', value_name = "TRIPLE")]
	pub targets:         Vec<Str>,
	/// Resolve all packages to their newest permitted versions.
	#[arg(long)]
	pub upgrade:         bool,
	/// Resolve only these distributions anew.
	#[arg(long, value_name = "NAME")]
	pub upgrade_package: Vec<Str>,
	/// Verify whether the lock would change without writing it.
	#[arg(long)]
	pub check:           bool,
	/// Also write a PEP 751 lock.
	#[arg(long, value_name = "PATH")]
	pub export_pylock:   Option<PathBuf>,
}

/// Options for `omp ext resolve`.
#[derive(Clone, Debug, Args)]
pub struct ExtResolveArgs {
	/// Extension specifications to resolve.
	#[arg(required = true, value_name = "SPEC")]
	pub specs:        Vec<Str>,
	/// Print the resolution graph, rules, and equivalent uv invocation.
	#[arg(long)]
	pub explain:      bool,
	/// Resolve layers as one local host.
	#[arg(long)]
	pub as_if_local:  bool,
	/// Resolve for these targets.
	#[arg(long, value_delimiter = ',', value_name = "TRIPLE")]
	pub target:       Vec<Str>,
	/// Print only the minimal unsatisfiable core on failure.
	#[arg(long)]
	pub minimal_core: bool,
}

/// Options for `omp ext sync`.
#[derive(Clone, Debug, Args)]
pub struct ExtSyncArgs {
	/// Remove site entries absent from the lock.
	#[arg(long)]
	pub prune:  bool,
	/// Provision this worker through the Rust supervisor.
	#[arg(long, value_name = "NAME")]
	pub worker: Option<Str>,
	/// Re-verify every locked artifact.
	#[arg(long)]
	pub verify: bool,
	/// Materialize from an air-gap bundle.
	#[arg(long, value_name = "BUNDLE")]
	pub from:   Option<PathBuf>,
}

/// Options for `omp ext upgrade`.
#[derive(Clone, Debug, Args)]
pub struct ExtUpgradeArgs {
	/// Extension identities to upgrade.
	pub ids: Vec<Str>,
	/// Exact target version for one identity.
	#[arg(long, value_name = "VERSION")]
	pub to: Option<Str>,
	/// Print the plan and capability diff only.
	#[arg(long)]
	pub dry_run: bool,
	/// Allow widened capabilities non-interactively.
	#[arg(long)]
	pub allow_capability_widening: bool,
	/// Restore this identity's previous resolution.
	#[arg(long, value_name = "ID")]
	pub rollback: Option<Str>,
}

/// Options for `omp ext gc`.
#[derive(Clone, Debug, Args)]
pub struct ExtGcArgs {
	/// Actually delete unreachable artifacts; omitted is a dry run.
	#[arg(long)]
	pub apply:            bool,
	/// Retain this many resolution generations per host key.
	#[arg(long, value_name = "N", default_value_t = 2)]
	pub keep_generations: usize,
	/// Retain the downloaded-artifact cache.
	#[arg(long)]
	pub keep_cache:       bool,
	/// Consider locks for every known workspace.
	#[arg(long)]
	pub all_projects:     bool,
}

/// Options for `omp ext doctor`.
#[derive(Clone, Debug, Args)]
pub struct ExtDoctorArgs {
	/// Repair mechanically repairable integrity and site-tree failures.
	#[arg(long)]
	pub fix: bool,
}

/// Options for `omp ext trust`.
#[derive(Clone, Debug, Args)]
pub struct ExtTrustArgs {
	/// Extension identity.
	pub id:     Str,
	/// Print the current trust grant only.
	#[arg(long)]
	pub show:   bool,
	/// Change containment tier after consent.
	#[arg(long, value_enum)]
	pub tier:   Option<Tier>,
	/// Change code-shipping level.
	#[arg(long, value_enum)]
	pub ship:   Option<Ship>,
	/// Accept this publisher-key fingerprint.
	#[arg(long, value_name = "FINGERPRINT")]
	pub key:    Option<Str>,
	/// Drop the grant without uninstalling.
	#[arg(long)]
	pub revoke: bool,
}

/// Options for `omp ext verify`.
#[derive(Clone, Debug, Args)]
pub struct ExtVerifyArgs {
	/// Extension identities to inspect; omitted means all.
	pub ids:         Vec<Str>,
	/// Hash every file against `RECORD`.
	#[arg(long)]
	pub deep:        bool,
	/// Recheck signatures and attestations.
	#[arg(long)]
	pub signatures:  bool,
	/// Refresh the revocation list first.
	#[arg(long)]
	pub revocations: bool,
}

/// Options for `omp ext bundle`.
#[derive(Clone, Debug, Args)]
pub struct ExtBundleArgs {
	/// Destination bundle path.
	pub output:          PathBuf,
	/// Target triples to include.
	#[arg(long, value_delimiter = ',', value_name = "TRIPLE")]
	pub targets:         Vec<Str>,
	/// Layer to bundle.
	#[arg(long, value_enum)]
	pub layer:           Option<Layer>,
	/// Embed catalog metadata for offline search.
	#[arg(long)]
	pub include_catalog: bool,
	/// Include publisher keys.
	#[arg(long, default_value_t = true)]
	pub include_keys:    bool,
}

/// Options for `omp ext publish`.
#[derive(Clone, Debug, Args)]
pub struct ExtPublishArgs {
	/// Distribution wheel to publish.
	#[arg(value_name = "WHEEL")]
	pub wheel:   Option<PathBuf>,
	/// Request index attestation review.
	#[arg(long)]
	pub attest:  bool,
	/// Validate locally without uploading.
	#[arg(long)]
	pub dry_run: bool,
}

/// Options for `omp ext search`.
#[derive(Clone, Debug, Args)]
pub struct ExtSearchArgs {
	/// Catalog query.
	pub query:      Str,
	/// Maximum result count.
	#[arg(long, default_value_t = 20)]
	pub limit:      usize,
	/// Require a declared capability.
	#[arg(long, value_name = "CAPABILITY")]
	pub capability: Option<Str>,
	/// Show reviewed extensions only.
	#[arg(long)]
	pub attested:   bool,
}

/// Index-management command tree.
#[derive(Clone, Debug, Args)]
pub struct ExtIndexArgs {
	/// Index operation.
	#[command(subcommand)]
	pub command: ExtIndexCommand,
}

/// Index-management operations.
#[derive(Clone, Debug, Subcommand)]
pub enum ExtIndexCommand {
	/// Add a named index URL.
	Add {
		/// Index name.
		name:  Str,
		/// Index URL.
		url:   Str,
		/// Put the index first.
		#[arg(long)]
		first: bool,
	},
	/// Remove a named index URL.
	Remove {
		/// Index name.
		name: Str,
	},
	/// List configured index URLs.
	List,
}

/// Options for `omp ext where`.
#[derive(Clone, Debug, Args)]
pub struct ExtWhereArgs {
	/// Optional extension identity.
	pub id: Option<Str>,
}

/// Dispatches a parsed extension command to its dedicated backend seam.
pub async fn run(args: ExtArgs) -> miette::Result<()> {
	let ExtArgs { data_dir, project, command, .. } = args;
	let data_dir = crate::cli::data_dir(data_dir)?;
	let state = StatePaths::new(&data_dir, &project);
	let settings = Settings::load_checked(&data_dir).map_err(|error| miette!("{error}"))?;
	let _environment = ExtensionEnvironment::from_environment();
	settings
		.extension_scopes(None)
		.map_err(|error| miette!("{error}"))?;
	match command {
		ExtCommand::List(args) => list(&state, args),
		ExtCommand::Info(args) => info(&state, args),
		ExtCommand::Install(args) => install(args).await,
		ExtCommand::Uninstall(_) => uninstall(),
		ExtCommand::Link(_) => link(),
		ExtCommand::Unlink { .. } => unlink(),
		ExtCommand::Enable { .. } => enable(),
		ExtCommand::Disable { .. } => disable(),
		ExtCommand::Features(_) => features(),
		ExtCommand::Lock(args) => lock(&state, args),
		ExtCommand::Resolve(args) => resolve(args).await,
		ExtCommand::Sync(args) => sync(args).await,
		ExtCommand::Upgrade(_) => upgrade(),
		ExtCommand::Pin { .. } => pin(),
		ExtCommand::Unpin { .. } => unpin(),
		ExtCommand::Gc(_) => gc(),
		ExtCommand::Doctor(_) => doctor(),
		ExtCommand::Trust(args) => trust(&state, args),
		ExtCommand::Verify(args) => verify(&state, args),
		ExtCommand::Bundle(args) => bundle(args).await,
		ExtCommand::Publish(_) => publish(),
		ExtCommand::Search(_) => search(),
		ExtCommand::Index(_) => index(),
		ExtCommand::Where(_) => where_paths(),
	}
}

macro_rules! unavailable_backend {
	($name:ident, $backend:literal) => {
		fn $name() -> miette::Result<()> {
			Err(miette!("omp ext {} backend is not available yet", $backend))
		}
	};
}
fn list(state: &StatePaths, _args: ExtListArgs) -> miette::Result<()> {
	let client =
		InstalledRecord::read(&state.client_installed).map_err(|error| miette!("{error}"))?;
	let workspace =
		InstalledRecord::read(&state.workspace_installed).map_err(|error| miette!("{error}"))?;
	println!("{} extensions", client.extensions.len() + workspace.extensions.len());
	Ok(())
}

fn info(state: &StatePaths, args: ExtInfoArgs) -> miette::Result<()> {
	let client =
		InstalledRecord::read(&state.client_installed).map_err(|error| miette!("{error}"))?;
	let workspace =
		InstalledRecord::read(&state.workspace_installed).map_err(|error| miette!("{error}"))?;
	let installed = client
		.extensions
		.iter()
		.chain(&workspace.extensions)
		.find(|entry| entry.id == args.id)
		.ok_or_else(|| miette!("extension {} is unknown", args.id))?;
	println!(
		"{} {:?} {}",
		installed.id,
		installed.tier,
		if installed.enabled {
			"enabled"
		} else {
			"disabled"
		}
	);
	Ok(())
}
async fn install(args: ExtInstallArgs) -> miette::Result<()> {
	validate_specs(&args.specs)?;
	Err(miette!("omp ext install requires the Resolver manifest-to-hash-pinned-closure backend"))
}
unavailable_backend!(uninstall, "uninstall");
unavailable_backend!(link, "link");
unavailable_backend!(unlink, "unlink");
unavailable_backend!(enable, "enable");
unavailable_backend!(disable, "disable");
unavailable_backend!(features, "features");
fn lock(state: &StatePaths, _args: ExtLockArgs) -> miette::Result<()> {
	let path = &state.client_lock;
	LockFile::read(path, BackendLayer::Client).map_err(|error| miette!("{error}"))?;
	Err(miette!("omp ext lock mutation requires the Resolver closure writer"))
}
async fn resolve(args: ExtResolveArgs) -> miette::Result<()> {
	validate_specs(&args.specs)?;
	Err(miette!("omp ext resolve requires the Resolver manifest-to-hash-pinned-closure backend"))
}
unavailable_backend!(upgrade, "upgrade");
unavailable_backend!(pin, "pin");
unavailable_backend!(unpin, "unpin");
unavailable_backend!(gc, "gc");
unavailable_backend!(doctor, "doctor");
async fn bundle(_args: ExtBundleArgs) -> miette::Result<()> {
	Err(miette!(
		"omp ext bundle requires the Resolver package snapshot API to provide lock and artifact \
		 payloads"
	))
}
fn trust(state: &StatePaths, args: ExtTrustArgs) -> miette::Result<()> {
	let grants = GrantsFile::read(&state.grants).map_err(|error| miette!("{error}"))?;
	if args.show {
		println!(
			"{} grants",
			grants
				.grants
				.iter()
				.filter(|grant| grant.id == args.id)
				.count()
		);
		return Ok(());
	}
	Err(miette!("omp ext trust mutation requires the Resolver consent transaction backend"))
}

fn verify(state: &StatePaths, _args: ExtVerifyArgs) -> miette::Result<()> {
	LockFile::read(&state.client_lock, BackendLayer::Client).map_err(|error| miette!("{error}"))?;
	Ok(())
}
unavailable_backend!(index, "index");
unavailable_backend!(publish, "publish");
unavailable_backend!(search, "search");
unavailable_backend!(where_paths, "where");

struct StatePaths {
	client_installed:    PathBuf,
	workspace_installed: PathBuf,
	client_lock:         PathBuf,
	grants:              PathBuf,
}

impl StatePaths {
	fn new(data_dir: &std::path::Path, project: &std::path::Path) -> Self {
		let workspace = project.join(".omp");
		Self {
			client_installed:    data_dir.join("ext/installed.toml"),
			workspace_installed: workspace.join("installed.toml"),
			client_lock:         data_dir.join("ext/omp.lock"),
			grants:              data_dir.join("ext/grants.toml"),
		}
	}
}
async fn sync(_args: ExtSyncArgs) -> miette::Result<()> {
	Err(miette!(
		"omp ext sync requires the Resolver materialization-plan API before it can call \
		 EnvClient::materialize_site"
	))
}

/// Encodes a Resolver-provided deployment snapshot as an air-gap bundle.
#[expect(dead_code, reason = "Resolver package snapshot output is landing separately")]
fn pack_airgap_bundle(targets: Vec<Str>, files: Vec<BundleFile>) -> miette::Result<bytes::Bytes> {
	pack_bundle("omp ext", targets, files).map_err(|error| miette!("{error}"))
}

/// Rechecks an air-gap bundle's fixed layout and every payload digest.
#[expect(dead_code, reason = "bundle verification is selected by the future package backend")]
fn verify_airgap_bundle(bytes: &[u8]) -> miette::Result<()> {
	unpack_bundle(bytes).map_err(|error| miette!("{error}"))?;
	Ok(())
}
/// Sends a resolver-produced site-materialization request to the owner
/// environment.
#[expect(dead_code, reason = "Resolver materialization-plan output is landing separately")]
async fn materialize_site(client: &EnvClient, request: MaterializeSite) -> miette::Result<()> {
	client.materialize_site(request).await.into_diagnostic()?;
	Ok(())
}

fn validate_specs(specs: &[Str]) -> miette::Result<()> {
	for spec in specs {
		SourceSpec::parse(spec).map_err(|error| miette!("{error}"))?;
	}
	Ok(())
}
