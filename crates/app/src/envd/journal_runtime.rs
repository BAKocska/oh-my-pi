//! Authenticated external CONTROL routing for session index and durable state.

use std::{
	collections::BTreeMap,
	sync::Arc,
	time::{SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use omp_agent::{JournalQuery, SessionStateWatchEvent, control::ControlSender};
use omp_core::{ArtifactUrl, Str};
use omp_proto::toolhost::v1::{
	ArtifactRow, JournalHostEnvelope, SessionRow, StateChanged as WireStateChanged,
	StateValue as WireStateValue, UsageReport, journal_host_envelope,
};
use omp_storage::{
	blob::BlobRef,
	gc::{ArtifactCatalog, ArtifactRecord, ArtifactRequest, Error as ArtifactError},
	index::{SessionFilter, SessionIndex, UsageQuery},
	state::{
		DurableRequest, GenerationFence, StateAuthority, StateChange, StateRevision, StateScope,
		StateStore,
	},
	transcript::SessionId,
};
use omp_tool::ArtifactLifetime;
use parking_lot::Mutex;

use super::{blobs::BlobHost, worker::ExternalJournalCall};
use crate::exthost::control::{
	ExternalJournalRequest, JournalConnectionIdentity, artifact_rows, journal_rows, session_rows,
	usage_rows,
};

/// Environment-owned endpoint for external Journal CONTROL requests.
#[derive(Clone)]
pub struct ExternalJournalActor {
	sender: flume::Sender<ExternalJournalCall>,
	agent:  Arc<Mutex<Option<ControlSender>>>,
}

impl ExternalJournalActor {
	/// Starts one actor over the Environment's authoritative storage handles.
	///
	/// # Errors
	///
	/// Fails if the shared artifact catalog cannot be opened.
	pub(crate) fn spawn(
		sessions: Arc<SessionIndex>,
		state: Option<Arc<StateStore>>,
		blobs: BlobHost,
		session_id: Str,
		project_scope: Str,
		project_path: Str,
	) -> Result<Self, super::server::EnvdError> {
		let catalog = Arc::new(Mutex::new(
			ArtifactCatalog::open(blobs.store())
				.map_err(|error| super::server::EnvdError::Blob(Str::from(error.to_string())))?,
		));
		let (sender, receiver) = flume::unbounded::<ExternalJournalCall>();
		let agent = Arc::new(Mutex::new(None));
		let actor_agent = Arc::clone(&agent);
		tokio::spawn(async move {
			while let Ok(call) = receiver.recv_async().await {
				let sessions = Arc::clone(&sessions);
				let state = state.clone();
				let session_id = session_id.clone();
				let project_scope = project_scope.clone();
				let project_path = project_path.clone();
				let catalog = Arc::clone(&catalog);
				let agent = Arc::clone(&actor_agent);
				tokio::spawn(async move {
					let reply = call.reply.clone();
					if let Err(error) = dispatch(
						call.request,
						call.identity,
						&sessions,
						state.as_deref(),
						&session_id,
						&project_scope,
						&project_path,
						&catalog,
						&agent,
						&reply,
					)
					.await
					{
						let _ = reply.send(Err(error));
					}
				});
			}
		});
		Ok(Self { sender, agent })
	}

	/// Returns the endpoint installed into each authenticated extension host.
	#[must_use]
	pub(crate) fn sender(&self) -> flume::Sender<ExternalJournalCall> {
		self.sender.clone()
	}

	/// Installs the sole active Agent Journal mailbox before child activation.
	///
	/// # Errors
	///
	/// Refuses a second binding rather than transferring session authority.
	pub(crate) fn bind_agent(&self, sender: ControlSender) -> Result<(), super::server::EnvdError> {
		let mut agent = self.agent.lock();
		if agent.is_some() {
			return Err(
				super::worker::WorkerError::Protocol(Str::new_static(
					"agent journal CONTROL is already bound",
				))
				.into(),
			);
		}
		*agent = Some(sender);
		Ok(())
	}
}

#[allow(
	clippy::too_many_arguments,
	reason = "the actor carries distinct authenticated authorities"
)]
async fn dispatch(
	request: ExternalJournalRequest,
	identity: JournalConnectionIdentity,
	sessions: &SessionIndex,
	state: Option<&StateStore>,
	session_id: &Str,
	project_scope: &Str,
	project_path: &Str,
	catalog: &Mutex<ArtifactCatalog>,
	agent: &Mutex<Option<ControlSender>>,
	reply: &flume::Sender<Result<JournalHostEnvelope, Str>>,
) -> Result<(), Str> {
	match request {
		ExternalJournalRequest::Query { extension, query, .. } => {
			if query.session != session_id.as_str() {
				return unsupported("cross-session journal query");
			}
			let sender = bound_agent(agent)?;
			let kinds = if query.kinds.is_empty() {
				vec![None]
			} else {
				query
					.kinds
					.into_iter()
					.map(|kind| Some(Str::from(kind)))
					.collect()
			};
			let limit = query.limit.map(|limit| limit as usize);
			let queries = kinds
				.into_iter()
				.map(|kind| JournalQuery {
					caller_extension: extension.clone(),
					granted_extensions: Vec::new(),
					kind,
					rev: None,
					since: query.since_index,
					limit,
					live: query.live_only,
				})
				.collect();
			let rows = sender.query(queries).await.map_err(display_error)?;
			emit(reply, journal_rows(&rows));
		},
		ExternalJournalRequest::ListSessions { query, .. } => {
			if query.cursor.is_some() {
				return unsupported("sessions cursor pagination");
			}
			let page = sessions
				.list(&SessionFilter {
					project: Some(project_path.clone()),
					limit: query.limit.unwrap_or(100),
					..SessionFilter::default()
				})
				.map_err(display_error)?;
			let rows = page.sessions.into_iter().map(|row| SessionRow {
				session:       row.id.0.to_string(),
				cwd_uri:       row.cwd.to_string(),
				updated_at_ms: row.updated_ms,
				terminal:      false,
				props:         None,
			});
			emit(reply, session_rows(rows));
		},
		ExternalJournalRequest::QueryUsage { query, .. } => {
			let session = query.session.map(|session| SessionId(Str::from(session)));
			let usage = sessions
				.usage(&UsageQuery {
					project: session.is_none().then(|| project_path.clone()),
					session,
					include_subagents: query.include_tree,
					..UsageQuery::default()
				})
				.map_err(display_error)?;
			emit(
				reply,
				usage_rows(usage.into_iter().map(|bucket| UsageReport {
					usage:    Some(bucket.usage),
					terminal: false,
					props:    None,
				})),
			);
		},
		ExternalJournalRequest::StateGet { extension, request, .. } => {
			let authority = state_authority(identity, extension, session_id, project_scope)?;
			let scope = state_scope(request.scope)?;
			let namespace = requested_namespace(&request.namespace, authority.namespace());
			let value = if scope == StateScope::Session {
				require_own_namespace(namespace, authority.namespace())?;
				bound_agent(agent)?
					.session_state_get(authority, Str::from(request.key))
					.await
					.map_err(display_error)?
					.map(|value| {
						(value.revision.get(), Bytes::copy_from_slice(value.value.get().as_bytes()))
					})
			} else {
				state_owner(state)?
					.value(&authority, scope, namespace, &request.key)
					.map_err(display_error)?
					.map(|value| (value.revision.get(), Bytes::copy_from_slice(value.value.as_ref())))
			};
			let _ = reply.send(Ok(state_value(value)));
		},
		ExternalJournalRequest::StateCas { stamp, author, request, .. } => {
			let authority = state_authority(
				identity,
				Str::from(author.provenance.extension_id()),
				session_id,
				project_scope,
			)?;
			require_own_namespace(
				requested_namespace(&request.namespace, authority.namespace()),
				authority.namespace(),
			)?;
			let scope = state_scope(request.scope)?;
			let expected =
				(request.expected_revision != 0).then(|| StateRevision::new(request.expected_revision));
			let durable =
				DurableRequest::new(stamp.request_id, Some(stamp.idempotency_key), GenerationFence {
					host:    stamp.host_generation,
					session: stamp.session_generation,
				})
				.map_err(display_error)?;
			let value = if scope == StateScope::Session {
				let raw = serde_json::from_slice(&request.value_json).map_err(display_error)?;
				let value = bound_agent(agent)?
					.session_state_compare_exchange(
						epoch_millis()?,
						authority,
						Str::from(request.key),
						expected,
						raw,
						durable,
					)
					.await
					.map_err(display_error)?;
				(value.revision.get(), Bytes::copy_from_slice(value.value.get().as_bytes()))
			} else {
				let value = state_owner(state)?
					.compare_exchange(
						&authority,
						scope,
						Str::from(request.key),
						expected,
						&request.value_json,
						&durable,
					)
					.map_err(display_error)?;
				(value.revision.get(), Bytes::copy_from_slice(value.value.as_ref()))
			};
			let _ = reply.send(Ok(state_value(Some(value))));
		},
		ExternalJournalRequest::StateWatch { extension, request, .. } => {
			let authority = state_authority(identity, extension, session_id, project_scope)?;
			let scope = state_scope(request.scope)?;
			let namespace = requested_namespace(&request.namespace, authority.namespace());
			let since =
				(request.after_revision != 0).then(|| StateRevision::new(request.after_revision));
			if scope == StateScope::Session {
				require_own_namespace(namespace, authority.namespace())?;
				let events = bound_agent(agent)?
					.session_state_watch(authority, Str::from(request.key), since)
					.await
					.map_err(display_error)?;
				loop {
					tokio::select! {
						event = events.recv_async() => {
							let Ok(event) = event else { break };
							match event {
								SessionStateWatchEvent::Value(value) => {
									if reply
										.send(Ok(state_changed(
											value.revision.get(),
											Bytes::copy_from_slice(value.value.get().as_bytes()),
										)))
										.is_err()
									{
										break;
									}
								},
								SessionStateWatchEvent::Terminal(_) => break,
							}
						},
						() = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
							if reply.is_disconnected() {
								break;
							}
						},
					}
				}
			} else {
				let watcher = state_owner(state)?
					.watch(&authority, scope, namespace, since)
					.map_err(display_error)?;
				loop {
					tokio::select! {
						change = watcher.recv_async() => {
							let Ok(change) = change else { break };
							if let StateChange::Value(value) = change
								&& value.key == request.key
								&& reply
									.send(Ok(state_changed(
										value.revision.get(),
										Bytes::copy_from_slice(value.value.as_ref()),
									)))
									.is_err()
							{
								break;
							}
						},
						() = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
							if reply.is_disconnected() {
								break;
							}
						},
					}
				}
			}
		},
		ExternalJournalRequest::AdoptArtifact { stamp, author, request, .. } => {
			let lifetime = parse_lifetime(&request.lifetime)?;
			let current_session = SessionId(session_id.clone());
			let durable_request = ArtifactRequest {
				principal:          author.principal.id(),
				extension:          author.provenance.extension_id(),
				idempotency_key:    stamp.idempotency_key.as_str(),
				session:            &current_session,
				host_generation:    stamp.host_generation,
				session_generation: stamp.session_generation,
			};
			let record = if request.source_url.starts_with("artifact://") {
				let source = parse_artifact_url(&request.source_url)?;
				catalog
					.lock()
					.adopt_url_once(durable_request, &source, None, lifetime)
					.map_err(artifact_error)?
			} else {
				let (hash, claimed_size) = parse_blob_source(&request.source_url)?;
				catalog
					.lock()
					.adopt_once(durable_request, hash, claimed_size, lifetime)
					.map_err(artifact_error)?
			};
			let row = artifact_row(&record, &current_session)
				.ok_or_else(|| Str::new_static("artifact is not visible from this session"))?;
			emit(reply, artifact_rows([row]));
		},
		ExternalJournalRequest::StatArtifact { request, .. } => {
			let url = parse_artifact_url(&request.url)?;
			let current_session = SessionId(session_id.clone());
			let record = catalog
				.lock()
				.stat_url(&current_session, &url)
				.map_err(display_error)?;
			let row = artifact_row(&record, &current_session)
				.ok_or_else(|| Str::new_static("artifact is not visible from this session"))?;
			emit(reply, artifact_rows([row]));
		},
		ExternalJournalRequest::ListArtifacts { request, .. } => {
			let requested_session = request
				.session
				.filter(|session| !session.is_empty())
				.map(Str::from);
			let requested_session = requested_session
				.as_ref()
				.map_or_else(|| SessionId(session_id.clone()), |session| SessionId(session.clone()));
			if requested_session.0.as_str() != session_id.as_str() {
				return Err(Str::new_static(
					"Denied: non-durable cross-session artifact listing is not permitted",
				));
			}
			let cursor = request
				.cursor
				.as_deref()
				.map(str::parse::<u64>)
				.transpose()
				.map_err(display_error)?;
			let page = catalog
				.lock()
				.list(Some(&requested_session), cursor, request.limit.unwrap_or(200))
				.map_err(display_error)?;
			let current_session = SessionId(session_id.clone());
			let mut rows = page
				.records
				.iter()
				.filter_map(|record| artifact_row(record, &current_session))
				.collect::<Vec<_>>();
			if let Some(cursor) = page.next_cursor
				&& let Some(last) = rows.last_mut()
			{
				last.props = Some(cursor_props(cursor));
			}
			emit(reply, artifact_rows(rows));
		},
		ExternalJournalRequest::PinArtifact { stamp, author, request, .. } => {
			let url = parse_artifact_url(&request.url)?;
			let lifetime = parse_lifetime(&request.lifetime)?;
			let current_session = SessionId(session_id.clone());
			let durable_request = ArtifactRequest {
				principal:          author.principal.id(),
				extension:          author.provenance.extension_id(),
				idempotency_key:    stamp.idempotency_key.as_str(),
				session:            &current_session,
				host_generation:    stamp.host_generation,
				session_generation: stamp.session_generation,
			};
			let record = catalog
				.lock()
				.pin_url_once(durable_request, &url, lifetime)
				.map_err(artifact_error)?;
			let row = artifact_row(&record, &current_session)
				.ok_or_else(|| Str::new_static("artifact is not visible from this session"))?;
			emit(reply, artifact_rows([row]));
		},
	}
	Ok(())
}
fn parse_blob_source(source: &str) -> Result<([u8; 32], Option<u64>), Str> {
	let source = source
		.strip_prefix("blob://")
		.ok_or_else(|| Str::new_static("invalid blob source URL"))?;
	let (resource, query) = source
		.split_once('?')
		.map_or((source, None), |(resource, query)| (resource, Some(query)));
	let digest = resource.strip_prefix("b3/").unwrap_or(resource);
	let claimed_size = query
		.and_then(|query| {
			query
				.split('&')
				.find_map(|field| field.strip_prefix("size="))
		})
		.map(str::parse::<u64>)
		.transpose()
		.map_err(display_error)?;
	BlobRef::parse_hex(digest, claimed_size.unwrap_or_default())
		.map(|reference| (reference.hash, claimed_size))
		.map_err(display_error)
}

