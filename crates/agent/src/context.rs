//! Bounded, fail-open context projection over a durable thread.
//!
//! Extensions receive [`ContextView`] metadata and can return [`PatchOp`]s
//! against stable transcript-event ids.  The original journal projection is
//! never mutated.

use std::collections::{HashMap, HashSet};

use omp_core::{Str, sf};
use omp_proto::thread::v1::{self as thread, Item, Thread};
use smallvec::SmallVec;
use thiserror::Error;

/// Compact flags describing an item without transferring its body.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(transparent)]
pub struct RefFlags(u8);

impl RefFlags {
	/// The item is an elided representation.
	pub const ELIDED: Self = Self(1 << 3);
	/// The item represents an error result.
	pub const IS_ERROR: Self = Self(1);
	/// The item cannot be changed by a projection patch.
	pub const PINNED: Self = Self(1 << 2);
	/// The item was marked useless by the producing tool.
	pub const USELESS: Self = Self(1 << 1);

	/// Returns whether this flag set contains `flag`.
	#[must_use]
	pub const fn contains(self, flag: Self) -> bool {
		self.0 & flag.0 != 0
	}
}

/// Body-free metadata for one projected thread item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageRef {
	/// Stable id: the decimal transcript event index, not the advisory sequence.
	pub id:      Str,
	/// Physical transcript event index.
	pub event:   u64,
	/// Advisory item sequence number.
	pub seq:     u64,
	/// Thread item kind.
	pub kind:    MessageKind,
	/// Optional durable turn identity.
	pub turn_id: Option<Str>,
	/// Compact item flags.
	pub flags:   RefFlags,
}

/// Item kinds visible to a projection handler.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageKind {
	/// A system message.
	System,
	/// A user message.
	User,
	/// A model-issued tool call.
	ToolCall,
	/// A settled tool result.
	ToolResult,
	/// An assistant message.
	Assistant,
	/// Any non-message item.
	Other,
}

/// Immutable metadata view sent to projection handlers.
#[derive(Clone, Debug)]
pub struct ContextView {
	/// Refs in the same order as the projected thread.
	pub refs: SmallVec<MessageRef, 64>,
}

/// A bounded patch operation returned by a projection handler.
#[derive(Clone, Debug)]
pub enum PatchOp {
	/// Remove named items, optionally retaining a minimal placeholder.
	Prune {
		/// Stable projected item ids to remove.
		ids:              SmallVec<Str, 8>,
		/// Whether to leave a minimal replacement marker.
		keep_placeholder: bool,
	},
	/// Remove model-visible parts while retaining the projected item and its
	/// metadata.
	DropParts {
		/// Stable projected item ids whose parts are omitted.
		ids:    SmallVec<Str, 8>,
		/// Journal-only reason for omitting the parts.
		reason: Str,
	},
	/// Replace named items with one synthetic message at the first or last
	/// target.
	Replace {
		/// Stable projected item ids to replace.
		ids:  SmallVec<Str, 8>,
		/// Synthetic replacement text.
		text: Str,
		/// Role assigned to the synthetic replacement.
		role: thread::Role,
		/// Which target supplies the replacement position.
		at:   InheritPosition,
	},
	/// Insert one synthetic message beside a stable item id.
	Insert {
		/// Synthetic inserted text.
		text:   Str,
		/// Position relative to the projection.
		anchor: Anchor,
		/// Role assigned to the synthetic item.
		role:   thread::Role,
		/// Optional handler-local idempotency key.
		dedupe: Option<Str>,
	},
	/// Move named items immediately before `before`, preserving their order.
	Reorder {
		/// Stable projected item ids to move.
		ids:    SmallVec<Str, 8>,
		/// Stable item id immediately following the moved sequence.
		before: Str,
	},
}

/// Position inherited by a replacement item.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InheritPosition {
	/// Use the earliest target position.
	First,
	/// Use the latest target position.
	Last,
}

