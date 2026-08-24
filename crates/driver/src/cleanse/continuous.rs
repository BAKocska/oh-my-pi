//! Continuous file-owned cleanse dispatch.

use std::{
	collections::{BTreeMap, HashSet},
	future::Future,
};

use futures::{StreamExt as _, stream::FuturesUnordered};
use omp_core::Str;
use tokio_util::sync::CancellationToken;

use super::{
	Assignment, CleanseHost, Diagnostic, FileIssues, RepairOutcome, Report,
	balance::group_by_file,
	checkers::{CheckerRunner, Suite, diagnostic_key, run_suite_streaming},
};

/// Result of one streamed checker and repair pass before verification.
pub(super) struct ContinuousRun {
	pub(super) report:    Report,
	pub(super) workers:   usize,
	pub(super) cancelled: bool,
}

#[derive(Clone)]
struct Owner {
	worker:    usize,
	followups: flume::Sender<Vec<Diagnostic>>,
}

struct WorkerFinished<E> {
	worker:     usize,
	assignment: Assignment,
	outcome:    Result<RepairOutcome, E>,
}

/// Streams checker findings into a bounded, file-owned repair pool.
pub(super) async fn dispatch<H: CleanseHost>(
	host: &H,
	suite: &Suite,
	model: &str,
	max_agents: usize,
	cancel: &CancellationToken,
) -> Result<ContinuousRun, <H as CheckerRunner>::Error> {
	let (diagnostic_tx, diagnostic_rx) = flume::unbounded();
	let collection =
		run_suite_streaming(host.project_root(), suite, host, cancel, Some(diagnostic_tx));
	tokio::pin!(collection);

	let mut collection_result = None;
	let mut seen = HashSet::new();
	let mut pending = BTreeMap::<Option<Str>, Vec<Diagnostic>>::new();
	let mut owned = BTreeMap::<Option<Str>, Owner>::new();
	let mut active = BTreeMap::<usize, Assignment>::new();
	let mut workers = FuturesUnordered::new();
	let mut worker_count = 0;
	let mut dispatch_error = None;
	let mut cancelled = false;

	loop {
		while !cancelled
			&& dispatch_error.is_none()
			&& !pending.is_empty()
			&& workers.len() < max_agents
		{
			let groups = take_batch(&mut pending, max_agents);
			worker_count += 1;
			let worker = worker_count;
			let assignment = Assignment {
				index: worker - 1,
				weight: groups.iter().map(|group| group.weight).sum(),
				groups,
			};
			let peers = active.values().cloned().collect::<Vec<_>>();
			let (followup_tx, followup_rx) = flume::unbounded();
			for group in &assignment.groups {
				owned.insert(group.file.clone(), Owner { worker, followups: followup_tx.clone() });
			}
			active.insert(worker, assignment.clone());
			workers.push(worker_future(host, assignment, worker, peers, model, followup_rx, cancel));
		}

		let collection_done = collection_result.is_some();
		let drained = collection_done
			&& workers.is_empty()
			&& (pending.is_empty() || cancelled || dispatch_error.is_some());
		if drained {
			break;
		}

		tokio::select! {
			biased;
			() = cancel.cancelled(), if !cancelled => {
				cancelled = true;
			},
			result = &mut collection, if !collection_done => {
				if let Ok(report) = &result {
					route_diagnostics(
						&report.diagnostics,
						&mut seen,
						&mut pending,
						&owned,
					);
				}
				collection_result = Some(result);
			},
			batch = diagnostic_rx.recv_async(), if !collection_done => {
				if let Ok(batch) = batch {
					route_diagnostics(&batch, &mut seen, &mut pending, &owned);
				}
			},
			finished = workers.next(), if !workers.is_empty() => {
				if let Some(finished) = finished {
					active.remove(&finished.worker);
					for group in &finished.assignment.groups {
						if owned.get(&group.file).is_some_and(|owner| owner.worker == finished.worker) {
							owned.remove(&group.file);
						}
					}
					match finished.outcome {
						Ok(_) => {},
						Err(error) => {
							dispatch_error.get_or_insert(error);
						},
					}
				}
			},
		}
	}

	if collection_result.is_none() {
		collection_result = Some(collection.await);
	}
	while let Some(finished) = workers.next().await {
		match finished.outcome {
			Ok(_) => {},
			Err(error) => {
				dispatch_error.get_or_insert(error);
			},
		}
	}
	if cancelled {
		return Ok(ContinuousRun {
			report:    collection_result.and_then(Result::ok).unwrap_or_default(),
			workers:   worker_count,
			cancelled: true,
		});
	}
	if let Some(error) = dispatch_error {
		return Err(error);
	}
	let report = collection_result.expect("collection settled before dispatch drain")?;
	Ok(ContinuousRun { report, workers: worker_count, cancelled: false })
}

