//! Capability, invocation-authority, lease, and quota enforcement for DATA.

use std::{
	collections::HashMap,
	sync::{
		Arc,
		atomic::{AtomicU64, Ordering},
	},
};

use bytes::Bytes;
use omp_core::Str;
use parking_lot::Mutex;
use thiserror::Error;

use super::worker::HostKey;

/// Capabilities implemented by the environment DATA plane.
pub const CAPABILITIES: &[&str] = &[
	"invocation",
	"env.exec",
	"env.process",
	"env.workspace.snapshot",
	"env.worktree",
	"env.blob",
	"env.doc.read",
	"env.doc.write",
	"env.fs.read",
	"env.fs.write",
	"env.search",
	"env.lsp",
	"env.dap.read",
	"env.dap.execute",
];

/// An exact, wildcard-free set of DATA grants.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Grants(Arc<[Str]>);

impl Grants {
	/// Returns every capability this Environment actually implements.
	#[must_use]
	pub fn all() -> Self {
		Self(CAPABILITIES.iter().copied().map(Str::new_static).collect())
	}

	/// Retains supported capabilities from `grants`, removing duplicates.
	#[must_use]
	pub fn supported<I, S>(grants: I) -> Self
	where
		I: IntoIterator<Item = S>,
		S: AsRef<str>,
	{
		let mut values: Vec<Str> = grants
			.into_iter()
			.filter_map(|grant| {
				let grant = grant.as_ref();
				CAPABILITIES.contains(&grant).then(|| Str::from(grant))
			})
			.collect();
		values.sort_unstable();
		values.dedup();
		Self(values.into())
	}

	/// Computes the requested intersection without granting unsupported names.
	#[must_use]
	pub fn requested(&self, requested: &[String]) -> Self {
		Self::supported(
			requested
				.iter()
				.map(String::as_str)
				.filter(|capability| self.contains(capability)),
		)
	}

	/// Computes an exact set intersection.
	#[must_use]
	pub fn intersection(&self, other: &Self) -> Self {
		Self::supported(self.iter().filter(|capability| other.contains(capability)))
	}

	/// Returns whether this set contains `capability` exactly.
	#[must_use]
	pub fn contains(&self, capability: &str) -> bool {
		self.0.iter().any(|grant| grant.as_str() == capability)
	}

	/// Iterates grants in stable lexical order.
	pub fn iter(&self) -> impl ExactSizeIterator<Item = &str> + DoubleEndedIterator + Clone + '_ {
		self.0.iter().map(Str::as_str)
	}

	/// Converts Core's narrowed effect envelope into exact DATA capability
	/// bounds.
	#[must_use]
	pub fn from_effect_envelope(envelope: &omp_proto::policy::v1::EffectEnvelope) -> Self {
		let mut grants = Vec::with_capacity(10);
		if let Some(documents) = &envelope.documents {
			if documents.read {
				grants.extend([
					"env.doc.read",
					"env.fs.read",
					"env.search",
					"env.lsp",
					"env.dap.read",
					"env.blob",
				]);
			}
			if !documents.write_globs.is_empty() {
				grants.extend(["env.doc.write", "env.fs.write", "env.blob"]);
			}
		}
		if envelope
			.exec
			.as_ref()
			.is_some_and(|exec| !exec.commands.is_empty())
		{
			grants.extend(["env.exec", "env.dap.read", "env.dap.execute", "env.blob"]);
		}
		Self::supported(grants)
	}
}

/// Immutable Environment tier for a language-server operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LspOperationTier {
	/// Query-only operation.
	ReadOnly,
	/// Operation that may mutate workspace state or execute a server command.
	Mutation,
}

/// Returns the immutable tier for one raw LSP request method.
#[must_use]
pub fn lsp_request_tier(method: &str) -> LspOperationTier {
	match method {
		"workspace/executeCommand"
		| "textDocument/rename"
		| "workspace/willCreateFiles"
		| "workspace/willRenameFiles"
		| "workspace/willDeleteFiles" => LspOperationTier::Mutation,
		_ => LspOperationTier::ReadOnly,
	}
}

/// Returns the immutable tier for one raw LSP notification method.
///
/// Only connection lifecycle controls are query-tier. Every other raw
/// notification fails closed as a mutation because vendor methods can execute
/// arbitrary server commands.
#[must_use]
pub fn lsp_notification_tier(method: &str) -> LspOperationTier {
	match method {
		"initialized" | "$/cancelRequest" | "$/setTrace" | "exit" => LspOperationTier::ReadOnly,
		_ => LspOperationTier::Mutation,
	}
}