/// Position for an inserted synthetic item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Anchor {
	/// Immediately before an item.
	Before(Str),
	/// Immediately after an item.
	After(Str),
	/// After prompt-head items and before conversational items.
	Head,
	/// At the end of the projected thread.
	Tail,
}

/// A rejected patch is free: validation finishes before item mutation.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum PatchRejected {
	/// An operation requires an id absent from the current projection.
	#[error("unknown context item {0}")]
	Unknown(Str),
	/// An operation attempted to alter a pinned item.
	#[error("context item {0} is pinned")]
	Pinned(Str),
	/// Two operations target the same item ambiguously.
	#[error("context patch operations conflict at {0}")]
	Conflict(Str),
	/// A replacement or reorder has no target items.
	#[error("context operation requires at least one item")]
	Empty,
	/// A reorder cannot move an item before itself.
	#[error("context reorder cannot use a moved item as its anchor")]
	ReorderAnchor,
}

/// Per-operation result of applying a collected projection patch.
#[derive(Debug)]
pub struct PatchOutcome {
	/// Projected thread after every valid operation has materialized.
	pub thread:  Thread,
	/// Rejected operations paired with their index in the input slice.
	pub dropped: SmallVec<(usize, PatchRejected), 4>,
}

/// Result of deciding whether the expensive handler path is needed.
#[derive(Debug)]
pub enum ContextProjection {
	/// No handler is registered: the input thread is returned untouched and no
	/// refs exist.
	Unchanged(Thread),
	/// Handler path: immutable metadata accompanies the owned thread.
	View {
		/// Original projected thread to patch after handler decisions.
		thread: Thread,
		/// Body-free metadata visible to handlers.
		view:   ContextView,
	},
}

/// Builds the body-free view only when at least one projection handler is
/// registered.
///
/// `event_indexes` is projection order and supplies stable ids.  A short event
/// list is rejected by omitting unmatched tail items, rather than inventing
/// unstable ids.
#[must_use]
pub fn project_context(
	thread: Thread,
	event_indexes: &[u64],
	context_handlers: bool,
) -> ContextProjection {
	if !context_handlers {
		return ContextProjection::Unchanged(thread);
	}
	let refs = thread
		.items
		.iter()
		.zip(event_indexes.iter().copied())
		.map(|(item, event)| MessageRef {
			id: event.to_string().into(),
			event,
			seq: item.seq,
			kind: message_kind(item),
			turn_id: None,
			flags: RefFlags::default(),
		})
		.collect();
	ContextProjection::View { thread, view: ContextView { refs } }
}

/// Applies every valid operation in one materialization pass.
///
/// Validation is per-operation fail-open: rejected operations are returned for
/// journaling while their valid siblings still apply. This supersedes the
/// earlier whole-handler-drop contract; handler execution failure remains
/// whole-handler fail-open, but a handler's invalid operation does not discard
/// its siblings.
#[must_use]
pub fn apply_patches(
	mut thread: Thread,
	view: &ContextView,
	operations: &[PatchOp],
) -> PatchOutcome {
	let indexes: HashMap<&str, usize> = view
		.refs
		.iter()
		.enumerate()
		.map(|(index, reference)| (reference.id.as_str(), index))
		.collect();
	let mut touched = vec![false; thread.items.len()];
	let mut plans = Vec::with_capacity(operations.len());
	let mut seen_dedupe = HashSet::new();
	let mut dropped = SmallVec::new();

	for (operation_index, operation) in operations.iter().enumerate() {
		match validate_operation(operation, view, &indexes, &touched, &seen_dedupe) {
			Ok(plan) => {
				commit_plan(&plan, &mut touched, &mut seen_dedupe);
				plans.push(plan);
			},
			Err(rejection) => dropped.push((operation_index, rejection)),
		}
	}

	let mut slots: Vec<Slot> = (0..thread.items.len()).map(Slot::Keep).collect();
	let mut synthetic = Vec::new();
	for plan in plans {
		apply_plan(&mut slots, &mut synthetic, plan);
	}
	let mut items: Vec<Option<Item>> = thread.items.drain(..).map(Some).collect();
	thread.items = slots
		.into_iter()
		.map(|slot| match slot {
			Slot::Keep(index) => items[index].take().expect("each keep slot is unique"),
			Slot::DropParts(index) => {
				let mut item = items[index].take().expect("each drop-parts slot is unique");
				clear_parts(&mut item);
				item
			},
			Slot::Synth(index) | Slot::Placeholder(index) => synthetic[index].clone(),
		})
		.collect();
	PatchOutcome { thread, dropped }
}

