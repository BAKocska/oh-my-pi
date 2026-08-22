//! Manifest-gated inter-extension services over CONTROL.

use std::{
	collections::{BTreeMap, BTreeSet},
	sync::{
		Arc,
		atomic::{AtomicU64, Ordering},
	},
};

use omp_core::{CowBytes, Duration, SparseMap, Str, sf};
use parking_lot::Mutex;
use thiserror::Error;

use crate::envd::worker::HostKey;

/// Exact service name and revision.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ServiceKey {
	/// Globally qualified service name.
	pub name: Str,
	/// Explicit compatibility revision.
	pub rev:  u32,
}

impl ServiceKey {
	/// Creates a service identity.
	pub fn new(name: impl Into<Str>, rev: u32) -> Self {
		Self { name: name.into(), rev }
	}
}

/// Provider declarations and consumer grants published from one manifest.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ServiceManifest {
	provides: BTreeSet<ServiceKey>,
	requires: BTreeSet<ServiceKey>,
}

impl ServiceManifest {
	/// Normalizes provider declarations and consumer requirements.
	pub fn new(
		provides: impl IntoIterator<Item = ServiceKey>,
		requires: impl IntoIterator<Item = ServiceKey>,
	) -> Self {
		Self { provides: provides.into_iter().collect(), requires: requires.into_iter().collect() }
	}

	/// Iterates over services this extension declares as a provider.
	pub fn provides(&self) -> impl DoubleEndedIterator<Item = &ServiceKey> + ExactSizeIterator {
		self.provides.iter()
	}

	/// Iterates over services this extension is granted permission to consume.
	pub fn requires(&self) -> impl DoubleEndedIterator<Item = &ServiceKey> + ExactSizeIterator {
		self.requires.iter()
	}
}

/// Exact difference between manifest services and frozen decorators.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ServiceDeclarationDrift {
	/// Manifest providers absent from the frozen registry.
	pub missing:    Box<[ServiceKey]>,
	/// Frozen providers absent from the manifest.
	pub unexpected: Box<[ServiceKey]>,
}

impl ServiceDeclarationDrift {
	fn between(expected: &BTreeSet<ServiceKey>, actual: &BTreeSet<ServiceKey>) -> Self {
		Self {
			missing:    expected.difference(actual).cloned().collect(),
			unexpected: actual.difference(expected).cloned().collect(),
		}
	}

	/// Returns whether the provider sets are equal.
	pub fn is_empty(&self) -> bool {
		self.missing.is_empty() && self.unexpected.is_empty()
	}
}

/// The only sanctioned transport for inter-extension RPC.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceTransport {
	/// A brokered request on the dedicated CONTROL descriptor.
	Control,
}

/// A resolved, manifest-authorized service connection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceRoute {
	/// Consumer whose manifest contains the requirement.
	pub caller:              HostKey,
	/// Active extension providing this exact revision.
	pub provider:            HostKey,
	/// Provider generation fenced when the route was resolved.
	pub provider_generation: u64,
	/// Resolved service identity.
	pub service:             ServiceKey,
	/// Transport fixed by the service contract.
	pub transport:           ServiceTransport,
}

/// Result of resolving a manifest-authorized service dependency.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServiceConnection {
	/// The provider is active and calls may be correlated immediately.
	Active(ServiceRoute),
	/// The admitted provider must complete its lazy lifecycle before retrying.
	ActivationRequired {
		/// Consumer whose manifest contains the requirement.
		caller:   HostKey,
		/// Admitted provider to activate.
		provider: HostKey,
		/// Exact service revision which triggered activation.
		service:  ServiceKey,
	},
}

/// Correlation and generation fields carried by one service Request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceRequestMeta {
	/// Caller's current child generation.
	pub host_generation:    u64,
	/// Session epoch shared by the caller and provider.
	pub session_generation: u64,
	/// Caller deadline propagated to the provider.
	pub deadline:           Duration,
}

/// Broker-assigned request correlation identifier.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ServiceCallId(
	/// Monotonic nonzero correlation value scoped to this broker.
	pub u64,
);

/// CONTROL request delivered to a provider child.
pub struct ServiceDispatch {
	/// Broker correlation identifier.
	pub id:      ServiceCallId,
	/// Authorized route.
	pub route:   ServiceRoute,
	/// Caller-scoped request metadata.
	pub meta:    ServiceRequestMeta,
	/// Public async method name.
	pub method:  Str,
	/// Encoded method arguments.
	pub payload: CowBytes<'static>,
}