/// Returns the exact grant required by an LSP operation tier.
#[must_use]
pub const fn lsp_tier_capability(tier: LspOperationTier) -> &'static str {
	match tier {
		LspOperationTier::ReadOnly => "env.lsp",
		LspOperationTier::Mutation => "env.doc.write",
	}
}

/// Returns the immutable Environment tier for one DAP action.
#[must_use]
pub const fn dap_action_tier(action: omp_docserver::DapAction) -> omp_docserver::DapApprovalTier {
	action.approval_tier()
}

/// Classifies one DAP wire action, failing closed for unknown/custom commands.
#[must_use]
pub fn dap_command_tier(command: &str) -> omp_docserver::DapApprovalTier {
	command
		.parse::<omp_docserver::DapAction>()
		.map_or(omp_docserver::DapApprovalTier::Execution, dap_action_tier)
}

/// Returns the exact DATA capability required by one DAP action.
#[must_use]
pub const fn dap_action_capability(action: omp_docserver::DapAction) -> &'static str {
	match dap_action_tier(action) {
		omp_docserver::DapApprovalTier::ReadOnly => "env.dap.read",
		omp_docserver::DapApprovalTier::Execution => "env.dap.execute",
	}
}

/// Returns the exact DATA capability required by one DAP wire command.
#[must_use]
pub fn dap_command_capability(command: &str) -> &'static str {
	match dap_command_tier(command) {
		omp_docserver::DapApprovalTier::ReadOnly => "env.dap.read",
		omp_docserver::DapApprovalTier::Execution => "env.dap.execute",
	}
}

/// Invocation-scoped credentials carried by every DATA request.
pub struct DataAuthority<'a> {
	/// Stable invocation identity.
	pub invocation_id:      &'a str,
	/// Opaque Core-minted effect token.
	pub effect_token:       &'a [u8],
	/// Extension-host process generation.
	pub host_generation:    u64,
	/// Owning session generation.
	pub session_generation: u64,
}

/// A typed fail-closed DATA authorization refusal.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum PolicyError {
	/// The invocation has not reached `EFFECTS_AUTHORIZED`, or has settled.
	#[error("invocation effects are not authorized")]
	EffectsNotAuthorized,
	/// The connection or invocation envelope lacks a required capability.
	#[error("capability denied: {capability}")]
	Denied {
		/// Exact capability required by the refused operation.
		capability: &'static str,
	},
	/// The effect token is absent, mismatched, revoked, or claimed by another
	/// connection.
	#[error("effect token is invalid or revoked")]
	InvalidEffectToken,
	/// The request was minted by a stale host or session generation.
	#[error("host or session generation is stale")]
	StaleGeneration,
	/// A document lease belongs to another connection.
	#[error("document lease is owned by another connection")]
	LeaseNotOwned,
	/// ENFORCE was requested while sandbox installation remains deferred.
	#[error("sandbox ENFORCE is unavailable")]
	EnforcementUnavailable,
	/// A per-extension DATA quota is exhausted.
	#[error("quota {quota} exhausted ({used}/{limit})")]
	QuotaExceeded {
		/// Stable name of the exhausted DATA resource.
		quota: &'static str,
		/// Maximum resources permitted per extension.
		limit: u64,
		/// Resources already charged when the operation was refused.
		used:  u64,
	},
}

/// Refuses ENFORCE while kernel sandbox installation is explicitly deferred.
///
/// OBSERVE/OFF policy remains outside this deferred enforcement path.
pub const fn require_sandbox_enforcement(enforce: bool) -> Result<(), PolicyError> {
	if enforce {
		Err(PolicyError::EnforcementUnavailable)
	} else {
		Ok(())
	}
}

#[derive(Clone)]
struct AuthorizedInvocation {
	phase:              omp_core::InvocationPhase,
	effect_token:       Bytes,
	envelope:           Grants,
	authorized_at_ms:   u64,
	host_generation:    u64,
	session_generation: u64,
	claimed_by:         Option<u64>,
}

#[derive(Default)]
struct HostAuthority {
	grants:      Grants,
	invocations: HashMap<Str, AuthorizedInvocation>,
	quota:       [u64; Quota::COUNT],
}

#[derive(Default)]
struct AuthorityState {
	hosts:  HashMap<HostKey, HostAuthority>,
	leases: HashMap<Bytes, u64>,
}

