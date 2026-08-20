//! Native, free-threaded Python value and Environment bindings.

use std::{
	cmp::Ordering,
	collections::BTreeMap,
	hash::{Hash, Hasher},
	str::FromStr,
	sync::{Arc, LazyLock},
};

use omp_core::{
	ActivateReason, AgentUrl, ArtifactUrl, ClientPath, Duration, DurationUnit, EnvPath, HistoryUrl,
	InvocationPhase, LifecyclePhase, Principal, RestartReason, Secret, Str, WorkspaceUri,
};
use omp_env::WorkerEnvClient;
use omp_storage::state::StateScope;
use omp_tool::{Authority, CostClass, Durability, OperationSpec};
use parking_lot::RwLock;
use pyo3::{
	Bound, Py, PyAny, PyErr, PyResult, Python, create_exception,
	exceptions::{PyException, PyRuntimeError, PyTypeError, PyValueError},
	pyclass, pyfunction, pymethods, pymodule,
	types::{
		PyAnyMethods, PyBytes, PyBytesMethods, PyDict, PyList, PyListMethods, PyModule,
		PyModuleMethods, PyTuple, PyTupleMethods,
	},
	wrap_pyfunction,
};

create_exception!(_omp, OmpError, PyException, "Base class for omp runtime failures.");
create_exception!(_omp, ManifestError, OmpError, "An extension manifest is invalid.");
create_exception!(
	_omp,
	ApiLevelError,
	ManifestError,
	"The requested omp API level is unsupported."
);
create_exception!(_omp, DeclarationLimit, ManifestError, "The declaration limit was exceeded.");
create_exception!(_omp, CapabilityError, OmpError, "A required capability was not granted.");
create_exception!(_omp, TrustError, CapabilityError, "The active trust tier is insufficient.");
create_exception!(
	_omp,
	DuplicateRegistration,
	OmpError,
	"A declaration collides with an incumbent."
);
create_exception!(_omp, DeclarationSealed, OmpError, "A declaration ran after the registry froze.");
create_exception!(
	_omp,
	EffectsNotAuthorized,
	OmpError,
	"The invocation has not authorized effects."
);
create_exception!(_omp, DeadlineExceeded, OmpError, "The active invocation deadline elapsed.");
create_exception!(_omp, HostDisconnected, OmpError, "The host CONTROL channel disconnected.");
create_exception!(_omp, FrameTooLarge, OmpError, "An encoded extension frame exceeds its bound.");
create_exception!(_omp, EnvUnavailable, OmpError, "No Environment exists at this placement.");
create_exception!(_omp, PlacementError, OmpError, "A resource is unavailable at this placement.");
create_exception!(
	_omp,
	StaleGeneration,
	OmpError,
	"A request carries a retired host or session generation."
);

#[derive(Debug, Default)]
struct ResourceState {
	quotas:  BTreeMap<Str, QuotaStatusValue>,
	dropped: BTreeMap<Str, u64>,
}

#[derive(Clone, Copy, Debug)]
struct QuotaStatusValue {
	limit:  u64,
	used:   u64,
	window: Option<Duration>,
}

#[derive(Clone, Debug)]
struct SchemeEntry {
	member:      Str,
	readable:    bool,
	mintable:    bool,
	selectors:   bool,
	description: Str,
}

#[derive(Debug, Default)]
struct SchemeSnapshot {
	device_hash: [u8; 32],
	entries:     Box<[SchemeEntry]>,
}

#[derive(Debug, Default)]
struct PythonRuntime {
	client:    RwLock<Option<WorkerEnvClient>>,
	root_uri:  RwLock<Option<Str>>,
	resources: RwLock<ResourceState>,
	schemes:   RwLock<SchemeSnapshot>,
}

static RUNTIME: LazyLock<PythonRuntime> = LazyLock::new(PythonRuntime::default);
static ASYNC_RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
	tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.thread_name("omp-py-data")
		.build()
		.expect("omp Python DATA runtime must initialize")
});

/// Installs an invocation-scoped Environment client after lazy DATA connection.
///
/// Worker startup and Python import deliberately do not call this function.
/// The first Environment operation causes the host to connect, complete
/// `ClientHello`, construct the scoped client, and then install it here.
pub fn install_environment_client(client: WorkerEnvClient, root_uri: impl Into<Str>) {
	*RUNTIME.client.write() = Some(client);
	*RUNTIME.root_uri.write() = Some(root_uri.into());
}

/// Replaces the live URL resolver snapshot used by `omp.urls.schemes()`.
pub fn set_scheme_snapshot<I, M, D>(device_hash: [u8; 32], entries: I)
where
	I: IntoIterator<Item = (M, bool, bool, bool, D)>,
	M: Into<Str>,
	D: Into<Str>,
{
	let entries = entries
		.into_iter()
		.map(|(member, readable, mintable, selectors, description)| SchemeEntry {
			member: member.into(),
			readable,
			mintable,
			selectors,
			description: description.into(),
		})
		.collect::<Vec<_>>()
		.into_boxed_slice();
	*RUNTIME.schemes.write() = SchemeSnapshot { device_hash, entries };
}

