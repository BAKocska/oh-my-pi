//! Filesystem capability discovery and runtime model-discovery normalization.

pub mod active_repo;
pub mod at_path;
pub mod cache;
pub mod containment;
pub mod context;
pub mod custom_tools;
pub mod foreign;
pub mod managed_skills;
pub mod manifest;
pub mod mcp;
pub mod mcp_ssh;
pub mod models;
pub mod native;
pub mod packages;
pub mod project;
pub mod prompts;
pub mod registry;
pub mod roles;
pub mod rules;
pub mod runtime;
pub mod settings;
pub mod skills;
pub mod slash_commands;

use std::{
	collections::BTreeMap,
	env,
	path::{Path, PathBuf},
	sync::Arc,
};

use futures::future::join_all;
use omp_catalog::{
	ContextStrategy, Pricing, RouteId, ThinkingPolicyId, WirePolicyId,
	discover::{DiscoveredModel, DiscoveryDefaults, DiscoveryNormalizer, NormalizedDiscovery},
};
use omp_core::Str;

use self::{
	foreign::ForeignContentSettings,
	manifest::{
		CapabilityKind, CapabilityPayload, CapabilityRecord, DiscoveredCapability, SourceScope,
	},
	native::NativeDiscoveryOptions,
	registry::{CAPABILITY_KINDS, CapabilityResult, DiscoveryRegistry, LoadContext, LoadOptions},
	skills::SkillDiscoverySettings,
};
use crate::{
	rulebook::{RuleSnapshot, RulebookSettings},
	skills::SkillSnapshot,
};
/// One command contributed by native content discovery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandContribution {
	/// Primary spelling without `/`.
	pub name:        Str,
	/// Alternate spellings.
	pub aliases:     Vec<Str>,
	/// One-line description.
	pub description: Str,
	/// Inline argument hint.
	pub hint:        Option<Str>,
	/// Human-readable discovery source label.
	pub origin:      Str,
	/// Optional prompt template dispatched when this command is submitted.
	pub template:    Option<Str>,
}

const INIT_WORKFLOW_TEMPLATE: &str = r#"Use parallel `task` research agents for independent slices of the repository: core source, tests, configuration/build, and scripts/documentation. Synthesize their findings into one AGENTS.md.

The document MUST:
- be titled "Repository Guidelines" and use Markdown headings;
- concisely explain project purpose, architecture and data flow, key directories, development commands, code conventions, important files, runtime/tooling preferences, and testing/QA;
- include useful commands, paths, naming patterns, and architecture-specific guidance;
- omit facts that are obvious from the directory tree.

After analysis, write AGENTS.md to the project root."#;

fn embedded_workflow_commands() -> [CommandContribution; 1] {
	[CommandContribution {
		name:        omp_core::sf!("init"),
		aliases:     Vec::new(),
		description: omp_core::sf!("Generate AGENTS.md for the current codebase"),
		hint:        None,
		origin:      omp_core::sf!("Bundled OMP workflow"),
		template:    Some(omp_core::sf!(INIT_WORKFLOW_TEMPLATE)),
	}]
}

/// Immutable active content snapshots shared by prompt, UI, and internal URL
/// composition.
#[derive(Clone, Debug)]
pub struct ActiveContentSnapshots {
	/// Active skills.
	pub skills:       Arc<SkillSnapshot>,
	/// Active declarative rules.
	pub rules:        Arc<RuleSnapshot>,
	/// Active native Markdown slash commands in discovery precedence order.
	pub commands:     Arc<[CommandContribution]>,
	/// Bounded non-fatal diagnostics emitted while loading static content.
	pub warnings:     Arc<[Str]>,
	/// Frozen declarations from the same startup discovery pass.
	pub declarations: Arc<[DiscoveredCapability]>,
}

/// Discovers native repository/user content once and freezes the skill/rule
/// winners used by a session composition.
pub fn active_content_snapshots(root: &Path) -> ActiveContentSnapshots {
	let home = env::var_os("HOME").map_or_else(|| root.to_path_buf(), PathBuf::from);
	let mut discovered =
		native::discover_capabilities(root, &home, 64, &NativeDiscoveryOptions::default());
	let foreign = foreign::discover(root, &ForeignContentSettings::default());
	discovered.declarations.extend(foreign.skills);
	discovered.declarations.extend(foreign.rules);
	discovered.declarations.extend(foreign.prompts);
	discovered.declarations.extend(foreign.instructions);
	discovered.declarations.extend(foreign.commands);
	discovered.warnings.extend(foreign.warnings);
	let managed = managed_skills::discover_dead_last(
		&native::user_config_root(&home),
		&SkillDiscoverySettings::default(),
	);
	discovered.declarations.extend(managed.declarations);
	discovered
		.warnings
		.extend(managed.warnings.into_iter().map(|warning| warning.message));
	let mut commands = discovered
		.declarations
		.iter()
		.filter_map(|declaration| {
			let CapabilityPayload::SlashCommands(command) = &declaration.payload else {
				return None;
			};
			let origin = match declaration.source.scope {
				SourceScope::Project => "Project .omp",
				SourceScope::User => "User .omp",
				_ => "OMP command",
			};
			Some(CommandContribution {
				name:        command.name.clone(),
				aliases:     Vec::new(),
				description: command.description.clone(),
				hint:        command
					.argument_hint
					.clone()
					.or_else(|| Some(Str::new_static("[arguments]"))),
				origin:      Str::new_static(origin),
				template:    Some(command.content.clone()),
			})
		})
		.collect::<Vec<_>>();
	if !commands
		.iter()
		.any(|command| command.name.as_str().eq_ignore_ascii_case("init"))
	{
		commands.extend(embedded_workflow_commands());
	}
	ActiveContentSnapshots {
		skills:       Arc::new(SkillSnapshot::from_declarations(&discovered.declarations)),
		rules:        Arc::new(RuleSnapshot::from_declarations(
			&discovered.declarations,
			&RulebookSettings::default(),
		)),
		commands:     commands.into(),
		warnings:     discovered.warnings.into(),
		declarations: discovered.declarations.into(),
	}
}

