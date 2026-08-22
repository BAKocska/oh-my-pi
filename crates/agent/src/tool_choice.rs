//! Future-turn tool-choice directives with explicit claim settlement.
//!
//! A directive is claimed only while constructing a new model request.  The
//! queue therefore never mutates or reorders a tool-call batch that the model
//! has already emitted.

use std::{collections::VecDeque, future::Future, pin::Pin, sync::Arc};

use omp_core::{Str, sf};
use omp_llm_inference::call::ToolChoice;
use serde_json::Value;

/// Why a claimed choice could not be settled successfully.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum RejectReason {
	/// The turn was aborted.
	Aborted,
	/// The turn failed.
	Error,
	/// The queue was cleared.
	Cleared,
	/// The directive was explicitly removed.
	Removed,
	/// The selected tool was not available in the request manifest.
	Unavailable,
	/// The directive-owned invoker was not called by the model.
	NotInvoked,
}

/// Resolution selected by a directive after a rejected claim.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RejectOutcome {
	/// Replay exactly the rejected yield before any later directive.
	Requeue,
	/// Drop this yield and retain the rest of its sequence.
	#[default]
	Drop,
	/// Drop this yield and every remaining yield from its sequence.
	DropSequence,
}

/// Whether a directive enters before or after already queued future turns.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DirectivePriority {
	/// Serve before directives already waiting.
	Head,
	/// Serve after directives already waiting.
	#[default]
	Tail,
}

/// Information supplied after a choice settles normally.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolveInfo {
	/// Choice served to the model.
	pub choice: ToolChoice,
}

/// Information supplied when a claimed choice is rejected.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RejectInfo {
	/// Choice that was claimed.
	pub choice: ToolChoice,
	/// Rejection cause.
	pub reason: RejectReason,
}

/// Future returned by a directive-owned tool invoker.
pub type InvokeFuture = Pin<Box<dyn Future<Output = Value> + Send + 'static>>;
/// Handler that bypasses a registered tool executor for a directive-owned call.
pub type Invoker = Arc<dyn Fn(Value) -> InvokeFuture + Send + Sync + 'static>;
/// Successful-settlement notification.
pub type ResolveHandler = Arc<dyn Fn(ResolveInfo) + Send + Sync + 'static>;
/// Rejection policy callback.
pub type RejectHandler = Arc<dyn Fn(RejectInfo) -> RejectOutcome + Send + Sync + 'static>;

/// Lifecycle callbacks attached to every yield of one directive.
#[derive(Clone, Default)]
pub struct DirectiveCallbacks {
	/// Called after the choice settles successfully.
	pub on_resolved: Option<ResolveHandler>,
	/// Selects the disposition of a rejected choice.
	pub on_rejected: Option<RejectHandler>,
	/// Optional executor for the forced tool call.
	pub on_invoked:  Option<Invoker>,
}

/// Options shared by all directive producers.
#[derive(Clone, Default)]
pub struct PushOptions {
	/// Queue placement for this future directive.
	pub priority:  DirectivePriority,
	/// Stable label for observation and targeted removal.
	pub label:     Option<Str>,
	/// Lifecycle callbacks.
	pub callbacks: DirectiveCallbacks,
}

enum ChoiceGenerator {
	Once(Option<ToolChoice>),
	Sequence(std::vec::IntoIter<ToolChoice>),
	Custom(Box<dyn Iterator<Item = ToolChoice> + Send + 'static>),
}

impl Iterator for ChoiceGenerator {
	type Item = ToolChoice;

	fn next(&mut self) -> Option<Self::Item> {
		match self {
			Self::Once(choice) => choice.take(),
			Self::Sequence(choices) => choices.next(),
			Self::Custom(choices) => choices.next(),
		}
	}
}

struct Directive {
	generator: ChoiceGenerator,
	label:     Str,
	callbacks: DirectiveCallbacks,
	root:      u64,
}

struct InFlight {
	directive_root: u64,
	label:          Str,
	choice:         ToolChoice,
	callbacks:      DirectiveCallbacks,
	invoked:        bool,
}

struct PendingInvoker {
	id:          Str,
	source_tool: Str,
	invoker:     Invoker,
}

/// Metadata for the most recently registered pending preview invoker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingInvokerHead<'a> {
	/// Unique preview identity.
	pub id:          &'a str,
	/// Tool that staged the preview.
	pub source_tool: &'a str,
}