/// Updates the cached Environment root used for pure typed-path URI resolution.
///
/// The host calls this only after a successful DATA handshake. It performs no
/// Python work and does not open a socket.
pub fn set_environment_root(root_uri: impl Into<Str>) {
	*RUNTIME.root_uri.write() = Some(root_uri.into());
}

/// Replaces the locally cached quota receipt pushed by the host.
pub fn set_resource_receipt<I, D>(quotas: I, dropped: D)
where
	I: IntoIterator<Item = (Str, u64, u64, Option<Duration>)>,
	D: IntoIterator<Item = (Str, u64)>,
{
	let mut state = RUNTIME.resources.write();
	state.quotas.clear();
	state.dropped.clear();
	state.quotas.extend(
		quotas
			.into_iter()
			.map(|(name, limit, used, window)| (name, QuotaStatusValue { limit, used, window })),
	);
	state.dropped.extend(dropped);
}

fn value_error(error: impl std::fmt::Display) -> PyErr {
	PyValueError::new_err(error.to_string())
}

/// Immutable Python duration retaining its explicit source unit.
#[pyclass(name = "Duration", frozen, module = "_omp", from_py_object)]
#[derive(Clone, Debug)]
struct PyDuration(Duration);

#[pymethods]
impl PyDuration {
	#[new]
	#[pyo3(signature = (value = None, *, seconds = None))]
	fn new(value: Option<&Bound<'_, PyAny>>, seconds: Option<f64>) -> PyResult<Self> {
		match (value, seconds) {
			(Some(value), None) => {
				let text = value.extract::<&str>().map_err(|_| {
					PyTypeError::new_err("Duration positional value must be a unit-suffixed string")
				})?;
				Duration::from_str(text).map(Self).map_err(value_error)
			},
			(None, Some(seconds)) if seconds.is_finite() && seconds >= 0.0 => {
				let nanos = seconds * 1_000_000_000.0;
				if nanos > u64::MAX as f64 {
					return Err(PyValueError::new_err("duration is too large"));
				}
				let rounded = nanos.round();
				if (nanos - rounded).abs() > f64::EPSILON * nanos.abs().max(1.0) {
					return Err(PyValueError::new_err(
						"seconds cannot be represented as whole nanoseconds",
					));
				}
				Ok(Self(Duration::new(rounded as u64, DurationUnit::Nanoseconds)))
			},
			(None, Some(_)) => Err(PyValueError::new_err("seconds must be finite and non-negative")),
			(Some(_), Some(_)) => {
				Err(PyTypeError::new_err("pass either a string or seconds=, not both"))
			},
			(None, None) => Err(PyTypeError::new_err("Duration requires a string or seconds=")),
		}
	}

	#[getter]
	fn seconds(&self) -> PyResult<f64> {
		Ok(self.0.to_std().map_err(value_error)?.as_secs_f64())
	}

	#[getter]
	const fn value(&self) -> u64 {
		self.0.value()
	}

	#[getter]
	fn unit(&self) -> String {
		self.0.unit().to_string()
	}

	fn __str__(&self) -> String {
		self.0.to_string()
	}

	fn __repr__(&self) -> String {
		format!("Duration({:?})", self.0.to_string())
	}

	fn __hash__(&self) -> isize {
		let mut hasher = std::collections::hash_map::DefaultHasher::new();
		self.0.hash(&mut hasher);
		hasher.finish() as isize
	}

	fn __richcmp__(&self, other: &Self, op: pyo3::basic::CompareOp) -> bool {
		match op {
			pyo3::basic::CompareOp::Eq => self.0 == other.0,
			pyo3::basic::CompareOp::Ne => self.0 != other.0,
			pyo3::basic::CompareOp::Lt => self.0 < other.0,
			pyo3::basic::CompareOp::Le => self.0 <= other.0,
			pyo3::basic::CompareOp::Gt => self.0 > other.0,
			pyo3::basic::CompareOp::Ge => self.0 >= other.0,
		}
	}

	fn __sub__(&self, other: &Self) -> PyResult<Self> {
		let left = self.0.to_std().map_err(value_error)?;
		let right = other.0.to_std().map_err(value_error)?;
		let difference = left.checked_sub(right).ok_or_else(|| {
			PyValueError::new_err("Duration subtraction cannot produce a negative duration")
		})?;
		Duration::from_std(difference, DurationUnit::Nanoseconds)
			.map(Self)
			.map_err(value_error)
	}
}

/// Creates the immutable Python view of a configured core duration.
pub fn bind_duration(py: Python<'_>, duration: Duration) -> PyResult<Py<PyAny>> {
	Ok(Py::new(py, PyDuration(duration))?.into_any())
}

/// Opaque Python secret whose representation never reveals its bytes.
///
/// Raw bytes are available only from the temporary value yielded by
/// [`Self::use_`]; callers must use that context manager rather than logging
/// this object.
#[pyclass(name = "Secret", frozen, module = "_omp")]
#[derive(Debug)]
struct PySecret(Arc<Secret>);

#[pymethods]
impl PySecret {
	#[new]
	fn new(bytes: &Bound<'_, PyBytes>) -> Self {
		Self(Arc::new(Secret::from(bytes.as_bytes().to_vec())))
	}