#[derive(Clone, Debug)]
enum Slot {
	Keep(usize),
	DropParts(usize),
	Synth(usize),
	Placeholder(usize),
}

#[derive(Debug)]
enum Plan {
	Prune { indexes: SmallVec<usize, 8>, placeholder: Option<Item> },
	DropParts { indexes: SmallVec<usize, 8> },
	Replace { indexes: SmallVec<usize, 8>, item: Item, at: InheritPosition },
	Insert { index: usize, after: bool, item: Item, dedupe: Option<Str> },
	Reorder { indexes: SmallVec<usize, 8>, before: usize },
	Skip,
}

fn validate_operation(
	operation: &PatchOp,
	view: &ContextView,
	indexes: &HashMap<&str, usize>,
	touched: &[bool],
	seen_dedupe: &HashSet<Str>,
) -> Result<Plan, PatchRejected> {
	let resolve = |id: &Str, required: bool| match indexes.get(id.as_str()).copied() {
		Some(index) => Ok(Some(index)),
		None if required => Err(PatchRejected::Unknown(id.clone())),
		None => Ok(None),
	};
	let claim = |ids: &[Str], required: bool| -> Result<SmallVec<usize, 8>, PatchRejected> {
		let mut resolved = SmallVec::with_capacity(ids.len());
		for id in ids {
			if let Some(index) = resolve(id, required)? {
				if view.refs[index].flags.contains(RefFlags::PINNED) {
					return Err(PatchRejected::Pinned(id.clone()));
				}
				if touched[index] || resolved.contains(&index) {
					return Err(PatchRejected::Conflict(id.clone()));
				}
				resolved.push(index);
			}
		}
		if required && resolved.is_empty() {
			Err(PatchRejected::Empty)
		} else {
			Ok(resolved)
		}
	};
	match operation {
		PatchOp::Prune { ids, keep_placeholder } => {
			let resolved = claim(ids, false)?;
			Ok(Plan::Prune {
				indexes:     resolved,
				placeholder: keep_placeholder.then(placeholder_item),
			})
		},
		PatchOp::DropParts { ids, reason: _ } => Ok(Plan::DropParts { indexes: claim(ids, true)? }),
		PatchOp::Replace { ids, text, role, at } => Ok(Plan::Replace {
			indexes: claim(ids, true)?,
			item:    synthetic_item(text.clone(), *role),
			at:      *at,
		}),
		PatchOp::Insert { text, anchor, role, dedupe } => {
			if dedupe.as_ref().is_some_and(|key| seen_dedupe.contains(key)) {
				return Ok(Plan::Skip);
			}
			let (index, after) = match anchor {
				Anchor::Before(id) => (resolve(id, true)?.expect("required"), false),
				Anchor::After(id) => (resolve(id, true)?.expect("required"), true),
				Anchor::Head => (0, false),
				Anchor::Tail => (view.refs.len(), false),
			};
			Ok(Plan::Insert {
				index,
				after,
				item: synthetic_item(text.clone(), *role),
				dedupe: dedupe.clone(),
			})
		},
		PatchOp::Reorder { ids, before } => {
			let moved = claim(ids, true)?;
			let before_index = resolve(before, true)?.expect("required");
			if moved.contains(&before_index) {
				return Err(PatchRejected::ReorderAnchor);
			}
			if touched[before_index] {
				return Err(PatchRejected::Conflict(before.clone()));
			}
			Ok(Plan::Reorder { indexes: moved, before: before_index })
		},
	}
}