/// Shared authoritative invocation/token table for all connections of one
/// Environment.
#[derive(Default)]
pub struct AuthorityTable {
	state:           Mutex<AuthorityState>,
	next_connection: AtomicU64,
}

impl AuthorityTable {
	/// Allocates an opaque connection owner used to bind tokens and leases.
	#[must_use]
	pub fn connection_owner(&self) -> u64 {
		self
			.next_connection
			.fetch_add(1, Ordering::Relaxed)
			.wrapping_add(1)
	}

	/// Installs the manifest-derived extension grants for one host.
	pub fn register_host(&self, host: HostKey, grants: Grants) {
		let mut state = self.state.lock();
		state.hosts.entry(host).or_default().grants = grants;
	}

	/// Records a newly opened extension invocation at `OPEN`.
	pub fn open(&self, host: HostKey, invocation_id: Str) {
		let mut state = self.state.lock();
		state.hosts.entry(host).or_default().invocations.insert(
			invocation_id,
			AuthorizedInvocation {
				phase:              omp_core::InvocationPhase::Open,
				effect_token:       Bytes::new(),
				envelope:           Grants::default(),
				authorized_at_ms:   0,
				host_generation:    0,
				session_generation: 0,
				claimed_by:         None,
			},
		);
	}

	/// Advances an open invocation through the canonical seven-phase machine and
	/// installs the exact Core-minted effect token and narrowed envelope.
	pub fn authorize(
		&self,
		host: &HostKey,
		invocation_id: &str,
		effect_token: Bytes,
		envelope: Grants,
		authorized_at_ms: u64,
		host_generation: u64,
		session_generation: u64,
	) -> Result<(), PolicyError> {
		if effect_token.is_empty() {
			return Err(PolicyError::InvalidEffectToken);
		}
		if authorized_at_ms == 0 {
			return Err(PolicyError::EffectsNotAuthorized);
		}
		let mut state = self.state.lock();
		let Some(host_authority) = state.hosts.get_mut(host) else {
			return Err(PolicyError::Denied { capability: "extension host" });
		};
		let bounded_envelope = host_authority.grants.intersection(&envelope);
		let Some(invocation) = host_authority.invocations.get_mut(invocation_id) else {
			return Err(PolicyError::EffectsNotAuthorized);
		};
		if invocation.phase != omp_core::InvocationPhase::Open {
			return Err(PolicyError::InvalidEffectToken);
		}
		for phase in [
			omp_core::InvocationPhase::ArgsFinalized,
			omp_core::InvocationPhase::Admission,
			omp_core::InvocationPhase::Admitted,
			omp_core::InvocationPhase::AssistantItemCommitted,
			omp_core::InvocationPhase::EffectsAuthorized,
		] {
			debug_assert!(invocation.phase.can_transition_to(phase));
			invocation.phase = phase;
		}
		invocation.effect_token = effect_token;
		invocation.envelope = bounded_envelope;
		invocation.authorized_at_ms = authorized_at_ms;
		invocation.host_generation = host_generation;
		invocation.session_generation = session_generation;
		Ok(())
	}

	/// Returns whether `invocation_id` names a live extension-worker invocation.
	#[must_use]
	pub fn is_worker_invocation(&self, host: &HostKey, invocation_id: &str) -> bool {
		self
			.state
			.lock()
			.hosts
			.get(host)
			.is_some_and(|authority| authority.invocations.contains_key(invocation_id))
	}

	/// Validates phase, exact token, generations, connection binding, and effect
	/// envelope.
	pub fn validate(
		&self,
		host: &HostKey,
		connection_owner: u64,
		credentials: DataAuthority<'_>,
		capability: &'static str,
	) -> Result<(), PolicyError> {
		let mut state = self.state.lock();
		let Some(invocation) = state
			.hosts
			.get_mut(host)
			.and_then(|authority| authority.invocations.get_mut(credentials.invocation_id))
		else {
			return Err(PolicyError::EffectsNotAuthorized);
		};
		if invocation.authorized_at_ms == 0
			|| !invocation
				.phase
				.allows_operation(omp_core::InvocationPhase::EffectsAuthorized)
		{
			return Err(PolicyError::EffectsNotAuthorized);
		}
		if invocation.host_generation != credentials.host_generation
			|| invocation.session_generation != credentials.session_generation
		{
			return Err(PolicyError::StaleGeneration);
		}
		if invocation.effect_token.as_ref() != credentials.effect_token
			|| credentials.effect_token.is_empty()
		{
			return Err(PolicyError::InvalidEffectToken);
		}
		match invocation.claimed_by {
			Some(owner) if owner != connection_owner => return Err(PolicyError::InvalidEffectToken),
			Some(_) => {},
			None => invocation.claimed_by = Some(connection_owner),
		}
		if !invocation.envelope.contains(capability) {
			return Err(PolicyError::Denied { capability });
		}
		Ok(())
	}