/// Queue of tool-choice directives claimed strictly for future model requests.
///
/// At most one yield may be in flight.  Callers must resolve or reject that
/// claim before requesting another, which prevents retries and stream
/// completion paths from advancing a generator twice.
#[derive(Default)]
pub struct ToolChoiceQueue {
	queue:               VecDeque<Directive>,
	in_flight:           Option<InFlight>,
	last_resolved_label: Option<Str>,
	pending_invokers:    Vec<PendingInvoker>,
	next_root:           u64,
}

impl ToolChoiceQueue {
	/// Creates an empty queue.
	pub const fn new() -> Self {
		Self {
			queue:               VecDeque::new(),
			in_flight:           None,
			last_resolved_label: None,
			pending_invokers:    Vec::new(),
			next_root:           0,
		}
	}

	/// Queues one choice.
	pub fn push_once(&mut self, choice: ToolChoice, options: PushOptions) {
		self.push_inner(ChoiceGenerator::Once(Some(choice)), options);
	}

	/// Queues a finite choice sequence without erasing its iterator.
	pub fn push_sequence(&mut self, choices: Vec<ToolChoice>, options: PushOptions) {
		self.push_inner(ChoiceGenerator::Sequence(choices.into_iter()), options);
	}

	/// Queues a lazy, potentially stateful generator.
	///
	/// Type erasure allocates once when the directive is registered, never per
	/// yield or per stream event.
	pub fn push_generator<I>(&mut self, choices: I, options: PushOptions)
	where
		I: IntoIterator<Item = ToolChoice>,
		I::IntoIter: Send + 'static,
	{
		self.push_inner(ChoiceGenerator::Custom(Box::new(choices.into_iter())), options);
	}

	/// Claims the next choice for a not-yet-emitted model request.
	///
	/// Returns `None` while another claim awaits settlement.
	pub fn claim_next(&mut self) -> Option<ToolChoice> {
		if self.in_flight.is_some() {
			return None;
		}
		loop {
			let head = self.queue.front_mut()?;
			if let Some(choice) = head.generator.next() {
				self.in_flight = Some(InFlight {
					directive_root: head.root,
					label:          head.label.clone(),
					choice:         choice.clone(),
					callbacks:      head.callbacks.clone(),
					invoked:        false,
				});
				return Some(choice);
			}
			self.queue.pop_front();
		}
	}

	/// Settles the active claim.
	///
	/// A directive with an invoker resolves only after that invoker was
	/// dispatched; otherwise it follows the `NotInvoked` rejection policy.
	pub fn resolve(&mut self) {
		let Some(in_flight) = self.in_flight.take() else {
			return;
		};
		if in_flight.callbacks.on_invoked.is_some() && !in_flight.invoked {
			self.reject_claim(in_flight, RejectReason::NotInvoked);
			return;
		}
		self.last_resolved_label = Some(in_flight.label);
		if let Some(callback) = in_flight.callbacks.on_resolved {
			callback(ResolveInfo { choice: in_flight.choice });
		}
	}

	/// Rejects the active claim and applies its callback-selected disposition.
	pub fn reject(&mut self, reason: RejectReason) {
		if let Some(in_flight) = self.in_flight.take() {
			self.reject_claim(in_flight, reason);
		}
	}

	/// Returns whether a choice is awaiting settlement.
	pub const fn has_in_flight(&self) -> bool {
		self.in_flight.is_some()
	}

	/// Dispatches the active directive-owned invoker.
	///
	/// Merely inspecting queue state never marks the directive invoked.
	pub fn invoke_in_flight(&mut self, input: Value) -> Option<InvokeFuture> {
		let in_flight = self.in_flight.as_mut()?;
		let invoker = in_flight.callbacks.on_invoked.as_ref()?;
		in_flight.invoked = true;
		Some(invoker(input))
	}

