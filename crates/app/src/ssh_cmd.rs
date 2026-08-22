//! Authoritative scoped native SSH configuration and standalone client.

use std::path::PathBuf;

use clap::{Args, Subcommand, ValueEnum};
use miette::IntoDiagnostic as _;
use omp_core::Str;

use crate::envd::ssh::{AuthPolicy, HostConfig, HostStore, SshService};

/// Native SSH command options.
#[derive(Clone, Debug, Args)]
pub struct SshArgs {
	/// Configuration and client operation.
	#[command(subcommand)]
	pub command: SshCommand,
}

/// Writable native SSH configuration scope.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum SshScope {
	/// Repository-local `.omp/hosts.toml`.
	#[default]
	Project,
	/// Profile-local `hosts.toml`.
	User,
}

/// Native SSH configuration and bounded client operations.
#[derive(Clone, Debug, Subcommand)]
pub enum SshCommand {
	/// List configured host aliases.
	List {
		/// Restrict inventory to one scope.
		#[arg(long, value_enum)]
		scope: Option<SshScope>,
	},
	/// Add or replace one configured host.
	Add {
		/// Stable configured alias.
		alias:    Str,
		/// DNS name or numeric address.
		#[arg(long)]
		host:     Str,
		/// Remote account name.
		#[arg(long)]
		user:     Str,
		/// SSH port.
		#[arg(long, default_value_t = 22)]
		port:     u16,
		/// Pinned SHA-256 server host-key fingerprint.
		#[arg(long = "host-key")]
		host_key: Str,
		/// Unencrypted private-key path; omission uses the native SSH agent.
		#[arg(long)]
		key:      Option<PathBuf>,
		/// Writable configuration scope.
		#[arg(long, value_enum, default_value_t = SshScope::Project)]
		scope:    SshScope,
	},
	/// Remove one configured host.
	Remove {
		/// Stable configured alias.
		alias: Str,
		/// Writable configuration scope.
		#[arg(long, value_enum, default_value_t = SshScope::Project)]
		scope: SshScope,
	},
	/// Probe pinned-host-key authentication and SFTP support.
	Probe { alias: Str },
	/// Execute one bounded remote command.
	Exec {
		alias:   Str,
		#[arg(trailing_var_arg = true, required = true)]
		command: Vec<Str>,
	},
}

/// Runs scoped writer and bounded native transport operations.
pub async fn run(args: SshArgs) -> miette::Result<()> {
	let user = crate::cli::data_dir(None)?.join("hosts.toml");
	let project = std::env::current_dir()
		.into_diagnostic()?
		.join(".omp/hosts.toml");
	match args.command {
		SshCommand::List { scope } => {
			for (label, path) in scoped_paths(scope, &project, &user) {
				let store = HostStore::load(path).into_diagnostic()?;
				for alias in store.aliases() {
					let host = store.get(alias.as_str()).into_diagnostic()?;
					println!("{label}\t{}\t{}@{}:{}", alias, host.user, host.address, host.port);
				}
			}
			Ok(())
		},
		SshCommand::Add { alias, host, user: remote_user, port, host_key, key, scope } => {
			let path = scope_path(scope, &project, &user);
			let store = HostStore::load(path).into_diagnostic()?;
			store
				.upsert(path, alias.clone(), HostConfig {
					address: host,
					port,
					user: remote_user,
					host_key,
					auth: key.map_or(AuthPolicy::Agent, |path| AuthPolicy::Key { path }),
					timeout_secs: 30,
				})
				.into_diagnostic()?;
			println!("configured SSH host `{alias}` in {}", path.display());
			Ok(())
		},
		SshCommand::Remove { alias, scope } => {
			let path = scope_path(scope, &project, &user);
			let removed = HostStore::load(path)
				.into_diagnostic()?
				.remove(path, alias.as_str())
				.into_diagnostic()?;
			if !removed {
				return Err(miette::miette!(
					"SSH host `{alias}` is not configured in {}",
					path.display()
				));
			}
			println!("removed SSH host `{alias}` from {}", path.display());
			Ok(())
		},
		SshCommand::Probe { alias } => {
			let service = service(alias.as_str(), &project, &user)?;
			let caps = service.probe(alias.as_str()).await.into_diagnostic()?;
			println!("{}: exec={} sftp={}", alias, caps.exec, caps.sftp);
			Ok(())
		},
		SshCommand::Exec { alias, command } => {
			let service = service(alias.as_str(), &project, &user)?;
			let command = command
				.iter()
				.map(Str::as_str)
				.collect::<Vec<_>>()
				.join(" ");
			let output = service
				.exec(alias.as_str(), &command, 1024 * 1024)
				.await
				.into_diagnostic()?;
			print!("{}", String::from_utf8_lossy(output.stdout.as_ref()));
			eprint!("{}", String::from_utf8_lossy(output.stderr.as_ref()));
			if output.exit_status.unwrap_or_default() != 0 {
				return Err(miette::miette!(
					"remote command exited with status {}",
					output.exit_status.unwrap_or_default()
				));
			}
			Ok(())
		},
	}
}

fn service(
	alias: &str,
	project: &std::path::Path,
	user: &std::path::Path,
) -> miette::Result<SshService> {
	let project_store = HostStore::load(project).into_diagnostic()?;
	if project_store.get(alias).is_ok() {
		Ok(SshService::new(project_store))
	} else {
		let user_store = HostStore::load(user).into_diagnostic()?;
		user_store.get(alias).into_diagnostic()?;
		Ok(SshService::new(user_store))
	}
}

fn scope_path<'a>(
	scope: SshScope,
	project: &'a std::path::Path,
	user: &'a std::path::Path,
) -> &'a std::path::Path {
	match scope {
		SshScope::Project => project,
		SshScope::User => user,
	}
}

fn scoped_paths<'a>(
	scope: Option<SshScope>,
	project: &'a std::path::Path,
	user: &'a std::path::Path,
) -> Vec<(&'static str, &'a std::path::Path)> {
	match scope {
		Some(SshScope::Project) => vec![("project", project)],
		Some(SshScope::User) => vec![("user", user)],
		None => vec![("project", project), ("user", user)],
	}
}
