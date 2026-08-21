//! Behavioral tests for the `cached` attribute macro.

use std::sync::atomic::{AtomicUsize, Ordering};

static PLAIN_CALLS: AtomicUsize = AtomicUsize::new(0);
static RESULT_CALLS: AtomicUsize = AtomicUsize::new(0);

#[omp_macros::cached(size = 4)]
fn doubled(value: u32) -> u32 {
	PLAIN_CALLS.fetch_add(1, Ordering::Relaxed);
	value * 2
}

#[omp_macros::cached(size = 4, result = true, name = "NAMED_RESULT_CACHE")]
fn checked(value: i32) -> Result<i32, &'static str> {
	RESULT_CALLS.fetch_add(1, Ordering::Relaxed);
	if value < 0 {
		Err("negative")
	} else {
		Ok(value + 1)
	}
}

#[test]
fn plain_values_are_memoized() {
	assert_eq!(doubled(21), 42);
	assert_eq!(doubled(21), 42);
	assert_eq!(PLAIN_CALLS.load(Ordering::Relaxed), 1);
}

#[test]
fn result_successes_are_memoized_and_errors_are_not() {
	assert_eq!(checked(2), Ok(3));
	assert_eq!(checked(2), Ok(3));
	assert_eq!(RESULT_CALLS.load(Ordering::Relaxed), 1);

	assert_eq!(checked(-1), Err("negative"));
	assert_eq!(checked(-1), Err("negative"));
	assert_eq!(RESULT_CALLS.load(Ordering::Relaxed), 3);
}
