//! `uv` resolver driver and deterministic resolution-policy checks.

use std::{
	ffi::OsString,
	io,
	path::PathBuf,
	process::{Command, Output},
};

use omp_core::{Str, sf};

use super::{ExtensionCode, ExtensionError};

/// The CPython ABI tags allowed by R3.
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
