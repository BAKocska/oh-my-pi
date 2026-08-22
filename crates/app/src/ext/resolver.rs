//! `uv` resolver driver and deterministic resolution-policy checks.

use std::{
	ffi::OsString,
	fs, io,
	path::{Path, PathBuf},
	process::{Command, Output},
	sync::atomic::{AtomicU64, Ordering},
};

use omp_core::{Hash32, Str};
use tokio_util::sync::CancellationToken;

use super::{ExtensionCode, ExtensionError};
use crate::envd::vcs::git::{
	commands::GitCommands,
	repo::Repository,
	runner::{GitDeadline, GitRunOptions, GitRunner},
};

static GIT_MATERIALIZATION_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Environment-backed native Git source fetcher for pinned extension trees.
#[derive(Clone)]
pub struct NativeGitResolver {
	runner:     GitRunner,
	commands:   GitCommands,
	cache_root: PathBuf,
}

impl NativeGitResolver {
	/// Creates a resolver over the Environment Git runner and an app-owned
	/// content cache.
	#[must_use]
	pub fn new(runner: GitRunner, cache_root: PathBuf) -> Self {
		Self { commands: GitCommands::new(runner.clone()), runner, cache_root }
	}

	/// Fetches exactly the pinned revision and atomically materializes a clean
	/// source tree. Returns the validated contained subdirectory when declared.
	pub async fn materialize(
		&self,
		source: &super::config::SourceSpec,
		destination: &Path,
		cancel: &CancellationToken,
	) -> Result<PathBuf, ExtensionError> {
		let super::config::SourceSpec::Git { repository, revision, subdirectory } = source else {
			return Err(ext_git_error("native Git resolver requires a git: source"));
		};
		if destination.exists() {
			return Err(ext_git_error("Git materialization destination already exists"));
		}
		fs::create_dir_all(&self.cache_root).map_err(git_io)?;
		let cache_name = Hash32::sum(repository.as_bytes()).to_hex();
		let cache = self.cache_root.join(cache_name.as_str());
		if !cache.is_dir() {
			let sequence = GIT_MATERIALIZATION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
			let stage = self
				.cache_root
				.join(format!(".git-cache-{sequence:016x}.tmp"));
			let stage_arg = utf8_path(&stage)?;
			let output = self
				.runner
				.run(
					&self.cache_root,
					&["init", "--bare", stage_arg],
					GitRunOptions {
						read_only:       false,
						parse_sensitive: true,
						deadline:        GitDeadline::Local,
					},
					cancel,
				)
				.await
				.map_err(git_run)?;
			if output.exit_code != 0 {
				let _ = fs::remove_dir_all(&stage);
				return Err(git_exit(output.exit_code));
			}
			match fs::rename(&stage, &cache) {
				Ok(()) => {},
				Err(_) if cache.is_dir() => {
					let _ = fs::remove_dir_all(&stage);
				},
				Err(error) => {
					let _ = fs::remove_dir_all(&stage);
					return Err(git_io(error));
				},
			}
		}
		let bare = Repository {
			worktree_root: cache.clone(),
			git_dir:       cache.clone(),
			common_dir:    cache.clone(),
			primary_root:  cache.clone(),
			bare:          true,
		};
		self
			.commands
			.add_remote(&bare, "origin", repository, cancel)
			.await
			.map_err(git_command)?;
		let target = "refs/omp/extensions/source";
		self
			.commands
			.fetch_refspec(&bare, "origin", revision, target, cancel)
			.await
			.map_err(git_command)?;
		let commit_ref = format!("{target}^{{commit}}");
		let resolved = self
			.commands
			.resolve_ref(&cache, &commit_ref, cancel)
			.await
			.map_err(git_command)?
			.ok_or_else(|| ext_git_error("fetched Git revision is absent"))?;
		if matches!(revision.len(), 40 | 64) && !resolved.eq_ignore_ascii_case(revision) {
			return Err(ext_git_error("fetched Git revision differs from the pinned commit"));
		}

		let sequence = GIT_MATERIALIZATION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
		if let Some(parent) = destination.parent() {
			fs::create_dir_all(parent).map_err(git_io)?;
		}
		let stage = destination.with_file_name(format!(".git-source-{sequence:016x}.tmp"));
		let cache_arg = utf8_path(&cache)?;
		let stage_arg = utf8_path(&stage)?;
		let output = self
			.runner
			.run(
				&self.cache_root,
				&["clone", "--no-checkout", cache_arg, stage_arg],
				GitRunOptions {
					read_only:       false,
					parse_sensitive: true,
					deadline:        GitDeadline::Local,
				},
				cancel,
			)
			.await
			.map_err(git_run)?;
		if output.exit_code != 0 {
			let _ = fs::remove_dir_all(&stage);
			return Err(git_exit(output.exit_code));
		}
		let output = self
			.runner
			.run(
				&stage,
				&["checkout", "--detach", resolved.as_str()],
				GitRunOptions {
					read_only:       false,
					parse_sensitive: true,
					deadline:        GitDeadline::Local,
				},
				cancel,
			)
			.await
			.map_err(git_run)?;
		if output.exit_code != 0 {
			let _ = fs::remove_dir_all(&stage);
			return Err(git_exit(output.exit_code));
		}
		fs::remove_dir_all(stage.join(".git")).map_err(git_io)?;
		fs::rename(&stage, destination).map_err(git_io)?;
		let root = fs::canonicalize(destination).map_err(git_io)?;
		let selected = subdirectory
			.as_ref()
			.map_or_else(|| root.clone(), |path| root.join(path));
		let selected = fs::canonicalize(selected).map_err(git_io)?;
		if !selected.starts_with(&root) {
			return Err(ext_git_error("Git source subdirectory escapes the materialized tree"));
		}
		Ok(selected)
	}
}