fn parse_artifact_url(value: &str) -> Result<ArtifactUrl, Str> {
	let url = ArtifactUrl::new(value).map_err(display_error)?;
	if url.selector().is_some() {
		return Err(Str::new_static("artifact CONTROL URLs cannot carry read selectors"));
	}
	Ok(url)
}

fn parse_lifetime(value: &str) -> Result<ArtifactLifetime, Str> {
	if value.is_empty() {
		Ok(ArtifactLifetime::Session)
	} else {
		value.parse().map_err(display_error)
	}
}

fn artifact_row(record: &ArtifactRecord, session: &SessionId) -> Option<ArtifactRow> {
	let lifetime: &'static str = record.lifetime.into();
	Some(ArtifactRow {
		url:      record.url_for(session)?.as_str().to_owned(),
		hash:     Bytes::copy_from_slice(&record.reference.hash),
		size:     record.reference.size,
		lifetime: lifetime.to_owned(),
		pinned:   record.pinned,
		terminal: false,
		props:    None,
	})
}

fn cursor_props(cursor: u64) -> omp_proto::inference::v1::ValueMap {
	omp_proto::inference::v1::ValueMap {
		fields: BTreeMap::from([("next_cursor".to_owned(), omp_proto::inference::v1::Value {
			kind: Some(omp_proto::inference::v1::value::Kind::Uint(cursor)),
		})]),
	}
}