fn worker_future<'a, H: CleanseHost>(
	host: &'a H,
	assignment: Assignment,
	worker: usize,
	peers: Vec<Assignment>,
	model: &'a str,
	followups: flume::Receiver<Vec<Diagnostic>>,
	cancel: &'a CancellationToken,
) -> impl Future<Output = WorkerFinished<<H as CheckerRunner>::Error>> + Send + 'a {
	let outcome = host.repair_worker(assignment.clone(), worker, peers, model, followups, cancel);
	async move { WorkerFinished { worker, assignment, outcome: outcome.await } }
}

fn route_diagnostics(
	diagnostics: &[Diagnostic],
	seen: &mut HashSet<Str>,
	pending: &mut BTreeMap<Option<Str>, Vec<Diagnostic>>,
	owned: &BTreeMap<Option<Str>, Owner>,
) {
	for diagnostic in diagnostics {
		if !seen.insert(diagnostic_key(diagnostic)) {
			continue;
		}
		if let Some(owner) = owned.get(&diagnostic.file)
			&& owner.followups.send(vec![diagnostic.clone()]).is_ok()
		{
			continue;
		}
		pending
			.entry(diagnostic.file.clone())
			.or_default()
			.push(diagnostic.clone());
	}
}

fn take_batch(
	pending: &mut BTreeMap<Option<Str>, Vec<Diagnostic>>,
	max_agents: usize,
) -> Vec<FileIssues> {
	let diagnostics = pending.values().flatten().cloned().collect::<Vec<_>>();
	let groups = group_by_file(&diagnostics);
	let total = groups.iter().map(|group| group.weight).sum::<u64>();
	let budget = total.div_ceil(max_agents as u64).max(1);
	let mut batch = Vec::new();
	let mut weight = 0_u64;
	for group in groups {
		if !batch.is_empty() && weight >= budget {
			break;
		}
		weight = weight.saturating_add(group.weight);
		pending.remove(&group.file);
		batch.push(group);
	}
	batch
}

#[cfg(test)]
mod tests {
	use std::{
		path::{Path, PathBuf},
		sync::{
			Arc,
			atomic::{AtomicBool, AtomicUsize, Ordering},
		},
	};

	use futures::future::BoxFuture;
	use omp_core::sf;
	use parking_lot::Mutex;
	use tokio::sync::{Barrier, Notify};

	use super::*;
	use crate::cleanse::{
		BinaryResolver, Checker, CheckerEffect, ProcessOutput, TargetChoice, parsers::ParserKind,
	};

	#[derive(Debug, thiserror::Error)]
	#[error("test cleanse failure")]
	struct TestError;

	type CheckerFn = dyn Fn(
			Checker,
			Option<flume::Sender<ProcessOutput>>,
			CancellationToken,
		) -> BoxFuture<'static, Result<ProcessOutput, TestError>>
		+ Send
		+ Sync;
	type WorkerFn = dyn Fn(
			Assignment,
			usize,
			Vec<Assignment>,
			flume::Receiver<Vec<Diagnostic>>,
			CancellationToken,
		) -> BoxFuture<'static, Result<RepairOutcome, TestError>>
		+ Send
		+ Sync;

	struct TestHost {
		root:    PathBuf,
		checker: Arc<CheckerFn>,
		worker:  Arc<WorkerFn>,
	}

	impl BinaryResolver for TestHost {
		fn resolve(&self, _: &Path, _: &Path, _: &[&str]) -> Option<PathBuf> {
			None
		}
	}

	impl CheckerRunner for TestHost {
		type Error = TestError;

		fn run_checker(
			&self,
			checker: &Checker,
			cancel: &CancellationToken,
			partials: Option<flume::Sender<ProcessOutput>>,
		) -> impl Future<Output = Result<ProcessOutput, Self::Error>> + Send {
			(self.checker)(checker.clone(), partials, cancel.clone())
		}
	}

