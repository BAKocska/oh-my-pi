use std::{
	collections::BTreeMap,
	future::{self, Future},
	path::Path,
	time::Duration,
};

use omp_core::{CowBytes, Str, encoding::hex, sf};
use omp_proto::env::v1::{
	EnvironmentDelta, ExecOutcome as EnvExecOutcome, ExecRequest, OpenSessionRequest,
	OutputChannel as EnvOutputChannel, ProcessSpec, PtySpec, RestartPolicy, RestartSpec, Script,
	ShellProfileInput, StartProcess,
};
use omp_tool::{BlobRef, JobOwner};
use omp_tools::{
	auto_background::DetachedJob,
	shell::{
		DetachRequest, ExecOutcome, ExecStatus, Fault, OutputChannel, RunEvent, RunRequest, Session,
		SessionOptions, ShellExec, ShellRun, Update,
	},
};

use super::{
	exec::{ExecError, ExecEvent, ExecHost, ExecRun},
	exec_settings::{DirenvMode, ShellProfile, ShellSettings},
};

/// Shell resource adapter backed by the app-owned execution host.
#[derive(Clone)]
pub struct ShellExecHost {
	host:     ExecHost,
	cwd_uri:  Str,
	settings: ShellSettings,
}

impl ShellExecHost {
	/// Binds shell execution to the workspace root URI used for sessions and
	/// detached processes.
	pub(crate) const fn new(host: ExecHost, cwd_uri: Str, settings: ShellSettings) -> Self {
		Self { host, cwd_uri, settings }
	}
}
impl ShellExecHost {
	async fn shell_profile(&self) -> ShellProfileInput {
		let mut profile = self.settings.profile;
		let mut executable = self
			.settings
			.executable
			.as_deref()
			.unwrap_or_default()
			.to_owned();
		if profile == ShellProfile::User && executable.is_empty() {
			executable = std::env::var("SHELL")
				.ok()
				.filter(|shell| {
					let path = Path::new(shell);
					path.is_absolute()
						&& path.is_file()
						&& path
							.file_name()
							.and_then(|name| name.to_str())
							.is_some_and(|name| matches!(name, "bash" | "zsh" | "fish"))
				})
				.unwrap_or_default();
			if executable.is_empty() {
				profile = ShellProfile::Brush;
			}
		}
		if executable.is_empty() {
			executable = match profile {
				ShellProfile::Bash => String::from("bash"),
				ShellProfile::Zsh => String::from("zsh"),
				ShellProfile::Fish => String::from("fish"),
				ShellProfile::Brush | ShellProfile::User => String::new(),
			};
		}
		let profile_name: &'static str = profile.into();
		let args = self
			.settings
			.args
			.iter()
			.filter(|argument| {
				profile != ShellProfile::Fish || !matches!(argument.as_str(), "-l" | "--login")
			})
			.map(ToString::to_string)
			.collect();
		let snapshot_prefix =
			if matches!(profile, ShellProfile::Bash | ShellProfile::Zsh | ShellProfile::User) {
				let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
				match home {
					Some(home) => super::shell_profile::capture(&executable, &home)
						.await
						.ok()
						.flatten()
						.map(|path| format!(". {} &&", shell_word(&path.to_string_lossy()))),
					None => None,
				}
			} else {
				None
			};
		let command_prefix = match (snapshot_prefix, self.settings.command_prefix.as_deref()) {
			(Some(snapshot), Some(prefix)) => format!("{snapshot} {prefix}"),
			(Some(snapshot), None) => snapshot,
			(None, Some(prefix)) => prefix.to_owned(),
			(None, None) => String::new(),
		};
		ShellProfileInput {
			profile: profile_name.to_owned(),
			executable,
			args,
			command_prefix,
			env_delta: None,
			login: self.settings.login && profile != ShellProfile::Fish,
			wire_revision: omp_proto::SCHEMA_REV,
		}
	}