fn emit(
	reply: &flume::Sender<Result<JournalHostEnvelope, Str>>,
	rows: impl Iterator<Item = JournalHostEnvelope>,
) {
	for row in rows {
		if reply.send(Ok(row)).is_err() {
			break;
		}
	}
}

fn state_authority(
	identity: JournalConnectionIdentity,
	extension: Str,
	session_id: &Str,
	project_id: &Str,
) -> Result<StateAuthority, Str> {
	let generation =
		GenerationFence { host: identity.host_generation, session: identity.session_generation };
	StateAuthority::new_core(
		identity.principal,
		identity.provenance,
		extension,
		session_id.clone(),
		project_id.clone(),
		generation,
	)
	.map_err(display_error)
}

fn state_scope(scope: i32) -> Result<StateScope, Str> {
	match omp_proto::toolhost::v1::StateScope::try_from(scope) {
		Ok(omp_proto::toolhost::v1::StateScope::Session) => Ok(StateScope::Session),
		Ok(omp_proto::toolhost::v1::StateScope::Project) => Ok(StateScope::Project),
		Ok(omp_proto::toolhost::v1::StateScope::User) => Ok(StateScope::User),
		Ok(omp_proto::toolhost::v1::StateScope::Organization) => Ok(StateScope::Organization),
		_ => Err(Str::new_static("Unsupported: invalid durable state scope")),
	}
}

