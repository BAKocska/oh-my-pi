use std::{
	collections::BTreeMap,
	future::{self, Future},
	path::Path,
	time::Duration,
};

use omp_core::{CowBytes, Str, encoding::hex, fmts};
use omp_proto::env::v1::{
	EnvironmentDelta, ExecOutcome as EnvExecOutcome, ExecRequest, OpenSessionRequest,
	OutputChannel as EnvOutputChannel, ProcessSpec, PtySpec, RestartPolicy, RestartSpec, Script,
	StartProcess,
};
use omp_tool::{BlobRef, JobOwner};
use omp_tools::{
	auto_background::DetachedJob,
	shell::{
		DetachRequest, ExecOutcome, ExecStatus, Fault, OutputChannel, RunEvent, RunRequest, Session,
		SessionOptions, ShellExec, ShellRun, Update,
	},
};

use super::exec::{ExecError, ExecEvent, ExecHost, ExecRun};

/// Shell resource adapter backed by the app-owned execution host.
#[derive(Clone)]
pub struct ShellExecHost {
	host:    ExecHost,
	cwd_uri: Str,
}

impl ShellExecHost {
	/// Binds shell execution to the workspace root URI used for sessions and
	/// detached processes.
	pub(crate) const fn new(host: ExecHost, cwd_uri: Str) -> Self {
		Self { host, cwd_uri }
	}
}
impl ShellExecHost {
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
}

fn hardened_environment(user: std::collections::BTreeMap<Str, Str>, pty: bool) -> EnvironmentDelta {
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
	if !pty {
		set.insert(String::from("TERM"), String::from("dumb"));
	}
	if std::env::var_os("OMP_BASH_NO_CI").is_some_and(|value| {
		let value = value.to_string_lossy();
		!value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
	}) {
		set.remove("CI");
	}
	set.extend(
		user
			.into_iter()
			.map(|(key, value)| (key.to_string(), value.to_string())),
	);
	EnvironmentDelta { set, ..Default::default() }
}

fn named_process(started: omp_proto::env::v1::ProcessStarted) -> DetachedJob {
	let id = fmts!("{}#{}", started.name, started.generation);
	DetachedJob {
		id,
		owner: JobOwner::NamedProcess {
			name:       Str::from(started.name),
			generation: started.generation,
		},
	}
}

fn cwd_fault(message: impl Into<Str>) -> Fault {
	Fault::Resource { operation: Str::new_static("cwd"), message: message.into() }
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
		let environment = hardened_environment(options.env, pty);
		let opened = self
			.host
			.open_session(OpenSessionRequest {
				cwd_uri: cwd_uri.to_string(),
				env_delta: Some(environment),
				pty: pty
					.then(|| PtySpec { terminal: String::from("xterm-256color"), ..Default::default() }),
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
		let environment = hardened_environment(request.options.env, pty);
		let started = self
			.host
			.start_process(StartProcess {
				name: request.name.to_string(),
				spec: Some(ProcessSpec {
					source: Some(Script { text: request.command.to_string(), ..Default::default() }),
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
						fmts!("invalid output channel {}", frame.channel),
					));
				},
			};
			Ok(RunEvent::Output(Update {
				channel,
				data: CowBytes::owned(frame.data),
				sequence: frame.sequence,
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
						fmts!("invalid execution outcome {}", status.outcome),
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
			}))
		},
	}
}

fn resource_fault(operation: &'static str, error: ExecError) -> Fault {
	protocol_fault(operation, fmts!("{error}"))
}

fn protocol_fault(operation: &'static str, message: impl Into<Str>) -> Fault {
	Fault::Resource { operation: Str::new_static(operation), message: message.into() }
}