/// Provider response routed to the correlated caller.
pub enum ServiceResponse {
	/// Successful encoded return value.
	Success(CowBytes<'static>),
	/// Provider-reported method failure.
	Failure(Str),
	/// Provider became unavailable before producing a result.
	Unavailable(Str),
}

/// Cancellation propagated when the caller drops its pending Request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceCancellation {
	/// Correlated call to cancel.
	pub id:       ServiceCallId,
	/// Provider child which owns the executing method.
	pub provider: HostKey,
}

/// Manifest, routing, or correlation failure.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ServiceError {
	/// A host published more than one manifest.
	#[error("service manifest for {0:?} is already published")]
	DuplicateManifest(HostKey),
	/// Provider activation named a host whose manifest was never published.
	#[error("service manifest for {0:?} is not published")]
	UnknownManifest(HostKey),
	/// Two admitted extensions provide the same exact service revision.
	#[error("service {service:?} is provided by both {first:?} and {second:?}")]
	DuplicateProvider {
		/// Conflicting service identity.
		service: ServiceKey,
		/// Existing provider.
		first:   HostKey,
		/// New provider.
		second:  HostKey,
	},
	/// Frozen `@omp.service` decorators drifted from the manifest.
	#[error("frozen service declarations differ from the manifest")]
	DeclarationDrift(ServiceDeclarationDrift),
	/// The consumer has no manifest grant for the requested service.
	#[error("extension {caller:?} has no declared grant for service {service:?}")]
	Capability {
		/// Consumer attempting to connect.
		caller:  HostKey,
		/// Undeclared service dependency.
		service: ServiceKey,
	},
	/// No admitted extension provides the exact revision.
	#[error("no admitted provider for service {0:?}")]
	Unavailable(ServiceKey),
	/// A provider route was resolved before that provider's current generation.
	#[error("service route for {0:?} belongs to a stale provider generation")]
	StaleRoute(ServiceKey),
	/// A caller frame belonged to an old child or session generation.
	#[error(
		"stale service generation for {host:?}: expected host {expected_host} session \
		 {expected_session}, got host {actual_host} session {actual_session}"
	)]
	StaleGeneration {
		/// Caller host identity.
		host:             HostKey,
		/// Current caller host generation.
		expected_host:    u64,
		/// Request caller host generation.
		actual_host:      u64,
		/// Broker session generation.
		expected_session: u64,
		/// Request session generation.
		actual_session:   u64,
	},
	/// A response did not match a pending correlation or its provider
	/// generation.
	#[error("stale or unknown service response correlation {0}")]
	StaleCorrelation(u64),
}

const _: () = assert!(std::mem::size_of::<ServiceError>() <= 128, "ServiceError must stay compact");

/// Failure observed while awaiting a service method.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ServiceCallError {
	/// The provider returned an application error.
	#[error("service method failed: {0}")]
	Provider(Str),
	/// The provider disconnected or was replaced before responding.
	#[error("service provider became unavailable: {0}")]
	Unavailable(Str),
}

struct PendingRecord {
	provider:        HostKey,
	host_generation: u64,
	response:        flume::Sender<ServiceResponse>,
}

/// Awaitable half of one correlated service Request.
///
/// Dropping this value before a response removes the correlation immediately
/// and emits a CONTROL cancellation. No journal entry or agent message is used
/// as a request, response, wake-up, or fallback transport.
pub struct PendingServiceCall {
	id:            ServiceCallId,
	provider:      HostKey,
	pending:       Arc<Mutex<SparseMap<u64, PendingRecord>>>,
	cancellations: flume::Sender<ServiceCancellation>,
	response:      flume::Receiver<ServiceResponse>,
	armed:         bool,
}

impl PendingServiceCall {
	/// Waits for the provider response while preserving caller cancellation.
	pub async fn response(mut self) -> Result<CowBytes<'static>, ServiceCallError> {
		let response = self.response.recv_async().await;
		self.armed = false;
		match response {
			Ok(ServiceResponse::Success(payload)) => Ok(payload),
			Ok(ServiceResponse::Failure(message)) => Err(ServiceCallError::Provider(message)),
			Ok(ServiceResponse::Unavailable(message)) => Err(ServiceCallError::Unavailable(message)),
			Err(_) => Err(ServiceCallError::Unavailable(sf!("provider response channel closed"))),
		}
	}
}

