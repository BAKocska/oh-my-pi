//! Lock-safe session maintenance, cold archive, and blob reclamation.

use std::{
	collections::{HashMap, HashSet},
	fs::File,
	io::{Read as _, Write as _},
	path::{Path, PathBuf},
	time::Duration,
};

use flate2::{Compression, write::GzEncoder};
use miette::{IntoDiagnostic as _, miette};
use omp_storage::{
	blob::BlobStore,
	gc,
	index::{SessionFilter, SessionIndex, SessionStatus},
	transcript::SessionId,
};
use rusqlite::{Connection, TransactionBehavior, params};
use serde_json::json;

use crate::cli::GcArgs;

struct GcLock(PathBuf);

impl Drop for GcLock {
	fn drop(&mut self) {
		let _ = std::fs::remove_file(&self.0);
	}
}

/// Runs dry by default; destructive work requires `--apply`.
pub fn run(args: GcArgs) -> miette::Result<()> {
	let data_dir = crate::cli::data_dir(args.data_dir)?;
	let sessions_dir = args
		.sessions_dir
		.unwrap_or_else(|| data_dir.join("sessions"));
	let index_path = args
		.index
		.unwrap_or_else(|| sessions_dir.join("sessions.sqlite3"));
	let _lock = acquire_lock(&data_dir.join("gc.lock"))?;
	let index = SessionIndex::open(&index_path).into_diagnostic()?;
	let page = index
		.list(&SessionFilter { limit: u32::MAX, ..SessionFilter::default() })
		.into_diagnostic()?;
	let cutoff = now_ms().saturating_sub(args.cold_archive_after_days.saturating_mul(86_400_000));
	let mut protected = HashSet::new();
	for session in page.sessions.iter().take(args.retain_newest_global) {
		protected.insert(session.id.as_str().to_owned());
	}
	let mut per_cwd = HashMap::<String, usize>::new();
	for session in &page.sessions {
		let count = per_cwd.entry(session.cwd.as_str().to_owned()).or_default();
		if *count < args.retain_newest_per_cwd {
			protected.insert(session.id.as_str().to_owned());
			*count += 1;
		}
	}
	let parents = page
		.sessions
		.iter()
		.filter_map(|session| {
			session
				.parent
				.as_ref()
				.map(|parent| parent.as_str().to_owned())
		})
		.collect::<HashSet<_>>();
	let mut candidates = Vec::new();
	let mut ambiguous_lineage = 0_usize;
	for session in &page.sessions {
		let active = matches!(
			session.status,
			SessionStatus::Pending | SessionStatus::Interrupted | SessionStatus::Unknown
		);
		let lineage = session.parent.is_some() || parents.contains(session.id.as_str());
		if lineage
			&& session.updated_ms < cutoff
			&& !active
			&& !protected.contains(session.id.as_str())
		{
			ambiguous_lineage += 1;
			continue;
		}
		if session.updated_ms < cutoff
			&& !active
			&& !lineage
			&& !protected.contains(session.id.as_str())
		{
			candidates.push(session.id.clone());
		}
	}
	let mut archived_bytes = 0_u64;
	if args.apply && args.archive {
		for session in &candidates {
			archived_bytes = archived_bytes.saturating_add(archive_session(
				&sessions_dir,
				&data_dir.join("archive/sessions"),
				&index_path,
				session,
			)?);
		}
	}
	let retained = page
		.sessions
		.iter()
		.filter(|session| !args.apply || !candidates.contains(&session.id))
		.map(|session| session.id.clone())
		.collect::<Vec<_>>();
	let sweep = if args.apply {
		gc::sweep(
			&BlobStore::open(&data_dir).into_diagnostic()?,
			&retained,
			Duration::from_secs(args.min_age_seconds),
		)
		.into_diagnostic()?
	} else {
		gc::SweepReport::default()
	};
	if args.apply {
		optimize_index(&index_path)?;
		if args.wal {
			checkpoint_databases(&data_dir, &index_path)?;
		}
	}
	let report = json!({
		"applied": args.apply,
		"archiveRequested": args.archive,
		"archiveCandidates": candidates.len(),
		"archivedBytes": archived_bytes,
		"lineageProtected": ambiguous_lineage,
		"retainedSessions": retained.len(),
		"blobsExamined": sweep.examined_count,
		"blobsReclaimed": sweep.reclaimed_count,
		"bytesReclaimed": sweep.reclaimed_bytes,
		"corruptReferences": sweep.corrupt_references,
	});
	if args.json {
		println!("{}", serde_json::to_string_pretty(&report).into_diagnostic()?);
	} else {
		println!(
			"{}: {} archive candidate(s), {} lineage-protected, {} blob(s) reclaimed ({} bytes)",
			if args.apply { "applied" } else { "dry run" },
			candidates.len(),
			ambiguous_lineage,
			sweep.reclaimed_count,
			sweep.reclaimed_bytes,
		);
	}
	Ok(())
}

