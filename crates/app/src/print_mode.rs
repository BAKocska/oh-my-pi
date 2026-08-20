//! Single-shot, stdout-safe inference mode.

use std::{
	io::IsTerminal as _,
	path::{Path, PathBuf},
};

use bytes::Bytes;
use futures::StreamExt as _;
use miette::{IntoDiagnostic as _, miette};
use omp_core::{Str, sf};
use omp_llm_catalog::ModelKey;
use omp_llm_inference::{
	Client,
	call::{CallMeta, ContentPart, MediaInput, Target},
	event::ChatEvent,
	id::RequestId,
	receipt::ExecutionBudget,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::{
	cli::{PrintArgs, chat_request_with_messages, data_dir, turn_id},
	usage_error::CliUsageError,
};

const MAX_ATTACHMENT_BYTES: usize = 10 * 1024 * 1024;

/// Runs a prompt using the canonical inference stream, serializing every stdout
/// write.
pub async fn run(args: PrintArgs) -> miette::Result<()> {
	let data_dir = data_dir(None)?;
	let model = args
		.model
		.or_else(|| {
			crate::settings::Settings::load(&data_dir)
				.default_model
				.map(Str::from)
		})
		.ok_or_else(|| miette!("print mode requires --model or config.default_model"))?;
	let initial = initial_parts(&args.prompt).await?;
	if initial.is_empty() {
		return Err(
			CliUsageError::new("print mode requires a prompt or piped standard input").into(),
		);
	}
	let system = discover_system_prompt()?;
	let store =
		crate::daemon::open_credential_store(data_dir.join("credentials.db")).into_diagnostic()?;
	let registry = crate::daemon::production_registry(&data_dir, store)
		.await
		.into_diagnostic()?;
	let planner =
		omp_llm_inference::router::Router::new(registry.clone(), std::time::Duration::from_secs(30));
	let meta = CallMeta {
		id:       RequestId::from(turn_id()),
		target:   Target::Model(ModelKey::from(model)),
		deadline: None,
		budget:   ExecutionBudget::default(),
		session:  None,
	};
	let mut client = Client::new(registry.service(), planner, meta);
	let mut events = client
		.execute(chat_request_with_messages(initial, args.follow_ups, system))
		.await
		.into_diagnostic()?;
	let json = args.mode.as_str() == "json";
	let mut stdout = tokio::io::stdout();
	if json {
		write_json(&mut stdout, "{\"type\":\"session_start\"}\n").await?;
	} else {
		let mut stderr = tokio::io::stderr();
		stderr.write_all(b"Working...\n").await.into_diagnostic()?;
	}
	let mut completed = false;
	while let Some(event) = events.next().await {
		match event.into_diagnostic()? {
			ChatEvent::TextDelta { text, .. } if json => {
				write_json(
					&mut stdout,
					&format!("{{\"type\":\"text_delta\",\"text\":{}}}\n", json_string(text.as_str())),
				)
				.await?
			},
			ChatEvent::ThinkingDelta { text, .. } if json && !args.shape_transcript => {
				write_json(
					&mut stdout,
					&format!(
						"{{\"type\":\"thinking_delta\",\"text\":{}}}\n",
						json_string(text.as_str())
					),
				)
				.await?
			},
			ChatEvent::TextDelta { text, .. } => stdout
				.write_all(sanitize(text.as_str()).as_bytes())
				.await
				.into_diagnostic()?,
			ChatEvent::ThinkingDelta { text, .. } if args.print_thoughts => stdout
				.write_all(sanitize(text.as_str()).as_bytes())
				.await
				.into_diagnostic()?,
			ChatEvent::Completed(_) => {
				completed = true;
				if json {
					write_json(&mut stdout, "{\"type\":\"completed\"}\n").await?;
				}
			},
			_ => {},
		}
	}
	if !completed {
		return Err(miette!("inference stream ended without completion"));
	}
	if !json {
		stdout.write_all(b"\n").await.into_diagnostic()?;
	}
	stdout.flush().await.into_diagnostic()
}

async fn initial_parts(words: &[Str]) -> miette::Result<Vec<ContentPart>> {
	let mut parts = Vec::new();
	let mut text = String::new();
	let mut consumed = 0usize;
	for word in words {
		if let Some(path) = word.strip_prefix("@") {
			let attachment = read_reference(Path::new(path.as_str()), &mut consumed)?;
			match attachment {
				Attachment::Text(contents) => append_text(&mut text, &contents),
				Attachment::Image { media_type, data } => {
					parts.push(ContentPart::Image(MediaInput::Bytes { media_type, data }))
				},
				Attachment::Document { media_type, data } => {
					parts.push(ContentPart::Document(MediaInput::Bytes { media_type, data }))
				},
			}
		} else {
			append_text(&mut text, word);
		}
	}
	if text.is_empty() && !std::io::stdin().is_terminal() {
		let mut stdin = tokio::io::stdin();
		stdin.read_to_string(&mut text).await.into_diagnostic()?;
	}
	if !text.is_empty() {
		parts.insert(0, ContentPart::Text { text: text.into(), proof: None });
	}
	Ok(parts)
}

fn append_text(target: &mut String, value: &str) {
	if !target.is_empty() {
		target.push(' ');
	}
	target.push_str(value);
}

enum Attachment {
	Text(String),
	Image { media_type: Str, data: Bytes },
	Document { media_type: Str, data: Bytes },
}

fn read_reference(path: &Path, consumed: &mut usize) -> miette::Result<Attachment> {
	let bytes = std::fs::read(path).into_diagnostic()?;
	*consumed = consumed
		.checked_add(bytes.len())
		.ok_or_else(|| miette!("attachment budget overflow"))?;
	if *consumed > MAX_ATTACHMENT_BYTES {
		return Ok(Attachment::Text(format!(
			"<file name=\"{}\">(skipped: too large)</file>",
			path.display()
		)));
	}
	if let Some(media_type) = image_media_type(&bytes) {
		// Image resizing is a deliberate seam: no image pipeline is linked into
		// this binary, so bytes remain lossless until `images.autoResize` has one.
		return Ok(Attachment::Image {
			media_type: media_type.into(),
			data:       Bytes::from(bytes),
		});
	}
	if let Some(media_type) = document_media_type(path, &bytes) {
		return Ok(Attachment::Document {
			media_type: media_type.into(),
			data:       Bytes::from(bytes),
		});
	}
	String::from_utf8(bytes)
		.map(Attachment::Text)
		.into_diagnostic()
}

fn image_media_type(bytes: &[u8]) -> Option<&'static str> {
	if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
		Some("image/png")
	} else if bytes.starts_with(b"\xff\xd8\xff") {
		Some("image/jpeg")
	} else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
		Some("image/gif")
	} else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
		Some("image/webp")
	} else {
		None
	}
}

