//! Asynchronous, incremental validation of streamed edit targets.

use std::{
	collections::{HashMap, HashSet},
	path::{Path, PathBuf},
	sync::{
		Arc,
		atomic::{AtomicU64, Ordering},
	},
	time::{Duration, Instant},
};

use flume::{Receiver, Sender};
use omp_core::Str;
use omp_tool::Rev;
use parking_lot::Mutex;

/// Edit argument dialect understood by the streaming guard.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamingEditDialect {
	/// Hashline sections.
	Hashline,
	/// Old/new replacement operations.
	Replace,
	/// Patch-envelope operations.
	Patch,
	/// Apply-patch envelope operations.
	ApplyPatch,
	/// Sparse/sloppy operations.
	Sloppy,
}

impl StreamingEditDialect {
	/// Resolves a registered edit revision to guard parsing behavior.
	pub fn from_revision(revision: &Rev) -> Option<Self> {
		match revision.family.as_str() {
			"hl" => Some(Self::Hashline),
			"rep" => Some(Self::Replace),
			"patch" => Some(Self::Patch),
			"apply_patch" => Some(Self::ApplyPatch),
			"sloppy" => Some(Self::Sloppy),
			_ => None,
		}
	}
}

/// Early-abort reason emitted by a streaming edit precheck.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamingEditAbort {
	/// Tool call whose streamed arguments failed validation.
	pub call_id: Str,
	/// Authored target path.
	pub path:    Str,
	/// Exact removed line absent from the current target.
	pub missing: Str,
	/// Guard epoch which produced this verdict.
	pub epoch:   u64,
}

#[derive(Debug)]
struct CallState {
	dialect: StreamingEditDialect,
	buffer:  String,
	queued:  HashMap<PathBuf, HashSet<Str>>,
}

#[derive(Debug)]
enum WorkerMessage {
	Check { epoch: u64, call_id: Str, display: Str, path: PathBuf, removed: Vec<Str> },
	Invalidate(PathBuf),
	Reset,
	Barrier(Sender<()>),
}

/// Turn-scoped streaming edit guard with a serialized asynchronous worker.
///
/// Fragment parsing is synchronous and allocation-bounded by the streamed
/// argument buffer. File I/O and removed-line scans run on the worker; reset
/// invalidates queued/in-flight verdicts immediately through an atomic epoch.
#[derive(Debug)]
pub struct StreamingEditGuard {
	cwd:         Arc<PathBuf>,
	enabled:     bool,
	epoch:       Arc<AtomicU64>,
	invalidated: Arc<Mutex<HashSet<(u64, Str)>>>,
	calls:       Mutex<HashMap<Str, CallState>>,
	work_tx:     Sender<WorkerMessage>,
	abort_rx:    Receiver<StreamingEditAbort>,
}

impl StreamingEditGuard {
	/// Starts a guard worker rooted at the session working directory.
	pub fn new(cwd: PathBuf, enabled: bool) -> Self {
		let (work_tx, work_rx) = flume::unbounded();
		let (abort_tx, abort_rx) = flume::unbounded();
		let epoch = Arc::new(AtomicU64::new(0));
		let invalidated = Arc::new(Mutex::new(HashSet::new()));
		tokio::spawn(worker(work_rx, abort_tx, Arc::clone(&epoch), Arc::clone(&invalidated)));
		Self {
			cwd: Arc::new(cwd),
			enabled,
			epoch,
			invalidated,
			calls: Mutex::new(HashMap::new()),
			work_tx,
			abort_rx,
		}
	}

	/// Registers one streamed edit call at its tool-call start event.
	pub fn start(&self, call_id: impl Into<Str>, revision: &Rev) {
		if !self.enabled {
			return;
		}
		let Some(dialect) = StreamingEditDialect::from_revision(revision) else {
			return;
		};
		let call_id = call_id.into();
		self
			.invalidated
			.lock()
			.remove(&(self.epoch.load(Ordering::Acquire), call_id.clone()));
		self.calls.lock().insert(call_id, CallState {
			dialect,
			buffer: String::new(),
			queued: HashMap::new(),
		});
	}