fn commit_plan(plan: &Plan, touched: &mut [bool], seen_dedupe: &mut HashSet<Str>) {
	match plan {
		Plan::Prune { indexes, .. } | Plan::DropParts { indexes } | Plan::Replace { indexes, .. } => {
			for index in indexes {
				touched[*index] = true;
			}
		},
		Plan::Insert { dedupe: Some(key), .. } => {
			seen_dedupe.insert(key.clone());
		},
		Plan::Reorder { indexes, before } => {
			for index in indexes {
				touched[*index] = true;
			}
			touched[*before] = true;
		},
		Plan::Insert { dedupe: None, .. } | Plan::Skip => {},
	}
}

fn apply_plan(slots: &mut Vec<Slot>, synthetic: &mut Vec<Item>, plan: Plan) {
	match plan {
		Plan::Prune { indexes, placeholder } => {
			for index in indexes {
				if let Some(position) = slots
					.iter()
					.position(|slot| matches!(slot, Slot::Keep(value) if *value == index))
				{
					slots.remove(position);
					if let Some(item) = placeholder.as_ref() {
						let synthetic_index = synthetic.len();
						synthetic.push(item.clone());
						slots.insert(position, Slot::Placeholder(synthetic_index));
					}
				}
			}
		},
		Plan::DropParts { indexes } => {
			for index in indexes {
				let position = slots
					.iter()
					.position(|slot| matches!(slot, Slot::Keep(value) if *value == index))
					.expect("drop-parts target survives validation");
				slots[position] = Slot::DropParts(index);
			}
		},
		Plan::Replace { indexes, item, at } => {
			let positions: Vec<_> = slots
				.iter()
				.enumerate()
				.filter_map(|(position, slot)| {
					matches!(slot, Slot::Keep(value) if indexes.contains(value)).then_some(position)
				})
				.collect();
			let position = if at == InheritPosition::First {
				positions[0]
			} else {
				*positions.last().expect("nonempty replacement")
			};
			slots.retain(|slot| !matches!(slot, Slot::Keep(value) if indexes.contains(value)));
			let synthetic_index = synthetic.len();
			synthetic.push(item);
			slots.insert(position.min(slots.len()), Slot::Synth(synthetic_index));
		},
		Plan::Insert { index, after, item, dedupe: _ } => {
			let position = slots
				.iter()
				.position(|slot| {
					matches!(
						slot,
						Slot::Keep(value) | Slot::DropParts(value) if *value == index
					)
				})
				.map_or(slots.len(), |position| position + usize::from(after));
			let synthetic_index = synthetic.len();
			synthetic.push(item);
			slots.insert(position, Slot::Synth(synthetic_index));
		},
		Plan::Reorder { indexes, before } => {
			let moved: Vec<_> = slots
				.iter()
				.filter(|slot| matches!(slot, Slot::Keep(value) if indexes.contains(value)))
				.cloned()
				.collect();
			slots.retain(|slot| !matches!(slot, Slot::Keep(value) if indexes.contains(value)));
			let position = slots
				.iter()
				.position(|slot| matches!(slot, Slot::Keep(value) if *value == before))
				.expect("anchor survives validation");
			slots.splice(position..position, moved);
		},
		Plan::Skip => {},
	}
}

fn message_kind(item: &Item) -> MessageKind {
	match item.kind.as_ref() {
		Some(thread::item::Kind::Message(message)) => match thread::Role::try_from(message.role).ok()
		{
			Some(thread::Role::System) => MessageKind::System,
			Some(thread::Role::User) => MessageKind::User,
			Some(thread::Role::Assistant) => MessageKind::Assistant,
			_ => MessageKind::Other,
		},
		Some(thread::item::Kind::ToolCall(_)) => MessageKind::ToolCall,
		Some(thread::item::Kind::ToolResult(_)) => MessageKind::ToolResult,
		None => MessageKind::Other,
	}
}
fn clear_parts(item: &mut Item) {
	match item.kind.as_mut() {
		Some(thread::item::Kind::Message(message)) => message.parts.clear(),
		Some(thread::item::Kind::ToolResult(result)) => result.parts.clear(),
		Some(thread::item::Kind::ToolCall(_)) | None => {},
	}
}

