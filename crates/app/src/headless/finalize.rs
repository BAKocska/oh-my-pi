//! Ordered bounded finalization for non-interactive session owners.

use std::{future::Future, pin::Pin, time::Duration};

use tokio::io::{AsyncWrite, AsyncWriteExt as _};

/// Ordered finalization phase which exceeded its budget.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
#[repr(u8)]
pub enum FinalizerPhase {
	/// Advisor catch-up.
	Advisor,
	/// Mnemopi consolidation.
	Mnemopi,
	/// Serialized stdout flush.
	Stdout,
	/// Telemetry drain.
	Telemetry,
}

/// Time bounds applied to ordered headless finalization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FinalizerBudget {
	/// Advisor catch-up bound.
	pub advisor:   Duration,
	/// Mnemopi consolidation bound.
	pub mnemopi:   Duration,
	/// Stdout flush bound.
	pub stdout:    Duration,
	/// Telemetry drain bound.
	pub telemetry: Duration,
}

impl FinalizerBudget {
	/// Normal successful completion: ten minutes for advisor catch-up and the
	/// configured Mnemopi consolidation bound.
	#[must_use]
	pub const fn success(mnemopi: Duration) -> Self {
		Self {
			advisor: Duration::from_secs(600),
			mnemopi,
			stdout: Duration::from_secs(30),
			telemetry: Duration::from_secs(30),
		}
	}

	/// Terminal failure: every remaining phase is bounded by thirty seconds.
	#[must_use]
	pub const fn terminal_error() -> Self {
		Self {
			advisor:   Duration::from_secs(30),
			mnemopi:   Duration::from_secs(30),
			stdout:    Duration::from_secs(30),
			telemetry: Duration::from_secs(30),
		}
	}
}

type FinalizerFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
type FinalizerAction = Box<dyn FnOnce() -> FinalizerFuture + Send + Sync + 'static>;

/// Result of best-effort ordered finalization.
#[derive(Debug, Default)]
pub struct FinalizerReport {
	/// Phases cancelled after exceeding their bound, in execution order.
	pub timed_out:    Vec<FinalizerPhase>,
	/// Typed stdout flush failure, if flushing completed unsuccessfully.
	pub stdout_error: Option<std::io::Error>,
}

/// Session-owned advisor, memory, stdout, and telemetry drain actions.
///
/// Actions are boxed once at authority registration because these cold paths
/// cross independently owned runtime types. They are invoked at most once.
#[derive(Default)]
pub struct HeadlessFinalizerHandle {
	advisor:   Option<FinalizerAction>,
	mnemopi:   Option<FinalizerAction>,
	telemetry: Option<FinalizerAction>,
}

impl HeadlessFinalizerHandle {
	/// Creates an empty finalizer. Disabled authorities therefore complete
	/// immediately without sleeps.
	#[must_use]
	pub const fn new() -> Self {
		Self { advisor: None, mnemopi: None, telemetry: None }
	}

	/// Registers the advisor catch-up action.
	pub fn set_advisor<F, Fut>(&mut self, action: F)
	where
		F: FnOnce() -> Fut + Send + Sync + 'static,
		Fut: Future<Output = ()> + Send + 'static,
	{
		self.advisor = Some(Box::new(|| Box::pin(action())));
	}

	/// Registers the enabled Mnemopi consolidation action.
	pub fn set_mnemopi<F, Fut>(&mut self, action: F)
	where
		F: FnOnce() -> Fut + Send + Sync + 'static,
		Fut: Future<Output = ()> + Send + 'static,
	{
		self.mnemopi = Some(Box::new(|| Box::pin(action())));
	}

	/// Registers the telemetry drain action.
	pub fn set_telemetry<F, Fut>(&mut self, action: F)
	where
		F: FnOnce() -> Fut + Send + Sync + 'static,
		Fut: Future<Output = ()> + Send + 'static,
	{
		self.telemetry = Some(Box::new(|| Box::pin(action())));
	}

	/// Drains advisor, Mnemopi, stdout, and telemetry in that exact order.
	/// Session and Environment disposal remains the caller's final step after
	/// this future returns.
	pub async fn finalize<W>(mut self, stdout: &mut W, budget: FinalizerBudget) -> FinalizerReport
	where
		W: AsyncWrite + Unpin,
	{
		let mut report = FinalizerReport::default();
		run_action(self.advisor.take(), budget.advisor, FinalizerPhase::Advisor, &mut report).await;
		run_action(self.mnemopi.take(), budget.mnemopi, FinalizerPhase::Mnemopi, &mut report).await;
		match tokio::time::timeout(budget.stdout, stdout.flush()).await {
			Ok(Ok(())) => {},
			Ok(Err(error)) => report.stdout_error = Some(error),
			Err(_) => report.timed_out.push(FinalizerPhase::Stdout),
		}
		run_action(self.telemetry.take(), budget.telemetry, FinalizerPhase::Telemetry, &mut report)
			.await;
		report
	}
}

async fn run_action(
	action: Option<FinalizerAction>,
	budget: Duration,
	phase: FinalizerPhase,
	report: &mut FinalizerReport,
) {
	let Some(action) = action else {
		return;
	};
	if tokio::time::timeout(budget, action()).await.is_err() {
		report.timed_out.push(phase);
	}
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;

	use parking_lot::Mutex;

	use super::*;

	#[tokio::test]
	async fn finalizer_runs_authorities_in_order() {
		let order = Arc::new(Mutex::new(Vec::new()));
		let mut handle = HeadlessFinalizerHandle::new();
		let advisor = Arc::clone(&order);
		handle.set_advisor(move || async move { advisor.lock().push(FinalizerPhase::Advisor) });
		let mnemopi = Arc::clone(&order);
		handle.set_mnemopi(move || async move { mnemopi.lock().push(FinalizerPhase::Mnemopi) });
		let telemetry = Arc::clone(&order);
		handle.set_telemetry(move || async move { telemetry.lock().push(FinalizerPhase::Telemetry) });
		let mut stdout = tokio::io::sink();
		let report = handle
			.finalize(&mut stdout, FinalizerBudget::success(Duration::from_secs(1)))
			.await;
		assert!(report.timed_out.is_empty());
		assert!(report.stdout_error.is_none());
		assert_eq!(*order.lock(), [
			FinalizerPhase::Advisor,
			FinalizerPhase::Mnemopi,
			FinalizerPhase::Telemetry,
		]);
	}

	#[tokio::test(start_paused = true)]
	async fn timed_out_authority_does_not_skip_later_phases() {
		let telemetry_ran = Arc::new(Mutex::new(false));
		let mut handle = HeadlessFinalizerHandle::new();
		handle.set_advisor(|| std::future::pending());
		let observed = Arc::clone(&telemetry_ran);
		handle.set_telemetry(move || async move { *observed.lock() = true });
		let mut stdout = tokio::io::sink();
		let report = handle
			.finalize(&mut stdout, FinalizerBudget {
				advisor:   Duration::from_millis(1),
				mnemopi:   Duration::from_millis(1),
				stdout:    Duration::from_millis(1),
				telemetry: Duration::from_millis(1),
			})
			.await;
		assert_eq!(report.timed_out, [FinalizerPhase::Advisor]);
		assert!(*telemetry_ran.lock());
	}
}