	/// Parses one argument fragment and queues only newly discovered checks.
	pub fn push_fragment(&self, call_id: &str, fragment: &str) {
		if !self.enabled {
			return;
		}
		let epoch = self.epoch.load(Ordering::Acquire);
		let mut calls = self.calls.lock();
		let Some(call) = calls.get_mut(call_id) else {
			return;
		};
		call.buffer.push_str(fragment);
		for target in extract_targets(call.dialect, &call.buffer) {
			let Some(path) = resolve_path(&self.cwd, &target.path) else {
				continue;
			};
			let known = call.queued.entry(path.clone()).or_default();
			let removed = target
				.removed
				.into_iter()
				.filter(|line| !line.is_empty() && known.insert(line.clone()))
				.collect::<Vec<_>>();
			if removed.is_empty() {
				continue;
			}
			let _ = self.work_tx.send(WorkerMessage::Check {
				epoch,
				call_id: Str::new(call_id),
				display: target.path,
				path,
				removed,
			});
		}
	}

	/// Invalidates one cached target after an edit commits.
	pub fn invalidate(&self, path: &str) {
		let Some(path) = resolve_path(&self.cwd, path) else {
			return;
		};
		let _ = self.work_tx.send(WorkerMessage::Invalidate(path));
	}

	/// Synchronously starts a fresh epoch and invalidates every queued verdict.
	///
	/// Call this before any awaited turn-start event fan-out.
	pub fn reset(&self) {
		self.epoch.fetch_add(1, Ordering::AcqRel);
		self.invalidated.lock().clear();
		self.calls.lock().clear();
		let _ = self.work_tx.send(WorkerMessage::Reset);
	}

	/// Invalidates every target discovered for a completed edit call.
	pub fn invalidate_call(&self, call_id: &str) {
		let epoch = self.epoch.load(Ordering::Acquire);
		self.invalidated.lock().insert((epoch, Str::new(call_id)));
		let Some(call) = self.calls.lock().remove(call_id) else {
			return;
		};
		for path in call.queued.into_keys() {
			let _ = self.work_tx.send(WorkerMessage::Invalidate(path));
		}
	}

	/// Receives the next current-epoch abort verdict.
	pub async fn recv_abort(&self) -> Option<StreamingEditAbort> {
		loop {
			let abort = self.abort_rx.recv_async().await.ok()?;
			if abort.epoch == self.epoch.load(Ordering::Acquire)
				&& !call_invalidated(&self.invalidated, abort.epoch, &abort.call_id)
			{
				return Some(abort);
			}
		}
	}

	/// Drains one current-epoch abort verdict without waiting.
	pub fn try_abort(&self) -> Option<StreamingEditAbort> {
		while let Ok(abort) = self.abort_rx.try_recv() {
			if abort.epoch == self.epoch.load(Ordering::Acquire)
				&& !call_invalidated(&self.invalidated, abort.epoch, &abort.call_id)
			{
				return Some(abort);
			}
		}
		None
	}

	/// Waits until every check queued before this call has settled.
	pub async fn settle(&self) {
		let (reply, done) = flume::bounded(1);
		if self
			.work_tx
			.send_async(WorkerMessage::Barrier(reply))
			.await
			.is_ok()
		{
			let _ = done.recv_async().await;
		}
	}
}

#[derive(Debug)]
struct GuardTarget {
	path:    Str,
	removed: Vec<Str>,
}

fn extract_targets(dialect: StreamingEditDialect, buffer: &str) -> Vec<GuardTarget> {
	let document = if dialect == StreamingEditDialect::Replace {
		let Ok(document) = omp_slopjson::parse(buffer) else {
			return Vec::new();
		};
		document
	} else {
		omp_slopjson::parse_streaming(buffer)
	};
	match dialect {
		StreamingEditDialect::Replace => document
			.get("edits")
			.and_then(omp_slopjson::Value::as_array)
			.into_iter()
			.flatten()
			.filter_map(|edit| {
				let path = edit.get("path")?.as_str()?;
				let old = edit.get("old")?.as_str()?;
				Some(GuardTarget {
					path:    Str::new(path),
					removed: old
						.lines()
						.filter(|line| !line.is_empty())
						.map(Str::new)
						.collect(),
				})
			})
			.collect(),
		StreamingEditDialect::Patch | StreamingEditDialect::ApplyPatch => document
			.get("input")
			.and_then(omp_slopjson::Value::as_str)
			.map(extract_patch_targets)
			.unwrap_or_default(),
		StreamingEditDialect::Sloppy => document
			.get("input")
			.and_then(omp_slopjson::Value::as_str)
			.map(extract_sloppy_targets)
			.unwrap_or_default(),
		StreamingEditDialect::Hashline => Vec::new(),
	}
}