fn utf8_path(path: &Path) -> Result<&str, ExtensionError> {
	path
		.to_str()
		.ok_or_else(|| ext_git_error("Git materialization path is not UTF-8"))
}

fn ext_git_error(detail: &str) -> ExtensionError {
	ExtensionError::new(ExtensionCode::EIntegrity, detail)
}

fn git_io(error: io::Error) -> ExtensionError {
	ExtensionError::new(ExtensionCode::EIntegrity, format!("Git materialization I/O: {error}"))
}

fn git_run(error: crate::envd::vcs::git::runner::GitRunError) -> ExtensionError {
	ExtensionError::new(ExtensionCode::EIntegrity, format!("Environment Git failed: {error}"))
}

fn git_command(error: crate::envd::vcs::git::commands::CommandError) -> ExtensionError {
	ExtensionError::new(ExtensionCode::EIntegrity, format!("Environment Git failed: {error}"))
}

fn git_exit(code: i32) -> ExtensionError {
	ExtensionError::new(
		ExtensionCode::EIntegrity,
		format!("Environment Git exited with status {code}"),
	)
}

/// The `CPython` ABI tags allowed by R3.
pub const ACCEPTED_ABIS: [&str; 3] = ["cp314t", "abi3t", "none"];

/// One enabled extension requirement participating in a host-child unit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolveRequirement {
	/// Owning extension id, used in unsat explanations.
	pub extension_id: Str,
	/// Hash-pinned requirement text supplied to uv.
	pub requirement:  Str,
}

/// Pure data used to construct one reproducible `uv` invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UvRequest {
	/// `uv` executable, ordinarily from `OMP_EXT_UV` or PATH.
	pub executable:        PathBuf,
	/// Target triple for R4/R12.
	pub target:            Str,
	/// Ordered first-index sources.
	pub indexes:           Vec<String>,
	/// Optional R9 timestamp clamp.
	pub exclude_newer:     Option<Str>,
	/// Requirement file whose entries contain SHA-256 hashes.
	pub requirements_file: PathBuf,
	/// Additional root requirements (one host child or explicit pool).
	pub requirements:      Vec<ResolveRequirement>,
}

impl UvRequest {
	/// Constructs the exact argv passed to `uv`. This stays pure so callers can
	/// show `resolve --explain` without touching the network.
	#[must_use]
	pub fn argv(&self) -> Vec<OsString> {
		let mut argv = vec![
			OsString::from("pip"),
			OsString::from("install"),
			OsString::from("--dry-run"),
			OsString::from("--only-binary"),
			OsString::from(":all:"),
			OsString::from("--require-hashes"),
			OsString::from("--python-platform"),
			OsString::from(self.target.as_str()),
			OsString::from("--python-version"),
			OsString::from("3.14"),
			OsString::from("--index-strategy"),
			OsString::from("first-index"),
		];
		for index in &self.indexes {
			argv.push(OsString::from("--index-url"));
			argv.push(OsString::from(index));
		}
		if let Some(exclude_newer) = &self.exclude_newer {
			argv.push(OsString::from("--exclude-newer"));
			argv.push(OsString::from(exclude_newer.as_str()));
		}
		argv.push(OsString::from("--requirement"));
		argv.push(self.requirements_file.clone().into_os_string());
		argv.extend(
			self
				.requirements
				.iter()
				.map(|requirement| OsString::from(requirement.requirement.as_str())),
		);
		argv
	}