fn document_media_type(path: &Path, bytes: &[u8]) -> Option<&'static str> {
	if bytes.starts_with(b"%PDF-") {
		return Some("application/pdf");
	}
	if bytes.starts_with(b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1") {
		return Some("application/vnd.ms-office");
	}
	match path
		.extension()
		.and_then(|extension| extension.to_str())
		.map(str::to_ascii_lowercase)
		.as_deref()
	{
		Some("docx") => {
			Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document")
		},
		Some("pptx") => {
			Some("application/vnd.openxmlformats-officedocument.presentationml.presentation")
		},
		Some("xlsx") => Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
		Some("ipynb") => Some("application/x-ipynb+json"),
		Some("html" | "htm") => Some("text/html"),
		_ => None,
	}
}

fn discover_system_prompt() -> miette::Result<Option<Str>> {
	let cwd = std::env::current_dir().into_diagnostic()?;
	let home = std::env::var_os("HOME")
		.map(PathBuf::from)
		.unwrap_or_else(|| cwd.clone());
	discover_system_prompt_from(&cwd, &home)
}

fn discover_system_prompt_from(cwd: &Path, home: &Path) -> miette::Result<Option<Str>> {
	let roots = crate::discovery::native::discover_roots(cwd, home, 32);
	let candidates = roots
		.project
		.iter()
		.map(|root| root.join("SYSTEM.md"))
		.chain(std::iter::once(roots.user.join("SYSTEM.md")));
	for path in candidates {
		if path.is_file() {
			return std::fs::read_to_string(path)
				.map(Str::from)
				.into_diagnostic()
				.map(Some);
		}
	}
	Ok(None)
}

async fn write_json(stdout: &mut tokio::io::Stdout, line: &str) -> miette::Result<()> {
	stdout.write_all(line.as_bytes()).await.into_diagnostic()?;
	stdout.flush().await.into_diagnostic()
}

fn sanitize(text: &str) -> String {
	text.replace('\0', "")
}
fn json_string(text: &str) -> String {
	format!("{:?}", sanitize(text))
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn classifies_text_documents_and_images_by_content() {
		assert_eq!(image_media_type(b"\x89PNG\r\n\x1a\nmore"), Some("image/png"));
		assert_eq!(
			document_media_type(Path::new("report.pdf"), b"%PDF-1.7"),
			Some("application/pdf")
		);
		assert!(document_media_type(Path::new("sheet.xlsx"), b"PK\x03\x04").is_some());
	}
	#[test]
	fn attachment_budget_returns_an_explicit_skip_notice() {
		let file = std::env::temp_dir().join("omp-print-large-reference.txt");
		std::fs::write(&file, vec![b'x'; MAX_ATTACHMENT_BYTES + 1]).expect("write");
		let Attachment::Text(notice) = read_reference(&file, &mut 0).expect("notice") else {
			panic!("text notice");
		};
		assert!(notice.contains("skipped: too large"));
		let _ = std::fs::remove_file(file);
	}
	#[test]
	fn discovers_nearest_project_system_prompt() {
		let tree = tempfile::tempdir().expect("tree");
		let cwd = tree.path().join("nested");
		std::fs::create_dir_all(cwd.join(".omp")).expect("config");
		std::fs::write(cwd.join(".omp/SYSTEM.md"), "project instructions").expect("system");
		assert_eq!(
			discover_system_prompt_from(&cwd, tree.path()).expect("discover"),
			Some(sf!("project instructions"))
		);
	}
}