	impl CleanseHost for TestHost {
		fn project_root(&self) -> &Path {
			&self.root
		}

		fn project_files(&self) -> &[PathBuf] {
			&[]
		}

		fn pick_target(
			&self,
			_: &[Checker],
			_: &CancellationToken,
		) -> impl Future<Output = Result<TargetChoice, Self::Error>> {
			std::future::ready(Ok(TargetChoice::All))
		}

		fn prompt_request(
			&self,
			_: &CancellationToken,
		) -> impl Future<Output = Result<Option<Str>, Self::Error>> {
			std::future::ready(Ok(None))
		}

		fn discover_custom(
			&self,
			_: &str,
			_: &str,
			_: &CancellationToken,
		) -> impl Future<Output = Result<Str, Self::Error>> + Send {
			std::future::ready(Ok(sf!("[]")))
		}

		fn repair_worker(
			&self,
			assignment: Assignment,
			worker: usize,
			peers: Vec<Assignment>,
			_: &str,
			followups: flume::Receiver<Vec<Diagnostic>>,
			cancel: &CancellationToken,
		) -> impl Future<Output = Result<RepairOutcome, Self::Error>> + Send {
			(self.worker)(assignment, worker, peers, followups, cancel.clone())
		}

		fn journal_remainder(&self, _: &Report) -> Result<(), Self::Error> {
			Ok(())
		}
	}

	fn checker(id: &str) -> Checker {
		Checker {
			id:       sf!("{id}"),
			label:    sf!("{id}"),
			language: sf!("test"),
			cwd:      PathBuf::from("."),
			binary:   PathBuf::from(id),
			args:     Vec::new(),
			parser:   ParserKind::Generic,
			effect:   CheckerEffect::ReadOnly,
			test:     false,
		}
	}

	fn output(text: &str) -> ProcessOutput {
		ProcessOutput { exit_code: Some(1), stdout: Str::new(text), stderr: sf!("") }
	}

	fn suite(ids: &[&str]) -> Suite {
		Suite { checkers: ids.iter().map(|id| checker(id)).collect(), skipped: Vec::new() }
	}

	fn success(worker: usize) -> RepairOutcome {
		RepairOutcome {
			name:    Str::from(format!("CleanseA{worker}")),
			success: true,
			output:  sf!(""),
		}
	}

	#[tokio::test]
	async fn checker_interleaving_dispatches_before_all_checkers_finish() {
		let first_emitted = Arc::new(Notify::new());
		let worker_started = Arc::new(Notify::new());
		let starts = Arc::new(Mutex::new(Vec::new()));
		let host = TestHost {
			root:    PathBuf::from("."),
			checker: {
				let first_emitted = Arc::clone(&first_emitted);
				let worker_started = Arc::clone(&worker_started);
				Arc::new(move |checker, partials, _| {
					let first_emitted = Arc::clone(&first_emitted);
					let worker_started = Arc::clone(&worker_started);
					Box::pin(async move {
						if checker.id == "a" {
							partials
								.as_ref()
								.unwrap()
								.send(output("a.rs:1:1: error: a\n"))
								.unwrap();
							first_emitted.notify_one();
							worker_started.notified().await;
							Ok(output("a.rs:1:1: error: a\n"))
						} else {
							first_emitted.notified().await;
							partials
								.as_ref()
								.unwrap()
								.send(output("b.rs:1:1: error: b\n"))
								.unwrap();
							Ok(output("b.rs:1:1: error: b\n"))
						}
					})
				})
			},
			worker:  {
				let starts = Arc::clone(&starts);
				let worker_started = Arc::clone(&worker_started);
				Arc::new(move |assignment, worker, _, _, _| {
					starts
						.lock()
						.push(assignment.groups[0].file.clone().unwrap());
					worker_started.notify_one();
					Box::pin(async move { Ok(success(worker)) })
				})
			},
		};
		let result = dispatch(&host, &suite(&["a", "b"]), "@smol", 2, &CancellationToken::new())
			.await
			.unwrap();
		assert_eq!(result.report.checks.len(), 2);
		assert_eq!(starts.lock().len(), 2);
	}

