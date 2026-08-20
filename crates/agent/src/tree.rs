//! Session agent roster, recursive budget authority, and concurrency permits.

use std::{
	collections::HashMap,
	future::Future,
	sync::{
		Arc,
		atomic::{AtomicU8, AtomicUsize, Ordering},
	},
	time::Instant,
};

use omp_core::{AppendVec, InvocationPhase, Str};
use parking_lot::{Mutex, RwLock};
use thiserror::Error;

/// Default tree-wide number of concurrently running agent turns.
pub const DEFAULT_MAX_CONCURRENCY: usize = 32;
/// Default number of whole spawn waves allowed to await admission.
pub const DEFAULT_MAX_ADMISSION_QUEUE: usize = 128;

/// CONTROL operation whose generated metadata requires effects authorization.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum EffectsOperation {
	/// Starts a foreground or Core-owned child session.
	SpawnAgent,
	/// Creates or replaces a durable standing authorization.
	ScheduleUpsert,
	/// Requests paid constrained inference.
	Completion,
}

/// Enforces the shared `EFFECTS_AUTHORIZED` minimum phase for CONTROL effects.
///
/// Wire responders map [`SpawnRefusal::MinimumPhase`] to
/// `SPAWN_REFUSAL_MINIMUM_PHASE`; all three operations deliberately use the
/// same refusal so hooks cannot spend or spawn speculatively.
pub fn enforce_minimum_phase(
	phase: InvocationPhase,
	_: EffectsOperation,
) -> Result<(), SpawnRefusal> {
	if phase.allows_operation(InvocationPhase::EffectsAuthorized) {
		Ok(())
	} else {
		Err(SpawnRefusal::MinimumPhase)
	}
}

/// Stable classification of a roster node.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display, strum::EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum AgentKind {
	/// The interactive session root.
	Main,
	/// A child admitted through subagent spawning.
	Subagent,
}

/// Lifecycle state stored in each roster node without allocating on reads.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display, strum::EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum AgentStatus {
	/// Admitted but not currently submitting a turn.
	Pending   = 0,
	/// A turn is actively consuming a concurrency permit.
	Running   = 1,
	/// Idle and available for steering.
	Settled   = 2,
	/// Successfully terminal.
	Completed = 3,
	/// Terminal with an error.
	Failed    = 4,
	/// Terminal after cancellation.
	Cancelled = 5,
	/// Terminal after a hard budget or deadline ceiling.
	Exhausted = 6,
}

impl AgentStatus {
	/// Decodes the compact atomic representation, treating corrupt values as
	/// failed.
	#[must_use]
	pub const fn from_atomic(value: u8) -> Self {
		match value {
			0 => Self::Pending,
			1 => Self::Running,
			2 => Self::Settled,
			3 => Self::Completed,
			4 => Self::Failed,
			5 => Self::Cancelled,
			6 => Self::Exhausted,
			_ => Self::Failed,
		}
	}

	/// Reports whether this status cannot receive another turn.
	#[must_use]
	pub const fn terminal(self) -> bool {
		matches!(self, Self::Completed | Self::Failed | Self::Cancelled | Self::Exhausted)
	}
}

/// Durable usage totals used for hard subtree budget checks.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Usage {
	/// Submitted provider requests.
	pub requests:      u64,
	/// Metered input tokens, including the inference-owned cache policy.
	pub input_tokens:  u64,
	/// Output and reasoning tokens.
	pub output_tokens: u64,
	/// Cost in micros of USD from durable turn receipts only.
	pub usd_micros:    u64,
}

impl Usage {
	fn saturating_add(self, right: Self) -> Self {
		Self {
			requests:      self.requests.saturating_add(right.requests),
			input_tokens:  self.input_tokens.saturating_add(right.input_tokens),
			output_tokens: self.output_tokens.saturating_add(right.output_tokens),
			usd_micros:    self.usd_micros.saturating_add(right.usd_micros),
		}
	}
}