	/// Returns a context manager which temporarily yields the secret bytes.
	#[pyo3(name = "use")]
	fn use_(&self) -> PySecretUse {
		PySecretUse(Arc::clone(&self.0))
	}

	fn __str__(&self) -> &'static str {
		"<redacted>"
	}

	fn __repr__(&self) -> &'static str {
		"Secret(<redacted>)"
	}

	fn __format__(&self, _format_spec: &str) -> &'static str {
		"<redacted>"
	}
}

/// Short-lived Python context manager for a [`PySecret`] exposure.
#[pyclass(frozen, module = "_omp")]
#[derive(Debug)]
struct PySecretUse(Arc<Secret>);

#[pymethods]
impl PySecretUse {
	fn __enter__<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
		self.0.expose(|bytes| PyBytes::new(py, bytes))
	}

	fn __exit__(
		&self,
		_exc_type: Option<&Bound<'_, PyAny>>,
		_exc_value: Option<&Bound<'_, PyAny>>,
		_traceback: Option<&Bound<'_, PyAny>>,
	) -> bool {
		false
	}
}

/// Canonical ordered Python invocation phase.
#[pyclass(name = "InvocationPhase", frozen, module = "_omp", from_py_object)]
#[derive(Clone, Debug)]
struct PyInvocationPhase(InvocationPhase);

#[pymethods]
impl PyInvocationPhase {
	#[classattr]
	const ADMISSION: Self = Self(InvocationPhase::Admission);
	#[classattr]
	const ADMITTED: Self = Self(InvocationPhase::Admitted);
	#[classattr]
	const ARGS_FINALIZED: Self = Self(InvocationPhase::ArgsFinalized);
	#[classattr]
	const ASSISTANT_ITEM_COMMITTED: Self = Self(InvocationPhase::AssistantItemCommitted);
	#[classattr]
	const EFFECTS_AUTHORIZED: Self = Self(InvocationPhase::EffectsAuthorized);
	#[classattr]
	const OPEN: Self = Self(InvocationPhase::Open);
	#[classattr]
	const SETTLED: Self = Self(InvocationPhase::Settled);

	#[getter]
	fn value(&self) -> &'static str {
		self.0.into()
	}

	#[getter]
	const fn ordinal(&self) -> u8 {
		self.0.ordinal()
	}

	fn __str__(&self) -> &'static str {
		self.0.into()
	}

	fn __repr__(&self) -> String {
		format!("InvocationPhase.{}", <&str>::from(self.0))
	}

	const fn __hash__(&self) -> isize {
		self.0 as isize
	}

	fn __richcmp__(&self, other: &Self, op: pyo3::basic::CompareOp) -> bool {
		compare(self.0.cmp(&other.0), op)
	}
}

/// Canonical ordered Python extension lifecycle phase.
#[pyclass(name = "LifecyclePhase", frozen, module = "_omp", from_py_object)]
#[derive(Clone, Debug)]
struct PyLifecyclePhase(LifecyclePhase);

#[pymethods]
impl PyLifecyclePhase {
	#[classattr]
	const ACTIVE: Self = Self(LifecyclePhase::Active);
	#[classattr]
	const DECLARED: Self = Self(LifecyclePhase::Declared);
	#[classattr]
	const DEGRADED: Self = Self(LifecyclePhase::Degraded);
	#[classattr]
	const FROZEN: Self = Self(LifecyclePhase::Frozen);
	#[classattr]
	const VERIFIED: Self = Self(LifecyclePhase::Verified);

	#[getter]
	fn value(&self) -> &'static str {
		self.0.into()
	}

	#[getter]
	const fn ordinal(&self) -> u8 {
		self.0.ordinal()
	}

	fn __str__(&self) -> &'static str {
		self.0.into()
	}

	fn __repr__(&self) -> String {
		format!("LifecyclePhase.{}", <&str>::from(self.0))
	}

	const fn __hash__(&self) -> isize {
		self.0 as isize
	}

	fn __richcmp__(&self, other: &Self, op: pyo3::basic::CompareOp) -> bool {
		compare(self.0.cmp(&other.0), op)
	}
}

fn compare(ordering: Ordering, op: pyo3::basic::CompareOp) -> bool {
	match op {
		pyo3::basic::CompareOp::Eq => ordering == Ordering::Equal,
		pyo3::basic::CompareOp::Ne => ordering != Ordering::Equal,
		pyo3::basic::CompareOp::Lt => ordering == Ordering::Less,
		pyo3::basic::CompareOp::Le => ordering != Ordering::Greater,
		pyo3::basic::CompareOp::Gt => ordering == Ordering::Greater,
		pyo3::basic::CompareOp::Ge => ordering != Ordering::Less,
	}
}