impl Drop for PendingServiceCall {
	fn drop(&mut self) {
		if !self.armed {
			return;
		}
		if self.pending.lock().remove(self.id.0).is_some() {
			let _ = self
				.cancellations
				.send(ServiceCancellation { id: self.id, provider: self.provider.clone() });
		}
	}
}

#[derive(Clone)]
struct ActiveProvider {
	host:       HostKey,
	generation: u64,
}

/// Core-side manifest registry and CONTROL service request broker.
pub struct ServiceBroker {
	session_generation: u64,
	manifests:          BTreeMap<HostKey, ServiceManifest>,
	admitted:           BTreeMap<ServiceKey, HostKey>,
	providers:          BTreeMap<ServiceKey, ActiveProvider>,
	active_generations: BTreeMap<HostKey, u64>,
	next_id:            AtomicU64,
	pending:            Arc<Mutex<SparseMap<u64, PendingRecord>>>,
	cancellations_tx:   flume::Sender<ServiceCancellation>,
	cancellations_rx:   flume::Receiver<ServiceCancellation>,
}

impl ServiceBroker {
	/// Creates an empty broker fenced to one session epoch. This is inert until
	/// manifests are published and providers activate.
	pub fn new(session_generation: u64) -> Self {
		let (cancellations_tx, cancellations_rx) = flume::unbounded();
		Self {
			session_generation,
			manifests: BTreeMap::new(),
			admitted: BTreeMap::new(),
			providers: BTreeMap::new(),
			active_generations: BTreeMap::new(),
			next_id: AtomicU64::new(1),
			pending: Arc::new(Mutex::new(SparseMap::new())),
			cancellations_tx,
			cancellations_rx,
		}
	}