fn synthetic_item(text: Str, role: thread::Role) -> Item {
	Item {
		seq:           0,
		created_at_ms: 0,
		kind:          Some(thread::item::Kind::Message(thread::Message {
			role:  role as i32,
			parts: vec![thread::Part { kind: Some(thread::part::Kind::Text(text.into())) }],
		})),
		props:         None,
	}
}

fn placeholder_item() -> Item {
	synthetic_item(sf!("[tool result omitted by context projection]"), thread::Role::User)
}

#[cfg(test)]
mod tests {
	use std::collections::BTreeMap;

	use omp_proto::inference::v1 as pb;

	use super::*;

	fn item(text: &str) -> Item {
		synthetic_item(text.into(), thread::Role::User)
	}
	fn projected() -> (Thread, ContextView) {
		let thread = Thread { items: vec![item("a"), item("b"), item("c")] };
		let view = match project_context(thread.clone(), &[10, 11, 12], true) {
			ContextProjection::View { view, .. } => view,
			ContextProjection::Unchanged(_) => unreachable!(),
		};
		(thread, view)
	}
	fn texts(thread: &Thread) -> Vec<&str> {
		thread
			.items
			.iter()
			.map(|item| match item.kind.as_ref().expect("item kind") {
				thread::item::Kind::Message(message) => {
					match message.parts[0].kind.as_ref().expect("part kind") {
						thread::part::Kind::Text(text) => text.as_str(),
						_ => "",
					}
				},
				_ => "",
			})
			.collect()
	}

	#[test]
	fn invalid_middle_operation_drops_independently() {
		let (thread, view) = projected();
		let outcome = apply_patches(thread, &view, &[
			PatchOp::Prune {
				ids:              ["10".into()].into_iter().collect(),
				keep_placeholder: false,
			},
			PatchOp::Replace {
				ids:  ["missing".into()].into_iter().collect(),
				text: "x".into(),
				role: thread::Role::User,
				at:   InheritPosition::First,
			},
			PatchOp::Replace {
				ids:  ["12".into()].into_iter().collect(),
				text: "z".into(),
				role: thread::Role::User,
				at:   InheritPosition::First,
			},
		]);

		assert_eq!(texts(&outcome.thread), ["b", "z"]);
		assert_eq!(outcome.dropped.len(), 1);
		assert_eq!(outcome.dropped[0].0, 1);
		assert!(matches!(
			&outcome.dropped[0].1,
			PatchRejected::Unknown(id) if id.as_str() == "missing"
		));
	}

	#[test]
	fn rejected_operation_does_not_commit_touched_or_dedupe_claims() {
		let (thread, view) = projected();
		let touched = apply_patches(thread, &view, &[
			PatchOp::Reorder { ids: ["10".into()].into_iter().collect(), before: "10".into() },
			PatchOp::Prune {
				ids:              ["10".into()].into_iter().collect(),
				keep_placeholder: false,
			},
		]);
		assert_eq!(texts(&touched.thread), ["b", "c"]);
		assert!(matches!(touched.dropped.as_slice(), [(0, PatchRejected::ReorderAnchor)]));

		let (thread, view) = projected();
		let dedupe = apply_patches(thread, &view, &[
			PatchOp::Insert {
				text:   "invalid".into(),
				anchor: Anchor::After("missing".into()),
				role:   thread::Role::User,
				dedupe: Some("same-key".into()),
			},
			PatchOp::Insert {
				text:   "valid".into(),
				anchor: Anchor::After("10".into()),
				role:   thread::Role::User,
				dedupe: Some("same-key".into()),
			},
		]);
		assert_eq!(texts(&dedupe.thread), ["a", "valid", "b", "c"]);
		assert_eq!(dedupe.dropped.len(), 1);
		assert_eq!(dedupe.dropped[0].0, 0);
		assert!(matches!(dedupe.dropped[0].1, PatchRejected::Unknown(_)));
	}