macro_rules! string_enum {
	($rust:ident, $python:literal, $inner:ty, [$($member:ident => $variant:path),+ $(,)?]) => {
		#[doc = concat!("Canonical Python ", $python, " vocabulary.")]
		#[pyclass(name = $python, frozen, module = "_omp", from_py_object)]
		#[derive(Clone, Debug)]
		struct $rust($inner);

		#[pymethods]
		impl $rust {
			$(#[classattr]
			const $member: Self = Self($variant);)+

			#[getter]
			fn value(&self) -> String { self.0.to_string() }

			fn __str__(&self) -> String { self.0.to_string() }

			fn __repr__(&self) -> String {
				format!(concat!($python, ".{}"), self.0.to_string().to_ascii_uppercase())
			}

			const fn __hash__(&self) -> isize { self.0 as isize }

			fn __richcmp__(&self, other: &Self, op: pyo3::basic::CompareOp) -> bool {
				compare((self.0 as u8).cmp(&(other.0 as u8)), op)
			}
		}
	};
}
string_enum!(PyActivateReason, "ActivateReason", ActivateReason, [
	FIRST_REACH => ActivateReason::FirstReach,
	RESTART => ActivateReason::Restart,
	HOT_RELOAD => ActivateReason::HotReload,
]);
string_enum!(PyRestartReason, "RestartReason", RestartReason, [
	CRASH => RestartReason::Crash,
	HOT_RELOAD => RestartReason::HotReload,
	CANCEL_ESCALATION => RestartReason::CancelEscalation,
	PROTOCOL_ERROR => RestartReason::ProtocolError,
	OOM => RestartReason::Oom,
	HEALTH_TIMEOUT => RestartReason::HealthTimeout,
]);
string_enum!(PyStateScope, "StateScope", StateScope, [
	SESSION => StateScope::Session,
	PROJECT => StateScope::Project,
	USER => StateScope::User,
	ORGANIZATION => StateScope::Organization,
]);

string_enum!(PyDurability, "Durability", Durability, [
	EPHEMERAL => Durability::Ephemeral,
	DURABLE => Durability::Durable,
]);
string_enum!(PyCostClass, "CostClass", CostClass, [
	NONE => CostClass::None,
	METERED => CostClass::Metered,
	PAID => CostClass::Paid,
]);
string_enum!(PyAuthority, "Authority", Authority, [
	CORE => Authority::Core,
	ENVIRONMENT => Authority::Environment,
]);

/// Generated phase, durability, cost, and authority metadata.
#[pyclass(name = "OperationSpec", frozen, module = "_omp", from_py_object)]
#[derive(Clone, Debug)]
struct PyOperationSpec(OperationSpec);

#[pymethods]
impl PyOperationSpec {
	#[getter]
	const fn minimum_phase(&self) -> PyInvocationPhase {
		PyInvocationPhase(self.0.minimum_phase)
	}

	#[getter]
	const fn durability(&self) -> PyDurability {
		PyDurability(self.0.durability)
	}

	#[getter]
	const fn cost(&self) -> PyCostClass {
		PyCostClass(self.0.cost)
	}

	#[getter]
	const fn authority(&self) -> PyAuthority {
		PyAuthority(self.0.authority)
	}

	fn __repr__(&self) -> String {
		format!(
			"OperationSpec(minimum_phase={}, durability={}, cost={}, authority={})",
			<&str>::from(self.0.minimum_phase),
			<&str>::from(self.0.durability),
			<&str>::from(self.0.cost),
			<&str>::from(self.0.authority),
		)
	}

	fn __hash__(&self) -> isize {
		let mut hasher = std::collections::hash_map::DefaultHasher::new();
		self.0.hash(&mut hasher);
		hasher.finish() as isize
	}

	fn __richcmp__(&self, other: &Self, op: pyo3::basic::CompareOp) -> bool {
		match op {
			pyo3::basic::CompareOp::Eq => self.0 == other.0,
			pyo3::basic::CompareOp::Ne => self.0 != other.0,
			_ => false,
		}
	}
}

#[pyfunction]
fn operation_spec(symbol: &Bound<'_, PyAny>) -> PyResult<Option<PyOperationSpec>> {
	let name = if let Ok(name) = symbol.extract::<String>() {
		name
	} else if let Ok(name) = symbol.getattr("__omp_symbol__") {
		name.extract::<String>()?
	} else {
		return Err(PyTypeError::new_err(
			"operation_spec expects a qualified symbol name or an omp public symbol",
		));
	};
	Ok(omp_tool::operation_spec(&name)
		.copied()
		.map(PyOperationSpec))
}

macro_rules! typed_location {
	($rust:ident, $python:literal, $inner:ty) => {
		#[doc = concat!("Typed Python ", $python, " location value.")]
		#[pyclass(name = $python, frozen, module = "_omp", from_py_object)]
		#[derive(Clone, Debug)]
		struct $rust($inner);

		#[pymethods]
		impl $rust {
			#[new]
			fn new(value: &str) -> PyResult<Self> {
				<$inner>::new(Str::new(value))
					.map(Self)
					.map_err(value_error)
			}

			#[getter]
			fn uri(&self) -> &str {
				self.0.as_str()
			}

			fn __str__(&self) -> &str {
				self.0.as_str()
			}

			fn __repr__(&self) -> String {
				format!(concat!($python, "({:?})"), self.0.as_str())
			}

			fn __hash__(&self) -> isize {
				let mut hasher = std::collections::hash_map::DefaultHasher::new();
				self.0.hash(&mut hasher);
				hasher.finish() as isize
			}

			fn __richcmp__(&self, other: &Self, op: pyo3::basic::CompareOp) -> bool {
				match op {
					pyo3::basic::CompareOp::Eq => self.0 == other.0,
					pyo3::basic::CompareOp::Ne => self.0 != other.0,
					_ => false,
				}
			}
		}
	};
}

