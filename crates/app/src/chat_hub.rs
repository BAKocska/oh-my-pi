//! Chat-owned composition for the unified hub tool.

use std::{
	collections::BTreeMap,
	sync::{Arc, LazyLock},
	time::{Duration as StdDuration, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use omp_agent::{Broker, BrokerInbox, CancelOutcome, DeliveryMode, JobBoard, PeerMessage};
use omp_core::{Duration, DurationUnit, Str, sf};
use omp_env::{EnvClient, ProcessAttachmentEvent};
use omp_proto::env::v1::{
	AttachOutput, EnvironmentDelta, ListProcesses, ProcessSpec, PtySpec, ReadyLog, ReadyProbe,
	ReadyTcp, RestartSpec, Script, SendInput, SignalProcess, StartProcess, StopProcess, ready_probe,
	send_input,
};
use omp_tool::{ArtifactLifetime, ExpectedArtifact, JobOwner, JobRef, Tool};
use omp_tools::hub::{Fault, HubBackend, HubRouter, Op, Params, Request, Response, RestartPolicy};
use parking_lot::Mutex;
use serde_json::json;

static ROUTER: LazyLock<HubRouter<ChatHubBackend>> = LazyLock::new(HubRouter::new);
const DEFAULT_ROUTE: &str = "*";

/// Produces the one process-global hub tool registered in the env registry.
pub fn tool() -> impl Tool {
	omp_tools::hub::tool(ChatHubRoute)
}

/// Installs one live chat composition, restoring the prior one on drop.
pub fn attach(backend: Arc<ChatHubBackend>) -> HubAttachment {
	let previous = ROUTER.attach(sf!(DEFAULT_ROUTE), backend);
	HubAttachment { previous }
}

pub struct HubAttachment {
	previous: Option<Arc<ChatHubBackend>>,
}

impl Drop for HubAttachment {
	fn drop(&mut self) {
		ROUTER.detach(DEFAULT_ROUTE);
		if let Some(previous) = self.previous.take() {
			ROUTER.attach(sf!(DEFAULT_ROUTE), previous);
		}
	}
}

#[derive(Clone, Copy)]
struct ChatHubRoute;

impl HubBackend for ChatHubRoute {
	async fn execute<'a>(
		&'a self,
		_caller_id: &'a str,
		request: Request,
	) -> Result<Response, Fault> {
		// The native env connection is the authenticated owner; the live chat
		// attachment supplies the narrower agent identity used for peer routing.
		ROUTER.execute(DEFAULT_ROUTE, request).await
	}
}

pub struct ChatHubBackend {
	broker:   Broker,
	inbox:    tokio::sync::Mutex<BrokerInbox>,
	jobs:     Arc<JobBoard>,
	env:      EnvClient,
	agent_id: Str,
	session:  Str,
	launches: Mutex<BTreeMap<Str, Params>>,
}

impl ChatHubBackend {
	pub(crate) fn new(
		broker: Broker,
		inbox: BrokerInbox,
		jobs: Arc<JobBoard>,
		env: EnvClient,
		agent_id: Str,
		session: Str,
	) -> Self {
		Self {
			broker,
			inbox: tokio::sync::Mutex::new(inbox),
			jobs,
			env,
			agent_id,
			session,
			launches: Mutex::new(BTreeMap::new()),
		}
	}

	fn response(value: serde_json::Value) -> Result<Response, Fault> {
		serde_json::to_string_pretty(&value)
			.map(|text| Response { text: Str::from(text), useless: false })
			.map_err(|error| fault(error.to_string()))
	}

	async fn process_generation(&self, name: &str) -> Result<u64, Fault> {
		self
			.env
			.list_processes(ListProcesses { props: None })
			.await
			.map_err(|error| fault(error.to_string()))?
			.processes
			.into_iter()
			.find(|process| process.name == name)
			.map(|process| process.generation)
			.ok_or_else(|| fault(format!("process {name:?} was not found")))
	}

	async fn peer_send(&self, params: &Params) -> Result<Response, Fault> {
		let to = params
			.to
			.clone()
			.ok_or_else(|| fault("peer recipient is required"))?;
		if to.eq_ignore_ascii_case(self.agent_id.as_str()) {
			return Err(fault("cannot send a hub message to the calling agent"));
		}
		let id = Str::from(ulid::Ulid::generate().to_string());
		let message = PeerMessage {
			id:         id.clone(),
			from:       self.agent_id.clone(),
			to:         to.clone(),
			text:       params.message.clone().unwrap_or_default(),
			mode:       DeliveryMode::Aside,
			reply_to:   params.reply_to.clone(),
			sent_ms:    now_ms(),
			session_id: self.session.clone(),
		};
		let receipts = self
			.broker
			.send(message)
			.map_err(|error| fault(error.to_string()))?;
		if params.await_reply {
			let timeout = wait_timeout(params.timeout_ms);
			let reply = self
				.inbox
				.lock()
				.await
				.wait_for_timeout(Some(to.as_str()), Some(id.as_str()), timeout)
				.await
				.map_err(|error| fault(error.to_string()))?;
			return Self::response(
				json!({ "receipt": receipts.first().map(ToString::to_string), "reply": reply.map(message_json) }),
			);
		}
		Self::response(json!({
			"id": id,
			"receipts": receipts.iter().map(ToString::to_string).collect::<Vec<_>>(),
		}))
	}

	async fn wait(&self, params: &Params) -> Result<Response, Fault> {
		if params.name.is_some() {
			return self.process_wait(params).await;
		}
		let mut jobs = self.jobs.watch(params.ids.as_deref());
		if jobs.is_empty() {
			let message = self
				.inbox
				.lock()
				.await
				.wait_for_timeout(params.from_peer.as_deref(), None, wait_timeout(params.timeout_ms))
				.await
				.map_err(|error| fault(error.to_string()))?;
			return Self::response(json!({ "message": message.map(message_json) }));
		}
		let mut inbox = self.inbox.lock().await;
		let timeout = wait_timeout(params.timeout_ms);
		let result = async {
			tokio::select! {
				biased;
				peer = inbox.wait_for_timeout(params.from_peer.as_deref(), None, timeout) => {
					peer.map(|message| (message.map(message_json), None)).map_err(|error| fault(error.to_string()))
				},
				settlement = jobs.next() => Ok((None, settlement)),
			}
		};
		let (peer, settlement) = if let Some(timeout) = timeout {
			tokio::time::timeout(timeout, result)
				.await
				.map_err(|_| fault("hub wait timed out"))??
		} else {
			result.await?
		};
		if let Some(settlement) = settlement {
			let id = settlement.job.id.clone();
			let item = format!("{:?}", settlement.item);
			settlement
				.lease
				.claim()
				.map_err(|error| fault(error.to_string()))?;
			return Self::response(json!({ "job": id, "settled": true, "item": item }));
		}
		Self::response(json!({ "message": peer }))
	}

	async fn cancel_jobs(&self, params: &Params) -> Result<Response, Fault> {
		let grace = Duration::new(5, DurationUnit::Seconds);
		let mut outcomes = BTreeMap::new();
		for id in params.ids.as_deref().unwrap_or_default() {
			let outcome = self
				.jobs
				.cancel(id, grace)
				.await
				.map_err(|error| fault(error.to_string()))?;
			outcomes.insert(id.as_str(), match outcome {
				CancelOutcome::Missing => "missing",
				CancelOutcome::AlreadySettled => "already_settled",
				CancelOutcome::Accepted => "accepted",
			});
		}
		Self::response(json!({ "jobs": outcomes }))
	}

	async fn start(&self, params: &Params) -> Result<Response, Fault> {
		let name = params
			.name
			.as_ref()
			.ok_or_else(|| fault("process name is required"))?;
		let command = command_text(
			params
				.application
				.as_deref()
				.ok_or_else(|| fault("application is required"))?,
			params.args.as_deref().unwrap_or_default(),
		);
		let ready = params.ready.as_ref().map_or_else(Vec::new, |ready| {
			let timeout_ms = ready.timeout.unwrap_or(30.0).mul_add(1_000.0, 0.0) as u64;
			let mut probes = Vec::new();
			if let Some(pattern) = &ready.log {
				probes.push(ReadyProbe {
					probe: Some(ready_probe::Probe::Log(ReadyLog {
						pattern: pattern.to_string(),
						props:   None,
					})),
					timeout_ms,
					props: None,
				});
			}
			if let Some(port) = ready.port {
				probes.push(ReadyProbe {
					probe: Some(ready_probe::Probe::Tcp(ReadyTcp {
						host:  ready.host.as_deref().unwrap_or("127.0.0.1").to_owned(),
						port:  u32::from(port),
						props: None,
					})),
					timeout_ms,
					props: None,
				});
			}
			probes
		});
		let restart = match params.restart.unwrap_or(RestartPolicy::No) {
			RestartPolicy::No => omp_proto::env::v1::RestartPolicy::Never,
			RestartPolicy::OnFailure => omp_proto::env::v1::RestartPolicy::OnFailure,
			RestartPolicy::Always => omp_proto::env::v1::RestartPolicy::Always,
		};
		let cwd_uri = params.cwd.as_ref().map_or_else(
			|| {
				self
					.env
					.info()
					.map_or_else(String::new, |info| info.root_uri)
			},
			|cwd| {
				if cwd.contains("://") {
					cwd.to_string()
				} else {
					format!("file://{cwd}")
				}
			},
		);
		let cwd =
			omp_core::EnvPath::new(Str::from(cwd_uri)).map_err(|error| fault(error.to_string()))?;
		let started = self
			.env
			.start_process(&cwd, StartProcess {
				name: name.to_string(),
				spec: Some(ProcessSpec {
					source:    Some(Script { text: command, props: None }),
					cwd_uri:   String::new(),
					env_delta: Some(EnvironmentDelta {
						set:   params
							.env
							.clone()
							.unwrap_or_default()
							.into_iter()
							.map(|(key, value)| (key.to_string(), value.to_string()))
							.collect(),
						unset: Vec::new(),
						props: None,
					}),
					pty:       params
						.pty
						.unwrap_or(true)
						.then(|| PtySpec { terminal: "xterm-256color".to_owned(), ..Default::default() }),
					restart:   Some(RestartSpec { policy: restart as i32, ..Default::default() }),
					props:     None,
				}),
				ready,
				props: None,
			})
			.await
			.map_err(|error| fault(error.to_string()))?;
		self.launches.lock().insert(name.clone(), params.clone());
		let lifetime = if params.detached {
			ArtifactLifetime::Durable
		} else {
			ArtifactLifetime::Session
		};
		self.jobs.register(JobRef {
			id:       Str::from(format!("process:{}:{}", started.name, started.generation)),
			owner:    JobOwner::NamedProcess {
				name:       Str::from(started.name.clone()),
				generation: started.generation,
			},
			artifact: ExpectedArtifact {
				description: Str::from(format!("completion of named process {}", started.name)),
				media_type: None,
				lifetime,
			},
		});
		Self::response(
			json!({ "name": started.name, "generation": started.generation, "ready": true }),
		)
	}

	async fn process_wait(&self, params: &Params) -> Result<Response, Fault> {
		let name = params
			.name
			.as_deref()
			.ok_or_else(|| fault("process name is required"))?;
		let generation = self.process_generation(name).await?;
		let mut attachment = self
			.env
			.attach_output(AttachOutput {
				name: name.to_owned(),
				after_sequence: params.cursor.unwrap_or(0),
				generation,
				props: None,
			})
			.await
			.map_err(|error| fault(error.to_string()))?;
		let deadline = StdDuration::from_secs_f64(params.timeout.unwrap_or(30.0));
		let event = tokio::time::timeout(deadline, async {
			while let Some(event) = attachment.next_event().await.map_err(|error| fault(error.to_string()))? {
				match event {
					ProcessAttachmentEvent::Output(output) if params.pattern.as_deref().is_none_or(|pattern| String::from_utf8_lossy(&output.data).contains(pattern)) => return Ok(Some(json!({ "name": output.name, "cursor": output.sequence, "output": String::from_utf8_lossy(&output.data) }))),
					ProcessAttachmentEvent::State(state) => {
						let target = params.wait_for.as_deref().unwrap_or("exit");
						let process_state = omp_proto::env::v1::ProcessState::try_from(
							state.process.as_ref().map_or(0, |process| process.state),
						)
						.ok();
						if (target == "ready" && matches!(process_state, Some(omp_proto::env::v1::ProcessState::Ready | omp_proto::env::v1::ProcessState::Running))) || (target == "exit" && matches!(process_state, Some(omp_proto::env::v1::ProcessState::Exited | omp_proto::env::v1::ProcessState::Stopped | omp_proto::env::v1::ProcessState::Failed))) { return Ok(Some(json!({ "name": name, "state": format!("{process_state:?}") }))); }
					},
					_ => {},
				}
			}
			Ok::<_, Fault>(None)
		}).await.map_err(|_| fault("process wait timed out"))??;
		Self::response(json!({ "event": event }))
	}

	async fn logs(&self, params: &Params) -> Result<Response, Fault> {
		let name = params
			.name
			.as_deref()
			.ok_or_else(|| fault("process name is required"))?;
		let generation = self.process_generation(name).await?;
		let mut attachment = self
			.env
			.attach_output(AttachOutput {
				name: name.to_owned(),
				after_sequence: params.cursor.unwrap_or(0),
				generation,
				props: None,
			})
			.await
			.map_err(|error| fault(error.to_string()))?;
		let limit = usize::from(params.lines.unwrap_or(100));
		let mut lines = Vec::new();
		let mut cursor = params.cursor.unwrap_or(0);
		let idle = if params.follow {
			StdDuration::from_secs_f64(params.timeout.unwrap_or(30.0))
		} else {
			StdDuration::from_millis(20)
		};
		loop {
			match tokio::time::timeout(idle, attachment.next_event()).await {
				Ok(Ok(Some(ProcessAttachmentEvent::Output(output)))) => {
					cursor = cursor.max(output.sequence);
					for line in String::from_utf8_lossy(&output.data).lines() {
						if params
							.grep
							.as_deref()
							.is_none_or(|pattern| line.contains(pattern))
						{
							lines.push(line.to_owned());
						}
					}
					if params.follow && !lines.is_empty() {
						break;
					}
				},
				Ok(Ok(Some(ProcessAttachmentEvent::State(_)) | None)) | Err(_) => break,
				Ok(Ok(Some(ProcessAttachmentEvent::Attached(_)))) => {},
				Ok(Err(error)) => return Err(fault(error.to_string())),
			}
		}
		if !params.head && lines.len() > limit {
			lines.drain(..lines.len() - limit);
		} else {
			lines.truncate(limit);
		}
		Self::response(json!({ "name": name, "lines": lines, "cursor": cursor }))
	}

	async fn process_send(&self, params: &Params) -> Result<Response, Fault> {
		let name = params
			.name
			.as_deref()
			.ok_or_else(|| fault("process name is required"))?;
		let generation = self.process_generation(name).await?;
		if let Some(signal) = params.signal {
			self
				.env
				.signal_process(SignalProcess {
					name: name.to_owned(),
					signal: format!("{signal:?}").to_uppercase(),
					generation,
					props: None,
				})
				.await
				.map_err(|error| fault(error.to_string()))?;
		}
		let mut data = params
			.text
			.as_deref()
			.unwrap_or_default()
			.as_bytes()
			.to_vec();
		for key in params.keys.as_deref().unwrap_or_default() {
			append_key(&mut data, key);
		}
		if params.enter.unwrap_or(true) && params.text.is_some() {
			data.push(b'\n');
		}
		if !data.is_empty() {
			self
				.env
				.send_process_input(SendInput {
					name: name.to_owned(),
					input: Some(send_input::Input::Data(Bytes::from(data))),
					generation,
					props: None,
				})
				.await
				.map_err(|error| fault(error.to_string()))?;
		}
		Self::response(json!({ "name": name, "accepted": true }))
	}
}

impl HubBackend for ChatHubBackend {
	async fn execute<'a>(
		&'a self,
		_caller_id: &'a str,
		request: Request,
	) -> Result<Response, Fault> {
		let params = request.params;
		match params.op {
			Op::Send if params.to.is_some() => self.peer_send(&params).await,
			Op::Send => self.process_send(&params).await,
			Op::Wait => self.wait(&params).await,
			Op::Inbox => Self::response(
				json!({ "messages": self.inbox.lock().await.inbox(params.peek).into_iter().map(message_json).collect::<Vec<_>>() }),
			),
			Op::List => Self::response(json!({ "peers": self.broker.peers(None) })),
			Op::Jobs => Self::response(
				json!({ "jobs": self.jobs.snapshot().into_iter().map(|job| job.id).collect::<Vec<_>>() }),
			),
			Op::Cancel => self.cancel_jobs(&params).await,
			Op::Start => self.start(&params).await,
			Op::Ps => {
				let list = self
					.env
					.list_processes(ListProcesses { props: None })
					.await
					.map_err(|error| fault(error.to_string()))?;
				Self::response(
					json!({ "processes": list.processes.into_iter().map(|process| json!({ "name": process.name, "generation": process.generation, "state": process.state })).collect::<Vec<_>>() }),
				)
			},
			Op::Logs => self.logs(&params).await,
			Op::Stop => {
				let name = params
					.name
					.as_deref()
					.ok_or_else(|| fault("process name is required"))?;
				let grace_ms = params.timeout.unwrap_or(5.0).mul_add(1_000.0, 0.0) as u64;
				let generation = self.process_generation(name).await?;
				self
					.env
					.stop_process(StopProcess {
						name: name.to_owned(),
						grace_ms,
						generation,
						props: None,
					})
					.await
					.map_err(|error| fault(error.to_string()))?;
				Self::response(json!({ "name": name, "stopping": true }))
			},
			Op::Restart => {
				let name = params
					.name
					.as_deref()
					.ok_or_else(|| fault("process name is required"))?;
				let launch = self
					.launches
					.lock()
					.get(name)
					.cloned()
					.ok_or_else(|| fault("process launch specification is not retained"))?;
				let generation = self.process_generation(name).await?;
				self
					.env
					.stop_process(StopProcess {
						name: name.to_owned(),
						grace_ms: 5_000,
						generation,
						props: None,
					})
					.await
					.map_err(|error| fault(error.to_string()))?;
				self.start(&launch).await
			},
			Op::Describe => {
				let name = params
					.name
					.as_deref()
					.ok_or_else(|| fault("process name is required"))?;
				let launch = self.launches.lock().get(name).cloned();
				let list = self
					.env
					.list_processes(ListProcesses { props: None })
					.await
					.map_err(|error| fault(error.to_string()))?;
				let process = list
					.processes
					.into_iter()
					.find(|process| process.name == name);
				Self::response(
					json!({ "name": name, "generation": process.as_ref().map(|process| process.generation), "state": process.as_ref().map(|process| process.state), "retained": launch.is_some() }),
				)
			},
		}
	}
}