fn archive_session(
	sessions_dir: &Path,
	archive_dir: &Path,
	index_path: &Path,
	session: &SessionId,
) -> miette::Result<u64> {
	let source = sessions_dir.join(format!("{}.jsonl", session.as_str()));
	if !source.is_file() {
		return Err(miette!("session journal is missing: {}", source.display()));
	}
	std::fs::create_dir_all(archive_dir).into_diagnostic()?;
	let destination = archive_dir.join(format!("{}.jsonl.gz", session.as_str()));
	if destination.exists() {
		return Err(miette!("archive destination already exists: {}", destination.display()));
	}
	let temporary = destination.with_extension(format!("gz.tmp-{}", std::process::id()));
	let mut input = File::open(&source).into_diagnostic()?;
	let mut encoder =
		GzEncoder::new(File::create(&temporary).into_diagnostic()?, Compression::default());
	std::io::copy(&mut input, &mut encoder).into_diagnostic()?;
	let output = encoder.finish().into_diagnostic()?;
	output.sync_all().into_diagnostic()?;
	std::fs::rename(&temporary, &destination).into_diagnostic()?;
	let artifacts = sessions_dir.join(session.as_str());
	if artifacts.is_dir() {
		std::fs::rename(&artifacts, archive_dir.join(session.as_str())).into_diagnostic()?;
	}
	let mut connection = Connection::open(index_path).into_diagnostic()?;
	connection
		.pragma_update(None, "foreign_keys", true)
		.into_diagnostic()?;
	let transaction = connection
		.transaction_with_behavior(TransactionBehavior::Immediate)
		.into_diagnostic()?;
	transaction
		.execute("DELETE FROM prompts_fts WHERE session_id = ?1", [session.as_str()])
		.into_diagnostic()?;
	transaction
		.execute("DELETE FROM sessions WHERE id = ?1", [session.as_str()])
		.into_diagnostic()?;
	transaction.commit().into_diagnostic()?;
	std::fs::remove_file(&source).into_diagnostic()?;
	Ok(std::fs::metadata(destination).into_diagnostic()?.len())
}

fn optimize_index(path: &Path) -> miette::Result<()> {
	let connection = Connection::open(path).into_diagnostic()?;
	connection
		.execute("INSERT INTO prompts_fts(prompts_fts) VALUES('optimize')", [])
		.into_diagnostic()?;
	connection
		.execute_batch("PRAGMA optimize;")
		.into_diagnostic()?;
	Ok(())
}

fn checkpoint_databases(data_dir: &Path, index_path: &Path) -> miette::Result<()> {
	for path in [
		index_path.to_owned(),
		data_dir.join("credentials.db"),
		data_dir.join("models.db"),
		data_dir.join("history.db"),
		data_dir.join("stats.db"),
	] {
		if path.is_file() {
			Connection::open(path)
				.into_diagnostic()?
				.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
				.into_diagnostic()?;
		}
	}
	Ok(())
}

fn acquire_lock(path: &Path) -> miette::Result<GcLock> {
	if let Some(parent) = path.parent() {
		std::fs::create_dir_all(parent).into_diagnostic()?;
	}
	let create = || {
		std::fs::OpenOptions::new()
			.write(true)
			.create_new(true)
			.open(path)
	};
	let mut file = match create() {
		Ok(file) => file,
		Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && stale_lock(path) => {
			std::fs::remove_file(path).into_diagnostic()?;
			create().into_diagnostic()?
		},
		Err(error) => return Err(error).into_diagnostic(),
	};
	writeln!(file, "{}", std::process::id()).into_diagnostic()?;
	file.sync_all().into_diagnostic()?;
	Ok(GcLock(path.to_owned()))
}

fn stale_lock(path: &Path) -> bool {
	let Ok(text) = std::fs::read_to_string(path) else {
		return false;
	};
	let Ok(pid) = text.trim().parse::<u32>() else {
		return false;
	};
	#[cfg(unix)]
	{
		nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_err()
	}
	#[cfg(not(unix))]
	{
		let _ = pid;
		false
	}
}

fn now_ms() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map_or(0, |duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
}