	async fn detached_command(&self, command: &Str) -> String {
		let profile = self.shell_profile().await;
		let command = if profile.command_prefix.is_empty() {
			command.to_string()
		} else {
			format!("{} {command}", profile.command_prefix)
		};
		if matches!(profile.profile.as_str(), "" | "brush") {
			return command;
		}
		let mut rendered = shell_word(&profile.executable);
		for argument in profile.args {
			rendered.push(' ');
			rendered.push_str(&shell_word(&argument));
		}
		if profile.login {
			rendered.push_str(" -l");
		}
		rendered.push_str(" -c ");
		rendered.push_str(&shell_word(&command));
		rendered
	}

	fn resolve_cwd(&self, requested: Option<&str>) -> Result<Str, Fault> {
		let root = url::Url::parse(&self.cwd_uri)
			.map_err(|error| cwd_fault(format!("workspace root URI is invalid: {error}")))?;
		let root_path = root
			.to_file_path()
			.map_err(|()| cwd_fault("workspace root is not a local file URI"))?;
		let path = match requested {
			None => root_path,
			Some(value) if value.contains("://") => url::Url::parse(value)
				.map_err(|error| cwd_fault(format!("working-directory URI is invalid: {error}")))?
				.to_file_path()
				.map_err(|()| cwd_fault("working-directory URI is not a local file URI"))?,
			Some(value) => {
				let path = Path::new(value);
				if path.is_absolute() {
					path.into()
				} else {
					root_path.join(path)
				}
			},
		};
		if !path.is_dir() {
			return Err(cwd_fault(format!(
				"working directory is not an existing directory: {}",
				path.display()
			)));
		}
		let uri = url::Url::from_file_path(path)
			.map_err(|()| cwd_fault("working directory cannot be represented as a file URI"))?;
		Ok(Str::from(uri.to_string()))
	}

	async fn environment(
		&self,
		cwd_uri: &str,
		user: BTreeMap<Str, Str>,
		pty: bool,
	) -> EnvironmentDelta {
		let direnv = if self.settings.direnv == DirenvMode::Auto {
			url::Url::parse(cwd_uri)
				.ok()
				.and_then(|url| url.to_file_path().ok())
				.map(|cwd| async move {
					super::direnv::load(
						&cwd,
						Duration::from_millis(self.settings.direnv_load_timeout_ms),
					)
					.await
				})
		} else {
			None
		};
		let direnv = match direnv {
			Some(load) => load.await,
			None => None,
		};
		hardened_environment(user, pty, direnv)
	}
}

fn hardened_environment(
	user: std::collections::BTreeMap<Str, Str>,
	pty: bool,
	direnv: Option<super::direnv::DirenvDelta>,
) -> EnvironmentDelta {
	let mut set: BTreeMap<String, String> = [
		("PAGER", "cat"),
		("GIT_PAGER", "cat"),
		("MANPAGER", "cat"),
		("SYSTEMD_PAGER", "cat"),
		("BAT_PAGER", "cat"),
		("DELTA_PAGER", "cat"),
		("GH_PAGER", "cat"),
		("GLAB_PAGER", "cat"),
		("AWS_PAGER", ""),
		("PSQL_PAGER", "cat"),
		("MYSQL_PAGER", "cat"),
		("HOMEBREW_PAGER", "cat"),
		("LESS", "FRX"),
		("NO_COLOR", "1"),
		("PYTHONUNBUFFERED", "1"),
		("GIT_EDITOR", "true"),
		("VISUAL", "true"),
		("EDITOR", "true"),
		("GIT_TERMINAL_PROMPT", "0"),
		("SSH_ASKPASS", "false"),
		("CI", "true"),
		("AGENT", "1"),
		("npm_config_yes", "true"),
		("npm_config_update_notifier", "false"),
		("npm_config_fund", "false"),
		("npm_config_audit", "false"),
		("PNPM_DISABLE_SELF_UPDATE_CHECK", "true"),
		("YARN_ENABLE_TELEMETRY", "0"),
		("PNPM_UPDATE_NOTIFIER", "false"),
		("YARN_ENABLE_PROGRESS_BARS", "0"),
		("CARGO_TERM_PROGRESS_WHEN", "never"),
		("PIP_NO_INPUT", "1"),
		("PIP_DISABLE_PIP_VERSION_CHECK", "1"),
		("GH_PROMPT_DISABLED", "1"),
		("DEBIAN_FRONTEND", "noninteractive"),
		("TF_INPUT", "0"),
		("TF_IN_AUTOMATION", "1"),
		("COMPOSER_NO_INTERACTION", "1"),
		("CLOUDSDK_CORE_DISABLE_PROMPTS", "1"),
	]
	.into_iter()
	.map(|(key, value)| (String::from(key), String::from(value)))
	.collect();
	if let Some(direnv) = &direnv {
		set.extend(
			direnv
				.set
				.iter()
				.map(|(key, value)| (key.to_string(), value.to_string())),
		);
	}
	if !pty {
		set.insert(String::from("TERM"), String::from("dumb"));
	}
	if std::env::var_os("OMP_BASH_NO_CI").is_some_and(|value| {
		let value = value.to_string_lossy();
		!value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
	}) {
		set.remove("CI");
	}
	let explicit = user
		.keys()
		.cloned()
		.collect::<std::collections::BTreeSet<_>>();
	set.extend(
		user
			.into_iter()
			.map(|(key, value)| (key.to_string(), value.to_string())),
	);
	let unset = direnv
		.into_iter()
		.flat_map(|delta| delta.unset)
		.filter(|key| !explicit.contains(key) && !set.contains_key(key.as_str()))
		.map(|key| key.to_string())
		.collect();
	EnvironmentDelta { set, unset, props: None }
}

