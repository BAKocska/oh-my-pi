use std::sync::Arc;

use omp_agent::{CanonicalPromptSource, SlotClass, SlotDecl, SlotId, render_prompt};
use omp_core::Str;
use omp_driver::{
	prompt_head::ProductionPromptHead,
	rulebook::{PromptHeadAuthority, PromptInvalidationError},
};
use omp_scribe::Props;

fn declaration(slot: SlotId, class: SlotClass, owner: &str) -> SlotDecl {
	SlotDecl { slot, class, owner: Str::from(owner), priority: 0 }
}

fn authority() -> ProductionPromptHead {
	ProductionPromptHead::new(vec![
		declaration(SlotId::Memory, SlotClass::Epochal, "fixture.extension"),
		declaration(SlotId::Runtime, SlotClass::Frozen, "fixture.extension"),
		declaration(SlotId::Rules, SlotClass::Stable, "other.extension"),
	])
}

#[tokio::test]
async fn accepted_invalidation_advances_the_consumed_generation() {
	let authority = authority();
	let generations = authority.generation_store();

	assert_eq!(generations.generation(11, SlotId::Memory), 0);
	assert_eq!(
		authority
			.invalidate("fixture.extension", 11, "memory")
			.await,
		Ok(1)
	);
	assert_eq!(generations.generation(11, SlotId::Memory), 1);
	assert_eq!(
		authority
			.invalidate("fixture.extension", 11, "memory")
			.await,
		Ok(2)
	);
	assert_eq!(generations.generation(11, SlotId::Memory), 2);
}

#[tokio::test]
async fn accepted_generation_changes_the_prompt_cache_hash_not_wire_items() {
	let authority = authority();
	let source = authority.wrap_prompt_source(Arc::new(CanonicalPromptSource));
	let props = Props::new();
	let canonical = render_prompt(&CanonicalPromptSource, &props).expect("canonical prompt renders");
	let before = render_prompt(source.as_ref(), &props).expect("initial prompt renders");
	assert_eq!(before, canonical);

	authority
		.invalidate("fixture.extension", 11, "memory")
		.await
		.expect("declared writable contribution accepts invalidation");
	let after = render_prompt(source.as_ref(), &props).expect("invalidated prompt renders");

	assert_eq!(after.items, before.items);
	assert_ne!(after.hash, before.hash);
}

#[tokio::test]
async fn declaration_rejections_are_typed_and_do_not_advance_generation() {
	let authority = authority();
	let generations = authority.generation_store();

	assert_eq!(
		authority
			.invalidate("fixture.extension", 11, "missing")
			.await,
		Err(PromptInvalidationError::UnknownSlot)
	);
	assert_eq!(
		authority
			.invalidate("fixture.extension", 11, "runtime")
			.await,
		Err(PromptInvalidationError::FrozenSlot)
	);
	assert_eq!(
		authority.invalidate("fixture.extension", 11, "rules").await,
		Err(PromptInvalidationError::NotOwner)
	);
	assert_eq!(generations.generation(11, SlotId::Runtime), 0);
	assert_eq!(generations.generation(11, SlotId::Rules), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_invalidation_is_serialized_without_lost_generations() {
	const INVALIDATIONS: u64 = 64;

	let authority = Arc::new(authority());
	let generations = authority.generation_store();
	let mut tasks = Vec::with_capacity(INVALIDATIONS as usize);
	for _ in 0..INVALIDATIONS {
		let authority = Arc::clone(&authority);
		tasks.push(tokio::spawn(async move {
			authority
				.invalidate("fixture.extension", 11, "memory")
				.await
				.expect("declared writable contribution accepts invalidation")
		}));
	}

	let mut observed = Vec::with_capacity(INVALIDATIONS as usize);
	for task in tasks {
		observed.push(task.await.expect("invalidation task completes"));
	}
	observed.sort_unstable();

	assert_eq!(observed, (1..=INVALIDATIONS).collect::<Vec<_>>());
	assert_eq!(generations.generation(11, SlotId::Memory), INVALIDATIONS);
}