const fn requested_namespace<'a>(requested: &'a str, own: &'a str) -> &'a str {
	if requested.is_empty() { own } else { requested }
}

fn require_own_namespace(requested: &str, own: &str) -> Result<(), Str> {
	if requested == own {
		Ok(())
	} else {
		Err(Str::new_static(
			"Denied: state writes and SESSION state reads require the authenticated namespace",
		))
	}
}

fn state_owner(state: Option<&StateStore>) -> Result<&StateStore, Str> {
	state.ok_or_else(|| {
		Str::new_static("Unsupported: this environment is not the durable state authority")
	})
}

fn bound_agent(agent: &Mutex<Option<ControlSender>>) -> Result<ControlSender, Str> {
	agent
		.lock()
		.clone()
		.ok_or_else(|| Str::new_static("agent journal CONTROL is not bound"))
}

fn state_value(value: Option<(u64, Bytes)>) -> JournalHostEnvelope {
	let (revision, value_json, present) =
		value.map_or((0, Bytes::new(), false), |(revision, value)| (revision, value, true));
	JournalHostEnvelope {
		body:  Some(journal_host_envelope::Body::StateValue(WireStateValue {
			revision,
			value_json,
			present,
			props: None,
		})),
		props: None,
	}
}

const fn state_changed(revision: u64, value_json: Bytes) -> JournalHostEnvelope {
	JournalHostEnvelope {
		body:  Some(journal_host_envelope::Body::StateChanged(WireStateChanged {
			revision,
			value_json,
			props: None,
		})),
		props: None,
	}
}