	#[test]
	fn drop_parts_preserves_item_metadata() {
		let details = pb::Value { kind: Some(pb::value::Kind::String("details".to_owned())) };
		let props = pb::ValueMap {
			fields: BTreeMap::from([("omp/tool-rev".to_owned(), pb::Value {
				kind: Some(pb::value::Kind::String("read.1".to_owned())),
			})]),
		};
		let provider_metadata = pb::ValueMap {
			fields: BTreeMap::from([("provider/key".to_owned(), pb::Value {
				kind: Some(pb::value::Kind::String("verbatim".to_owned())),
			})]),
		};
		let tool_result = Item {
			seq:           7,
			created_at_ms: 99,
			kind:          Some(thread::item::Kind::ToolResult(thread::ToolResult {
				call_id: "call-1".to_owned(),
				parts: vec![thread::Part {
					kind: Some(thread::part::Kind::Text("large result".to_owned())),
				}],
				is_error: true,
				name: "read".to_owned(),
				details: Some(details),
				pruned_at_ms: Some(42),
				useless: Some(true),
				provider_metadata: Some(provider_metadata),
				..Default::default()
			})),
			props:         Some(props),
		};
		let message = Item {
			seq:           8,
			created_at_ms: 100,
			kind:          Some(thread::item::Kind::Message(thread::Message {
				role:  thread::Role::Assistant as i32,
				parts: vec![thread::Part {
					kind: Some(thread::part::Kind::Text("assistant text".to_owned())),
				}],
			})),
			props:         None,
		};
		let thread = Thread { items: vec![tool_result, message] };
		let view = match project_context(thread.clone(), &[10, 11], true) {
			ContextProjection::View { view, .. } => view,
			ContextProjection::Unchanged(_) => unreachable!(),
		};
		let mut expected = thread.clone();
		clear_parts(&mut expected.items[0]);
		clear_parts(&mut expected.items[1]);

		let outcome = apply_patches(thread, &view, &[PatchOp::DropParts {
			ids:    ["10".into(), "11".into()].into_iter().collect(),
			reason: "projection budget".into(),
		}]);

		assert!(outcome.dropped.is_empty());
		assert_eq!(outcome.thread, expected);
	}

	#[test]
	fn pinned_drop_parts_is_rejected_without_mutation() {
		let (thread, mut view) = projected();
		view.refs[0].flags = RefFlags::PINNED;
		let expected = thread.clone();

		let outcome = apply_patches(thread, &view, &[PatchOp::DropParts {
			ids:    ["10".into()].into_iter().collect(),
			reason: "must survive".into(),
		}]);

		assert_eq!(outcome.thread, expected);
		assert!(matches!(
			outcome.dropped.as_slice(),
			[(0, PatchRejected::Pinned(id))] if id.as_str() == "10"
		));
	}

	#[test]
	fn reorder_moves_named_items_once() {
		let (thread, view) = projected();
		let outcome = apply_patches(thread, &view, &[PatchOp::Reorder {
			ids:    ["12".into()].into_iter().collect(),
			before: "10".into(),
		}]);
		assert!(outcome.dropped.is_empty());
		assert_eq!(texts(&outcome.thread), ["c", "a", "b"]);
	}

	#[test]
	fn zero_handler_keeps_thread_allocation() {
		let thread = Thread { items: vec![item("a")] };
		let pointer = thread.items.as_ptr();
		match project_context(thread, &[10], false) {
			ContextProjection::Unchanged(thread) => assert_eq!(pointer, thread.items.as_ptr()),
			ContextProjection::View { .. } => panic!("refs must not be constructed"),
		}
	}
}
