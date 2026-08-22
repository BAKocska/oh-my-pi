//! Environment-backed native Git materialization for pinned extension trees.
//!
//! The host owns the Git runner, so the fetch/checkout driver lives beside it
//! rather than in the extension domain crate.

use std::{
	fs, io,
	path::{Path, PathBuf},
	sync::atomic::{AtomicU64, Ordering},
};

use omp_core::{Hash32, Str};
use omp_ext::{ExtensionCode, ExtensionError};
use tokio_util::sync::CancellationToken;

use super::vcs::git::{
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
	pub fn new(runner: GitRunner, cache_root: PathBuf) -> Self {
		Self { commands: GitCommands::new(runner.clone()), runner, cache_root }
	}

	/// Fetches exactly the pinned revision and atomically materializes a clean
	/// source tree. Returns the validated contained subdirectory when declared.
	pub async fn materialize(
		&self,
		source: &omp_ext::config::SourceSpec,
		destination: &Path,
		cancel: &CancellationToken,
	) -> Result<PathBuf, ExtensionError> {
		let omp_ext::config::SourceSpec::Git { repository, revision, subdirectory } = source else {
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

fn git_run(error: super::vcs::git::runner::GitRunError) -> ExtensionError {
	ExtensionError::new(ExtensionCode::EIntegrity, format!("Environment Git failed: {error}"))
}

fn git_command(error: super::vcs::git::commands::CommandError) -> ExtensionError {
	ExtensionError::new(ExtensionCode::EIntegrity, format!("Environment Git failed: {error}"))
}

fn git_exit(code: i32) -> ExtensionError {
	ExtensionError::new(
		ExtensionCode::EIntegrity,
		format!("Environment Git exited with status {code}"),
	)
}