fn shell_word(word: &str) -> String {
	format!("'{}'", word.replace('\'', "'\\''"))
}

fn named_process(started: omp_proto::env::v1::ProcessStarted) -> DetachedJob {
	let id = sf!("{}#{}", started.name, started.generation);
	DetachedJob {
		id,
		owner: JobOwner::NamedProcess {
			name:       Str::from(started.name),
			generation: started.generation,
		},
	}
}

fn cwd_fault(message: impl Into<Str>) -> Fault {
	Fault::Resource { operation: sf!("cwd"), message: message.into() }
}
/// Foreground shell run retaining the concrete host's process-tree guard.
pub struct HostShellRun {
	host: ExecHost,
	run:  ExecRun,
}

impl ShellRun for HostShellRun {
	async fn next_event(&mut self) -> Result<Option<RunEvent>, Fault> {
		let Some(event) = self.run.next_event().await else {
			return Ok(None);
		};
		map_event(event).map(Some)
	}

	fn cancel(&self) -> impl Future<Output = Result<(), Fault>> + Send + '_ {
		self.run.cancel();
		future::ready(Ok(()))
	}

	fn detach(&self, name: Str) -> impl Future<Output = Result<DetachedJob, Fault>> + Send + '_ {
		future::ready(
			self
				.host
				.detach_exec(self.run.id(), &name)
				.map(named_process)
				.map_err(|error| resource_fault("detach_running", error)),
		)
	}
}

impl ShellExec for ShellExecHost {
	type Run = HostShellRun;

	async fn open_session(&self, options: SessionOptions) -> Result<Session, Fault> {
		let cwd_uri = self.resolve_cwd(options.cwd.as_deref())?;
		let pty = options.pty;
		let environment = self.environment(&cwd_uri, options.env, pty).await;
		let opened = self
			.host
			.open_session(OpenSessionRequest {
				cwd_uri: cwd_uri.to_string(),
				env_delta: Some(environment),
				pty: pty
					.then(|| PtySpec { terminal: String::from("xterm-256color"), ..Default::default() }),
				shell_profile: Some(self.shell_profile().await),
				..Default::default()
			})
			.await
			.map_err(|error| resource_fault("open_session", error))?;
		Ok(Session { id: opened.session })
	}