macro_rules! typed_url_location {
	($rust:ident, $python:literal, $inner:ty) => {
		#[doc = concat!("Typed Python ", $python, " URL value.")]
		#[pyclass(name = $python, frozen, module = "_omp", from_py_object)]
		#[derive(Clone, Debug)]
		struct $rust($inner);

		#[pymethods]
		impl $rust {
			#[new]
			fn new(value: &str) -> PyResult<Self> {
				<$inner>::new(Str::new(value))
					.map(Self)
					.map_err(value_error)
			}

			#[getter]
			fn uri(&self) -> &str {
				self.0.as_str()
			}

			#[getter]
			fn resource(&self) -> &str {
				self.0.resource()
			}

			#[getter]
			fn selector(&self) -> Option<&str> {
				self.0.selector()
			}

			fn with_selector(&self, py: Python<'_>, selector: &str) -> PyResult<Self> {
				py.import("omp.urls")?
					.getattr("parse_selector")?
					.call1((selector,))?;
				let base_len =
					self.0.as_str().len() - self.0.selector().map_or(0, |value| value.len() + 1);
				let mut value = String::with_capacity(base_len + selector.len() + 1);
				value.push_str(&self.0.as_str()[..base_len]);
				value.push(':');
				value.push_str(selector);
				<$inner>::new(Str::new(value))
					.map(Self)
					.map_err(value_error)
			}

			fn read(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
				Ok(py
					.import("omp")?
					.getattr("_read_url")?
					.call1((self.clone(),))?
					.unbind())
			}

			fn __str__(&self) -> &str {
				self.0.as_str()
			}

			fn __repr__(&self) -> String {
				format!(concat!($python, "({:?})"), self.0.as_str())
			}

			fn __hash__(&self) -> isize {
				let mut hasher = std::collections::hash_map::DefaultHasher::new();
				self.0.hash(&mut hasher);
				hasher.finish() as isize
			}

			fn __richcmp__(&self, other: &Self, op: pyo3::basic::CompareOp) -> bool {
				match op {
					pyo3::basic::CompareOp::Eq => self.0 == other.0,
					pyo3::basic::CompareOp::Ne => self.0 != other.0,
					_ => false,
				}
			}
		}
	};
}

#[pyfunction]
fn _phase_legality_matrix(py: Python<'_>) -> PyResult<Py<PyAny>> {
	let matrix = PyDict::new(py);
	for row in omp_tool::phase_legality_matrix() {
		matrix.set_item(row.public_name, row.legal)?;
	}
	let proxy = py.import("types")?.getattr("MappingProxyType")?;
	Ok(proxy.call1((matrix,))?.unbind())
}

#[pyfunction]
fn _runtime_metadata(py: Python<'_>) -> PyResult<Py<PyAny>> {
	let metadata = PyDict::new(py);
	for symbol in omp_tool::runtime_symbols() {
		let row = PyDict::new(py);
		row.set_item("owner", symbol.owner)?;
		row.set_item("signature", symbol.signature)?;
		row.set_item("callback_abi", <&str>::from(symbol.callback_abi))?;
		row.set_item("operation", Py::new(py, PyOperationSpec(symbol.operation))?)?;
		row.set_item(
			"timeout",
			symbol
				.timeout
				.map(|timeout| Py::new(py, PyDuration(timeout)))
				.transpose()?,
		)?;
		row.set_item("examples", symbol.examples)?;
		metadata.set_item(symbol.public_name, row)?;
	}
	let proxy = py.import("types")?.getattr("MappingProxyType")?;
	Ok(proxy.call1((metadata,))?.unbind())
}

typed_url_location!(PyArtifactUrl, "ArtifactUrl", ArtifactUrl);
typed_url_location!(PyHistoryUrl, "HistoryUrl", HistoryUrl);
typed_url_location!(PyAgentUrl, "AgentUrl", AgentUrl);
typed_location!(PyWorkspaceUri, "WorkspaceUri", WorkspaceUri);

#[pyclass(name = "EnvPath", frozen, module = "_omp", from_py_object)]
/// A path in the workspace Environment filesystem namespace.
#[derive(Clone, Debug)]
struct PyEnvPath(EnvPath);

#[pymethods]
impl PyEnvPath {
	#[new]
	fn new(value: &str) -> PyResult<Self> {
		EnvPath::new(Str::new(value)).map(Self).map_err(value_error)
	}

	#[getter]
	fn uri(&self) -> PyResult<String> {
		path_uri(self.0.as_str())
	}

	#[pyo3(signature = (*parts))]
	fn join(&self, parts: &Bound<'_, PyTuple>) -> PyResult<Self> {
		let parts = parts
			.iter()
			.map(|part| part.extract::<String>())
			.collect::<PyResult<Vec<_>>>()?;
		join_env_path(self.0.as_str(), parts.iter().map(String::as_str))
	}