	fn allocate_call_id(&self) -> ServiceCallId {
		let id = self
			.next_id
			.try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
				Some(if current == u64::MAX { 1 } else { current + 1 })
			})
			.expect("service call id update closure is infallible");
		ServiceCallId(id)
	}

	/// Publishes static service declarations and grants without starting a
	/// child.
	pub fn publish_manifest(
		&mut self,
		host: HostKey,
		manifest: ServiceManifest,
	) -> Result<(), ServiceError> {
		if self.manifests.contains_key(&host) {
			return Err(ServiceError::DuplicateManifest(host));
		}
		for service in manifest.provides() {
			if let Some(first) = self.admitted.get(service) {
				return Err(ServiceError::DuplicateProvider {
					service: service.clone(),
					first:   first.clone(),
					second:  host,
				});
			}
		}
		for service in manifest.provides() {
			self.admitted.insert(service.clone(), host.clone());
		}
		self.manifests.insert(host, manifest);
		Ok(())
	}

	/// Verifies a frozen provider registry and makes its exact revisions
	/// routable.
	pub fn activate_provider(
		&mut self,
		host: &HostKey,
		host_generation: u64,
		declared: impl IntoIterator<Item = ServiceKey>,
	) -> Result<(), ServiceError> {
		let actual = declared.into_iter().collect::<BTreeSet<_>>();
		let expected = self
			.manifests
			.get(host)
			.ok_or_else(|| ServiceError::UnknownManifest(host.clone()))?
			.provides
			.clone();
		if self
			.active_generations
			.get(host)
			.is_some_and(|generation| *generation != host_generation)
		{
			self.deactivate_provider(host, "provider generation replaced");
		}
		let drift = ServiceDeclarationDrift::between(&expected, &actual);
		if !drift.is_empty() {
			return Err(ServiceError::DeclarationDrift(drift));
		}
		for service in &actual {
			if let Some(first) = self.providers.get(service)
				&& first.host != *host
			{
				return Err(ServiceError::DuplicateProvider {
					service: service.clone(),
					first:   first.host.clone(),
					second:  host.clone(),
				});
			}
		}
		for service in actual {
			self.providers.insert(service, ActiveProvider {
				host:       host.clone(),
				generation: host_generation,
			});
		}
		self
			.active_generations
			.insert(host.clone(), host_generation);
		Ok(())
	}

	/// Resolves a connection only when the consumer manifest grants it.
	///
	/// An admitted but inactive provider is returned as
	/// [`ServiceConnection::ActivationRequired`], allowing the supervisor to
	/// run its lazy lifecycle and retry without ambient service discovery.
	pub fn connect(
		&self,
		caller: &HostKey,
		service: ServiceKey,
	) -> Result<ServiceConnection, ServiceError> {
		let granted = self
			.manifests
			.get(caller)
			.is_some_and(|manifest| manifest.requires.contains(&service));
		if !granted {
			return Err(ServiceError::Capability { caller: caller.clone(), service });
		}
		if let Some(provider) = self.providers.get(&service) {
			return Ok(ServiceConnection::Active(ServiceRoute {
				caller: caller.clone(),
				provider: provider.host.clone(),
				provider_generation: provider.generation,
				service,
				transport: ServiceTransport::Control,
			}));
		}
		let provider = self
			.admitted
			.get(&service)
			.cloned()
			.ok_or_else(|| ServiceError::Unavailable(service.clone()))?;
		Ok(ServiceConnection::ActivationRequired { caller: caller.clone(), provider, service })
	}

	/// Begins one method call and installs its response correlation before the
	/// dispatch can be written to CONTROL.
	///
	/// # Errors
	/// Returns [`ServiceError::StaleRoute`] when a restart replaced the provider
	/// after `connect` resolved it.
	pub fn begin_call(
		&self,
		route: ServiceRoute,
		meta: ServiceRequestMeta,
		method: impl Into<Str>,
		payload: CowBytes<'static>,
	) -> Result<(ServiceDispatch, PendingServiceCall), ServiceError> {
		if !self
			.manifests
			.get(&route.caller)
			.is_some_and(|manifest| manifest.requires.contains(&route.service))
		{
			return Err(ServiceError::Capability { caller: route.caller, service: route.service });
		}
		let active_host = self.active_generations.get(&route.caller).copied();
		let expected_host = active_host.unwrap_or(0);
		if active_host.is_none()
			|| meta.session_generation != self.session_generation
			|| meta.host_generation != expected_host
		{
			return Err(ServiceError::StaleGeneration {
				host: route.caller,
				expected_host,
				actual_host: meta.host_generation,
				expected_session: self.session_generation,
				actual_session: meta.session_generation,
			});
		}
		let current = self.providers.get(&route.service);
		if !current.is_some_and(|provider| {
			provider.host == route.provider && provider.generation == route.provider_generation
		}) {
			return Err(ServiceError::StaleRoute(route.service));
		}
		let id = self.allocate_call_id();
		let (response_tx, response_rx) = flume::bounded(1);
		self.pending.lock().insert(id.0, PendingRecord {
			provider:        route.provider.clone(),
			host_generation: route.provider_generation,
			response:        response_tx,
		});
		let pending = PendingServiceCall {
			id,
			provider: route.provider.clone(),
			pending: Arc::clone(&self.pending),
			cancellations: self.cancellations_tx.clone(),
			response: response_rx,
			armed: true,
		};
		let dispatch = ServiceDispatch { id, route, meta, method: method.into(), payload };
		Ok((dispatch, pending))
	}

	/// Completes one correlated call after validating provider and generation.
	pub fn complete(
		&self,
		provider: &HostKey,
		host_generation: u64,
		id: ServiceCallId,
		response: ServiceResponse,
	) -> Result<(), ServiceError> {
		let mut pending = self.pending.lock();
		let Some(record) = pending.get(id.0) else {
			return Err(ServiceError::StaleCorrelation(id.0));
		};
		if &record.provider != provider || record.host_generation != host_generation {
			return Err(ServiceError::StaleCorrelation(id.0));
		}
		let record = pending
			.remove(id.0)
			.expect("validated pending correlation exists");
		drop(pending);
		let _ = record.response.send(response);
		Ok(())
	}

	/// Removes one provider's routes and fails only its in-flight method calls.
	pub fn deactivate_provider(&mut self, provider: &HostKey, reason: impl Into<Str>) {
		self.active_generations.remove(provider);
		self.providers.retain(|_, active| &active.host != provider);
		let reason = reason.into();
		self.pending.lock().retain(|_, record| {
			if &record.provider == provider {
				let _ = record
					.response
					.send(ServiceResponse::Unavailable(reason.clone()));
				false
			} else {
				true
			}
		});
	}

	/// Receives the next caller cancellation for forwarding on CONTROL.
	pub async fn cancellation(&self) -> Option<ServiceCancellation> {
		self.cancellations_rx.recv_async().await.ok()
	}

	/// Returns the number of in-flight correlated calls.
	pub fn pending_len(&self) -> usize {
		self.pending.lock().len()
	}
}