fn epoch_millis() -> Result<u64, Str> {
	let millis = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map_err(display_error)?
		.as_millis();
	u64::try_from(millis).map_err(display_error)
}

fn unsupported<T>(operation: &'static str) -> Result<T, Str> {
	Err(Str::from(format!("Unsupported: {operation} is not available")))
}

fn artifact_error(error: ArtifactError) -> Str {
	match error {
		ArtifactError::IdempotencyConflict(key) => Str::from(format!("IdempotencyConflict: {key}")),
		error => display_error(error),
	}
}

fn display_error(error: impl std::fmt::Display) -> Str {
	Str::from(error.to_string())
}

#[cfg(test)]
mod tests {
	use omp_agent::{Journal, JournalAuthor, JournalRequestStamp};
	use omp_core::{ArtifactDigest, Principal, Provenance};
	use omp_proto::toolhost::v1::{AdoptArtifact, PinArtifact, QueryJournal, StatArtifact};
	use omp_storage::transcript::{Header, SessionId};

	use super::*;

	fn identity() -> JournalConnectionIdentity {
		JournalConnectionIdentity {
			principal:          Principal::new(Str::from("os:test"), Str::from("Test User")),
			provenance:         Provenance::new(
				Str::from("publisher"),
				Str::from("dev.example"),
				Str::from("1.0.0"),
				ArtifactDigest::new([7; 32]),
				Str::from("workspace"),
				Str::from("trusted"),
				1,
			),
			host_generation:    1,
			session_generation: 1,
		}
	}