	/// R7 checks requirements against the actual frozen runtime metadata before
	/// invoking uv, preventing a silently shadowed site copy.
	pub fn reject_frozen_conflicts(&self, frozen: &[(&str, &str)]) -> Result<(), ExtensionError> {
		for requirement in &self.requirements {
			let Some((name, version)) = requirement.requirement.as_str().split_once("==") else {
				continue;
			};
			if let Some((_, frozen_version)) = frozen
				.iter()
				.find(|(frozen_name, _)| name.eq_ignore_ascii_case(frozen_name))
				&& version != *frozen_version
			{
				return Err(ExtensionError::new(
					ExtensionCode::EFrozenConflict,
					format!("{name}=={version} conflicts with frozen {name}=={frozen_version}"),
				));
			}
		}
		Ok(())
	}
}

/// A declared enabled extension root. It is an alias that makes the
/// host-child resolution boundary explicit at CLI call sites.
pub type EnabledExtension = ResolveRequirement;

/// A per-target resolution plan for one enabled host-child unit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvePlan {
	/// Exactly one `uv` request per materializing target.
	pub requests: Vec<UvRequest>,
}

impl ResolvePlan {
	/// Builds per-target invocations without spawning `uv`.
	///
	/// `requirements_file` is the hash-pinned closure emitted by the install
	/// backend from the manifest or durable lock. The CLI never reconstructs
	/// dependency resolution itself.
	pub fn build(
		executable: PathBuf,
		enabled: &[EnabledExtension],
		targets: &[Str],
		indexes: Vec<String>,
		exclude_newer: Option<Str>,
		requirements_file: PathBuf,
	) -> Result<Self, ExtensionError> {
		if targets.is_empty() {
			return Err(ExtensionError::new(
				ExtensionCode::ETargetMissing,
				"at least one target is required",
			));
		}
		let mut ids = std::collections::BTreeSet::new();
		for extension in enabled {
			if !ids.insert(&extension.extension_id) {
				return Err(ExtensionError::new(
					ExtensionCode::EDupId,
					format!("duplicate enabled extension {}", extension.extension_id),
				));
			}
		}
		Ok(Self {
			requests: targets
				.iter()
				.map(|target| UvRequest {
					executable:        executable.clone(),
					target:            target.clone(),
					indexes:           indexes.clone(),
					exclude_newer:     exclude_newer.clone(),
					requirements_file: requirements_file.clone(),
					requirements:      enabled.to_vec(),
				})
				.collect(),
		})
	}

	/// Returns the exact `uv` argv for every target, for `resolve --explain`.
	#[must_use]
	pub fn explain(&self) -> Vec<Vec<OsString>> {
		self.requests.iter().map(UvRequest::argv).collect()
	}

	/// Executes every planned target and preserves each exact invocation.
	pub fn run<R: UvRunner>(
		&self,
		runner: &R,
		frozen: &[(&str, &str)],
	) -> Result<Vec<ResolveOutcome>, ExtensionError> {
		self
			.requests
			.iter()
			.map(|request| resolve_with(runner, request, frozen))
			.collect()
	}
}

/// Process boundary for `uv`; production uses [`SystemUv`] while tests inject
/// a deterministic resolver without a network or executable.
pub trait UvRunner {
	/// Executes an argv prepared by [`UvRequest::argv`].
	fn run(&self, executable: &PathBuf, argv: &[OsString]) -> io::Result<Output>;
}

/// The production `uv` process runner.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemUv;

impl UvRunner for SystemUv {
	fn run(&self, executable: &PathBuf, argv: &[OsString]) -> io::Result<Output> {
		Command::new(executable).args(argv).output()
	}
}

/// An explainable result of resolving one host-child unit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolveOutcome {
	/// Exact equivalent command line.
	pub argv:   Vec<OsString>,
	/// Captured uv standard output.
	pub stdout: Vec<u8>,
	/// Captured uv standard error.
	pub stderr: Vec<u8>,
}