	#[tokio::test]
	async fn same_file_is_steered_to_its_in_flight_owner() {
		let worker_started = Arc::new(Notify::new());
		let steered = Arc::new(Mutex::new(Vec::new()));
		let host = TestHost {
			root:    PathBuf::from("."),
			checker: {
				let worker_started = Arc::clone(&worker_started);
				Arc::new(move |_, partials, _| {
					let worker_started = Arc::clone(&worker_started);
					Box::pin(async move {
						let partials = partials.unwrap();
						partials.send(output("a.rs:1:1: error: first\n")).unwrap();
						worker_started.notified().await;
						partials
							.send(output("a.rs:1:1: error: first\na.rs:2:1: error: late\n"))
							.unwrap();
						Ok(output("a.rs:1:1: error: first\na.rs:2:1: error: late\n"))
					})
				})
			},
			worker:  {
				let worker_started = Arc::clone(&worker_started);
				let steered = Arc::clone(&steered);
				Arc::new(move |_, worker, _, followups, _| {
					let steered = Arc::clone(&steered);
					worker_started.notify_one();
					Box::pin(async move {
						let diagnostics = followups.recv_async().await.unwrap();
						steered.lock().extend(diagnostics);
						Ok(success(worker))
					})
				})
			},
		};
		let result = dispatch(&host, &suite(&["one"]), "@smol", 2, &CancellationToken::new())
			.await
			.unwrap();
		assert_eq!(result.workers, 1);
		assert_eq!(steered.lock()[0].line, Some(2));
	}

	#[tokio::test]
	async fn different_files_run_concurrently_without_starvation_or_duplicate_ownership() {
		let barrier = Arc::new(Barrier::new(2));
		let active = Arc::new(AtomicUsize::new(0));
		let peak = Arc::new(AtomicUsize::new(0));
		let assigned = Arc::new(Mutex::new(Vec::new()));
		let overlap = Arc::new(AtomicBool::new(false));
		let live_files = Arc::new(Mutex::new(HashSet::new()));
		let host = TestHost {
			root:    PathBuf::from("."),
			checker: Arc::new(|_, partials, _| {
				Box::pin(async move {
					let partials = partials.unwrap();
					for file in ["a.rs", "b.rs", "c.rs"] {
						partials
							.send(output(&format!("{file}:1:1: error: bad\n")))
							.unwrap();
					}
					Ok(output("a.rs:1:1: error: bad\nb.rs:1:1: error: bad\nc.rs:1:1: error: bad\n"))
				})
			}),
			worker:  {
				let barrier = Arc::clone(&barrier);
				let active = Arc::clone(&active);
				let peak = Arc::clone(&peak);
				let assigned = Arc::clone(&assigned);
				let overlap = Arc::clone(&overlap);
				let live_files = Arc::clone(&live_files);
				Arc::new(move |assignment, worker, _, _, _| {
					let barrier = Arc::clone(&barrier);
					let active = Arc::clone(&active);
					let peak = Arc::clone(&peak);
					let assigned = Arc::clone(&assigned);
					let overlap = Arc::clone(&overlap);
					let live_files = Arc::clone(&live_files);
					Box::pin(async move {
						let files = assignment
							.groups
							.iter()
							.filter_map(|group| group.file.clone())
							.collect::<Vec<_>>();
						{
							let mut live = live_files.lock();
							for file in &files {
								if !live.insert(file.clone()) {
									overlap.store(true, Ordering::SeqCst);
								}
							}
						}
						assigned.lock().extend(files.iter().cloned());
						let now = active.fetch_add(1, Ordering::SeqCst) + 1;
						peak.fetch_max(now, Ordering::SeqCst);
						if worker <= 2 {
							barrier.wait().await;
						}
						active.fetch_sub(1, Ordering::SeqCst);
						let mut live = live_files.lock();
						for file in files {
							live.remove(&file);
						}
						Ok(success(worker))
					})
				})
			},
		};
		let result = dispatch(&host, &suite(&["one"]), "@smol", 2, &CancellationToken::new())
			.await
			.unwrap();
		assert_eq!(peak.load(Ordering::SeqCst), 2);
		assert!(!overlap.load(Ordering::SeqCst));
		let mut files = assigned.lock().clone();
		files.sort();
		files.dedup();
		assert_eq!(files, [sf!("a.rs"), sf!("b.rs"), sf!("c.rs")]);
		assert_eq!(result.report.diagnostics.len(), 3);
	}