/// Hard ceilings for an agent and every descendant.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Budget {
	/// Maximum subtree provider requests.
	pub max_requests:      Option<u64>,
	/// Maximum subtree metered input tokens.
	pub max_input_tokens:  Option<u64>,
	/// Maximum subtree output and reasoning tokens.
	pub max_output_tokens: Option<u64>,
	/// Maximum subtree durable receipt spend in micros of USD.
	pub max_usd_micros:    Option<u64>,
	/// Maximum duration from admission to settlement.
	pub max_wall:          Option<std::time::Duration>,
}

impl Budget {
	/// Clamps this budget to the unspent remainder represented by `parent`.
	#[must_use]
	pub fn clamped_to(self, parent: BudgetRemainder) -> Self {
		Self {
			max_requests:      clamp(self.max_requests, parent.requests),
			max_input_tokens:  clamp(self.max_input_tokens, parent.input_tokens),
			max_output_tokens: clamp(self.max_output_tokens, parent.output_tokens),
			max_usd_micros:    clamp(self.max_usd_micros, parent.usd_micros),
			max_wall:          match (self.max_wall, parent.wall) {
				(Some(child), Some(ancestor)) => Some(child.min(ancestor)),
				(None, value) => value,
				(value, None) => value,
			},
		}
	}
}

fn clamp(child: Option<u64>, parent: Option<u64>) -> Option<u64> {
	match (child, parent) {
		(Some(child), Some(parent)) => Some(child.min(parent)),
		(None, value) => value,
		(value, None) => value,
	}
}

/// Remaining capacity at one point in an ancestor chain.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BudgetRemainder {
	/// Remaining requests.
	pub requests:      Option<u64>,
	/// Remaining input tokens.
	pub input_tokens:  Option<u64>,
	/// Remaining output tokens.
	pub output_tokens: Option<u64>,
	/// Remaining durable-receipt spend.
	pub usd_micros:    Option<u64>,
	/// Remaining wall time.
	pub wall:          Option<std::time::Duration>,
}

#[derive(Debug)]
struct BudgetAccount {
	budget:      Budget,
	usage:       Usage,
	admitted_at: Instant,
}

impl BudgetAccount {
	fn remainder(&self) -> BudgetRemainder {
		BudgetRemainder {
			requests:      self
				.budget
				.max_requests
				.map(|cap| cap.saturating_sub(self.usage.requests)),
			input_tokens:  self
				.budget
				.max_input_tokens
				.map(|cap| cap.saturating_sub(self.usage.input_tokens)),
			output_tokens: self
				.budget
				.max_output_tokens
				.map(|cap| cap.saturating_sub(self.usage.output_tokens)),
			usd_micros:    self
				.budget
				.max_usd_micros
				.map(|cap| cap.saturating_sub(self.usage.usd_micros)),
			wall:          self
				.budget
				.max_wall
				.map(|cap| cap.saturating_sub(self.admitted_at.elapsed())),
		}
	}

	fn permits(&self, next: Usage) -> Result<(), BudgetCeiling> {
		let total = self.usage.saturating_add(next);
		if self
			.budget
			.max_requests
			.is_some_and(|cap| total.requests > cap)
		{
			return Err(BudgetCeiling::Requests);
		}
		if self
			.budget
			.max_input_tokens
			.is_some_and(|cap| total.input_tokens > cap)
		{
			return Err(BudgetCeiling::InputTokens);
		}
		if self
			.budget
			.max_output_tokens
			.is_some_and(|cap| total.output_tokens > cap)
		{
			return Err(BudgetCeiling::OutputTokens);
		}
		if self
			.budget
			.max_usd_micros
			.is_some_and(|cap| total.usd_micros > cap)
		{
			return Err(BudgetCeiling::Usd);
		}
		if self
			.budget
			.max_wall
			.is_some_and(|cap| self.admitted_at.elapsed() > cap)
		{
			return Err(BudgetCeiling::Wall);
		}
		Ok(())
	}
}