	fn query() -> ExternalJournalRequest {
		ExternalJournalRequest::Query {
			request_id: 1,
			extension:  Str::from("dev.example"),
			query:      QueryJournal { session: "session".to_owned(), ..QueryJournal::default() },
		}
	}

	fn stamp(request_id: &str) -> JournalRequestStamp {
		JournalRequestStamp {
			request_id:         Str::from(request_id),
			idempotency_key:    Str::from(request_id),
			host_generation:    1,
			session_generation: 1,
		}
	}

	fn author() -> JournalAuthor {
		let identity = identity();
		JournalAuthor { principal: identity.principal, provenance: identity.provenance }
	}

	async fn call(
		actor: &ExternalJournalActor,
		request: ExternalJournalRequest,
	) -> Result<JournalHostEnvelope, Str> {
		let (reply, replies) = flume::unbounded();
		actor
			.sender()
			.send(ExternalJournalCall { request, identity: identity(), reply })
			.expect("send external request");
		replies.recv_async().await.expect("external response")
	}

	#[tokio::test]
	async fn current_session_query_fails_unbound_then_routes_to_agent_owner() {
		let state_dir = tempfile::tempdir().expect("state directory");
		let sessions = Arc::new(
			SessionIndex::open(state_dir.path().join("sessions.sqlite3")).expect("sessions index"),
		);
		let state = Arc::new(StateStore::open(state_dir.path().join("state")).expect("state store"));
		let blobs = BlobHost::open(state_dir.path().join("blobs")).expect("blob store");
		let actor = ExternalJournalActor::spawn(
			sessions,
			Some(state),
			blobs,
			Str::from("session"),
			Str::from("project"),
			Str::from("/project"),
		)
		.expect("external actor");

		let (reply, replies) = flume::unbounded();
		actor
			.sender()
			.send(ExternalJournalCall { request: query(), identity: identity(), reply })
			.expect("send unbound query");
		assert!(
			replies
				.recv_async()
				.await
				.expect("unbound response")
				.expect_err("unbound query must fail")
				.contains("not bound")
		);

		let journal_path = state_dir.path().join("session.jsonl");
		let mut journal = Journal::create(&journal_path, &Header {
			v:       4,
			id:      SessionId(Str::from("session")),
			created: 1,
			cwd:     state_dir.path().to_path_buf(),
		})
		.expect("journal");
		let (control, mailbox) = omp_agent::control::channel();
		actor.bind_agent(control).expect("bind agent owner");
		let owner = tokio::spawn(async move { while mailbox.handle_next(&mut journal).await {} });

		let (reply, replies) = flume::unbounded();
		actor
			.sender()
			.send(ExternalJournalCall { request: query(), identity: identity(), reply })
			.expect("send bound query");
		let response = replies
			.recv_async()
			.await
			.expect("bound response")
			.expect("bound query succeeds");
		assert!(matches!(
			response.body,
			Some(journal_host_envelope::Body::JournalRow(row)) if row.terminal
		));
		owner.abort();
	}