	#[tokio::test]
	async fn released_file_is_requeued_when_late_steering_cannot_deliver() {
		let released = Arc::new(Notify::new());
		let starts = Arc::new(AtomicUsize::new(0));
		let host = TestHost {
			root:    PathBuf::from("."),
			checker: {
				let released = Arc::clone(&released);
				Arc::new(move |_, partials, _| {
					let released = Arc::clone(&released);
					Box::pin(async move {
						let partials = partials.unwrap();
						partials.send(output("a.rs:1:1: error: first\n")).unwrap();
						released.notified().await;
						partials
							.send(output("a.rs:1:1: error: first\na.rs:2:1: error: late\n"))
							.unwrap();
						Ok(output("a.rs:1:1: error: first\na.rs:2:1: error: late\n"))
					})
				})
			},
			worker:  {
				let released = Arc::clone(&released);
				let starts = Arc::clone(&starts);
				Arc::new(move |_, worker, _, followups, _| {
					starts.fetch_add(1, Ordering::SeqCst);
					drop(followups);
					released.notify_one();
					Box::pin(async move { Ok(success(worker)) })
				})
			},
		};
		let result = dispatch(&host, &suite(&["one"]), "@smol", 2, &CancellationToken::new())
			.await
			.unwrap();
		assert_eq!(result.workers, 2);
		assert_eq!(starts.load(Ordering::SeqCst), 2);
	}

	#[tokio::test]
	async fn cancellation_stops_new_dispatch_and_drains_active_work() {
		let cancel = CancellationToken::new();
		let worker_started = Arc::new(Notify::new());
		let host = TestHost {
			root:    PathBuf::from("."),
			checker: Arc::new(|_, partials, cancel| {
				Box::pin(async move {
					partials
						.unwrap()
						.send(output("a.rs:1:1: error: bad\n"))
						.unwrap();
					cancel.cancelled().await;
					Ok(ProcessOutput {
						exit_code: None,
						stdout:    sf!(""),
						stderr:    sf!("cancelled"),
					})
				})
			}),
			worker:  {
				let worker_started = Arc::clone(&worker_started);
				Arc::new(move |_, worker, _, _, cancel| {
					worker_started.notify_one();
					Box::pin(async move {
						cancel.cancelled().await;
						Ok(success(worker))
					})
				})
			},
		};
		let trigger = {
			let cancel = cancel.clone();
			let worker_started = Arc::clone(&worker_started);
			tokio::spawn(async move {
				worker_started.notified().await;
				cancel.cancel();
			})
		};
		let result = dispatch(&host, &suite(&["one"]), "@smol", 4, &cancel)
			.await
			.unwrap();
		trigger.await.unwrap();
		assert!(result.cancelled);
		assert_eq!(result.workers, 1);
	}

	#[tokio::test]
	async fn checker_failure_is_actionable_and_final_verification_converges() {
		let calls = Arc::new(AtomicUsize::new(0));
		let assigned = Arc::new(Mutex::new(Vec::new()));
		let host = TestHost {
			root:    PathBuf::from("."),
			checker: {
				let calls = Arc::clone(&calls);
				Arc::new(move |_, _, _| {
					let call = calls.fetch_add(1, Ordering::SeqCst);
					Box::pin(async move {
						if call == 0 {
							Ok(output("opaque checker crash"))
						} else {
							Ok(ProcessOutput {
								exit_code: Some(0),
								stdout:    sf!(""),
								stderr:    sf!(""),
							})
						}
					})
				})
			},
			worker:  {
				let assigned = Arc::clone(&assigned);
				Arc::new(move |assignment, worker, _, _, _| {
					assigned.lock().extend(
						assignment
							.groups
							.into_iter()
							.flat_map(|group| group.diagnostics),
					);
					Box::pin(async move { Ok(success(worker)) })
				})
			},
		};
		let suite = suite(&["one"]);
		let first = dispatch(&host, &suite, "@smol", 2, &CancellationToken::new())
			.await
			.unwrap();
		assert_eq!(first.workers, 1);
		assert_eq!(assigned.lock()[0].code.as_deref(), Some("checker-failed"));
		let verified =
			super::run_suite_streaming(Path::new("."), &suite, &host, &CancellationToken::new(), None)
				.await
				.unwrap();
		assert!(verified.diagnostics.is_empty());
	}
}