/// One roster node retained for the life of its session.
pub struct AgentNode {
	/// Stable agent identity.
	pub id:      Str,
	/// Session-unique display and routing name.
	pub name:    Str,
	/// Whether this is the root or a spawned child.
	pub kind:    AgentKind,
	/// Parent identity, absent only for the root.
	pub parent:  Option<Str>,
	/// Tree depth, with root at zero.
	pub depth:   u16,
	/// Session identity owning this journal.
	pub session: Str,
	status:      AtomicU8,
	activity:    Mutex<Str>,
	budget:      Mutex<BudgetAccount>,
}

impl AgentNode {
	/// Returns this node's allocation-free lifecycle state.
	#[must_use]
	pub fn status(&self) -> AgentStatus {
		AgentStatus::from_atomic(self.status.load(Ordering::Acquire))
	}

	/// Publishes a lifecycle state.
	pub fn set_status(&self, status: AgentStatus) {
		self.status.store(status as u8, Ordering::Release);
	}

	/// Replaces the short roster activity text.
	pub fn set_activity(&self, activity: Str) {
		*self.activity.lock() = activity;
	}

	/// Returns a clone of the latest roster activity text.
	#[must_use]
	pub fn activity(&self) -> Str {
		self.activity.lock().clone()
	}

	/// Returns direct durable-receipt usage for this node.
	#[must_use]
	pub fn usage(&self) -> Usage {
		self.budget.lock().usage
	}
}

/// Reason a spawn wave could not be admitted.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum SpawnRefusal {
	/// The requested parent was absent or terminal.
	#[error("parent agent is unavailable")]
	ParentGone,
	/// The requested child would exceed the tree depth ceiling.
	#[error("agent depth ceiling exceeded")]
	DepthExceeded,
	/// CONTROL effects were invoked before `EFFECTS_AUTHORIZED`.
	#[error("SPAWN_REFUSAL_MINIMUM_PHASE")]
	MinimumPhase,
	/// The whole spawn wave cannot fit in the bounded admission queue.
	#[error(
		"agent concurrency exhausted (running={running}, queued={queued}, max={max_concurrency})"
	)]
	ConcurrencyExhausted {
		/// Turns holding concurrency permits.
		running:         usize,
		/// Spawn-wave slots already awaiting permits.
		queued:          usize,
		/// Tree-wide concurrency ceiling.
		max_concurrency: usize,
	},
}

/// Ceiling which rejected a request before it reached a provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum BudgetCeiling {
	/// Request count would exceed its cap.
	Requests,
	/// Input tokens would exceed their cap.
	InputTokens,
	/// Output tokens would exceed their cap.
	OutputTokens,
	/// Durable receipt spend would exceed its cap.
	Usd,
	/// Admission-to-settlement duration exceeded its cap.
	Wall,
}

/// Budget pre-dispatch rejection for a node or any ancestor.
#[derive(Debug, Error, Eq, PartialEq)]
#[error("agent budget exhausted: {ceiling}")]
pub struct BudgetExceeded {
	/// The first ancestor ceiling crossed by the proposed request.
	pub ceiling: BudgetCeiling,
}

/// RAII reservation for a complete spawn wave or an active agent turn.
///
/// Dropping it releases every held permit. A waiter must call
/// [`Self::release_for_wait`] before awaiting a child and [`Self::reacquire`]
/// afterwards; this is the release-while-waiting accounting rule.
pub struct SpawnPermit {
	semaphore: Arc<tokio::sync::Semaphore>,
	held:      Option<tokio::sync::OwnedSemaphorePermit>,
	units:     u32,
}

impl SpawnPermit {
	/// Releases this agent's active-turn capacity before waiting on a child.
	pub fn release_for_wait(&mut self) {
		let _ = self.held.take();
	}

	/// Re-acquires the same capacity after a child wait completes.
	///
	/// # Panics
	/// Panics only when an internal semaphore is closed, which this tree never
	/// does.
	pub async fn reacquire(&mut self) {
		if self.held.is_none() {
			self.held = Some(
				Arc::clone(&self.semaphore)
					.acquire_many_owned(self.units)
					.await
					.expect("agent tree semaphore is never closed"),
			);
		}
	}