	/// Validates a read-class DATA request's authorization phase, token,
	/// generations, and connection ownership without requiring a mutation
	/// capability in the narrowed effect envelope.
	pub fn validate_read(
		&self,
		host: &HostKey,
		connection_owner: u64,
		credentials: DataAuthority<'_>,
	) -> Result<(), PolicyError> {
		let mut state = self.state.lock();
		let Some(invocation) = state
			.hosts
			.get_mut(host)
			.and_then(|authority| authority.invocations.get_mut(credentials.invocation_id))
		else {
			return Err(PolicyError::EffectsNotAuthorized);
		};
		if invocation.authorized_at_ms == 0
			|| !invocation
				.phase
				.allows_operation(omp_core::InvocationPhase::EffectsAuthorized)
		{
			return Err(PolicyError::EffectsNotAuthorized);
		}
		if invocation.host_generation != credentials.host_generation
			|| invocation.session_generation != credentials.session_generation
		{
			return Err(PolicyError::StaleGeneration);
		}
		if invocation.effect_token.as_ref() != credentials.effect_token
			|| credentials.effect_token.is_empty()
		{
			return Err(PolicyError::InvalidEffectToken);
		}
		match invocation.claimed_by {
			Some(owner) if owner != connection_owner => return Err(PolicyError::InvalidEffectToken),
			Some(_) => {},
			None => invocation.claimed_by = Some(connection_owner),
		}
		Ok(())
	}

	/// Settles an invocation and revokes its token before returning.
	pub fn settle(&self, host: &HostKey, invocation_id: &str) {
		let mut state = self.state.lock();
		if let Some(authority) = state.hosts.get_mut(host)
			&& let Some(mut invocation) = authority.invocations.remove(invocation_id)
		{
			if invocation.phase == omp_core::InvocationPhase::EffectsAuthorized {
				invocation.phase = omp_core::InvocationPhase::Settled;
			}
			invocation.effect_token = Bytes::new();
		}
	}

	/// Records ownership of a newly opened document lease.
	pub fn register_lease(&self, lease_id: Bytes, connection_owner: u64) {
		self.state.lock().leases.insert(lease_id, connection_owner);
	}

	/// Checks that a lease belongs to the requesting connection.
	pub fn check_lease(&self, lease_id: &[u8], connection_owner: u64) -> Result<(), PolicyError> {
		match self.state.lock().leases.get(lease_id) {
			Some(owner) if *owner == connection_owner => Ok(()),
			Some(_) => Err(PolicyError::LeaseNotOwned),
			None => Ok(()),
		}
	}

	/// Removes a lease from the cross-connection ownership table.
	pub fn release_lease(&self, lease_id: &[u8], connection_owner: u64) {
		let mut state = self.state.lock();
		if state.leases.get(lease_id).copied() == Some(connection_owner) {
			state.leases.remove(lease_id);
		}
	}

	fn reserve(&self, host: &HostKey, quota: Quota, amount: u64) -> Result<(), PolicyError> {
		let mut state = self.state.lock();
		let usage = &mut state.hosts.entry(host.clone()).or_default().quota[quota.index];
		let Some(next) = usage.checked_add(amount) else {
			return Err(PolicyError::QuotaExceeded {
				quota: quota.name,
				limit: quota.limit,
				used:  *usage,
			});
		};
		if next > quota.limit {
			return Err(PolicyError::QuotaExceeded {
				quota: quota.name,
				limit: quota.limit,
				used:  *usage,
			});
		}
		*usage = next;
		Ok(())
	}

	fn release(&self, host: &HostKey, quota: Quota, amount: u64) {
		let mut state = self.state.lock();
		if let Some(authority) = state.hosts.get_mut(host) {
			authority.quota[quota.index] = authority.quota[quota.index].saturating_sub(amount);
		}
	}
}

#[derive(Clone, Copy)]
struct Quota {
	index: usize,
	name:  &'static str,
	limit: u64,
}