	fn close_session(
		&self,
		session: &Session,
	) -> impl Future<Output = Result<(), Fault>> + Send + '_ {
		future::ready(
			self
				.host
				.close_session(&session.id)
				.map(|_| ())
				.map_err(|error| resource_fault("close_session", error)),
		)
	}

	async fn run<'a>(
		&'a self,
		session: &'a Session,
		request: RunRequest,
	) -> Result<Self::Run, Fault> {
		let (_, run) = self
			.host
			.exec(
				ExecRequest {
					session: session.id.clone(),
					source: Some(Script { text: request.command.to_string(), ..Default::default() }),
					..Default::default()
				},
				request.timeout_ms.map(Duration::from_millis),
			)
			.await
			.map_err(|error| resource_fault("run", error))?;
		Ok(HostShellRun { host: self.host.clone(), run })
	}

	async fn detach(&self, request: DetachRequest) -> Result<DetachedJob, Fault> {
		let cwd_uri = self.resolve_cwd(request.options.cwd.as_deref())?;
		let pty = request.options.pty;
		let environment = self.environment(&cwd_uri, request.options.env, pty).await;
		let started = self
			.host
			.start_process(StartProcess {
				name: request.name.to_string(),
				spec: Some(ProcessSpec {
					source: Some(Script {
						text: self.detached_command(&request.command).await,
						..Default::default()
					}),
					cwd_uri: cwd_uri.to_string(),
					env_delta: Some(environment),
					pty: pty.then(|| PtySpec {
						terminal: String::from("xterm-256color"),
						..Default::default()
					}),
					restart: Some(RestartSpec {
						policy: RestartPolicy::Never as i32,
						..Default::default()
					}),
					..Default::default()
				}),
				..Default::default()
			})
			.await
			.map_err(|error| resource_fault("detach", error))?;
		Ok(named_process(started))
	}
}

fn map_event(event: ExecEvent) -> Result<RunEvent, Fault> {
	match event {
		ExecEvent::Started { exec_id } => Ok(RunEvent::Started { exec_id }),
		ExecEvent::Output(frame) => {
			let channel = match EnvOutputChannel::try_from(frame.channel) {
				Ok(EnvOutputChannel::Stdout) => OutputChannel::Stdout,
				Ok(EnvOutputChannel::Stderr) => OutputChannel::Stderr,
				Ok(EnvOutputChannel::Pty) => OutputChannel::Pty,
				Ok(EnvOutputChannel::Unspecified) | Err(_) => {
					return Err(protocol_fault(
						"next_event",
						sf!("invalid output channel {}", frame.channel),
					));
				},
			};
			Ok(RunEvent::Output(Update {
				channel,
				data: CowBytes::owned(frame.data),
				sequence: frame.sequence,
				exec_id: frame.exec,
				started: false,
				terminal: channel == OutputChannel::Pty,
			}))
		},
		ExecEvent::Exit(event) => {
			let status = event
				.status
				.ok_or_else(|| protocol_fault("next_event", "terminal event omitted status"))?;
			let outcome = match EnvExecOutcome::try_from(status.outcome) {
				Ok(EnvExecOutcome::Exited) => ExecOutcome::Exited,
				Ok(EnvExecOutcome::Failed) => ExecOutcome::Failed,
				Ok(EnvExecOutcome::Timeout) => ExecOutcome::Timeout,
				Ok(EnvExecOutcome::Cancelled) => ExecOutcome::Cancelled,
				Ok(EnvExecOutcome::Denied) => ExecOutcome::Denied,
				Ok(EnvExecOutcome::Unspecified) | Err(_) => {
					return Err(protocol_fault(
						"next_event",
						sf!("invalid execution outcome {}", status.outcome),
					));
				},
			};
			let signal = (!status.signal.is_empty()).then(|| Str::from(status.signal));
			let spilled_output = status.spilled_output.map(|blob| BlobRef {
				hash:       Str::from(hex::encode(&blob.hash).into_string()),
				media_type: Str::from(blob.mime),
				byte_len:   blob.size,
			});
			Ok(RunEvent::Exit(ExecStatus {
				outcome,
				exit_code: status.exit_code,
				signal,
				wall_clock_ms: status.wall_clock_ms,
				spilled_output,
				aborted: status.aborted,
				effects_unknown: false,
				final_cwd_uri: (!event.final_cwd_uri.is_empty())
					.then(|| Str::from(event.final_cwd_uri)),
				final_cwd_revision: event.final_cwd_revision,
			}))
		},
	}
}

fn resource_fault(operation: &'static str, error: ExecError) -> Fault {
	protocol_fault(operation, sf!("{error}"))
}

fn protocol_fault(operation: &'static str, message: impl Into<Str>) -> Fault {
	Fault::Resource { operation: sf!(operation), message: message.into() }
}