fn extract_patch_targets(input: &str) -> Vec<GuardTarget> {
	let mut targets = Vec::<GuardTarget>::new();
	let mut current = None;
	let complete = input.rfind('\n').map_or("", |end| &input[..=end]);
	for line in complete.lines() {
		if let Some(path) = line
			.strip_prefix("*** Update File:")
			.or_else(|| line.strip_prefix("*** Delete File:"))
		{
			targets.push(GuardTarget { path: Str::new(path.trim()), removed: Vec::new() });
			current = Some(targets.len() - 1);
			continue;
		}
		if line.starts_with("*** ") {
			current = None;
			continue;
		}
		if let Some(index) = current
			&& let Some(removed) = line.strip_prefix('-')
			&& !line.starts_with("---")
		{
			targets[index].removed.push(Str::new(removed));
		}
	}
	targets
}

fn extract_sloppy_targets(input: &str) -> Vec<GuardTarget> {
	let mut targets = Vec::<GuardTarget>::new();
	let mut current = None;
	for line in input.lines() {
		let trimmed = line.trim();
		if let Some(path) = trimmed.strip_prefix('§')
			&& !path.is_empty()
			&& path != "*"
		{
			let path = path.strip_prefix('*').unwrap_or(path).trim();
			if !path.is_empty() {
				targets.push(GuardTarget { path: Str::new(path), removed: Vec::new() });
				current = Some(targets.len() - 1);
			}
			continue;
		}
		let Some(index) = current else { continue };
		let mut rest = line;
		while let Some(open) = rest.find('⟪') {
			let selected = &rest[open + '⟪'.len_utf8()..];
			let Some(close) = selected.find('⟫') else {
				break;
			};
			let body = &selected[..close];
			if let Some((old, _)) = body.split_once('│')
				&& !old.is_empty()
			{
				targets[index].removed.push(Str::new(old));
			}
			rest = &selected[close + '⟫'.len_utf8()..];
		}
	}
	targets
}

fn resolve_path(cwd: &Path, authored: &str) -> Option<PathBuf> {
	if authored.contains("://") {
		return None;
	}
	let path = Path::new(authored);
	Some(if path.is_absolute() {
		path.to_path_buf()
	} else {
		cwd.join(path)
	})
}

async fn worker(
	rx: Receiver<WorkerMessage>,
	abort_tx: Sender<StreamingEditAbort>,
	epoch: Arc<AtomicU64>,
	invalidated: Arc<Mutex<HashSet<(u64, Str)>>>,
) {
	let mut cache = HashMap::<PathBuf, Option<Str>>::new();
	let mut confirmed = HashMap::<PathBuf, HashSet<Str>>::new();
	let mut aborted_epoch = None;
	while let Ok(message) = rx.recv_async().await {
		match message {
			WorkerMessage::Barrier(done) => {
				let _ = done.send(());
			},
			WorkerMessage::Reset => {
				cache.clear();
				confirmed.clear();
				aborted_epoch = None;
			},
			WorkerMessage::Invalidate(path) => {
				cache.remove(&path);
				confirmed.remove(&path);
			},
			WorkerMessage::Check { epoch: queued_epoch, call_id, display, path, removed } => {
				if epoch.load(Ordering::Acquire) != queued_epoch
					|| aborted_epoch == Some(queued_epoch)
					|| call_invalidated(&invalidated, queued_epoch, &call_id)
				{
					continue;
				}
				let content = if let Some(content) = cache.get(&path) {
					content.clone()
				} else {
					let loaded = tokio::fs::read_to_string(&path)
						.await
						.ok()
						.map(|text| Str::new(normalize_lf(&text)));
					if epoch.load(Ordering::Acquire) != queued_epoch
						|| call_invalidated(&invalidated, queued_epoch, &call_id)
					{
						continue;
					}
					cache.insert(path.clone(), loaded.clone());
					loaded
				};
				let Some(content) = content else { continue };
				let known = confirmed.entry(path.clone()).or_default();
				let mut slice = Instant::now();
				for line in removed {
					if epoch.load(Ordering::Acquire) != queued_epoch
						|| call_invalidated(&invalidated, queued_epoch, &call_id)
					{
						break;
					}
					let normalized = Str::new(normalize_lf(&line));
					if known.contains(&normalized) {
						continue;
					}
					if !content.contains(normalized.as_str()) {
						aborted_epoch = Some(queued_epoch);
						let _ = abort_tx
							.send_async(StreamingEditAbort {
								call_id: call_id.clone(),
								path:    display.clone(),
								missing: normalized,
								epoch:   queued_epoch,
							})
							.await;
						break;
					}
					known.insert(normalized);
					if slice.elapsed() >= Duration::from_millis(2) {
						tokio::task::yield_now().await;
						if epoch.load(Ordering::Acquire) != queued_epoch
							|| call_invalidated(&invalidated, queued_epoch, &call_id)
						{
							break;
						}
						slice = Instant::now();
					}
				}
			},
		}
	}
}