/// One data-only winning set ready for its owning runtime.
#[derive(Clone, Debug)]
pub struct WinningCapabilitySet {
	/// Capability family consumed by the domain owner.
	pub kind:    CapabilityKind,
	/// Immutable winning declarations.
	pub winners: Arc<[Arc<CapabilityRecord>]>,
}

/// Immutable, per-chat/session discovery result.
///
/// The snapshot contains only static declarations and diagnostics. Runtime
/// owners may project their winning set, but discovery never imports or
/// activates executable extension code.
#[derive(Clone, Debug)]
pub struct DiscoverySnapshot {
	results:      Arc<BTreeMap<CapabilityKind, CapabilityResult>>,
	winning_sets: Arc<[WinningCapabilitySet]>,
}

impl DiscoverySnapshot {
	/// Returns the complete diagnostics and claims for one capability family.
	pub fn result(&self, kind: CapabilityKind) -> Option<&CapabilityResult> {
		self.results.get(&kind)
	}

	/// Returns one immutable winning set for its domain owner.
	pub fn winning_set(&self, kind: CapabilityKind) -> Option<&[Arc<CapabilityRecord>]> {
		self
			.results
			.get(&kind)
			.map(|result| result.winners.as_ref())
	}

	/// Iterates complete results in canonical capability order.
	pub fn results(&self) -> impl ExactSizeIterator<Item = &CapabilityResult> + DoubleEndedIterator {
		self.results.values()
	}

	/// Returns data-only domain dispatch sets in canonical capability order.
	pub fn dispatch_sets(&self) -> &[WinningCapabilitySet] {
		&self.winning_sets
	}
}

/// Mutable discovery assembly consumed exactly once to freeze a session
/// snapshot. Consuming `self` prevents a chat from rediscovering beneath an
/// already composed prompt or runtime registry.
#[derive(Debug)]
pub struct DiscoveryComposition {
	registry: DiscoveryRegistry,
	context:  LoadContext,
}

impl DiscoveryComposition {
	/// Starts one session composition. The registry's cache is installed into
	/// the load context so no provider can accidentally use a process-global or
	/// sibling-session cache.
	pub fn new(registry: DiscoveryRegistry, mut context: LoadContext) -> Self {
		context.cache = Arc::clone(registry.cache());
		Self { registry, context }
	}

	/// Concurrently loads all canonical families and freezes their winners,
	/// suppressed claims, warnings, timings, and failures.
	pub async fn freeze(self, options: LoadOptions<'_>) -> DiscoverySnapshot {
		let loaded = join_all(
			CAPABILITY_KINDS
				.iter()
				.copied()
				.map(|kind| self.registry.load(kind, &self.context, options)),
		)
		.await;
		let results = loaded
			.into_iter()
			.map(|result| (result.kind, result))
			.collect::<BTreeMap<_, _>>();
		let winning_sets = results
			.iter()
			.map(|(kind, result)| WinningCapabilitySet {
				kind:    *kind,
				winners: Arc::clone(&result.winners),
			})
			.collect::<Vec<_>>()
			.into();
		DiscoverySnapshot { results: Arc::new(results), winning_sets }
	}
}

/// Normalizes provider-returned model rows conservatively before applying them
/// as runtime catalog overlays.
///
/// Missing evidence remains unknown; this module never infers capabilities from
/// provider or model names.
pub fn normalize(
	rows: &[DiscoveredModel],
	wire_policy: WirePolicyId,
	extended_wire_policy: Option<WirePolicyId>,
	thinking: Option<ThinkingPolicyId>,
) -> Result<Vec<NormalizedDiscovery>, Box<omp_catalog::discover::DiscoveryError>> {
	DiscoveryNormalizer::new(DiscoveryDefaults {
		wire_policy,
		extended_wire_policy,
		context: ContextStrategy::Replay,
		thinking,
		pricing: Pricing::default(),
	})
	.normalize_batch(rows)
	.map_err(Box::new)
}

/// Returns the route restriction carried by an authenticated discovery request.
pub const fn route_scope(route: RouteId) -> RouteId {
	route
}
