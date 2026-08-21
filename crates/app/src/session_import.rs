//! Foreign session import command over transcript-v4 post-hoc repair.

use std::{
	path::Path,
	time::{SystemTime, UNIX_EPOCH},
};

use miette::{IntoDiagnostic as _, WrapErr as _, miette};
use omp_storage::{
	index::{EventProjection, NewSession, RepairRecord, SessionIndex, SessionKind},
	transcript::{ForeignFormat, Header, SessionId, Writer, parse_foreign_jsonl},
};

use crate::cli::{ImportSessionArgs, ImportSessionFormat};

/// Imports one foreign JSONL session and prints its new durable session id.
pub fn run(data_dir: &Path, project: &Path, args: &ImportSessionArgs) -> miette::Result<()> {
	let bytes = std::fs::read_to_string(&args.path)
		.into_diagnostic()
		.wrap_err_with(|| format!("could not read foreign session {}", args.path.display()))?;
	let format = match args.format {
		ImportSessionFormat::ClaudeCode => ForeignFormat::ClaudeCode,
		ImportSessionFormat::Codex => ForeignFormat::Codex,
	};
	let parsed = parse_foreign_jsonl(format, &bytes);
	if parsed.entries.is_empty() {
		return Err(miette!(
			"{} contained no importable messages ({} malformed records)",
			args.path.display(),
			parsed.diagnostics.len()
		));
	}
	let had_diagnostics = !parsed.diagnostics.is_empty();
	let created = parsed
		.entries
		.first()
		.map(|entry| entry.event.ts)
		.filter(|timestamp| *timestamp > 0)
		.unwrap_or_else(now_ms);
	let events = parsed.into_events(format, 0);
	let root = std::fs::canonicalize(project).into_diagnostic()?;
	let state_dir = crate::project_state::directory(data_dir, &root).into_diagnostic()?;
	let sessions_dir = state_dir.join("sessions");
	std::fs::create_dir_all(&sessions_dir).into_diagnostic()?;
	let session_id = SessionId(omp_core::Str::from(omp_core::Ulid::generate().to_string()));
	let journal_path = sessions_dir.join(format!("{}.jsonl", session_id.0));
	let index = SessionIndex::open(state_dir.join("sessions.sqlite3")).into_diagnostic()?;
	let root_display = root.to_string_lossy().into_owned();
	let metadata = NewSession {
		id:         &session_id,
		cwd:        &root_display,
		project:    &root_display,
		created_ms: created,
		kind:       SessionKind::Interactive,
		parent:     None,
		remote:     false,
	};
	let header = Header { v: 4, id: session_id.clone(), created, cwd: root };
	let mut writer = index
		.create_session(&metadata, || {
			let writer = Writer::create(&journal_path, &header)?;
			let watermark = std::fs::metadata(&journal_path)?.len();
			Ok::<_, omp_storage::transcript::Error>((writer, watermark))
		})
		.map_err(|error| miette!(error.to_string()))?;
	writer
		.append_many(&events)
		.map_err(|error| miette!(error.to_string()))?;

	let journal = std::fs::read(&journal_path).into_diagnostic()?;
	let records = repair_records(&journal, &events)?;
	let through = journal.len() as u64;
	index
		.repair(&session_id, through, records)
		.into_diagnostic()?;
	println!("{}", session_id.0);
	if had_diagnostics {
		eprintln!(
			"import completed with recoverable malformed records; source line numbers were preserved"
		);
	}
	Ok(())
}

fn repair_records<'a>(
	journal: &[u8],
	events: &'a [omp_storage::transcript::Event],
) -> miette::Result<Vec<RepairRecord<'a>>> {
	let mut newlines = journal
		.iter()
		.enumerate()
		.filter_map(|(offset, byte)| (*byte == b'\n').then_some(offset + 1));
	newlines
		.next()
		.ok_or_else(|| miette!("imported journal header is incomplete"))?;
	let mut records = Vec::with_capacity(events.len());
	for (event_index, event) in events.iter().enumerate() {
		let watermark = newlines.next().unwrap_or(journal.len());
		let kind = match &event.kind {
			omp_storage::transcript::Kind::Msg(_) => "msg",
			omp_storage::transcript::Kind::Label { .. } => "label",
			_ => "imported",
		};
		records.push(RepairRecord {
			event_index: event_index as u64,
			byte_watermark: watermark as u64,
			ts_ms: event.ts,
			kind,
			projection: EventProjection::Plain,
		});
	}
	Ok(records)
}

fn now_ms() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or_default()
		.as_millis()
		.try_into()
		.unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn repair_positions_preserve_every_physical_event() {
		let imported = parse_foreign_jsonl(
			ForeignFormat::Codex,
			concat!(
				r#"{"type":"event_msg","payload":{"type":"user_message","message":"hello"}}"#,
				"\n",
			),
		);
		let events = imported.into_events(ForeignFormat::Codex, 0);
		let journal = b"{header}\n{message}\n{label}\n";
		let records = repair_records(journal, &events).unwrap();
		assert_eq!(records.len(), 2);
		assert!(records[0].byte_watermark < records[1].byte_watermark);
	}
}