fn call_invalidated(invalidated: &Mutex<HashSet<(u64, Str)>>, epoch: u64, call_id: &Str) -> bool {
	invalidated.lock().contains(&(epoch, call_id.clone()))
}

fn normalize_lf(text: &str) -> String {
	if text.contains('\r') {
		text.replace("\r\n", "\n").replace('\r', "\n")
	} else {
		text.to_owned()
	}
}

#[cfg(test)]
mod tests {
	use tempfile::tempdir;

	use super::*;

	fn revision(family: &str) -> Rev {
		Rev { family: Str::new(family), n: 1 }
	}

	fn patch_args(path: &str, removed: &str) -> String {
		serde_json::json!({
			"input": format!("*** Begin Patch\n*** Update File: {path}\n@@\n-{removed}\n+replacement\n*** End Patch\n")
		}).to_string()
	}

	#[tokio::test]
	async fn distinguishes_empty_file_from_failed_load() {
		let temp = tempdir().expect("tempdir");
		tokio::fs::write(temp.path().join("empty.txt"), "")
			.await
			.expect("empty");
		let guard = StreamingEditGuard::new(temp.path().to_path_buf(), true);
		guard.start("empty", &revision("apply_patch"));
		guard.push_fragment("empty", &patch_args("empty.txt", "absent"));
		let abort = tokio::time::timeout(Duration::from_secs(1), guard.recv_abort())
			.await
			.expect("verdict timeout")
			.expect("abort");
		assert_eq!(abort.missing, "absent");
	}

	#[tokio::test]
	async fn verifies_incremental_removed_lines_and_invalidates_cache() {
		let temp = tempdir().expect("tempdir");
		let path = temp.path().join("target.txt");
		tokio::fs::write(&path, "alpha\nbeta\n")
			.await
			.expect("source");
		let guard = StreamingEditGuard::new(temp.path().to_path_buf(), true);
		guard.start("first", &revision("apply_patch"));
		guard.push_fragment("first", &patch_args("target.txt", "alpha"));
		guard.settle().await;
		assert!(guard.try_abort().is_none());

		tokio::fs::write(&path, "gamma\n").await.expect("mutated");
		guard.invalidate_call("first");
		guard.start("second", &revision("apply_patch"));
		guard.push_fragment("second", &patch_args("target.txt", "alpha"));
		let abort = tokio::time::timeout(Duration::from_secs(1), guard.recv_abort())
			.await
			.expect("verdict timeout")
			.expect("abort");
		assert_eq!(abort.call_id, "second");
	}

	#[tokio::test]
	async fn completed_call_invalidation_drops_a_stale_async_result() {
		let temp = tempdir().expect("tempdir");
		tokio::fs::write(temp.path().join("target.txt"), "present\n")
			.await
			.expect("source");
		let guard = StreamingEditGuard::new(temp.path().to_path_buf(), true);
		guard.start("done", &revision("apply_patch"));
		guard.push_fragment("done", &patch_args("target.txt", "missing"));
		guard.invalidate_call("done");
		guard.settle().await;
		assert!(guard.try_abort().is_none());
	}

	#[tokio::test]
	async fn reset_epoch_drops_queued_and_in_flight_verdicts_synchronously() {
		let temp = tempdir().expect("tempdir");
		tokio::fs::write(temp.path().join("target.txt"), "present\n")
			.await
			.expect("source");
		let guard = StreamingEditGuard::new(temp.path().to_path_buf(), true);
		guard.start("stale", &revision("apply_patch"));
		guard.push_fragment("stale", &patch_args("target.txt", "missing"));
		guard.reset();

		guard.start("fresh", &revision("apply_patch"));
		guard.push_fragment("fresh", &patch_args("target.txt", "present"));
		guard.settle().await;
		assert!(guard.try_abort().is_none());
	}
}
