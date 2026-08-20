//! Temporary size probe.

macro_rules! probe {
	($($t:ty),* $(,)?) => {
		$( println!("{:>6}  {}", std::mem::size_of::<$t>(), stringify!($t)); )*
	};
}

#[test]
fn sizes() {
	probe!(
		omp_core::Str,
		Option<omp_core::Str>,
		omp_core::CowBytes<'static>,
		omp_core::Principal,
		omp_core::Provenance,
		tonic::Status,
		omp_storage::transcript::Error,
		omp_storage::transcript::writer::JournalError,
		omp_storage::transcript::writer::JournalIndeterminate,
		omp_storage::transcript::writer::AppendManyError,
		omp_storage::index::Error,
		omp_storage::state::Error,
		omp_storage::state::StateEntry,
		omp_storage::state::StateEntryId,
		omp_storage::state::StateChange,
		omp_storage::state::StateValue,
		omp_agent::JournalError,
		omp_agent::AgentError,
		omp_tool::Effects,
		omp_tool::DocEffects,
		omp_tool::ExecEffects,
		omp_tool::Usd,
		omp_tool::PolicyDenied,
		omp_tool::ToolIdentity,
		omp_tool::Rev,
		omp_tool::ArgPath,
		omp_tool::ArgSpec,
		omp_tool::ArgSpecRegistryError,
		omp_tool::render::RenderRegistryError,
		omp_proto::env::v1::AdmitInvocation,
		omp_proto::thread::v1::Item,
		omp_proto::inference::v1::TurnError,
		omp_app::exthost::services::ServiceError,
		omp_app::exthost::services::ServiceKey,
		omp_app::envd::worker::HostKey,
		omp_app::exthost::control::JournalControlError,
	);
}