	/// Runs `future` without holding this agent's turn permit, then restores it.
	pub async fn wait<F: Future>(&mut self, future: F) -> F::Output {
		self.release_for_wait();
		let output = future.await;
		self.reacquire().await;
		output
	}

	/// Returns how many concurrency units this reservation represents.
	#[must_use]
	pub const fn units(&self) -> u32 {
		self.units
	}
}

/// Session-scoped append-only roster and resource authority.
pub struct AgentTree {
	nodes:           AppendVec<Arc<AgentNode>>,
	by_id:           RwLock<HashMap<Str, usize>>,
	by_name:         RwLock<HashMap<Str, usize>>,
	permits:         Arc<tokio::sync::Semaphore>,
	max_depth:       u16,
	max_concurrency: usize,
	max_queue:       usize,
	queued:          AtomicUsize,
}

impl AgentTree {
	/// Creates an empty tree with explicit depth, concurrency, and queue
	/// ceilings.
	#[must_use]
	pub fn new(max_depth: u16, max_concurrency: usize, max_queue: usize) -> Self {
		let max_concurrency = max_concurrency.max(1);
		Self {
			nodes: AppendVec::new(),
			by_id: RwLock::new(HashMap::new()),
			by_name: RwLock::new(HashMap::new()),
			permits: Arc::new(tokio::sync::Semaphore::new(max_concurrency)),
			max_depth,
			max_concurrency,
			max_queue,
			queued: AtomicUsize::new(0),
		}
	}

	/// Creates a tree with the standard session ceilings.
	#[must_use]
	pub fn standard(max_depth: u16) -> Self {
		Self::new(max_depth, DEFAULT_MAX_CONCURRENCY, DEFAULT_MAX_ADMISSION_QUEUE)
	}

	/// Adds a root or admitted child to the append-only roster.
	pub fn register(
		&self,
		id: Str,
		name: Str,
		kind: AgentKind,
		parent: Option<Str>,
		session: Str,
		budget: Budget,
	) -> Result<Arc<AgentNode>, SpawnRefusal> {
		let depth = match parent.as_ref() {
			Some(parent) => self
				.node(parent)
				.ok_or(SpawnRefusal::ParentGone)?
				.depth
				.saturating_add(1),
			None => 0,
		};
		if depth > self.max_depth {
			return Err(SpawnRefusal::DepthExceeded);
		}
		let node = Arc::new(AgentNode {
			id: id.clone(),
			name: name.clone(),
			kind,
			parent,
			depth,
			session,
			status: AtomicU8::new(AgentStatus::Pending as u8),
			activity: Mutex::new(Str::new_static("")),
			budget: Mutex::new(BudgetAccount {
				budget,
				usage: Usage::default(),
				admitted_at: Instant::now(),
			}),
		});
		let index = self.nodes.push(Arc::clone(&node));
		self.by_id.write().insert(id, index);
		self.by_name.write().insert(name, index);
		Ok(node)
	}

	/// Returns a node by stable identity without scanning the roster.
	#[must_use]
	pub fn node(&self, id: &str) -> Option<Arc<AgentNode>> {
		let index = *self.by_id.read().get(id)?;
		self.nodes.get(index).cloned()
	}

	/// Returns a node by session-local name without scanning the roster.
	#[must_use]
	pub fn named(&self, name: &str) -> Option<Arc<AgentNode>> {
		let index = *self.by_name.read().get(name)?;
		self.nodes.get(index).cloned()
	}

	/// Iterates the append-only roster in admission order.
	pub fn roster(&self) -> impl Iterator<Item = &Arc<AgentNode>> {
		self.nodes.iter()
	}