impl Quota {
	const BLOB_INGEST: Self =
		Self { index: 2, name: "blob_ingest_bytes", limit: 256 * 1024 * 1024 };
	const COUNT: usize = 5;
	const DOCUMENT_LEASES: Self = Self { index: 0, name: "document_leases", limit: 128 };
	const EXEC_CONCURRENCY: Self = Self { index: 3, name: "exec_concurrency", limit: 32 };
	const PROCESS_CHURN: Self = Self { index: 1, name: "process_churn", limit: 256 };
	const STREAM_FANOUT: Self = Self { index: 4, name: "stream_fanout", limit: 64 };
}

/// Per-connection quota accounting backed by the extension-wide ledger.
pub struct QuotaAccount {
	table: AuthorityTableRef,
	host:  Option<HostKey>,
	usage: [u64; Quota::COUNT],
}

type AuthorityTableRef = Arc<AuthorityTable>;

impl QuotaAccount {
	/// Creates accounting for an owner or extension connection.
	#[must_use]
	pub const fn new(table: AuthorityTableRef, host: Option<HostKey>) -> Self {
		Self { table, host, usage: [0; Quota::COUNT] }
	}

	fn reserve(&mut self, quota: Quota, amount: u64) -> Result<(), PolicyError> {
		if let Some(host) = &self.host {
			self.table.reserve(host, quota, amount)?;
		}
		self.usage[quota.index] = self.usage[quota.index].saturating_add(amount);
		Ok(())
	}

	fn release(&mut self, quota: Quota, amount: u64) {
		let released = amount.min(self.usage[quota.index]);
		self.usage[quota.index] -= released;
		if let Some(host) = &self.host {
			self.table.release(host, quota, released);
		}
	}

	/// Reserves one live document lease.
	pub fn reserve_document_lease(&mut self) -> Result<(), PolicyError> {
		self.reserve(Quota::DOCUMENT_LEASES, 1)
	}

	/// Releases one live document lease.
	pub fn release_document_lease(&mut self) {
		self.release(Quota::DOCUMENT_LEASES, 1);
	}

	/// Charges one named-process start or restart.
	pub fn charge_process_start(&mut self) -> Result<(), PolicyError> {
		self.reserve(Quota::PROCESS_CHURN, 1)
	}

	/// Charges blob bytes accepted on this connection.
	pub fn charge_blob_bytes(&mut self, bytes: usize) -> Result<(), PolicyError> {
		self.reserve(Quota::BLOB_INGEST, bytes as u64)
	}

	/// Reserves one live exec session or run.
	pub fn reserve_exec(&mut self) -> Result<(), PolicyError> {
		self.reserve(Quota::EXEC_CONCURRENCY, 1)
	}

	/// Releases one live exec session or run.
	pub fn release_exec(&mut self) {
		self.release(Quota::EXEC_CONCURRENCY, 1);
	}

	/// Reserves one live event stream.
	pub fn reserve_stream(&mut self) -> Result<(), PolicyError> {
		self.reserve(Quota::STREAM_FANOUT, 1)
	}

	/// Releases one live event stream.
	pub fn release_stream(&mut self) {
		self.release(Quota::STREAM_FANOUT, 1);
	}
}

impl Drop for QuotaAccount {
	fn drop(&mut self) {
		if let Some(host) = &self.host {
			for quota in [
				Quota::DOCUMENT_LEASES,
				Quota::PROCESS_CHURN,
				Quota::BLOB_INGEST,
				Quota::EXEC_CONCURRENCY,
				Quota::STREAM_FANOUT,
			] {
				self.table.release(host, quota, self.usage[quota.index]);
			}
		}

		#[cfg(test)]
		mod tests {
			use super::*;

			#[test]
			fn dap_tiers_match_pi_and_unknown_actions_fail_closed() {
				assert_eq!(dap_command_capability("variables"), "env.dap.read");
				assert_eq!(dap_command_capability("read_memory"), "env.dap.read");
				assert_eq!(dap_command_capability("evaluate"), "env.dap.execute");
				assert_eq!(dap_command_capability("continue"), "env.dap.execute");
				assert_eq!(dap_command_capability("vendor_mutation"), "env.dap.execute");
			}

			#[test]
			fn mutative_lsp_methods_require_write_before_effects_authorization() {
				assert_eq!(lsp_tier_capability(lsp_request_tier("textDocument/hover")), "env.lsp");
				assert_eq!(
					lsp_tier_capability(lsp_request_tier("workspace/executeCommand")),
					"env.doc.write"
				);
				assert_eq!(
					lsp_tier_capability(lsp_notification_tier("workspace/didRenameFiles")),
					"env.doc.write"
				);
			}
		}
	}
}