/// Resolves one host-child unit after all pure R1–R12 inputs have been checked.
pub fn resolve_with<R: UvRunner>(
	runner: &R,
	request: &UvRequest,
	frozen: &[(&str, &str)],
) -> Result<ResolveOutcome, ExtensionError> {
	request.reject_frozen_conflicts(frozen)?;
	let argv = request.argv();
	let output = runner
		.run(&request.executable, &argv)
		.map_err(|error| ExtensionError::new(ExtensionCode::EUnsat, error.to_string()))?;
	if !output.status.success() {
		return Err(ExtensionError::new(
			ExtensionCode::EUnsat,
			String::from_utf8_lossy(&output.stderr),
		));
	}
	Ok(ResolveOutcome { argv, stdout: output.stdout, stderr: output.stderr })
}

/// Returns the minimal enabled-extension subset still unsatisfiable. The first
/// phase bisects to remove independent halves; bounded deletion then makes the
/// result subset-minimal even when the conflict spans a bisection boundary.
pub fn minimal_unsat_core<T: Clone>(
	requirements: &[T],
	max_probes: usize,
	mut unsatisfiable: impl FnMut(&[T]) -> bool,
) -> Vec<T> {
	if requirements.is_empty() || !unsatisfiable(requirements) {
		return Vec::new();
	}
	let mut probes = 1;
	let mut core = requirements.to_vec();
	let mut width = core.len() / 2;
	while width > 0 && probes < max_probes {
		let mut reduced = false;
		let mut start = 0;
		while start < core.len() && probes < max_probes {
			let end = (start + width).min(core.len());
			let mut candidate = core.clone();
			candidate.drain(start..end);
			probes += 1;
			if !candidate.is_empty() && unsatisfiable(&candidate) {
				core = candidate;
				reduced = true;
				break;
			}
			start = end;
		}
		if !reduced {
			width /= 2;
		}
	}
	let mut index = 0;
	while index < core.len() && probes < max_probes {
		let mut candidate = core.clone();
		candidate.remove(index);
		probes += 1;
		if !candidate.is_empty() && unsatisfiable(&candidate) {
			core = candidate;
		} else {
			index += 1;
		}
	}
	core
}

/// Validates an observed wheel ABI against R3.
pub fn validate_abi(tag: &str) -> Result<(), ExtensionError> {
	let abi = tag.split('-').nth(1).unwrap_or_default();
	if ACCEPTED_ABIS.contains(&abi) {
		Ok(())
	} else {
		Err(ExtensionError::new(
			ExtensionCode::EAbiRejected,
			format!("wheel ABI {abi:?}; accepted: cp314t, abi3t, none"),
		))
	}
}

/// R4 requires every materializing target to have a target-specific wheel.
pub fn validate_target(target: &Str, available_targets: &[Str]) -> Result<(), ExtensionError> {
	if available_targets.contains(target) {
		Ok(())
	} else {
		Err(ExtensionError::new(
			ExtensionCode::ETargetMissing,
			format!("no wheel for target {target}"),
		))
	}
}

#[cfg(test)]
mod tests {
	use omp_core::sf;

	use super::*;

	#[test]
	fn uv_argv_enforces_nonnegotiable_flags() {
		let request = UvRequest {
			executable:        PathBuf::from("uv"),
			target:            sf!("aarch64-apple-darwin"),
			indexes:           vec!["https://ext.omp.dev/simple".to_owned()],
			exclude_newer:     Some(sf!("2026-08-20T00:00:00Z")),
			requirements_file: PathBuf::from("requirements.txt"),
			requirements:      vec![],
		};
		let argv = request
			.argv()
			.into_iter()
			.map(|argument| argument.into_string().expect("utf8 argv"))
			.collect::<Vec<_>>();
		assert_eq!(argv, [
			"pip",
			"install",
			"--dry-run",
			"--only-binary",
			":all:",
			"--require-hashes",
			"--python-platform",
			"aarch64-apple-darwin",
			"--python-version",
			"3.14",
			"--index-strategy",
			"first-index",
			"--index-url",
			"https://ext.omp.dev/simple",
			"--exclude-newer",
			"2026-08-20T00:00:00Z",
			"--requirement",
			"requirements.txt"
		]);
	}
}
