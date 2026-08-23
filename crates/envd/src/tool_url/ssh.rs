//! Bounded `ssh://` direct-operation resolver.

use omp_core::{CowBytes, Str};
use omp_tools::read::{
	Fault,
	resolver::{
		LineOffsetCache, Resolve, ResourceCompletion, ResourceEntry, ResourceList, fuzzy_score,
	},
	selector::ParsedSelector,
};

use crate::ssh::{SshError, SshService};

pub(crate) struct SshResolver {
	service: SshService,
	lines:   LineOffsetCache,
}

impl SshResolver {
	pub(super) fn new(service: SshService) -> Self {
		Self { service, lines: LineOffsetCache::default() }
	}
}

impl Resolve for SshResolver {
	async fn read<'a>(
		&'a self,
		resource: &'a str,
		selector: &'a ParsedSelector,
	) -> Result<CowBytes<'static>, Fault> {
		use super::select_bytes;
		let (alias, path) = parse_resource(resource)?;
		if path == "/" {
			let listing = self.list(resource, 1_000, 1024 * 1024).await?;
			let mut body = String::new();
			for entry in listing.entries {
				body.push_str(if entry.directory { "d " } else { "f " });
				body.push_str(entry.name.as_str());
				body.push('\n');
			}
			return select_bytes(&self.lines, resource, CowBytes::from(body.into_bytes()), selector);
		}
		let bytes = self
			.service
			.read(alias.as_str(), path.as_str(), 8 * 1024 * 1024)
			.await
			.map_err(ssh_fault)?;
		select_bytes(&self.lines, resource, bytes, selector)
	}

	async fn read_query<'a>(
		&'a self,
		resource: &'a str,
		query: Option<&'a str>,
		selector: &'a ParsedSelector,
	) -> Result<CowBytes<'static>, Fault> {
		use super::select_bytes;
		let Some(query) = query else {
			return self.read(resource, selector).await;
		};
		let (alias, path) = parse_resource(resource)?;
		let mut operation = None;
		let mut command = None;
		for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
			match name.as_ref() {
				"op" if operation.is_none() => operation = Some(value.into_owned()),
				"command" if command.is_none() => command = Some(value.into_owned()),
				_ => {
					return Err(Fault::Invalid {
						message: Str::new_static(
							"ssh:// accepts one op and, for exec, one command query parameter.",
						),
					});
				},
			}
		}
		let bytes = match operation.as_deref() {
			Some("stat") if command.is_none() => {
				let metadata = self.service.stat(&alias, &path).await.map_err(ssh_fault)?;
				CowBytes::from(
					format!(
						"kind: {}\\nsize: {}\\n",
						if metadata.directory {
							"directory"
						} else {
							"file"
						},
						metadata.size
					)
					.into_bytes(),
				)
			},
			Some("exec") => {
				let command = command.ok_or_else(|| Fault::Invalid {
					message: Str::new_static("ssh://?op=exec requires command."),
				})?;
				let output = self
					.service
					.exec(&alias, &command, 1024 * 1024)
					.await
					.map_err(ssh_fault)?;
				let mut body = Vec::with_capacity(output.stdout.len() + output.stderr.len() + 64);
				body.extend_from_slice(output.stdout.as_ref());
				if !output.stderr.is_empty() {
					body.extend_from_slice(b"\\n[stderr]\\n");
					body.extend_from_slice(output.stderr.as_ref());
				}
				if let Some(status) = output.exit_status {
					body.extend_from_slice(format!("\\n[exit status: {status}]\\n").as_bytes());
				}
				CowBytes::from(body)
			},
			_ => {
				return Err(Fault::Invalid {
					message: Str::new_static("ssh:// query op must be stat or exec."),
				});
			},
		};
		select_bytes(&self.lines, resource, bytes, selector)
	}

	async fn list(
		&self,
		resource: &str,
		max_entries: usize,
		max_bytes: usize,
	) -> Result<ResourceList, Fault> {
		if resource.is_empty() {
			let aliases = self.service.aliases();
			let truncated = aliases.len() > max_entries;
			let entries = aliases
				.into_iter()
				.take(max_entries)
				.map(|alias| ResourceEntry {
					uri:       Str::new(format!("ssh://{alias}/")),
					name:      alias,
					directory: true,
					size:      0,
				})
				.collect();
			return Ok(ResourceList { entries, truncated });
		}
		let (alias, path) = parse_resource(resource)?;
		let (remote, mut truncated) = self
			.service
			.list(alias.as_str(), path.as_str(), max_entries)
			.await
			.map_err(ssh_fault)?;
		let mut consumed = 0usize;
		let mut entries = Vec::with_capacity(remote.len());
		for entry in remote {
			consumed = consumed.saturating_add(entry.name.len());
			if consumed > max_bytes {
				truncated = true;
				break;
			}
			let child_path = if path == "/" {
				format!("/{}", encode_component(&entry.name))
			} else {
				format!("{}/{}", path.trim_end_matches('/'), encode_component(&entry.name))
			};
			entries.push(ResourceEntry {
				uri:       Str::new(format!("ssh://{alias}{child_path}")),
				name:      entry.name,
				directory: entry.directory,
				size:      entry.size,
			});
		}
		Ok(ResourceList { entries, truncated })
	}

	async fn complete(
		&self,
		query: &str,
		max_results: usize,
	) -> Result<Vec<ResourceCompletion>, Fault> {
		let mut values = self
			.service
			.aliases()
			.into_iter()
			.filter_map(|alias| {
				let score = fuzzy_score(query, &alias)?;
				Some(ResourceCompletion {
					value: Str::new(format!("ssh://{alias}/")),
					description: Str::new_static("configured SSH host"),
					score,
				})
			})
			.collect::<Vec<_>>();
		values.sort_unstable_by(|a, b| b.score.cmp(&a.score).then_with(|| a.value.cmp(&b.value)));
		values.truncate(max_results);
		Ok(values)
	}
}