	#[pyo3(signature = (encoding = "utf-8"))]
	fn read_text(&self, py: Python<'_>, encoding: &str) -> PyResult<Py<PyAny>> {
		let module = py.import("omp.env")?;
		Ok(module
			.getattr("_read_text")?
			.call1((self.clone(), encoding))?
			.unbind())
	}

	fn read_bytes(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
		let module = py.import("omp.env")?;
		Ok(module
			.getattr("_read_bytes")?
			.call1((self.clone(),))?
			.unbind())
	}

	fn local_path(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
		let module = py.import("omp.env")?;
		Ok(module
			.getattr("_local_path")?
			.call1((self.clone(),))?
			.unbind())
	}

	fn __str__(&self) -> &str {
		self.0.as_str()
	}

	fn __repr__(&self) -> String {
		format!("EnvPath({:?})", self.0.as_str())
	}

	fn __hash__(&self) -> isize {
		let mut hasher = std::collections::hash_map::DefaultHasher::new();
		self.0.hash(&mut hasher);
		hasher.finish() as isize
	}

	fn __richcmp__(&self, other: &Self, op: pyo3::basic::CompareOp) -> bool {
		match op {
			pyo3::basic::CompareOp::Eq => self.0 == other.0,
			pyo3::basic::CompareOp::Ne => self.0 != other.0,
			_ => false,
		}
	}
}

#[pyclass(name = "ClientPath", frozen, module = "_omp", from_py_object)]
/// A path in the client-machine filesystem namespace.
#[derive(Clone, Debug)]
struct PyClientPath(ClientPath);

#[pymethods]
impl PyClientPath {
	#[new]
	fn new(value: &str) -> PyResult<Self> {
		ClientPath::new(Str::new(value))
			.map(Self)
			.map_err(value_error)
	}

	#[getter]
	fn uri(&self) -> &str {
		self.0.as_str()
	}

	#[pyo3(signature = (*parts))]
	fn join(&self, parts: &Bound<'_, PyTuple>) -> PyResult<Self> {
		let parts = parts
			.iter()
			.map(|part| part.extract::<String>())
			.collect::<PyResult<Vec<_>>>()?;
		join_client_path(self.0.as_str(), parts.iter().map(String::as_str))
	}

	fn __str__(&self) -> &str {
		self.0.as_str()
	}

	fn __repr__(&self) -> String {
		format!("ClientPath({:?})", self.0.as_str())
	}

	fn __hash__(&self) -> isize {
		let mut hasher = std::collections::hash_map::DefaultHasher::new();
		self.0.hash(&mut hasher);
		hasher.finish() as isize
	}

	fn __richcmp__(&self, other: &Self, op: pyo3::basic::CompareOp) -> bool {
		match op {
			pyo3::basic::CompareOp::Eq => self.0 == other.0,
			pyo3::basic::CompareOp::Ne => self.0 != other.0,
			_ => false,
		}
	}
}

fn join_env_path<'a>(base: &str, parts: impl Iterator<Item = &'a str>) -> PyResult<PyEnvPath> {
	let joined = join_path(base, parts)?;
	EnvPath::new(Str::new(joined))
		.map(PyEnvPath)
		.map_err(value_error)
}

fn join_client_path<'a>(
	base: &str,
	parts: impl Iterator<Item = &'a str>,
) -> PyResult<PyClientPath> {
	let joined = join_path(base, parts)?;
	ClientPath::new(Str::new(joined))
		.map(PyClientPath)
		.map_err(value_error)
}

fn join_path<'a>(base: &str, parts: impl Iterator<Item = &'a str>) -> PyResult<String> {
	let mut joined = String::from(base.trim_end_matches('/'));
	for part in parts {
		if part.is_empty() || part.as_bytes().contains(&0) {
			return Err(PyValueError::new_err("path components must be non-empty and contain no NUL"));
		}
		joined.push('/');
		joined.push_str(part.trim_matches('/'));
	}
	Ok(joined)
}

fn path_uri(path: &str) -> PyResult<String> {
	if path.starts_with("file://") {
		return Ok(path.to_owned());
	}
	let root = RUNTIME.root_uri.read();
	let root = root
		.as_deref()
		.ok_or_else(|| EnvUnavailable::new_err("no Environment is installed"))?;
	let mut uri = String::with_capacity(root.len() + path.len() + 1);
	uri.push_str(root.trim_end_matches('/'));
	if !path.starts_with('/') {
		uri.push('/');
	}
	uri.push_str(path);
	Ok(uri)
}

#[pyclass(name = "BlobRef", frozen, module = "_omp", from_py_object)]
/// A content-addressed reference in one Environment blob store.
#[derive(Clone, Debug)]
struct PyBlobRef {
	hash: [u8; 32],
	size: u64,
}

#[pymethods]
impl PyBlobRef {
	#[new]
	fn new(hash: &[u8], size: u64) -> PyResult<Self> {
		let hash = <[u8; 32]>::try_from(hash)
			.map_err(|_| PyValueError::new_err("BlobRef hash must contain exactly 32 bytes"))?;
		Ok(Self { hash, size })
	}