	/// Registers or replaces one uniquely identified, non-forcing preview
	/// invoker.
	pub fn register_pending_invoker(&mut self, id: Str, source_tool: Str, invoker: Invoker) {
		self.remove_pending_invoker(id.as_str());
		self
			.pending_invokers
			.push(PendingInvoker { id, source_tool, invoker });
	}

	/// Removes one pending preview invoker by exact identity.
	pub fn remove_pending_invoker(&mut self, id: &str) {
		self.pending_invokers.retain(|pending| pending.id != id);
	}

	/// Removes every non-forcing preview invoker without changing directives.
	pub fn clear_pending_invokers(&mut self) {
		self.pending_invokers.clear();
	}

	/// Returns whether at least one preview invoker is pending.
	pub fn has_pending_invoker(&self) -> bool {
		!self.pending_invokers.is_empty()
	}

	/// Dispatches the most recently registered non-forcing preview invoker.
	pub fn invoke_pending(&self, input: Value) -> Option<InvokeFuture> {
		self
			.pending_invokers
			.last()
			.map(|pending| (pending.invoker)(input))
	}

	/// Returns metadata for the most recently registered pending invoker.
	pub fn pending_head(&self) -> Option<PendingInvokerHead<'_>> {
		self
			.pending_invokers
			.last()
			.map(|pending| PendingInvokerHead {
				id:          pending.id.as_str(),
				source_tool: pending.source_tool.as_str(),
			})
	}

	/// Removes queued directives with `label`, rejecting a matching claim first.
	pub fn remove_by_label(&mut self, label: &str) {
		if self
			.in_flight
			.as_ref()
			.is_some_and(|claim| claim.label == label)
		{
			self.reject(RejectReason::Removed);
		}
		self.queue.retain(|directive| directive.label != label);
	}

	/// Clears directives, the active claim, previews, and observations.
	pub fn clear(&mut self) {
		self.reject(RejectReason::Cleared);
		self.queue.clear();
		self.pending_invokers.clear();
		self.last_resolved_label = None;
	}

	/// Returns and clears the most recently resolved directive label.
	pub fn consume_last_resolved_label(&mut self) -> Option<Str> {
		self.last_resolved_label.take()
	}

	/// Iterates queued labels in service order without allocating.
	pub fn labels(&self) -> impl ExactSizeIterator<Item = &str> {
		self.queue.iter().map(|directive| directive.label.as_str())
	}

	/// Returns the number of directive generators retained in service order.
	pub fn len(&self) -> usize {
		self.queue.len()
	}

	/// Returns whether no directives or active claim remain.
	pub fn is_empty(&self) -> bool {
		self.queue.is_empty() && self.in_flight.is_none()
	}

	fn push_inner(&mut self, generator: ChoiceGenerator, options: PushOptions) {
		let root = self.next_root;
		self.next_root = self.next_root.saturating_add(1);
		let directive = Directive {
			generator,
			label: options.label.unwrap_or_else(|| sf!("anonymous")),
			callbacks: options.callbacks,
			root,
		};
		match options.priority {
			DirectivePriority::Head => self.queue.push_front(directive),
			DirectivePriority::Tail => self.queue.push_back(directive),
		}
	}

	fn reject_claim(&mut self, in_flight: InFlight, reason: RejectReason) {
		let outcome = in_flight
			.callbacks
			.on_rejected
			.as_ref()
			.map_or(RejectOutcome::Drop, |callback| {
				callback(RejectInfo { choice: in_flight.choice.clone(), reason })
			});
		match outcome {
			RejectOutcome::Drop => {},
			RejectOutcome::DropSequence => {
				self
					.queue
					.retain(|directive| directive.root != in_flight.directive_root);
			},
			RejectOutcome::Requeue => {
				self.queue.push_front(Directive {
					generator: ChoiceGenerator::Once(Some(in_flight.choice)),
					label:     sf!("{}-requeued", in_flight.label),
					callbacks: in_flight.callbacks,
					root:      in_flight.directive_root,
				});
			},
		}
	}
}
