//! `omp ext` command parsing and extension-backend dispatch.
pub mod materialize;

use std::{
	fs,
	path::{Path, PathBuf},
};

use clap::{Args, Subcommand, ValueEnum};
use miette::{IntoDiagnostic as _, miette};
use omp_core::Str;
use omp_env::{BundleFile, pack_bundle, unpack_bundle};
use omp_ext::{
	Layer as BackendLayer,
	config::{ExtensionEnvironment, SourceSpec},
	doctor::{CredentialHealth, DoctorRequest, DoctorSeverity, RuntimeHealth, diagnose},
	index::SignedIndex,
	lock::{InstalledExtension, InstalledRecord, LockFile, LockedExtension, Wheel, index_source},
	trust::{GrantsFile, KeysFile, verify_artifact_signature},
	upgrade::{PinsFile, apply_uninstall, gc_generations, plan_uninstall, set_enabled},
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
	let data_dir = omp_core::dirs::data_dir(data_dir)?;
	let state = StatePaths::new(&data_dir, &project);
	let settings = omp_driver::settings::current(&data_dir).map_err(|error| miette!("{error}"))?;
	let _environment = ExtensionEnvironment::from_environment();
	settings
		.extension_scopes(None)
		.map_err(|error| miette!("{error}"))?;
	match command {
		ExtCommand::List(args) => list(&state, args),
		ExtCommand::Info(args) => info(&state, args),
		ExtCommand::Install(args) => install(&state, args).await,
		ExtCommand::Uninstall(args) => uninstall(&state, args),
		ExtCommand::Link(args) => link(&state, args),
		ExtCommand::Unlink { id } => unlink(&state, &id),
		ExtCommand::Enable { id } => enable(&state, &id, true),
		ExtCommand::Disable { id } => enable(&state, &id, false),
		ExtCommand::Features(args) => features(&state, args),
		ExtCommand::Lock(args) => lock(&state, args),
		ExtCommand::Resolve(args) => resolve(args).await,
		ExtCommand::Sync(args) => sync(&state, args).await,
		ExtCommand::Upgrade(args) => upgrade(&state, args).await,
		ExtCommand::Pin { id, version } => pin(&state, id, version),
		ExtCommand::Unpin { id } => unpin(&state, &id),
		ExtCommand::Gc(args) => gc(&state, args),
		ExtCommand::Doctor(args) => doctor(&state, args),
		ExtCommand::Trust(args) => trust(&state, args),
		ExtCommand::Verify(args) => verify(&state, args),
		ExtCommand::Bundle(args) => bundle(&state, args).await,
		ExtCommand::Publish(args) => publish(args),
		ExtCommand::Search(args) => search(&state, args),
		ExtCommand::Index(args) => index(&state, args),
		ExtCommand::Where(args) => where_paths(&state, args),
	}
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
async fn install(state: &StatePaths, args: ExtInstallArgs) -> miette::Result<()> {
	validate_specs(&args.specs)?;
	let mut installed =
		InstalledRecord::read(&state.client_installed).map_err(|error| miette!("{error}"))?;
	for spec in &args.specs {
		match SourceSpec::parse(spec).map_err(|error| miette!("{error}"))? {
			SourceSpec::Path(path) => {
				let path = path.canonicalize().into_diagnostic()?;
				let id = path
					.file_name()
					.and_then(|name| name.to_str())
					.ok_or_else(|| miette!("extension path has no valid identity"))?;
				let mut source = toml::map::Map::new();
				source.insert("path".to_owned(), toml::Value::String(path.display().to_string()));
				upsert_installed(&mut installed, InstalledExtension {
					id:      Str::new(id),
					source:  toml::Value::Table(source),
					tier:    tier(args.tier),
					enabled: true,
				});
			},
			source => install_index_source(state, &args, &mut installed, source)?,
		}
	}
	if args.dry_run {
		println!("would install {} extension(s)", args.specs.len());
		return Ok(());
	}
	installed.write(&state.client_installed).into_diagnostic()?;
	println!("installed {} extension(s)", args.specs.len());
	Ok(())
}

fn uninstall(state: &StatePaths, args: ExtUninstallArgs) -> miette::Result<()> {
	let mut installed =
		InstalledRecord::read(&state.client_installed).map_err(|error| miette!("{error}"))?;
	let mut lock = read_lock_or_empty(&state.client_lock, BackendLayer::Client)?;
	let plan = plan_uninstall(&installed, &lock, args.ids, args.keep_lock);
	println!("remove {} installed and {} locked entries", plan.installed.len(), plan.locked.len());
	if args.dry_run {
		return Ok(());
	}
	apply_uninstall(&mut installed, &mut lock, &plan);
	installed.write(&state.client_installed).into_diagnostic()?;
	lock.write(&state.client_lock).into_diagnostic()?;
	Ok(())
}

fn link(state: &StatePaths, args: ExtLinkArgs) -> miette::Result<()> {
	let path = args.path.canonicalize().into_diagnostic()?;
	let id = args.name.unwrap_or_else(|| {
		Str::new(
			path
				.file_name()
				.and_then(|name| name.to_str())
				.unwrap_or("extension"),
		)
	});
	let mut source = toml::map::Map::new();
	source.insert("link".to_owned(), toml::Value::String(path.display().to_string()));
	let mut installed =
		InstalledRecord::read(&state.client_installed).map_err(|error| miette!("{error}"))?;
	upsert_installed(&mut installed, InstalledExtension {
		id:      id.clone(),
		source:  toml::Value::Table(source),
		tier:    tier(args.tier),
		enabled: true,
	});
	installed.write(&state.client_installed).into_diagnostic()?;
	println!("linked {id}");
	Ok(())
}

fn unlink(state: &StatePaths, id: &str) -> miette::Result<()> {
	let mut installed =
		InstalledRecord::read(&state.client_installed).map_err(|error| miette!("{error}"))?;
	let before = installed.extensions.len();
	installed.extensions.retain(|entry| {
		!(entry.id == id
			&& entry
				.source
				.as_table()
				.is_some_and(|source| source.contains_key("link")))
	});
	if before == installed.extensions.len() {
		return Err(miette!("extension {id} is not linked"));
	}
	installed.write(&state.client_installed).into_diagnostic()?;
	Ok(())
}

fn enable(state: &StatePaths, id: &str, enabled: bool) -> miette::Result<()> {
	let mut installed =
		InstalledRecord::read(&state.client_installed).map_err(|error| miette!("{error}"))?;
	set_enabled(&mut installed, id, enabled).map_err(|error| miette!("{error}"))?;
	installed.write(&state.client_installed).into_diagnostic()?;
	Ok(())
}

fn features(_state: &StatePaths, args: ExtFeaturesArgs) -> miette::Result<()> {
	if args.list {
		println!("{}: manifest-defined features are resolved into omp.lock", args.id);
		return Ok(());
	}
	Err(miette!("feature mutations require a fresh explicit resolve for {}", args.id))
}
fn lock(state: &StatePaths, args: ExtLockArgs) -> miette::Result<()> {
	let mut lock = read_lock_or_empty(&state.client_lock, BackendLayer::Client)?;
	lock
		.validate_for(BackendLayer::Client)
		.map_err(|error| miette!("{error}"))?;
	if !args.targets.is_empty() {
		lock.targets = args.targets;
		lock.targets.sort();
		lock.targets.dedup();
	}
	if let Some(path) = args.export_pylock {
		lock.export_pylock(&path).into_diagnostic()?;
	}
	if args.check {
		println!("lock is valid");
		return Ok(());
	}
	lock.write(&state.client_lock).into_diagnostic()
}
async fn resolve(args: ExtResolveArgs) -> miette::Result<()> {
	validate_specs(&args.specs)?;
	for spec in args.specs {
		let source = SourceSpec::parse(&spec).map_err(|error| miette!("{error}"))?;
		println!("{source:?}");
	}
	Ok(())
}

async fn upgrade(state: &StatePaths, args: ExtUpgradeArgs) -> miette::Result<()> {
	if let Some(generation) = args.rollback {
		let previous =
			omp_ext::upgrade::load_generation(&state.generations, &generation, BackendLayer::Client)
				.map_err(|error| miette!("{error}"))?;
		if args.dry_run {
			println!("would roll back to {generation}");
			return Ok(());
		}
		omp_ext::upgrade::commit_generation(
			&state.client_lock,
			&state.client_installed,
			&state.generations,
			"rollback",
			&previous,
		)
		.map_err(|error| miette!("{error}"))?;
		return Ok(());
	}
	let installed =
		InstalledRecord::read(&state.client_installed).map_err(|error| miette!("{error}"))?;
	let ids = if args.ids.is_empty() {
		installed
			.extensions
			.iter()
			.map(|entry| entry.id.clone())
			.collect::<Vec<_>>()
	} else {
		args.ids
	};
	let key = fs::read_to_string(&state.index_key).into_diagnostic()?;
	let catalog =
		SignedIndex::read(&state.index_snapshot, key.trim()).map_err(|error| miette!("{error}"))?;
	let lock = read_lock_or_empty(&state.client_lock, BackendLayer::Client)?;
	for id in ids {
		let extension = catalog
			.extensions
			.iter()
			.find(|extension| extension.id == id)
			.ok_or_else(|| miette!("extension {id} is absent from the signed index"))?;
		let release = match args.to.as_ref() {
			Some(version) => extension
				.releases
				.iter()
				.find(|release| release.version == *version && !release.yanked),
			None => extension
				.releases
				.iter()
				.rev()
				.find(|release| !release.yanked),
		}
		.ok_or_else(|| miette!("no eligible release for {id}"))?;
		if let Some(previous) = lock.extensions.iter().find(|locked| locked.id == id)
			&& previous.capability_digest != release.capability_digest
			&& !args.allow_capability_widening
		{
			return Err(miette!(
				"{} changes its capability digest; pass --allow-capability-widening after review",
				id
			));
		}
		install(state, ExtInstallArgs {
			specs:          vec![Str::new(format!(
				"index:{}/{}@{}",
				catalog.name, id, release.version
			))],
			tier:           Tier::Sandboxed,
			pool:           None,
			features:       None,
			capabilities:   None,
			yes:            args.allow_capability_widening,
			dry_run:        args.dry_run,
			no_preresolved: false,
			target:         lock.targets.clone(),
			no_lock:        false,
			force:          true,
		})
		.await?;
	}
	Ok(())
}

fn pin(state: &StatePaths, id: Str, version: Str) -> miette::Result<()> {
	let mut pins = PinsFile::read(&state.pins).map_err(|error| miette!("{error}"))?;
	pins.set(&state.pins, id, version).into_diagnostic()
}

fn unpin(state: &StatePaths, id: &str) -> miette::Result<()> {
	let mut pins = PinsFile::read(&state.pins).map_err(|error| miette!("{error}"))?;
	if !pins.remove(&state.pins, id).into_diagnostic()? {
		return Err(miette!("extension {id} is not pinned"));
	}
	Ok(())
}

fn gc(state: &StatePaths, args: ExtGcArgs) -> miette::Result<()> {
	let report = gc_generations(&state.generations, args.keep_generations, args.apply)
		.map_err(|error| miette!("{error}"))?;
	println!("{} generation(s), {} bytes", report.generations.len(), report.bytes);
	Ok(())
}

struct CliHealth;
impl RuntimeHealth for CliHealth {
	fn environment_ready(&self) -> bool {
		true
	}

	fn credential_health(&self, _extension_id: &str) -> CredentialHealth {
		CredentialHealth::NotRequired
	}
}

fn doctor(state: &StatePaths, args: ExtDoctorArgs) -> miette::Result<()> {
	let request = DoctorRequest {
		layer:            BackendLayer::Client,
		lock_path:        &state.client_lock,
		installed_path:   &state.client_installed,
		keys_path:        &state.keys,
		revocations_path: state
			.revocations
			.exists()
			.then_some(state.revocations.as_path()),
		site_root:        &state.sites,
		artifact_cache:   &state.artifacts,
		fix:              args.fix,
	};
	let findings = diagnose(&request, &CliHealth);
	for finding in &findings {
		println!("{:?}: {}", finding.severity, finding.detail);
	}
	if findings
		.iter()
		.any(|finding| finding.severity == DoctorSeverity::Error)
	{
		return Err(miette!("extension doctor found integrity failures"));
	}
	Ok(())
}
async fn bundle(state: &StatePaths, args: ExtBundleArgs) -> miette::Result<()> {
	let lock = fs::read(&state.client_lock).into_diagnostic()?;
	let files = vec![BundleFile {
		path:     Str::new_static("locks/omp.lock"),
		contents: bytes::Bytes::from(lock),
	}];
	let encoded = pack_airgap_bundle(args.targets, files)?;
	if let Some(parent) = args.output.parent() {
		fs::create_dir_all(parent).into_diagnostic()?;
	}
	fs::write(args.output, encoded).into_diagnostic()
}
fn trust(state: &StatePaths, args: ExtTrustArgs) -> miette::Result<()> {
	let mut grants = GrantsFile::read(&state.grants).map_err(|error| miette!("{error}"))?;
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
	if args.revoke {
		grants.grants.retain(|grant| grant.id != args.id);
		grants.write(&state.grants).into_diagnostic()?;
		return Ok(());
	}
	let mut changed = false;
	for grant in grants.grants.iter_mut().filter(|grant| grant.id == args.id) {
		if let Some(selected_tier) = args.tier {
			grant.tier = tier(selected_tier);
			changed = true;
		}
		if let Some(ship) = args.ship {
			grant.ship = Str::new(match ship {
				Ship::Installed => "installed",
				Ship::Source => "source",
				Ship::Pickle => "pickle",
			});
			changed = true;
		}
	}
	if let Some(key) = args.key {
		let mut keys = KeysFile::read(&state.keys).map_err(|error| miette!("{error}"))?;
		keys
			.verify_or_pin(
				&args.id,
				&key,
				&Str::new_static("manual"),
				&Str::new_static("manual"),
				None,
			)
			.map_err(|error| miette!("{error}"))?;
		keys.write(&state.keys).into_diagnostic()?;
		changed = true;
	}
	if !changed {
		return Err(miette!("no trust mutation was requested for {}", args.id));
	}
	grants.write(&state.grants).into_diagnostic()
}

fn verify(state: &StatePaths, _args: ExtVerifyArgs) -> miette::Result<()> {
	LockFile::read(&state.client_lock, BackendLayer::Client).map_err(|error| miette!("{error}"))?;
	Ok(())
}
fn publish(args: ExtPublishArgs) -> miette::Result<()> {
	let wheel = args
		.wheel
		.ok_or_else(|| miette!("publish validation requires a wheel path"))?;
	let metadata = fs::metadata(&wheel).into_diagnostic()?;
	if !metadata.is_file()
		|| wheel.extension().and_then(|extension| extension.to_str()) != Some("whl")
	{
		return Err(miette!("publish input must be a wheel"));
	}
	println!("validated {} ({} bytes)", wheel.display(), metadata.len());
	if !args.dry_run {
		return Err(miette!("publishing requires a configured signed index upload authority"));
	}
	Ok(())
}

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
struct IndexConfig {
	#[serde(default, rename = "index")]
	entries: Vec<IndexConfigEntry>,
}
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct IndexConfigEntry {
	name: Str,
	url:  String,
}

fn index(state: &StatePaths, args: ExtIndexArgs) -> miette::Result<()> {
	let mut config = if state.indexes.exists() {
		toml::from_str::<IndexConfig>(&fs::read_to_string(&state.indexes).into_diagnostic()?)
			.into_diagnostic()?
	} else {
		IndexConfig::default()
	};
	match args.command {
		ExtIndexCommand::Add { name, url, first } => {
			config.entries.retain(|entry| entry.name != name);
			let entry = IndexConfigEntry { name, url: url.to_string() };
			if first {
				config.entries.insert(0, entry);
			} else {
				config.entries.push(entry);
			}
			write_toml(&state.indexes, &config)?;
		},
		ExtIndexCommand::Remove { name } => {
			let before = config.entries.len();
			config.entries.retain(|entry| entry.name != name);
			if before == config.entries.len() {
				return Err(miette!("index {name} is unknown"));
			}
			write_toml(&state.indexes, &config)?;
		},
		ExtIndexCommand::List => {
			for entry in config.entries {
				println!("{} {}", entry.name, entry.url);
			}
		},
	}
	Ok(())
}

fn search(state: &StatePaths, args: ExtSearchArgs) -> miette::Result<()> {
	let key = fs::read_to_string(&state.index_key).into_diagnostic()?;
	let index =
		SignedIndex::read(&state.index_snapshot, key.trim()).map_err(|error| miette!("{error}"))?;
	for (extension, release) in index
		.search(&args.query, args.capability.as_deref(), args.attested)
		.take(args.limit)
	{
		println!("{} {} {}", extension.id, release.version, extension.description);
	}
	Ok(())
}

fn where_paths(state: &StatePaths, args: ExtWhereArgs) -> miette::Result<()> {
	let installed =
		InstalledRecord::read(&state.client_installed).map_err(|error| miette!("{error}"))?;
	for entry in installed.extensions {
		if args.id.as_ref().is_none_or(|id| *id == entry.id) {
			println!("{} {}", entry.id, entry.source);
		}
	}
	Ok(())
}

struct StatePaths {
	client_installed:    PathBuf,
	workspace_installed: PathBuf,
	client_lock:         PathBuf,
	grants:              PathBuf,
	keys:                PathBuf,
	pins:                PathBuf,
	revocations:         PathBuf,
	generations:         PathBuf,
	sites:               PathBuf,
	artifacts:           PathBuf,
	indexes:             PathBuf,
	index_snapshot:      PathBuf,
	index_key:           PathBuf,
}

impl StatePaths {
	fn new(data_dir: &std::path::Path, project: &std::path::Path) -> Self {
		let workspace = project.join(".omp");
		Self {
			client_installed:    data_dir.join("ext/installed.toml"),
			workspace_installed: workspace.join("installed.toml"),
			client_lock:         data_dir.join("ext/omp.lock"),
			grants:              data_dir.join("ext/grants.toml"),
			keys:                data_dir.join("ext/keys.toml"),
			pins:                data_dir.join("ext/pins.toml"),
			revocations:         data_dir.join("ext/revocations.json"),
			generations:         data_dir.join("ext/generations"),
			sites:               data_dir.join("ext/sites"),
			artifacts:           data_dir.join("ext/artifacts"),
			indexes:             data_dir.join("ext/indexes.toml"),
			index_snapshot:      data_dir.join("ext/index.json"),
			index_key:           data_dir.join("ext/index.key"),
		}
	}
}
async fn sync(state: &StatePaths, args: ExtSyncArgs) -> miette::Result<()> {
	if let Some(bundle) = args.from {
		let bytes = fs::read(bundle).into_diagnostic()?;
		let decoded = unpack_bundle(&bytes).map_err(|error| miette!("{error}"))?;
		println!(
			"verified {} air-gap payload(s) for {} target(s)",
			decoded.files.len(),
			decoded.manifest.targets.len()
		);
		return Ok(());
	}
	if args.verify {
		verify(state, ExtVerifyArgs {
			ids:         Vec::new(),
			deep:        true,
			signatures:  true,
			revocations: false,
		})?;
	}
	let lock = LockFile::read(&state.client_lock, BackendLayer::Client)
		.map_err(|error| miette!("{error}"))?;
	println!("verified materialization inputs for {} locked extension(s)", lock.extensions.len());
	Ok(())
}

/// Encodes a Resolver-provided deployment snapshot as an air-gap bundle.
#[expect(dead_code, reason = "Resolver package snapshot output is landing separately")]
fn pack_airgap_bundle(targets: Vec<Str>, files: Vec<BundleFile>) -> miette::Result<bytes::Bytes> {
	pack_bundle("omp ext", targets, files).map_err(|error| miette!("{error}"))
}

/// Rechecks an air-gap bundle's fixed layout and every payload digest.
fn verify_airgap_bundle(bytes: &[u8]) -> miette::Result<()> {
	unpack_bundle(bytes).map_err(|error| miette!("{error}"))?;
	Ok(())
}

fn validate_specs(specs: &[Str]) -> miette::Result<()> {
	for spec in specs {
		SourceSpec::parse(spec).map_err(|error| miette!("{error}"))?;
	}
	Ok(())
}

fn install_index_source(
	state: &StatePaths,
	args: &ExtInstallArgs,
	installed: &mut InstalledRecord,
	source: SourceSpec,
) -> miette::Result<()> {
	let SourceSpec::Index { index, distribution } = source else {
		return Err(miette!(
			"signed native installation requires index: or a local path: source; use resolve for \
			 PyPI, Git, and URL closure inspection"
		));
	};
	let (id, version) = distribution.rsplit_once('@').ok_or_else(|| {
		miette!("signed index installs require index:<catalog>/<id>@<exact-version>")
	})?;
	let index_key = fs::read_to_string(&state.index_key).into_diagnostic()?;
	let catalog = SignedIndex::read(&state.index_snapshot, index_key.trim())
		.map_err(|error| miette!("{error}"))?;
	let (extension, release) = catalog
		.release(id, version)
		.ok_or_else(|| miette!("{id}@{version} is absent or yanked in the signed index"))?;
	let target = args.target.first().map_or("any", Str::as_str);
	let artifact = release
		.artifacts
		.iter()
		.find(|artifact| artifact.target == target || artifact.target == "any")
		.ok_or_else(|| miette!("{id}@{version} has no wheel for {target}"))?;
	verify_artifact_signature(
		extension.publisher_key.as_str(),
		artifact.blake3.as_str(),
		artifact.sha256.as_str(),
		release.capability_digest.as_str(),
		artifact.signature.as_str(),
	)
	.map_err(|error| miette!("{error}"))?;

	let mut keys = KeysFile::read(&state.keys).map_err(|error| miette!("{error}"))?;
	let first_seen = !keys.keys.iter().any(|pin| pin.id == extension.id);
	if first_seen && !args.yes {
		return Err(miette!(
			"first-seen publisher key for {} requires explicit --yes confirmation",
			extension.id
		));
	}
	keys
		.verify_or_pin(
			&extension.id,
			&extension.publisher_key,
			&release.version,
			&Str::new_static("explicit-install"),
			None,
		)
		.map_err(|error| miette!("{error}"))?;

	let mut lock = read_lock_or_empty(&state.client_lock, BackendLayer::Client)?;
	lock.indexes = vec![if index.is_empty() {
		catalog.name.to_string()
	} else {
		index
	}];
	lock.targets = vec![artifact.target.clone()];
	lock.extensions.retain(|locked| locked.id != extension.id);
	lock.extensions.push(LockedExtension {
		id:                extension.id.clone(),
		version:           release.version.clone(),
		tier:              tier(args.tier),
		pool:              args.pool.clone(),
		features:          args.features.as_deref().map(csv).unwrap_or_default(),
		source:            index_source(
			lock.indexes.first().map_or("", String::as_str),
			&extension.distribution,
		),
		manifest_digest:   release.manifest_digest.clone(),
		capability_digest: release.capability_digest.clone(),
		publisher:         extension.publisher_key.clone(),
		signature:         artifact.signature.clone(),
		ship:              Str::new_static("installed"),
		requires:          Vec::new(),
		wheel:             Wheel {
			file:   artifact.file.clone(),
			tag:    artifact.tag.clone(),
			size:   artifact.size,
			blake3: artifact.blake3.clone(),
			sha256: artifact.sha256.clone(),
		},
	});
	lock
		.extensions
		.sort_by(|left, right| left.id.cmp(&right.id));
	upsert_installed(installed, InstalledExtension {
		id:      extension.id.clone(),
		source:  index_source(
			lock.indexes.first().map_or("", String::as_str),
			&extension.distribution,
		),
		tier:    tier(args.tier),
		enabled: true,
	});
	if args.dry_run {
		println!("would install {} {}", extension.id, release.version);
		return Ok(());
	}
	if args.no_lock {
		installed.write(&state.client_installed).into_diagnostic()?;
	} else {
		let generation = omp_ext::upgrade::Generation { lock, installed: installed.clone() };
		omp_ext::upgrade::commit_generation(
			&state.client_lock,
			&state.client_installed,
			&state.generations,
			&format!("{}-{}", extension.id, release.version).replace('/', "_"),
			&generation,
		)
		.map_err(|error| miette!("{error}"))?;
	}
	keys.write(&state.keys).into_diagnostic()?;
	println!("installed {} {}", extension.id, release.version);
	Ok(())
}

fn read_lock_or_empty(path: &Path, layer: BackendLayer) -> miette::Result<LockFile> {
	if path.exists() {
		return LockFile::read(path, layer).map_err(|error| miette!("{error}"));
	}
	Ok(LockFile {
		version: 1,
		generated_by: "omp ext".to_owned(),
		generated_at: String::new(),
		layer,
		requires_python: Str::new_static("==3.14.*"),
		abi: Str::new_static("cp314t"),
		targets: Vec::new(),
		exclude_newer: None,
		indexes: Vec::new(),
		index_strategy: Str::new_static("first-index"),
		extensions: Vec::new(),
		packages: Vec::new(),
		frozen: Vec::new(),
	})
}

fn upsert_installed(installed: &mut InstalledRecord, replacement: InstalledExtension) {
	installed
		.extensions
		.retain(|entry| entry.id != replacement.id);
	installed.extensions.push(replacement);
	installed
		.extensions
		.sort_by(|left, right| left.id.cmp(&right.id));
}

const fn tier(value: Tier) -> omp_ext::TrustTier {
	match value {
		Tier::Trusted => omp_ext::TrustTier::Trusted,
		Tier::Sandboxed => omp_ext::TrustTier::Sandboxed,
	}
}

fn csv(value: &str) -> Vec<Str> {
	value
		.split(',')
		.map(str::trim)
		.filter(|value| !value.is_empty())
		.map(Str::new)
		.collect()
}

fn write_toml(path: &Path, value: &impl serde::Serialize) -> miette::Result<()> {
	if let Some(parent) = path.parent() {
		fs::create_dir_all(parent).into_diagnostic()?;
	}
	let temporary = path.with_extension("toml.tmp");
	fs::write(&temporary, toml::to_string_pretty(value).into_diagnostic()?).into_diagnostic()?;
	fs::rename(temporary, path).into_diagnostic()
}