	/// Reserves an entire spawn wave, queuing it as one unit when saturated.
	///
	/// A queue overflow refuses the whole wave before any member can start.
	pub async fn admit(&self, count: usize) -> Result<SpawnPermit, SpawnRefusal> {
		let count = u32::try_from(count).unwrap_or(u32::MAX);
		let slots = usize::try_from(count).unwrap_or(usize::MAX);
		if count == 0 || slots > self.max_concurrency {
			return Err(self.concurrency_refusal());
		}
		let queued = self.queued.fetch_add(slots, Ordering::AcqRel);
		if queued.saturating_add(slots) > self.max_queue {
			self.queued.fetch_sub(slots, Ordering::AcqRel);
			return Err(self.concurrency_refusal());
		}
		let permit = Arc::clone(&self.permits)
			.acquire_many_owned(count)
			.await
			.expect("agent tree semaphore is never closed");
		self.queued.fetch_sub(slots, Ordering::AcqRel);
		Ok(SpawnPermit {
			semaphore: Arc::clone(&self.permits),
			held:      Some(permit),
			units:     count,
		})
	}

	/// Checks all ancestor ceilings before dispatch and records receipt-backed
	/// usage.
	///
	/// Callers must pass only usage committed by a durable receipt; telemetry is
	/// intentionally not an input to this method.
	pub fn debit_receipt(&self, node_id: &str, usage: Usage) -> Result<(), BudgetExceeded> {
		let mut lineage = Vec::new();
		let mut current = self
			.node(node_id)
			.ok_or(BudgetExceeded { ceiling: BudgetCeiling::Requests })?;
		loop {
			lineage.push(Arc::clone(&current));
			let Some(parent) = current.parent.as_ref() else {
				break;
			};
			current = self
				.node(parent)
				.ok_or(BudgetExceeded { ceiling: BudgetCeiling::Requests })?;
		}
		lineage.reverse();
		let mut accounts = lineage
			.iter()
			.map(|node| node.budget.lock())
			.collect::<Vec<_>>();
		for account in &accounts {
			account
				.permits(usage)
				.map_err(|ceiling| BudgetExceeded { ceiling })?;
		}
		for account in &mut accounts {
			account.usage = account.usage.saturating_add(usage);
		}
		Ok(())
	}

	/// Clamps a child's requested budget against every ancestor's unspent
	/// remainder.
	pub fn clamp_budget(&self, parent_id: &str, requested: Budget) -> Result<Budget, SpawnRefusal> {
		let mut effective = requested;
		let mut current = self.node(parent_id).ok_or(SpawnRefusal::ParentGone)?;
		loop {
			effective = effective.clamped_to(current.budget.lock().remainder());
			let Some(parent) = current.parent.as_ref() else {
				break;
			};
			current = self.node(parent).ok_or(SpawnRefusal::ParentGone)?;
		}
		Ok(effective)
	}

	/// Returns the tree-wide concurrency ceiling.
	#[must_use]
	pub const fn max_concurrency(&self) -> usize {
		self.max_concurrency
	}

	fn concurrency_refusal(&self) -> SpawnRefusal {
		SpawnRefusal::ConcurrencyExhausted {
			running:         self
				.max_concurrency
				.saturating_sub(self.permits.available_permits()),
			queued:          self.queued.load(Ordering::Acquire),
			max_concurrency: self.max_concurrency,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn permit_is_released_while_waiting() {
		let tree = AgentTree::new(2, 1, 2);
		let mut parent = tree.admit(1).await.unwrap();
		parent
			.wait(async {
				drop(tree.admit(1).await.unwrap());
			})
			.await;
	}

	#[test]
	fn child_budget_clamps_to_ancestor_remainder() {
		let tree = AgentTree::standard(2);
		tree
			.register(
				Str::from("root"),
				Str::from("Main"),
				AgentKind::Main,
				None,
				Str::from("s"),
				Budget { max_requests: Some(4), ..Budget::default() },
			)
			.unwrap();
		tree
			.debit_receipt("root", Usage { requests: 3, ..Usage::default() })
			.unwrap();
		assert_eq!(
			tree
				.clamp_budget("root", Budget { max_requests: Some(9), ..Budget::default() })
				.unwrap()
				.max_requests,
			Some(1)
		);
	}
}