	#[tokio::test]
	async fn artifact_adoption_uses_authoritative_size_and_pin_is_durable() {
		let state_dir = tempfile::tempdir().expect("state directory");
		let sessions = Arc::new(
			SessionIndex::open(state_dir.path().join("sessions.sqlite3")).expect("sessions index"),
		);
		let state = Arc::new(StateStore::open(state_dir.path().join("state")).expect("state store"));
		let blobs = BlobHost::open(state_dir.path().join("blobs")).expect("blob store");
		let stored = blobs.put(b"authoritative bytes").expect("put blob");
		let digest = omp_core::hex::encode(&stored.hash).to_string();
		let actor = ExternalJournalActor::spawn(
			sessions,
			Some(state),
			blobs,
			Str::from("session"),
			Str::from("project"),
			Str::from("/project"),
		)
		.expect("external actor");

		let wrong_size = call(&actor, ExternalJournalRequest::AdoptArtifact {
			request_id: 2,
			stamp:      stamp("wrong-size"),
			author:     author(),
			request:    AdoptArtifact {
				source_url: format!("blob://b3/{digest}?size={}", stored.size + 1),
				lifetime: "session".to_owned(),
				..AdoptArtifact::default()
			},
		})
		.await
		.expect_err("peer size mismatch must fail");
		assert!(wrong_size.contains("length") || wrong_size.contains("size"));

		let missing = omp_core::hex::encode(&[9; 32]).to_string();
		let not_found = call(&actor, ExternalJournalRequest::AdoptArtifact {
			request_id: 3,
			stamp:      stamp("not-found"),
			author:     author(),
			request:    AdoptArtifact {
				source_url: format!("blob://b3/{missing}"),
				lifetime: "session".to_owned(),
				..AdoptArtifact::default()
			},
		})
		.await
		.expect_err("missing blob must fail");
		assert!(not_found.contains("not found"));

		let adopted = call(&actor, ExternalJournalRequest::AdoptArtifact {
			request_id: 4,
			stamp:      stamp("adopt"),
			author:     author(),
			request:    AdoptArtifact {
				source_url: format!("blob://b3/{digest}?size={}", stored.size),
				lifetime: "session".to_owned(),
				..AdoptArtifact::default()
			},
		})
		.await
		.expect("adopt blob");
		let Some(journal_host_envelope::Body::ArtifactRow(adopted)) = adopted.body else {
			panic!("adopt response must be an artifact row");
		};
		assert_eq!(adopted.size, stored.size);
		assert!(!adopted.pinned);

		let replayed = call(&actor, ExternalJournalRequest::AdoptArtifact {
			request_id: 41,
			stamp:      stamp("adopt"),
			author:     author(),
			request:    AdoptArtifact {
				source_url: format!("blob://b3/{digest}?size={}", stored.size),
				lifetime: "session".to_owned(),
				..AdoptArtifact::default()
			},
		})
		.await
		.expect("exact artifact replay");
		assert!(matches!(
			replayed.body,
			Some(journal_host_envelope::Body::ArtifactRow(row)) if row.url == adopted.url
		));
		let conflict = call(&actor, ExternalJournalRequest::AdoptArtifact {
			request_id: 42,
			stamp:      stamp("adopt"),
			author:     author(),
			request:    AdoptArtifact {
				source_url: format!("blob://b3/{digest}?size={}", stored.size),
				lifetime: "durable".to_owned(),
				..AdoptArtifact::default()
			},
		})
		.await
		.expect_err("changed artifact replay must conflict");
		assert!(conflict.starts_with("IdempotencyConflict:"));

		let pinned = call(&actor, ExternalJournalRequest::PinArtifact {
			request_id: 5,
			stamp:      stamp("pin"),
			author:     author(),
			request:    PinArtifact {
				url: adopted.url.clone(),
				lifetime: "durable".to_owned(),
				..PinArtifact::default()
			},
		})
		.await
		.expect("pin artifact");
		assert!(matches!(
			pinned.body,
			Some(journal_host_envelope::Body::ArtifactRow(row))
				if row.pinned && row.lifetime == "durable"
		));

		let durable = call(&actor, ExternalJournalRequest::StatArtifact {
			request_id: 6,
			request:    StatArtifact {
				url: format!("artifact://b3/{digest}"),
				..StatArtifact::default()
			},
		})
		.await
		.expect("stat durable digest");
		assert!(matches!(
			durable.body,
			Some(journal_host_envelope::Body::ArtifactRow(row)) if row.pinned
		));
	}
}