const fn wait_timeout(timeout_ms: Option<u64>) -> Option<StdDuration> {
	match timeout_ms {
		Some(0) | None => None,
		Some(timeout) => Some(StdDuration::from_millis(timeout)),
	}
}

fn message_json(message: PeerMessage) -> serde_json::Value {
	json!({ "id": message.id, "from": message.from, "to": message.to, "message": message.text, "replyTo": message.reply_to, "sentMs": message.sent_ms })
}

fn command_text(application: &str, args: &[Str]) -> String {
	std::iter::once(application)
		.chain(args.iter().map(Str::as_str))
		.map(shell_word)
		.collect::<Vec<_>>()
		.join(" ")
}

fn shell_word(word: &str) -> String {
	if !word.is_empty()
		&& word
			.bytes()
			.all(|byte| byte.is_ascii_alphanumeric() || b"-_./:".contains(&byte))
	{
		return word.to_owned();
	}
	format!("'{}'", word.replace('\'', "'\\''"))
}

fn append_key(data: &mut Vec<u8>, key: &str) {
	match key.to_ascii_uppercase().as_str() {
		"ENTER" => data.push(b'\n'),
		"TAB" => data.push(b'\t'),
		"ESCAPE" => data.push(0x1b),
		"CTRL_C" => data.push(0x03),
		"CTRL_D" => data.push(0x04),
		"UP" => data.extend_from_slice(b"\x1b[A"),
		"DOWN" => data.extend_from_slice(b"\x1b[B"),
		"RIGHT" => data.extend_from_slice(b"\x1b[C"),
		"LEFT" => data.extend_from_slice(b"\x1b[D"),
		_ => {},
	}
}

fn now_ms() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or_default()
		.as_millis()
		.try_into()
		.unwrap_or(u64::MAX)
}

fn fault(message: impl Into<Str>) -> Fault {
	Fault { message: message.into() }
}
