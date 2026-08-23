//! Late-bound RPC host-resource resolver.

use std::sync::{Arc, Weak};

use omp_core::{CowBytes, Str};
use omp_tools::read::{
	Fault,
	resolver::{LineOffsetCache, Resolve},
	selector::{ParsedSelector, parse_uri},
};
use parking_lot::RwLock;

use crate::HostResources;

static BROKER: RwLock<Option<Weak<dyn HostResources>>> = RwLock::new(None);

/// Installs the sole live RPC host-resource authority.
pub fn bind(broker: &Arc<dyn HostResources>) -> Result<(), ()> {
	let mut current = BROKER.write();
	if current.as_ref().and_then(Weak::upgrade).is_some() {
		return Err(());
	}
	*current = Some(Arc::downgrade(broker));
	Ok(())
}

/// Removes the authority only when it still names this RPC generation owner.
pub fn unbind(broker: &Arc<dyn HostResources>) {
	let mut current = BROKER.write();
	if current
		.as_ref()
		.and_then(Weak::upgrade)
		.as_ref()
		.is_some_and(|active| Arc::ptr_eq(active, broker))
	{
		*current = None;
	}
}

/// Resolver arm for the bounded set of schemes declared by the RPC host.
pub(crate) struct HostUriResolver {
	lines:  LineOffsetCache,
	broker: Option<Arc<dyn HostResources>>,
}

impl HostUriResolver {
	pub(crate) fn new(broker: Option<Arc<dyn HostResources>>) -> Self {
		Self { lines: LineOffsetCache::default(), broker }
	}
}

impl Resolve for HostUriResolver {
	async fn read<'a>(
		&'a self,
		authored_uri: &'a str,
		selector: &'a ParsedSelector,
	) -> Result<CowBytes<'static>, Fault> {
		use super::select_bytes;
		let parsed = parse_uri(authored_uri)
			.map_err(|_| Fault::Invalid {
				message: Str::new_static("Host resource URI is malformed."),
			})?
			.ok_or_else(|| Fault::Invalid {
				message: Str::new_static("Host resource URI requires scheme:// syntax."),
			})?;
		let broker = self
			.broker
			.as_ref()
			.cloned()
			.or_else(|| BROKER.read().as_ref().and_then(Weak::upgrade))
			.ok_or_else(|| Fault::Unsupported {
				message: Str::new_static("RPC host resource authority is unavailable."),
			})?;
		let url = parsed.query.map_or_else(
			|| format!("{}://{}", parsed.raw_scheme, parsed.resource),
			|query| format!("{}://{}?{query}", parsed.raw_scheme, parsed.resource),
		);
		let result = broker
			.resolve_read(&url)
			.await
			.map_err(|message| Fault::Source { message })?;
		let content = result.content.unwrap_or_default();
		let selected =
			select_bytes(&self.lines, &url, CowBytes::from(content.into_bytes()), selector)?;
		if result.notes.is_empty() {
			return Ok(selected);
		}
		use std::fmt::Write as _;
		let mut rendered =
			String::from_utf8(selected.into_bytes().to_vec()).map_err(|_| Fault::Invalid {
				message: Str::new_static("Host resource selection did not resolve to UTF-8 text."),
			})?;
		rendered.push_str("\n\nResolution notes:");
		for note in result.notes {
			let _ = write!(rendered, "\n- {note}");
		}
		Ok(CowBytes::from(rendered.into_bytes()))
	}
}

/// Routes one whole-resource write through the same declared generation.
pub(crate) async fn write(uri: &str, content: String) -> Result<u64, Str> {
	let byte_len = content.len() as u64;
	let broker = BROKER
		.read()
		.as_ref()
		.and_then(Weak::upgrade)
		.ok_or_else(|| Str::new_static("RPC host resource authority is unavailable."))?;
	broker.resolve_write(uri, content).await?;
	Ok(byte_len)
}