pub(crate) fn parse_resource(resource: &str) -> Result<(Str, Str), Fault> {
	let (raw_alias, raw_path) = resource.split_once('/').unwrap_or((resource, ""));
	if raw_alias.is_empty() || raw_alias.contains(['@', ':', '[', ']']) {
		return Err(Fault::Invalid {
			message: Str::new_static(
				"ssh:// authority must be one configured host alias; user, port, and address \
				 overrides are forbidden.",
			),
		});
	}
	let alias = decode_component(raw_alias)?;
	let path = decode_path(raw_path)?;
	Ok((alias, Str::new(format!("/{path}"))))
}

fn decode_path(raw: &str) -> Result<String, Fault> {
	let mut decoded = String::new();
	for segment in raw.split('/') {
		let segment = decode_component(segment)?;
		if segment == "." || segment == ".." || segment.contains(['\\', '\0']) {
			return Err(Fault::Invalid {
				message: Str::new_static(
					"ssh:// paths cannot contain dot segments, backslashes, or NUL bytes.",
				),
			});
		}
		if !decoded.is_empty() {
			decoded.push('/');
		}
		decoded.push_str(&segment);
	}
	Ok(decoded)
}

fn decode_component(raw: &str) -> Result<Str, Fault> {
	let bytes = raw.as_bytes();
	let mut out = Vec::with_capacity(bytes.len());
	let mut i = 0;
	while i < bytes.len() {
		if bytes[i] == b'%' {
			if i + 2 >= bytes.len() {
				return Err(percent_fault());
			}
			let high = hex(bytes[i + 1]).ok_or_else(percent_fault)?;
			let low = hex(bytes[i + 2]).ok_or_else(percent_fault)?;
			out.push(high << 4 | low);
			i += 3;
		} else {
			out.push(bytes[i]);
			i += 1;
		}
	}
	String::from_utf8(out)
		.map(Str::new)
		.map_err(|_| Fault::Invalid {
			message: Str::new_static("ssh:// components must decode to UTF-8."),
		})
}

const fn hex(byte: u8) -> Option<u8> {
	match byte {
		b'0'..=b'9' => Some(byte - b'0'),
		b'a'..=b'f' => Some(byte - b'a' + 10),
		b'A'..=b'F' => Some(byte - b'A' + 10),
		_ => None,
	}
}
fn percent_fault() -> Fault {
	Fault::Invalid { message: Str::new_static("ssh:// contains invalid percent encoding.") }
}

fn encode_component(value: &str) -> String {
	let mut out = String::with_capacity(value.len());
	for byte in value.bytes() {
		if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
			out.push(char::from(byte));
		} else {
			use std::fmt::Write as _;
			let _ = write!(out, "%{byte:02X}");
		}
	}
	out
}

fn ssh_fault(error: SshError) -> Fault {
	Fault::Source { message: Str::new(error.to_string()) }
}