	#[getter]
	fn hash<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
		PyBytes::new(py, &self.hash)
	}

	#[getter]
	const fn size(&self) -> u64 {
		self.size
	}

	#[getter]
	fn hex(&self) -> String {
		omp_core::encoding::hex::encode(&self.hash).to_string()
	}

	fn __repr__(&self) -> String {
		format!("BlobRef(hash={}, size={})", self.hex(), self.size)
	}

	fn __hash__(&self) -> isize {
		let mut hasher = std::collections::hash_map::DefaultHasher::new();
		self.hash.hash(&mut hasher);
		hasher.finish() as isize
	}

	fn __richcmp__(&self, other: &Self, op: pyo3::basic::CompareOp) -> bool {
		match op {
			pyo3::basic::CompareOp::Eq => self.hash == other.hash,
			pyo3::basic::CompareOp::Ne => self.hash != other.hash,
			_ => false,
		}
	}
}

/// Core-authenticated principal identity exposed read-only to Python.
#[pyclass(name = "Principal", frozen, module = "_omp", from_py_object)]
#[derive(Clone, Debug)]
struct PyPrincipal(Principal);

#[pymethods]
impl PyPrincipal {
	#[getter]
	fn id(&self) -> &str {
		self.0.id()
	}

	#[getter]
	fn display(&self) -> &str {
		self.0.display()
	}

	#[staticmethod]
	const fn __repr__() -> &'static str {
		"Principal(<core-issued>)"
	}

	fn __hash__(&self) -> isize {
		let mut hasher = std::collections::hash_map::DefaultHasher::new();
		self.0.hash(&mut hasher);
		hasher.finish() as isize
	}

	fn __richcmp__(&self, other: &Self, op: pyo3::basic::CompareOp) -> bool {
		match op {
			pyo3::basic::CompareOp::Eq => self.0 == other.0,
			pyo3::basic::CompareOp::Ne => self.0 != other.0,

			_ => false,
		}
	}
}
/// Creates the read-only Python view of a core-authenticated principal.
pub fn bind_principal(py: Python<'_>, principal: Principal) -> PyResult<Py<PyAny>> {
	Ok(Py::new(py, PyPrincipal(principal))?.into_any())
}

/// One quota's immutable local standing.
#[pyclass(name = "QuotaStatus", frozen, module = "_omp", skip_from_py_object)]
#[derive(Clone, Debug)]
struct PyQuotaStatus {
	#[pyo3(get)]
	limit:  u64,
	#[pyo3(get)]
	used:   u64,
	#[pyo3(get)]
	window: Option<PyDuration>,
}

/// Immutable snapshot of extension resource quota standing.
#[pyclass(name = "ResourceReceipt", frozen, module = "_omp")]
#[derive(Debug)]
struct PyResourceReceipt {
	#[pyo3(get)]
	quotas:  Py<PyAny>,
	#[pyo3(get)]
	dropped: Py<PyAny>,
}

#[pyfunction]
fn resources(py: Python<'_>) -> PyResult<PyResourceReceipt> {
	let state = RUNTIME.resources.read();
	let quotas = pyo3::types::PyDict::new(py);
	for (name, status) in &state.quotas {
		let value = Py::new(py, PyQuotaStatus {
			limit:  status.limit,
			used:   status.used,
			window: status.window.map(PyDuration),
		})?;
		quotas.set_item(name.as_str(), value)?;
	}
	let dropped = pyo3::types::PyDict::new(py);
	for (name, count) in &state.dropped {
		dropped.set_item(name.as_str(), count)?;
	}
	let proxy = py.import("types")?.getattr("MappingProxyType")?;
	Ok(PyResourceReceipt {
		quotas:  proxy.call1((quotas,))?.unbind(),
		dropped: proxy.call1((dropped,))?.unbind(),
	})
}

#[pyfunction]
fn _read_bytes_blocking<'py>(py: Python<'py>, path: &PyEnvPath) -> PyResult<Bound<'py, PyBytes>> {
	let client = RUNTIME
		.client
		.read()
		.clone()
		.ok_or_else(|| EnvUnavailable::new_err("no Environment DATA client is installed"))?;
	let bytes = ASYNC_RUNTIME
		.block_on(async {
			let lease = client.open_document(&path.0, None).await?;
			let read = client.read_document(&lease, None, None).await?;
			read
				.content()
				.cloned()
				.ok_or(omp_env::ClientError::UnexpectedResponse { expected: "whole-document bytes" })
		})
		.map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
	Ok(PyBytes::new(py, &bytes))
}

#[pyfunction]
fn _scheme_snapshot(py: Python<'_>) -> PyResult<Py<PyAny>> {
	let snapshot = RUNTIME.schemes.read();
	let urls = py.import("omp.urls")?;
	let scheme_type = urls.getattr("Scheme")?;
	let info_type = urls.getattr("SchemeInfo")?;
	let entries = PyList::empty(py);
	for entry in &snapshot.entries {
		let scheme = scheme_type.get_item(entry.member.as_str())?;
		let info = info_type.call1((
			entry.readable,
			entry.mintable,
			entry.selectors,
			entry.description.as_str(),
		))?;
		entries.append((scheme, info))?;
	}
	let hash = PyBytes::new(py, &snapshot.device_hash);
	Ok(PyTuple::new(py, [hash.into_any(), entries.into_any()])?
		.unbind()
		.into_any())
}
#[pyfunction]
fn _local_path_string(_path: &PyEnvPath) -> PyResult<String> {
	Err(PlacementError::new_err(
		"this extension is not colocated with an authorized Environment filesystem",
	))
}

/// Immutable reference to the inherited CONTROL transport.
#[pyclass(name = "ControlHandle", frozen, module = "_omp")]
#[derive(Debug)]
struct PyControlHandle {
	fd: i32,
}

#[pymethods]
impl PyControlHandle {
	#[new]
	fn new(fd: i32) -> Self {
		Self { fd }
	}

	#[getter]
	fn fd(&self) -> i32 {
		self.fd
	}
}

/// Immutable reference to an invocation-scoped DATA transport.
#[pyclass(name = "DataHandle", frozen, module = "_omp")]
#[derive(Debug)]
struct PyDataHandle {
	generation: u64,
}

#[pymethods]
impl PyDataHandle {
	#[new]
	fn new(generation: u64) -> Self {
		Self { generation }
	}

	#[getter]
	fn generation(&self) -> u64 {
		self.generation
	}
}

/// Frozen cancellation token shared safely by free-threaded Python callers.
#[pyclass(name = "Cancellation", frozen, module = "_omp")]
#[derive(Debug, Default)]
struct PyCancellation {
	cancelled: std::sync::atomic::AtomicBool,
}

#[pymethods]
impl PyCancellation {
	#[new]
	fn new() -> Self {
		Self::default()
	}

	fn cancel(&self) {
		self
			.cancelled
			.store(true, std::sync::atomic::Ordering::Release);
	}

	#[getter]
	fn cancelled(&self) -> bool {
		self.cancelled.load(std::sync::atomic::Ordering::Acquire)
	}
}

/// Return CPython's identifier for the attached current thread.
#[pyfunction]
fn _thread_id() -> u64 {
	crate::interrupt::current_thread_id()
}

/// Deliver a stage-two `KeyboardInterrupt` to a Python thread id.
#[pyfunction]
fn _interrupt(py: Python<'_>, thread_id: u64) -> bool {
	crate::interrupt::interrupt(py, thread_id)
}

/// Registers the native `_omp` module before `CPython` initialization.
pub fn register() {
	pyo3::append_to_inittab!(_omp);
}

#[pymodule(gil_used = false)]
fn _omp(py: Python<'_>, module: &Bound<'_, PyModule>) -> PyResult<()> {
	module.add_class::<PyDuration>()?;
	module.add_class::<PySecret>()?;
	module.add_class::<PySecretUse>()?;
	module.add_class::<PyInvocationPhase>()?;
	module.add_class::<PyLifecyclePhase>()?;
	module.add_class::<PyDurability>()?;
	module.add_class::<PyActivateReason>()?;
	module.add_class::<PyRestartReason>()?;
	module.add_class::<PyStateScope>()?;
	module.add_class::<PyCostClass>()?;
	module.add_class::<PyAuthority>()?;
	module.add_class::<PyOperationSpec>()?;
	module.add_class::<PyEnvPath>()?;
	module.add_class::<PyClientPath>()?;
	module.add_class::<PyBlobRef>()?;
	module.add_class::<PyArtifactUrl>()?;
	module.add_class::<PyHistoryUrl>()?;
	module.add_class::<PyAgentUrl>()?;
	module.add_class::<PyWorkspaceUri>()?;
	module.add_class::<PyPrincipal>()?;
	module.add_class::<PyQuotaStatus>()?;
	module.add_class::<PyResourceReceipt>()?;
	module.add_class::<PyControlHandle>()?;
	module.add_class::<PyDataHandle>()?;
	module.add_class::<PyCancellation>()?;
	module.add_function(wrap_pyfunction!(_interrupt, module)?)?;
	module.add_function(wrap_pyfunction!(_thread_id, module)?)?;
	module.add_function(wrap_pyfunction!(operation_spec, module)?)?;
	module.add_function(wrap_pyfunction!(resources, module)?)?;
	module.add_function(wrap_pyfunction!(_read_bytes_blocking, module)?)?;
	module.add_function(wrap_pyfunction!(_local_path_string, module)?)?;
	module.add_function(wrap_pyfunction!(_runtime_metadata, module)?)?;

	module.add_function(wrap_pyfunction!(_phase_legality_matrix, module)?)?;
	macro_rules! add_exception {
		($name:ident) => {
			module.add(stringify!($name), py.get_type::<$name>())?;
		};
	}
	add_exception!(OmpError);
	add_exception!(ManifestError);
	add_exception!(ApiLevelError);
	add_exception!(DeclarationLimit);
	add_exception!(CapabilityError);
	add_exception!(TrustError);
	add_exception!(DuplicateRegistration);
	add_exception!(DeclarationSealed);
	add_exception!(EffectsNotAuthorized);
	add_exception!(DeadlineExceeded);
	add_exception!(HostDisconnected);
	add_exception!(FrameTooLarge);
	add_exception!(EnvUnavailable);
	add_exception!(PlacementError);
	module.add_function(wrap_pyfunction!(_scheme_snapshot, module)?)?;
	add_exception!(StaleGeneration);
	Ok(())
}
